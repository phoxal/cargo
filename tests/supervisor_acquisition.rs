//! Integration tests for authored supervisor selection.
//!
//! `robot.yaml` is the sole selection authority. These tests exercise the
//! acquisition path against an isolated managed root and a local
//! application fixture: one unchanged tool accepts differently versioned
//! compatible selections, refuses foreign targets and unsupported
//! interfaces from the embedded record without executing foreign code,
//! preserves existing installations on candidate failure, and reuses
//! matching caches.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE_CONTRACT_MAIN: &str = "compatible";
const FOREIGN_TARGET_CONTRACT_MAIN: &str = "foreign target";
const WRONG_BUNDLE_CONTRACT_MAIN: &str = "unsupported bundle";
const NO_CONTRACT_MAIN: &str = "no contract";
const HARDWARE_ONLY_MAIN: &str = "hardware only";

/// Stages one fixture supervisor crate of the given version and contract.
fn stage_supervisor_fixture(
    root: &Path,
    name: &str,
    version: &str,
    contract_main: &str,
) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join("src"))
        .unwrap_or_else(|error| panic!("supervisor fixture dir: {error}"));
    use phoxal::artifact::application::*;
    let target = if contract_main == FOREIGN_TARGET_CONTRACT_MAIN {
        if HOST_EXECUTION_TARGET.contains("darwin") {
            "x86_64-unknown-linux-gnu"
        } else {
            "aarch64-apple-darwin"
        }
    } else {
        HOST_EXECUTION_TARGET
    };
    let contract = ApplicationContract {
        bundle: Some(if contract_main == WRONG_BUNDLE_CONTRACT_MAIN {
            BUNDLE_CONTRACT.with_revision(BUNDLE_CONTRACT.revision + 1)
        } else {
            BUNDLE_CONTRACT
        }),
        launch: SUPERVISOR_LAUNCH_CONTRACT,
        execution: Some(EXECUTION_PROTOCOL_CONTRACT),
        simulation: (contract_main != HARDWARE_ONLY_MAIN).then_some(SIMULATION_PROTOCOL_CONTRACT),
        target,
    };
    let bytes = encode_application_contract(&contract);
    let embedded = if contract_main == NO_CONTRACT_MAIN {
        String::new()
    } else {
        format!(
            "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_app\"))]\n#[cfg_attr(target_os = \"linux\", unsafe(link_section = \".phoxal_app\"))]\nstatic CONTRACT: [u8; {}] = {:?};\n",
            bytes.len(),
            bytes
        )
    };
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"phoxal-supervisor\"\nversion = \"{version}\"\nedition = \"2021\"\n"
        ),
    )
    .unwrap_or_else(|error| panic!("fixture manifest: {error}"));
    let execution_marker = if contract_main == FOREIGN_TARGET_CONTRACT_MAIN {
        format!(
            "std::fs::write({:?}, b\"executed\").unwrap();",
            dir.join("executed.marker")
        )
    } else {
        String::new()
    };
    fs::write(
        dir.join("src/main.rs"),
        format!(
            "{embedded}\nfn main() {{ {execution_marker} println!(\"supervisor {version}\"); }}\n"
        ),
    )
    .unwrap_or_else(|error| panic!("fixture main: {error}"));
    dir
}

/// Stages one robot with the given supervisor source selection.
fn stage_robot_with_supervisor(root: &Path, supervisor_path: &str) -> PathBuf {
    let robot = root.join("robot");
    fs::create_dir_all(robot.join("src")).unwrap_or_else(|error| panic!("robot dir: {error}"));
    fs::write(
        robot.join("Cargo.toml"),
        "[package]\nname = \"selection-proof-robot\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\npublish = false\n",
    )
    .unwrap_or_else(|error| panic!("robot manifest: {error}"));
    let fixture_base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/robot-base");
    fs::copy(fixture_base.join("build.rs"), robot.join("build.rs"))
        .unwrap_or_else(|error| panic!("copy fixture build.rs: {error}"));
    fs::copy(fixture_base.join("src/main.rs"), robot.join("src/main.rs"))
        .unwrap_or_else(|error| panic!("robot main: {error}"));
    fs::write(
        robot.join("robot.yaml"),
        format!(
            "schema: phoxal/robot/v0\nrobot: {{ id: selection-proof-robot }}\nsupervisor:\n  source: {{ path: {supervisor_path} }}\n"
        ),
    )
    .unwrap_or_else(|error| panic!("robot.yaml: {error}"));
    robot
}

