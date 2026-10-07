//! Regressions for editing a prepared composition and re-preparing it.
//!
//! A robot's own recorded brain products and its participants' freshly
//! prepared products must never conflict: the recorded self products feed
//! only bindings, never definitions, so editing the brain's messages or
//! selecting a new participant revision prepares and checks green without
//! deleting any cache by hand.

use std::fs;
use std::process::Command;

#[path = "support/sdk.rs"]
mod sdk;

fn phoxal_dep(features: &str) -> String {
    format!(
        "phoxal = {{ path = {:?}, default-features = false, features = [{features}] }}\n",
        sdk::sdk_root()
    )
}

/// Runs the compiled `cargo-phoxal` binary with an isolated Phoxal home.
/// One stable dependency-build tree under the workspace's ignored
/// `target/` directory, shared with `prepared_selection`'s suite: every
/// nested fixture resolves the same workspace phoxal path dependency plus
/// the same crates.io resolution, so the first fixture in a cold run pays
/// the build once and every later fixture, suite, and run reuses it. No
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
    Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(cwd)
        .env("PHOXAL_HOME", cwd.join(".phoxal-home"))
        .env("CARGO_TARGET_DIR", shared_target_dir())
        .env("CARGO_INCREMENTAL", "0")
        .env("CARGO_PROFILE_DEV_DEBUG", "line-tables-only")
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawn cargo-phoxal: {error}"))
}

fn report(output: &std::process::Output) -> String {
    format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

const BUILD_RS: &str = "fn main() -> Result<(), phoxal::build::Error> {\n    phoxal::build::api(phoxal::build::BuildApiConfig::default())\n}\n";

/// One participant: a projected status output over an authored payload.
const PROVIDER_BIN: &str = r#"//! Provider: one projected output over an authored package payload.
use phoxal::contracts::Latest;

#[phoxal::message(package = "proof.cycle.v1")]
pub struct ProviderState {
    #[phoxal(tag = 1)]
    pub value: u64,
}

#[phoxal::endpoints]
pub struct ProviderApi {
    #[phoxal::output(projection = state, max_bytes = 512)]
    provider_status: Latest<ProviderState>,
}

pub struct Provider {
    beats: u64,
}

#[phoxal::runtime(contract = ProviderApi, period_ms = 20)]
impl Provider {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self { beats: 0 })
    }

    #[step]
    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }

    #[publish(provider_status)]
    fn provider_status(&self) -> ProviderState {
        ProviderState { value: self.beats }
    }
}

fn main() {}
"#;

/// The brain: consumes the participant's status through an identity
/// connection and serves an authored heartbeat, so the compiled brain
/// retains definitions of its own messages beside the participant's.
/// An empty `heartbeat_package` keeps the message private; otherwise the
/// message shares that package namespace with the participant's payload.
fn brain_main(payload: &str, heartbeat_package: &str) -> String {
    let message = if heartbeat_package.is_empty() {
        "#[phoxal::message]".to_owned()
    } else {
        format!("#[phoxal::message(package = \"{heartbeat_package}\")]")
    };
    let beat_value = if payload == "String" {
        "format!(\"{}\", self.beats)".to_owned()
    } else {
        "self.beats".to_owned()
    };
    format!(
        r#"//! Brain: binds the participant's status and serves an authored
//! heartbeat report.
phoxal::api!();

use phoxal::contracts::{{Empty, Latest, RequestReply}};
use phoxal::runtime::Context;

use crate::api::provider::ProviderState;

{message}
pub struct Heartbeat {{
    #[phoxal(tag = 1)]
    pub beats: {payload},
}}

#[phoxal::endpoints(package = "proof.cycle.brain.v1")]
pub struct BrainApi {{
    #[phoxal::input(max_age_ms = 100, max_bytes = 512)]
    telemetry: Latest<ProviderState>,

    #[phoxal::operation(max_items = 4, max_bytes = 256)]
    read: RequestReply<Empty, Heartbeat>,
}}

pub struct Brain {{
    beats: u64,
}}

#[phoxal::runtime(contract = BrainApi, period_ms = 50)]
impl Brain {{
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {{
        Ok(Self {{ beats: 0 }})
    }}

    #[handle(read)]
    fn read(&mut self, _ctx: &mut Context<'_, Self>, _request: Empty) -> phoxal::Result<Heartbeat> {{
        Ok(Heartbeat {{ beats: {beat_value} }})
    }}

    #[step]
    fn advance(&mut self, ctx: &mut Context<'_, Self>) -> phoxal::Result<()> {{
        let _ = ctx.telemetry().value();
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }}
}}

