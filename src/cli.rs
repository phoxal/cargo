//! Argument parsing and the one invocation normalization boundary.
//!
//! Raw Cargo argument tails, explicit command paths, and per-command
//! selections resolve here exactly once; command workflows consume the
//! normalized [`Invocation`] and never reparse raw arguments.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

use crate::project::{CargoOptions, CargoSelection, Error, LockMode};

/// Parses the process arguments, accepting both `cargo-phoxal ...` and the
/// `cargo phoxal ...` external-subcommand form.
pub(crate) fn parse() -> Cli {
    let arguments = cargo_arguments(std::env::args_os());
    Cli::parse_from(arguments)
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
    // Clap propagates a global Vec from one level only. Lift robot
    // selections before parsing so occurrences around the subcommand retain
    // total order, in every supported spelling.
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
        if !tail && (argument == "-r" || argument == "--robot") {
            lifted.push(argument);
            if let Some(value) = iterator.next() {
                lifted.push(value);
            }
        } else if !tail
            && argument.to_str().is_some_and(|value| {
                value.starts_with("--robot=")
                    || (value.starts_with("-r") && value.len() > 2 && !value.starts_with("--"))
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

/// Resolves one command-relative path against the invocation directory.
pub(crate) fn explicit_path(invocation: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        invocation.join(path)
    }
}

/// One normalized command invocation.
///
/// Paths, Cargo selections, and the diagnostics policy are resolved once at
/// this boundary; workflow handlers consume only these resolved values.
#[derive(Debug)]
pub(crate) struct Invocation {
    /// The invocation directory for command-relative paths.
    pub(crate) invocation: PathBuf,
    /// Explicit robot files in caller order, resolved absolutely.
    pub(crate) files: Vec<PathBuf>,
    /// Whether compiler output and Phoxal diagnostics use JSON.
    pub(crate) diagnostics_json: bool,
    /// The normalized command workflow and its resolved selections.
    pub(crate) command: Command,
}

/// The normalized command workflows.
#[derive(Debug, Clone)]
pub(crate) enum Command {
    /// Print the composed, authored-validated configuration.
    Config { json: bool },
    /// Prepare selected contracts and generated APIs.
    Prepare { options: CargoOptions },
    /// Validate the project and selected APIs, then Cargo-check code.
    Check { options: CargoOptions },
    /// Compile, assemble, and publish the complete runnable directory.
    Build {
        options: CargoOptions,
        output: Option<PathBuf>,
        print_output_path: bool,
        watch: bool,
    },
    /// Compile, assemble, publish, and run the robot on hardware.
    Run { options: CargoOptions },
    /// Run ordinary Rust unit and integration tests.
    Test { options: CargoOptions },
    /// Execute a standalone scenario declared under scenarios/.
    Scenario {
        options: CargoOptions,
        file: PathBuf,
        desktop: bool,
    },
    /// Build and simulate, or delegate to an existing build.
    Simulation(SimulationInvocation),
}

/// One normalized simulation invocation.
#[derive(Debug, Clone)]
pub(crate) struct SimulationInvocation {
    /// Resolved Cargo selections for the compiled robot.
    pub(crate) options: CargoOptions,
    /// The scene to simulate, resolved against the invocation directory.
    pub(crate) scene: PathBuf,
    /// An existing runnable build that bypasses Cargo and acquisition.
    pub(crate) build: Option<PathBuf>,
    /// Run without a desktop window for a finite duration.
    pub(crate) headless: bool,
    /// Stop after simulated time.
    pub(crate) duration: Option<Duration>,
    /// Open the desktop simulation paused.
    pub(crate) paused: bool,
}

impl SimulationInvocation {
    /// Whether this invocation delegates to an existing runnable build.
    pub(crate) fn uses_existing_build(&self) -> bool {
        self.build.is_some()
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "cargo phoxal",
    bin_name = "cargo phoxal",
    version = env!("CARGO_PKG_VERSION"),
    about = "Validate and build a Phoxal robot project"
)]
pub(crate) struct Cli {
    /// Compose exactly these robot files, repeating --robot once per file in
    /// the given order (default: robot.yaml).
    #[arg(short = 'r', long = "robot", global = true, value_name = "ROBOT_FILE")]
    files: Vec<PathBuf>,
    #[command(subcommand)]
    command: CommandArgs,
}

impl Cli {
    /// Resolves paths, selections, and diagnostics policy once.
    pub(crate) fn resolve(mut self, invocation: &Path) -> Result<Invocation, Error> {
        self.normalize_executable_paths(invocation);
        let files = self
            .files
            .iter()
            .map(|file| explicit_path(invocation, file))
            .collect::<Vec<_>>();
        let resolved = match self.command {
            CommandArgs::Config(arguments) => Command::Config {
                json: arguments.json,
            },
            CommandArgs::Prepare(arguments) => Command::Prepare {
                options: arguments.options.into_options(Vec::new(), Vec::new()),
            },
            CommandArgs::Check(arguments) => Command::Check {
                options: arguments.into_options(Vec::new()),
            },
            CommandArgs::Build(arguments) => {
                let print_output_path = arguments.print_output_path;
                let watch = arguments.watch;
                let output = arguments
                    .output
                    .as_ref()
                    .map(|path| explicit_path(invocation, path));
                Command::Build {
                    options: arguments.into_options(),
                    output,
                    print_output_path,
                    watch,
                }
            }
            CommandArgs::Run(arguments) => Command::Run {
                options: arguments.into_options(),
            },
            CommandArgs::Test(arguments) => Command::Test {
                options: arguments.into_test_options(),
            },
            CommandArgs::Scenario(arguments) => {
                let mut options = arguments.options.into_options(Vec::new(), Vec::new());
                options.release |= arguments.release;
                Command::Scenario {
                    options,
                    file: explicit_path(invocation, &arguments.file),
                    desktop: arguments.desktop,
                }
            }
            CommandArgs::Simulation(arguments) => {
                let mut options = arguments.options.into_options(Vec::new(), Vec::new());
                options.release |= arguments.release;
                let build = arguments
                    .build
                    .as_ref()
                    .map(|path| explicit_path(invocation, path));
                if build.is_some() && !files.is_empty() {
                    return Err(Error::DeclarationCheck {
                        message: "simulation --build cannot be combined with --robot".into(),
                    });
                }
                Command::Simulation(SimulationInvocation {
                    options,
                    scene: explicit_path(invocation, &arguments.scene),
                    build,
                    headless: arguments.headless,
                    duration: arguments.duration,
                    paused: arguments.paused,
                })
            }
        };
        let diagnostics_json = match &resolved {
            Command::Config { .. } => false,
            command => command_options(command).is_some_and(CargoOptions::diagnostics_json),
        };
        Ok(Invocation {
            invocation: invocation.to_owned(),
            files,
            diagnostics_json,
            command: resolved,
        })
    }

    fn normalize_executable_paths(&mut self, invocation: &Path) {
        let controls = match &mut self.command {
            CommandArgs::Config(_) => return,
            CommandArgs::Check(arguments) => &mut arguments.options.build,
            CommandArgs::Test(arguments) => &mut arguments.options.build,
            CommandArgs::Prepare(arguments) => &mut arguments.options,
            CommandArgs::Build(arguments) => &mut arguments.options,
            CommandArgs::Run(arguments) => &mut arguments.options,
            CommandArgs::Simulation(arguments) => &mut arguments.options,
            CommandArgs::Scenario(arguments) => &mut arguments.options,
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
}

/// Returns the resolved options of one normalized command, when it has any.
fn command_options(command: &Command) -> Option<&CargoOptions> {
    match command {
        Command::Config { .. } => None,
        Command::Prepare { options }
        | Command::Check { options }
        | Command::Build { options, .. }
        | Command::Run { options }
        | Command::Test { options }
        | Command::Scenario { options, .. } => Some(options),
        Command::Simulation(simulation) => Some(&simulation.options),
    }
}

#[derive(Debug, Subcommand)]
enum CommandArgs {
    /// Print the composed, authored-validated configuration without compilation.
    Config(ConfigArgs),
    /// Prepare selected contracts and generated APIs.
    Prepare(PrepareArgs),
    /// Validate the project and selected APIs, then Cargo-check requested code.
    Check(CommandArgsSelectors),
    /// Build the complete robot and publish its complete runnable directory.
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
    duration: Option<Duration>,
    /// Open the desktop simulation paused.
    #[arg(long)]
    paused: bool,
}

fn positive_duration(value: &str) -> Result<Duration, String> {
    let duration = humantime::parse_duration(value).map_err(|error| error.to_string())?;
    if duration.is_zero() {
        return Err("duration must be positive (for example 10s or 250ms)".into());
    }
    Ok(duration)
}

#[derive(Debug, Args)]
struct CommandArgsSelectors {
    #[command(flatten)]
    options: CommonArgs,
    /// Additional arguments passed to Cargo after Phoxal's standard selectors.
    #[arg(last = true, allow_hyphen_values = true)]
    cargo_args: Vec<OsString>,
}

impl CommandArgsSelectors {
    fn into_options(self, trailing: Vec<OsString>) -> CargoOptions {
        self.options.into_options(self.cargo_args, trailing)
    }
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[command(flatten)]
    options: BuildControls,
    /// Cargo profile for the built executables (default: dev).
    #[arg(long, value_name = "PROFILE")]
    profile: Option<String>,
    /// Complete runnable-directory destination; defaults to the managed
    /// publication slot under Cargo's target directory.
    #[arg(long, short = 'o', value_name = "DIRECTORY")]
    output: Option<PathBuf>,
    /// Print the resolved publication path without building.
    #[arg(long)]
    print_output_path: bool,
    /// Rebuild saved source changes until Ctrl-C, preserving the last build.
    #[arg(long)]
    watch: bool,
}

impl BuildArgs {
    fn into_options(self) -> CargoOptions {
        let mut options = self.options.into_options(Vec::new(), Vec::new());
        options.profile = self.profile;
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
        options.release |= self.release;
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

impl TestArgs {
    fn into_test_options(self) -> CargoOptions {
        let mut cargo_args = Vec::new();
        if let Some(filter) = self.filter {
            cargo_args.push(filter);
        }
        if self.no_run {
            cargo_args.push(OsString::from("--no-run"));
        }
        self.options.into_options(cargo_args, self.test_args)
    }
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
        let mut options = CargoOptions {
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
        };
        // The one raw-argument boundary: selection-relevant flags in the
        // Cargo tail fold into their structured fields here, so every
        // downstream selection reads resolved fields only.
        options.normalize();
        options
    }
}

impl CommonArgs {
    fn into_options(self, cargo_args: Vec<OsString>, test_args: Vec<OsString>) -> CargoOptions {
        let mut options = self.build.into_options(cargo_args, test_args);
        // Merge over the already-normalized selection: a folded raw tail
        // selection wins; the structured fields fill the remainder.
        options.profile = options.profile.or(self.profile);
        options.release |= self.release;
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
        let root = Path::new("/robots/my rover");
        let scenarios = root.join("scenarios");
        assert_eq!(
            explicit_path(root, Path::new("scenarios/forward stop.rs")),
            explicit_path(&scenarios, Path::new("forward stop.rs"))
        );
        assert_eq!(
            explicit_path(root, Path::new("scene with spaces.xml")),
            root.join("scene with spaces.xml")
        );
        assert_eq!(
            explicit_path(&scenarios, Path::new("/existing build")),
            PathBuf::from("/existing build")
        );
        assert_eq!(
            explicit_path(&scenarios, Path::new("release robot.zip")),
            scenarios.join("release robot.zip")
        );
    }

    #[test]
    fn cargo_external_subcommand_and_direct_invocation_parse_identically() {
        for arguments in [
            vec!["cargo-phoxal", "phoxal", "check", "--locked"],
            vec!["cargo-phoxal", "check", "--locked"],
        ] {
            let parsed =
                Cli::try_parse_from(cargo_arguments(arguments.into_iter().map(OsString::from)))
                    .unwrap();
            assert!(matches!(parsed.command, CommandArgs::Check(_)));
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
        let CommandArgs::Simulation(arguments) = parsed.command else {
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
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Check { options, .. } = invocation.command else {
            panic!("the check command resolved as a different workflow");
        };
        assert_eq!(options.lock, LockMode::Locked);

        let parsed =
            Cli::try_parse_from(["cargo-phoxal", "test", "--frozen"]).expect("test command parses");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Test { options } = invocation.command else {
            panic!("the test command resolved as a different workflow");
        };
        assert_eq!(options.lock, LockMode::Frozen);
    }

    #[test]
    fn test_arguments_after_the_delimiter_stay_with_the_test_harness() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "test",
            "--offline",
            "--",
            "--exact",
            "brain_tests::starts_empty",
        ])
        .expect("test arguments parse");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Test { options } = invocation.command else {
            panic!("the test command resolved as a different workflow");
        };
        assert!(options.cargo_args.is_empty());
        assert_eq!(
            options.test_args,
            [
                OsString::from("--exact"),
                OsString::from("brain_tests::starts_empty")
            ]
        );
        assert!(!invocation.diagnostics_json);
    }

    #[test]
    fn raw_tail_message_format_resolves_once_into_the_policy() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "check",
            "--offline",
            "--",
            "--message-format",
            "json",
        ])
        .expect("raw Cargo tail parses");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Check { options, .. } = invocation.command else {
            panic!("the check command resolved as a different workflow")
        };
        assert_eq!(options.message_format.as_deref(), Some("json"));
        assert!(options.cargo_args.is_empty());
        assert!(invocation.diagnostics_json);

        // A raw tail message format wins over the structured spelling.
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "check",
            "--message-format",
            "human",
            "--",
            "--message-format=json-render-diagnostics",
        ])
        .expect("competing formats parse");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Check { options, .. } = invocation.command else {
            panic!("the check command resolved as a different workflow")
        };
        assert_eq!(
            options.message_format.as_deref(),
            Some("json-render-diagnostics")
        );
        assert!(invocation.diagnostics_json);
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
        assert!(matches!(parsed.command, CommandArgs::Build(_)));
    }

    #[test]
    fn whole_robot_commands_reject_cargo_selectors_and_raw_tails() {
        for command in ["prepare", "build", "run"] {
            for selector in ["--workspace", "--lib", "--examples", "--tests", "--benches"] {
                assert!(Cli::try_parse_from(["cargo-phoxal", command, selector]).is_err());
            }
            assert!(Cli::try_parse_from(["cargo-phoxal", command, "--", "--release"]).is_err());
        }
        assert!(Cli::try_parse_from(["cargo-phoxal", "build", "--release"]).is_err());
        assert!(Cli::try_parse_from(["cargo-phoxal", "run", "--output", "build"]).is_err());
        let parsed = Cli::try_parse_from(["cargo-phoxal", "run", "--release"])
            .expect("run release selection");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Run { options } = invocation.command else {
            panic!("wrong workflow")
        };
        assert!(options.release);
    }

    #[test]
    fn raw_tail_selection_flags_fold_once_at_the_boundary() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "check",
            "--offline",
            "--",
            "--profile",
            "release",
            "--features",
            "vision",
            "--jobs",
            "4",
        ])
        .expect("raw Cargo tail parses");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Check { options, .. } = invocation.command else {
            panic!("the check command resolved as a different workflow")
        };
        assert_eq!(options.profile.as_deref(), Some("release"));
        assert_eq!(options.features, ["vision".to_owned()]);
        assert_eq!(
            options.cargo_args,
            [OsString::from("--jobs"), OsString::from("4")]
        );

        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "check",
            "--profile",
            "dev",
            "--",
            "--profile=release",
        ])
        .expect("competing profiles parse");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Check { options, .. } = invocation.command else {
            panic!("the check command resolved as a different workflow")
        };
        // A raw tail selection wins over the structured spelling.
        assert_eq!(options.profile.as_deref(), Some("release"));
        assert!(options.cargo_args.is_empty());
    }

    #[test]
    fn simulation_build_refuses_selected_files_at_the_boundary() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "-r",
            "robot.yaml",
            "simulation",
            "scene.xml",
            "--build",
            "build",
        ])
        .expect("parses before resolution");
        let error = parsed
            .resolve(Path::new("/robot"))
            .expect_err("--build refuses file selections");
        assert!(
            format!("{error:#}").contains("cannot be combined with --robot"),
            "{error:#}"
        );
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
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Check { options, .. } = invocation.command else {
            panic!("the check command resolved as a different workflow")
        };
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
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Test { options } = invocation.command else {
            panic!("the test command resolved as a different workflow")
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
        assert!(options.diagnostics_json());
    }

    #[test]
    fn compile_only_test_requests_normalize_at_the_command_boundary() {
        let parsed = Cli::try_parse_from([
            "cargo-phoxal",
            "test",
            "--no-run",
            "--offline",
            "--",
            "--nocapture",
        ])
        .expect("compile-only test parses");
        let invocation = parsed.resolve(Path::new("/robot")).expect("resolves");
        let Command::Test { options } = invocation.command else {
            panic!("the test command resolved as a different workflow")
        };
        assert!(options.no_run, "--no-run normalizes into the typed field");
        assert!(
            options.cargo_args.is_empty(),
            "the folded flag does not stay in the raw tail"
        );
        assert_eq!(options.test_args, [OsString::from("--nocapture")]);
    }
}
