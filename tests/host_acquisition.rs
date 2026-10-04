//! Cold-cache acquisition through real isolated Git and local registries.
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

/// The development SDK's version, read from its manifest so the fixture's
/// registry requirement never drifts from the selected SDK version.
fn sdk_version() -> String {
    let manifest = fs::read_to_string(sdk_root().join("Cargo.toml"))
        .unwrap_or_else(|error| panic!("the SDK manifest is readable: {error}"));
    manifest
        .lines()
        .find_map(|line| line.strip_prefix("version = "))
        .unwrap_or_else(|| panic!("the SDK manifest declares a version"))
        .trim_matches('"')
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

/// The standalone provider package published to the temporary local
/// registry below: one authored call endpoint with a true SDK dependency.
const REGISTRY_PROVIDER_MAIN: &str = r#"use phoxal::runtime::Context;

#[phoxal::messages(package = "proof.regreal.v1")]
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

/// Real registry acquisition with an isolated local SDK dependency patch.
///
/// The provider package is acquired through a temporary git-protocol
/// registry — a real clone of a fixture index, a real checksummed
/// `.crate` download — while its true `phoxal` dependency is satisfied by
/// a config-level `[patch.crates-io]` pointing at this framework, so the
/// development SDK is neither published nor mirrored. The packaged
/// `Cargo.lock` records the patched path entry, keeping
/// `cargo install --locked` consistent.
///
/// Routes investigated on this host's cargo, recorded as evidence:
/// - a `sparse+file://` index cannot be fetched: the sparse client parses
///   request URLs with the `http` crate, whose URI grammar rejects
///   `file:///` with `InvalidFormat` (reproduced by a minimal
///   `http::Uri` parse probe);
/// - a `[patch]` in the provider manifest is stripped by `cargo package`
///   from the normalized manifest inside the `.crate`;
/// - `cargo install` from a package-root working directory does not
///   discover the project's `.cargo/config.toml` (the same limitation
///   `registry_config` documents), so a project-level patch never reaches
///   the install.
///
/// The working route delivers the registry and the patch through an
/// isolated temporary `CARGO_HOME` the install inherits. Alongside the
/// registry participant, one real capability component — `component.yaml`
/// motor and encoder capabilities beside an empty authored contract — is
/// prepared by the same command, and the consumer composes the registry
/// provider's generated call marker with the capability-derived standard
/// endpoints in one authored contract.
#[test]
fn local_registry_acquisition_with_sdk_patch_prepares_real_products()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let sdk = sdk_root().canonicalize()?;
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let target = tool_root().join("target");
    let version = sdk_version();

    // The registry index exists before packaging: the patch target's
    // workspace manifests name the `phoxal` registry, so any
    // packaging-time manifest parse needs it configured.
    let git_index = root.join("registry-index");
    let dl = root.join("registry-dl");
    fs::create_dir_all(git_index.join("pr/oo"))?;
    fs::write(
        git_index.join("config.json"),
        format!("{{\"dl\":\"file://{}\",\"api\":null}}\n", dl.display()),
    )?;

    // One provider package with a true SDK dependency and no registry
    // coordinate, resolved through the same local patch the install uses.
    let provider = root.join("provider");
    fs::create_dir_all(provider.join("src"))?;
    fs::create_dir_all(provider.join(".cargo"))?;
    fs::write(
        provider.join("Cargo.toml"),
        format!(
            "[package]\nname = \"proof-registry-provider\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"proof-registry-provider\"\npath = \"src/main.rs\"\n\n\
             [dependencies]\n\
             phoxal = {{ version = \"{version}\", default-features = false, features = [\"runtime\"] }}\n"
        ),
    )?;
    fs::write(provider.join("src/main.rs"), REGISTRY_PROVIDER_MAIN)?;
    fs::write(
        provider.join(".cargo/config.toml"),
        format!(
            "[registries.phoxal]\nindex = \"file://{}\"\n\n[patch.crates-io]\nphoxal = {{ path = \"{}\" }}\n",
            git_index.display(),
            sdk.display()
        ),
    )?;
    let package = Command::new(&cargo)
        .args(["package", "--no-verify", "--allow-dirty"])
        .arg("--target-dir")
        .arg(&target)
        .current_dir(&provider)
        .env("CARGO_BUILD_JOBS", "2")
        .output()?;
    assert!(
        package.status.success(),
        "cargo package failed:\n{}\n{}",
        String::from_utf8_lossy(&package.stdout),
        String::from_utf8_lossy(&package.stderr)
    );
    let crate_file = target.join("package/proof-registry-provider-0.1.0.crate");
    assert!(crate_file.is_file(), "the packaged crate is missing");

    // The registry serves that real artifact under its true checksum, with
    // an index entry carrying the package's true SDK dependency.
    let cksum_output = Command::new("shasum")
        .arg("-a")
        .arg("256")
        .arg(&crate_file)
        .output()?;
    assert!(cksum_output.status.success());
    let cksum = String::from_utf8_lossy(&cksum_output.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    assert_eq!(cksum.len(), 64, "a real sha256 checksum");
    fs::create_dir_all(dl.join("proof-registry-provider/0.1.0"))?;
    fs::copy(
        &crate_file,
        dl.join("proof-registry-provider/0.1.0/download"),
    )?;
    fs::write(
        git_index.join("pr/oo/proof-registry-provider"),
        format!(
            "{{\"name\":\"proof-registry-provider\",\"vers\":\"0.1.0\",\"deps\":\
             [{{\"name\":\"phoxal\",\"req\":\"{version}\",\"features\":[\"runtime\"],\
             \"optional\":false,\"default_features\":false,\"target\":null,\
             \"kind\":\"normal\",\"registry\":null,\"package\":null}}],\
             \"features\":{{}},\"yanked\":false,\"links\":null,\"v\":1,\"cksum\":\"{cksum}\"}}\n"
        ),
    )?;
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
            .current_dir(&git_index)
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
    git(&["init", "--quiet", "-b", "master"])?;
    git(&["add", "."])?;
    git(&["commit", "--quiet", "-m", "fixture registry index"])?;

    // One real capability component: motor and encoder capabilities beside
    // an empty authored contract the derived standard endpoints splice
    // into, prepared as a local path participant by the same command.
    let drive = root.join("drive");
    fs::create_dir_all(drive.join("src"))?;
    fs::write(
        drive.join("Cargo.toml"),
        format!(
            "[package]\nname = \"capability-drive-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
             [[bin]]\nname = \"capability-drive-fixture\"\npath = \"src/main.rs\"\n\n\
             [dependencies]\n\
             phoxal = {{ path = {:?}, default-features = false, features = [\"runtime\"] }}\n\n\
             [build-dependencies]\n\
             phoxal = {{ path = {:?}, default-features = false, features = [\"build\"] }}\n",
            sdk.display().to_string(),
            sdk.display().to_string()
        ),
    )?;
    fs::write(
        drive.join("build.rs"),
        "fn main() -> Result<(), phoxal::build::Error> { phoxal::build::api(phoxal::build::BuildApiConfig::default()) }\n",
    )?;
    fs::write(
        drive.join("component.yaml"),
        "schema: phoxal/component/v0\nmodel: { file: model.xml, root_body: mount }\ncapabilities:\n  motor:\n    kind: motor\n    command: velocity\n    max_torque_nm: 2.0\n  encoder:\n    kind: encoder\n    publish_rate_hz: 50.0\n",
    )?;
    fs::write(
        drive.join("model.xml"),
        "<mujoco model=\"fixture\">\n  <worldbody>\n    <body name=\"mount\"><geom type=\"sphere\" size=\"0.01\"/></body>\n  </worldbody>\n</mujoco>\n",
    )?;
    fs::write(
        drive.join("src/main.rs"),
        "/// The component's endpoint surface: the derived standard actuator\n/// input and encoder output splice in beside this empty authored\n/// contract.\n#[phoxal::endpoints]\npub struct DriveApi {}\n\nstruct Drive;\n\n#[phoxal::runtime(contract = DriveApi, period_ms = 20)]\nimpl Drive {\n    #[init]\n    fn new(_config: ()) -> phoxal::Result<Self> {\n        Ok(Drive)\n    }\n\n    #[step]\n    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {\n        Ok(())\n    }\n}\n\nfn main() -> phoxal::Result<()> {\n    phoxal::runtime::run::<Drive>()\n}\n",
    )?;

    // The robot composes both participants in one authored contract.
    let robot = scaffold_robot(
        root,
        "  pinned: { source: { package: { name: proof-registry-provider, version: 0.1.0 } } }\n  drive: { source: { path: ../drive } }\n",
        "phoxal::api!();\n\nuse phoxal::contracts::Queue;\n\n/// One brain contract composing the registry provider's generated call\n/// marker with the capability component's derived standard endpoint.\n#[phoxal::endpoints]\npub struct BrainApi {\n    #[phoxal::call]\n    ask: api::operations::proof::regreal::v1::Ask,\n\n    #[phoxal::input(max_items = 16, max_bytes = 512)]\n    encoder: Queue<::phoxal::contracts::component::encoder::EncoderSample>,\n}\n\nfn main() {\n    let method = <api::operations::proof::regreal::v1::Ask as phoxal::contracts::Operation>::METHOD;\n    assert_eq!(method.signature().service, \"proof.regreal.v1.Ask\");\n    let sample: phoxal::contracts::Observation<\n        ::phoxal::contracts::component::encoder::EncoderSample,\n    > = api::drive::encoder();\n    let _ = sample;\n}\n",
    )?;

    // The isolated Cargo configuration: the temporary registry and the
    // local SDK patch live only in this temporary CARGO_HOME, which the
    // install inherits. No user configuration is touched.
    let cargo_home = root.join("cargo-home");
    fs::create_dir_all(&cargo_home)?;
    fs::write(
        cargo_home.join("config.toml"),
        format!(
            "[registries.phoxal]\nindex = \"file://{}\"\n\n[patch.crates-io]\nphoxal = {{ path = \"{}\" }}\n",
            git_index.display(),
            sdk.display()
        ),
    )?;
    let home = root.join("phoxal-home");
    fs::create_dir_all(&home)?;
    let cargo_phoxal = cargo_phoxal();
    let prepared = Command::new(&cargo_phoxal)
        .arg("prepare")
        .current_dir(&robot)
        .env("PHOXAL_HOME", &home)
        .env("CARGO_HOME", &cargo_home)
        .env("CARGO_TARGET_DIR", &target)
        .env("CARGO_BUILD_JOBS", "2")
        .output()?;
    assert!(
        prepared.status.success(),
        "cargo phoxal prepare failed:\n{}\n{}",
        String::from_utf8_lossy(&prepared.stdout),
        String::from_utf8_lossy(&prepared.stderr)
    );
    let prepared_root = robot.join(".phoxal/prepared");
    let registry_products: Vec<_> = fs::read_dir(&prepared_root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("registry-phoxal-proof-registry-provider")
            })
        })
        .collect();
    assert_eq!(
        registry_products.len(),
        1,
        "the registry acquisition produced {registry_products:?}"
    );
    assert!(registry_products[0].join("contract.json").is_file());
    assert!(registry_products[0].join("descriptors.pb").is_file());
    let drive_products: Vec<_> = fs::read_dir(&prepared_root)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("path-drive"))
        })
        .collect();
    assert_eq!(
        drive_products.len(),
        1,
        "the capability component preparation produced {drive_products:?}"
    );
    assert!(drive_products[0].join("contract.json").is_file());
    assert!(drive_products[0].join("descriptors.pb").is_file());

    let output = build_robot(&robot)?;
    assert!(
        output.status.success(),
        "registry-acquired consumer build failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
