#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

mod project;

use project::{
    CargoOperation, CargoOptions, CargoSelection, LockMode, PreparedProject, Project,
    PublicationKind, PublicationOptions, SelectedTarget, SimulationBound, SimulationPresentation,
    SimulationRunOptions, SimulationRunReport, SubmissionResult, install_simulator,
    prepare_publication, simulator_status, submit_publication, uninstall_simulator,
};

fn main() -> ExitCode {
    let cli = Cli::parse_from(cargo_arguments(std::env::args_os()));
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
        Command::Publish(arguments) => run_publication(arguments),
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
        Command::Simulation(arguments) => match arguments.command {
            SimulationCommand::Install(arguments) => run_simulator_install(arguments, false),
            SimulationCommand::Upgrade(arguments) => run_simulator_install(arguments, true),
            SimulationCommand::Status(arguments) => run_simulator_status(arguments),
            SimulationCommand::Uninstall => run_simulator_uninstall(),
            SimulationCommand::Run(arguments) => run_simulation(*arguments),
        },
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
                    let options = arguments.into_options();
                    let prepared = project.prepare(&options)?;
                    let output = output.unwrap_or_else(|| prepared.default_bundle_path());
                    let bundle = prepared.build_bundle(&options, output)?;
                    print_status(
                        &options,
                        &format!("compiled bundle: {}", bundle.root().display()),
                    );
                    Ok(())
                }
                Command::Run(arguments) => {
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
                Command::Publish(_) => unreachable!("publish was handled above"),
                Command::Prepare(_) => unreachable!("prepare was handled above"),
            }
        }
    }
}

fn run_simulator_install(
    arguments: SimulationInstallArgs,
    replace: bool,
) -> Result<(), crate::project::Error> {
    let options = CargoOptions {
        cargo_path: arguments.cargo,
        offline: arguments.offline,
        ..CargoOptions::default()
    };
    let status = install_simulator(
        &options,
        arguments.mujoco_distribution.as_deref(),
        replace || arguments.force,
        arguments.installer_version.as_deref(),
        arguments.simulator_version.as_deref(),
    )?;
    println!("simulator: installed");
    println!(
        "version: {}",
        status.simulator_version.as_deref().unwrap_or("unknown")
    );
    println!(
        "mujoco: {}",
        status.mujoco_version.as_deref().unwrap_or("unknown")
    );
    if let Some(executable) = status.executable {
        println!("executable: {}", executable.display());
    }
    Ok(())
}

fn run_simulator_status(arguments: SimulationStatusArgs) -> Result<(), crate::project::Error> {
    let status = simulator_status()?;
    if arguments.json {
        println!(
            "{}",
            serde_json::to_string(&status).map_err(|source| {
                crate::project::Error::SimulationInvalid {
                    message: format!("cannot encode simulator status: {source}"),
                }
            })?
        );
    } else if status.installed {
        println!("simulator: installed");
        println!("root: {}", status.root.display());
        println!(
            "version: {}",
            status.simulator_version.as_deref().unwrap_or("unknown")
        );
        println!(
            "mujoco: {}",
            status.mujoco_version.as_deref().unwrap_or("unknown")
        );
        if let Some(executable) = status.executable {
            println!("executable: {}", executable.display());
        }
    } else {
        println!("simulator: not installed");
        println!("root: {}", status.root.display());
    }
    Ok(())
}

fn run_simulator_uninstall() -> Result<(), crate::project::Error> {
    let root = uninstall_simulator()?;
    println!("simulator: uninstalled");
    println!("root: {}", root.display());
    Ok(())
}

