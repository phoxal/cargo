//! Independent simulator provisioning and local finite-run orchestration.
//!
//! A simulator is an application selected outside the robot Cargo graph. This
//! module keeps its own small Cargo project and lockfile, probes the native
//! application's explicit model facts, then asks the prepared robot project
//! to assemble a simulation bundle.

use std::collections::BTreeMap;
use std::ffi::OsStr;
#[cfg(test)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::cargo::CargoOptions;
use crate::project::{CompiledBundle, Error, Project, SimulationModelFacts};
use phoxal::artifact::bundle::BundleManifest;
use phoxal::artifact::simulation_run::{
    SimulationApplicationReference, SimulationBinding, SimulationBundleReference,
    SimulationCapturePolicy, SimulationCaptureRequirement, SimulationExecutionBounds,
    SimulationModelReference, SimulationProgram, SimulationRunSpecification,
};
use phoxal::scenario::CapturePolicy;
use phoxal::scenario::plan_support::{Action, Capture, Program};

/// The official independently installed native simulation application.
pub const DEFAULT_SIMULATOR_PACKAGE: &str = "phoxal-simulator";
/// The binary target exposed by the simulator application.
pub const DEFAULT_SIMULATOR_BINARY: &str = "phoxal-simulator";
const PROVISION_LOCK: &str = "provision.lock";
// Allow the supervisor its existing whole-graph admission budget.
// This controls process startup only, not runtime cadence or Safety validity.
const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_EXECUTION_TIMEOUT: Duration = Duration::from_secs(180);
const PROCESS_POLL: Duration = Duration::from_millis(25);
/// The presentation selected for a finite simulation run.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SimulationPresentation {
    /// Run without creating a presentation window.
    Headless,
    /// Open the simulator's interactive presentation.
    Desktop,
}

impl SimulationPresentation {
    /// The simulator command-line spelling.
    #[must_use]
    pub const fn flag(self) -> &'static str {
        match self {
            Self::Headless => "--headless",
            Self::Desktop => "--desktop",
        }
    }
}

/// One positive finite bound for a simulation command.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum SimulationBound {
    /// Advance exactly this many native quanta.
    Steps(u64),
    /// Advance exactly this many seconds after native quantum validation.
    Duration(f64),
}

impl SimulationBound {
    fn validate(self) -> Result<(), Error> {
        match self {
            Self::Steps(steps) if steps > 0 => Ok(()),
            Self::Steps(_) => Err(simulation_error("steps must be a positive integer")),
            Self::Duration(duration) if duration.is_finite() && duration > 0.0 => Ok(()),
            Self::Duration(_) => Err(simulation_error(
                "duration must be a positive finite number",
            )),
        }
    }

