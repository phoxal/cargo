#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

mod project;

use project::{
    CargoOperation, CargoOptions, CargoSelection, LockMode, PreparedProject, Project,
    SelectedTarget,
};

fn main() -> ExitCode {
    let arguments = cargo_arguments(std::env::args_os());
    let cli = Cli::parse_from(arguments);
    let json_diagnostics = cli.json_diagnostics();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error, json_diagnostics);
            ExitCode::FAILURE
        }
    }
}

// Cargo external subcommands receive their own name as argv[1].
fn cargo_arguments(arguments: impl IntoIterator<Item = OsString>) -> Vec<OsString> {
    let mut arguments: Vec<_> = arguments.into_iter().collect();
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "phoxal")
    {
        arguments.remove(1);
    }
    // Clap propagates a global Vec from one level only. Lift file selections
    // before parsing so occurrences around the subcommand retain total order.
    let mut lifted = Vec::new();
    let mut retained = Vec::new();
    let mut iterator = arguments.into_iter();
    let Some(program) = iterator.next() else {
        return Vec::new();
    };
    let mut tail = false;
    while let Some(argument) = iterator.next() {
        if argument == "--" {
            tail = true;
        }
        if !tail && (argument == "-f" || argument == "--file") {
            lifted.push(argument);
            if let Some(value) = iterator.next() {
                lifted.push(value);
            }
        } else if !tail
            && argument.to_str().is_some_and(|value| {
                value.starts_with("--file=")
                    || (value.starts_with("-f") && value.len() > 2 && !value.starts_with("--"))
            })
        {
            lifted.push(argument);
        } else {
            retained.push(argument);
        }
    }
    std::iter::once(program)
        .chain(lifted)
        .chain(retained)
        .collect()
}

fn explicit_path(invocation: &std::path::Path, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        invocation.join(path)
    }
}