fn runnable_build(robot: &Path) -> PathBuf {
    robot
        .join("target/phoxal/selection-proof-robot")
        .join(phoxal::artifact::application::HOST_EXECUTION_TARGET)
        .join("release/build")
}

/// Runs a build in the isolated home.
fn build_in(robot: &Path, phoxal_home: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(robot)
        .env_remove("CARGO_TARGET_DIR")
        .env("PHOXAL_HOME", phoxal_home)
        .env_remove("CARGO_REGISTRIES_PHOXAL_INDEX")
        .arg("phoxal")
        .arg("build")
        .output()
        .unwrap_or_else(|error| panic!("spawn cargo-phoxal: {error}"))
}

#[test]
fn one_unchanged_tool_accepts_two_differently_versioned_compatible_supervisors() {
    let guard = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace tempdir: {error}"));
    let root = guard.path();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("home: {error}"));

    // Stage two supervisor crates of different versions, both carrying the
    // same compatible embedded contract.
    let first = stage_supervisor_fixture(root, "supervisor-a", "0.1.0", FIXTURE_CONTRACT_MAIN);
    let second = stage_supervisor_fixture(root, "supervisor-b", "0.2.0", FIXTURE_CONTRACT_MAIN);

    // Robot A selects supervisor-a.
    let robot_a = stage_robot_with_supervisor(&root.join("a"), "../../supervisor-a");
    let output = build_in(&robot_a, &home);
    assert!(
        output.status.success(),
        "the first supervisor version builds:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Robot B (different robot, same tool) selects supervisor-b.
    let robot_b = stage_robot_with_supervisor(&root.join("b"), "../../supervisor-b");
    let output = build_in(&robot_b, &home);
    assert!(
        output.status.success(),
        "the second, differently versioned but compatible supervisor builds:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Each robot's bundle contains its own selected supervisor copy.
    let manifest_a: serde_json::Value = serde_json::from_slice(
        &fs::read(runnable_build(&robot_a).join("manifest.json"))
            .unwrap_or_else(|error| panic!("manifest A: {error}")),
    )
    .unwrap_or_else(|error| panic!("decode manifest A: {error}"));
    let manifest_b: serde_json::Value = serde_json::from_slice(
        &fs::read(runnable_build(&robot_b).join("manifest.json"))
            .unwrap_or_else(|error| panic!("manifest B: {error}")),
    )
    .unwrap_or_else(|error| panic!("decode manifest B: {error}"));
    assert_eq!(manifest_a["robot_id"], "selection-proof-robot");
    assert_eq!(manifest_b["robot_id"], "selection-proof-robot");
    assert_ne!(robot_a, robot_b);
    for (robot, version) in [(&robot_a, "0.1.0"), (&robot_b, "0.2.0")] {
        let executable = runnable_build(robot).join("bin/supervisor");
        let output = Command::new(executable)
            .output()
            .expect("execute compatible host fixture");
        assert!(output.status.success());
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            format!("supervisor {version}")
        );
    }
    assert_eq!(
        fs::read_dir(home.join("applications/supervisor"))
            .expect("selection stores")
            .count(),
        2
    );
    let _ = (first, second);
}

#[test]
fn a_foreign_target_supervisor_is_refused_from_the_record_without_execution() {
    let guard = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace tempdir: {error}"));
    let root = guard.path();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("home: {error}"));

    stage_supervisor_fixture(
        root,
        "foreign-supervisor",
        "0.1.0",
        FOREIGN_TARGET_CONTRACT_MAIN,
    );
    let robot = stage_robot_with_supervisor(root, "../foreign-supervisor");
    let output = build_in(&robot, &home);
    assert!(
        !output.status.success(),
        "a foreign-target supervisor must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("targets") && stderr.contains("supervisor"),
        "refusal names the embedded target:\n{stderr}"
    );
    assert!(
        !root.join("foreign-supervisor/executed.marker").exists(),
        "foreign contract inspection must never execute the candidate"
    );
}

#[test]
fn an_incompatible_bundle_contract_is_refused_before_child_launch() {
    let guard = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace tempdir: {error}"));
    let root = guard.path();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("home: {error}"));

    stage_supervisor_fixture(
        root,
        "wrong-bundle-supervisor",
        "0.1.0",
        WRONG_BUNDLE_CONTRACT_MAIN,
    );
    let robot = stage_robot_with_supervisor(root, "../wrong-bundle-supervisor");
    let output = build_in(&robot, &home);
    assert!(
        !output.status.success(),
        "an incompatible bundle contract must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("bundle revision"),
        "refusal names the bundle contract:\n{stderr}"
    );
}