    fn validate_for_quantum(self, quantum_ns: u64) -> Result<(), Error> {
        if quantum_ns == 0 {
            return Err(simulation_error("simulation quantum must be positive"));
        }
        match self {
            Self::Steps(_) => Ok(()),
            Self::Duration(duration) => {
                let duration_ns = duration * 1_000_000_000.0;
                let quanta = duration_ns / quantum_ns as f64;
                let nearest = quanta.round();
                let error = (quanta - nearest).abs();
                let tolerance = f64::EPSILON * quanta.abs().max(1.0) * 16.0;
                if !quanta.is_finite() || nearest < 1.0 || error > tolerance {
                    return Err(simulation_error(format!(
                        "duration {duration} seconds is not an integral number of {quantum_ns}ns simulation quanta"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// Inputs for one local simulation command.
#[derive(Clone, Debug, PartialEq)]
pub struct SimulationRunOptions {
    scene: PathBuf,
    presentation: SimulationPresentation,
    bound: SimulationBound,
    simulator_executable: Option<PathBuf>,
    simulator_package: String,
    simulator_binary: String,
    output: Option<PathBuf>,
    scope: String,
    supervisor_id: String,
    run_id: String,
    startup_timeout: Duration,
    execution_timeout: Duration,
    cleanup_timeout: Duration,
    auto_run: bool,
}

impl SimulationRunOptions {
    /// Construct a request using the official simulator selection.
    pub fn new(
        scene: impl Into<PathBuf>,
        presentation: SimulationPresentation,
        bound: SimulationBound,
    ) -> Result<Self, Error> {
        bound.validate()?;
        Ok(Self {
            scene: scene.into(),
            presentation,
            bound,
            simulator_executable: None,
            simulator_package: DEFAULT_SIMULATOR_PACKAGE.to_owned(),
            simulator_binary: DEFAULT_SIMULATOR_BINARY.to_owned(),
            output: None,
            scope: "local".to_owned(),
            supervisor_id: "local".to_owned(),
            run_id: "local-simulation".to_owned(),
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
            auto_run: false,
        })
    }

    /// The scene path supplied by the caller.
    #[must_use]
    pub fn scene(&self) -> &Path {
        &self.scene
    }

    /// Use an explicitly selected simulator executable.
    ///
    /// This is the injection seam for local development and deterministic
    /// process fixtures. The executable is checked as a regular file before
    /// launch. Its target and interfaces are validated; its contents are trusted.
    #[must_use]
    pub fn with_simulator_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.simulator_executable = Some(path.into());
        self
    }

    /// Put the immutable robot simulation bundle at an explicit path.
    #[must_use]
    pub fn with_output(mut self, path: impl Into<PathBuf>) -> Self {
        self.output = Some(path.into());
        self
    }

    /// Set the explicit router namespace, supervisor identity, and run id.
    #[must_use]
    pub fn with_identity(
        mut self,
        scope: impl Into<String>,
        supervisor_id: impl Into<String>,
        run_id: impl Into<String>,
    ) -> Self {
        self.scope = scope.into();
        self.supervisor_id = supervisor_id.into();
        self.run_id = run_id.into();
        self
    }

    /// Start advancing immediately when the desktop presentation becomes ready.
    #[must_use]
    pub const fn with_auto_run(mut self) -> Self {
        self.auto_run = true;
        self
    }
}

pub use phoxal::artifact::installation::{SimulatorArtifactSummary, SimulatorInstallationStatus};

/// Bounded cleanup evidence for a local simulation process pair.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SimulationCleanup {
    /// Whether the supervisor was asked to stop.
    pub supervisor_stop_requested: bool,
    /// Whether the supervisor exited before the cleanup deadline.
    pub supervisor_exited: bool,
    /// Whether a forced kill was required.
    pub supervisor_killed: bool,
    /// Human-readable cleanup diagnostic, if cleanup was incomplete.
    pub error: Option<String>,
}

/// Terminal evidence retained by one local finite simulation run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "schema")]
pub enum SimulationRunReport {
    /// The first simulation-run report generation.
    #[serde(rename = "phoxal/simulation-run/v0")]
    V0 {
        /// Canonical scene resource path.
        scene: PathBuf,
        /// Exact simulator artifact used.
        simulator: SimulatorArtifactSummary,
        /// Compiled simulation bundle path.
        bundle: PathBuf,
        /// Explicit router namespace.
        scope: String,
        /// Explicit supervisor identity.
        supervisor_id: String,
        /// Explicit finite run identity.
        run_id: String,
        /// Whether the supervisor survived startup readiness.
        supervisor_ready: bool,
        /// Whether the simulator emitted the required provider-contract terminal
        /// evidence for the finite run.
        provider_contract_verified: bool,
        /// Simulator exit code, or none when it terminated by signal.
        simulator_exit_code: Option<i32>,
        /// Wall time spent inside the simulator process for this finite run.
        simulator_wall_time_ns: u64,
        /// Complete simulator standard output.
        simulator_stdout: String,
        /// Complete simulator standard error.
        simulator_stderr: String,
        /// Bounded supervisor cleanup evidence.
        cleanup: SimulationCleanup,
        /// Runtime-observed scenario evidence, when this was a scenario run.
        scenario: Option<ScenarioExecutionReport>,
        /// Parsed terminal evidence emitted by the native simulator.
        terminal: Option<SimulatorTerminalEvidence>,
    },
}

/// Evidence observed by the supervisor while executing one scenario program.
///
/// Re-exported from `phoxal::artifact::simulation`. The framework module is
/// the source of truth; this alias keeps every existing
/// internal call site compiling unchanged.
pub use phoxal::artifact::simulation::ScenarioExecutionReport;

/// One runtime-acknowledged scenario action.
///
/// Re-exported from `phoxal::artifact::simulation`.
#[allow(unused_imports)]
pub use phoxal::artifact::simulation::ScenarioStepEvidence;

/// One typed capture stream drained by the supervisor.
///
/// Re-exported from `phoxal::artifact::simulation`.
#[allow(unused_imports)]
pub use phoxal::artifact::simulation::ScenarioCaptureEvidence;

impl SimulationRunReport {
    /// Whether the simulator exited successfully and cleanup completed.
    #[must_use]
    pub fn success(&self) -> bool {
        let Self::V0 {
            simulator_exit_code,
            cleanup,
            supervisor_ready,
            provider_contract_verified,
            ..
        } = self;
        *simulator_exit_code == Some(0)
            && cleanup.error.is_none()
            && *supervisor_ready
            && *provider_contract_verified
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SimulatorArtifact {
    pub(crate) summary: SimulatorArtifactSummary,
}
/// One command-scoped prepared simulation snapshot.
///
/// The resolved participant graph, staged asset closure, selected simulator,
/// and native model facts are prepared and probed exactly once per fixture
/// request; finalization reuses the same snapshot so a source or model change
/// cannot mix old probe facts with a new bundle. A later request refreshes
/// inputs normally by preparing a fresh snapshot.
pub struct PreparedSimulation {
    pub(crate) cargo_options: CargoOptions,
    pub(crate) prepared: crate::PreparedProject,
    pub(crate) frozen: crate::project::bundle::FrozenSimulation,
    pub(crate) facts: SimulationModelFacts,
    /// The managed application's lifetime guard, held from provisioning
    /// through probe, launch, and cleanup until this snapshot drops. The
    /// field is intentionally read never and dropped always: its value is
    /// the held lock. An explicit override keeps `None`: the caller owns
    /// that path's stability.
    _application_guard: Option<crate::project::file_lock::SharedFileLock>,
}

/// Prepares one command-scoped simulation: provisioning, project
/// preparation, one probe bundle, and one bounded native probe.
pub fn prepare_simulation(
    project: &Project,
    cargo_options: &CargoOptions,
    request: &SimulationRunOptions,
) -> Result<PreparedSimulation, Error> {
    cargo_options.validate()?;
    validate_request(request)?;

    // Provisioning happens before Project::prepare. A missing application in
    // locked or frozen mode therefore fails before the robot Cargo manifest or
    // its owning Cargo.lock can be changed.
    let scene = canonical_scene(request.scene())?;
    let provisioned = provision(request, cargo_options)?;
    let SimulatorProvision {
        artifact: simulator,
        guard: application_guard,
    } = provisioned;
    let prepared = project.prepare(cargo_options)?;
    // Freeze the complete closure once: every executable build, native
    // asset stage, supervisor acquisition, the guarded simulator selection, and
    // the scene copy land in one command-owned staging root that the probe
    // and finalization both reuse verbatim.
    let frozen = crate::project::bundle::freeze_simulation_closure(
        &prepared,
        cargo_options,
        &scene,
        &simulator,
    )?;
    probe_snapshot(prepared, frozen, application_guard, cargo_options, request)
}

/// Prepares a case from robot resources frozen before the Cargo harness launch.
pub(crate) fn prepare_simulation_from_robot(
    prepared: &crate::PreparedProject,
    robot: &super::bundle::FrozenRobot,
    cargo_options: &CargoOptions,
    request: &SimulationRunOptions,
) -> Result<PreparedSimulation, Error> {
    cargo_options.validate()?;
    validate_request(request)?;
    let scene = canonical_scene(request.scene())?;
    let SimulatorProvision {
        artifact: simulator,
        guard,
    } = provision(request, cargo_options)?;
    let frozen = super::bundle::freeze_scene_closure(robot, &scene, &simulator)?;
    probe_snapshot(prepared.clone(), frozen, guard, cargo_options, request)
}

fn probe_snapshot(
    prepared: crate::PreparedProject,
    frozen: super::bundle::FrozenSimulation,
    application_guard: Option<super::file_lock::SharedFileLock>,
    cargo_options: &CargoOptions,
    request: &SimulationRunOptions,
) -> Result<PreparedSimulation, Error> {
    let probe_output = probe_bundle_path(&prepared);
    let probe_bundle = crate::project::bundle::finalize_simulation_bundle(
        &prepared,
        &frozen,
        &probe_output,
        None,
        None,
    )?;
    frozen.verify_simulator()?;
    let facts = probe(
        &frozen.simulator,
        &frozen.scene,
        probe_bundle.root(),
        request,
    )?;
    eprintln!(
        "cargo phoxal: simulation prepared from one native probe (model {}, quantum {} ns)",
        facts.model_identity, facts.quantum_ns
    );
    Ok(PreparedSimulation {
        cargo_options: cargo_options.clone(),
        prepared,
        frozen,
        facts,
        _application_guard: application_guard,
    })
}

/// Finalizes one finite simulation from an already prepared snapshot.
///
/// The immutable bundle is built from the snapshot's prepared graph and
/// native facts; no second preparation, probe bundle, or native probe runs.
pub fn run_prepared(
    snapshot: PreparedSimulation,
    request: &SimulationRunOptions,
    scenario: Option<&Program>,
) -> Result<SimulationRunReport, Error> {
    snapshot.cargo_options.validate()?;
    validate_request(request)?;
    request
        .bound
        .validate_for_quantum(snapshot.facts.quantum_ns)?;
    let output = request.output.clone().unwrap_or_else(|| {
        snapshot
            .prepared
            .default_bundle_path()
            .with_file_name("simulation-bundle")
    });
    let bundle = crate::project::bundle::finalize_simulation_bundle(
        &snapshot.prepared,
        &snapshot.frozen,
        &output,
        Some(&snapshot.facts),
        scenario.map(|program| crate::project::bundle::SimulationRunInput {
            program,
            fixture_instance_id: "scenario",
        }),
    )?;
    eprintln!("cargo phoxal: finalizing the simulation bundle from the prepared snapshot");
    // Staging, probe, and run use the same lifetime-guarded application selection.
    snapshot.frozen.verify_simulator()?;
    launch(
        &snapshot.frozen.simulator,
        &bundle,
        &snapshot.frozen.scene,
        &snapshot.facts,
        request,
        scenario,
    )
}

/// Run one finite simulation from a prepared robot source project.
pub(crate) fn run(
    project: &Project,
    cargo_options: &CargoOptions,
    request: &SimulationRunOptions,
    scenario: Option<&Program>,
) -> Result<SimulationRunReport, Error> {
    let snapshot = prepare_simulation(project, cargo_options, request)?;
    run_prepared(snapshot, request, scenario)
}

fn validate_request(request: &SimulationRunOptions) -> Result<(), Error> {
    request.bound.validate()?;
    validate_identity_part("scope", &request.scope)?;
    validate_identity_part("supervisor_id", &request.supervisor_id)?;
    validate_identity_part("run_id", &request.run_id)?;
    if request.startup_timeout.is_zero()
        || request.execution_timeout.is_zero()
        || request.cleanup_timeout.is_zero()
    {
        return Err(simulation_error(
            "simulation startup, execution, and cleanup timeouts must be positive",
        ));
    }
    if request.simulator_package.is_empty() || request.simulator_binary.is_empty() {
        return Err(simulation_error(
            "simulator package, version, and binary must be non-empty",
        ));
    }
    Ok(())
}

fn network_forbidden(options: &CargoOptions) -> bool {
    options.offline || options.lock == crate::project::cargo::LockMode::Frozen
}

fn validate_identity_part(field: &str, value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.len() > 128
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
    {
        return Err(simulation_error(format!(
            "{field} must be 1-128 lowercase ASCII letters, digits, '-' or '_'"
        )));
    }
    Ok(())
}

fn canonical_scene(path: &Path) -> Result<PathBuf, Error> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(simulation_error(format!(
            "simulation scene {} must be a regular non-symlink file",
            path.display()
        )));
    }
    let extension = path.extension().and_then(OsStr::to_str).unwrap_or_default();
    if !matches!(extension, "xml" | "mjz") {
        return Err(simulation_error(format!(
            "simulation scene {} must end in .xml or .mjz",
            path.display()
        )));
    }
    path.canonicalize().map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })
}

fn probe_bundle_path(prepared: &crate::PreparedProject) -> PathBuf {
    prepared
        .default_bundle_path()
        .with_file_name("simulation-probe-bundle")
}

/// One prepared simulator selection with its managed lifetime guard.
///
/// The managed application is guarded by a shared hold on the provisioning
/// lock for as long as this value lives: replacement and removal require
/// the exclusive lock and cannot proceed while any prepared selection is
/// alive. An explicit override carries no guard; the caller owns that
/// path's stability. Executable contents are trusted, with no drift detection.
pub(crate) struct SimulatorProvision {
    pub(crate) artifact: SimulatorArtifact,
    pub(crate) guard: Option<crate::project::file_lock::SharedFileLock>,
}

fn provision(
    request: &SimulationRunOptions,
    cargo_options: &CargoOptions,
) -> Result<SimulatorProvision, Error> {
    if let Some(path) = &request.simulator_executable {
        validate_simulator_application(path)?;
        return Ok(SimulatorProvision {
            artifact: SimulatorArtifact {
                summary: SimulatorArtifactSummary {
                    package: request.simulator_package.clone(),
                    version: "source-build".into(),
                    binary: request.simulator_binary.clone(),
                    source: "explicit-executable".to_owned(),
                    executable: path.canonicalize().map_err(|source| Error::ArtifactFile {
                        path: path.clone(),
                        source,
                    })?,
                },
            },
            guard: None,
        });
    }

    let root = simulator_store_root()?;
    provision_managed_at(&root, request, cargo_options)
}

fn provision_managed_at(
    root: &Path,
    request: &SimulationRunOptions,
    cargo_options: &CargoOptions,
) -> Result<SimulatorProvision, Error> {
    if !root.join("selection.json").is_file() {
        // The pre-existing initial-installation transition: with no
        // selection recorded there is no prepared selection to guard, so
        // the exclusive installer owns this step alone. It retains the
        // existing offline/frozen refusals and never mutates a robot.
        install_simulator_at(root, cargo_options, None, false)?;
    }
    guarded_selection(root, || provision_at(root, request))
}

fn guarded_selection(
    root: &Path,
    read: impl FnOnce() -> Result<SimulatorArtifact, Error>,
) -> Result<SimulatorProvision, Error> {
    // Take the shared side BEFORE reading anything: the selection and the
    // application tree validated below are then exactly the tree this
    // guard protects for the selection's whole lifetime. A replacement
    // that completes before acquisition is read freshly here; one that
    // starts after cannot proceed until the guard drops.
    let parent = root
        .parent()
        .ok_or_else(|| simulation_error("managed simulator root has no parent"))?;
    let lock_path = parent.join(PROVISION_LOCK);
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|source| Error::ArtifactFile {
            path: lock_path.clone(),
            source,
        })?;
    let guard = crate::project::file_lock::SharedFileLock::try_acquire(lock).map_err(|error| {
        simulation_error(format!(
            "a simulator installation is replacing {}; retry once it completes: {error}",
            root.display()
        ))
    })?;
    let artifact = read()?;
    Ok(SimulatorProvision {
        artifact,
        guard: Some(guard),
    })
}

fn provision_at(root: &Path, _request: &SimulationRunOptions) -> Result<SimulatorArtifact, Error> {
    let status = installer_command(
        root,
        "status",
        &CargoOptions::default(),
        None,
        false,
        None,
        None,
    )?;
    let summary = status.artifact.ok_or_else(|| {
        simulation_error("simulator is not installed; run cargo phoxal simulation install")
    })?;
    validate_simulator_application(&summary.executable)?;
    Ok(SimulatorArtifact { summary })
}

fn validate_simulator_application(path: &Path) -> Result<(), Error> {
    use phoxal::artifact::application::*;
    ensure_regular_file(path, "simulator executable")?;
    let record = super::application::read_embedded_contract(path)?;
    let required = ApplicationContract {
        bundle: Some(BUNDLE_CONTRACT),
        launch: SIMULATOR_LAUNCH_CONTRACT,
        execution: None,
        simulation: Some(SIMULATION_PROTOCOL_CONTRACT),
        target: HOST_EXECUTION_TARGET,
    };
    record.validate_for(&required).map_err(|error| {
        simulation_error(format!(
            "incompatible simulator interfaces or target: {error}"
        ))
    })?;
    Ok(())
}

fn simulator_store_root() -> Result<PathBuf, Error> {
    if let Some(root) = std::env::var_os("PHOXAL_HOME") {
        if root.is_empty() {
            return Err(simulation_error("PHOXAL_HOME cannot be empty"));
        }
        return Ok(PathBuf::from(root).join("applications/simulation"));
    }
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            simulation_error("cannot locate the user data directory; set PHOXAL_HOME")
        })?;
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from(home).join("Library/Application Support/Phoxal/applications/simulation"))
    }
    #[cfg(target_os = "linux")]
    {
        let data = std::env::var_os("XDG_DATA_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".local/share"));
        Ok(data.join("phoxal/applications/simulation"))
    }
}

