//! Integration tests for prepared Rust-contract selection identity.
//!
//! Two services may select different binaries of one local package; each
//! selection must keep its own prepared contract products and the composing
//! brain must bind each instance to its own binary's endpoint set.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[path = "support/sdk.rs"]
mod sdk;

fn phoxal_dep(features: &str) -> String {
    format!(
        "phoxal = {{ path = {:?}, default-features = false, features = [{features}] }}\n",
        sdk::sdk_root()
    )
}

const ALPHA_BIN: &str = r#"//! Alpha binary: one projected output and one served probe operation.
use phoxal::contracts::component::battery::BatterySample;
use phoxal::contracts::component::lidar::LaserScan;
use phoxal::contracts::geometry::Pose;
use phoxal::contracts::{Empty, Latest, RequestReply};

#[phoxal::message(package = "proof.multi.v1")]
pub struct AlphaState {
    #[phoxal(tag = 1)]
    pub value: u64,
    #[phoxal(tag = 2)]
    pub pose: Option<Pose>,
    #[phoxal(tag = 3)]
    pub battery: Option<BatterySample>,
    #[phoxal(tag = 4)]
    pub scan: Option<LaserScan>,
}

#[phoxal::endpoints]
pub struct AlphaApi {
    #[phoxal::output(projection = state, max_bytes = 512)]
    alpha_status: Latest<AlphaState>,

    #[phoxal::operation(
        contract = "proof.multi.v1.Probe",
        max_items = 4,
        max_bytes = 512
    )]
    probe: RequestReply<Empty, AlphaState>,
}

pub struct Alpha {
    beats: u64,
}

#[phoxal::runtime(contract = AlphaApi, period_ms = 20)]
impl Alpha {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self { beats: 0 })
    }

    #[handle(probe)]
    fn probe(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>, _request: Empty) -> phoxal::Result<AlphaState> {
        Ok(AlphaState {
            value: self.beats,
            pose: None,
            battery: None,
            scan: None,
        })
    }

    #[step]
    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }

    #[publish(alpha_status)]
    fn alpha_status(&self) -> AlphaState {
        AlphaState {
            value: self.beats,
            pose: None,
            battery: None,
            scan: None,
        }
    }
}

fn main() {}
"#;

const BETA_BIN: &str = r#"//! Beta binary: a differently named endpoint with a different payload.
use phoxal::contracts::Latest;

#[phoxal::message(package = "proof.multi.v1")]
pub struct BetaState {
    #[phoxal(tag = 1)]
    pub label: String,
}

#[phoxal::endpoints]
pub struct BetaApi {
    #[phoxal::output(projection = state, max_bytes = 512)]
    beta_status: Latest<BetaState>,
}

pub struct Beta {
    beats: u64,
}

#[phoxal::runtime(contract = BetaApi, period_ms = 20)]
impl Beta {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self { beats: 0 })
    }

    #[step]
    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }

    #[publish(beta_status)]
    fn beta_status(&self) -> BetaState {
        BetaState {
            label: format!("beta-{}", self.beats),
        }
    }
}

fn main() {}
"#;

const BRAIN_MAIN: &str = r#"//! Composes both binaries of one package through their prepared contracts.
phoxal::api!();

use phoxal::contracts::{Empty, RequestReply};

/// The brain's own Rust contract: the probe response is a prepared
/// participant's payload, carried through its generated type and explicit
/// identity.
#[phoxal::endpoints]
pub struct BrainApi {
    #[phoxal::call(
        contract = "proof.multi.v1.Probe",
        response = "proof.multi.v1.AlphaState",
        max_items = 4,
        max_bytes = 512
    )]
    probe: RequestReply<Empty, api::first::AlphaState>,
}

pub struct Brain {
    beats: u64,
}

#[phoxal::runtime(contract = BrainApi, period_ms = 50)]
impl Brain {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self { beats: 0 })
    }

    #[step]
    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }
}

fn main() -> phoxal::Result<()> {
    // Each selection binds its own binary's endpoint set; both constants
    // resolve only when the two prepared contracts stayed distinct.
    let _ = (
        api::first::ALPHA_STATUS,
        api::second::BETA_STATUS,
    );
    phoxal::runtime::run::<Brain>()
}
"#;

