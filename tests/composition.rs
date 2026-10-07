//! Real CLI authored composition needs no compiler or acquired participants.
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};
fn invoke(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("{error}"))
}
fn config(root: &Path, args: &[&str]) -> serde_json::Value {
    let output = invoke(root, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| panic!("{error}"))
}
fn project(root: &Path) {
    fs::create_dir_all(root.join("src/nested")).unwrap_or_else(|error| panic!("{error}"));
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname='composition-proof'\nversion='0.1.0'\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    fs::write(root.join("robot.yaml"), "schema: phoxal/robot/v0\nrobot:\n  id: common\n  brain: {}\n  services:\n    motion:\n      source: {path: ../motion}\n      config: {limits: {speed: 1, turn: 2}, sequence: [1, 2]}\nsupervisor: {source: {path: ../unavailable}}\n").unwrap_or_else(|error| panic!("{error}"));
}
#[test]
fn explicit_files_order_operators_nested_paths_and_plain_roundtrip() {
    let temporary = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = temporary.path();
    project(root);
    fs::write(root.join("one.yaml"), "schema: phoxal/robot/v0\nrobot:\n  id: one\n  services:\n    motion:\n      source: !replace {git: {name: motion, url: https://example.test/motion, rev: '0123456789abcdef0123456789abcdef01234567'}}\n      config:\n        limits: {speed: !delete, acceleration: 3}\n        sequence: []\n").unwrap_or_else(|error| panic!("{error}"));
    fs::write(root.join("two.yaml"), "schema: phoxal/robot/v0\nrobot:\n  id: two\n  services:\n    motion:\n      config:\n        limits: !replace {}\n        sequence: [4]\n").unwrap_or_else(|error| panic!("{error}"));
    let value = config(
        &root.join("src/nested"),
        &[
            "-f",
            "../../robot.yaml",
            "config",
            "-f",
            "../../one.yaml",
            "-f",
            "../../two.yaml",
            "--json",
        ],
    );
    assert_eq!(value["robot"]["id"], "two");
    assert_eq!(
        value["robot"]["services"]["motion"]["config"]["limits"],
        serde_json::json!({})
    );
    assert_eq!(
        value["robot"]["services"]["motion"]["config"]["sequence"],
        serde_json::json!([4])
    );
    assert!(
        value["robot"]["services"]["motion"]["source"]
            .get("path")
            .is_none()
    );
    let reverse = config(
        root,
        &[
            "config",
            "--json",
            "-f",
            "robot.yaml",
            "-f",
            "two.yaml",
            "-f",
            "one.yaml",
        ],
    );
    assert_eq!(reverse["robot"]["id"], "one");
    assert_eq!(
        reverse["robot"]["services"]["motion"]["config"]["sequence"],
        serde_json::json!([])
    );
    let standalone = "schema: phoxal/robot/v0\nrobot:\n  id: alone\nsupervisor: {source: {path: ../unavailable}}\n";
    fs::write(root.join("alone.yaml"), standalone).unwrap_or_else(|error| panic!("{error}"));
    let alone = config(root, &["config", "--json", "-f", "alone.yaml"]);
    assert_eq!(alone["robot"]["id"], "alone");
    assert_eq!(alone["robot"]["services"], serde_json::json!({}));
    assert_eq!(config(root, &["config", "--json"])["robot"]["id"], "common");
    let plain = invoke(root, &["config", "-f", "robot.yaml", "-f", "one.yaml"]);
    assert!(plain.status.success());
    assert!(!String::from_utf8_lossy(&plain.stdout).contains('!'));
    fs::write(root.join("resolved.yaml"), plain.stdout).unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(
        config(root, &["config", "--json", "-f", "resolved.yaml"]),
        config(
            root,
            &["config", "--json", "-f", "robot.yaml", "-f", "one.yaml"]
        )
    );
}
#[test]
fn invalid_compositions_fail_without_acquisition_and_build_rejects_files() {
    let temporary = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = temporary.path();
    project(root);
    for text in [
        "schema: phoxal/robot/v9\n",
        "schema: phoxal/robot/v0\nrobot: {id: first, id: duplicate}\n",
        "schema: phoxal/robot/v0\nsupervisor: !delete\n",
        "schema: phoxal/robot/v0\nrobot:\n  services: {motion: {config: null}}\n",
        "schema: phoxal/robot/v0\nrobot:\n  services: {motion: {bindings: {manual: absent.intent}}}\n",
        "schema: phoxal/robot/v0\nrobot:\n  services: {motion: {bindings: {manual: [brain.intent, brain.intent]}}}\n",
        "schema: phoxal/robot/v0\nrobot:\n  services: {motion: {config: !unsupported {}}}\n",
        "schema: phoxal/robot/v0\nrobot:\n  services: {motion: {config: {values: [!replace {}]}}}\n",
        "schema: phoxal/robot/v0\nrobot:\n  services: {motion: {config: !delete 4}}\n",
    ] {
        fs::write(root.join("invalid.yaml"), text).unwrap_or_else(|error| panic!("{error}"));
        let output = invoke(root, &["config", "-f", "robot.yaml", "-f", "invalid.yaml"]);
        assert!(!output.status.success(), "invalid layer accepted: {text}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("invalid.yaml"));
        if text.contains("id: duplicate") {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("duplicate entry with key \"id\"")
            );
        }
    }
    let output = invoke(
        root,
        &[
            "simulation",
            "scene.xml",
            "--build",
            "build",
            "-f",
            "robot.yaml",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be combined with --file"));
    assert!(!root.join("target").exists());
}

#[test]
fn native_mount_names_and_diagnostic_origins_follow_resolved_values() {
    let temporary = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = temporary.path();
    project(root);
    fs::write(root.join("mount.yaml"), "schema: phoxal/robot/v0\nrobot:\n  components:\n    motor: {source: {path: ../motor}, mount_site: Front.LeftMount}\n").unwrap_or_else(|error| panic!("{error}"));
    let value = config(
        root,
        &["config", "--json", "-f", "robot.yaml", "-f", "mount.yaml"],
    );
    assert_eq!(
        value["robot"]["components"]["motor"]["mount_site"],
        "Front.LeftMount"
    );
    for name in ["", "   "] {
        fs::write(root.join("mount.yaml"), format!("schema: phoxal/robot/v0\nrobot:\n  components:\n    motor: {{source: {{path: ../motor}}, mount_site: '{name}'}}\n")).unwrap_or_else(|error| panic!("{error}"));
        let output = invoke(root, &["config", "-f", "robot.yaml", "-f", "mount.yaml"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("mount_site must not be empty"));
    }
    // This protects composition error attribution, rather than the schema's unknown-field policy.
    fs::write(root.join("bad.yaml"), "schema: phoxal/robot/v0\ntypo: true\nrobot:\n  id: common\nsupervisor: {source: {path: ../supervisor}}\n").unwrap_or_else(|error| panic!("{error}"));
    fs::write(
        root.join("later.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  id: later\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let output = invoke(root, &["config", "-f", "bad.yaml", "-f", "later.yaml"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("resolved composition (selected inputs:"),
        "{error}"
    );
    assert!(error.contains("bad.yaml") && error.contains("later.yaml"));
    fs::write(root.join("base.yaml"), "schema: phoxal/robot/v0\nrobot:\n  id: common\n  services:\n    motion:\n      source: {path: ../motion}\n      bindings: {manual: absent.intent}\nsupervisor: {source: {path: ../supervisor}}\n").unwrap_or_else(|error| panic!("{error}"));
    let output = invoke(root, &["config", "-f", "base.yaml", "-f", "later.yaml"]);
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("base.yaml: robot.services.motion.bindings.manual"),
        "{error}"
    );
    fs::write(root.join("replace.yaml"), "schema: phoxal/robot/v0\nrobot:\n  services:\n    motion: !replace\n      source: {path: ../motion}\n      bindings: {manual: missing.intent}\n").unwrap_or_else(|error| panic!("{error}"));
    let output = invoke(
        root,
        &[
            "config",
            "-f",
            "base.yaml",
            "-f",
            "replace.yaml",
            "-f",
            "later.yaml",
        ],
    );
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("replace.yaml: robot.services.motion.bindings.manual"),
        "{error}"
    );
    fs::write(
        root.join("delete.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  services:\n    motion:\n      bindings:\n        manual: !delete\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    assert!(
        invoke(
            root,
            &[
                "config",
                "-f",
                "base.yaml",
                "-f",
                "replace.yaml",
                "-f",
                "delete.yaml"
            ]
        )
        .status
        .success()
    );
}

#[test]
fn binary_selection_diagnostics_identify_the_owning_authored_path() {
    let temporary = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = temporary.path();
    project(root);
    fs::write(
        root.join("supervisor.yaml"),
        "schema: phoxal/robot/v0\nsupervisor:\n  binary: invalid target\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let output = invoke(
        root,
        &["config", "-f", "robot.yaml", "-f", "supervisor.yaml"],
    );
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("supervisor.yaml: supervisor.binary 'invalid target'"),
        "{error}"
    );
    assert!(!error.contains("robot.services.supervisor"), "{error}");

    fs::write(
        root.join("service.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  services:\n    motion:\n      binary: invalid target\n",
    )
    .unwrap_or_else(|error| panic!("{error}"));
    let output = invoke(root, &["config", "-f", "robot.yaml", "-f", "service.yaml"]);
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("service.yaml: robot.services.motion:"),
        "{error}"
    );
    assert!(error.contains("binary 'invalid target'"), "{error}");
}