fn run(mut cli: Cli) -> Result<(), crate::project::Error> {
    let invocation = std::env::current_dir().map_err(|source| project::Error::ArtifactFile {
        path: ".".into(),
        source,
    })?;
    cli.normalize_executable_paths(&invocation);
    let files: Vec<_> = cli
        .files
        .iter()
        .map(|file| explicit_path(&invocation, file))
        .collect();
    if matches!(&cli.command, Command::Simulation(arguments) if arguments.build.is_some())
        && !files.is_empty()
    {
        return Err(project::Error::DeclarationCheck {
            message: "simulation --build cannot be combined with --file".into(),
        });
    }
    let _operation_lock = if matches!(&cli.command, Command::Simulation(arguments) if arguments.build.is_some())
        || matches!(&cli.command, Command::Config(_))
    {
        None
    } else {
        Some(project::participant::operation_lock(
            project::ProjectLayout::discover(&invocation)?.root(),
        )?)
    };
    let command = cli.command;
    match command {
        Command::Simulation(arguments) => run_simulation(arguments, &files),
        Command::Prepare(arguments) => {
            let options = arguments.options.into_options(Vec::new(), Vec::new());
            let project = Project::discover_files(&invocation, &files)?;
            let changes = project.prepare_inputs(&options)?;
            for change in changes {
                eprintln!("prepared {change}");
            }
            Ok(())
        }
        command => {
            let project = Project::discover_files(&invocation, &files)?;
            match command {
                Command::Config(arguments) => {
                    let output = if arguments.json {
                        serde_json::to_string_pretty(project.document()).map_err(|error| {
                            project::Error::DeclarationCheck {
                                message: error.to_string(),
                            }
                        })?
                    } else {
                        serde_yaml::to_string(project.document()).map_err(|source| {
                            project::Error::ParseRobot {
                                path: "resolved config".into(),
                                source,
                            }
                        })?
                    };
                    println!("{output}");
                    Ok(())
                }
                Command::Check(arguments) => run_cargo(
                    &project,
                    CargoOperation::Check,
                    arguments.into_options(Vec::new()),
                ),
                Command::Build(arguments) => {
                    let invocation =
                        std::env::current_dir().map_err(|source| project::Error::ArtifactFile {
                            path: ".".into(),
                            source,
                        })?;
                    let output = arguments
                        .output
                        .as_ref()
                        .map(|path| explicit_path(&invocation, path));
                    let options = arguments.into_options();
                    let prepared = project.prepare(&options)?;
                    let runnable = prepared.default_bundle_path(&options);
                    let bundle = prepared.build_bundle(&options, runnable)?;
                    let project::RobotDocument::V0 { robot, .. } = prepared.document();
                    let archive = output.unwrap_or_else(|| {
                        prepared
                            .layout()
                            .root()
                            .join("bundle")
                            .join(format!("{}.zip", robot.id))
                    });
                    bundle.archive(&archive)?;
                    print_status(
                        &options,
                        &format!("runnable build: {}", bundle.root().display()),
                    );
                    print_status(&options, &format!("robot archive: {}", archive.display()));
                    Ok(())
                }
                Command::Run(arguments) => {
                    let options = arguments.into_options();
                    let prepared = project.prepare(&options)?;
                    let output = prepared.default_bundle_path(&options);
                    let bundle = prepared.run_local(&options, output)?;
                    print_status(
                        &options,
                        &format!("compiled bundle: {}", bundle.root().display()),
                    );
                    Ok(())
                }
                Command::Test(arguments) => run_test(&project, arguments),
                Command::Scenario(arguments) => {
                    let mut options = arguments.options.into_options(Vec::new(), Vec::new());
                    options.release = arguments.release;
                    let invocation =
                        std::env::current_dir().map_err(|source| project::Error::ArtifactFile {
                            path: ".".into(),
                            source,
                        })?;
                    let file = explicit_path(&invocation, &arguments.file);
                    let prepared = project.prepare(&options)?;
                    let output = project::scenario::fixture_host::run_scenario(
                        &prepared,
                        &options,
                        &file,
                        arguments.desktop,
                    )?;
                    print_bytes(&output.stdout, false);
                    print_bytes(&output.stderr, true);
                    Ok(())
                }
                Command::Simulation(_) => unreachable!("simulation was handled above"),
                Command::Prepare(_) => unreachable!("prepare was handled above"),
            }
        }
    }
}

fn run_cargo(
    project: &Project,
    operation: CargoOperation,
    options: CargoOptions,
) -> Result<(), crate::project::Error> {
    let json = json_requested(&options);
    let mut preparation_options = options.clone();
    if matches!(operation, CargoOperation::Test) {
        preparation_options.cargo_args.clear();
        preparation_options.test_args.clear();
    }
    let prepared = project.prepare(&preparation_options)?;
    let outputs = match operation {
        CargoOperation::Check => prepared.check(&options)?,
        CargoOperation::Test | CargoOperation::Build => prepared.run(operation, &options)?,
    };
    for output in outputs {
        print_bytes(&output.stdout, false);
        print_bytes(&output.stderr, true);
    }
    if json {
        eprintln!("cargo phoxal: {} completed", operation.as_str());
    }
    Ok(())
}

fn run_test(project: &Project, arguments: TestArgs) -> Result<(), crate::project::Error> {
    let TestArgs {
        options,
        filter,
        no_run,
        test_args,
    } = arguments;
    let mut cargo_args = Vec::new();
    if let Some(filter) = filter {
        cargo_args.push(filter);
    }
    if no_run {
        cargo_args.push(OsString::from("--no-run"));
    }
    run_cargo(
        project,
        CargoOperation::Test,
        options.into_options(cargo_args, test_args),
    )
}

