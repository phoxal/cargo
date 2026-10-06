use crate::project::error::Error;
use cargo_metadata::{CargoOpt, Message, Metadata, MetadataCommand};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitStatus};
/// The deployment target one command builds for.
///
/// An explicit `--target` argument or option wins; otherwise the command
/// targets the assembling host's concrete triple, never an ambiguous
/// `host` marker.
pub(crate) fn effective_target(options: &CargoOptions) -> String {
    cargo_arg_value(&options.cargo_args, "--target")
        .or_else(|| options.target.clone())
        .unwrap_or_else(phoxal::artifact::bundle::host_execution_target)
}
/// Reads one `--target <value>` style argument from a raw Cargo argument
/// list.
fn cargo_arg_value(arguments: &[std::ffi::OsString], name: &str) -> Option<String> {
    let arguments = arguments
        .iter()
        .map(|argument| argument.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let equals = format!("{name}=");
    let mut value = None;
    let mut index = 0;
    while index < arguments.len() {
        if arguments[index] == name && index + 1 < arguments.len() {
            value = Some(arguments[index + 1].clone());
            index += 2;
            continue;
        }
        if let Some(rest) = arguments[index].strip_prefix(&equals) {
            value = Some(rest.to_owned());
        }
        index += 1;
    }
    value
}
/// The normalized build selection, shared by acquisition and bundle grouping.
#[derive(Debug, serde::Serialize)]
pub(crate) struct BuildSelection {
    target: String,
    profile: String,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
}
impl BuildSelection {
    pub(crate) fn capture(options: &CargoOptions) -> Self {
        let profile = cargo_arg_value(&options.cargo_args, "--profile")
            .or_else(|| options.profile.clone())
            .unwrap_or_else(|| {
                if options.release || options.cargo_args.iter().any(|v| v == "--release") {
                    "release".into()
                } else {
                    "dev".into()
                }
            });
        let mut features = options.features.clone();
        let mut args = options.cargo_args.iter();
        while let Some(arg) = args.next() {
            let arg = arg.to_string_lossy();
            let value = if arg == "--features" {
                args.next().map(|v| v.to_string_lossy())
            } else {
                arg.strip_prefix("--features=")
                    .map(std::borrow::Cow::Borrowed)
            };
            if let Some(value) = value {
                features.extend(
                    value
                        .split([',', ' '])
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned),
                );
            }
        }
        features.sort();
        features.dedup();
        Self {
            target: effective_target(options),
            profile,
            features,
            all_features: options.all_features
                || options.cargo_args.iter().any(|v| v == "--all-features"),
            no_default_features: options.no_default_features
                || options
                    .cargo_args
                    .iter()
                    .any(|v| v == "--no-default-features"),
        }
    }
    /// Root feature flags apply to the brain, not independently selected applications.
    pub(crate) fn for_source(options: &CargoOptions) -> Self {
        let mut selection = Self::capture(options);
        selection.features.clear();
        selection.all_features = false;
        selection.no_default_features = false;
        selection
    }
    pub(crate) fn apply(&self, command: &mut Command) {
        command.args(["--target", &self.target, "--profile", &self.profile]);
        if !self.features.is_empty() {
            command.args(["--features", &self.features.join(",")]);
        }
        if self.all_features {
            command.arg("--all-features");
        }
        if self.no_default_features {
            command.arg("--no-default-features");
        }
    }
}
/// Constructs the shared registry/Git acquisition command for executable owners.
pub(crate) fn source_install_command(
    root: &Path,
    source: &phoxal::artifact::document::Source,
    binary: &str,
    installation: &Path,
    options: &CargoOptions,
) -> Command {
    use phoxal::artifact::document::Source;
    let mut command = Command::new(options.cargo_program());
    command
        .current_dir(root)
        .args(["install", "--locked", "--message-format", "json", "--root"])
        .arg(installation)
        .args(["--bin", binary]);
    BuildSelection::for_source(options).apply(&mut command);
    if options.offline || options.lock == LockMode::Frozen {
        command.arg("--offline");
    }
    match source {
        Source::Git(git) => {
            command.args(["--git", &git.url, "--rev", &git.rev, &git.name]);
        }
        Source::Path(path) => {
            command.arg("--path").arg(root.join(path));
        }
    }
    command
}
/// Cargo's lockfile policy for project preparation and commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LockMode {
    /// Let Cargo resolve within the authored requirements and update the lock.
    #[default]
    Unlocked,
    /// Require the existing lock to be current.
    Locked,
    /// Require the existing lock and forbid all network access.
    Frozen,
}
impl LockMode {
    /// Returns the flags used for this policy.
    #[must_use]
    pub const fn flags(self) -> &'static [&'static str] {
        match self {
            Self::Unlocked => &[],
            Self::Locked => &["--locked"],
            Self::Frozen => &["--frozen"],
        }
    }
}
/// Shared preparation and Cargo-command options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoOptions {
    /// Cargo executable selected by the caller.
    ///
    /// When absent, the `CARGO` environment variable is used and finally the
    /// `cargo` executable is resolved through `PATH`.  Capturing this once in
    /// the options keeps metadata, checks, builds, tests, and updates on the
    /// same Cargo installation.
    pub cargo_path: Option<std::path::PathBuf>,
    /// Lockfile policy.
    pub lock: LockMode,
    /// Forbid registry and Git network access without requiring `--frozen`.
    pub offline: bool,
    /// Optional target triple.
    pub target: Option<String>,
    /// Optional Cargo profile.
    pub profile: Option<String>,
    /// Features enabled on the root invocation.
    pub features: Vec<String>,
    /// Enable every root feature.
    pub all_features: bool,
    /// Disable default root features.
    pub no_default_features: bool,
    /// Build with Cargo's release profile.
    pub release: bool,
    /// Cargo compiler output format.
    pub message_format: Option<String>,
    /// Additional Cargo arguments before a test delimiter.
    pub cargo_args: Vec<OsString>,
    /// Arguments after Cargo's `--` test delimiter.
    pub test_args: Vec<OsString>,
    /// Cargo package and target-selection options supplied by the caller.
    pub selection: CargoSelection,
}
impl Default for CargoOptions {
    fn default() -> Self {
        Self {
            cargo_path: None,
            lock: LockMode::Unlocked,
            offline: false,
            target: None,
            profile: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            release: false,
            message_format: None,
            cargo_args: Vec::new(),
            test_args: Vec::new(),
            selection: CargoSelection::default(),
        }
    }
}
/// Cargo package and target selectors that remain meaningful at the
/// `cargo-phoxal` boundary.
///
/// The project compiler still selects the validated brain, service, and
/// driver targets for bundle assembly.  These selectors are forwarded to
/// source-development Cargo invocations so root, workspace, package, and
/// test workflows retain Cargo's documented semantics.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CargoSelection {
    /// Restrict Cargo to the workspace rather than the root package.
    pub workspace: bool,
    /// Explicit package specifications, in caller order.
    pub packages: Vec<String>,
    /// Workspace packages excluded from a workspace selection.
    pub excludes: Vec<String>,
    /// Select every target in the selected package set.
    pub all_targets: bool,
    /// Select the package library target.
    pub lib: bool,
    /// Select all binary targets.
    pub bins: bool,
    /// Select named binary targets.
    pub binaries: Vec<String>,
    /// Select all example targets.
    pub examples: bool,
    /// Select named example targets.
    pub examples_named: Vec<String>,
    /// Select all integration tests.
    pub tests: bool,
    /// Select named integration tests.
    pub tests_named: Vec<String>,
    /// Select all benchmarks.
    pub benches: bool,
    /// Select named benchmarks.
    pub benches_named: Vec<String>,
}
impl CargoSelection {
    fn append_to(&self, command: &mut Command) {
        if self.workspace {
            command.arg("--workspace");
        }
        for package in &self.packages {
            command.args(["--package", package]);
        }
        for exclude in &self.excludes {
            command.args(["--exclude", exclude]);
        }
        if self.all_targets {
            command.arg("--all-targets");
        }
        if self.lib {
            command.arg("--lib");
        }
        if self.bins {
            command.arg("--bins");
        }
        for binary in &self.binaries {
            command.args(["--bin", binary]);
        }
        if self.examples {
            command.arg("--examples");
        }
        for example in &self.examples_named {
            command.args(["--example", example]);
        }
        if self.tests {
            command.arg("--tests");
        }
        for test in &self.tests_named {
            command.args(["--test", test]);
        }
        if self.benches {
            command.arg("--benches");
        }
        for bench in &self.benches_named {
            command.args(["--bench", bench]);
        }
    }
    fn is_empty(&self) -> bool {
        !self.workspace
            && self.packages.is_empty()
            && self.excludes.is_empty()
            && !self.all_targets
            && !self.lib
            && !self.bins
            && self.binaries.is_empty()
            && !self.examples
            && self.examples_named.is_empty()
            && !self.tests
            && self.tests_named.is_empty()
            && !self.benches
            && self.benches_named.is_empty()
    }
}
impl CargoOptions {
    /// Validates flags before any Cargo process or manifest mutation occurs.
    pub fn validate(&self) -> Result<(), Error> {
        let target = effective_target(self);
        if !(target.ends_with("-apple-darwin") || target.contains("-unknown-linux-")) {
            return Err(Error::InvalidOptions {
                message: format!(
                    "unsupported target {target}: Phoxal supports Linux and macOS only"
                ),
            });
        }
        if self
            .features
            .iter()
            .any(|feature| feature.trim().is_empty())
        {
            return Err(Error::InvalidOptions {
                message: "--features cannot contain an empty feature name".to_owned(),
            });
        }
        Ok(())
    }
    pub(crate) fn append_common(
        &self,
        command: &mut Command,
        include_message_format: bool,
        include_selection: bool,
    ) {
        for flag in self.lock.flags() {
            command.arg(flag);
        }
        if self.offline {
            command.arg("--offline");
        }
        if let Some(target) = &self.target {
            command.args(["--target", target]);
        }
        if let Some(profile) = &self.profile {
            command.args(["--profile", profile]);
        }
        if self.all_features {
            command.arg("--all-features");
        }
        if self.no_default_features {
            command.arg("--no-default-features");
        }
        if !self.features.is_empty() {
            command.args(["--features", &self.features.join(",")]);
        }
        if self.release {
            command.arg("--release");
        }
        if include_message_format && let Some(message_format) = &self.message_format {
            command.args(["--message-format", message_format]);
        }
        if include_selection {
            self.selection.append_to(command);
        }
        command.args(&self.cargo_args);
    }
    pub(crate) fn cargo_program(&self) -> std::path::PathBuf {
        self.cargo_path
            .clone()
            .or_else(|| std::env::var_os("CARGO").map(std::path::PathBuf::from))
            .unwrap_or_else(|| std::path::PathBuf::from("cargo"))
    }
}
/// The supported source-development Cargo operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CargoOperation {
    /// Validate the root brain and selected executable targets.
    Check,
    /// Build the root brain and selected executable targets.
    Build,
    /// Run tests selected by Cargo for the root robot package.
    Test,
}
impl CargoOperation {
    /// The Cargo subcommand spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Build => "build",
            Self::Test => "test",
        }
    }
}
/// Captured output from one successful Cargo invocation.
#[derive(Debug)]
pub struct CargoOutput {
    /// Captured standard output.
    pub stdout: Vec<u8>,
    /// Captured standard error.
    pub stderr: Vec<u8>,
}
/// Invokes Cargo metadata from an optional isolated source tree.
pub(crate) fn load_metadata_at(
    manifest: &Path,
    current_dir: &Path,
    target_dir: Option<&Path>,
    options: &CargoOptions,
) -> Result<Metadata, Error> {
    options.validate()?;
    let mut command = MetadataCommand::new();
    command.cargo_path(options.cargo_program());
    command
        .manifest_path(manifest)
        .current_dir(current_dir)
        .features(CargoOpt::SomeFeatures(options.features.clone()));
    if let Some(target_dir) = target_dir {
        command.env("CARGO_TARGET_DIR", target_dir);
    }
    if options.all_features {
        command.features(CargoOpt::AllFeatures);
    }
    if options.no_default_features {
        command.features(CargoOpt::NoDefaultFeatures);
    }
    let mut extra = options
        .lock
        .flags()
        .iter()
        .map(|flag| (*flag).to_owned())
        .collect::<Vec<_>>();
    if options.offline {
        extra.push("--offline".to_owned());
    }
    if let Some(target) = &options.target {
        extra.extend(["--filter-platform".to_owned(), target.clone()]);
    }
    command.other_options(extra);
    command.exec().map_err(|source| Error::CargoMetadata {
        manifest: manifest.to_owned(),
        source,
    })
}
/// Runs the selected target set from the prepared project's root graph.
pub(crate) fn run(
    prepared: &crate::PreparedProject,
    operation: CargoOperation,
    options: &CargoOptions,
) -> Result<Vec<CargoOutput>, Error> {
    run_with_env(prepared, operation, options, &[])
}
/// Run Cargo with immutable environment entries inherited by test binaries.
pub(crate) fn run_with_env(
    prepared: &crate::PreparedProject,
    operation: CargoOperation,
    options: &CargoOptions,
    environment: &[(OsString, OsString)],
) -> Result<Vec<CargoOutput>, Error> {
    options.validate()?;
    let mut outputs = Vec::new();
    match operation {
        CargoOperation::Test => {
            let root = prepared.cargo_root_package();
            let mut command = command_for(prepared, operation, options, true, true);
            command.envs(environment.iter().cloned());
            if options.selection.is_empty() {
                command.args(["--package", root.id.to_string().as_str()]);
            }
            command.args([
                "--manifest-path",
                &prepared.cargo_manifest_path().display().to_string(),
            ]);
            command.args(["--"]);
            command.args(&options.test_args);
            outputs.push(run_command(command, operation)?);
        }
        CargoOperation::Check | CargoOperation::Build => {
            if !options.selection.is_empty() {
                let mut command = command_for(prepared, operation, options, true, true);
                command.envs(environment.iter().cloned());
                command.args([
                    "--manifest-path",
                    &prepared.cargo_manifest_path().display().to_string(),
                ]);
                outputs.push(run_command(command, operation)?);
                return Ok(outputs);
            }
            let mut command = command_for(prepared, operation, options, true, false);
            command.envs(environment.iter().cloned());
            command.args([
                "--manifest-path",
                &prepared.cargo_manifest_path().display().to_string(),
            ]);
            append_target_selection(&mut command, prepared, &prepared.cargo_sources().brain);
            outputs.push(run_command(command, operation)?);
        }
    }
    Ok(outputs)
}
fn command_for(
    prepared: &crate::PreparedProject,
    operation: CargoOperation,
    options: &CargoOptions,
    include_message_format: bool,
    include_selection: bool,
) -> Command {
    let mut command = Command::new(options.cargo_program());
    command.current_dir(prepared.cargo_workdir());
    command.arg(operation.as_str());
    options.append_common(&mut command, include_message_format, include_selection);
    command
}
/// Builds one selected executable while retaining Cargo's machine-readable
/// artifact records for bundle assembly.
pub(crate) fn build_target(
    prepared: &crate::PreparedProject,
    target: &crate::SelectedTarget,
    options: &CargoOptions,
) -> Result<CargoOutput, Error> {
    options.validate()?;
    if let Some(executable) = &target.executable {
        if !executable.is_file() {
            return Err(Error::ArtifactCapture {
                package: target.package.clone(),
                target: target.target.clone(),
                message: "prepared binary is missing; run `cargo phoxal prepare`".to_owned(),
            });
        }
        return Ok(CargoOutput {
            stdout: Vec::new(),
            stderr: Vec::new(),
        });
    }
    if let Some(manifest) = &target.manifest_path {
        let workdir = manifest.parent().ok_or_else(|| Error::ArtifactCapture {
            package: target.package.clone(),
            target: target.target.clone(),
            message: "local participant manifest has no parent".to_owned(),
        })?;
        let mut command = Command::new(options.cargo_program());
        command
            .current_dir(workdir)
            .args(["build", "--manifest-path"]);
        command.arg(manifest);
        let mut local_options = options.clone();
        local_options.features.clear();
        local_options.all_features = false;
        local_options.no_default_features = false;
        local_options.cargo_args.clear();
        local_options.append_common(&mut command, false, false);
        command.args(["--package", &target.package, "--bin", &target.target]);
        command.args(["--message-format", "json-render-diagnostics"]);
        return run_command(command, CargoOperation::Build);
    }
    let mut command = command_for(prepared, CargoOperation::Build, options, false, false);
    command.args([
        "--manifest-path",
        &prepared.cargo_manifest_path().display().to_string(),
    ]);
    append_target_selection(&mut command, prepared, target);
    command.args([
        "--message-format",
        options
            .message_format
            .as_deref()
            .filter(|format| format.starts_with("json"))
            .unwrap_or("json-render-diagnostics"),
    ]);
    run_command(command, CargoOperation::Build)
}
fn append_target_selection(
    command: &mut Command,
    prepared: &crate::PreparedProject,
    target: &crate::SelectedTarget,
) {
    if target.package_id != prepared.cargo_root_package().id.to_string() {
        command.args([
            "--package",
            prepared.cargo_root_package().id.to_string().as_str(),
        ]);
    }
    command.args(["--package", target.package_id.as_str()]);
    command.args(["--bin", target.target.as_str()]);
    if !target.required_features.is_empty() {
        let mut features = collect_existing_features(command);
        for feature in &target.required_features {
            if !features.iter().any(|existing| existing == feature) {
                features.push(feature.clone());
            }
        }
        command.args(["--features", &features.join(",")]);
    }
}
/// Pull every comma-separated feature already present on `command` so the
/// auto-merge in `append_target_selection` does not duplicate an explicit
/// caller list.
fn collect_existing_features(command: &Command) -> Vec<String> {
    let arguments: Vec<std::ffi::OsString> = command
        .get_args()
        .map(|argument| argument.to_os_string())
        .collect();
    let mut features = Vec::new();
    let mut index = 0;
    while let Some(argument) = arguments.get(index) {
        let value = argument.to_string_lossy();
        if value == "--features" {
            if let Some(next) = arguments.get(index + 1) {
                for piece in next.to_string_lossy().split(',') {
                    let trimmed = piece.trim();
                    if !trimmed.is_empty() && !features.iter().any(|f| f == trimmed) {
                        features.push(trimmed.to_owned());
                    }
                }
                index += 2;
                continue;
            }
        } else if let Some(rest) = value.strip_prefix("--features=") {
            for piece in rest.split(',') {
                let trimmed = piece.trim();
                if !trimmed.is_empty() && !features.iter().any(|f| f == trimmed) {
                    features.push(trimmed.to_owned());
                }
            }
        }
        index += 1;
    }
    features
}
/// Extracts one selected executable from Cargo's JSON compiler-artifact stream.
pub(crate) fn artifact_path(
    stdout: &[u8],
    target: &crate::SelectedTarget,
) -> Result<std::path::PathBuf, Error> {
    if let Some(executable) = &target.executable {
        return Ok(executable.clone());
    }
    let mut executable = None;
    for line in stdout.split(|byte| *byte == b'\n') {
        if line.is_empty() {
            continue;
        }
        let message =
            serde_json::from_slice::<Message>(line).map_err(|error| Error::ArtifactCapture {
                package: target.package.clone(),
                target: target.target.clone(),
                message: format!("invalid Cargo JSON message: {error}"),
            })?;
        if let Message::CompilerArtifact(artifact) = message
            && artifact.package_id.to_string() == target.package_id
            && artifact.target.name == target.target
            && artifact.target.is_bin()
        {
            executable = artifact.executable.map(|path| path.into_std_path_buf());
        }
    }
    executable.ok_or_else(|| Error::ArtifactCapture {
        package: target.package.clone(),
        target: target.target.clone(),
        message: "no compiler-artifact executable matched the selected package".to_owned(),
    })
}
pub(crate) fn run_command(
    mut command: Command,
    operation: CargoOperation,
) -> Result<CargoOutput, Error> {
    let output = command.output().map_err(|source| Error::CargoSpawn {
        operation: operation.as_str().to_owned(),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::CargoCommand {
            operation: operation.as_str().to_owned(),
            status: status_string(output.status),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    Ok(CargoOutput {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}
fn status_string(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_owned())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collect_existing_features_reads_space_and_equals_forms() {
        let mut command = Command::new("cargo");
        command.args(["--features", "scenario,imu"]);
        command.args(["--features=vision"]);
        let features = collect_existing_features(&command);
        assert_eq!(
            features,
            vec!["scenario".to_owned(), "imu".to_owned(), "vision".to_owned()]
        );
    }
    #[test]
    fn collect_existing_features_dedupes_overlapping_entries() {
        let mut command = Command::new("cargo");
        command.args(["--features", "scenario,imu"]);
        command.args(["--features=scenario"]);
        let features = collect_existing_features(&command);
        assert_eq!(features, vec!["scenario".to_owned(), "imu".to_owned()]);
    }
}
