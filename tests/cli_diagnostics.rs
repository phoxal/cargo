//! Public preparation diagnostics and executable paths use the caller's context.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

// The CLI owns a metadata child. Keep the complete test-owned process group
// under the guard, including when an assertion unwinds before socket release.
struct Owned(std::process::Child);
impl Owned {
    fn spawn(command: &mut Command) -> Self {
        use std::os::unix::process::CommandExt;
        Self(
            command
                .process_group(0)
                .spawn()
                .unwrap_or_else(|error| panic!("spawn test-owned CLI process group: {error}")),
        )
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        use std::time::{Duration, Instant};
        let group = format!("-{}", self.0.id());
        let _ = Command::new("/bin/kill").args(["-TERM", &group]).output();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match self.0.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    let _ = Command::new("/bin/kill").args(["-KILL", &group]).output();
                    let _ = self.0.kill();
                    let forced_deadline = Instant::now() + Duration::from_secs(2);
                    while matches!(self.0.try_wait(), Ok(None)) && Instant::now() < forced_deadline
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    if !matches!(self.0.try_wait(), Ok(Some(_))) {
                        eprintln!("Test-owned CLI group {group} cleanup remains unconfirmed");
                    }
                    break;
                }
            }
        }
    }
}

#[test]
fn nested_invocation_normalizes_cargo_paths_and_honors_json_diagnostics() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("robot with spaces");
    let nested = root.join("nested directory");
    fs::create_dir_all(&nested).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='review-robot'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("robot.yaml"), "schema: phoxal/robot/v0\nrobot: { id: path-review }\nsupervisor: { source: { path: ../supervisor } }\n").unwrap();
    let wrapper = nested.join("my cargo");
    fs::write(&wrapper, include_str!("fixtures/process/diagnostic.sh")).unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    for command in [vec!["check"], vec!["scenario", "missing.rs"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
            .current_dir(&nested)
            .args(command)
            .args(["--cargo", "./my cargo", "--message-format=json"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let diagnostic: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
        assert_eq!(diagnostic["reason"], "phoxal-diagnostic");
        assert!(
            diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("REVIEW_CARGO_EXECUTED")
        );
    }
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(&nested)
        .args(["simulation", "missing.xml", "--message-format=json"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let diagnostic: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(diagnostic["reason"], "phoxal-diagnostic");
    assert!(
        diagnostic["message"]
            .as_str()
            .unwrap()
            .contains("missing.xml")
    );
}

#[test]
fn human_feedback_is_flushed_while_metadata_is_blocked_and_json_stays_unstyled() {
    use std::{
        io::Write,
        os::unix::net::UnixStream,
        process::Stdio,
        time::{Duration, Instant},
    };
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='feedback-robot'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.path().join("robot.yaml"), "schema: phoxal/robot/v0\nrobot: {id: feedback-robot}\nsupervisor: {source: {path: ../supervisor}}\n").unwrap();
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
    for json in [false, true] {
        let stderr = root.path().join("stderr");
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(root.path())
            .args(["check", "--cargo"])
            .arg(&fixture)
            .stdout(Stdio::piped())
            .stderr(fs::File::create(&stderr).unwrap());
        if json {
            command.arg("--message-format=json");
        }
        let mut child = Owned::spawn(&mut command);
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let bytes = fs::read(&stderr).unwrap();
            if root.path().join("ready").exists()
                && (json
                    || bytes
                        .windows(b"Still working".len())
                        .any(|w| w == b"Still working"))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "feedback did not arrive while blocked"
            );
            assert!(child.0.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(20));
        }
        let blocked = fs::read(&stderr).unwrap();
        println!(
            "Blocked metadata, JSON={json}: {}",
            String::from_utf8_lossy(&blocked)
        );
        if json {
            assert!(blocked.is_empty());
        } else {
            assert!(blocked.starts_with(b"Check: Starting.\n"));
        }
        UnixStream::connect(root.path().join("release.sock"))
            .unwrap()
            .write_all(b"release")
            .unwrap();
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "metadata failure did not reap");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(child.0.try_wait().unwrap().unwrap().code(), Some(1));
        let final_bytes = fs::read(&stderr).unwrap();
        println!(
            "Finished metadata, JSON={json}: {}",
            String::from_utf8_lossy(&final_bytes)
        );
        if json {
            let value: serde_json::Value = serde_json::from_slice(&final_bytes).unwrap();
            assert!(value["message"].as_str().unwrap().contains("RAW_CHILD"));
        } else {
            assert!(!String::from_utf8_lossy(&final_bytes).contains("Completed"));
        }
        fs::remove_file(root.path().join("ready")).unwrap();
        fs::remove_file(root.path().join("release.sock")).unwrap();
    }
}