fn main() -> phoxal::Result<()> {{
    phoxal::runtime::run::<Brain>()
}}
"#
    )
}

/// Stages the fixture supervisor crate into one prepared-cycle robot so
/// the authored `supervisor.source.path` selection resolves offline.
fn stage_supervisor(robot: &std::path::Path) -> std::io::Result<()> {
    let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/robot-base/supervisor");
    fs::create_dir_all(robot.join("supervisor/src"))?;
    // The staged crate's phoxal path dependency must resolve from the
    // tempdir, so the framework phoxal crate is referenced absolutely.
    let phoxal_path = sdk::sdk_root();
    let manifest = format!(
        "[package]\nname = \"phoxal-supervisor\"\nversion = \"0.0.0-dev.8\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nphoxal = {{ path = {:?}, default-features = false }}\n\n[[bin]]\nname = \"phoxal-supervisor\"\npath = \"src/main.rs\"\n",
        phoxal_path
    );
    fs::write(robot.join("supervisor/Cargo.toml"), manifest)?;
    fs::copy(
        source.join("src/main.rs"),
        robot.join("supervisor/src/main.rs"),
    )?;
    Ok(())
}

fn robot_manifest() -> String {
    format!(
        "[package]\nname = \"proof-cycle-robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n\
         [dependencies]\n{}\n\
         [build-dependencies]\n{}",
        phoxal_dep("\"runtime\""),
        phoxal_dep("\"build\"")
    )
}

#[test]
fn brain_message_edit_keeps_preparation_green() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let robot = root.join("robot");
    let provider = robot.join("provider");
    fs::create_dir_all(provider.join("src"))?;
    fs::create_dir_all(robot.join("src"))?;
    fs::write(provider.join("Cargo.toml"), provider_manifest())?;
    fs::write(provider.join("src/main.rs"), PROVIDER_BIN)?;
    fs::write(robot.join("Cargo.toml"), robot_manifest())?;
    stage_supervisor(&robot)?;
    fs::write(robot.join("build.rs"), BUILD_RS)?;
    fs::write(robot.join("src/main.rs"), brain_main("u64", ""))?;
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  id: proof-cycle-robot\n  services:\n    provider:\n      source:\n        path: provider\n  brain:\n    bindings:\n      telemetry:\n      - provider.provider_status\nsupervisor:\n  source:\n    path: supervisor\n",
    )?;

    let first = invoke(&robot, &["prepare", "--offline"]);
    assert!(
        first.status.success(),
        "the first preparation succeeds:\n{}",
        report(&first)
    );

    // Edit the brain's own message: the field's type changes, so the
    // brain's previously recorded copy of `Heartbeat` no longer matches
    // the authored definition. Re-preparation must refresh the record
    // instead of rejecting the conflict.
    fs::write(robot.join("src/main.rs"), brain_main("String", ""))?;
    let second = invoke(&robot, &["prepare", "--offline"]);
    assert!(
        second.status.success(),
        "editing the brain's own message keeps preparation green:\n{}",
        report(&second)
    );

    let check = invoke(&robot, &["check", "--offline"]);
    assert!(
        check.status.success(),
        "check validates the edited brain against the refreshed record:\n{}",
        report(&check)
    );

    // A further warm preparation reports no change: the refreshed record
    // is stable, not rewritten on every pass.
    let warm = invoke(&robot, &["prepare", "--offline"]);
    assert!(warm.status.success(), "{}", report(&warm));
    assert!(
        String::from_utf8_lossy(&warm.stdout).is_empty(),
        "the warm prepare reports no change:\n{}",
        report(&warm)
    );
    Ok(())
}

