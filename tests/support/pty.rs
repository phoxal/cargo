//! Explicit owned PTY I/O for terminal-process acceptance.
use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub struct Pty {
    pub master: File,
    pub bytes: Vec<u8>,
}
impl Pty {
    pub fn attach(command: &mut Command, columns: u16) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        command
            .stdout(Stdio::from(
                slave
                    .try_clone()
                    .unwrap_or_else(|error| panic!("clone PTY slave: {error}")),
            ))
            .stderr(Stdio::from(slave))
            .stdin(Stdio::null())
            .env("TERM", "xterm-256color")
            .env_remove("CI")
            .env_remove("NO_COLOR");
        Self {
            master,
            bytes: Vec::new(),
        }
    }
    pub fn drain(&mut self) {
        loop {
            let mut bytes = [0; 4096];
            match self.master.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => self.bytes.extend_from_slice(&bytes[..count]),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(error) => panic!("PTY read failed: {error}"),
            }
        }
    }
    pub fn until(&mut self, text: &str, seconds: u64) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            self.drain();
            if String::from_utf8_lossy(&self.bytes).contains(text) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "PTY output missing {text:?}: {:?}",
                String::from_utf8_lossy(&self.bytes)
            );
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
    pub fn resize(&self, columns: u16) {
        let size = libc::winsize {
            ws_row: 24,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size) },
            0
        );
    }
}