/// Installs the independently selected simulator-owned installer and invokes it.
pub fn install_simulator(
    options: &CargoOptions,
    distribution: Option<&Path>,
    replace: bool,
    release: Option<&str>,
    application_release: Option<&str>,
) -> Result<SimulatorInstallationStatus, Error> {
    installer_command(
        &simulator_store_root()?,
        "install",
        options,
        distribution,
        replace,
        release,
        application_release,
    )
}
fn install_simulator_at(
    root: &Path,
    options: &CargoOptions,
    distribution: Option<&Path>,
    replace: bool,
) -> Result<SimulatorInstallationStatus, Error> {
    installer_command(root, "install", options, distribution, replace, None, None)
}
/// Inspects installed application status through its owning installer.
pub fn simulator_status() -> Result<SimulatorInstallationStatus, Error> {
    let root = simulator_store_root()?;
    simulator_status_at(&root)
}
fn simulator_status_at(root: &Path) -> Result<SimulatorInstallationStatus, Error> {
    if !root.exists() {
        return Ok(SimulatorInstallationStatus {
            installed: false,
            root: root.into(),
            simulator_version: None,
            mujoco_version: None,
            executable: None,
            artifact: None,
        });
    }
    installer_command(
        root,
        "status",
        &CargoOptions::default(),
        None,
        false,
        None,
        None,
    )
}
/// Removes the managed application through its owning installer.
pub fn uninstall_simulator() -> Result<PathBuf, Error> {
    uninstall_simulator_at(&simulator_store_root()?)
}
fn uninstall_simulator_at(root: &Path) -> Result<PathBuf, Error> {
    if !root.exists() {
        return Ok(root.into());
    }
    installer_command(
        root,
        "uninstall",
        &CargoOptions::default(),
        None,
        false,
        None,
        None,
    )?;
    Ok(root.into())
}
fn installer_command(
    root: &Path,
    operation: &str,
    options: &CargoOptions,
    distribution: Option<&Path>,
    replace: bool,
    release: Option<&str>,
    application_release: Option<&str>,
) -> Result<SimulatorInstallationStatus, Error> {
    let installer =
        super::simulator_installer::select(options, replace && operation == "install", release)?;
    let home = root
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| simulation_error("managed simulator root has no application home"))?;
    let mut command = Command::new(&installer.executable);
    command.env("PHOXAL_HOME", home).arg(operation);
    if operation == "install" {
        if let Some(version) = application_release {
            command.arg("--simulator-version").arg(version);
        }
        if replace {
            command.arg("--replace");
        }
        if network_forbidden(options) {
            command.arg("--offline");
        }
        if let Some(distribution) = distribution {
            command.arg("--mujoco-distribution").arg(distribution);
        }
    }
    command.env("CARGO", options.cargo_program());
    let output = command.output().map_err(|source| Error::CargoSpawn {
        operation: "simulator installer".into(),
        source,
    })?;
    if !output.status.success() {
        return Err(simulation_error(format!(
            "simulator installer {operation} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| simulation_error(format!("invalid simulator installer status: {e}")))
}

fn probe(
    simulator: &SimulatorArtifact,
    scene: &Path,
    bundle: &Path,
    request: &SimulationRunOptions,
) -> Result<SimulationModelFacts, Error> {
    let mut command = Command::new(&simulator.summary.executable);
    command.args([
        "--probe",
        "--scene",
        &scene.display().to_string(),
        "--bundle",
        &bundle.display().to_string(),
        "--json",
    ]);
    command.arg(request.presentation.flag());
    let output = command
        .output()
        .map_err(|source| Error::SimulationInvalid {
            message: format!(
                "cannot start simulator probe {}: {source}",
                simulator.summary.executable.display()
            ),
        })?;
    if !output.status.success() {
        return Err(Error::SimulationInvalid {
            message: format!(
                "simulator probe failed ({}){}",
                status_string(output.status),
                diagnostic_output(&output.stderr)
            ),
        });
    }
    let facts: SimulationModelFacts = serde_json::from_slice(&output.stdout).map_err(|source| {
        simulation_error(format!(
            "simulator probe returned no valid SimulationModelFacts: {source}"
        ))
    })?;
    if facts.model_identity.is_empty() || facts.quantum_ns == 0 {
        return Err(simulation_error(
            "simulator probe returned incomplete model identity or quantum facts",
        ));
    }
    Ok(facts)
}

fn build_run_specification(
    bundle: &CompiledBundle,
    scene: &Path,
    facts: &SimulationModelFacts,
    simulator: &SimulatorArtifact,
    request: &SimulationRunOptions,
    program: &Program,
) -> Result<SimulationRunSpecification, Error> {
    let manifest_path = bundle.root().join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path).map_err(|source| Error::ArtifactFile {
        path: manifest_path.clone(),
        source,
    })?;
    let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes).map_err(|source| {
        simulation_error(format!(
            "cannot decode immutable bundle manifest {}: {source}",
            manifest_path.display()
        ))
    })?;
    let BundleManifest::V0 {
        robot_id,
        connections,
        ..
    } = &manifest;

    program
        .verify_identity()
        .map_err(|error| simulation_error(format!("scenario program identity: {error}")))?;

    let mut bindings = BTreeMap::<(String, String), SimulationBinding>::new();
    for step in program.steps() {
        let (target_instance, signature, payload_bytes) = match &step.action {
            Action::Setpoint {
                target_instance,
                consumer_signature,
                encoded_payload,
                ..
            } => (target_instance, consumer_signature, encoded_payload.len()),
            Action::Withdraw {
                target_instance,
                producer_signature,
            } => (target_instance, producer_signature, 0),
            Action::Command { .. } => continue,
        };
        let max_message_bytes = u32::try_from(payload_bytes).map_err(|_| {
            simulation_error(format!(
                "scenario payload for {}.{} exceeds u32",
                target_instance, signature.endpoint
            ))
        })?;
        let key = (target_instance.clone(), signature.endpoint.to_owned());
        let target = format!("{}.{}", target_instance, signature.endpoint);
        let binding = SimulationBinding {
            target_instance: target_instance.clone(),
            source_instance: "supervisor".to_owned(),
            signature: signature.clone(),
            max_message_bytes,
            replaces_authored_source: connections.iter().any(|connection| {
                format!(
                    "{}.{}",
                    connection.consumer.instance, connection.consumer.endpoint
                ) == target
            }),
        };
        bindings
            .entry(key)
            .and_modify(|existing| {
                existing.max_message_bytes =
                    existing.max_message_bytes.max(binding.max_message_bytes);
            })
            .or_insert(binding);
    }

    let captures = program
        .captures()
        .iter()
        .map(|capture| match capture {
            Capture::State {
                name,
                signature,
                policy,
            }
            | Capture::Sample {
                name,
                signature,
                policy,
            }
            | Capture::Event {
                name,
                signature,
                policy,
            } => {
                let (instance, _) = name.split_once('/').ok_or_else(|| {
                    simulation_error(format!(
                        "observation capture `{name}` has no configured instance"
                    ))
                })?;
                Ok(SimulationCaptureRequirement::Observation {
                    instance: instance.to_owned(),
                    signature: signature.clone(),
                    policy: capture_policy(*policy),
                })
            }
            Capture::NativeBody { name, .. } => {
                if name != robot_id {
                    return Err(simulation_error(format!(
                        "native body `{name}` is not the selected model root `{robot_id}`; arbitrary body recording is not supported"
                    )));
                }
                Ok(SimulationCaptureRequirement::RootBody {
                    body: name.clone(),
                    every_steps: 1,
                })
            }
        })
        .collect::<Result<Vec<_>, Error>>()?;

    let maximum_command_deadline = program
        .steps()
        .iter()
        .filter_map(|step| match &step.action {
            Action::Command { host_deadline, .. } => Some(*host_deadline),
            _ => None,
        })
        .max()
        .unwrap_or(Duration::from_secs(30));
    if maximum_command_deadline > request.execution_timeout {
        return Err(simulation_error(format!(
            "scenario command deadline {} ms exceeds the run host deadline {} ms",
            maximum_command_deadline.as_millis(),
            request.execution_timeout.as_millis()
        )));
    }

    Ok(SimulationRunSpecification::V0 {
        bundle: SimulationBundleReference {
            robot_id: robot_id.clone(),
            manifest_sha256: format!("{:x}", Sha256::digest(&manifest_bytes)),
        },
        model: SimulationModelReference {
            scene: scene.display().to_string(),
            model_identity: facts.model_identity.clone(),
        },
        simulator: SimulationApplicationReference {
            package: simulator.summary.package.clone(),
            version: simulator.summary.version.clone(),
            binary: simulator.summary.binary.clone(),
        },
        program: SimulationProgram {
            bytes: program.program_bytes().to_vec(),
        },
        bindings: bindings.into_values().collect(),
        captures,
        execution: SimulationExecutionBounds {
            quantum_ns: facts.quantum_ns,
            transitions: program.transition_count(),
            host_deadline_ms: u64::try_from(request.execution_timeout.as_millis())
                .unwrap_or(u64::MAX),
            shutdown_grace_ms: u64::try_from(request.cleanup_timeout.as_millis())
                .unwrap_or(u64::MAX),
        },
    })
}

const fn capture_policy(policy: CapturePolicy) -> SimulationCapturePolicy {
    match policy {
        CapturePolicy::Latest => SimulationCapturePolicy::Latest,
        CapturePolicy::BestEffortHistory { capacity } => {
            SimulationCapturePolicy::BestEffortHistory { capacity }
        }
        CapturePolicy::RequiredHistory { capacity } => {
            SimulationCapturePolicy::RequiredHistory { capacity }
        }
    }
}

fn launch(
    simulator: &SimulatorArtifact,
    bundle: &CompiledBundle,
    scene: &Path,
    facts: &SimulationModelFacts,
    request: &SimulationRunOptions,
    scenario: Option<&Program>,
) -> Result<SimulationRunReport, Error> {
    let supervisor_path = bundle.executable("supervisor");
    // Unix socket names must fit even when the source checkout path is long.
    // The private temporary directory is owned by this launcher and lives until cleanup.
    let readiness_directory = tempfile::Builder::new()
        .prefix("phoxal-sim-")
        .tempdir_in("/tmp")
        .map_err(|source| Error::ArtifactFile {
            path: std::env::temp_dir(),
            source,
        })?;
    let readiness_path = readiness_directory.path().join("ready.json");
    let scenario_result_path = readiness_directory.path().join("scenario-result.json");
    let run_specification_path = readiness_directory.path().join("simulation-run.json");
    let endpoint = format!(
        "unixsock-stream/{}",
        readiness_directory.path().join("router.sock").display()
    );
    let mut supervisor_command = Command::new(&supervisor_path);
    supervisor_command
        .arg(bundle.root())
        .arg("--state-dir")
        .arg(readiness_directory.path())
        .args([
            "--scope",
            &request.scope,
            "--supervisor-id",
            &request.supervisor_id,
            "--ready-file",
            &readiness_path.display().to_string(),
            "--listen",
            &endpoint,
            "--owner-pid",
            &std::process::id().to_string(),
        ]);
    if let Some(program) = scenario {
        let specification =
            build_run_specification(bundle, scene, facts, simulator, request, program)?;
        atomic_json(&run_specification_path, &specification)?;
        supervisor_command.args([
            "--simulation-run",
            &run_specification_path.display().to_string(),
            "--scenario-result",
            &scenario_result_path.display().to_string(),
        ]);
    }
    let mut supervisor = supervisor_command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|source| Error::SupervisorLaunch {
            message: format!("cannot start {}: {source}", supervisor_path.display()),
        })?;
    let supervisor_ready =
        match wait_process_ready(&mut supervisor, &readiness_path, request.startup_timeout) {
            Ok(ready) => ready,
            Err(error) => {
                let cleanup = cleanup_process(&mut supervisor, request.cleanup_timeout);
                return Err(simulation_error(format!(
                    "{error}; cleanup: {}",
                    cleanup_diagnostic(&cleanup)
                )));
            }
        };
    if !supervisor_ready {
        let cleanup = cleanup_process(&mut supervisor, request.cleanup_timeout);
        return Err(Error::SupervisorLaunch {
            message: format!(
                "supervisor exited before readiness for bundle {}; see supervisor diagnostics above; cleanup: {}",
                bundle.root().display(),
                cleanup_diagnostic(&cleanup)
            ),
        });
    }

    let mut simulator_command = Command::new(&simulator.summary.executable);
    simulator_command
        .args([
            "--scene",
            &scene.display().to_string(),
            "--bundle",
            &bundle.root().display().to_string(),
            request.presentation.flag(),
            "--scope",
            &request.scope,
            "--supervisor-id",
            &request.supervisor_id,
            "--run-id",
            &request.run_id,
            "--connect",
            &endpoint,
        ])
        .stdin(Stdio::null());
    match request.bound {
        SimulationBound::Steps(steps) => {
            simulator_command.args(["--steps", &steps.to_string()]);
        }
        SimulationBound::Duration(duration) => {
            simulator_command.args(["--duration", &duration.to_string()]);
        }
    }
    if request.auto_run {
        simulator_command.arg("--auto-run");
    }
    let simulator_started = Instant::now();
    let output = match bounded_output(&mut simulator_command, request.execution_timeout) {
        Ok(output) => output,
        Err(detail) => {
            let cleanup = cleanup_process(&mut supervisor, request.cleanup_timeout);
            return Err(simulation_error(format!(
                "simulator {} failed: {detail}; cleanup: {}",
                simulator.summary.executable.display(),
                cleanup_diagnostic(&cleanup)
            )));
        }
    };
    let simulator_wall_time_ns =
        u64::try_from(simulator_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let terminal = terminal_evidence(&output.stdout);
    let provider_contract_verified = terminal
        .as_ref()
        .is_some_and(|evidence| terminal_evidence_verified(evidence, request.presentation));
    let cleanup = cleanup_process(&mut supervisor, request.cleanup_timeout);
    let scenario = if scenario.is_some() {
        let bytes = fs::read(&scenario_result_path).map_err(|source| Error::ArtifactFile {
            path: scenario_result_path.clone(),
            source,
        })?;
        Some(serde_json::from_slice(&bytes).map_err(|source| {
            simulation_error(format!(
                "cannot parse scenario execution evidence {}: {source}",
                scenario_result_path.display()
            ))
        })?)
    } else {
        None
    };
    Ok(SimulationRunReport::V0 {
        scene: scene.to_owned(),
        simulator: simulator.summary.clone(),
        bundle: bundle.root().to_owned(),
        scope: request.scope.clone(),
        supervisor_id: request.supervisor_id.clone(),
        run_id: request.run_id.clone(),
        supervisor_ready,
        provider_contract_verified,
        simulator_exit_code: output.status.code(),
        simulator_wall_time_ns,
        simulator_stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        simulator_stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        cleanup,
        scenario,
        terminal,
    })
}

fn bounded_output(
    command: &mut Command,
    deadline: Duration,
) -> Result<std::process::Output, String> {
    let mut stdout = tempfile::tempfile().map_err(|error| format!("stdout capture: {error}"))?;
    let mut stderr = tempfile::tempfile().map_err(|error| format!("stderr capture: {error}"))?;
    command
        .stdout(Stdio::from(
            stdout
                .try_clone()
                .map_err(|error| format!("stdout clone: {error}"))?,
        ))
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .map_err(|error| format!("stderr clone: {error}"))?,
        ));
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start process: {error}"))?;
    let started = Instant::now();
    let status = loop {
        match child
            .try_wait()
            .map_err(|error| format!("cannot inspect process: {error}"))?
        {
            Some(status) => break status,
            None if started.elapsed() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "host execution deadline of {} ms expired and the process was terminated",
                    deadline.as_millis()
                ));
            }
            None => thread::sleep(PROCESS_POLL),
        }
    };
    stdout
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("stdout rewind: {error}"))?;
    stderr
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("stderr rewind: {error}"))?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    stdout
        .read_to_end(&mut stdout_bytes)
        .map_err(|error| format!("stdout read: {error}"))?;
    stderr
        .read_to_end(&mut stderr_bytes)
        .map_err(|error| format!("stderr read: {error}"))?;
    Ok(std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

/// Native terminal evidence emitted by the simulator application.
///
/// Re-exported from `phoxal::artifact::simulation`. The framework module is
/// the source of truth.
pub use phoxal::artifact::simulation::SimulatorTerminalEvidence;

#[cfg(test)]
fn provider_contract_verified(stdout: &[u8], presentation: SimulationPresentation) -> bool {
    terminal_evidence(stdout)
        .as_ref()
        .is_some_and(|evidence| terminal_evidence_verified(evidence, presentation))
}

fn terminal_evidence(stdout: &[u8]) -> Option<SimulatorTerminalEvidence> {
    let line = stdout
        .split(|byte| *byte == b'\n')
        .rfind(|line| !line.is_empty())?;
    serde_json::from_slice::<SimulatorTerminalEvidence>(line).ok()
}

fn terminal_evidence_verified(
    evidence: &SimulatorTerminalEvidence,
    presentation: SimulationPresentation,
) -> bool {
    let SimulatorTerminalEvidence::V0 {
        provider_contract_verified,
        outcome,
        completed_steps,
        requested_steps,
        ..
    } = evidence;
    *provider_contract_verified
        && ((outcome == "success" && completed_steps == requested_steps)
            || (presentation == SimulationPresentation::Desktop
                && outcome == "stopped"
                && completed_steps <= requested_steps))
        && *requested_steps > 0
}

#[derive(Deserialize)]
#[serde(tag = "schema")]
enum SupervisorReadiness {
    #[serde(rename = "phoxal/supervisor-ready/v0")]
    V0 { execution: String },
}

fn wait_process_ready(
    child: &mut Child,
    readiness_path: &Path,
    timeout: Duration,
) -> Result<bool, Error> {
    let deadline = Instant::now() + timeout;
    loop {
        if readiness_path.is_file() {
            let bytes = fs::read(readiness_path).map_err(|source| Error::ArtifactFile {
                path: readiness_path.to_owned(),
                source,
            })?;
            let readiness: SupervisorReadiness =
                serde_json::from_slice(&bytes).map_err(|source| {
                    simulation_error(format!(
                        "supervisor readiness {} is invalid: {source}",
                        readiness_path.display()
                    ))
                })?;
            let SupervisorReadiness::V0 { execution } = &readiness;
            if execution.is_empty() {
                return Err(simulation_error(format!(
                    "supervisor readiness {} has an unsupported or incomplete contract",
                    readiness_path.display()
                )));
            }
            return Ok(true);
        }
        match child.try_wait().map_err(|source| Error::SupervisorLaunch {
            message: format!("cannot inspect supervisor readiness: {source}"),
        })? {
            Some(_) => return Ok(false),
            None if Instant::now() >= deadline => {
                return Err(Error::SupervisorLaunch {
                    message: format!(
                        "supervisor readiness timed out after {} ms while the process is still running",
                        timeout.as_millis()
                    ),
                });
            }
            None => thread::sleep(PROCESS_POLL),
        }
    }
}

fn cleanup_process(child: &mut Child, timeout: Duration) -> SimulationCleanup {
    let mut result = SimulationCleanup {
        supervisor_stop_requested: true,
        supervisor_exited: false,
        supervisor_killed: false,
        error: None,
    };
    match child.try_wait() {
        Ok(Some(_)) => {
            result.supervisor_exited = true;
            return result;
        }
        Ok(None) => {}
        Err(error) => {
            result.error = Some(format!("cannot inspect supervisor cleanup: {error}"));
            return result;
        }
    }
    {
        // The supervisor owns a termination handler that performs orderly
        // Runtime, session, bus, and router shutdown.
        if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) } != 0 {
            result.error = Some(format!(
                "cannot request orderly supervisor stop: {}",
                std::io::Error::last_os_error()
            ));
            return result;
        }
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                result.supervisor_exited = true;
                return result;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(PROCESS_POLL),
            Ok(None) => {
                if let Err(error) = child.kill() {
                    result.error = Some(format!(
                        "supervisor did not exit after orderly stop and forced kill failed: {error}"
                    ));
                    return result;
                }
                result.supervisor_killed = true;
                return match child.wait() {
                    Ok(_) => {
                        result.supervisor_exited = true;
                        result
                    }
                    Err(error) => {
                        result.error =
                            Some(format!("cannot reap supervisor after forced kill: {error}"));
                        result
                    }
                };
            }
            Err(error) => {
                result.error = Some(format!("cannot reap supervisor after kill: {error}"));
                return result;
            }
        }
    }
}