/// The Git-fixture robot's brain: a real runtime brain (check validates
/// the compiled brain even with no connections) whose heartbeat output is
/// brain-owned, plus the remote prepared binding reference.
const GIT_BRAIN_MAIN: &str = r#"//! Composes the Git-prepared provider through its extracted products.
phoxal::api!();

use phoxal::contracts::Latest;

#[phoxal::message(package = "proof.git.v1")]
pub struct Heartbeat {
    #[phoxal(tag = 1)]
    pub beats: u64,
}

#[phoxal::endpoints]
pub struct BrainApi {
    #[phoxal::output(projection = state, max_bytes = 256)]
    heartbeat: Latest<Heartbeat>,
}

pub struct Brain {
    beats: u64,
}

#[phoxal::runtime(contract = BrainApi, period_ms = 50)]
impl Brain {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self { beats: 0 })
    }

    #[step]
    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }

    #[publish(heartbeat)]
    fn heartbeat(&self) -> Heartbeat {
        Heartbeat { beats: self.beats }
    }
}

fn main() -> phoxal::Result<()> {
    let _ = api::provider::ALPHA_STATUS;
    phoxal::runtime::run::<Brain>()
}
"#;

#[test]
fn two_binaries_of_one_package_keep_distinct_prepared_contracts()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let robot = root.join("robot");
    let provider = robot.join("provider");
    fs::create_dir_all(provider.join("src"))?;
    fs::create_dir_all(robot.join("src"))?;
    fs::write(
        provider.join("Cargo.toml"),
        format!(
            "[package]\nname = \"proof-multi-provider\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [[bin]]\nname = \"alpha\"\npath = \"src/alpha.rs\"\n\
             [[bin]]\nname = \"beta\"\npath = \"src/beta.rs\"\n\
             [[bin]]\nname = \"sensor-a\"\npath = \"src/alpha.rs\"\n\
             [[bin]]\nname = \"sensor_a\"\npath = \"src/beta.rs\"\n\
             [dependencies]\n{}",
            phoxal_dep("\"runtime\"")
        ),
    )?;
    fs::write(provider.join("src/alpha.rs"), ALPHA_BIN)?;
    fs::write(provider.join("src/beta.rs"), BETA_BIN)?;
    fs::write(
        robot.join("Cargo.toml"),
        format!(
            "[package]\nname = \"proof-multi-robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\
             [dependencies]\n{}\n\
             [build-dependencies]\n{}",
            phoxal_dep("\"runtime\""),
            phoxal_dep("\"build\"")
        ),
    )?;
    fs::write(
        robot.join("build.rs"),
        "fn main() -> Result<(), phoxal::build::Error> {\n    phoxal::build::api(phoxal::build::BuildApiConfig::default())\n}\n",
    )?;
    fs::write(robot.join("src/main.rs"), BRAIN_MAIN)?;
    let robot_yaml = robot.join("robot.yaml");
    let valid_wiring = "schema: phoxal/robot/v0\nrobot: { id: proof-multi-robot }\nsupervisor:\n  source: { path: supervisor }\nservices:\n  first:\n    source: { path: provider }\n    binary: alpha\n  second:\n    source: { path: provider }\n    binary: beta\n  third:\n    source: { path: provider }\n    binary: sensor-a\n  fourth:\n    source: { path: provider }\n    binary: sensor_a\nconnections:\n  - from: first.probe\n    to: brain.probe\n";
    fs::write(&robot_yaml, valid_wiring)?;

    let prepare = invoke(&robot, &["prepare", "--offline"]);
    assert!(
        prepare.status.success(),
        "prepare must build both binaries:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&prepare.stdout),
        String::from_utf8_lossy(&prepare.stderr),
    );

    // Each selection keeps its own prepared directory, keyed by the shared
    // identity scheme the build helper consumes.
    let local = phoxal_build::prepared_input_root(&robot)?;
    let alpha_dir = prepared_dir_for(&local, "bin-alpha");
    let beta_dir = prepared_dir_for(&local, "bin-beta");
    for (label, dir) in [("alpha", &alpha_dir), ("beta", &beta_dir)] {
        assert!(
            dir.join("contract.json").is_file(),
            "the {label} selection must keep its own prepared contract at {}",
            dir.display()
        );
    }
    // Fold-colliding binary names (`sensor-a` vs `sensor_a`) fold to the
    // same readable suffix; they must still occupy TWO directories whose
    // recorded exact binaries are distinct — the complete-identity
    // digest separates them.
    let mut folded: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&local)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with("--bin-sensor-a")
        {
            folded.push(entry.path());
        }
    }
    assert_eq!(
        folded.len(),
        2,
        "fold-colliding binaries keep two directories"
    );
    let mut recorded = Vec::new();
    for dir in &folded {
        let contract = fs::read_to_string(dir.join("contract.json"))?;
        let binary = contract
            .split("\"binary\": ")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_default()
            .to_owned();
        recorded.push(binary);
    }
    recorded.sort();
    assert_eq!(
        recorded,
        vec!["\"sensor-a\"", "\"sensor_a\""],
        "each directory records its own exact binary"
    );
    let alpha_endpoints = fs::read_to_string(alpha_dir.join("contract.json"))?;
    let beta_endpoints = fs::read_to_string(beta_dir.join("contract.json"))?;
    assert!(
        alpha_endpoints.contains("alpha_status"),
        "the alpha selection names its own endpoint"
    );
    assert!(
        beta_endpoints.contains("beta_status"),
        "the beta selection names its own endpoint"
    );

    // The composing brain generates both instance bindings, compiles, and
    // its own compiled contract validates against the real graph.
    let check = invoke(&robot, &["check", "--offline"]);
    assert!(
        check.status.success(),
        "check must validate the valid wiring:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );
    assert!(
        String::from_utf8_lossy(&check.stderr).contains("the compiled brain"),
        "check reports the compiled brain contract"
    );

    // The generated bindings route the participant's nested geometry and
    // additional component vocabulary to the SDK's canonical types instead
    // of duplicating them into the robot API.
    let mut generated_packages = BTreeMap::new();
    for entry in fs::read_dir(shared_target_dir().join("debug/build"))
        .expect("the checked robot keeps its build directory")
        .filter_map(|entry| Some(entry.ok()?.path().join("out/phoxal-api/merged")))
        .filter(|out| out.is_dir())
    {
        for file in fs::read_dir(&entry).expect("generated package files") {
            let path = file.expect("generated package file").path();
            let name = path
                .file_name()
                .expect("package file name")
                .to_string_lossy()
                .to_string();
            generated_packages.insert(name, fs::read_to_string(&path).expect("generated source"));
        }
    }
    let sources = generated_packages.values().cloned().collect::<String>();
    assert!(
        sources.contains("::phoxal::contracts::geometry::Pose"),
        "generated AlphaState fields reference the SDK geometry vocabulary:\n{sources}"
    );
    assert!(
        sources.contains("::phoxal::contracts::component::battery::BatterySample"),
        "generated bindings reference the SDK battery vocabulary:\n{sources}"
    );
    assert!(
        sources.contains("::phoxal::contracts::component::lidar::LaserScan"),
        "generated bindings reference the SDK lidar vocabulary:\n{sources}"
    );
    for duplicated in [
        "phoxal.geometry.v1.rs",
        "phoxal.component.battery.v1.rs",
        "phoxal.component.lidar.v1.rs",
    ] {
        assert!(
            !generated_packages.contains_key(duplicated),
            "the SDK-owned package {duplicated} must not be duplicated into the robot API"
        );
    }

    // A connection naming a brain endpoint the compiled brain does not
    // declare must fail: the brain's contract is never inferred from the
    // connection itself.
    fs::write(
        &robot_yaml,
        valid_wiring.replace("to: brain.probe", "to: brain.missing"),
    )?;
    let missing = invoke(&robot, &["check", "--offline"]);
    assert!(
        !missing.status.success(),
        "a nonexistent brain endpoint must fail check"
    );
    assert!(String::from_utf8_lossy(&missing.stderr).contains("absent from the runtime artifact"));

    // A real brain requirement bound to an incompatible producer must
    // fail: the probe call cannot be satisfied by a data output.
    fs::write(
        &robot_yaml,
        valid_wiring.replace("from: first.probe", "from: first.alpha_status"),
    )?;
    let mismatched = invoke(&robot, &["check", "--offline"]);
    assert!(
        !mismatched.status.success(),
        "a mismatched brain requirement must fail check"
    );

    // The brain still compiles its owned payload when no connection mentions
    // it. An unwired Latest input is explicitly optional; authored connections
    // make that input required, as covered by the admitted runtime tests.
    fs::write(
        robot.join("src/main.rs"),
        BRAIN_MAIN.replace(
            "    probe: RequestReply<Empty, api::first::AlphaState>,\n}",
            "    probe: RequestReply<Empty, api::first::AlphaState>,\n}\n\n\
             /// A brain-owned payload, so the input is fully Rust-authored.\n\
             #[phoxal::message(package = \"proof.brain.v1\")]\n\
             pub struct Telemetry {\n    #[phoxal(tag = 1)]\n    pub value: u64,\n}\n",
        )
        .replace(
            "use phoxal::contracts::{Empty, RequestReply};",
            "use phoxal::contracts::{Empty, Latest, RequestReply};",
        )
        .replace(
            "    probe: RequestReply<Empty, api::first::AlphaState>,",
            "    probe: RequestReply<Empty, api::first::AlphaState>,\n\n    #[phoxal::input(max_age_ms = 100, max_bytes = 256)]\n    telemetry: Latest<Telemetry>,",
        ),
    )?;
    let no_brain_connections = valid_wiring.replace(
        "connections:\n  - from: first.probe\n    to: brain.probe\n",
        "connections: []\n",
    );
    assert!(
        !no_brain_connections.contains("brain."),
        "the unwired probe wiring must not mention the brain at all"
    );
    fs::write(&robot_yaml, no_brain_connections)?;
    let unwired = invoke(&robot, &["check", "--offline"]);
    assert!(
        unwired.status.success(),
        "an unwired Latest input must compile without inventing a producer: {}",
        String::from_utf8_lossy(&unwired.stderr)
    );
    Ok(())
}

