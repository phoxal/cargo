//! Explicit command workflows.
//!
//! Every handler makes its responsibilities visible: discover the project,
//! prepare its selections, compile, assemble, publish, and report. Handlers
//! consume the normalized [`Invocation`] from [`crate::cli`] and delegate
//! presentation to [`crate::output`]; they never reparse arguments.

use std::path::Path;

use crate::cli::{Command, Invocation, SimulationInvocation};
use crate::feedback;
use crate::output;
use crate::project::{self, CargoOperation, CargoOptions, Error, Project};

/// The phase reporter of one command: the live progress handle when the
/// command owns feedback, or the silent reporter.
fn phases<'a>(progress: Option<&'a feedback::Progress<'a>>) -> &'a dyn project::PhaseReporter {
    match progress {
        Some(progress) => progress,
        None => &project::NoPhases,
    }
}

/// Runs one normalized command invocation.
pub(crate) fn run(
    invocation: Invocation,
    progress: Option<&feedback::Progress<'_>>,
) -> Result<(), Error> {
    let invocation_diagnostics_json = invocation.diagnostics_json;
    let Invocation {
        invocation: directory,
        files,
        command,
        diagnostics_json: _,
    } = invocation;
    let needs_operation_lock = match &command {
        Command::Config { .. } => false,
        Command::Simulation(simulation) => !simulation.uses_existing_build(),
        // The path query resolves without preparing, compiling, or
        // publishing: it takes no project admission and creates nothing.
        Command::Build {
            print_output_path: true,
            ..
        } => false,
        Command::Build { watch: true, .. } => false,
        _ => true,
    };
    let _operation_lock = if needs_operation_lock {
        Some(project::participant::operation_lock(
            project::ProjectLayout::discover(&directory)?.root(),
        )?)
    } else {
        None
    };
    match command {
        Command::Simulation(simulation) => run_simulation(
            simulation,
            &files,
            &directory,
            // The diagnostics policy was resolved with the invocation.
            invocation_diagnostics_json,
        ),
        Command::Prepare { options } => {
            let project = discover(&directory, &files)?;
            prepare(&project, &options, invocation_diagnostics_json, progress)
        }
        Command::Build {
            options,
            output,
            print_output_path: false,
            watch: true,
        } => project::watch::run(&directory, &files, &options, output),
        command => {
            let project = discover(&directory, &files)?;
            match command {
                Command::Config { json } => config(&project, json),
                Command::Check { options } => {
                    run_cargo_operation(&project, CargoOperation::Check, options, progress)
                }
                Command::Build {
                    options,
                    output,
                    print_output_path,
                    watch: _,
                } => build(&project, &options, output, print_output_path, progress),
                Command::Run { options } => run_local(&project, &options, progress),
                Command::Test { options } => {
                    run_cargo_operation(&project, CargoOperation::Test, options, progress)
                }
                Command::Scenario {
                    options,
                    file,
                    desktop,
                } => scenario(&project, &options, &file, desktop),
                Command::Simulation(_) | Command::Prepare { .. } => {
                    unreachable!("handled above")
                }
            }
        }
    }
}

/// Discovers the project from the invocation directory and selected files.
fn discover(directory: &std::path::Path, files: &[std::path::PathBuf]) -> Result<Project, Error> {
    Project::discover_files(directory, files)
}

/// Prepares selected contracts and generated APIs, reporting each change.
fn prepare(
    project: &Project,
    options: &CargoOptions,
    diagnostics_json: bool,
    progress: Option<&feedback::Progress<'_>>,
) -> Result<(), Error> {
    let changes = project.prepare_inputs(options, phases(progress))?;
    if let Some(progress) = progress {
        progress.suspend();
    }
    for change in changes {
        if diagnostics_json {
            eprintln!("prepared {change}");
        } else {
            feedback::line(&format!("Prepare: Updated {change}."));
        }
    }
    Ok(())
}