/// The brain authors a second message in the same package namespace the
/// participant provides. The recorded self products must skip only the
/// definitions the participant supplies — not the whole shared package —
/// so an unchanged repeated preparation keeps every type bound.
#[test]
fn shared_namespace_repeated_preparation_stays_green() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let robot = root.join("robot");
    let provider = robot.join("provider");
    fs::create_dir_all(provider.join("src"))?;
    fs::create_dir_all(robot.join("src"))?;
    fs::write(provider.join("Cargo.toml"), provider_manifest())?;
    fs::write(provider.join("src/main.rs"), PROVIDER_BIN)?;
    fs::write(robot.join("Cargo.toml"), robot_manifest())?;
    stage_supervisor(&robot)?;
    fs::write(robot.join("build.rs"), BUILD_RS)?;
    // `Heartbeat` shares `proof.cycle.v1` with the provider's payload.
    fs::write(
        robot.join("src/main.rs"),
        brain_main("u64", "proof.cycle.v1"),
    )?;
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  id: proof-cycle-robot\n  services:\n    provider:\n      source:\n        path: provider\n  brain:\n    bindings:\n      telemetry:\n      - provider.provider_status\nsupervisor:\n  source:\n    path: supervisor\n",
    )?;

    let first = invoke(&robot, &["prepare", "--offline"]);
    assert!(
        first.status.success(),
        "the first preparation succeeds:\n{}",
        report(&first)
    );

    // Nothing changed: the second preparation must keep the brain's own
    // `Heartbeat` beside the participant-provided `ProviderState`.
    let second = invoke(&robot, &["prepare", "--offline"]);
    assert!(
        second.status.success(),
        "an unchanged repeated preparation keeps every shared-namespace type:\n{}",
        report(&second)
    );
    let check = invoke(&robot, &["check", "--offline"]);
    assert!(
        check.status.success(),
        "check validates both definitions of the shared namespace:\n{}",
        report(&check)
    );
    Ok(())
}

fn provider_manifest() -> String {
    format!(
        "[package]\nname = \"proof-cycle-provider\"\nversion = \"0.1.0\"\npublish = false\nedition = \"2024\"\n\
         [dependencies]\n{}",
        phoxal_dep("\"runtime\"")
    )
}

/// The provider with one field added: the next revision of the same
/// package, selected by pinning the new commit.
const PROVIDER_BIN_REVISED: &str = r#"//! Provider revision B: the payload gains a field.
use phoxal::contracts::Latest;

#[phoxal::message(package = "proof.cycle.v1")]
pub struct ProviderState {
    #[phoxal(tag = 1)]
    pub value: u64,
    #[phoxal(tag = 2)]
    pub label: String,
}

#[phoxal::endpoints]
pub struct ProviderApi {
    #[phoxal::output(projection = state, max_bytes = 512)]
    provider_status: Latest<ProviderState>,
}

pub struct Provider {
    beats: u64,
}

#[phoxal::runtime(contract = ProviderApi, period_ms = 20)]
impl Provider {
    #[init]
    fn new(_config: ()) -> phoxal::Result<Self> {
        Ok(Self { beats: 0 })
    }

    #[step]
    fn advance(&mut self, _ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }

    #[publish(provider_status)]
    fn provider_status(&self) -> ProviderState {
        ProviderState {
            value: self.beats,
            label: format!("step-{}", self.beats),
        }
    }
}

fn main() {}
"#;

fn git(source: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
        ])
        .args(args)
        .current_dir(source)
        .output()
        .unwrap_or_else(|error| panic!("spawn git: {error}"))
}