fn run_simulation(arguments: SimulationRunArgs) -> Result<(), crate::project::Error> {
    let SimulationRunArgs {
        scene,
        headless,
        desktop: _,
        steps,
        duration,
        simulator,
        output,
        scope,
        supervisor_id,
        run_id,
        options,
    } = arguments;
    let presentation = if headless {
        SimulationPresentation::Headless
    } else {
        SimulationPresentation::Desktop
    };
    let bound = match (steps, duration) {
        (Some(steps), None) => SimulationBound::Steps(steps),
        (None, Some(duration)) => SimulationBound::Duration(duration),
        (None, None) => SimulationBound::Steps(1),
        (Some(_), Some(_)) => {
            return Err(crate::project::Error::SimulationInvalid {
                message: "choose either --steps or --duration".to_owned(),
            });
        }
    };
    let mut request = SimulationRunOptions::new(scene, presentation, bound)?;
    if let Some(path) = simulator {
        request = request.with_simulator_executable(path);
    }
    if let Some(path) = output {
        request = request.with_output(path);
    }
    if scope.is_some() || supervisor_id.is_some() || run_id.is_some() {
        request = request.with_identity(
            scope.unwrap_or_else(|| "local".to_owned()),
            supervisor_id.unwrap_or_else(|| "local".to_owned()),
            run_id.unwrap_or_else(|| "local-simulation".to_owned()),
        );
    }
    let cargo_options = options.into_options(Vec::new(), Vec::new());
    let project = Project::discover(std::env::current_dir().map_err(|source| {
        crate::project::Error::Discovery(crate::project::DiscoveryError::Resolve {
            path: ".".into(),
            source,
        })
    })?)?;
    let report = project.run_simulation(&cargo_options, &request)?;
    let SimulationRunReport::V0 {
        simulator_stdout, ..
    } = &report;
    if !simulator_stdout.trim().is_empty() {
        print!("{}", simulator_stdout);
        if !simulator_stdout.ends_with('\n') {
            println!();
        }
    }
    println!(
        "{}",
        serde_json::to_string(&report).map_err(|source| {
            crate::project::Error::SimulationInvalid {
                message: format!("cannot encode simulation terminal report: {source}"),
            }
        })?
    );
    let SimulationRunReport::V0 {
        simulator: report_simulator,
        bundle: report_bundle,
        simulator_exit_code,
        simulator_stderr,
        provider_contract_verified,
        supervisor_ready,
        cleanup,
        ..
    } = &report;
    if !simulator_stderr.is_empty() {
        eprint!("{}", simulator_stderr);
    }
    eprintln!(
        "simulation: simulator={} bundle={} provider_contract={} cleanup={}",
        report_simulator.executable.display(),
        report_bundle.display(),
        if *provider_contract_verified {
            "verified"
        } else {
            "unverified"
        },
        if cleanup.error.is_none() {
            "complete"
        } else {
            "incomplete"
        }
    );
    if report.success() {
        Ok(())
    } else {
        Err(crate::project::Error::SimulationInvalid {
            message: format!(
                "simulation did not complete successfully (exit={}, supervisor_ready={}, provider_contract_verified={}, cleanup={})",
                simulator_exit_code.map_or_else(|| "signal".to_owned(), |code| code.to_string()),
                supervisor_ready,
                provider_contract_verified,
                if cleanup.error.is_none() {
                    "complete"
                } else {
                    "incomplete"
                }
            ),
        })
    }
}