/// Locates the one prepared directory whose identity ends with `suffix`.
fn prepared_dir_for(local: &std::path::Path, suffix: &str) -> PathBuf {
    let mut matches = Vec::new();
    if let Ok(entries) = fs::read_dir(local) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(suffix) {
                matches.push(entry.path());
            }
        }
    }
    assert_eq!(
        matches.len(),
        1,
        "exactly one prepared directory ends with {suffix}"
    );
    matches.remove(0)
}

/// Runs the compiled `cargo-phoxal` binary with an isolated Phoxal home.
/// One stable dependency-build tree under the workspace's ignored
/// `target/` directory, shared with `prepare_cycles`' suite: every nested
/// fixture resolves the same workspace phoxal path dependency plus the
/// same crates.io resolution, so the first fixture in a cold run pays the
/// build once and every later fixture, suite, and run reuses it. No
/// per-process directories are created, so repeated runs cannot
/// accumulate retained trees; `cargo clean` reclaims the space; and
/// cargo's target-dir file lock keeps concurrent fixture builds correct.
/// Fixture sources still rebuild on their own edits, and the cold path
/// itself stays proven by a clean checkout's first run. Nested builds
/// carry no debugging value, so they run without incremental compilation
/// and with line-tables-only debug info to keep the tree small.
fn shared_target_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target/suites")
        .join("composition")
}