#[test]
fn git_participant_revision_bump_keeps_preparation_green() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let source = root.join("source");
    let robot = root.join("robot");
    fs::create_dir_all(source.join("src"))?;
    fs::create_dir_all(robot.join("src"))?;
    fs::write(source.join("Cargo.toml"), provider_manifest())?;
    fs::write(source.join("src/main.rs"), PROVIDER_BIN)?;
    fs::write(robot.join("Cargo.toml"), robot_manifest())?;
    stage_supervisor(&robot)?;
    fs::write(robot.join("build.rs"), BUILD_RS)?;
    fs::write(robot.join("src/main.rs"), brain_main("u64", ""))?;

    // The provider carries real framework dependencies, so lockfile
    // resolution runs against the runner's cargo cache.
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
    assert!(git(&source, &["init"]).status.success());
    assert!(git(&source, &["add", "."]).status.success());
    assert!(
        git(&source, &["commit", "--quiet", "-m", "revision-a"])
            .status
            .success()
    );
    let revision = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)?
        .trim()
        .to_owned();

    let robot_yaml = |revision: &str| {
        format!(
            "schema: phoxal/robot/v0\nrobot:\n  id: proof-cycle-robot\n  services:\n    provider:\n      source:\n        git:\n          name: proof-cycle-provider\n          url: file://{}\n          rev: {revision}\n  brain:\n    bindings:\n      telemetry:\n      - provider.provider_status\nsupervisor:\n  source:\n    path: supervisor\n",
            source.display()
        )
    };
    fs::write(robot.join("robot.yaml"), robot_yaml(&revision))?;

    // A local file:// Git source still requires a non-offline fetch,
    // exactly like the declaration-based Git flow.
    let first = invoke(&robot, &["prepare"]);
    assert!(
        first.status.success(),
        "the pinned Git revision prepares:\n{}",
        report(&first)
    );

    // Publish revision B of the same package — its payload gains a field —
    // and select it. The brain's recorded copy of the revision-A payload
    // must not veto the freshly prepared revision-B definition.
    fs::write(source.join("src/main.rs"), PROVIDER_BIN_REVISED)?;
    assert!(git(&source, &["add", "."]).status.success());
    assert!(
        git(&source, &["commit", "--quiet", "-m", "revision-b"])
            .status
            .success()
    );
    let next = String::from_utf8(git(&source, &["rev-parse", "HEAD"]).stdout)?
        .trim()
        .to_owned();
    fs::write(robot.join("robot.yaml"), robot_yaml(&next))?;

    let second = invoke(&robot, &["prepare"]);
    assert!(
        second.status.success(),
        "selecting the new revision prepares without hand-deleting caches:\n{}",
        report(&second)
    );
    let check = invoke(&robot, &["check", "--offline"]);
    assert!(
        check.status.success(),
        "check validates the brain against the revised participant:\n{}",
        report(&check)
    );
    Ok(())
}

/// A brain converts canonical provider payloads in its ordinary step and
/// serves the stamped result through its normal endpoint contract.
/// Nested messages and enums exercise the complete retained schema closure.
fn converting_brain_main() -> String {
    r#"//! Ordinary typed conversion in the brain's canonical runtime.
mod conversions;

phoxal::api!();

use phoxal::contracts::Latest;

#[phoxal::message]
pub struct TelemetryDetail {
    #[phoxal(tag = 1)]
    pub label: String,
}

#[phoxal::message]
pub enum TelemetryNote {
    #[phoxal(tag = 1)]
    Plain(String),
    #[phoxal(tag = 2)]
    Sealed(TelemetryDetail),
}

#[phoxal::message]
pub enum TelemetryGrade {
    Unspecified = 0,
    Trusted = 1,
}

#[phoxal::message]
pub struct TelemetryIn {
    #[phoxal(tag = 1)]
    pub beats: u64,
    #[phoxal(tag = 2)]
    pub detail: Option<TelemetryDetail>,
    #[phoxal(tag = 3)]
    pub note: Option<TelemetryNote>,
    #[phoxal(tag = 4)]
    pub grade: TelemetryGrade,
}

#[phoxal::endpoints(package = "proof.cycle.brain.v1")]
pub struct BrainApi {
    #[phoxal::input(max_age_ms = 100, max_bytes = 512)]
    telemetry: Latest<crate::api::provider::ProviderState>,
    #[phoxal::output(stamped, max_bytes = 512)]
    converted: Latest<TelemetryIn>,
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
    fn advance(&mut self, ctx: &mut phoxal::runtime::Context<'_, Self>) -> phoxal::Result<()> {
        if ctx.telemetry().is_fresh() && let Some(sample) = ctx.telemetry().sample() {
            let converted = sample.payload().clone().try_into()?;
            let stamp = sample.stamp().clone();
            ctx.publish_converted(phoxal::runtime::Sample::new(converted, stamp))?;
        }
        self.beats = self.beats.saturating_add(1);
        Ok(())
    }
}

fn main() -> phoxal::Result<()> {
    phoxal::runtime::run::<Brain>()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use phoxal::runtime::{ExecutionTime, Harness, ObservationStamp, Sample};

    fn sample(value: u64, nanos: u64) -> Sample<crate::api::provider::ProviderState> {
        Sample::new(crate::api::provider::ProviderState { value }, ObservationStamp::new("provider.provider_status", ExecutionTime::from_nanos(nanos), Some(19)))
    }

    #[test]
    fn conversion_preserves_capture_and_reset_clears_it() -> phoxal::Result<()> {
        let mut brain = Harness::<Brain>::new(())?;
        let input = sample(42, 0);
        let stamp = input.stamp().clone();
        brain.inject_telemetry(input)?;
        brain.advance_to(Duration::ZERO)?;
        let output = brain.converted_sample().expect("accepted converted sample");
        assert_eq!(output.payload().beats, 42);
        assert_eq!(output.stamp(), &stamp);
        brain.advance_to(Duration::from_millis(150))?;
        assert_eq!(brain.converted_sample().expect("retained publication").stamp(), &stamp);
        brain.reset(())?;
        assert!(brain.converted_sample().is_none());
        brain.advance_to(Duration::from_millis(150))?;
        assert!(brain.converted_sample().is_none(), "old input cannot cross reset");
        Ok(())
    }

    #[test]
    fn conversion_failure_rejects_the_candidate_and_requires_reset() -> phoxal::Result<()> {
        let mut brain = Harness::<Brain>::new(())?;
        brain.inject_telemetry(sample(7, 0))?;
        brain.advance_to(Duration::ZERO)?;
        brain.inject_telemetry(sample(u64::MAX, 50_000_000))?;
        let error = brain.advance_to(Duration::from_millis(50)).expect_err("conversion must fail");
        assert!(error.to_string().contains("unrepresentable telemetry"), "{error}");
        assert_eq!(brain.converted().expect("last accepted publication").beats, 7);
        assert!(brain.advance_to(Duration::from_millis(100)).is_err());
        brain.reset(())?;
        brain.inject_telemetry(sample(8, 100_000_000))?;
        brain.advance_to(Duration::from_millis(100))?;
        assert_eq!(brain.converted().expect("fresh execution publication").beats, 8);
        Ok(())
    }
}
"#
    .to_owned()
}

