//! The simulator process owns parsing, streams, signals, and exit status.
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn proxy_preserves_raw_arguments_standard_streams_and_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("simulator");
    fs::write(
        &executable,
        "#!/bin/sh\nprintf '%s\\n' \"$@\"\ncat\nprintf 'native diagnostic\\n' >&2\nexit 23\n",
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .env("PHOXAL_SIMULATOR", executable)
        .args([
            "phoxal",
            "simulation",
            "--help",
            "a path with spaces",
            "--future-option=literal;value",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"input stream\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(23));
    assert_eq!(
        result.stdout,
        b"--help\na path with spaces\n--future-option=literal;value\ninput stream\n"
    );
    assert_eq!(result.stderr, b"native diagnostic\n");
}

#[test]
fn absent_executable_names_the_normal_install_command() {
    let result = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .env("PHOXAL_SIMULATOR", "/missing/phoxal-simulator")
        .args(["simulation", "--version"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("cargo install phoxal-simulator"));
}