/// Prints the composed, authored-validated configuration.
fn config(project: &Project, json: bool) -> Result<(), Error> {
    let output = if json {
        serde_json::to_string_pretty(project.document()).map_err(|error| {
            Error::DeclarationCheck {
                message: error.to_string(),
            }
        })?
    } else {
        serde_yaml::to_string(project.document()).map_err(|source| Error::ParseRobot {
            path: "resolved config".into(),
            source,
        })?
    };
    println!("{output}");
    Ok(())
}

/// Prepares, validates, and Cargo-checks the selected robot code.
fn run_cargo_operation(
    project: &Project,
    operation: CargoOperation,
    options: CargoOptions,
    progress: Option<&feedback::Progress<'_>>,
) -> Result<(), Error> {
    let diagnostics_json = options.diagnostics_json();
    let mut preparation_options = options.clone();
    if matches!(operation, CargoOperation::Test) {
        // Cargo's test filter and test-target selectors choose Rust tests;
        // preparation still follows the complete selected robot graph.
        preparation_options.cargo_args.clear();
        preparation_options.test_args.clear();
    }
    let prepared = project.prepare(&preparation_options, phases(progress))?;
    let outputs = match operation {
        CargoOperation::Check => {
            // Declaration validation emits permanent stderr before the final
            // captured Cargo output. Yield the terminal before that boundary.
            if let Some(progress) = progress {
                progress.suspend();
            }
            phases(progress).phase("checking the robot project");
            prepared.check(&options)?
        }
        CargoOperation::Test | CargoOperation::Build => {
            let phase = match operation {
                // Compile-only test requests keep their compile-only
                // meaning: Cargo builds the test executables and runs
                // nothing.
                CargoOperation::Test if options.no_run => "building robot tests",
                CargoOperation::Test => "testing the robot project",
                CargoOperation::Build => "building the robot project",
                CargoOperation::Check => unreachable!("handled above"),
            };
            phases(progress).phase(phase);
            prepared.run(operation, &options)?
        }
    };
    if let Some(progress) = progress {
        progress.suspend();
    }
    // JSON mode forwarded each compilation's records at the owning Cargo
    // boundary already; printing them again here would duplicate them.
    if !diagnostics_json {
        for output in outputs {
            output::bytes(&output.stdout, false);
            output::bytes(&output.stderr, true);
        }
    }
    if diagnostics_json {
        eprintln!("cargo phoxal: {} completed", operation.as_str());
    }
    Ok(())
}

/// Resolves the publication slot path without preparing, compiling,
/// acquiring, publishing, or creating any artifact.
///
/// The one resolver shared with the real build keys the complete ordered
/// robot selection and effective build settings; success prints exactly one
/// absolute path and a newline on stdout.
fn print_resolved_output_path(
    project: &Project,
    options: &CargoOptions,
    output: Option<&std::path::Path>,
) -> Result<(), Error> {
    // The query resolves the same settings the build resolves: the target
    // directory in the robot-root context and the effective toolchain,
    // without preparing, acquiring, or creating anything. An explicit
    // output still validates those settings before printing its path.
    let target_directory = project.resolve_target_directory(options)?;
    let toolchain = project.resolve_toolchain(options)?;
    if let Some(output) = output {
        println!("{}", output.display());
        return Ok(());
    }
    let stable = crate::project::slot_path(
        &target_directory,
        &project.robot_id(),
        project.robot_files(),
        options,
        &toolchain,
    );
    println!("{}", stable.display());
    Ok(())
}