fn cleanup_diagnostic(cleanup: &SimulationCleanup) -> String {
    cleanup
        .error
        .clone()
        .unwrap_or_else(|| "supervisor cleanup completed".to_owned())
}

fn ensure_regular_file(path: &Path, label: &str) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(simulation_error(format!(
            "{label} {} must be a regular non-symlink file",
            path.display()
        )));
    }
    Ok(())
}

fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| Error::ArtifactFile {
        path: parent.to_owned(),
        source,
    })?;
    let bytes = serde_json::to_vec_pretty(value).map_err(|source| Error::SimulationInvalid {
        message: format!("cannot serialize simulator selection: {source}"),
    })?;
    temporary
        .write_all(&bytes)
        .and_then(|_| temporary.as_file().sync_all())
        .map_err(|source| Error::ArtifactFile {
            path: path.to_owned(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| Error::ArtifactFile {
            path: path.to_owned(),
            source: error.error,
        })?;
    Ok(())
}

fn status_string(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| code.to_string(),
    )
}

fn diagnostic_output(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes).trim().to_owned();
    if text.is_empty() {
        String::new()
    } else {
        format!(": {text}")
    }
}

fn simulation_error(message: impl Into<String>) -> Error {
    Error::SimulationInvalid {
        message: message.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simulation_bound_rejects_zero_and_non_finite_values() {
        assert!(SimulationBound::Steps(0).validate().is_err());
        assert!(SimulationBound::Duration(0.0).validate().is_err());
        assert!(SimulationBound::Duration(f64::NAN).validate().is_err());
        assert!(SimulationBound::Steps(1).validate().is_ok());
    }

    #[test]
    fn duration_must_be_an_integral_number_of_native_quanta() {
        assert!(
            SimulationBound::Duration(0.03)
                .validate_for_quantum(10_000_000)
                .is_ok()
        );
        assert!(
            SimulationBound::Duration(0.025)
                .validate_for_quantum(10_000_000)
                .is_err()
        );
    }

    #[test]
    fn terminal_evidence_requires_the_provider_contract_marker() {
        let complete = br#"{"schema":"phoxal/simulation-run/v0","provider_contract_verified":true,"outcome":"success","completed_steps":2,"requested_steps":2}"#;
        assert!(provider_contract_verified(
            complete,
            SimulationPresentation::Headless
        ));
        let missing = br#"{"schema":"phoxal/simulation-run/v0","outcome":"success","completed_steps":2,"requested_steps":2}"#;
        assert!(!provider_contract_verified(
            missing,
            SimulationPresentation::Headless
        ));
        let incomplete = br#"{"schema":"phoxal/simulation-run/v0","provider_contract_verified":true,"outcome":"failed","completed_steps":1,"requested_steps":2}"#;
        assert!(!provider_contract_verified(
            incomplete,
            SimulationPresentation::Headless
        ));
    }

    #[test]
    fn desktop_stop_is_a_successful_control_outcome_but_not_a_headless_completion() {
        let stopped = br#"{"schema":"phoxal/simulation-run/v0","provider_contract_verified":true,"outcome":"stopped","completed_steps":0,"requested_steps":500}"#;
        assert!(provider_contract_verified(
            stopped,
            SimulationPresentation::Desktop
        ));
        assert!(!provider_contract_verified(
            stopped,
            SimulationPresentation::Headless
        ));
    }

    #[test]
    fn status_is_read_only_when_no_managed_installation_exists()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = tempfile::tempdir()?;
        let root = fixture.path().join("simulation");
        let status = simulator_status_at(&root)?;
        assert!(!status.installed);
        assert_eq!(status.root, root);
        assert!(!root.exists());
        Ok(())
    }

    #[test]
    fn a_live_process_readiness_timeout_is_distinct_from_process_exit()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let mut child = Command::new("sleep").arg("10").spawn()?;
        let outcome = wait_process_ready(
            &mut child,
            &directory.path().join("missing-ready.json"),
            Duration::from_millis(1),
        );
        let cleanup = cleanup_process(&mut child, Duration::from_secs(1));
        assert!(cleanup.supervisor_exited);
        let error = outcome.expect_err("a live process exceeded its readiness deadline");
        assert!(error.to_string().contains("readiness timed out"));
        assert!(error.to_string().contains("process is still running"));
        Ok(())
    }

    #[test]
    fn readiness_requires_the_machine_contract_and_cleanup_is_orderly()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let readiness = directory.path().join("ready.json");
        fs::write(
            &readiness,
            br#"{"schema":"phoxal/supervisor-ready/v0","execution":"execution-1"}"#,
        )?;
        let mut child = Command::new("sleep").arg("10").spawn()?;
        assert!(wait_process_ready(
            &mut child,
            &readiness,
            Duration::from_secs(1)
        )?);
        let cleanup = cleanup_process(&mut child, Duration::from_secs(1));
        assert!(cleanup.supervisor_stop_requested);
        assert!(cleanup.supervisor_exited);
        assert!(!cleanup.supervisor_killed);
        assert!(cleanup.error.is_none());
        Ok(())
    }
}