/// Ordinary robot-owned conversion over canonical provider bindings.
const CONVERSIONS: &str = r#"//! Robot-owned conversions for composed telemetry expectations.

use crate::TelemetryIn;
use crate::api::provider::ProviderState;

impl TryFrom<ProviderState> for TelemetryIn {
    type Error = std::io::Error;
    fn try_from(source: ProviderState) -> Result<TelemetryIn, Self::Error> {
        if source.value == u64::MAX {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "unrepresentable telemetry"));
        }
        Ok(TelemetryIn { beats: source.value, ..Default::default() })
    }
}
"#;

/// Complete clean preparation and bundle admission without a self schema,
/// secondary process, conversion sidecar, or alternate runtime entry.
#[test]
fn canonical_conversion_contract_stays_consumable() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path();
    let robot = root.join("robot");
    let provider = robot.join("provider");
    fs::create_dir_all(provider.join("src"))?;
    fs::create_dir_all(robot.join("src"))?;
    fs::write(provider.join("Cargo.toml"), provider_manifest())?;
    fs::write(provider.join("src/main.rs"), PROVIDER_BIN)?;
    fs::write(robot.join("Cargo.toml"), robot_manifest())?;
    stage_supervisor(&robot)?;
    fs::write(robot.join("build.rs"), BUILD_RS)?;
    fs::write(robot.join("src/main.rs"), converting_brain_main())?;
    fs::write(robot.join("src/conversions.rs"), CONVERSIONS)?;
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  id: proof-cycle-robot\n  brain:\n    binary: proof-cycle-robot\n    bindings:\n      telemetry:\n      - provider.provider_status\n  services:\n    provider:\n      source:\n        path: provider\nsupervisor:\n  source:\n    path: supervisor\n",
    )?;

    let first = invoke(&robot, &["prepare", "--offline"]);
    assert!(
        first.status.success(),
        "the converting composition prepares:\n{}",
        report(&first)
    );
    let check = invoke(&robot, &["check", "--offline"]);
    assert!(
        check.status.success(),
        "check compiles the complete canonical brain and its conversion:\n{}",
        report(&check)
    );
    assert!(
        !robot.join("src/bin").exists(),
        "no generated adapter target may appear in the authored tree"
    );
    assert!(
        !robot.join(".phoxal/conversions").exists(),
        "conversion attachment belongs to the prepared API, without a sidecar"
    );

    let build = invoke(&robot, &["build", "--offline"]);
    assert!(
        build.status.success(),
        "the bundle consumes the canonical runtime and its full payload closure:\n{}",
        report(&build)
    );
    let tests = invoke(&robot, &["test", "--offline"]);
    assert!(
        tests.status.success(),
        "canonical conversion execution, failure, provenance, and reset:\n{}",
        report(&tests)
    );
    assert!(
        String::from_utf8_lossy(&tests.stdout).contains("2 passed"),
        "both runtime proofs execute: {}",
        report(&tests)
    );
    Ok(())
}