fn run_publication(arguments: PublishArgs) -> Result<(), crate::project::Error> {
    let (kind, package) = match arguments.package {
        PublishPackage::Component(package) => (PublicationKind::Component, package),
        PublishPackage::Service(package) => (PublicationKind::Service, package),
        PublishPackage::Library(package) => (PublicationKind::Library, package),
        PublishPackage::ProcMacro(package) => (PublicationKind::ProcMacro, package),
        PublishPackage::Simulator(package) => (PublicationKind::SimulatorApplication, package),
        PublishPackage::Application(package) => (PublicationKind::Application, package),
        PublishPackage::Tool(package) => (PublicationKind::Tool, package),
    };
    let dry_run = package.dry_run;
    let result = prepare_publication(&PublicationOptions {
        kind,
        name: package.name,
        path: package.path,
        dry_run,
    })?;
    let result = if dry_run { result.retain() } else { result };
    println!(
        "publication: {}",
        if dry_run { "dry-run" } else { "prepared" }
    );
    println!("kind: {}", result.kind());
    println!("package: {}", result.package());
    println!("version: {}", result.version());
    println!("archive: {}", result.archive().display());
    println!("sha256: {}", result.checksum());
    println!("bytes: {}", result.bytes());
    if !dry_run {
        let submission = submit_publication(&result, |authorization| {
            eprintln!(
                "Authorize cargo-phoxal at {} with code {} (expires in {} seconds).",
                authorization.verification_uri,
                authorization.user_code,
                authorization.expires_in.as_secs()
            );
            eprintln!(
                "GitHub's public_repo scope covers every public repository accessible to this account."
            );
        })?;
        match submission {
            SubmissionResult::Available { archive_url } => {
                println!("publication: available");
                println!("archive-url: {archive_url}");
            }
            SubmissionResult::PendingReview {
                pull_request_url,
                branch,
            } => {
                println!("publication: pending-review");
                println!("branch: {branch}");
                println!("pull-request: {pull_request_url}");
            }
        }
    }
    Ok(())
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
        | crate::project::Error::BundleCleanup { .. }
        | crate::project::Error::Publication(_) => None,
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
            Command::Simulation(arguments) => match &arguments.command {
                SimulationCommand::Install(_) | SimulationCommand::Upgrade(_) => false,
                SimulationCommand::Status(_) | SimulationCommand::Uninstall => false,
                SimulationCommand::Run(arguments) => json_common(&arguments.options, &[]),
            },
            Command::Publish(_) => false,
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
    /// Provision and run the independent native simulator application.
    Simulation(SimulationArgs),
    /// Prepare an authored component or service package for registry review.
    Publish(PublishArgs),
}

#[derive(Debug, Args)]
struct PrepareArgs {
    #[command(flatten)]
    options: CommonArgs,
}

#[derive(Debug, Args)]
struct SimulationArgs {
    #[command(subcommand)]
    command: SimulationCommand,
}

#[derive(Debug, Subcommand)]
enum SimulationCommand {
    /// Download MuJoCo and install the matching simulator from the Phoxal registry.
    Install(SimulationInstallArgs),
    /// Replace the managed MuJoCo and simulator installation with the current versions.
    Upgrade(SimulationInstallArgs),
    /// Inspect the managed simulator installation.
    Status(SimulationStatusArgs),
    /// Remove the managed simulator installation.
    Uninstall,
    /// Run one finite scene against the selected robot bundle.
    Run(Box<SimulationRunArgs>),
}

#[derive(Debug, Args)]
struct SimulationInstallArgs {
    /// Select an exact simulator installer release independently of this tool.
    #[arg(long)]
    installer_version: Option<String>,
    /// Select an exact native simulator application release independently of the installer.
    #[arg(long)]
    simulator_version: Option<String>,
    /// Use an existing MuJoCo distribution instead of downloading the official release.
    #[arg(long = "mujoco-distribution")]
    mujoco_distribution: Option<PathBuf>,
    /// Replace an existing managed installation.
    #[arg(long)]
    force: bool,
    /// Cargo executable used to build the registry simulator package.
    #[arg(long, env = "CARGO", hide_env_values = true)]
    cargo: Option<PathBuf>,
    /// Use only already cached Cargo packages and require an explicit MuJoCo distribution.
    #[arg(long)]
    offline: bool,
}

#[derive(Debug, Args)]
struct SimulationStatusArgs {
    /// Emit one machine-readable JSON record.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SimulationRunArgs {
    /// Scene MJCF or MJZ archive.
    scene: PathBuf,
    /// Run without opening a presentation window.
    #[arg(long, conflicts_with = "desktop")]
    headless: bool,
    /// Run with the simulator desktop presentation.
    #[arg(long, conflicts_with = "headless")]
    desktop: bool,
    /// Advance exactly this many native quanta.
    #[arg(long, conflicts_with = "duration")]
    steps: Option<u64>,
    /// Advance exactly this many seconds, requiring an integral quantum count.
    #[arg(long, conflicts_with = "steps")]
    duration: Option<f64>,
    /// Explicit simulator executable injection or installed artifact path.
    #[arg(long)]
    simulator: Option<PathBuf>,
    /// Compiled simulation bundle output path.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Router namespace for this local launch.
    #[arg(long)]
    scope: Option<String>,
    /// Supervisor identity within the router namespace.
    #[arg(long)]
    supervisor_id: Option<String>,
    /// Finite run identity passed to the simulator.
    #[arg(long)]
    run_id: Option<String>,
    #[command(flatten)]
    options: CommonArgs,
}