fn print_status(options: &CargoOptions, message: &str) {
    if json_requested(options) {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

fn json_requested(options: &CargoOptions) -> bool {
    json_format(options.message_format.as_deref(), &options.cargo_args)
}

fn json_common(options: &CommonArgs, cargo_args: &[OsString]) -> bool {
    json_format(options.build.message_format.as_deref(), cargo_args)
}

fn json_format(explicit: Option<&str>, arguments: &[OsString]) -> bool {
    if explicit.is_some_and(|format| format.starts_with("json")) {
        return true;
    }
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let argument = argument.to_string_lossy();
        if let Some(format) = argument.strip_prefix("--message-format=") {
            return format.starts_with("json");
        }
        if argument == "--message-format"
            && arguments
                .next()
                .is_some_and(|format| format.to_string_lossy().starts_with("json"))
        {
            return true;
        }
    }
    false
}

fn print_error(error: &crate::project::Error, json: bool) {
    if !json {
        eprintln!("error: {error:#}");
        return;
    }
    let span = diagnostic_path(error).map(|path| {
        serde_json::json!({
            "file_name": path,
            "byte_start": 0,
            "byte_end": 0,
            "line_start": 1,
            "line_end": 1,
            "column_start": 1,
            "column_end": 1,
            "is_primary": true,
            "label": "Phoxal project diagnostic"
        })
    });
    let diagnostic = serde_json::json!({
        "reason": "phoxal-diagnostic",
        "message": format!("{error:#}"),
        "level": "error",
        "spans": span.into_iter().collect::<Vec<_>>(),
        "children": [],
        "rendered": format!("error: {error:#}\n")
    });
    eprintln!("{diagnostic}");
}

fn diagnostic_path(error: &crate::project::Error) -> Option<PathBuf> {
    match error {
        crate::project::Error::Discovery(error) => match error {
            crate::project::DiscoveryError::Resolve { path, .. }
            | crate::project::DiscoveryError::MissingRobot { start: path }
            | crate::project::DiscoveryError::MissingManifest { root: path } => Some(path.clone()),
        },
        crate::project::Error::DeclarationCheck { .. } => None,
        crate::project::Error::ContractPreparation { .. } => None,
        crate::project::Error::ReadRobot { path, .. }
        | crate::project::Error::ParseRobot { path, .. }
        | crate::project::Error::InvalidRobot { path, .. }
        | crate::project::Error::ReadManifest { path, .. }
        | crate::project::Error::ParseManifest { path, .. }
        | crate::project::Error::VirtualManifest { path }
        | crate::project::Error::ManifestPreparation { path, .. }
        | crate::project::Error::CargoMetadata { manifest: path, .. } => Some(path.clone()),
        crate::project::Error::ConfigurationInvalid { .. }
        | crate::project::Error::Source(_)
        | crate::project::Error::CargoCommand { .. }
        | crate::project::Error::CargoSpawn { .. }
        | crate::project::Error::InvalidOptions { .. }
        | crate::project::Error::ArtifactCapture { .. }
        | crate::project::Error::MissingArtifactContract { .. }
        | crate::project::Error::SimulationInvalid { .. }
        | crate::project::Error::ScenarioFailed { .. }
        | crate::project::Error::SupervisorLaunch { .. }
        | crate::project::Error::ArtifactInvalid { .. }
        | crate::project::Error::ArtifactFile { .. }
        | crate::project::Error::BundleDirectory { .. }
        | crate::project::Error::BundleInvalid { .. }
        | crate::project::Error::BundleCopy { .. }
        | crate::project::Error::BundleJson { .. }
        | crate::project::Error::BundleWrite { .. }
        | crate::project::Error::BundleBusy { .. }
        | crate::project::Error::BundleLock { .. }
        | crate::project::Error::BundlePublish { .. }
        | crate::project::Error::BundleCleanup { .. }
        | crate::project::Error::PreparedInput(_) => None,
    }
}

fn print_bytes(bytes: &[u8], stderr: bool) {
    if bytes.is_empty() {
        return;
    }
    if stderr {
        eprint!("{}", String::from_utf8_lossy(bytes));
    } else {
        print!("{}", String::from_utf8_lossy(bytes));
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "cargo phoxal",
    bin_name = "cargo phoxal",
    version = env!("CARGO_PKG_VERSION"),
    about = "Validate and build a Phoxal robot project"
)]
struct Cli {
    /// Compose exactly these robot files in order (default: robot.yaml).
    #[arg(short = 'f', long = "file", global = true)]
    files: Vec<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    fn normalize_executable_paths(&mut self, invocation: &std::path::Path) {
        let controls = match &mut self.command {
            Command::Config(_) => return,
            Command::Check(arguments) => &mut arguments.options.build,
            Command::Test(arguments) => &mut arguments.options.build,
            Command::Prepare(arguments) => &mut arguments.options,
            Command::Build(arguments) => &mut arguments.options,
            Command::Run(arguments) => &mut arguments.options,
            Command::Simulation(arguments) => &mut arguments.options,
            Command::Scenario(arguments) => &mut arguments.options,
        };
        if let Some(path) = &mut controls.cargo
            && path.is_relative()
            && path
                .parent()
                .is_some_and(|parent| !parent.as_os_str().is_empty())
        {
            *path = explicit_path(invocation, path);
        }
    }

    fn json_diagnostics(&self) -> bool {
        match &self.command {
            Command::Config(_) => false,
            Command::Check(arguments) => json_common(&arguments.options, &arguments.cargo_args),
            Command::Build(arguments) => {
                json_format(arguments.options.message_format.as_deref(), &[])
            }
            Command::Run(arguments) => {
                json_format(arguments.options.message_format.as_deref(), &[])
            }
            Command::Test(arguments) => json_common(&arguments.options, &[]),
            Command::Simulation(arguments) => {
                json_format(arguments.options.message_format.as_deref(), &[])
            }
            Command::Scenario(arguments) => {
                json_format(arguments.options.message_format.as_deref(), &[])
            }
            Command::Prepare(arguments) => {
                json_format(arguments.options.message_format.as_deref(), &[])
            }
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the composed, authored-validated configuration without compilation.
    Config(ConfigArgs),
    /// Prepare selected contracts and generated APIs.
    Prepare(PrepareArgs),
    /// Validate the project and selected APIs, then Cargo-check requested code.
    Check(CommandArgs),
    /// Build the complete robot in release mode and publish its ZIP.
    Build(BuildArgs),
    /// Build and run the robot on hardware, including real device drivers.
    Run(RunArgs),
    /// Run ordinary Rust unit and integration tests.
    Test(TestArgs),
    /// Build the robot and simulate an explicitly selected scene.
    Simulation(SimulationArgs),
    /// Execute a standalone Rust scenario declared under scenarios/.
    Scenario(ScenarioArgs),
}

#[derive(Debug, Args)]
struct ConfigArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ScenarioArgs {
    #[command(flatten)]
    options: BuildControls,
    /// Build and execute the release profile.
    #[arg(long)]
    release: bool,
    /// Rust file declared as a Cargo example or binary with test=false.
    #[arg(value_name = "SCENARIO_FILE")]
    file: PathBuf,
    /// Present the scenario on the desktop instead of headless execution.
    #[arg(long)]
    desktop: bool,
}

#[derive(Debug, Args)]
struct PrepareArgs {
    #[command(flatten)]
    options: BuildControls,
}

#[derive(Debug, Args)]
struct SimulationArgs {
    #[command(flatten)]
    options: BuildControls,
    /// Scene to simulate. No default scene is selected.
    #[arg(value_name = "SCENE_FILE")]
    scene: PathBuf,
    /// Use an existing runnable build without invoking Cargo or acquiring sources.
    #[arg(long, value_name = "BUILD_DIR", conflicts_with_all = ["release", "cargo", "locked", "frozen", "offline", "target", "features", "all_features", "no_default_features", "message_format"])]
    build: Option<PathBuf>,
    /// Compile the robot with the release profile.
    #[arg(long)]
    release: bool,
    /// Run without a desktop window for a finite duration.
    #[arg(long, requires = "duration", conflicts_with = "paused")]
    headless: bool,
    /// Stop after simulated time, for example 10s or 250ms.
    #[arg(long, value_parser = positive_duration)]
    duration: Option<std::time::Duration>,
    /// Open the desktop simulation paused.
    #[arg(long)]
    paused: bool,
}

fn positive_duration(value: &str) -> Result<std::time::Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("duration must be positive (for example 10s or 250ms)".into());
    }
    Ok(duration)
}

fn run_simulation(mut arguments: SimulationArgs, files: &[PathBuf]) -> Result<(), project::Error> {
    use std::os::unix::process::CommandExt as _;
    let invocation = std::env::current_dir().map_err(|source| project::Error::ArtifactFile {
        path: ".".into(),
        source,
    })?;
    arguments.scene = explicit_path(&invocation, &arguments.scene);
    arguments.build = arguments
        .build
        .as_ref()
        .map(|path| explicit_path(&invocation, path));
    let (build, scene) = match arguments.build {
        Some(build) => (build, arguments.scene.clone()),
        None => {
            let start = std::env::current_dir().map_err(|source| {
                project::Error::Discovery(project::DiscoveryError::Resolve {
                    path: ".".into(),
                    source,
                })
            })?;
            let project = Project::discover_files(&start, files)?;
            let mut options = arguments.options.into_options(Vec::new(), Vec::new());
            options.release = arguments.release;
            let (bundle, scene) = project.build_simulation(&options, &arguments.scene, None)?;
            (bundle.root().to_owned(), scene)
        }
    };
    let executable = project::selected_simulator_executable(None)?;
    let mut command = std::process::Command::new(executable);
    command.arg("run").arg(scene).arg("--build").arg(build);
    if arguments.headless {
        command.arg("--headless");
    }
    if let Some(duration) = arguments.duration {
        command
            .arg("--duration")
            .arg(humantime::format_duration(duration).to_string());
    }
    if arguments.paused {
        command.arg("--paused");
    }
    let error = command.exec();
    Err(project::Error::SimulationInvalid {
        message: format!(
            "cannot start phoxal-simulator: {error}; install it with cargo install phoxal-simulator"
        ),
    })
}

#[derive(Debug, Args)]
struct CommandArgs {
    #[command(flatten)]
    options: CommonArgs,
    /// Additional arguments passed to Cargo after Phoxal's standard selectors.
    #[arg(last = true, allow_hyphen_values = true)]
    cargo_args: Vec<OsString>,
}

impl CommandArgs {
    fn into_options(self, trailing: Vec<OsString>) -> CargoOptions {
        self.options.into_options(self.cargo_args, trailing)
    }
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[command(flatten)]
    options: BuildControls,
    /// Archive destination, defaulting to `bundle/<robot-id>.zip`.
    #[arg(long, short = 'o', value_name = "ZIP_FILE")]
    output: Option<PathBuf>,
}

impl BuildArgs {
    fn into_options(self) -> CargoOptions {
        let mut options = self.options.into_options(Vec::new(), Vec::new());
        options.release = true;
        options
    }
}

#[derive(Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    options: BuildControls,
    /// Execute a release build instead of development mode.
    #[arg(long)]
    release: bool,
}