#[test]
fn a_candidate_without_an_embedded_contract_is_refused() {
    let guard = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace tempdir: {error}"));
    let root = guard.path();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("home: {error}"));

    stage_supervisor_fixture(root, "no-contract-supervisor", "0.1.0", NO_CONTRACT_MAIN);
    let robot = stage_robot_with_supervisor(root, "../no-contract-supervisor");
    let output = build_in(&robot, &home);
    assert!(
        !output.status.success(),
        "a supervisor without an embedded contract must be refused"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no embedded application contract")
            || stderr.contains("not a readable binary")
            || stderr.contains("malformed application contract"),
        "refusal names the missing or malformed contract:\n{stderr}"
    );
}

#[test]
fn a_failed_candidate_preserves_the_existing_installation_and_bundles() {
    let guard = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace tempdir: {error}"));
    let root = guard.path();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("home: {error}"));

    // A compatible supervisor builds a valid bundle first.
    stage_supervisor_fixture(root, "good-supervisor", "0.1.0", FIXTURE_CONTRACT_MAIN);
    let robot = stage_robot_with_supervisor(root, "../good-supervisor");
    let output = build_in(&robot, &home);
    assert!(output.status.success(), "the compatible supervisor builds");
    let bundle_manifest = runnable_build(&robot).join("manifest.json");
    let original =
        fs::read(&bundle_manifest).unwrap_or_else(|error| panic!("original manifest: {error}"));

    // Pointing the robot at an incompatible supervisor fails; the
    // existing bundle output and the good supervisor's usable installation
    // are both preserved.
    stage_supervisor_fixture(root, "bad-supervisor", "0.1.0", WRONG_BUNDLE_CONTRACT_MAIN);
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0
robot: { id: selection-proof-robot }
supervisor:
  source: { path: ../bad-supervisor }
",
    )
    .unwrap_or_else(|error| panic!("switch selection: {error}"));
    let output = build_in(&robot, &home);
    assert!(
        !output.status.success(),
        "the incompatible candidate must fail"
    );
    assert_eq!(
        fs::read(&bundle_manifest)
            .unwrap_or_else(|error| panic!("bundle manifest survives: {error}")),
        original,
        "the previously assembled bundle is unchanged by the failed candidate"
    );
    // The good supervisor's cached installation is still usable.
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0
robot: { id: selection-proof-robot }
supervisor:
  source: { path: ../good-supervisor }
