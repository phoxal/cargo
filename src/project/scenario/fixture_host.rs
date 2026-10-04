//! Command-scoped run host for function-based simulation tests.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use phoxal::scenario::fixture_protocol::{
    self, ClientMessage, HostMessage, PROTOCOL_VERSION, RunFailure,
};

use crate::project::cargo::{CargoOperation, CargoOptions, CargoOutput};
use crate::project::{Error, PreparedProject, Project};

const MAX_ACTIVE_RUNS: usize = 1;

#[derive(Clone, Debug, Default)]
pub(crate) struct TestHostOptions {
    pub(crate) simulator: Option<PathBuf>,
    pub(crate) headless: bool,
}

/// Run Cargo's native test command while serving any fixture calls made by
/// the selected test binaries.
pub(crate) fn run_tests(
    _project: &Project,
    prepared: &PreparedProject,
    options: &CargoOptions,
    host: &TestHostOptions,
) -> Result<Vec<CargoOutput>, Error> {
    {
        let directory = tempfile::Builder::new()
            .prefix("phoxal-test-")
            .tempdir_in("/tmp")
            .map_err(|source| Error::ArtifactFile {
                path: std::env::temp_dir(),
                source,
            })?;
        let endpoint = directory.path().join("fixture.sock");
        let listener = std::os::unix::net::UnixListener::bind(&endpoint).map_err(|source| {
            Error::ArtifactFile {
                path: endpoint.clone(),
                source,
            }
        })?;
        listener
            .set_nonblocking(true)
            .map_err(|source| Error::ArtifactFile {
                path: endpoint.clone(),
                source,
            })?;

        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let mut host_options = options.clone();
        // Cargo's test filter and test-target selectors choose Rust tests.
        // Bundle preparation still follows the complete selected robot graph.
        host_options.cargo_args.clear();
        host_options.test_args.clear();
        host_options.selection = crate::project::CargoSelection::default();
        // Cargo test owns its build-directory lock until its test processes exit.
        // Capture all robot builds and source assets before starting that process,
        // then each case adds its own native scene/probe without nested Cargo.
        let robot = crate::project::bundle::freeze_robot_closure(prepared, &host_options)?;
        let prepared = prepared.clone();
        let fixture_options = host.clone();
        let project_root = prepared.cargo_workdir().to_owned();
        let environment = vec![
            (
                OsString::from(fixture_protocol::ENV_ENDPOINT),
                endpoint.clone().into_os_string(),
            ),
            (
                OsString::from(fixture_protocol::ENV_PROJECT_ROOT),
                project_root.clone().into_os_string(),
            ),
            (
                OsString::from(fixture_protocol::ENV_SCENE),
                OsString::from("simulation/scene.xml"),
            ),
        ];

        let result = std::thread::scope(|scope| {
            let server = scope.spawn(|| {
                serve(
                    listener,
                    &prepared,
                    &robot,
                    &host_options,
                    &fixture_options,
                    &project_root,
                    server_stop,
                )
            });
            let cargo = crate::project::cargo::run_with_env(
                &prepared,
                CargoOperation::Test,
                options,
                &environment,
            );
            stop.store(true, Ordering::Release);
            let _ = std::os::unix::net::UnixStream::connect(&endpoint);
            let host = server.join().map_err(|_| Error::SimulationInvalid {
                message: "cargo phoxal test run host panicked".to_owned(),
            })?;
            match (cargo, host) {
                (Ok(outputs), Ok(())) => Ok(outputs),
                (Err(error), _) => Err(error),
                (Ok(_), Err(error)) => Err(error),
            }
        });
        drop(directory);
        result
    }
}

fn serve(
    listener: std::os::unix::net::UnixListener,
    prepared: &PreparedProject,
    robot: &crate::project::bundle::FrozenRobot,
    options: &CargoOptions,
    host: &TestHostOptions,
    project_root: &Path,
    stop: Arc<AtomicBool>,
) -> Result<(), Error> {
    debug_assert_eq!(
        MAX_ACTIVE_RUNS, 1,
        "the command-scoped fixture host deliberately serves one finite run at a time"
    );
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|source| Error::SimulationInvalid {
                        message: format!("cannot configure fixture client blocking mode: {source}"),
                    })?;
                stream
                    .set_read_timeout(Some(Duration::from_secs(180)))
                    .map_err(|source| Error::SimulationInvalid {
                        message: format!("cannot configure fixture client read timeout: {source}"),
                    })?;
                stream
                    .set_write_timeout(Some(Duration::from_secs(180)))
                    .map_err(|source| Error::SimulationInvalid {
                        message: format!("cannot configure fixture client write timeout: {source}"),
                    })?;
                if let Err(error) =
                    serve_request(prepared, robot, options, host, project_root, &mut stream)
                {
                    eprintln!("cargo-phoxal: simulation fixture request failed: {error:#}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(Error::SimulationInvalid {
                    message: format!("fixture host accept failed: {error}"),
                });
            }
        }
    }
    Ok(())
}

