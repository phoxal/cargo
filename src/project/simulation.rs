//! Independent simulator provisioning and local finite-run orchestration.
//!
//! A simulator is an application selected outside the robot Cargo graph. This
//! module keeps its own small Cargo project and lockfile, probes the native
//! application's explicit model facts, then asks the prepared robot project
//! to assemble a simulation bundle.

use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::NamedTempFile;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::cargo::CargoOptions;
use crate::project::{CompiledBundle, Error, Project, SimulationModelFacts};
use phoxal::artifact::bundle::BundleManifest;
use phoxal::artifact::simulation::SimulationRunReport;
#[cfg(test)]
use phoxal::artifact::simulation::SimulatorTerminalEvidence;
use phoxal::artifact::simulation_run::{
    SimulationApplicationReference, SimulationBundleReference, SimulationCapturePolicy,
    SimulationCaptureRequirement, SimulationExecutionBounds, SimulationModelReference,
    SimulationProgram, SimulationRunSpecification,
};
use phoxal::scenario::CapturePolicy;
use phoxal::scenario::plan_support::{Action, Capture, Program};

/// The official independently installed native simulation application.
pub const DEFAULT_SIMULATOR_PACKAGE: &str = "phoxal-simulator";
/// The binary target exposed by the simulator application.
pub const DEFAULT_SIMULATOR_BINARY: &str = "phoxal-simulator";
// Allow the supervisor its existing whole-graph admission budget.
// This controls process startup only, not runtime cadence or Safety validity.
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
            execution_timeout: DEFAULT_EXECUTION_TIMEOUT,
            cleanup_timeout: Duration::from_secs(5),
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

use phoxal::artifact::installation::SimulatorArtifactSummary;

/// One command-scoped prepared simulation snapshot.
///
/// The resolved participant graph, staged asset closure, selected simulator,
/// and native model facts are prepared and probed exactly once per fixture
/// request; finalization reuses the same snapshot so a source or model change
/// cannot mix old probe facts with a new bundle. A later request refreshes
/// inputs normally by preparing a fresh snapshot.
pub struct PreparedSimulation {
    pub(crate) cargo_options: CargoOptions,
    pub(crate) prepared: crate::project::PreparedProject,
    pub(crate) frozen: crate::project::bundle::FrozenSimulation,
    pub(crate) facts: SimulationModelFacts,
    // Drop removes the native probe assembly when the command snapshot ends.
    _probe_directory: tempfile::TempDir,
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

    // The installed application is selected before Project::prepare. A missing
    // or incompatible application therefore fails before the robot Cargo
    // manifest or its owning Cargo.lock can be changed.
    let scene = canonical_scene(request.scene())?;
    let simulator = select_installed_simulator(request)?;
    let prepared = project.prepare(cargo_options, &super::NoPhases)?;
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
    probe_snapshot(prepared, frozen, cargo_options, request)
}

/// Prepares a case from robot resources frozen before the Cargo harness launch.
pub(crate) fn prepare_simulation_from_robot(
    prepared: &crate::project::PreparedProject,
    robot: &super::bundle::FrozenRobot,
    cargo_options: &CargoOptions,
    request: &SimulationRunOptions,
) -> Result<PreparedSimulation, Error> {
    cargo_options.validate()?;
    validate_request(request)?;
    let scene = canonical_scene(request.scene())?;
    let simulator = select_installed_simulator(request)?;
    let frozen = super::bundle::freeze_scene_closure(robot, &scene, &simulator)?;
    probe_snapshot(prepared.clone(), frozen, cargo_options, request)
}

