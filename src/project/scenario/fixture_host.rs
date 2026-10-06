//! Command-scoped run host for standalone scenario executables.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use phoxal::scenario::fixture_protocol::{
    self, ClientMessage, HostMessage, PROTOCOL_VERSION, RunFailure,
};

use crate::project::cargo::{CargoOperation, CargoOptions, CargoOutput};
use crate::project::{Error, PreparedProject};

const MAX_ACTIVE_RUNS: usize = 1;

#[derive(Clone, Debug, Default)]
pub(crate) struct TestHostOptions {
    pub(crate) simulator: Option<PathBuf>,
    pub(crate) headless: bool,
}

/// Compile a declared scenario target, then run it against a command-owned host.
pub(crate) fn run_scenario(
    prepared: &PreparedProject,
    options: &CargoOptions,
    file: &Path,
    desktop: bool,
) -> Result<CargoOutput, Error> {
    let root = prepared.cargo_workdir();
    let file = root
        .join(file)
        .canonicalize()
        .map_err(|source| Error::ArtifactFile {
            path: file.to_owned(),
            source,
        })?;
    if !file.starts_with(root.join("scenarios"))
        || file.extension().is_none_or(|extension| extension != "rs")
    {
        return Err(Error::InvalidOptions {
            message: "scenario must be a Rust file under the robot's scenarios/ directory".into(),
        });
    }
    let target = prepared
        .cargo_root_package()
        .targets
        .iter()
        .find(|target| target.src_path.as_std_path().canonicalize().ok().as_ref() == Some(&file))
        .ok_or_else(|| Error::InvalidOptions {
            message:
                "declare the scenario as a Cargo [[example]] or [[bin]] with path and test=false"
                    .into(),
        })?;
    if target.test || (!target.is_example() && !target.is_bin()) {
        return Err(Error::InvalidOptions {
            message: "scenario target must be an example or binary with test=false".into(),
        });
    }
    let mut compilation = options.clone();
    compilation.message_format = Some("json".into());
    compilation.selection = crate::project::CargoSelection::default();
    if target.is_example() {
        compilation
            .selection
            .examples_named
            .push(target.name.clone());
    } else {
        compilation.selection.binaries.push(target.name.clone());
    }
    let outputs = match prepared.run(CargoOperation::Build, &compilation) {
        Ok(outputs) => outputs,
        Err(Error::CargoCommand {
            operation,
            status,
            stdout,
            stderr,
        }) => {
            forward_compilation(options, stdout.as_bytes(), stderr.as_bytes());
            return Err(Error::CargoCommand {
                operation,
                status,
                stdout: String::new(),
                stderr: String::new(),
            });
        }
        Err(error) => return Err(error),
    };
    for output in &outputs {
        forward_compilation(options, &output.stdout, &output.stderr);
    }
    let executable = outputs
        .iter()
        .flat_map(|output| output.stdout.split(|byte| *byte == b'\n'))
        .filter_map(|line| serde_json::from_slice::<cargo_metadata::Message>(line).ok())
        .find_map(|message| match message {
            cargo_metadata::Message::CompilerArtifact(artifact)
                if artifact.target.name == target.name
                    && artifact.package_id == prepared.cargo_root_package().id =>
            {
                artifact.executable
            }
            _ => None,
        })
        .ok_or_else(|| Error::InvalidOptions {
            message: "Cargo did not emit the selected scenario executable".into(),
        })?;
    let host = TestHostOptions {
        simulator: None,
        headless: !desktop,
    };
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
        let environment = [
            (
                OsString::from(fixture_protocol::ENV_ENDPOINT),
                endpoint.clone().into_os_string(),
            ),
            (
                OsString::from(fixture_protocol::ENV_PROJECT_ROOT),
                project_root.clone().into_os_string(),
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
            let cargo = Command::new(executable.as_std_path())
                .current_dir(&project_root)
                .envs(environment.iter().cloned())
                .output()
                .map_err(|source| Error::CargoSpawn {
                    operation: "scenario".into(),
                    source,
                });
            stop.store(true, Ordering::Release);
            let _ = std::os::unix::net::UnixStream::connect(&endpoint);
            let host = server.join().map_err(|_| Error::SimulationInvalid {
                message: "cargo phoxal scenario run host panicked".to_owned(),
            })?;
            match (cargo, host) {
                (Ok(output), Ok(())) if output.status.success() => Ok(CargoOutput {
                    stdout: output.stdout,
                    stderr: output.stderr,
                }),
                (Ok(_), Err(error)) => Err(error),
                (Ok(output), Ok(())) => Err(Error::ScenarioFailed {
                    message: format!(
                        "scenario exited {}\n{}\n{}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ),
                }),
                (Err(error), _) => Err(error),
            }
        });
        drop(directory);
        result
    }
}