/// Discovers, prepares, compiles, assembles, and publishes the complete
/// runnable directory, or resolves and prints its publication path.
fn build(
    project: &Project,
    options: &CargoOptions,
    output: Option<std::path::PathBuf>,
    print_output_path: bool,
    progress: Option<&feedback::Progress<'_>>,
) -> Result<(), Error> {
    if print_output_path {
        return print_resolved_output_path(project, options, output.as_deref());
    }
    let diagnostics_json = options.diagnostics_json();
    let prepared = project.prepare(options, phases(progress))?;
    let runnable = match output {
        Some(output) => output,
        None => prepared.default_bundle_path(options)?,
    };
    let bundle = prepared.build_bundle(options, runnable.clone(), phases(progress))?;
    if let Some(progress) = progress {
        progress.suspend();
    }
    // The typed outcome reports once; the generic completion footer stays
    // silent for build.
    let outcome = match bundle.publication() {
        crate::project::publish::PublicationOutcome::Published { .. } => {
            format!("Build published: {}", runnable.display())
        }
        crate::project::publish::PublicationOutcome::Unchanged { .. } => {
            format!("Build unchanged: {}", runnable.display())
        }
    };
    output::status(diagnostics_json, &outcome);
    Ok(())
}

/// Discovers, prepares, compiles, assembles, publishes, and runs the robot
/// on hardware.
fn run_local(
    project: &Project,
    options: &CargoOptions,
    progress: Option<&feedback::Progress<'_>>,
) -> Result<(), Error> {
    let diagnostics_json = options.diagnostics_json();
    let prepared = project.prepare(options, phases(progress))?;
    let output = prepared.default_bundle_path(options)?;
    let bundle = prepared.run_local(options, output, phases(progress))?;
    output::status(
        diagnostics_json,
        &format!("compiled bundle: {}", bundle.root().display()),
    );
    Ok(())
}

/// Compiles and executes one standalone scenario through its command-owned
/// host, forwarding the scenario's raw output.
fn scenario(
    project: &Project,
    options: &CargoOptions,
    file: &Path,
    desktop: bool,
) -> Result<(), Error> {
    let prepared = project.prepare(options, &project::NoPhases)?;
    let output =
        crate::project::scenario::fixture_host::run_scenario(&prepared, options, file, desktop)?;
    output::bytes(&output.stdout, false);
    output::bytes(&output.stderr, true);
    Ok(())
}

/// Builds the robot and delegates native simulation, or delegates directly
/// to an existing runnable build.
///
/// The final simulator launch uses a raw Unix `exec`: the Phoxal process is
/// replaced, and the simulator's bytes, signals, and exit status pass
/// through unchanged.
fn run_simulation(
    simulation: SimulationInvocation,
    files: &[std::path::PathBuf],
    invocation_directory: &Path,
    diagnostics_json: bool,
) -> Result<(), Error> {
    use std::os::unix::process::CommandExt as _;
    let (build, scene) = match simulation.build {
        Some(build) => (build, simulation.scene.clone()),
        None => {
            // Compile and assemble first; the simulator is selected after
            // the bundle exists, exactly like every public launch.
            let project = Project::discover_files(invocation_directory, files)?;
            let (bundle, scene) = if diagnostics_json {
                project.build_simulation(&simulation.options, &simulation.scene, None)?
            } else {
                feedback::run("Simulation", false, |_| {
                    project.build_simulation(&simulation.options, &simulation.scene, None)
                })?
            };
            (bundle.root().to_owned(), scene)
        }
    };
    let executable = project::selected_simulator_executable(None)?;
    let mut command = std::process::Command::new(executable);
    command.arg("run").arg(scene).arg("--build").arg(build);
    if simulation.headless {
        command.arg("--headless");
    }
    if let Some(duration) = simulation.duration {
        command
            .arg("--duration")
            .arg(humantime::format_duration(duration).to_string());
    }
    if simulation.paused {
        command.arg("--paused");
    }
    let error = command.exec();
    Err(project::Error::SimulationInvalid {
        message: format!(
            "cannot start the selected Phoxal Simulator executable: {error}; check PHOXAL_SIMULATOR/PATH and reinstall the complete application using https://github.com/phoxal/simulator#readme"
        ),
    })
}
