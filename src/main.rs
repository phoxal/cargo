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
    if arguments
        .get(1)
        .is_some_and(|argument| argument == "simulation")
    {
        use std::os::unix::process::CommandExt as _;
        let executable =
            std::env::var_os("PHOXAL_SIMULATOR").unwrap_or_else(|| "phoxal-simulator".into());
        let error = std::process::Command::new(executable)
            .args(arguments.iter().skip(2))
            .exec();
        eprintln!(
            "cannot start phoxal-simulator: {error}; install it with cargo install phoxal-simulator"
        );
        return ExitCode::FAILURE;
    }
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
    arguments
}

fn run(cli: Cli) -> Result<(), crate::project::Error> {
    let command = cli.command;
    match command {
        Command::Prepare(arguments) => {
            let options = arguments.options.into_options(Vec::new(), Vec::new());
            let start = std::env::current_dir().map_err(|source| {
                crate::project::Error::Discovery(crate::project::DiscoveryError::Resolve {
                    path: ".".into(),
                    source,
                })
            })?;
            let layout = crate::project::ProjectLayout::discover(&start)
                .map_err(crate::project::Error::Discovery)?;
            let changes = crate::project::participant::prepare(&layout, &options)?;
            for change in changes {
                eprintln!("prepared {change}");
            }
            Ok(())
        }
        command => {
            let project = Project::discover(std::env::current_dir().map_err(|source| {
                crate::project::Error::Discovery(crate::project::DiscoveryError::Resolve {
                    path: ".".into(),
                    source,
                })
            })?)?;
            match command {
                Command::Check(arguments) => run_cargo(
                    &project,
                    CargoOperation::Check,
                    arguments.into_options(Vec::new()),
                ),
                Command::Build(arguments) => {
                    let output = arguments.output.clone();
                    let simulation_scene = arguments.simulation_scene.clone();
                    let options = arguments.into_options();
                    let bundle = if let Some(scene) = simulation_scene {
                        project.build_simulation(&options, &scene, output)?
                    } else {
                        let prepared = project.prepare(&options)?;
                        let output = output.unwrap_or_else(|| prepared.default_bundle_path());
                        prepared.build_bundle(&options, output)?
                    };
                    print_status(
                        &options,
                        &format!("compiled bundle: {}", bundle.root().display()),
                    );
                    Ok(())
                }
                Command::Run(arguments) => {
                    if arguments.simulation_scene.is_some() {
                        return Err(crate::project::Error::InvalidOptions { message: "use build --simulation-scene, then simulation run for native simulation".into() });
                    }
                    let output = arguments.output.clone();
                    let options = arguments.into_options();
                    let prepared = project.prepare(&options)?;
                    let output = output.unwrap_or_else(|| prepared.default_bundle_path());
                    let bundle = prepared.run_local(&options, output)?;
                    print_status(
                        &options,
                        &format!("compiled bundle: {}", bundle.root().display()),
                    );
                    Ok(())
                }
                Command::Test(arguments) => run_test(&project, arguments),
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
    let prepared = project.prepare(&options)?;
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
        simulator,
        desktop,
        test_args,
    } = arguments;
    let mut cargo_args = Vec::new();
    if let Some(filter) = filter {
        cargo_args.push(filter);
    }
    if no_run {
        cargo_args.push(OsString::from("--no-run"));
    }
    let options = options.into_options(cargo_args, test_args);
    let json = json_requested(&options);
    // Test filters and --no-run belong to cargo test, while preparation
    // builds the complete robot executable graph.
    let mut preparation = options.clone();
    preparation.cargo_args.clear();
    preparation.test_args.clear();
    preparation.selection = crate::project::CargoSelection::default();
    let prepared = project.prepare(&preparation)?;
    let outputs = crate::project::scenario::fixture_host::run_tests(
        project,
        &prepared,
        &options,
        &crate::project::scenario::fixture_host::TestHostOptions {
            simulator,
            headless: !desktop,
        },
    )?;
    for output in outputs {
        print_bytes(&output.stdout, false);
        print_bytes(&output.stderr, true);
    }
    if json {
        eprintln!("cargo phoxal: test completed");
    }
    Ok(())
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
    json_format(options.message_format.as_deref(), cargo_args)
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
        | crate::project::Error::BundleCleanup { .. } => None,
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
    #[command(subcommand)]
    command: Command,
}

impl Cli {
    fn json_diagnostics(&self) -> bool {
        match &self.command {
            Command::Check(arguments) => json_common(&arguments.options, &arguments.cargo_args),
            Command::Build(arguments) | Command::Run(arguments) => {
                json_common(&arguments.options, &arguments.cargo_args)
            }
            Command::Test(arguments) => json_common(&arguments.options, &[]),
            Command::Simulation(_) => false,
            Command::Prepare(arguments) => json_common(&arguments.options, &[]),
        }
    }
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Install exact selected participants and prepare their Protobuf sources.
    Prepare(PrepareArgs),
    /// Validate the project and selected APIs, then Cargo-check requested code.
    Check(CommandArgs),
    /// Prepare the project, validate composition, and build selected targets.
    Build(BuildArgs),
    /// Prepare, build, validate, and launch the selected supervisor locally.
    Run(BuildArgs),
    /// Prepare the project and run tests for the root robot package.
    Test(TestArgs),
    /// Forward arguments and process status to phoxal-simulator.
    Simulation(SimulationArgs),
}

#[derive(Debug, Args)]
struct PrepareArgs {
    #[command(flatten)]
    options: CommonArgs,
}

#[derive(Debug, Args)]
struct SimulationArgs {
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<OsString>,
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
    /// Freeze and probe this scene to produce a simulation bundle.
    #[arg(long)]
    simulation_scene: Option<PathBuf>,
    #[command(flatten)]
    options: CommonArgs,
    /// Compiled bundle directory, defaulting below Cargo's target directory.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Additional arguments passed to Cargo after Phoxal's standard selectors.
    #[arg(last = true, allow_hyphen_values = true)]
    cargo_args: Vec<OsString>,
}

impl BuildArgs {
    fn into_options(self) -> CargoOptions {
        self.options.into_options(self.cargo_args, Vec::new())
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
    /// Explicit simulator executable for source-development qualification.
    #[arg(long)]
    simulator: Option<PathBuf>,
    /// Present simulation runs in the desktop application instead of headless mode.
    #[arg(long)]
    desktop: bool,
    /// Arguments passed to the root test binary after Cargo's test delimiter.
    #[arg(last = true, allow_hyphen_values = true)]
    test_args: Vec<OsString>,
}

#[derive(Debug, Args)]
struct CommonArgs {
    /// Cargo executable to use for every metadata, build, check, test, and update invocation.
    #[arg(long, env = "CARGO", hide_env_values = true)]
    cargo: Option<PathBuf>,
    /// Require an existing current Cargo.lock.
    #[arg(long, conflicts_with = "frozen")]
    locked: bool,
    /// Require an existing current Cargo.lock and forbid network access.
    #[arg(long)]
    frozen: bool,
    /// Forbid network access while allowing Cargo's normal lock policy.
    #[arg(long)]
    offline: bool,
    /// Cargo target triple.
    #[arg(long)]
    target: Option<String>,
    /// Cargo profile.
    #[arg(long)]
    profile: Option<String>,
    /// Comma-separated or repeated root feature names.
    #[arg(long, value_delimiter = ',')]
    features: Vec<String>,
    /// Enable all root features.
    #[arg(long, conflicts_with = "no_default_features")]
    all_features: bool,
    /// Disable default root features.
    #[arg(long = "no-default-features")]
    no_default_features: bool,
    /// Build with Cargo's release profile.
    #[arg(long)]
    release: bool,
    /// Cargo compiler message format, such as `json`.
    #[arg(long)]
    message_format: Option<String>,
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

impl CommonArgs {
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
            profile: self.profile,
            features: self.features,
            all_features: self.all_features,
            no_default_features: self.no_default_features,
            release: self.release,
            message_format: self.message_format,
            cargo_args,
            test_args,
            selection: CargoSelection {
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
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

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
    fn run_boundary_preserves_the_build_output_and_cargo_argument_surface() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "run",
            "--output",
            "target/run-bundle",
            "--target",
            "aarch64-unknown-linux-gnu",
            "--",
            "--release",
        ])
        .expect("run command parses");
        let arguments = match parsed.command {
            Command::Run(arguments) => arguments,
            _ => panic!("the run command parsed as a different variant"),
        };
        assert_eq!(arguments.output, Some(PathBuf::from("target/run-bundle")));
        let options = arguments.into_options();
        assert_eq!(options.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
        assert!(!options.release);
        assert_eq!(options.cargo_args, [OsString::from("--release")]);
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