// Internal JSON identifies the executable; it must not hide compiler diagnostics.
fn forward_compilation(options: &CargoOptions, stdout: &[u8], stderr: &[u8]) {
    let output = compilation_diagnostics(
        stdout,
        stderr,
        options
            .message_format
            .as_deref()
            .is_some_and(|format| format.starts_with("json")),
    );
    crate::print_bytes(&output.stdout, false);
    crate::print_bytes(&output.stderr, true);
}

fn compilation_diagnostics(stdout: &[u8], stderr: &[u8], json: bool) -> CargoOutput {
    if json {
        return CargoOutput {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        };
    }
    let mut rendered = Vec::new();
    for line in stdout.split_inclusive(|byte| *byte == b'\n') {
        match serde_json::from_slice::<cargo_metadata::Message>(line) {
            Ok(cargo_metadata::Message::CompilerMessage(message)) => {
                if let Some(diagnostic) = message.message.rendered {
                    rendered.extend_from_slice(diagnostic.as_bytes());
                }
            }
            Ok(cargo_metadata::Message::TextLine(text)) => {
                rendered.extend_from_slice(text.as_bytes());
                rendered.push(b'\n');
            }
            Ok(_) => {}
            Err(_) => rendered.extend_from_slice(line),
        }
    }
    rendered.extend_from_slice(stderr);
    CargoOutput {
        stdout: Vec::new(),
        stderr: rendered,
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
            eprintln!(
                "cargo phoxal: scenario execution finished: simulator exit {simulator_exit_code:?}, provider verified {provider_contract_verified}, supervisor exited {}, forced kill {}, cleanup {}",
                cleanup.supervisor_exited,
                cleanup.supervisor_killed,
                cleanup.error.as_deref().unwrap_or("complete"),
            );
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

#[cfg(test)]
mod compiler_diagnostic_tests {
    use super::compilation_diagnostics;

    #[test]
    fn actual_cargo_scenario_errors_and_warnings_are_forwarded() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("scenarios")).unwrap();
        std::fs::write(directory.path().join("Cargo.toml"),
            "[package]\nname='diagnostic-fixture'\nversion='0.1.0'\nedition='2024'\n[[example]]\nname='scenario'\npath='scenarios/check.rs'\ntest=false\n").unwrap();
        for (source, success, diagnostic) in [
            (
                "fn main() { absent_function(); }",
                false,
                "cannot find function",
            ),
            (
                "fn main() { let unused_value = 1; }",
                true,
                "unused variable",
            ),
        ] {
            std::fs::write(directory.path().join("scenarios/check.rs"), source).unwrap();
            let output = std::process::Command::new(env!("CARGO"))
                .current_dir(directory.path())
                // This fixture checks successful warning forwarding even when
                // the parent repository's CI denies warnings.
                .env("RUSTFLAGS", "")
                .env_remove("CARGO_ENCODED_RUSTFLAGS")
                .args([
                    "build",
                    "--offline",
                    "--example",
                    "scenario",
                    "--message-format=json",
                ])
                .output()
                .unwrap();
            assert_eq!(
                output.status.success(),
                success,
                "Cargo fixture status {}:\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let human = compilation_diagnostics(&output.stdout, &output.stderr, false);
            let rendered = String::from_utf8(human.stderr).unwrap();
            assert!(rendered.contains(diagnostic), "{rendered}");
            assert!(!rendered.contains("\"reason\":\"compiler-artifact\""));
            let json = compilation_diagnostics(&output.stdout, &output.stderr, true);
            assert_eq!(json.stdout, output.stdout);
            assert!(
                String::from_utf8(json.stdout)
                    .unwrap()
                    .contains("\"reason\":\"compiler-message\"")
            );
        }
    }

    #[test]
    fn compiler_records_remain_json_or_render_as_human_diagnostics() {
        let stdout = br#"{"reason":"compiler-message","package_id":"path+file:///robot#0.1.0","manifest_path":"/robot/Cargo.toml","target":{"kind":["example"],"crate_types":["bin"],"name":"scenario","src_path":"/robot/scenarios/scenario.rs","edition":"2024","doc":false,"doctest":false,"test":false},"message":{"message":"bad scenario","code":null,"level":"error","spans":[],"children":[],"rendered":"error: bad scenario\n"}}
{"reason":"build-finished","success":false}
"#;
        let human = compilation_diagnostics(stdout, b"cargo failed\n", false);
        assert!(human.stdout.is_empty());
        assert_eq!(human.stderr, b"error: bad scenario\ncargo failed\n");
        let json = compilation_diagnostics(stdout, b"cargo failed\n", true);
        assert_eq!(json.stdout, stdout);
        assert_eq!(json.stderr, b"cargo failed\n");
    }
}