impl RunArgs {
    fn into_options(self) -> CargoOptions {
        let mut options = self.options.into_options(Vec::new(), Vec::new());
        options.release = self.release;
        options
    }
}

#[derive(Debug, Args)]
struct TestArgs {
    #[command(flatten)]
    options: CommonArgs,
    /// Optional substring filter passed to Cargo's Rust test harness.
    filter: Option<OsString>,
    /// Compile selected tests without executing them.
    #[arg(long)]
    no_run: bool,
    /// Arguments passed to the root test binary after Cargo's test delimiter.
    #[arg(last = true, allow_hyphen_values = true)]
    test_args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct BuildControls {
    /// Cargo executable to use for every metadata, build, check, test, and update invocation.
    #[arg(long, help_heading = "Advanced build options")]
    cargo: Option<PathBuf>,
    /// Require an existing current Cargo.lock.
    #[arg(
        long,
        conflicts_with = "frozen",
        help_heading = "Advanced build options"
    )]
    locked: bool,
    /// Require an existing current Cargo.lock and forbid network access.
    #[arg(long, help_heading = "Advanced build options")]
    frozen: bool,
    /// Forbid network access while allowing Cargo's normal lock policy.
    #[arg(long, help_heading = "Advanced build options")]
    offline: bool,
    /// Cargo target triple.
    #[arg(long, help_heading = "Advanced build options")]
    target: Option<String>,
    /// Comma-separated or repeated root feature names.
    #[arg(long, value_delimiter = ',', help_heading = "Advanced build options")]
    features: Vec<String>,
    /// Enable all root features.
    #[arg(long, help_heading = "Advanced build options")]
    all_features: bool,
    /// Disable default root features.
    #[arg(long = "no-default-features", help_heading = "Advanced build options")]
    no_default_features: bool,
    /// Cargo compiler message format, such as `json`.
    #[arg(long, help_heading = "Advanced build options")]
    message_format: Option<String>,
}