#[derive(Debug, Args)]
struct PublishArgs {
    #[command(subcommand)]
    package: PublishPackage,
}

#[derive(Debug, Subcommand)]
enum PublishPackage {
    /// Prepare a component package, including a targetless passive carrier.
    Component(PublishPackageArgs),
    /// Prepare a runnable service implementation package.
    Service(PublishPackageArgs),
    /// Prepare a reusable library package.
    Library(PublishPackageArgs),
    /// Prepare a procedural macro package.
    ProcMacro(PublishPackageArgs),
    /// Prepare an independently built simulator application package.
    Simulator(PublishPackageArgs),
    /// Prepare an independently built non-simulator application package.
    Application(PublishPackageArgs),
    /// Prepare a standalone developer or operator tool package.
    Tool(PublishPackageArgs),
}

#[derive(Debug, Args)]
struct PublishPackageArgs {
    /// Exact Cargo package name from the authored Cargo.toml.
    name: String,
    /// Source directory containing the authored Cargo.toml.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Prepare and verify locally without GitHub authentication or mutation.
    #[arg(long)]
    dry_run: bool,
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
    fn simulation_run_parses_scene_mode_bound_and_lock_policy() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "simulation",
            "run",
            "scene.xml",
            "--headless",
            "--steps",
            "4",
            "--locked",
            "--scope",
            "workshop",
            "--supervisor-id",
            "rover-01",
            "--run-id",
            "run-1",
        ])
        .expect("simulation run parses");
        let arguments = match parsed.command {
            Command::Simulation(arguments) => match arguments.command {
                SimulationCommand::Run(arguments) => arguments,
                _ => panic!("simulation run parsed as a management command"),
            },
            _ => panic!("simulation command parsed as a different variant"),
        };
        assert_eq!(arguments.scene, PathBuf::from("scene.xml"));
        assert!(arguments.headless);
        assert!(!arguments.desktop);
        assert_eq!(arguments.steps, Some(4));
        assert_eq!(arguments.duration, None);
        assert_eq!(arguments.scope.as_deref(), Some("workshop"));
        assert_eq!(arguments.supervisor_id.as_deref(), Some("rover-01"));
        assert_eq!(arguments.run_id.as_deref(), Some("run-1"));
        assert_eq!(
            arguments.options.into_options(Vec::new(), Vec::new()).lock,
            LockMode::Locked
        );
    }

    #[test]
    fn simulation_run_rejects_conflicting_modes_and_bounds() {
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "simulation",
                "run",
                "scene.xml",
                "--headless",
                "--desktop",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "cargo-phoxal",
                "simulation",
                "run",
                "scene.xml",
                "--steps",
                "1",
                "--duration",
                "0.01",
            ])
            .is_err()
        );
    }

    #[test]
    fn publication_supports_normal_submission_and_explicit_dry_run() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "publish",
            "component",
            "fixture-passive-caster",
            "--dry-run",
        ])
        .expect("publication dry-run parses");
        assert!(matches!(parsed.command, Command::Publish(_)));
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "publish",
            "component",
            "fixture-passive-caster",
        ])
        .expect("normal publication parses");
        assert!(matches!(parsed.command, Command::Publish(_)));
    }

    #[test]
    fn publication_accepts_every_registry_package_role() {
        for role in [
            "component",
            "service",
            "library",
            "proc-macro",
            "simulator",
            "application",
            "tool",
        ] {
            let parsed = Cli::try_parse_from([
                "cargo-phoxal",
                "publish",
                role,
                "example-package",
                "--dry-run",
            ])
            .unwrap_or_else(|error| panic!("{role} publication must parse: {error}"));
            assert!(matches!(parsed.command, Command::Publish(_)));
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
