//! Public preparation diagnostics and executable paths use the caller's context.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

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
    fs::write(
        &wrapper,
        "#!/bin/sh\necho REVIEW_CARGO_EXECUTED >&2\nexit 19\n",
    )
    .unwrap();
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