#[derive(Debug, Args)]
struct CommonArgs {
    #[command(flatten)]
    build: BuildControls,
    /// Cargo profile for checking or testing code.
    #[arg(long, conflicts_with = "release")]
    profile: Option<String>,
    /// Check or test in release mode.
    #[arg(long)]
    release: bool,
    /// Select the whole Cargo workspace.
    #[arg(long)]
    workspace: bool,
    /// Select an explicit Cargo package, repeatable.
    #[arg(long = "package", short = 'p', action = clap::ArgAction::Append)]
    packages: Vec<String>,
    /// Exclude a workspace package, repeatable.
    #[arg(long = "exclude", action = clap::ArgAction::Append)]
    excludes: Vec<String>,
    /// Select all targets.
    #[arg(long)]
    all_targets: bool,
    /// Select the package library target.
    #[arg(long)]
    lib: bool,
    /// Select all binary targets.
    #[arg(long)]
    bins: bool,
    /// Select a named binary target, repeatable.
    #[arg(long = "bin", action = clap::ArgAction::Append)]
    binaries: Vec<String>,
    /// Select all example targets.
    #[arg(long)]
    examples: bool,
    /// Select a named example target, repeatable.
    #[arg(long = "example", action = clap::ArgAction::Append)]
    examples_named: Vec<String>,
    /// Select all integration tests.
    #[arg(long)]
    tests: bool,
    /// Select a named integration test, repeatable.
    #[arg(long = "test", action = clap::ArgAction::Append)]
    tests_named: Vec<String>,
    /// Select all benchmarks.
    #[arg(long)]
    benches: bool,
    /// Select a named benchmark, repeatable.
    #[arg(long = "bench", action = clap::ArgAction::Append)]
    benches_named: Vec<String>,
}