#[cfg(test)]
mod managed_lifetime_tests {
    use super::*;
    use crate::project::file_lock::{ExclusiveFileLock, SharedFileLock};
    fn lock_file(root: &Path) -> File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.parent().unwrap().join(PROVISION_LOCK))
            .unwrap()
    }
    #[test]
    fn selection_reads_happen_under_the_shared_guard() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("simulation");
        let writer = ExclusiveFileLock::try_acquire(lock_file(&root)).unwrap();
        let read = std::cell::Cell::new(false);
        let result = guarded_selection(&root, || {
            read.set(true);
            Err(simulation_error("invalid selection"))
        });
        assert!(format!("{}", result.err().unwrap()).contains("installation is replacing"));
        assert!(!read.get());
        drop(writer);
        let result = guarded_selection(&root, || {
            read.set(true);
            Err(simulation_error("invalid selection"))
        });
        assert!(format!("{}", result.err().unwrap()).contains("invalid selection"));
        assert!(read.get());
        assert!(
            ExclusiveFileLock::try_acquire(lock_file(&root)).is_ok(),
            "failed selection releases the guard"
        );
    }
    #[test]
    fn a_prepared_selection_retains_the_shared_guard_until_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("simulation");
        let selected = guarded_selection(&root, || {
            Ok(SimulatorArtifact {
                summary: SimulatorArtifactSummary {
                    package: "fixture".into(),
                    version: "different-compatible-release".into(),
                    binary: "simulator".into(),
                    source: "fixture".into(),
                    executable: root.join("simulator"),
                },
            })
        })
        .unwrap();
        assert!(ExclusiveFileLock::try_acquire(lock_file(&root)).is_err());
        drop(selected);
        assert!(ExclusiveFileLock::try_acquire(lock_file(&root)).is_ok());
        let reader = SharedFileLock::try_acquire(lock_file(&root)).unwrap();
        assert!(ExclusiveFileLock::try_acquire(lock_file(&root)).is_err());
        drop(reader);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    use crate::project::bundle;
    use crate::project::{CargoOptions, Project};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Stages one minimal one-driver robot project with a leased brain
    /// setpoint source, plus fake probe/cargo executables and a staged
    /// supervisor source crate the authored selection acquires.
    fn stage_freeze_robot() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let guard = tempfile::tempdir().expect("robot tempdir");
        let root = guard.path().to_owned();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
        let base = fixtures.join("robot-base");
        fs::copy(base.join("model.xml"), root.join("model.xml")).expect("copy fixture asset");
        fs::create_dir_all(root.join("src")).expect("src dir");
        let runtime = r#"{
  "schema": "phoxal/artifact/v0",
  "record": "runtime",
  "period_ms": 20,
  "timeout_ms": 100,
  "init_timeout_ms": 1000,
  "config_schema": {
    "type": "null"
  },
  "inputs": [
    {
      "name": "encoders",
      "delivery": "observation_latest",
      "max_age_ms": null,
      "max_items": null,
      "max_bytes": null,
      "port": null,
      "signature": null,
      "request_fqn": null,
      "response_fqn": "phoxal.robotics.v1.EncoderSample",
      "response_max_bytes": null,
      "response_max_items": null
    }
  ],
  "outputs": [
    {
      "name": "actuators",
      "port": "actuators",
      "signature": {
        "endpoint": "actuators",
        "service": "phoxal.motion.v1.MotionCommands",
        "method": "actuators",
        "shape": "observation",
        "request": "google.protobuf.Empty",
        "response": "phoxal.component.actuator.v1.ActuatorSetpoint",
        "retained_latest": true,
        "lease_valid_for_ms": 100
      },
      "max_items": null,
      "max_bytes": 4096,
      "max_request_bytes": null,
      "every_steps": null,
      "bootstrap": false,
      "timeout_ms": null
    }
  ]
}"#;
        let build_rs = r##"fn main() {
    let payload = r#"__RUNTIME__"#;
    let length = payload.len() as u32;
    let mut frame: Vec<u8> = Vec::new();
    frame.extend(b"PHXART0\n");
    frame.extend(length.to_le_bytes());
    frame.extend(payload.as_bytes());
    let total = frame.len();
    let bytes: Vec<String> = frame.iter().map(|byte| byte.to_string()).collect();
    let bytes = bytes.join(", ");
    let artifact = format!("[used] placeholder");
    let _ = artifact;
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let source = format!(
        "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_art\"))]\n#[cfg_attr(target_os = \"linux\", unsafe(link_section = \".phoxal_art\"))]\nstatic PHOXAL_ARTIFACT: [u8; {total}] = [{bytes}];\n"
    );
    std::fs::write(out_dir.join("artifact.rs"), source).expect("write artifact.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
"##
        .replace("__RUNTIME__", runtime);
        fs::write(root.join("build.rs"), build_rs).expect("write build.rs");
        fs::write(
            root.join("src/main.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/artifact.rs\"));\nfn main() {}\n",
        )
        .expect("write main.rs");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\n\n[package]\nname = \"freeze-proof-robot\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\npublish = false\n",
        )
        .expect("write Cargo.toml");
        let ddsm = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/freeze-driver")
            .canonicalize()
            .expect("ddsm115 component path");
        let ddsm = relative_path(&root, &ddsm);
        fs::write(
            root.join("robot.yaml"),
            format!(
                "schema: phoxal/robot/v0\nrobot:\n  id: freeze-proof-robot\n  model: model.xml\n  components:\n    d1:\n      source:\n        path: {}\n      mount_site: front_left_wheel_mount\n      driver:\n        connection: {{ type: serial, port: /dev/ttyUSB0, baud: 115200 }}\n        config: {{ id: 1 }}\nbrain: {{}}\nservices: {{}}\nconnections:\n  d1.actuator: brain.actuators\n  brain.encoders: d1.encoder\nsupervisor:\n  source: {{ path: .fixture-supervisor }}\n  binary: phoxal-supervisor\n",
                ddsm.display()
            ),
        )
        .expect("write robot.yaml");

        let scene = root.join("scene.xml");
        fs::write(&scene, "<mujoco model=\"freeze\"><compiler angle=\"radian\"/><include file=\"part.xml\"/><option timestep=\"0.01\"/></mujoco>\n")
            .expect("write scene");
        fs::write(
            root.join("part.xml"),
            "<mujocoinclude><option gravity=\"0 0 -9.81\"/></mujocoinclude>\n",
        )
        .expect("write included resource");

        // A fake simulator whose probe reports the scene's content digest as
        // the model identity and the one expected provider set.
        let simulator = root.join("fake-simulator.sh");
        fs::write(
            &simulator,
            "#!/bin/sh\nif [ \"$1\" = stage-scene ]; then\n  mkdir -p \"$5\"\n  cp \"$3\" \"$5/$(basename \"$3\")\"\n  if [ -f \"$(dirname \"$3\")/part.xml\" ]; then cp \"$(dirname \"$3\")/part.xml\" \"$5/part.xml\"; fi\n  exit 0\nfi\n# fake probe: identity = sha256 of the received (frozen) scene;\n# the probe then mutates the LIVE scene so a reread would differ.\nscene=\"$3\"\nidentity=$(shasum -a 256 \"$scene\" 2>/dev/null | cut -d' ' -f1)\nif [ -n \"$FAKE_LIVE_SCENE\" ]; then\n  printf '<mujoco model=\"probe-mutated\"/>' > \"$FAKE_LIVE_SCENE\"\nfi\nprintf '{\"model_identity\":\"%s\",\"quantum_ns\":10000000,\"providers\":[{\"rate_microhertz\":50000000,\"service_instance\":\"d1\",\"port\":\"encoder\",\"shape\":\"observation\",\"retained_latest\":false,\"lease_valid_for_ms\":null,\"input_fqn\":\"google.protobuf.Empty\",\"payload_fqn\":\"phoxal.robotics.v1.EncoderSample\"}],\"actuation_bindings\":[{\"service_instance\":\"brain\",\"port\":\"actuators\",\"payload_fqn\":\"phoxal.component.actuator.v1.ActuatorSetpoint\",\"actuator_ids\":[\"d1.motor\"]}]}' \"$identity\"\n",
        )
        .expect("write fake simulator");
        make_executable(&simulator);

        // A staged fixture supervisor source: the robot's authored
        // selection acquires it through the ordinary Cargo install path
        // during preparation, and its binary embeds the pinned application
        // contract record for this host target.
        stage_fixture_supervisor(&root);

        // A counting Cargo wrapper: every real invocation is logged before
        // forwarding to the underlying cargo.
        let log = root.join("cargo-invocations.log");
        let real_cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let wrapper = root.join("cargo-wrapper.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{log}\"\nexec \"{real_cargo}\" \"$@\"\n",
                log = log.display()
            ),
        )
        .expect("write cargo wrapper");
        make_executable(&wrapper);
        (guard, scene, simulator, wrapper)
    }

    /// One lexical relative path from `from` (a directory) to `to`.
    fn relative_path(from: &Path, to: &Path) -> PathBuf {
        let from = from.canonicalize().expect("canonical from");
        let to = to.canonicalize().expect("canonical to");
        let mut from_parts = from.components().peekable();
        let mut to_parts = to.components().peekable();
        while from_parts
            .peek()
            .is_some_and(|part| to_parts.peek() == Some(part))
        {
            from_parts.next();
            to_parts.next();
        }
        let mut result = PathBuf::new();
        while from_parts.next().is_some() {
            result.push("..");
        }
        for part in to_parts {
            result.push(part);
        }
        result
    }

    /// Stages the fixture supervisor source crate under the robot root.
    ///
    /// The crate embeds the pinned application contract as ordinary static
    /// Rust through the SDK's compiled constants, so the authored `--locked`
    /// acquisition validates it without executing anything. A lockfile is
    /// generated offline at staging time so `--locked` acquisition resolves
    /// without network access.
    pub(super) fn stage_fixture_supervisor(root: &Path) {
        let crate_dir = root.join(".fixture-supervisor");
        fs::create_dir_all(crate_dir.join("src")).expect("fixture supervisor dir");
        let phoxal_crate = crate::project::test_support::sdk_root();
        let phoxal_dep = relative_path(&crate_dir, &phoxal_crate);
        fs::write(
            crate_dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"phoxal-supervisor\"\nversion = \"0.0.0-dev.8\"\nedition = \"2021\"\npublish = false\n\n[workspace]\n\n[dependencies]\nphoxal = {{ path = {:?}, default-features = false, features = [] }}\n\n[[bin]]\nname = \"phoxal-supervisor\"\npath = \"src/main.rs\"\n",
                phoxal_dep.display().to_string()
            ),
        )
        .expect("fixture supervisor manifest");
        fs::write(
            crate_dir.join("src/main.rs"),
            "use phoxal::artifact::application::{encode_application_contract, ApplicationContract, APPLICATION_RECORD_BYTES, BUNDLE_CONTRACT, EXECUTION_PROTOCOL_CONTRACT, SIMULATION_PROTOCOL_CONTRACT, SUPERVISOR_LAUNCH_CONTRACT, HOST_EXECUTION_TARGET};\nconst APPLICATION_CONTRACT: ApplicationContract = ApplicationContract { bundle: Some(BUNDLE_CONTRACT), launch: SUPERVISOR_LAUNCH_CONTRACT, execution: Some(EXECUTION_PROTOCOL_CONTRACT), simulation: Some(SIMULATION_PROTOCOL_CONTRACT), target: HOST_EXECUTION_TARGET };\n#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_app\"))]\n#[cfg_attr(target_os = \"linux\", unsafe(link_section = \".phoxal_app\"))]\nstatic EMBEDDED_APPLICATION_CONTRACT: [u8; APPLICATION_RECORD_BYTES] = encode_application_contract(&APPLICATION_CONTRACT);\nfn main() { println!(\"phoxal-supervisor 0.0.0-dev.8\"); }\n",
        )
        .expect("fixture supervisor main");
        // The authored `--locked` acquisition requires a current lockfile;
        // resolve one offline from the local Cargo cache at staging time.
        let mut command = std::process::Command::new(
            std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()),
        );
        command
            .args([
                "generate-lockfile",
                "--manifest-path",
                crate_dir.join("Cargo.toml").to_str().expect("UTF-8 path"),
                "--offline",
            ])
            .env_remove("CARGO_TARGET_DIR");
        if let Some(config) = crate::project::cargo::registry_config(root) {
            command.arg("--config").arg(config);
        }
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("generate the fixture supervisor lockfile: {error}"));
        assert!(
            output.status.success(),
            "fixture supervisor lockfile resolves offline:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(test)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .expect("stat fixture executable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("chmod fixture executable");
    }

    fn invocation_count(log: &Path) -> usize {
        fs::read_to_string(log)
            .map(|content| content.lines().filter(|line| !line.is_empty()).count())
            .unwrap_or(0)
    }

    fn freeze_options(wrapper: &Path) -> CargoOptions {
        CargoOptions {
            cargo_path: Some(wrapper.to_owned()),
            ..CargoOptions::default()
        }
    }

    #[test]
    fn finalization_reuses_the_frozen_closure_without_new_operations() {
        let (guard, scene, simulator, wrapper) = stage_freeze_robot();
        let root = guard.path();
        let project = Project::discover(root).expect("discover fixture robot");
        let options = freeze_options(&wrapper);
        // The probe mutates the live scene after the freeze: the frozen
        // bytes the probe measures and the closure finalizes must agree.
        // The launched executable is a wrapper exporting the live path for
        // the fake probe, keeping the test's own environment untouched.
        let wrapper = root.join("fake-simulator-entry.sh");
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexport FAKE_LIVE_SCENE=\"{scene}\"\nexec \"{simulator}\" \"$@\"\n",
                scene = scene.display(),
                simulator = simulator.display(),
            ),
        )
        .expect("write simulator entry wrapper");
        let wrapper = crate::project::simulation::simulator_fixture(&wrapper);
        let request = SimulationRunOptions::new(
            scene.clone(),
            SimulationPresentation::Headless,
            SimulationBound::Steps(4),
        )
        .expect("request")
        .with_simulator_executable(&wrapper);
        let original_scene = fs::read(&scene).expect("read original scene");
        let original_model = fs::read(root.join("model.xml")).expect("read original model");
        let original_part = fs::read(root.join("part.xml")).expect("read included resource");
        let prepared = project
            .prepare(&options)
            .expect("prepare before harness launch");
        let robot = crate::project::bundle::freeze_robot_closure(&prepared, &options)
            .expect("freeze robot before the Cargo test lock is held");
        let operations_before_probe = invocation_count(&root.join("cargo-invocations.log"));
        // A live robot asset edit after harness launch must not escape its precompiled closure.
        fs::write(
            root.join("model.xml"),
            "<mujoco model=\"after-harness-launch\"/>\n",
        )
        .expect("mutate live model");
        let snapshot = prepare_simulation_from_robot(&prepared, &robot, &options, &request)
            .expect("the fixture simulation prepares without nested Cargo");
        assert_eq!(
            operations_before_probe,
            invocation_count(&root.join("cargo-invocations.log")),
            "case preparation/probe must add zero Cargo operations while its harness is running"
        );
        let probe_output = probe_bundle_path(&snapshot.prepared);
        // The probe bundle is assembled from the frozen closure itself: its
        // artifact inventory and supervisor are the frozen ones, so no
        // second live assembly ran between freeze and probe.
        let probe_manifest: phoxal::artifact::bundle::BundleManifest = serde_json::from_slice(
            &fs::read(probe_output.join("manifest.json")).expect("read probe manifest"),
        )
        .expect("decode probe manifest");
        let phoxal::artifact::bundle::BundleManifest::V0 {
            artifacts: probe_artifacts,
            ..
        } = &probe_manifest;
        let frozen_artifact_ids = {
            let mut ids: Vec<String> = snapshot.frozen.inventory.keys().cloned().collect();
            ids.sort();
            ids
        };
        let mut probe_artifact_ids: Vec<String> = probe_artifacts
            .iter()
            .map(|artifact| artifact.id.clone())
            .collect();
        probe_artifact_ids.sort();
        assert_eq!(
            probe_artifact_ids, frozen_artifact_ids,
            "the probe bundle carries exactly the frozen artifact inventory"
        );
        assert!(
            probe_output.join("bin/supervisor").is_file(),
            "the probe bundle carries the frozen supervisor copy"
        );
        let after_prepare = invocation_count(&root.join("cargo-invocations.log"));
        assert!(
            after_prepare > 0,
            "preparation itself must exercise Cargo at least once"
        );
        // The pinned selection resolves for both probe and run; executable
        // bytes are trusted and never digested.
        snapshot
            .frozen
            .verify_simulator()
            .expect("the pinned application selection resolves");

        // Mutate every live source the finalization could otherwise reread:
        // the scene, the robot model, and the brain source itself.
        fs::write(
            &scene,
            "<mujoco model=\"mutated\"><option timestep=\"0.02\"/></mujoco>\n",
        )
        .expect("mutate scene");
        fs::write(root.join("model.xml"), "<model name=\"mutated\" />\n").expect("mutate model");
        fs::write(root.join("src/main.rs"), "fn main() {}\n").expect("mutate brain source");
        fs::write(
            root.join("part.xml"),
            "<mujocoinclude><option gravity=\"0 0 0\"/></mujocoinclude>\n",
        )
        .expect("mutate included resource");

        let output = root.join("target/frozen-bundle");
        let facts = snapshot.facts.clone();
        let frozen_scene = snapshot.frozen.scene.clone();
        let bundle = bundle::finalize_simulation_bundle(
            &snapshot.prepared,
            &snapshot.frozen,
            &output,
            Some(&facts),
            None,
        )
        .expect("finalization succeeds from the frozen closure");
        assert_eq!(
            fs::read(
                snapshot
                    .frozen
                    .scene
                    .parent()
                    .expect("frozen scene root")
                    .join("part.xml")
            )
            .expect("frozen include"),
            original_part
        );
        assert_eq!(
            fs::read(bundle.root().join("scene/part.xml")).expect("published include"),
            original_part
        );
        assert!(
            !bundle.root().join("scene/target").exists(),
            "build outputs must never enter the scene closure"
        );
        let after_finalize = invocation_count(&root.join("cargo-invocations.log"));
        assert_eq!(
            after_prepare, after_finalize,
            "finalization runs no Cargo operation of its own"
        );

        // The frozen scene copy and the staged robot model still carry the
        // original bytes the probe measured.
        assert_eq!(
            fs::read(&frozen_scene).expect("read frozen scene"),
            original_scene,
            "the frozen scene copy retains the probed bytes"
        );
        let manifest: phoxal::artifact::bundle::BundleManifest = serde_json::from_slice(
            &fs::read(bundle.root().join("manifest.json")).expect("read manifest"),
        )
        .expect("decode manifest");
        let phoxal::artifact::bundle::BundleManifest::V0 {
            simulation, model, ..
        } = manifest;
        let simulation = simulation.expect("simulation contract present");
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest as _;
        hasher.update(&original_scene);
        assert_eq!(
            simulation.model_identity,
            format!("{:x}", hasher.finalize()),
            "the admitted contract names the probed scene identity"
        );
        let model = model.expect("robot model staged");
        let staged_entry = bundle.root().join(&model.entry);
        assert_eq!(
            fs::read(&staged_entry).expect("read staged robot model"),
            original_model,
            "the staged robot model retains the prepared bytes"
        );

        // Restore the brain source and robot model: the freeze phase proved
        // finalization ignored them, and a fresh preparation must still see
        // a buildable robot whose only remaining change is the scene.
        fs::write(
            root.join("src/main.rs"),
            "include!(concat!(env!(\"OUT_DIR\"), \"/artifact.rs\"));\nfn main() {}\n",
        )
        .expect("restore brain source");
        fs::write(
            root.join("model.xml"),
            "<model name=\"cargo-phoxal-test-fixture\" />\n",
        )
        .expect("restore robot model");
        fs::write(
            scene.parent().expect("scene parent").join("part.xml"),
            "<mujocoinclude><option gravity=\"0 0 -9.81\"/></mujocoinclude>\n",
        )
        .expect("restore scene resource");

        // A later invocation prepares freshly and sees the mutated scene.
        let request = SimulationRunOptions::new(
            scene.clone(),
            SimulationPresentation::Headless,
            SimulationBound::Steps(4),
        )
        .expect("request")
        .with_simulator_executable(crate::project::simulation::simulator_fixture(&simulator));
        let second = prepare_simulation(&project, &options, &request)
            .expect("a fresh preparation sees the mutated inputs");
        assert_ne!(
            second.facts.model_identity, facts.model_identity,
            "the second probe measures the mutated scene"
        );
    }
}
/// A native-free executable fixture carrying the interfaces actually used by its scripted probe.
#[cfg(test)]
fn simulator_fixture(script: &std::path::Path) -> PathBuf {
    use phoxal::artifact::application::*;
    let record = ApplicationContract {
        bundle: Some(BUNDLE_CONTRACT),
        launch: SIMULATOR_LAUNCH_CONTRACT,
        execution: None,
        simulation: Some(SIMULATION_PROTOCOL_CONTRACT),
        target: HOST_EXECUTION_TARGET,
    };
    let bytes = encode_application_contract(&record);
    let data = bytes
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let source = script.with_extension("rs");
    let executable = script.with_extension("fixture");
    let code = format!(
        r##"
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__phoxal_app"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".phoxal_app"))]
static RECORD: [u8; 512] = [{data}];
fn main() {{ let status = std::process::Command::new("sh").arg({script:?}).args(std::env::args_os().skip(1)).status().unwrap(); std::process::exit(status.code().unwrap_or(1)); }}
"##,
        script = script.to_str().unwrap()
    );
    std::fs::write(&source, code).unwrap();
    let output = std::process::Command::new("rustc")
        .args(["--edition=2024", "--crate-name", "simulator_fixture"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}