fn probe_snapshot(
    prepared: crate::project::PreparedProject,
    frozen: super::bundle::FrozenSimulation,
    cargo_options: &CargoOptions,
    request: &SimulationRunOptions,
) -> Result<PreparedSimulation, Error> {
    let probe_directory = tempfile::Builder::new()
        .prefix("phoxal-probe-")
        .tempdir()
        .map_err(|source| Error::ArtifactFile {
            path: std::env::temp_dir(),
            source,
        })?;
    let probe_output = probe_directory.path().join("build");
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
    if cargo_options.diagnostics_json() {
        eprintln!(
            "cargo phoxal: simulation prepared from one native probe (model {}, quantum {} ns)",
            facts.model_identity, facts.quantum_ns
        );
    } else {
        eprintln!(
            "Simulation: Native model prepared (model {}, quantum {} ns).",
            facts.model_identity, facts.quantum_ns
        );
    }
    Ok(PreparedSimulation {
        cargo_options: cargo_options.clone(),
        prepared,
        frozen,
        facts,
        _probe_directory: probe_directory,
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
    let bundle = finalize_prepared(&snapshot, request, scenario)?;
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
pub(crate) fn finalize_prepared(
    snapshot: &PreparedSimulation,
    request: &SimulationRunOptions,
    scenario: Option<&Program>,
) -> Result<CompiledBundle, Error> {
    let output = match &request.output {
        Some(output) => output.clone(),
        None => snapshot
            .prepared
            .default_bundle_path(&snapshot.cargo_options)?,
    };
    let bundle = crate::project::bundle::finalize_simulation_bundle(
        &snapshot.prepared,
        &snapshot.frozen,
        &output,
        Some(&snapshot.facts),
        scenario.map(|program| crate::project::bundle::SimulationRunInput { program }),
    )?;
    if snapshot.cargo_options.diagnostics_json() {
        eprintln!("cargo phoxal: finalizing the simulation bundle from the prepared snapshot");
    } else {
        eprintln!("Simulation: Runnable build prepared.");
    }
    Ok(bundle)
}

fn validate_request(request: &SimulationRunOptions) -> Result<(), Error> {
    request.bound.validate()?;
    validate_identity_part("scope", &request.scope)?;
    validate_identity_part("supervisor_id", &request.supervisor_id)?;
    validate_identity_part("run_id", &request.run_id)?;
    if request.execution_timeout.is_zero() || request.cleanup_timeout.is_zero() {
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

/// Selects the installed simulator application for one launch.
///
/// The selection resolves the explicit executable, `PHOXAL_SIMULATOR`, or
/// `PATH` precedence to one canonical regular file, validates its embedded
/// application contract, and returns the directly needed summary record. It
/// never installs anything and must run before source preparation so a
/// missing or incompatible application fails before any manifest or lock
/// can change.
pub(crate) fn select_installed_simulator(
    request: &SimulationRunOptions,
) -> Result<SimulatorArtifactSummary, Error> {
    let executable = selected_simulator_executable(request.simulator_executable.as_deref())?;
    Ok(SimulatorArtifactSummary {
        package: request.simulator_package.clone(),
        version: "installed".into(),
        binary: request.simulator_binary.clone(),
        source: "user-installed-executable".into(),
        executable,
    })
}

/// Resolve and inspect the simulator before every public launch, including reuse of a build.
pub(crate) fn selected_simulator_executable(explicit: Option<&Path>) -> Result<PathBuf, Error> {
    let selected = explicit
        .map(Path::to_owned)
        .or_else(|| std::env::var_os("PHOXAL_SIMULATOR").map(PathBuf::from));
    let path = selected
        .or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths)
                    .map(|directory| directory.join(DEFAULT_SIMULATOR_BINARY))
                    .find(|path| path.is_file())
            })
        })
        .ok_or_else(|| {
            simulation_error(
                "phoxal-simulator is missing; install the complete application using https://github.com/phoxal/simulator#readme and select its executable with PHOXAL_SIMULATOR or PATH",
            )
        })?;
    if !path.try_exists().map_err(|source| Error::ArtifactFile {
        path: path.clone(),
        source,
    })? {
        return Err(simulation_error(format!(
            "selected phoxal-simulator is missing at {}; correct PHOXAL_SIMULATOR or reinstall the complete application using https://github.com/phoxal/simulator#readme",
            path.display()
        )));
    }
    validate_simulator_application(&path)?;
    path.canonicalize()
        .map_err(|source| Error::ArtifactFile { path, source })
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

fn probe(
    simulator: &SimulatorArtifactSummary,
    scene: &Path,
    bundle: &Path,
    request: &SimulationRunOptions,
) -> Result<SimulationModelFacts, Error> {
    let mut command = Command::new(&simulator.executable);
    command.args([
        "probe",
        &scene.display().to_string(),
        "--build",
        &bundle.display().to_string(),
        "--json",
    ]);
    let output = crate::owned_process::probe_output(
        &mut command,
        request.execution_timeout,
        request.cleanup_timeout,
    )
    .map_err(|source| Error::SimulationInvalid {
        message: format!(
            "simulator probe {}: {source}",
            simulator.executable.display()
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
    simulator: &SimulatorArtifactSummary,
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
    let BundleManifest::V0 { robot_id, .. } = &manifest;

    program
        .verify_identity()
        .map_err(|error| simulation_error(format!("scenario program identity: {error}")))?;

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
            package: simulator.package.clone(),
            version: simulator.version.clone(),
            binary: simulator.binary.clone(),
        },
        program: SimulationProgram {
            bytes: program.program_bytes().to_vec(),
        },
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
    simulator: &SimulatorArtifactSummary,
    bundle: &CompiledBundle,
    scene: &Path,
    facts: &SimulationModelFacts,
    request: &SimulationRunOptions,
    scenario: Option<&Program>,
) -> Result<SimulationRunReport, Error> {
    let directory = tempfile::Builder::new()
        .prefix("phoxal-run-spec-")
        .tempdir_in("/tmp")
        .map_err(|source| Error::ArtifactFile {
            path: std::env::temp_dir(),
            source,
        })?;
    let mut command = Command::new(&simulator.executable);
    command
        .arg("run")
        .arg(scene)
        .arg("--build")
        .arg(bundle.root())
        .arg("--scope")
        .arg(&request.scope)
        .arg("--supervisor-id")
        .arg(&request.supervisor_id)
        .arg("--run-id")
        .arg(&request.run_id)
        .stdin(Stdio::null());
    if let Some(program) = scenario {
        let path = directory.path().join("simulation-run.json");
        atomic_json(
            &path,
            &build_run_specification(bundle, scene, facts, simulator, request, program)?,
        )?;
        command.arg("--simulation-run").arg(path);
    }
    match request.bound {
        SimulationBound::Steps(steps) => {
            command.arg("--steps").arg(steps.to_string());
        }
        SimulationBound::Duration(duration) => {
            command.arg("--duration").arg(format!("{duration}s"));
        }
    }
    if request.presentation == SimulationPresentation::Headless {
        command.arg("--headless");
    } else if !request.auto_run {
        command.arg("--paused");
    }
    let output =
        bounded_output(&mut command, request.execution_timeout).map_err(simulation_error)?;
    read_run_report(&output)
}

/// Decodes the actual simulator output and checks its process exit identity.
pub(super) fn read_run_report(output: &std::process::Output) -> Result<SimulationRunReport, Error> {
    let line = output
        .stdout
        .split(|byte| *byte == b'\n')
        .rfind(|line| !line.is_empty())
        .ok_or_else(|| {
            simulation_error(format!(
                "simulator returned no terminal report: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        })?;
    let report: SimulationRunReport = serde_json::from_slice(line).map_err(|e| {
        simulation_error(format!(
            "invalid simulator terminal report: {e}; {}",
            String::from_utf8_lossy(&output.stderr)
        ))
    })?;
    let SimulationRunReport::V0 {
        simulator_exit_code,
        ..
    } = &report;
    if *simulator_exit_code != output.status.code() {
        return Err(simulation_error(format!(
            "simulator reported exit code {simulator_exit_code:?}, but its process returned {}",
            output.status
        )));
    }
    Ok(report)
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

#[cfg(test)]
fn provider_contract_verified(stdout: &[u8], presentation: SimulationPresentation) -> bool {
    terminal_evidence(stdout)
        .as_ref()
        .is_some_and(|evidence| terminal_evidence_verified(evidence, presentation))
}

#[cfg(test)]
fn terminal_evidence(stdout: &[u8]) -> Option<SimulatorTerminalEvidence> {
    let line = stdout
        .split(|byte| *byte == b'\n')
        .rfind(|line| !line.is_empty())?;
    serde_json::from_slice::<SimulatorTerminalEvidence>(line).ok()
}

#[cfg(test)]
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

/// A native-free executable fixture carrying the interfaces actually used by its scripted probe.
#[cfg(test)]
fn simulator_fixture(script: &std::path::Path, live_scene: Option<&std::path::Path>) -> PathBuf {
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
    let live_scene = live_scene.map(|path| path.to_str().unwrap());
    let code = format!(
        "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_app\"))]\n#[cfg_attr(target_os = \"linux\", unsafe(link_section = \".phoxal_app\"))]\nstatic RECORD: [u8; 512] = [{data}];\nconst LIVE_SCENE: Option<&str> = {live_scene:?};\nconst FIXTURE_CONTROL: &str = {control:?};\n{}",
        include_str!("../../tests/fixtures/process/native_probe.rs"),
        control = executable.to_str().unwrap(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::bundle;

    #[test]
    fn native_probe_public_fixture_and_valid_facts() {
        let (guard, scene, simulator, wrapper) = stage_freeze_robot();
        let executable = simulator_fixture(&simulator, None);
        let request = SimulationRunOptions::new(
            scene.clone(),
            SimulationPresentation::Headless,
            SimulationBound::Steps(4),
        )
        .unwrap();
        let artifact = SimulatorArtifactSummary {
            executable: executable.clone(),
            ..select_installed_simulator(&request.clone().with_simulator_executable(&executable))
                .unwrap()
        };
        let facts = probe(&artifact, &scene, guard.path(), &request).unwrap();
        assert_eq!(facts.quantum_ns, 10_000_000);
        assert!(!facts.model_identity.is_empty());
        if let Some(evidence) = std::env::var_os("PHOXAL_PROBE_EVIDENCE_DIR") {
            let evidence = PathBuf::from(evidence);
            fs::create_dir_all(&evidence).unwrap();
            fs::write(evidence.join("fixture.json"),serde_json::to_vec_pretty(&serde_json::json!({"root":guard.path(),"scene":scene,"simulator":executable,"cargo":wrapper})).unwrap()).unwrap();
            let _ = guard.keep();
        }
    }

    fn controlled_probe() -> (
        tempfile::TempDir,
        SimulatorArtifactSummary,
        SimulationRunOptions,
    ) {
        let guard = tempfile::tempdir().unwrap();
        let scene = guard.path().join("scene.xml");
        fs::write(&scene, "<mujoco/>").unwrap();
        let executable = simulator_fixture(&guard.path().join("simulator"), None);
        let request = SimulationRunOptions::new(
            scene,
            SimulationPresentation::Headless,
            SimulationBound::Steps(4),
        )
        .unwrap()
        .with_simulator_executable(&executable);
        let artifact = select_installed_simulator(&request).unwrap();
        (guard, artifact, request)
    }
    fn recorded_probe_pids_absent(executable: &Path) {
        let text = fs::read_to_string(executable.with_extension("pids")).unwrap();
        assert!(!text.is_empty());
        for pid in text.lines().map(|line| line.parse::<i32>().unwrap()) {
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "owned PID {pid} survived probe"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
    #[test]
    fn native_probe_rejects_malformed_incomplete_and_nonzero_facts() {
        let (guard, artifact, request) = controlled_probe();
        for (mode, message) in [
            ("malformed", "no valid SimulationModelFacts"),
            ("incomplete", "incomplete model identity or quantum"),
            ("nonzero", "failed (17): probe refused fixture"),
        ] {
            fs::write(artifact.executable.with_extension("mode"), mode).unwrap();
            let error = probe(&artifact, request.scene(), guard.path(), &request)
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
            assert!(!artifact.executable.with_extension("run-started").exists());
        }
    }
    #[test]
    fn native_probe_deadline_cleans_supported_descendants() {
        let (guard, artifact, mut request) = controlled_probe();
        request.execution_timeout = Duration::from_millis(750);
        request.cleanup_timeout = Duration::from_secs(1);
        for mode in [
            "stall",
            "stall-resistant",
            "stall-resistant-detached-no-exec",
            "stall-resistant-detached-exec",
            "stall-resistant-detached-drop-env",
            "stall-resistant-detached-drop-fd",
        ] {
            let _ = fs::remove_file(artifact.executable.with_extension("pids"));
            let _ = fs::remove_file(artifact.executable.with_extension("ready"));
            fs::write(artifact.executable.with_extension("mode"), mode).unwrap();
            let start = Instant::now();
            let error = probe(&artifact, request.scene(), guard.path(), &request)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("execution deadline of 750 ms expired"),
                "{mode}: {error}"
            );
            assert!(
                error.contains("probe stdout retained") && error.contains("probe stderr retained"),
                "{error}"
            );
            assert!(
                start.elapsed()
                    < request.execution_timeout
                        + request.cleanup_timeout
                        + Duration::from_millis(500)
            );
            assert!(
                artifact.executable.with_extension("ready").exists(),
                "{mode} never reached readiness"
            );
            recorded_probe_pids_absent(&artifact.executable);
        }
    }
    #[test]
    fn native_probe_success_cleans_fast_detached_helpers() {
        let (guard, artifact, mut request) = controlled_probe();
        request.cleanup_timeout = Duration::from_secs(1);
        for mode in [
            "valid-detached-resistant-no-exec",
            "valid-detached-resistant-exec",
            "valid-detached-resistant-drop-env",
            "valid-detached-resistant-drop-fd",
        ] {
            let _ = fs::remove_file(artifact.executable.with_extension("pids"));
            fs::write(artifact.executable.with_extension("mode"), mode).unwrap();
            let facts = probe(&artifact, request.scene(), guard.path(), &request).unwrap();
            assert_eq!(facts.quantum_ns, 10_000_000);
            assert!(!facts.model_identity.is_empty());
            recorded_probe_pids_absent(&artifact.executable);
        }
    }
    #[test]
    fn native_probe_output_limit_preserves_bounded_diagnostics() {
        let (guard, artifact, mut request) = controlled_probe();
        request.execution_timeout = Duration::from_secs(5);
        request.cleanup_timeout = Duration::from_secs(1);
        fs::write(artifact.executable.with_extension("mode"), "oversized").unwrap();
        let error = probe(&artifact, request.scene(), guard.path(), &request)
            .unwrap_err()
            .to_string();
        assert!(error.contains("128 MiB channel limit"), "{error}");
        assert!(error.contains("truncated"));
        assert!(error.len() < 132 * 1024);
        recorded_probe_pids_absent(&artifact.executable);
    }

    #[test]
    fn simulation_success_requires_orderly_supervisor_exit() {
        let base = serde_json::json!({
            "schema": "phoxal/simulation-run/v0",
            "scene": "scene.xml", "bundle": "bundle",
            "simulator": {"package": "phoxal-simulator", "version": "independent-release",
                "binary": "phoxal-simulator", "source": "installed-executable", "executable": "simulator"},
            "scope": "local", "supervisor_id": "local", "run_id": "test",
            "supervisor_ready": true, "provider_contract_verified": true,
            "simulator_exit_code": 0, "simulator_wall_time_ns": 1,
            "simulator_stdout": "", "simulator_stderr": "", "scenario": null, "terminal": null,
            "cleanup": {"supervisor_stop_requested": true, "supervisor_exited": true,
                "supervisor_killed": false, "error": null}
        });
        let report: SimulationRunReport = serde_json::from_value(base.clone()).unwrap();
        assert!(report.success());
        assert_eq!(serde_json::to_value(&report).unwrap(), base);
        let SimulationRunReport::V0 { cleanup, .. } = &report;
        assert_eq!(
            cleanup,
            &phoxal::artifact::simulation::SimulationCleanup {
                supervisor_stop_requested: true,
                supervisor_exited: true,
                supervisor_killed: false,
                error: None,
            }
        );
        for (field, value) in [
            ("supervisor_exited", serde_json::json!(false)),
            ("supervisor_killed", serde_json::json!(true)),
            ("error", serde_json::json!("supervisor exited 7")),
        ] {
            let mut invalid = base.clone();
            invalid["cleanup"][field] = value;
            let report: SimulationRunReport = serde_json::from_value(invalid).unwrap();
            assert!(
                !report.success(),
                "cleanup field {field} must refuse success"
            );
        }
        for field in ["scope", "supervisor_id", "run_id"] {
            let mut failure = base.clone();
            failure[field] = serde_json::Value::Null;
            failure["simulator_exit_code"] = serde_json::json!(1);
            failure["provider_contract_verified"] = serde_json::json!(false);
            let report: SimulationRunReport = serde_json::from_value(failure.clone()).unwrap();
            assert!(!report.success());
            assert_eq!(serde_json::to_value(report).unwrap(), failure);
        }
        // Synthetic producer-shaped evidence qualifies the typed consumer,
        // without claiming acceptance of the separately owned real producer.
        let rich_terminal = serde_json::json!({
            "schema": "phoxal/simulation-run/v0", "provider_contract_verified": true,
            "outcome": "success", "completed_steps": 2, "requested_steps": 2,
            "native_bindings": {"wheel": {"ports": ["actuator", "encoder"]}},
            "provenance": {"engine": {"sha256": "engine-hash"}},
            "presentation": {"mode": "headless"}, "model_identity": "model-hash"
        });
        let stdout = format!("{}\n", rich_terminal);
        let mut rich = base.clone();
        rich["terminal"] = rich_terminal;
        rich["simulator_stdout"] = serde_json::json!(stdout);
        let report: SimulationRunReport = serde_json::from_value(rich).unwrap();
        assert!(report.success());
        let SimulationRunReport::V0 {
            simulator_stdout,
            terminal,
            ..
        } = report;
        assert_eq!(simulator_stdout, stdout);
        assert!(matches!(
            terminal,
            Some(SimulatorTerminalEvidence::V0 {
                completed_steps: 2,
                requested_steps: 2,
                ..
            })
        ));
        let mut nonzero = base;
        nonzero["simulator_exit_code"] = serde_json::json!(1);
        let report: SimulationRunReport = serde_json::from_value(nonzero).unwrap();
        assert!(!report.success());
    }

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
        "response": "phoxal.component.actuator.v1.ActuatorCommand",
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
            "[package]\nname = \"freeze-proof-robot\"\nversion = \"0.1.0\"\nedition = \"2021\"\nbuild = \"build.rs\"\npublish = false\n",
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
                "schema: phoxal/robot/v0\nrobot:\n  id: freeze-proof-robot\n  model: model.xml\n  components:\n    d1:\n      source:\n        path: {}\n      mount_site: front_left_wheel_mount\n      driver:\n        config: {{ id: 1 }}\n        bindings:\n          actuator: brain.actuators\n  brain:\n    bindings:\n      encoders: d1.encoder\n  services: {{}}\nsupervisor:\n  source: {{ path: .fixture-supervisor }}\n  binary: phoxal-supervisor\n",
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
        let simulator = root.join("fake-simulator");
        // A staged fixture supervisor source: the robot's authored
        // selection acquires it through the ordinary Cargo install path
        // during preparation, and its binary embeds the pinned application
        // contract record for this host target.
        stage_fixture_supervisor(&root);

        // A counting Cargo wrapper: every real invocation is logged before
        // forwarding to the underlying cargo.
        let log = root.join("cargo-invocations.log");
        let real_cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let wrapper = root.join("cargo-wrapper");
        let source = wrapper.with_extension("rs");
        fs::write(
            &source,
            format!(
                "const LOG: &str = {log:?};\nconst REAL_CARGO: &str = {real_cargo:?};\n{}",
                include_str!("../../tests/fixtures/process/cargo_wrapper.rs")
            ),
        )
        .expect("write cargo wrapper");
        let output = std::process::Command::new("rustc")
            .args(["--edition=2024", "--crate-name", "cargo_wrapper"])
            .arg(&source)
            .arg("-o")
            .arg(&wrapper)
            .output()
            .expect("compile cargo wrapper");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        (guard, scene, simulator, wrapper)
    }

    #[test]
    fn native_fixture_staging_and_digest_preserve_special_paths() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().expect("isolated fixture");
        let executable = simulator_fixture(&root.path().join("probe"), None);
        for name in [
            "scene with spaces.xml",
            "scene\"quote.xml",
            r"scene\name.xml",
            "scene\nname.xml",
        ] {
            let source_dir = root.path().join(name);
            fs::create_dir(&source_dir).unwrap();
            let scene = source_dir.join(name);
            fs::write(&scene, b"test").unwrap();
            fs::write(source_dir.join("part.xml"), b"included").unwrap();
            let staged = source_dir.join("staged");
            let output = std::process::Command::new(&executable)
                .args(["stage-scene", "--scene"])
                .arg(&scene)
                .arg("--output")
                .arg(&staged)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(fs::read(staged.join(name)).unwrap(), b"test");
            assert_eq!(fs::read(staged.join("part.xml")).unwrap(), b"included");
            let output = std::process::Command::new(&executable)
                .arg("probe")
                .arg(staged.join(name))
                .output()
                .unwrap();
            assert!(output.status.success());
            let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                value["model_identity"],
                format!("{:x}", Sha256::digest(b"test"))
            );
        }
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
                "[package]\nname = \"phoxal-supervisor\"\nversion = \"0.0.0-dev.8\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nphoxal = {{ path = {:?}, default-features = false, features = [] }}\n\n[[bin]]\nname = \"phoxal-supervisor\"\npath = \"src/main.rs\"\n",
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
        let output = command
            .output()
            .unwrap_or_else(|error| panic!("generate the fixture supervisor lockfile: {error}"));
        assert!(
            output.status.success(),
            "fixture supervisor lockfile resolves offline:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        let wrapper = crate::project::simulation::simulator_fixture(&simulator, Some(&scene));
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
            .prepare(&options, &crate::project::NoPhases)
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
        let probe_output = snapshot._probe_directory.path().join("build");
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
            let mut ids: Vec<String> = snapshot.frozen.robot.inventory.keys().cloned().collect();
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

        // Per-case staging isolation: a second case captured from the same
        // frozen robot after the live-source mutations stages the mutated
        // scene independently, and neither its capture nor its publication
        // touches the first case's frozen staging or published bundle.
        let first_manifest_bytes =
            fs::read(bundle.root().join("manifest.json")).expect("read first published manifest");
        let live_scene_bytes = fs::read(&scene).expect("read mutated live scene");
        let live_part_bytes = fs::read(scene.parent().expect("scene parent").join("part.xml"))
            .expect("read mutated live include");
        let second_case = prepare_simulation_from_robot(&prepared, &robot, &options, &request)
            .expect("a second case captures from the same frozen robot");
        assert_ne!(
            second_case.frozen.scene, snapshot.frozen.scene,
            "each case stages its own scene copy"
        );
        let second_output = root.join("target/frozen-bundle-second");
        let second_bundle = bundle::finalize_simulation_bundle(
            &second_case.prepared,
            &second_case.frozen,
            &second_output,
            Some(&second_case.facts),
            None,
        )
        .expect("the second case finalizes from its own staging");
        let second_scene_relative = second_bundle.root().join(
            second_case
                .frozen
                .scene
                .strip_prefix(second_case.frozen.staging.path())
                .expect("second case scene inside its staging"),
        );
        assert_eq!(
            fs::read(&second_scene_relative).expect("second case published scene"),
            live_scene_bytes,
            "the second case freezes the current live scene bytes"
        );
        assert_eq!(
            fs::read(
                second_scene_relative
                    .parent()
                    .expect("scene root")
                    .join("part.xml")
            )
            .expect("second case published include"),
            live_part_bytes
        );
        assert_eq!(
            fs::read(&frozen_scene).expect("first case scene unchanged"),
            original_scene,
            "the second case never touches the first case's staging"
        );
        assert!(
            snapshot
                .frozen
                .staging
                .path()
                .join("bin/supervisor")
                .is_file(),
            "the first case's frozen staging stays complete after both publications"
        );
        assert_eq!(
            fs::read(bundle.root().join("manifest.json")).expect("reread first published manifest"),
            first_manifest_bytes,
            "publishing the second case leaves the first publication intact"
        );
        assert_eq!(
            invocation_count(&root.join("cargo-invocations.log")),
            after_finalize,
            "case capture and finalization add zero Cargo operations"
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
            model,
            instances,
            connections,
            ..
        } = manifest;
        assert!(instances.iter().any(|instance| instance.id == "d1"
            && instance.role == phoxal::artifact::bundle::InstanceRole::Driver));
        assert!(
            connections
                .iter()
                .any(|connection| connection.consumer.to_string() == "d1.actuator")
        );
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest as _;
        hasher.update(&original_scene);
        assert_eq!(
            snapshot.facts.model_identity,
            format!("{:x}", hasher.finalize()),
            "the command-owned facts name the frozen probed scene identity"
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
        .with_simulator_executable(crate::project::simulation::simulator_fixture(
            &simulator, None,
        ));
        let second = prepare_simulation(&project, &options, &request)
            .expect("a fresh preparation sees the mutated inputs");
        assert_ne!(
            second.facts.model_identity, facts.model_identity,
            "the second probe measures the mutated scene"
        );
    }
}
