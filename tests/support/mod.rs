//! Shared helpers for the cargo-phoxal integration tests.
//!
//! These helpers only touch the test runner's filesystem and launch
//! the compiled `cargo-phoxal` binary. They do not import anything from
//! `cargo_phoxal` or `phoxal`: the integration tests treat the binary as
//! an opaque subprocess the same way a downstream user would.

pub mod sdk;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Copies a committed fixture into a fresh tempdir so each invocation starts
/// from a clean filesystem. Robot cases overlay their distinct files on the
/// shared `robot-base`. Returns the tempdir guard alongside the destination
/// path so the caller can keep it alive for the test duration.
#[allow(dead_code)]
pub fn stage(fixture: &str) -> (tempfile::TempDir, PathBuf) {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let source = fixtures.join(fixture);
    let destination =
        tempfile::tempdir().unwrap_or_else(|error| panic!("create fixture tempdir: {error}"));
    let path = destination.path().to_owned();
    if source.join("robot.yaml").is_file() {
        let robot_base = fixtures.join("robot-base");
        copy_tree(&robot_base, &path)
            .unwrap_or_else(|error| panic!("copy fixture {}: {error}", robot_base.display()));
    }
    copy_tree(&source, &path)
        .unwrap_or_else(|error| panic!("copy fixture {}: {error}", source.display()));
    let supervisor = path.join("supervisor/Cargo.toml");
    if supervisor.is_file() {
        let manifest = fs::read_to_string(&supervisor)
            .unwrap_or_else(|error| panic!("fixture supervisor manifest: {error}"));
        let dependency = format!(
            "phoxal = {{ path = {:?}, default-features = false }}",
            sdk::sdk_root()
        );
        fs::write(&supervisor, manifest.replace(
            "phoxal = { version = \"=0.0.0-dev.8\", registry = \"phoxal\", default-features = false }", &dependency))
            .unwrap_or_else(|error| panic!("selected SDK fixture dependency: {error}"));
    }
    (destination, path)
}

#[allow(dead_code)]
fn copy_tree(source: &Path, destination: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(source, destination)?;
        return Ok(());
    }
    fs::create_dir_all(destination)?;
    let mut entries = fs::read_dir(source)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if matches!(
            entry.file_name().to_str(),
            Some("target" | ".git" | ".codex" | "Cargo.lock")
        ) {
            continue;
        }
        copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
    }
    Ok(())
}

/// Invokes the binary with `args`, anchored at `cwd` and isolated from any
/// user-managed Phoxal installation. Returns the raw `Output`; the caller
/// asserts on status, stdout, and stderr.
#[allow(dead_code)]
pub fn invoke(cwd: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
    command
        .current_dir(cwd)
        .env_remove("CARGO_TARGET_DIR")
        .env("PHOXAL_HOME", cwd.join(".phoxal-home"))
        .args(args);
    command
        .output()
        .unwrap_or_else(|error| panic!("spawn cargo-phoxal: {error}"))
}
