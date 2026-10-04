//! Integration tests for `cargo phoxal publish component ... --dry-run`.
//!
//! Spawns the compiled binary against a passive component fixture and
//! asserts on the structured dry-run report.

mod support;

use support::stage;

#[test]
fn publish_dry_run_emits_the_expected_package_summary() {
    let (_guard, root) = stage("passive-component");
    let output = support::invoke(
        &root,
        &[
            "publish",
            "component",
            "passive-component-fixture",
            "--path",
            ".",
            "--dry-run",
        ],
    );
    assert!(
        output.status.success(),
        "passive-component dry-run must succeed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("publication: dry-run"),
        "stdout must mark the run as a dry run, got:\n{stdout}"
    );
    assert!(
        stdout.contains("package: passive-component-fixture"),
        "stdout must name the package, got:\n{stdout}"
    );
    assert!(
        stdout.contains("kind: component"),
        "stdout must declare the publication kind, got:\n{stdout}"
    );
}