fn serve_request(
    prepared: &PreparedProject,
    robot: &crate::project::bundle::FrozenRobot,
    options: &CargoOptions,
    host: &TestHostOptions,
    project_root: &Path,
    stream: &mut std::os::unix::net::UnixStream,
) -> phoxal::Result<()> {
    let open: ClientMessage = fixture_protocol::read_message(stream)?;
    let ClientMessage::Open {
        version,
        request_id,
        test_identity,
        scene,
    } = open
    else {
        return Err(phoxal::anyhow!("expected fixture open request"));
    };
    if version != PROTOCOL_VERSION {
        return send_failure(
            stream,
            &request_id,
            "protocol",
            format!("unsupported fixture protocol version {version}; expected {PROTOCOL_VERSION}"),
        );
    }
    let scene = match resolve_scene(project_root, &scene) {
        Ok(scene) => scene,
        Err(error) => return send_failure(stream, &request_id, "scene", error.to_string()),
    };
    let probe_request = match super::run_host::build_probe_request(
        &test_identity,
        scene.clone(),
        host.simulator.as_deref(),
    ) {
        Ok(request) => request,
        Err(error) => {
            return send_failure(stream, &request_id, "probe_request", error.to_string());
        }
    };
    let snapshot = match crate::project::simulation::prepare_simulation_from_robot(
        prepared,
        robot,
        options,
        &probe_request,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return send_failure(stream, &request_id, "provision_or_probe", error.to_string());
        }
    };
    fixture_protocol::write_message(
        stream,
        &HostMessage::Probe {
            version: PROTOCOL_VERSION,
            request_id: request_id.clone(),
            quantum_ns: snapshot.facts.quantum_ns,
            model_identity: snapshot.facts.model_identity.clone(),
        },
    )?;

    let execute: ClientMessage = fixture_protocol::read_message(stream)?;
    let ClientMessage::Execute {
        request_id: execute_id,
        program,
    } = execute
    else {
        return send_failure(stream, &request_id, "protocol", "expected execute request");
    };
    if execute_id != request_id {
        return send_failure(
            stream,
            &request_id,
            "protocol",
            format!("execute request id `{execute_id}` does not match open request"),
        );
    }
    let program = match phoxal::scenario::plan_support::Program::decode(&program) {
        Ok(program) => program,
        Err(error) => return send_failure(stream, &request_id, "program", error.to_string()),
    };
    if program.scenario_name() != test_identity {
        return send_failure(
            stream,
            &request_id,
            "program",
            "program test identity does not match the open request",
        );
    }
    let facts = snapshot.facts.clone();
    let lifecycle = super::run_host::drive_lifecycle(
        &test_identity,
        snapshot,
        &program,
        host.simulator.as_deref(),
        host.headless,
    );
    match lifecycle {
        Ok(report) => {
            let crate::project::SimulationRunReport::V0 {
                simulator_exit_code,
                cleanup,
                supervisor_ready,
                provider_contract_verified,
                simulator_stdout,
                simulator_stderr,
                ..
            } = &report;
            let lifecycle_passing = report.success();
            if !lifecycle_passing {
                return send_failure(
                    stream,
                    &request_id,
                    "lifecycle",
                    format!(
                        "simulator exit {simulator_exit_code:?}, supervisor ready {supervisor_ready}, provider contract verified {provider_contract_verified}, cleanup {:?}; simulator stdout: {}; simulator stderr: {}",
                        cleanup,
                        simulator_stdout.chars().take(4_000).collect::<String>(),
                        simulator_stderr.chars().take(4_000).collect::<String>(),
                    ),
                );
            }
            let evidence = super::run_host::build_lifecycle_report(&facts, &program, &report);
            fixture_protocol::write_message(
                stream,
                &HostMessage::Completed {
                    request_id,
                    report: evidence,
                    cleanup_succeeded: cleanup.error.is_none()
                        && cleanup.supervisor_exited
                        && !cleanup.supervisor_killed,
                    lifecycle_passing,
                },
            )
        }
        Err(error) => send_failure(stream, &request_id, "execution", error.to_string()),
    }
}

fn resolve_scene(project_root: &Path, scene: &Path) -> phoxal::Result<PathBuf> {
    let joined = if scene.is_absolute() {
        scene.to_owned()
    } else {
        project_root.join(scene)
    };
    joined.canonicalize().map_err(|error| {
        phoxal::anyhow!(
            "cannot resolve simulation scene {}: {error}",
            joined.display()
        )
    })
}

fn send_failure(
    stream: &mut std::os::unix::net::UnixStream,
    request_id: &str,
    phase: impl Into<String>,
    cause: impl Into<String>,
) -> phoxal::Result<()> {
    fixture_protocol::write_message(
        stream,
        &HostMessage::Failed {
            request_id: request_id.to_owned(),
            failure: RunFailure {
                phase: phase.into(),
                cause: cause.into(),
                cleanup: "no admitted execution remains owned by this request".to_owned(),
                evidence: None,
            },
        },
    )
}
