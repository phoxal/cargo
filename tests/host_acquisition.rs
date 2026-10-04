//! Cold-cache acquisition through real isolated Git.
//! Cargo supplies the current tool binary; this suite neither builds a second
//! tool package nor imports its private implementation.
//! Select it explicitly with `--features host-acceptance`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The tool repository owning this source-acquisition acceptance suite.
fn tool_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_owned()
}
fn sdk_root() -> PathBuf {
    let mut command = cargo_metadata::MetadataCommand::new();
    command.manifest_path(tool_root().join("Cargo.toml"));
    if let Some(cargo) = std::env::var_os("CARGO") {
        command.cargo_path(cargo);
    }
    let metadata = command.exec().expect("tool dependency graph");
    metadata
        .packages
        .iter()
        .find(|package| package.name == "phoxal")
        .expect("SDK dependency")
        .manifest_path
        .as_std_path()
        .parent()
        .expect("SDK package root")
        .to_owned()
}

/// Scaffolds one cold robot project whose generated API is compiled by a
/// real `cargo build`.
fn scaffold_robot(
    directory: &Path,
    services: &str,
    main_body: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let robot = directory.join("robot");
    fs::create_dir_all(robot.join("src"))?;
    fs::write(
        robot.join("robot.yaml"),
        format!(
            "schema: phoxal/robot/v0\nrobot: {{ id: marker-proof }}\nsupervisor:\n  source: {{ path: supervisor }}\nservices:\n{services}"
        ),
    )?;
    let sdk = sdk_root();
    fs::write(
        robot.join("Cargo.toml"),
        format!(
            "[package]\nname = \"marker-proof-robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[dependencies]\nphoxal = {{ path = {:?}, default-features = false, features = [\"runtime\"] }}\n[build-dependencies]\nphoxal = {{ path = {:?}, default-features = false, features = [\"build\"] }}\n",
            sdk.display().to_string(),
            sdk.display().to_string()
        ),
    )?;
    fs::write(
        robot.join("build.rs"),
        "fn main() -> Result<(), phoxal::build::Error> { phoxal::build::api(phoxal::build::BuildApiConfig::default()) }\n",
    )?;
    fs::write(robot.join("src/main.rs"), main_body)?;
    Ok(robot)
}

fn build_robot(robot: &Path) -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    Ok(Command::new(cargo)
        .args(["build", "--offline", "--manifest-path"])
        .arg(robot.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", tool_root().join("target"))
        .output()?)
}

fn cargo_phoxal() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_cargo-phoxal"))
}

/// The standalone provider package checked into the temporary Git fixture
/// repository below: one authored call endpoint, self-contained manifest.
const GIT_PROVIDER_MAIN: &str = r#"use phoxal::runtime::Context;

#[phoxal::messages(package = "proof.gitreal.v1")]
mod contract {
    pub struct AskRequest {
        #[phoxal(tag = 1)]
        pub value: u64,
    }

    pub struct AskResponse {
        #[phoxal(tag = 1)]
        pub value: u64,
    }

    #[phoxal::endpoints]
    pub struct ProviderApi {
        #[phoxal::operation]
        ask: phoxal::contracts::RequestReply<AskRequest, AskResponse>,
    }
}

struct Provider;

#[phoxal::runtime(contract = contract::ProviderApi, period_ms = 20)]
impl Provider {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self)
    }

    #[handle(ask)]
    fn ask(
        &mut self,
        _ctx: &mut Context<'_, Self>,
        request: contract::AskRequest,
    ) -> phoxal::Result<contract::AskResponse> {
        Ok(contract::AskResponse {
            value: request.value.saturating_add(1),
        })
    }
}

fn main() -> phoxal::Result<()> {
    phoxal::runtime::run::<Provider>()
}
"#;

/// Acquires a provider from a real local Git repository pinned to a real
/// commit through the normal `cargo phoxal prepare` command path, then
/// compiles a cold consumer naming the generated marker.
#[test]
fn local_git_repository_acquisition_prepares_real_products()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();

    // A real repository with one real commit carrying the provider.
    let repo = root.join("provider-repo");
    fs::create_dir_all(repo.join("src"))?;
    fs::write(
        repo.join("Cargo.toml"),
        format!(
            "[package]\nname = \"proof-git-provider\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[[bin]]\nname = \"proof-git-provider\"\npath = \"src/main.rs\"\n\n[dependencies]\nphoxal = {{ path = {:?}, default-features = false, features = [\"runtime\"] }}\n",
            sdk_root().display().to_string()
        ),
    )?;
    fs::write(repo.join("src/main.rs"), GIT_PROVIDER_MAIN)?;
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args([
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .current_dir(&repo)
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "git {:?} failed: {}{}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok::<(), Box<dyn std::error::Error>>(())
    };
    git(&["init", "--quiet"])?;
    // `cargo install --locked` requires a committed lock file.
    let lock = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args(["generate-lockfile", "--manifest-path"])
        .arg(repo.join("Cargo.toml"))
        .output()?;
    assert!(
        lock.status.success(),
        "cargo generate-lockfile failed: {}{}",
        String::from_utf8_lossy(&lock.stdout),
        String::from_utf8_lossy(&lock.stderr)
    );
    git(&["add", "."])?;
    git(&["commit", "--quiet", "-m", "provider fixture"])?;
    let rev_output = Command::new("git")
        .current_dir(&repo)
        .args(["rev-parse", "HEAD"])
        .output()?;
    assert!(rev_output.status.success());
    let rev = String::from_utf8_lossy(&rev_output.stdout)
        .trim()
        .to_owned();
    assert_eq!(rev.len(), 40, "a real pinned commit");

    // A robot project acquiring the repository through the normal command.
    let robot = scaffold_robot(
        root,
        &format!(
            "  pinned: {{ source: {{ git: {{ name: proof-git-provider, url: {:?}, rev: {rev} }} }} }}\n",
            format!("file://{}", repo.canonicalize()?.display())
        ),
        "phoxal::api!();\n\nfn main() {\n    let _ = <api::operations::proof::gitreal::v1::Ask as phoxal::contracts::Operation>::METHOD;\n}\n",
    )?;
    // The prerequisite tool is built by this acceptance path itself, so no
    // stale prebuilt binary can stand in for the current sources.
    let cargo_phoxal = cargo_phoxal();
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home)?;
    let prepared = std::process::Command::new(&cargo_phoxal)
        .arg("prepare")
        .current_dir(&robot)
        .env("PHOXAL_HOME", &home)
        .env("CARGO_TARGET_DIR", tool_root().join("target"))
        .env("CARGO_BUILD_JOBS", "2")
        .output()?;
    assert!(
        prepared.status.success(),
        "cargo phoxal prepare failed:\n{}\n{}",
        String::from_utf8_lossy(&prepared.stdout),
        String::from_utf8_lossy(&prepared.stderr)
    );
    let prepared_root = robot.join(".phoxal/prepared");
    let git_products: Vec<_> = fs::read_dir(&prepared_root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("git-"))
        })
        .collect();
    assert_eq!(
        git_products.len(),
        1,
        "the pinned Git acquisition produced {git_products:?}"
    );
    assert!(git_products[0].join("contract.json").is_file());
    assert!(git_products[0].join("descriptors.pb").is_file());

    let output = build_robot(&robot)?;
    assert!(
        output.status.success(),
        "git-acquired consumer build failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