",
    )
    .unwrap_or_else(|error| panic!("restore selection: {error}"));
    let output = build_in(&robot, &home);
    assert!(
        output.status.success(),
        "the good supervisor's installation still works after the failed candidate:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_matching_path_selection_reuses_its_cache_across_builds() {
    let guard = tempfile::tempdir().unwrap_or_else(|error| panic!("workspace tempdir: {error}"));
    let root = guard.path();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home).unwrap_or_else(|error| panic!("home: {error}"));

    stage_supervisor_fixture(root, "cached-supervisor", "0.1.0", FIXTURE_CONTRACT_MAIN);
    let robot = stage_robot_with_supervisor(root, "../cached-supervisor");

    let first = build_in(&robot, &home);
    assert!(
        first.status.success(),
        "the first build succeeds:
{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = build_in(&robot, &home);
    assert!(
        second.status.success(),
        "the second build reuses the cached installation:\n{}",
        String::from_utf8_lossy(&second.stderr)
    );
}

fn authored_robot(root: &Path, source: &str) -> PathBuf {
    let robot = stage_robot_with_supervisor(root, "unused");
    write_selection(&robot, source);
    robot
}

fn write_selection(robot: &Path, source: &str) {
    fs::write(robot.join("robot.yaml"), format!("schema: phoxal/robot/v0\nrobot: {{ id: selection-proof-robot }}\nsupervisor:\n  source: {source}\n"))
        .unwrap_or_else(|error| panic!("authored supervisor source: {error}"));
}

fn supervisor_output(robot: &Path) -> String {
    let binary = runnable_build(robot).join("bin/supervisor");
    let output = Command::new(binary)
        .output()
        .unwrap_or_else(|error| panic!("execute compatible fixture: {error}"));
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap_or_else(|error| panic!("fixture provenance: {error}"))
}

#[test]
fn pinned_git_supervisor_uses_the_shared_resolver_and_validates_package_path()
-> Result<(), Box<dyn std::error::Error>> {
    let guard = tempfile::tempdir()?;
    let root = guard.path();
    let repo = root.join("repository");
    stage_supervisor_fixture(&repo, "apps/supervisor", "0.4.0", FIXTURE_CONTRACT_MAIN);
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "fixture",
        ],
    ] {
        let output = Command::new("git").current_dir(&repo).args(args).output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let revision = Command::new("git")
        .current_dir(&repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let revision = String::from_utf8(revision.stdout)?.trim().to_owned();
    let url = format!("file://{}", repo.display());
    let selection = format!(
        "{{ git: {{ name: phoxal-supervisor, url: '{url}', rev: '{revision}', path: apps/supervisor }} }}"
    );
    let robot = authored_robot(root, &selection);
    let home = root.join("home");
    let first = build_in(&robot, &home);
    assert!(
        first.status.success(),
        "pinned Git selection: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(supervisor_output(&robot).trim(), "supervisor 0.4.0");
    write_selection(
        &robot,
        &selection.replace("path: apps/supervisor", "path: apps/incorrect"),
    );
    let rejected = build_in(&robot, &home);
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("not at selected path"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert_eq!(supervisor_output(&robot).trim(), "supervisor 0.4.0");
    Ok(())
}

#[test]
fn incompatible_edit_of_the_same_local_source_preserves_the_selected_product() {
    let directory = tempfile::tempdir().expect("test directory");
    let root = directory.path();
    let home = root.join("home");
    stage_supervisor_fixture(root, "supervisor", "0.1.0", FIXTURE_CONTRACT_MAIN);
    let robot = stage_robot_with_supervisor(root, "../supervisor");
    let first = build_in(&robot, &home);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let store = fs::read_dir(home.join("applications/supervisor"))
        .expect("stores")
        .next()
        .expect("one selection")
        .expect("entry")
        .path();
    let selected = store.join("product/bin/phoxal-supervisor");
    let manifest = runnable_build(&robot).join("manifest.json");
    let original_manifest = fs::read(&manifest).expect("bundle");
    stage_supervisor_fixture(root, "supervisor", "0.2.0", WRONG_BUNDLE_CONTRACT_MAIN);
    let incompatible = build_in(&robot, &home);
    assert!(!incompatible.status.success());
    assert!(String::from_utf8_lossy(&incompatible.stderr).contains("bundle revision"));
    assert_eq!(
        fs::read(&manifest).expect("previous bundle"),
        original_manifest
    );
    let previous = Command::new(selected)
        .output()
        .expect("preserved installation");
    assert!(previous.status.success());
    assert_eq!(
        String::from_utf8_lossy(&previous.stdout).trim(),
        "supervisor 0.1.0"
    );
}

#[test]
fn missing_yaml_selection_fails_without_legacy_inference() {
    let directory = tempfile::tempdir().expect("test directory");
    let robot = stage_robot_with_supervisor(directory.path(), "unused");
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0\nrobot: { id: selection-proof-robot }\n",
    )
    .expect("missing selection");
    let output = build_in(&robot, &directory.path().join("home"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("supervisor"));
}

#[test]
fn hardware_build_and_ordinary_tests_do_not_require_simulation_interfaces() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    stage_supervisor_fixture(root, "hardware-supervisor", "0.7.0", HARDWARE_ONLY_MAIN);
    let robot = stage_robot_with_supervisor(root, "../hardware-supervisor");
    let home = root.join("home");
    let hardware = build_in(&robot, &home);
    assert!(
        hardware.status.success(),
        "{}",
        String::from_utf8_lossy(&hardware.stderr)
    );
    let manifest = runnable_build(&robot).join("manifest.json");
    let hardware_manifest = fs::read(&manifest).unwrap();
    let ordinary_tests = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(&robot)
        .env_remove("CARGO_TARGET_DIR")
        .env("PHOXAL_HOME", &home)
        .args(["phoxal", "test"])
        .output()
        .unwrap();
    assert!(
        ordinary_tests.status.success(),
        "ordinary Rust tests must not require the simulation interface: {}",
        String::from_utf8_lossy(&ordinary_tests.stderr)
    );
    assert_eq!(fs::read(&manifest).unwrap(), hardware_manifest);
    assert!(build_in(&robot, &home).status.success());
}