impl BuildControls {
    fn into_options(self, cargo_args: Vec<OsString>, test_args: Vec<OsString>) -> CargoOptions {
        CargoOptions {
            cargo_path: self.cargo,
            lock: if self.frozen {
                LockMode::Frozen
            } else if self.locked {
                LockMode::Locked
            } else {
                LockMode::Unlocked
            },
            offline: self.offline,
            target: self.target,
            features: self.features,
            all_features: self.all_features,
            no_default_features: self.no_default_features,
            message_format: self.message_format,
            cargo_args,
            test_args,
            ..CargoOptions::default()
        }
    }
}

impl CommonArgs {
    fn into_options(self, cargo_args: Vec<OsString>, test_args: Vec<OsString>) -> CargoOptions {
        let mut options = self.build.into_options(cargo_args, test_args);
        options.profile = self.profile;
        options.release = self.release;
        options.selection = CargoSelection {
            workspace: self.workspace,
            packages: self.packages,
            excludes: self.excludes,
            all_targets: self.all_targets,
            lib: self.lib,
            bins: self.bins,
            binaries: self.binaries,
            examples: self.examples,
            examples_named: self.examples_named,
            tests: self.tests,
            tests_named: self.tests_named,
            benches: self.benches,
            benches_named: self.benches_named,
        };
        options
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn explicit_command_paths_keep_the_invocation_directory_and_spaces() {
        let root = std::path::Path::new("/robots/my rover");
        let scenarios = root.join("scenarios");
        assert_eq!(
            explicit_path(root, std::path::Path::new("scenarios/forward stop.rs")),
            explicit_path(&scenarios, std::path::Path::new("forward stop.rs"))
        );
        assert_eq!(
            explicit_path(root, std::path::Path::new("scene with spaces.xml")),
            root.join("scene with spaces.xml")
        );
        assert_eq!(
            explicit_path(&scenarios, std::path::Path::new("/existing build")),
            PathBuf::from("/existing build")
        );
        assert_eq!(
            explicit_path(&scenarios, std::path::Path::new("release robot.zip")),
            scenarios.join("release robot.zip")
        );
    }

    #[test]
    fn cargo_external_subcommand_and_direct_invocation_parse_identically() {
        for arguments in [
            vec!["cargo-phoxal", "phoxal", "check", "--locked"],
            vec!["cargo-phoxal", "check", "--locked"],
        ] {
            let parsed = Cli::try_parse_from(super::cargo_arguments(
                arguments.into_iter().map(OsString::from),
            ))
            .unwrap();
            assert!(matches!(parsed.command, Command::Check(_)));
        }
    }

    #[test]
    fn simulation_requires_scene_and_enforces_execution_choices() {
        assert!(Cli::try_parse_from(["cargo-phoxal", "simulation"]).is_err());
        assert!(
            Cli::try_parse_from(["cargo-phoxal", "simulation", "scene.xml", "--headless"]).is_err()
        );
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "simulation",
                "scene.xml",
                "--build",
                "build",
                "--release"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "simulation",
                "scene.xml",
                "--duration",
                "NaN"
            ])
            .is_err()
        );
        let parsed = Cli::try_parse_from(["cargo-phoxal", "simulation", "scene.xml"])
            .expect("desktop simulation parses");
        let Command::Simulation(arguments) = parsed.command else {
            panic!("wrong command")
        };
        assert!(!arguments.release);
        assert!(!arguments.headless);
        assert!(arguments.duration.is_none());
        assert!(arguments.build.is_none());
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "simulation",
                "scene.xml",
                "--build",
                "build",
                "--headless",
                "--duration",
                "1.5s"
            ])
            .is_ok()
        );
    }

    #[test]
    fn documented_commands_parse_and_preserve_lock_policy() {
        let parsed = Cli::try_parse_from(["cargo-phoxal", "check", "--locked"])
            .expect("check command parses");
        let options = match parsed.command {
            Command::Check(arguments) => arguments.into_options(Vec::new()),
            _ => panic!("the check command parsed as a different variant"),
        };
        assert_eq!(options.lock, LockMode::Locked);

        let parsed =
            Cli::try_parse_from(["cargo-phoxal", "test", "--frozen"]).expect("test command parses");
        let options = match parsed.command {
            Command::Test(arguments) => arguments
                .options
                .into_options(Vec::new(), arguments.test_args),
            _ => panic!("the test command parsed as a different variant"),
        };
        assert_eq!(options.lock, LockMode::Frozen);
    }

    #[test]
    fn test_arguments_after_the_delimiter_are_not_sent_to_metadata() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "test",
            "--offline",
            "--",
            "--exact",
            "brain_tests::starts_empty",
        ])
        .expect("test arguments parse");
        let options = match parsed.command {
            Command::Test(arguments) => arguments
                .options
                .into_options(Vec::new(), arguments.test_args),
            _ => panic!("the test command parsed as a different variant"),
        };
        assert!(options.cargo_args.is_empty());
        assert_eq!(
            options.test_args,
            [
                OsString::from("--exact"),
                OsString::from("brain_tests::starts_empty")
            ]
        );
    }

    #[test]
    fn test_delimiter_does_not_turn_test_binary_arguments_into_json_diagnostics() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "test",
            "--",
            "--message-format=json-render-diagnostics",
        ])
        .expect("test binary arguments parse");
        assert!(!parsed.json_diagnostics());
    }

    #[test]
    fn build_boundary_parses_without_extra_process_options() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "build",
            "--output",
            "target/bundle",
            "--offline",
        ])
        .expect("build command parses");
        assert!(matches!(parsed.command, Command::Build(_)));
    }

    #[test]
    fn whole_robot_commands_reject_cargo_selectors_and_raw_tails() {
        for command in ["prepare", "build", "run"] {
            for selector in ["--workspace", "--lib", "--examples", "--tests", "--benches"] {
                assert!(Cli::try_parse_from(["cargo-phoxal", command, selector]).is_err());
            }
            assert!(Cli::try_parse_from(["cargo-phoxal", command, "--", "--release"]).is_err());
        }
        assert!(Cli::try_parse_from(["cargo-phoxal", "build", "--profile", "release"]).is_err());
        assert!(Cli::try_parse_from(["cargo-phoxal", "build", "--release"]).is_err());
        assert!(Cli::try_parse_from(["cargo-phoxal", "run", "--output", "build"]).is_err());
        let parsed = Cli::try_parse_from(["cargo-phoxal", "run", "--release"])
            .expect("run release selection");
        let Command::Run(arguments) = parsed.command else {
            panic!("wrong command")
        };
        assert!(arguments.into_options().release);
    }

    #[test]
    fn cargo_feature_flags_follow_cargo_semantics() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "check",
            "--all-features",
            "--no-default-features",
        ])
        .expect("Cargo permits both feature flags");
        let Command::Check(arguments) = parsed.command else {
            panic!("wrong command")
        };
        let options = arguments.into_options(Vec::new());
        options.validate().expect("both feature flags are valid");
        assert!(options.all_features && options.no_default_features);
        assert!(
            Cli::try_parse_from(["cargo-phoxal", "check", "--profile", "release", "--release"])
                .is_err()
        );
    }

    #[test]
    fn source_execution_accepts_controls_and_existing_build_refuses_them() {
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "scenario",
                "scenarios/forward.rs",
                "--offline",
                "--locked",
                "--features",
                "proof",
                "--release"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "simulation",
                "scene.xml",
                "--offline",
                "--features",
                "proof"
            ])
            .is_ok()
        );
        for flag in [
            "--locked",
            "--offline",
            "--all-features",
            "--no-default-features",
        ] {
            assert!(
                Cli::try_parse_from([
                    "cargo-phoxal",
                    "simulation",
                    "scene.xml",
                    "--build",
                    "build",
                    flag
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn cargo_path_and_native_package_selection_are_preserved() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "test",
            "--cargo",
            "/opt/cargo/bin/cargo",
            "--workspace",
            "--package",
            "robot",
            "--exclude",
            "fixture",
            "--all-targets",
            "--release",
            "--message-format",
            "json-render-diagnostics",
            "--",
            "--nocapture",
        ])
        .expect("Cargo selectors parse");
        let options = match parsed.command {
            Command::Test(arguments) => arguments
                .options
                .into_options(Vec::new(), arguments.test_args),
            _ => panic!("the test command parsed as a different variant"),
        };
        assert_eq!(
            options.cargo_path,
            Some(PathBuf::from("/opt/cargo/bin/cargo"))
        );
        assert!(options.selection.workspace);
        assert_eq!(options.selection.packages, ["robot"]);
        assert_eq!(options.selection.excludes, ["fixture"]);
        assert!(options.selection.all_targets);
        assert!(options.release);
        assert!(options.cargo_args.is_empty());
        assert_eq!(options.test_args, [OsString::from("--nocapture")]);
        assert!(json_requested(&options));
    }
}
