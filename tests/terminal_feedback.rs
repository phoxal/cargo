//! Real shared-PTY ownership for command-owned feedback and forwarded output.
#[path = "support/pty.rs"]
mod pty;
mod support;
use pty::Pty;
use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
struct Owned(std::process::Child);
impl Owned {
    fn spawn(command: &mut Command) -> Self {
        use std::os::unix::process::CommandExt;
        Self(
            command
                .process_group(0)
                .spawn()
                .unwrap_or_else(|error| panic!("spawn owned CLI group: {error}")),
        )
    }
    fn finish(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Some(status) = self
                .0
                .try_wait()
                .unwrap_or_else(|error| panic!("query owned CLI: {error}"))
            {
                return status;
            }
            assert!(Instant::now() < deadline, "CLI did not finish");
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(Some(_))) {
            return;
        }
        let group = format!("-{}", self.0.id());
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(10));
        }
        let _ = Command::new("/bin/kill")
            .args(["-KILL", &group])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = self.0.kill();
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            std::thread::park_timeout(Duration::from_millis(10));
        }
    }
}
#[test]
fn actual_pty_blocked_feedback_clears_before_error_and_resize_uses_plain_lines() {
    use std::{io::Write, os::unix::net::UnixStream};
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='pty-robot'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.path().join("robot.yaml"), "schema: phoxal/robot/v0\nrobot: {id: pty-robot}\nsupervisor: {source: {path: ../supervisor}}\n").unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    let fixture = root.path().join("cargo-fixture");
    assert!(
        Command::new("rustc")
            .args([
                "--edition=2024",
                "tests/fixtures/process/blocked_cargo.rs",
                "-o"
            ])
            .arg(&fixture)
            .status()
            .unwrap()
            .success()
    );
    for (index, columns, term, no_color, ci) in [
        (0, 100, "xterm-256color", true, false),
        (1, 60, "xterm-256color", false, false),
        (2, 100, "dumb", false, false),
        (3, 100, "xterm-256color", false, true),
        (4, 100, "unknown-terminal", false, false),
        (5, 100, "vt52", false, false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(root.path())
            .args(["check", "--cargo"])
            .arg(&fixture);
        let mut terminal = Pty::attach(&mut command, columns);
        command.env("TERM", term);
        if no_color {
            command.env("NO_COLOR", "1");
        }
        if ci {
            command.env("CI", "true");
        }
        let mut owner = Owned::spawn(&mut command);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.path().join("ready").exists() {
            assert!(Instant::now() < deadline);
            std::thread::park_timeout(Duration::from_millis(10));
        }
        if index == 0 {
            terminal.until("Still working", 3);
            assert!(
                terminal.bytes.contains(&27),
                "NO_COLOR keeps uncolored animation"
            );
            terminal.resize(60);
            let offset = terminal.bytes.len();
            terminal.until("Still working -", 3);
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                terminal.drain();
                std::thread::park_timeout(Duration::from_millis(10));
            }
            assert!(!terminal.bytes[offset..].contains(&27));
        } else {
            terminal.until("Check: Starting.", 3);
            assert!(!terminal.bytes.contains(&27));
        }
        UnixStream::connect(root.path().join("release.sock"))
            .unwrap()
            .write_all(b"release")
            .unwrap();
        assert_eq!(owner.finish().code(), Some(1));
        terminal.drain();
        let text = String::from_utf8_lossy(&terminal.bytes);
        assert!(text.contains("RAW_CHILD"));
        assert!(!text.split("error:").nth(1).unwrap().contains('\u{1b}'));
        println!(
            "Actual Cargo PTY columns={columns}, TERM={term}, NO_COLOR={no_color}, CI={ci}: {text:?}"
        );
        fs::remove_file(root.path().join("ready")).unwrap();
        fs::remove_file(root.path().join("release.sock")).unwrap();
    }
}
#[test]
fn actual_build_stdout_and_check_diagnostics_have_no_live_renderer_interleaving() {
    let (_guard, root) = support::stage("check-valid");
    for operation in ["build", "check"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command.current_dir(&root).args([operation, "--offline"]);
        let mut terminal = Pty::attach(&mut command, 100);
        let mut owner = Owned::spawn(&mut command);
        // Drain while the compiler runs, so its PTY cannot fill while reaping.
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            terminal.drain();
            if let Some(status) = owner.0.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "{:?}",
                    String::from_utf8_lossy(&terminal.bytes)
                );
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::park_timeout(Duration::from_millis(10));
        }
        terminal.drain();
        let text = String::from_utf8_lossy(&terminal.bytes);
        let boundary = if operation == "build" {
            "runnable build:"
        } else {
            "cargo phoxal: declaration check validated"
        };
        let permanent = text
            .split(boundary)
            .nth(1)
            .unwrap_or_else(|| panic!("missing permanent result {boundary}: {text}"));
        assert!(
            !permanent.contains('\u{1b}'),
            "live renderer crossed permanent stdout/stderr boundary"
        );
        assert!(text.contains(&format!(
            "{}: Completed.",
            if operation == "build" {
                "Build"
            } else {
                "Check"
            }
        )));
        println!("Actual shared Cargo PTY {operation}: {text:?}");
    }
}
