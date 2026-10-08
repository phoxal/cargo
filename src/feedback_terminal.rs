//! One stderr line, only while this command owns terminal output.
use std::io::{self, IsTerminal, Write};

pub(super) struct Terminal {
    columns: Option<u16>,
    drawn: bool,
    frame: usize,
}
impl Terminal {
    pub(super) fn new() -> Self {
        Self {
            columns: columns(),
            drawn: false,
            frame: 0,
        }
    }
    pub(super) fn live(&self) -> bool {
        self.columns.is_some()
    }
    pub(super) fn suspend(&mut self) {
        self.clear();
        self.columns = None;
    }
    pub(super) fn frame(&mut self) -> char {
        let frame = ['|', '/', '-', '\\'][self.frame % 4];
        self.frame = self.frame.wrapping_add(1);
        frame
    }
    pub(super) fn update(&mut self, text: &str) -> bool {
        let Some(width) = self.columns else {
            return false;
        };
        if columns() != Some(width) {
            // Resize can reflow the old line. Append rather than moving over
            // wrapped content, then permanently use ordinary lines.
            if self.drawn {
                write_best_effort(b"\n");
            }
            self.columns = None;
            self.drawn = false;
            return false;
        }
        let text: String = text
            .chars()
            .filter(|c| !c.is_control())
            .take(usize::from(width - 1))
            .collect();
        if !write_best_effort(format!("\r\x1b[2K{text}").as_bytes()) {
            self.columns = None;
            self.drawn = false;
            return false;
        }
        self.drawn = true;
        true
    }
    pub(super) fn line(&mut self, text: &str) {
        self.clear();
        write_best_effort(format!("{text}\n").as_bytes());
    }
    pub(super) fn clear(&mut self) {
        if self.drawn {
            if columns() == self.columns {
                write_best_effort(b"\r\x1b[2K");
            } else {
                write_best_effort(b"\n");
                self.columns = None;
            }
            self.drawn = false;
        }
    }
}
fn write_best_effort(bytes: &[u8]) -> bool {
    let mut output = io::stderr().lock();
    output
        .write_all(bytes)
        .and_then(|()| output.flush())
        .is_ok()
}
fn columns() -> Option<u16> {
    use std::os::fd::AsRawFd;
    if !io::stderr().is_terminal()
        || std::env::var_os("CI").is_some()
        || !std::env::var("TERM").ok().is_some_and(|term| {
            matches!(
                term.split('-').next(),
                Some(
                    "xterm"
                        | "screen"
                        | "tmux"
                        | "rxvt"
                        | "linux"
                        | "ansi"
                        | "alacritty"
                        | "foot"
                        | "wezterm"
                        | "kitty"
                )
            )
        })
    {
        return None;
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // TIOCGWINSZ writes only this valid winsize allocation.
    if unsafe { libc::ioctl(io::stderr().as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0
        || size.ws_col < 72
    {
        return None;
    }
    Some(size.ws_col)
}
