//! Integration tests for `cargo phoxal simulation status`.
//!
//! Status reporting has no MuJoCo or hardware prerequisite; it can run in
//! any environment where the cargo-phoxal binary itself is available.
//! Installation and executing a simulation test need a managed simulator and
//! stay as host acceptance, exercised outside CI.

mod support;

use support::stage;

#[test]
fn simulation_status_reports_a_deterministic_shape_in_json_mode() {
    // `cargo phoxal simulation status` is a global query that does not
    // read the working tree; we still stage a clean tempdir to keep the
    // test's filesystem surface predictable.
    let (_guard, root) = stage("check-valid");
    let output = support::invoke(&root, &["simulation", "status", "--json"]);
    assert!(
        output.status.success(),
        "simulation status must succeed even without a managed simulator:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|error| {
        panic!("simulation status --json must emit JSON, got:\n{stdout}\n{error}")
    });
    assert_eq!(
        parsed.get("installed").and_then(serde_json::Value::as_bool),
        Some(false),
        "isolated status must report no managed installation, got:\n{stdout}"
    );
    let expected_root = root
        .join(".phoxal-home")
        .join("applications")
        .join("simulation");
    assert_eq!(
        parsed.get("root").and_then(serde_json::Value::as_str),
        Some(expected_root.to_string_lossy().as_ref()),
        "status JSON must report the isolated managed root, got:\n{stdout}"
    );
}