#[test]
fn simulation_passthrough_keeps_raw_child_bytes_and_plain_modes_equivalent() {
    use phoxal::artifact::application::*;
    let root = tempfile::tempdir().unwrap();
    let fixture = root.path().join("simulator");
    let record = encode_application_contract(&ApplicationContract {
        bundle: Some(BUNDLE_CONTRACT),
        launch: SIMULATOR_LAUNCH_CONTRACT,
        execution: None,
        simulation: Some(SIMULATION_PROTOCOL_CONTRACT),
        target: HOST_EXECUTION_TARGET,
    });
    let data = record
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let source = root.path().join("simulator.rs");
    fs::write(&source, format!("#[used]\n#[cfg_attr(target_os=\"macos\", unsafe(link_section=\"__DATA,__phoxal_app\"))]\n#[cfg_attr(target_os=\"linux\", unsafe(link_section=\".phoxal_app\"))]\nstatic RECORD: [u8;512] = [{data}];\n{}", include_str!("fixtures/process/raw_output.rs"))).unwrap();
    assert!(
        Command::new("rustc")
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&fixture)
            .status()
            .unwrap()
            .success()
    );
    for (term, no_color) in [
        ("xterm-256color", false),
        ("xterm-256color", true),
        ("dumb", false),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(root.path())
            .args(["simulation", "scene.xml", "--build", "build"])
            .env("PHOXAL_SIMULATOR", &fixture)
            .env("TERM", term)
            .env_remove("NO_COLOR");
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"{\"raw\":true}\n\xff\x1b[31m\n");
        assert_eq!(output.stderr, b"Next: child-owned\n\xfe\x1b[0m\n");
    }
}

#[test]
fn owned_plain_diagnostics_do_not_depend_on_terminal_palette_or_color_settings() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='plain-robot'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.path().join("robot.yaml"), "schema: [broken\n").unwrap();
    let mut expected = None;
    for (term, no_color, colorfgbg) in [
        ("xterm-256color", false, "15;0"),
        ("xterm-256color", true, "0;15"),
        ("dumb", false, "0;15"),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(root.path())
            .arg("check")
            .env("TERM", term)
            .env("COLORFGBG", colorfgbg)
            .env_remove("NO_COLOR");
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.contains(&27));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Next: correct the selected robot file")
        );
        if let Some(expected) = &expected {
            assert_eq!(&output.stderr, expected);
        } else {
            expected = Some(output.stderr);
        }
    }
}

#[test]
fn assertion_unwind_reaps_the_blocked_cli_and_metadata_process_group() {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("Cargo.toml"),
        "[package]\nname='unwind-robot'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.path().join("robot.yaml"), "schema: phoxal/robot/v0\nrobot: {id: unwind-robot}\nsupervisor: {source: {path: ../supervisor}}\n").unwrap();
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
    let owned = Owned::spawn(
        Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
            .current_dir(root.path())
            .args(["check", "--cargo"])
            .arg(&fixture)
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    );
    let parent = owned.0.id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.path().join("ready").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let metadata: u32 = fs::read_to_string(root.path().join("ready"))
        .unwrap()
        .parse()
        .unwrap();
    let failure = std::panic::catch_unwind(move || {
        let _owned = owned;
        panic!("deliberate assertion-failure cleanup qualification");
    });
    assert!(failure.is_err());
    loop {
        let inventory = Command::new("ps").args(["-axo", "pid="]).output().unwrap();
        assert!(inventory.status.success(), "process inventory failed");
        let pids: Vec<u32> = String::from_utf8(inventory.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect();
        if !pids.contains(&parent) && !pids.contains(&metadata) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "owned process survived unwind: parent {parent}, metadata {metadata}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    println!(
        "Actual unwind cleanup: parent {parent}, metadata {metadata}, both absent after bounded group cleanup."
    );
}
