use crate::project::error::Error;
use cargo_metadata::{CargoOpt, Message, Metadata, MetadataCommand};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, ExitStatus};
/// The deployment target one command builds for.
///
/// An explicit `--target` selection wins; otherwise the command targets
/// the assembling host's concrete triple, never an ambiguous `host`
/// marker. Selection-relevant raw argument tails were already folded into
/// the structured selection once at the invocation boundary.
pub(crate) fn effective_target(options: &CargoOptions) -> String {
    options
        .target
        .clone()
        .unwrap_or_else(phoxal::artifact::bundle::host_execution_target)
}
/// The Cargo profile one command builds with.
///
/// An explicit `--profile` selection wins; `--release` selects Cargo's
/// `release` profile; the default is Cargo's development profile.
pub(crate) fn effective_profile(options: &CargoOptions) -> String {
    options
        .profile
        .clone()
        .unwrap_or_else(|| if options.release { "release" } else { "dev" }.to_owned())
}
/// The normalized build selection, shared by acquisition and bundle grouping.
///
/// Selection inputs are parsed once at the invocation boundary; this record
/// reads only resolved fields, so installation-store and artifact identities
/// never reparse a raw Cargo argument tail.
#[derive(Debug, serde::Serialize)]
pub(crate) struct BuildSelection {
    target: String,
    profile: String,
    features: Vec<String>,
    all_features: bool,
    no_default_features: bool,
}
impl BuildSelection {
    /// The root robot graph's selection.
    ///
    /// Root feature flags apply to the brain, not independently selected
    /// applications.
    pub(crate) fn root(options: &CargoOptions) -> Self {
        let mut features = options.features.clone();
        features.sort();
        features.dedup();
        Self {
            target: effective_target(options),
            profile: effective_profile(options),
            features,
            all_features: options.all_features,
            no_default_features: options.no_default_features,
        }
    }
    /// The selection for independently built participants and applications:
    /// the same target and profile without root feature state.
    pub(crate) fn participant(options: &CargoOptions) -> Self {
        Self {
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            ..Self::root(options)
        }
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
///
/// Pinned installations own a locked acquisition graph and selection cache,
/// separate from local source-development overlays. Do not apply one-off
/// local `configs` to this context or its captured package metadata.
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
    BuildSelection::participant(options).apply(&mut command);
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
/// Constructs one local package build command for an independently selected
/// participant, driver, or application package.
///
/// The build runs in the caller's effective invocation directory, which is
/// where Cargo discovers `.cargo` configuration and toolchain files; the
/// participant and driver builds use the manifest's directory, while the
/// supervisor application builds from the robot root. The command carries
/// the participant build selection (effective target and profile, no root
/// features) and the resolved lock and offline policy, and selects exactly
/// one package and binary, so the compiler-artifact stream admits one
/// executable.
///
/// Cargo identifies a compilation unit by package name, version, target,
/// profile, and settings, and compares path sources only through recorded
/// file modification times. Two independently selected local packages with
/// the same name and version can therefore satisfy one fingerprint in a
/// shared target directory and publish the other source's executable as
/// fresh. Each selected source builds below its own subdirectory of the
/// resolved target directory: distinct sources never share one unit, while
/// the same source keeps Cargo's ordinary incremental reuse.
pub(crate) fn local_package_build(
    invocation_directory: &Path,
    manifest_path: &Path,
    package: &str,
    binary: &str,
    message_format: &str,
    options: &CargoOptions,
) -> Result<Command, Error> {
    let directories =
        selected_source_compiler_directories(manifest_path, invocation_directory, options)?;
    let mut command = Command::new(options.cargo_program());
    command
        .current_dir(invocation_directory)
        .args(["build", "--manifest-path"])
        .arg(manifest_path);
    for config in &options.configs {
        command.arg("--config").arg(config);
    }
    directories.apply(&mut command);
    BuildSelection::participant(options).apply(&mut command);
    for flag in options.lock.flags() {
        command.arg(flag);
    }
    if options.offline {
        command.arg("--offline");
    }
    command.args(["--package", package, "--bin", binary]);
    command.args(["--message-format", message_format]);
    Ok(command)
}

/// The per-selected-source compiler directories below Cargo's resolved
/// final-output and intermediate-cache roots.
///
/// The selected source's canonical manifest path keys the subdirectory, so
/// equal selections share one warm Cargo unit and distinct selections never
/// collide, whatever target directory the caller shares. The directory is
/// passed as compiler-only overrides, preserving the ordinary
/// Cargo environment used by SDK prepared-input discovery. Nothing is created
/// here: Cargo builds into the directory it is given.
fn selected_source_compiler_directories(
    manifest_path: &Path,
    invocation_directory: &Path,
    options: &CargoOptions,
) -> Result<CompilerDirectories, Error> {
    use sha2::{Digest as _, Sha256};
    let resolved = compiler_directory_metadata(manifest_path, invocation_directory, options)?;
    let canonical = manifest_path
        .canonicalize()
        .map_err(|source| Error::ArtifactFile {
            path: manifest_path.to_owned(),
            source,
        })?;
    let digest = Sha256::digest(canonical.as_os_str().as_encoded_bytes());
    let namespace = |base: cargo_metadata::camino::Utf8PathBuf| {
        base.into_std_path_buf()
            .join("phoxal")
            .join(".selected")
            .join(format!("{digest:x}"))
    };
    Ok(CompilerDirectories {
        target: namespace(resolved.target_directory),
        build: resolved.build_directory.map(namespace),
    })
}

struct CompilerDirectories {
    target: std::path::PathBuf,
    build: Option<std::path::PathBuf>,
}
impl CompilerDirectories {
    fn apply(&self, command: &mut Command) {
        command.arg("--target-dir").arg(&self.target);
        // Cargo itself reports whether separate intermediate storage exists.
        // Older Cargo keeps intermediates in --target-dir and receives no new
        // option. The owner override follows caller overlays, so they cannot
        // merge distinct selected sources back into one compiler cache.
        if let Some(build) = &self.build {
            command
                .arg("--config")
                .arg(path_config("build.build-dir", build));
        }
    }
}
fn path_config(key: &str, path: &Path) -> String {
    format!(
        "{key}={}",
        toml::Value::String(path.to_string_lossy().into_owned())
    )
}
/// Pinned installs already own a complete selection cache, separate from
/// source-development overlays. Keep both Cargo cache locations inside it.
pub(crate) fn installation_cache(command: &mut Command, directory: &Path) {
    command
        .env("CARGO_TARGET_DIR", directory)
        .env("CARGO_BUILD_BUILD_DIR", directory);
}

/// Splits one raw `--features` value the way Cargo does: on commas and
/// spaces, ignoring empty pieces.
fn split_feature_names(value: &str) -> impl Iterator<Item = String> + '_ {
    value
        .split([',', ' '])
        .filter(|piece| !piece.is_empty())
        .map(str::to_owned)
}
/// Reads the attached value of one short Cargo flag spelling, such as
/// `-Fvision` or `-F=vision`.
fn short_attached(argument: &str, flag: &str) -> Option<String> {
    argument
        .strip_prefix(flag)
        .filter(|_| argument.len() > flag.len())
        .map(|value| value.strip_prefix('=').unwrap_or(value).to_owned())
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
    /// One-off compiler output root; prepared inputs keep ordinary configuration.
    pub compiler_target_dir: Option<std::path::PathBuf>,
    /// One-off Cargo configuration overrides, in caller order.
    pub configs: Vec<OsString>,
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
    /// Compile test executables without running them (`--no-run`).
    pub no_run: bool,
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
            compiler_target_dir: None,
            configs: Vec::new(),
            lock: LockMode::Unlocked,
            offline: false,
            target: None,
            profile: None,
            features: Vec::new(),
            all_features: false,
            no_default_features: false,
            release: false,
            no_run: false,
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
    /// Folds the selection- and policy-relevant flags of a raw Cargo
    /// argument tail into their structured fields, once, at the invocation
    /// boundary.
    ///
    /// Downstream selection, command construction, lock and offline policy,
    /// diagnostics policy, and identity recording read only the resolved
    /// fields. A raw `--target`, `--profile`, or `--message-format` wins over
    /// an explicit structured selection, matching Cargo's last-flag rule for
    /// the forwarded tail. Boolean flags
    /// combine, and the strongest requested lockfile policy wins, so a raw
    /// tail can tighten but never relax the structured policy. Feature
    /// values split on commas and spaces and ignore empty pieces, exactly as
    /// Cargo does, including the `-F` short spelling. Every other argument
    /// stays in the tail in caller order.
    pub(crate) fn normalize(&mut self) {
        let mut arguments = std::mem::take(&mut self.cargo_args).into_iter();
        let mut retained = Vec::new();
        let mut target = None;
        let mut profile = None;
        let mut features = Vec::<String>::new();
        let mut locked = false;
        let mut frozen = false;
        let mut message_format = None;
        while let Some(argument) = arguments.next() {
            let text = argument.to_string_lossy().into_owned();
            let inline = |name: &str| text.strip_prefix(&format!("{name}=")).map(str::to_owned);
            if let Some(value) = inline("--target") {
                target = Some(value);
            } else if let Some(value) = inline("--target-dir") {
                self.compiler_target_dir = Some(value.into());
            } else if let Some(value) = inline("--config") {
                self.configs.push(value.into());
            } else if text == "--target-dir"
                && let Some(value) = arguments.next()
            {
                self.compiler_target_dir = Some(value.into());
            } else if text == "--config"
                && let Some(value) = arguments.next()
            {
                self.configs.push(value);
            } else if let Some(value) = inline("--profile") {
                profile = Some(value);
            } else if let Some(value) = inline("--features") {
                features.extend(split_feature_names(&value));
            } else if let Some(value) = inline("--message-format") {
                message_format = Some(value);
            } else if let Some(value) = short_attached(&text, "-F") {
                features.extend(split_feature_names(&value));
            } else if text == "--target"
                && let Some(value) = arguments.next()
            {
                target = Some(value.to_string_lossy().into_owned());
            } else if text == "--profile"
                && let Some(value) = arguments.next()
            {
                profile = Some(value.to_string_lossy().into_owned());
            } else if (text == "--features" || text == "-F")
                && let Some(value) = arguments.next()
            {
                features.extend(split_feature_names(&value.to_string_lossy()));
            } else if text == "--message-format"
                && let Some(value) = arguments.next()
            {
                message_format = Some(value.to_string_lossy().into_owned());
            } else if text == "--release" || text == "-r" {
                self.release = true;
            } else if text == "--no-run" {
                self.no_run = true;
            } else if text == "--all-features" {
                self.all_features = true;
            } else if text == "--no-default-features" {
                self.no_default_features = true;
            } else if text == "--locked" {
                locked = true;
            } else if text == "--frozen" {
                frozen = true;
            } else if text == "--offline" {
                self.offline = true;
            } else {
                retained.push(argument);
            }
        }
        self.cargo_args = retained;
        self.target = target.or_else(|| self.target.take());
        self.profile = profile.or_else(|| self.profile.take());
        self.features.extend(features);
        self.message_format = message_format.or_else(|| self.message_format.take());
        // The strongest requested lockfile policy wins, so a raw tail can
        // tighten but never silently relax the structured selection.
        self.lock = if frozen || self.lock == LockMode::Frozen {
            LockMode::Frozen
        } else if locked || self.lock == LockMode::Locked {
            LockMode::Locked
        } else {
            LockMode::Unlocked
        };
    }
    pub(crate) fn append_common(
        &self,
        command: &mut Command,
        include_message_format: bool,
        include_selection: bool,
    ) {
        for config in &self.configs {
            command.arg("--config").arg(config);
        }
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
    /// Whether compiler output and Phoxal diagnostics use JSON.
    ///
    /// The policy derives from the resolved message format only; no caller
    /// reparses a raw Cargo tail.
    pub(crate) fn diagnostics_json(&self) -> bool {
        self.message_format
            .as_deref()
            .is_some_and(|format| format.starts_with("json"))
    }
    pub(crate) fn cargo_program(&self) -> std::path::PathBuf {
        self.cargo_path
            .clone()
            .or_else(|| std::env::var_os("CARGO").map(std::path::PathBuf::from))
            .unwrap_or_else(|| std::path::PathBuf::from("cargo"))
    }
    /// Cargo metadata must resolve the same configuration overlays as local
    /// compilation, in caller order so Cargo owns their merge precedence.
    fn metadata_config_args(&self) -> impl Iterator<Item = String> + '_ {
        self.configs
            .iter()
            .flat_map(|config| ["--config".to_owned(), config.to_string_lossy().into_owned()])
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
    extra.extend(options.metadata_config_args());
    command.other_options(extra);
    metadata_output(&command).map_err(|source| Error::CargoMetadata {
        manifest: manifest.to_owned(),
        source,
    })
}
/// Executes the existing metadata command through watch-owned cancellation.
pub(crate) fn metadata_output(
    command: &MetadataCommand,
) -> Result<Metadata, cargo_metadata::Error> {
    let output = crate::owned_process::output(&mut command.cargo_command())?;
    if !output.status.success() {
        crate::owned_process::retain_cancelled(&[], &output.stderr);
        return Err(cargo_metadata::Error::CargoMetadata {
            stderr: String::from_utf8(output.stderr)?,
        });
    }
    let stdout = std::str::from_utf8(&output.stdout)?;
    let json = stdout
        .lines()
        .find(|line| line.starts_with('{'))
        .ok_or(cargo_metadata::Error::NoJson)?;
    MetadataCommand::parse(json)
}

/// Resolves the effective toolchain identity for one invocation.
///
/// The selected Cargo executable reports its own version line; a missing or
/// unexecutable selection fails here, before any path is claimed. The
/// identity keys publication slots so distinct effective toolchains never
/// share one slot, and a future watch can pin it for restart detection.
pub(crate) fn effective_toolchain(
    options: &CargoOptions,
    working_directory: &Path,
) -> Result<String, Error> {
    options.validate()?;
    let cargo = version_line(working_directory, &options.cargo_program())?;
    let rustc = version_line(
        working_directory,
        &effective_rustc(options, working_directory),
    )?;
    Ok(format!("{cargo}\n{rustc}"))
}

/// One executable's first `--version` line, resolved in the same working
/// directory the Cargo owner uses for metadata and builds, so directory
/// toolchain overrides apply exactly as they do to real compilation.
fn version_line(working_directory: &Path, program: &Path) -> Result<String, Error> {
    let output = crate::owned_process::output(
        Command::new(program)
            .arg("--version")
            .current_dir(working_directory),
    )
    .map_err(|source| Error::CargoSpawn {
        operation: "resolve the toolchain".to_owned(),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::CargoCommand {
            operation: "resolve the toolchain".to_owned(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        });
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let version = version.lines().next().unwrap_or_default().trim();
    if version.is_empty() {
        return Err(Error::CargoCommand {
            operation: "resolve the toolchain".to_owned(),
            status: "no version line".to_owned(),
            stdout: String::new(),
            stderr: String::new(),
        });
    }
    Ok(version.to_owned())
}

/// The Rust compiler Cargo itself would use: the supported `RUSTC` and
/// `CARGO_BUILD_RUSTC` selections over the `build.rustc` configuration, or
/// the compiler beside the selected Cargo (a toolchain layout), or the
/// `rustc` on `PATH`.
///
/// Configuration resolution belongs to Cargo's own maintained resolver
/// ([`cargo_config2`]): the hierarchical merge walking up from the working
/// directory with the Cargo-home base layer, the documented environment
/// overlay precedence, the legacy extensionless configuration file winning
/// over `config.toml`, and program-path handling where bare executable
/// names resolve through `PATH` while relative paths anchor to their
/// defining configuration file. Nothing compiles and no configuration is
/// rewritten.
fn effective_rustc(options: &CargoOptions, working_directory: &Path) -> std::path::PathBuf {
    let configured = cargo_config2::Config::load_with_cwd(working_directory)
        .ok()
        .and_then(|config| config.build.rustc);
    if let Some(rustc) = configured {
        return rustc;
    }
    // No explicit selection: the compiler beside the selected Cargo
    // (a toolchain layout), or the `rustc` on `PATH`.
    if let Some(directory) = options.cargo_program().parent()
        && directory.join("rustc").is_file()
    {
        return directory.join("rustc");
    }
    std::path::PathBuf::from("rustc")
}

/// Resolves Cargo's target directory without dependency acquisition,
/// compilation, or artifact creation.
///
/// Resolution runs in the robot root with the invocation's lock and offline
/// policy: the same deliberate invocation context actual builds use,
/// because relative configured and environment target directories resolve
/// against the process directory, not the manifest.
pub(crate) fn resolve_target_directory(
    manifest: &Path,
    current_dir: &Path,
    options: &CargoOptions,
) -> Result<std::path::PathBuf, Error> {
    Ok(compiler_directory_metadata(manifest, current_dir, options)?
        .target_directory
        .into_std_path_buf())
}
/// Cargo resolves the complete directory configuration, including templates
/// and ordered overlays. No second configuration hierarchy is interpreted.
pub(crate) fn compiler_directory_metadata(
    manifest: &Path,
    current_dir: &Path,
    options: &CargoOptions,
) -> Result<Metadata, Error> {
    options.validate()?;
    let mut command = MetadataCommand::new();
    command
        .cargo_path(options.cargo_program())
        .manifest_path(manifest)
        .current_dir(current_dir)
        .no_deps();
    let mut extra = options
        .lock
        .flags()
        .iter()
        .map(|flag| (*flag).to_owned())
        .collect::<Vec<_>>();
    if options.offline {
        extra.push("--offline".to_owned());
    }
    extra.extend(options.metadata_config_args());
    if let Some(directory) = &options.compiler_target_dir {
        // Metadata has no --target-dir flag. Cargo's dedicated target-dir
        // environment override has the same precedence over configuration.
        // Set it on metadata only, so default intermediate storage follows
        // the compiler flag without changing SDK prepared-input discovery.
        command.env("CARGO_TARGET_DIR", current_dir.join(directory));
    }
    command.other_options(extra);
    let metadata = metadata_output(&command).map_err(|source| Error::CargoMetadata {
        manifest: manifest.to_owned(),
        source,
    })?;
    Ok(metadata)
}

/// Runs the selected target set from the prepared project's root graph.
pub(crate) fn run(
    prepared: &crate::project::PreparedProject,
    operation: CargoOperation,
    options: &CargoOptions,
) -> Result<Vec<CargoOutput>, Error> {
    run_with_env(prepared, operation, options, &[])
}
/// Run Cargo with immutable environment entries inherited by test binaries.
pub(crate) fn run_with_env(
    prepared: &crate::project::PreparedProject,
    operation: CargoOperation,
    options: &CargoOptions,
    environment: &[(OsString, OsString)],
) -> Result<Vec<CargoOutput>, Error> {
    options.validate()?;
    let mut outputs = Vec::new();
    match operation {
        CargoOperation::Test => {
            let root = prepared.cargo_root_package();
            let mut command = command_for(prepared, operation, options, true, true)?;
            command.envs(environment.iter().cloned());
            if options.no_run {
                command.arg("--no-run");
            }
            if options.selection.is_empty() {
                command.args(["--package", root.id.to_string().as_str()]);
            }
            command.args([
                "--manifest-path",
                &prepared.cargo_manifest_path().display().to_string(),
            ]);
            command.args(["--"]);
            command.args(&options.test_args);
            outputs.push(run_command(command, operation, options)?);
        }
        CargoOperation::Check | CargoOperation::Build => {
            if !options.selection.is_empty() {
                let mut command = command_for(prepared, operation, options, true, true)?;
                command.envs(environment.iter().cloned());
                command.args([
                    "--manifest-path",
                    &prepared.cargo_manifest_path().display().to_string(),
                ]);
                outputs.push(run_command(command, operation, options)?);
                return Ok(outputs);
            }
            let mut command = command_for(prepared, operation, options, true, false)?;
            command.envs(environment.iter().cloned());
            command.args([
                "--manifest-path",
                &prepared.cargo_manifest_path().display().to_string(),
            ]);
            append_target_selection(
                &mut command,
                prepared,
                &prepared.cargo_sources().brain,
                &options.features,
            );
            outputs.push(run_command(command, operation, options)?);
        }
    }
    Ok(outputs)
}
fn command_for(
    prepared: &crate::project::PreparedProject,
    operation: CargoOperation,
    options: &CargoOptions,
    include_message_format: bool,
    include_selection: bool,
) -> Result<Command, Error> {
    let mut command = Command::new(options.cargo_program());
    command.current_dir(prepared.cargo_workdir());
    command.arg(operation.as_str());
    options.append_common(&mut command, include_message_format, include_selection);
    selected_source_compiler_directories(
        prepared.cargo_manifest_path(),
        prepared.cargo_workdir(),
        options,
    )?
    .apply(&mut command);
    Ok(command)
}
/// Builds one selected executable while retaining Cargo's machine-readable
/// artifact records for bundle assembly.
pub(crate) fn build_target(
    prepared: &crate::project::PreparedProject,
    target: &crate::project::SelectedTarget,
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
        let command = local_package_build(
            workdir,
            manifest,
            &target.package,
            &target.target,
            "json-render-diagnostics",
            options,
        )?;
        return run_command(command, CargoOperation::Build, options);
    }
    let mut command = command_for(prepared, CargoOperation::Build, options, false, false)?;
    command.args([
        "--manifest-path",
        &prepared.cargo_manifest_path().display().to_string(),
    ]);
    append_target_selection(&mut command, prepared, target, &options.features);
    command.args([
        "--message-format",
        options
            .message_format
            .as_deref()
            .filter(|format| format.starts_with("json"))
            .unwrap_or("json-render-diagnostics"),
    ]);
    run_command(command, CargoOperation::Build, options)
}
fn append_target_selection(
    command: &mut Command,
    prepared: &crate::project::PreparedProject,
    target: &crate::project::SelectedTarget,
    root_features: &[String],
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
        let mut features = root_features.to_vec();
        for feature in &target.required_features {
            if !features.iter().any(|existing| existing == feature) {
                features.push(feature.clone());
            }
        }
        command.args(["--features", &features.join(",")]);
    }
}
/// The exact executable one consumer selects from Cargo's JSON stream.
#[derive(Debug)]
pub(crate) enum SelectedExecutable {
    /// The selected executable path; the last matching artifact wins so a
    /// rebuild's fresh record supersedes earlier ones.
    Found(std::path::PathBuf),
    /// The stream ended without a matching executable artifact.
    Missing,
}
/// One exact compiler-artifact selection.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExecutableSelector<'a> {
    /// The exact Cargo package identity, when the consumer knows it.
    pub(crate) package_id: Option<&'a str>,
    /// The exact package manifest path, constraining a selection whose
    /// package identity is not otherwise known, such as one local package
    /// built through its resolved manifest.
    pub(crate) manifest_path: Option<&'a Path>,
    /// The Cargo target name, a binary or example spelling.
    pub(crate) binary: &'a str,
    /// Restrict matching to `kind = ["bin"]` targets.
    pub(crate) bin_only: bool,
}
impl ExecutableSelector<'_> {
    fn matches(&self, artifact: &cargo_metadata::Artifact) -> bool {
        artifact.target.name == self.binary
            && (!self.bin_only || artifact.target.is_bin())
            && self
                .package_id
                .is_none_or(|expected| artifact.package_id.to_string() == expected)
            && self
                .manifest_path
                .is_none_or(|expected| artifact.manifest_path.as_std_path() == expected)
    }
}
/// Parses Cargo's entire stdout JSON message stream before any consumer
/// uses it.
///
/// Every non-empty line must parse as one Cargo message. A malformed or
/// non-protocol line fails the whole stream, so a valid prefix can never
/// hide a protocol violation after the record a consumer selected.
/// Diagnostics rendering stays separate and keeps its lenient raw-line
/// forwarding policy.
pub(crate) fn parse_message_stream(stdout: &[u8]) -> Result<Vec<Message>, String> {
    stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_slice::<Message>(line)
                .map_err(|error| format!("invalid Cargo JSON message: {error}"))
        })
        .collect()
}
/// Selects one exact executable from an already-validated message stream.
///
/// Artifacts for other packages or targets are irrelevant and ignored. A
/// matching artifact without an executable contributes nothing; the stream
/// as a whole must still produce the selected executable, and the last
/// match wins so a rebuild's fresh record supersedes earlier ones.
pub(crate) fn select_executable(
    messages: &[Message],
    selector: &ExecutableSelector<'_>,
) -> SelectedExecutable {
    let mut executable = None;
    for message in messages {
        if let Message::CompilerArtifact(artifact) = message
            && selector.matches(artifact)
            && let Some(path) = artifact.executable.clone()
        {
            executable = Some(path.into_std_path_buf());
        }
    }
    match executable {
        Some(executable) => SelectedExecutable::Found(executable),
        None => SelectedExecutable::Missing,
    }
}
/// Extracts one selected executable from Cargo's JSON compiler-artifact stream.
pub(crate) fn artifact_path(
    stdout: &[u8],
    target: &crate::project::SelectedTarget,
) -> Result<std::path::PathBuf, Error> {
    if let Some(executable) = &target.executable {
        return Ok(executable.clone());
    }
    let messages = parse_message_stream(stdout).map_err(|message| Error::ArtifactCapture {
        package: target.package.clone(),
        target: target.target.clone(),
        message,
    })?;
    let selector = ExecutableSelector {
        package_id: Some(&target.package_id),
        manifest_path: None,
        binary: &target.target,
        bin_only: true,
    };
    match select_executable(&messages, &selector) {
        SelectedExecutable::Found(executable) => Ok(executable),
        SelectedExecutable::Missing => Err(Error::ArtifactCapture {
            package: target.package.clone(),
            target: target.target.clone(),
            message: "no compiler-artifact executable matched the selected package".to_owned(),
        }),
    }
}
pub(crate) fn run_command(
    mut command: Command,
    operation: CargoOperation,
    options: &CargoOptions,
) -> Result<CargoOutput, Error> {
    let output =
        crate::owned_process::output(&mut command).map_err(|source| Error::CargoSpawn {
            operation: operation.as_str().to_owned(),
            source,
        })?;
    forward_compiler_stream(&output, options);
    if !output.status.success() {
        return Err(cargo_command_failure(
            operation.as_str(),
            status_string(output.status),
            &output,
            options,
        ));
    }
    Ok(CargoOutput {
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

/// One Cargo command failure.
///
/// When the JSON compiler channels were already delivered on their real
/// streams, the project error carries only the operation, status, and
/// context; embedding the channels again would duplicate every compiler
/// record inside the structured diagnostic. Human mode keeps the complete
/// captured output, including Rust test failure evidence.
pub(crate) fn cargo_command_failure(
    operation: &str,
    status: String,
    output: &std::process::Output,
    options: &CargoOptions,
) -> Error {
    let delivered = options.diagnostics_json();
    Error::CargoCommand {
        operation: operation.to_owned(),
        status,
        stdout: if delivered {
            String::new()
        } else {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        },
        stderr: if delivered {
            String::new()
        } else {
            String::from_utf8_lossy(&output.stderr).trim().to_owned()
        },
    }
}

/// Forwards one captured compilation's compiler records to their channels
/// exactly once, on success and failure.
///
/// In JSON diagnostics mode the compiler's own records belong on stdout
/// with the compiler's stderr on stderr; project diagnostics, status, and
/// progress stay on stderr. Other modes stream nothing here so the human
/// owners keep their existing ordering.
pub(crate) fn forward_compiler_stream(output: &std::process::Output, options: &CargoOptions) {
    if !options.diagnostics_json() {
        crate::owned_process::retain_cancelled(&output.stdout, &output.stderr);
        return;
    }
    crate::output::bytes(&output.stdout, false);
    crate::output::bytes(&output.stderr, true);
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

    fn normalized(mut options: CargoOptions) -> CargoOptions {
        options.normalize();
        options
    }

    fn raw(arguments: &[&str]) -> Vec<OsString> {
        arguments.iter().map(OsString::from).collect()
    }

    #[test]
    fn compiler_output_overrides_normalize_without_changing_the_environment() {
        let options = normalized(CargoOptions {
            cargo_args: raw(&[
                "--target-dir=first",
                "--config",
                "build.rustflags=['--cfg','selected']",
                "--target-dir",
                "output with spaces",
                "--config=build.incremental=true",
            ]),
            ..CargoOptions::default()
        });
        assert_eq!(
            options.compiler_target_dir,
            Some("output with spaces".into())
        );
        assert_eq!(
            options.configs,
            raw(&[
                "build.rustflags=['--cfg','selected']",
                "build.incremental=true",
            ])
        );
        assert!(options.cargo_args.is_empty());
        assert_eq!(normalized(options.clone()), options);
        let mut command = Command::new("cargo");
        options.append_common(&mut command, false, false);
        assert!(command.get_envs().next().is_none());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            raw(&[
                "--config",
                "build.rustflags=['--cfg','selected']",
                "--config",
                "build.incremental=true",
            ])
        );
    }

    #[test]
    fn normalize_folds_lock_offline_and_short_aliases() {
        let options = normalized(CargoOptions {
            cargo_args: raw(&[
                "--frozen",
                "--locked",
                "--offline",
                "-r",
                "-Fvision",
                "-F",
                "imu",
                "-F=balance",
            ]),
            ..CargoOptions::default()
        });
        assert_eq!(options.lock, LockMode::Frozen, "frozen outranks locked");
        assert!(options.offline);
        assert!(options.release);
        assert_eq!(
            options.features,
            ["vision", "imu", "balance"]
                .iter()
                .map(|feature| feature.to_string())
                .collect::<Vec<_>>()
        );
        assert!(options.cargo_args.is_empty());

        let options = normalized(CargoOptions {
            lock: LockMode::Unlocked,
            cargo_args: raw(&["--locked"]),
            ..CargoOptions::default()
        });
        assert_eq!(options.lock, LockMode::Locked);
        // A raw tail can tighten but never relax a structured policy.
        let options = normalized(CargoOptions {
            lock: LockMode::Frozen,
            cargo_args: raw(&["--locked"]),
            ..CargoOptions::default()
        });
        assert_eq!(options.lock, LockMode::Frozen);
        let options = normalized(CargoOptions {
            lock: LockMode::Locked,
            cargo_args: raw(&["--frozen"]),
            ..CargoOptions::default()
        });
        assert_eq!(options.lock, LockMode::Frozen);
        let options = normalized(CargoOptions {
            offline: true,
            cargo_args: raw(&["--offline"]),
            ..CargoOptions::default()
        });
        assert!(options.offline);
    }

    #[test]
    fn normalize_folds_every_selection_flag_form_once() {
        let options = normalized(CargoOptions {
            cargo_args: raw(&[
                "--target=x86_64-unknown-linux-gnu",
                "--profile",
                "custom",
                "--features",
                "vision, imu",
                "--features=balance",
                "--release",
                "--all-features",
                "--no-default-features",
                "--jobs",
                "4",
            ]),
            ..CargoOptions::default()
        });
        assert_eq!(options.target.as_deref(), Some("x86_64-unknown-linux-gnu"));
        assert_eq!(options.profile.as_deref(), Some("custom"));
        assert!(options.release && options.all_features && options.no_default_features);
        assert_eq!(
            options.features,
            ["vision", "imu", "balance"]
                .iter()
                .map(|feature| feature.to_string())
                .collect::<Vec<_>>()
        );
        // Unrelated tail arguments survive in caller order for forwarding.
        assert_eq!(options.cargo_args, raw(&["--jobs", "4"]));
        // A second normalization is a no-op: fields are already resolved.
        let again = normalized(options.clone());
        assert_eq!(again, options);
    }

    #[test]
    fn normalize_ignores_empty_feature_pieces_and_valueless_tails() {
        let options = normalized(CargoOptions {
            cargo_args: raw(&["--features", "", "--features=,,", "--target"]),
            ..CargoOptions::default()
        });
        assert!(options.features.is_empty());
        // A valueless trailing flag is forwarded for Cargo to reject.
        assert_eq!(options.cargo_args, raw(&["--target"]));
    }

    #[test]
    fn normalize_lets_a_raw_selection_win_and_keeps_structured_features() {
        let options = normalized(CargoOptions {
            target: Some("aarch64-apple-darwin".to_owned()),
            profile: Some("dev".to_owned()),
            features: vec!["scenario".to_owned()],
            cargo_args: raw(&["--profile=release"]),
            ..CargoOptions::default()
        });
        assert_eq!(options.target.as_deref(), Some("aarch64-apple-darwin"));
        assert_eq!(options.profile.as_deref(), Some("release"));
        assert_eq!(options.features, ["scenario".to_owned()]);
    }

    #[test]
    fn build_selection_resolves_profiles_and_sorted_features() {
        let options = CargoOptions {
            release: true,
            features: vec!["vision".to_owned(), "imu".to_owned(), "vision".to_owned()],
            ..CargoOptions::default()
        };
        let selection = BuildSelection::root(&options);
        assert_eq!(selection.profile, "release");
        assert_eq!(selection.features, ["imu", "vision"]);
        let dev = BuildSelection::root(&CargoOptions::default());
        assert_eq!(dev.profile, "dev");
    }

    #[test]
    fn participant_selection_keeps_target_and_profile_without_root_features() {
        let options = CargoOptions {
            target: Some("x86_64-unknown-linux-gnu".to_owned()),
            release: true,
            features: vec!["vision".to_owned()],
            all_features: true,
            no_default_features: true,
            ..CargoOptions::default()
        };
        let selection = BuildSelection::participant(&options);
        assert_eq!(selection.target, "x86_64-unknown-linux-gnu");
        assert_eq!(selection.profile, "release");
        assert!(selection.features.is_empty());
        assert!(!selection.all_features && !selection.no_default_features);
    }

    #[test]
    fn effective_target_defaults_to_the_host_triple() {
        assert_eq!(
            effective_target(&CargoOptions::default()),
            phoxal::artifact::bundle::host_execution_target()
        );
    }

    fn artifact_line(
        package_id: &str,
        binary: &str,
        executable: Option<&str>,
        manifest_path: Option<&str>,
    ) -> String {
        let executable = match executable {
            Some(path) => format!("{path:?}"),
            None => "null".to_owned(),
        };
        let manifest_path = match manifest_path {
            Some(path) => format!(",\"manifest_path\":\"{path}\""),
            None => String::new(),
        };
        format!(
            "{{\"reason\":\"compiler-artifact\",\"package_id\":\"{package_id}\"{manifest_path},\
             \"target\":{{\"kind\":[\"bin\"],\"crate_types\":[\"bin\"],\"name\":\"{binary}\",\
             \"src_path\":\"/robot/src/main.rs\",\"edition\":\"2024\",\"doc\":false,\
             \"doctest\":false,\"test\":false}},\"profile\":{{\"opt_level\":\"0\",\
             \"debug_assertions\":true,\"overflow_checks\":true,\"test\":false}},\
             \"features\":[],\"filenames\":[],\"executable\":{executable},\"fresh\":false}}"
        )
    }

    fn parsed(stdout: &str) -> Vec<Message> {
        parse_message_stream(stdout.as_bytes())
            .unwrap_or_else(|message| panic!("strict stream: {message}"))
    }

    #[test]
    fn select_executable_matches_the_exact_package_and_binary() {
        let messages = parsed(&format!(
            "{}\n{}\n{}",
            artifact_line(
                "path+file:///robot#0.1.0",
                "other",
                Some("/robot/other"),
                None
            ),
            artifact_line("path+file:///dep#0.2.0", "brain", Some("/dep/brain"), None),
            artifact_line(
                "path+file:///robot#0.1.0",
                "brain",
                Some("/robot/brain"),
                None
            ),
        ));
        let found = select_executable(
            &messages,
            &ExecutableSelector {
                package_id: Some("path+file:///robot#0.1.0"),
                manifest_path: None,
                binary: "brain",
                bin_only: true,
            },
        );
        assert!(matches!(
            found,
            SelectedExecutable::Found(path) if path == std::path::Path::new("/robot/brain")
        ));
    }

    #[test]
    fn message_stream_rejects_malformed_suffixes_after_valid_records() {
        let valid_prefix = artifact_line(
            "path+file:///robot#0.1.0",
            "brain",
            Some("/robot/brain"),
            None,
        );
        for malformed_suffix in ["Compiling robot\n", "{not json}\n"] {
            let error =
                parse_message_stream(format!("{valid_prefix}\n{malformed_suffix}").as_bytes())
                    .expect_err("a valid prefix must not hide a trailing protocol violation");
            assert!(error.contains("invalid Cargo JSON message"));
        }
        assert!(
            parse_message_stream(b"\n")
                .expect("empty stream")
                .is_empty()
        );
        assert!(parse_message_stream(valid_prefix.as_bytes()).is_ok());
    }

    #[test]
    fn select_executable_takes_the_last_valid_match() {
        let messages = parsed(&format!(
            "{}\n{}",
            artifact_line(
                "path+file:///robot#0.1.0",
                "brain",
                Some("/robot/brain"),
                None
            ),
            artifact_line(
                "path+file:///robot#0.1.0",
                "brain",
                Some("/robot/brain-rebuilt"),
                None,
            ),
        ));
        let found = select_executable(
            &messages,
            &ExecutableSelector {
                package_id: Some("path+file:///robot#0.1.0"),
                manifest_path: None,
                binary: "brain",
                bin_only: true,
            },
        );
        assert!(matches!(
            found,
            SelectedExecutable::Found(path) if path == std::path::Path::new("/robot/brain-rebuilt")
        ));
    }

    #[test]
    fn select_executable_reports_missing_executables() {
        let selector = ExecutableSelector {
            package_id: Some("path+file:///robot#0.1.0"),
            manifest_path: None,
            binary: "brain",
            bin_only: true,
        };
        let missing = select_executable(&parsed("\n"), &selector);
        assert!(matches!(missing, SelectedExecutable::Missing));
        let no_executable = artifact_line("path+file:///robot#0.1.0", "brain", None, None);
        // A matching artifact without an executable is not a selection.
        let missing = select_executable(&parsed(&no_executable), &selector);
        assert!(matches!(missing, SelectedExecutable::Missing));
    }

    #[test]
    fn select_executable_constrains_one_exact_manifest() {
        let foreign = artifact_line(
            "path+file:///dep#0.2.0",
            "brain",
            Some("/dep/brain"),
            Some("/supervisor-workspace/dep/Cargo.toml"),
        );
        let intended = artifact_line(
            "path+file:///robot#0.1.0",
            "brain",
            Some("/robot/brain"),
            Some("/resolved/Cargo.toml"),
        );
        // The unrelated same-name executable arrives after the intended
        // artifact; the manifest constraint must still reject it.
        let messages = parsed(&format!("{intended}\n{foreign}"));
        let selector = ExecutableSelector {
            package_id: None,
            manifest_path: Some(std::path::Path::new("/resolved/Cargo.toml")),
            binary: "brain",
            bin_only: true,
        };
        let found = select_executable(&messages, &selector);
        assert!(matches!(
            found,
            SelectedExecutable::Found(path) if path == std::path::Path::new("/robot/brain")
        ));
        // Without the constraint the same stream would admit the foreign
        // executable as the last name-only match.
        let unconstrained = ExecutableSelector {
            package_id: None,
            manifest_path: None,
            binary: "brain",
            bin_only: true,
        };
        assert!(matches!(
            select_executable(&messages, &unconstrained),
            SelectedExecutable::Found(path) if path == std::path::Path::new("/dep/brain")
        ));
    }

    #[test]
    fn select_executable_admits_examples_without_a_package_identity() {
        let example = artifact_line(
            "path+file:///robot#0.1.0",
            "scenario",
            Some("/robot/scenario"),
            None,
        )
        .replace("\"kind\":[\"bin\"]", "\"kind\":[\"example\"]");
        let selector = ExecutableSelector {
            package_id: None,
            manifest_path: None,
            binary: "scenario",
            bin_only: false,
        };
        let found = select_executable(&parsed(&example), &selector);
        assert!(matches!(
            found,
            SelectedExecutable::Found(path) if path == std::path::Path::new("/robot/scenario")
        ));
        // bin_only selectors refuse the same example target.
        let bin_only = ExecutableSelector {
            package_id: None,
            manifest_path: None,
            binary: "scenario",
            bin_only: true,
        };
        assert!(matches!(
            select_executable(&parsed(&example), &bin_only),
            SelectedExecutable::Missing
        ));
    }

    #[test]
    fn local_package_build_preserves_the_caller_invocation_context() {
        let options = CargoOptions {
            release: true,
            offline: true,
            lock: LockMode::Locked,
            features: vec!["root-only".to_owned()],
            ..CargoOptions::default()
        };
        let staged = tempfile::tempdir().expect("staged supervisor");
        let root = staged.path().join("robot with spaces");
        let manifest = root.join("supervisor/Cargo.toml");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::create_dir_all(manifest.parent().unwrap().join("src")).unwrap();
        std::fs::write(
            manifest.parent().unwrap().join("src/main.rs"),
            "fn main() {}\n",
        )
        .unwrap();
        std::fs::write(
            &manifest,
            "[package]\nname = \"phoxal-supervisor\"\nversion = \"0.0.0-dev.8\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let command = local_package_build(
            &root,
            &manifest,
            "phoxal-supervisor",
            "phoxal-supervisor",
            "json",
            &options,
        )
        .expect("command");
        // The invocation directory is the caller's effective context, where
        // Cargo discovers .cargo configuration and toolchain files.
        assert_eq!(
            root.as_os_str(),
            command
                .get_current_dir()
                .expect("invocation directory")
                .as_os_str()
        );
        // The selected source builds below its own subdirectory of the
        // resolved target directory, keyed by the canonical source root.
        {
            use sha2::{Digest as _, Sha256};
            let source_root = manifest.canonicalize().unwrap();
            let expected = resolve_target_directory(&manifest, &root, &options)
                .expect("resolved target directory")
                .join("phoxal")
                .join(".selected")
                .join(format!(
                    "{:x}",
                    Sha256::digest(source_root.as_os_str().as_encoded_bytes())
                ));
            let configured = command
                .get_args()
                .collect::<Vec<_>>()
                .windows(2)
                .find(|arguments| arguments[0] == "--target-dir")
                .map(|arguments| arguments[1])
                .map(std::ffi::OsStr::to_owned)
                .expect("selected build directory configured");
            assert_eq!(
                std::path::PathBuf::from(&configured)
                    .canonicalize()
                    .ok()
                    .as_deref(),
                None::<&std::path::Path>,
                "the directory is not created by command construction"
            );
            assert_eq!(
                configured,
                expected.into_os_string(),
                "selected sources build in their own per-source directory"
            );
        }
        let arguments = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        let directories = selected_source_compiler_directories(&manifest, &root, &options).unwrap();
        let intermediate = directories.build.as_ref().map_or_else(String::new, |path| {
            format!(
                "\u{1f}--config\u{1f}{}",
                path_config("build.build-dir", path)
            )
        });
        let expected = format!(
            "build\u{1f}--manifest-path\u{1f}{}\u{1f}--target-dir\u{1f}{}{intermediate}\u{1f}--target\u{1f}{}\u{1f}--profile\u{1f}release\u{1f}--locked\u{1f}--offline\u{1f}--package\u{1f}phoxal-supervisor\u{1f}--bin\u{1f}phoxal-supervisor\u{1f}--message-format\u{1f}json",
            manifest.display(),
            directories.target.display(),
            phoxal::artifact::bundle::host_execution_target()
        );
        assert_eq!(arguments.join("\u{1f}"), expected);
    }
}