fn invoke(cwd: &std::path::Path, args: &[&str]) -> std::process::Output {
    fs::create_dir_all(cwd.join(".cargo"))
        .unwrap_or_else(|error| panic!("fixture Cargo configuration: {error}"));
    fs::write(
        cwd.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir = {:?}\n",
            shared_target_dir().to_string_lossy()
        ),
    )
    .unwrap_or_else(|error| panic!("fixture target root: {error}"));
    Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(cwd)
        .env("PHOXAL_HOME", cwd.join(".phoxal-home"))
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawn cargo-phoxal: {error}"))
}

#[test]
fn git_rust_contract_participant_prepares_from_the_installed_artifact()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let source = root.join("source");
    let robot = root.join("robot");
    fs::create_dir_all(source.join("src"))?;
    fs::create_dir_all(robot.join("src"))?;
    // A Rust-contract package: no api/ directory, no service.yaml, no build
    // script — the endpoint surface lives in the compiled artifact.
    fs::write(
        source.join("Cargo.toml"),
        format!(
            "[package]\nname = \"proof-git-rust-provider\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\
             [dependencies]\n{}",
            phoxal_dep("\"runtime\"")
        ),
    )?;
    fs::write(source.join("src/main.rs"), ALPHA_BIN)?;
    fs::write(
        robot.join("Cargo.toml"),
        format!(
            "[package]\nname = \"proof-git-rust-robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\
             [dependencies]\n{}\n\
             [build-dependencies]\n{}",
            phoxal_dep("\"runtime\""),
            phoxal_dep("\"build\"")
        ),
    )?;
    fs::write(
        robot.join("build.rs"),
        "fn main() -> Result<(), phoxal::build::Error> {\n    phoxal::build::api(phoxal::build::BuildApiConfig::default())\n}\n",
    )?;
    fs::write(robot.join("src/main.rs"), GIT_BRAIN_MAIN)?;

    // The provider carries real framework dependencies, so lockfile
    // resolution runs against the runner's cargo cache instead of an
    // isolated empty home; `cargo install --locked` requires the committed
    // lock file.
    let lock = Command::new("cargo")
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(source.join("Cargo.toml"))
        .current_dir(&source)
        .output()?;
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let git_init = Command::new("git")
        .args([
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
        ])
        .arg("init")
        .current_dir(&source)
        .output()?;
    assert!(
        git_init.status.success(),
        "{}",
        String::from_utf8_lossy(&git_init.stderr)
    );
    let git_add = Command::new("git")
        .args([
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
        ])
        .args(["add", "."])
        .current_dir(&source)
        .output()?;
    assert!(
        git_add.status.success(),
        "{}",
        String::from_utf8_lossy(&git_add.stderr)
    );
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
            "commit",
            "--quiet",
            "-m",
            "proof",
        ])
        .current_dir(&source)
        .output()?;
    assert!(commit.status.success());
    let revision = Command::new("git")
        .arg("rev-parse")
        .arg("HEAD")
        .current_dir(&source)
        .output()?;
    assert!(revision.status.success());
    let revision = String::from_utf8(revision.stdout)?.trim().to_owned();
    fs::write(
        robot.join("robot.yaml"),
        format!(
            "schema: phoxal/robot/v0\nrobot: {{ id: proof-git-rust-robot }}\nsupervisor:\n  source: {{ path: supervisor }}\nservices:\n  provider:\n    source:\n      git:\n        name: proof-git-rust-provider\n        url: file://{}\n        rev: {revision}\nconnections: []\n",
            source.display()
        ),
    )?;

    // A local file:// Git source still requires a non-offline fetch,
    // exactly like the declaration-based Git flow.
    let prepare = invoke(&robot, &["prepare"]);
    assert!(
        prepare.status.success(),
        "a Git Rust-contract package prepares from its installed artifact:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&prepare.stdout),
        String::from_utf8_lossy(&prepare.stderr),
    );
    let prepared_root = phoxal_build::prepared_input_root(&robot)?;
    let mut matches = Vec::new();
    for entry in fs::read_dir(&prepared_root)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("git-proof-git-rust-provider@") {
            matches.push(entry.path());
        }
    }
    assert_eq!(matches.len(), 1, "one Git prepared contract");
    let endpoints = fs::read_to_string(matches[0].join("contract.json"))?;
    assert!(
        endpoints.contains("alpha_status"),
        "the Git selection's prepared contract names its own endpoint"
    );

    // The composing brain generates the remote instance binding.
    let check = invoke(&robot, &["check", "--offline"]);
    assert!(
        check.status.success(),
        "check compiles the Git-prepared instance API:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&check.stdout),
        String::from_utf8_lossy(&check.stderr),
    );

    // A second prepare is a warm no-op over the same installation.
    let warm = invoke(&robot, &["prepare", "--offline"]);
    assert!(warm.status.success());
    assert!(
        String::from_utf8_lossy(&warm.stdout).is_empty(),
        "the warm prepare reports no installation change"
    );
    Ok(())
}
