//! Cargo package preparation for registry submission.
//!
//! This module selects a package from authored Cargo manifests, captures the
//! Cargo context needed to package it, and verifies the resulting archive.
//!
//! Remote submission consumes the archive through the sibling `submission`
//! module. Dry runs do not initialize credentials or make a remote request.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tar::Archive;

use crate::project::error::{Error, PublicationError};

const GENERATED_LIB: &str = "_cargo/lib.rs";
const MAX_ARCHIVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_ENTRY_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;

/// A package role accepted by the publication command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum PublicationKind {
    /// A component package, including a passive data-only component.
    Component,
    /// A runnable service implementation package.
    Service,
    /// A reusable library package.
    Library,
    /// A procedural macro package.
    #[serde(rename = "proc-macro")]
    ProcMacro,
    /// An independently built simulator application.
    #[serde(rename = "simulator")]
    SimulatorApplication,
    /// An independently built non-simulator application.
    Application,
    /// A standalone developer or operator tool.
    Tool,
}

impl PublicationKind {
    /// Returns the command spelling for this package role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Component => "component",
            Self::Service => "service",
            Self::Library => "library",
            Self::ProcMacro => "proc-macro",
            Self::SimulatorApplication => "simulator",
            Self::Application => "application",
            Self::Tool => "tool",
        }
    }

    fn accepts(self, actual: Self) -> bool {
        self == actual
    }
}

impl std::fmt::Display for PublicationKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Arguments controlling one local package publication preparation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationOptions {
    /// Semantic package role selected by the command.
    pub kind: PublicationKind,
    /// Exact authored Cargo package name.
    pub name: String,
    /// Optional source directory override.
    pub path: Option<PathBuf>,
    /// Keep preparation local and skip remote registry submission.
    pub dry_run: bool,
}

/// The local result of a validated publication preparation.
#[derive(Debug)]
pub struct PublicationResult {
    staging: Option<tempfile::TempDir>,
    kind: PublicationKind,
    package: String,
    version: String,
    archive: PathBuf,
    checksum: String,
    registry_kind: String,
    bytes: u64,
}

impl PublicationResult {
    /// Retains the prepared files after this value is dropped.
    ///
    /// Normal publication keeps its staging directory scoped to the command.
    /// Explicit dry runs use this method because their local artifacts are the
    /// requested output.
    #[must_use]
    pub fn retain(mut self) -> Self {
        if let Some(staging) = self.staging.take() {
            let _retained_root = staging.keep();
        }
        self
    }

    /// Returns the selected semantic package role.
    #[must_use]
    pub const fn kind(&self) -> PublicationKind {
        self.kind
    }

    /// Returns the exact Cargo package name.
    #[must_use]
    pub fn package(&self) -> &str {
        &self.package
    }

    /// Returns the authored package version.
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the `.crate` archive path.
    #[must_use]
    pub fn archive(&self) -> &Path {
        &self.archive
    }

    /// Returns the verified archive SHA-256 digest.
    #[must_use]
    pub fn checksum(&self) -> &str {
        &self.checksum
    }

    /// Returns the registry's exact content role for this archive.
    #[must_use]
    pub fn registry_kind(&self) -> &str {
        &self.registry_kind
    }

    /// Returns the archive byte count.
    #[must_use]
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// Prepares one local package archive for registry submission.
///
/// This function only prepares immutable local bytes. The caller decides
/// whether those bytes remain a dry run or are submitted.
pub fn prepare_publication(options: &PublicationOptions) -> Result<PublicationResult, Error> {
    let selected = select_package(options)?;
    let staging = tempfile::Builder::new()
        .prefix("phoxal-publication-")
        .tempdir()
        .map_err(|source| PublicationError::StagingDirectory { source })?;
    let staging_root = staging.path().to_owned();

    let captured = capture_source(&selected, &staging_root)?;
    let expected_assets = stage_package(&selected, &captured)?;
    let archive = package_with_cargo(&selected.package, &captured)?;
    let archive_bytes = fs::metadata(&archive)
        .map_err(|source| PublicationError::CaptureSource {
            path: archive.clone(),
            source,
        })?
        .len();
    if archive_bytes > MAX_ARCHIVE_BYTES {
        return Err(PublicationError::ArchiveTooLarge {
            path: archive,
            bytes: archive_bytes,
        }
        .into());
    }
    let verified = verify_archive(
        &archive,
        &selected.package,
        &selected.version,
        selected.role,
        &expected_assets,
    )?;

    Ok(PublicationResult {
        staging: Some(staging),
        kind: selected.role.publication_kind(),
        package: selected.package,
        version: selected.version,
        archive,
        checksum: verified.checksum,
        registry_kind: selected.role.registry_kind().to_owned(),
        bytes: verified.bytes,
    })
}

#[derive(Debug, Clone)]
struct SelectedPackage {
    package: String,
    version: String,
    source_root: PathBuf,
    manifest: PathBuf,
    workspace: Option<WorkspaceContext>,
    manifest_value: toml::Value,
    role: PackageRole,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageRole {
    PassiveComponent,
    RustComponent,
    Service,
    Library,
    ProcMacro,
    SimulatorApplication,
    Application,
    Tool,
}

impl PackageRole {
    const fn publication_kind(self) -> PublicationKind {
        match self {
            Self::PassiveComponent | Self::RustComponent => PublicationKind::Component,
            Self::Service => PublicationKind::Service,
            Self::Library => PublicationKind::Library,
            Self::ProcMacro => PublicationKind::ProcMacro,
            Self::SimulatorApplication => PublicationKind::SimulatorApplication,
            Self::Application => PublicationKind::Application,
            Self::Tool => PublicationKind::Tool,
        }
    }

    const fn registry_kind(self) -> &'static str {
        match self {
            Self::PassiveComponent | Self::RustComponent => "component",
            Self::Service => "service",
            Self::Library => "library",
            Self::ProcMacro => "proc-macro",
            Self::SimulatorApplication => "simulator",
            Self::Application => "application",
            Self::Tool => "tool",
        }
    }
}

impl std::fmt::Display for PackageRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::PassiveComponent => "passive component",
            Self::RustComponent => "component",
            Self::Service => "service",
            Self::Library => "library",
            Self::ProcMacro => "procedural macro",
            Self::SimulatorApplication => "simulator",
            Self::Application => "application",
            Self::Tool => "tool",
        })
    }
}

#[derive(Debug, Clone)]
struct WorkspaceContext {
    root: PathBuf,
    manifest: PathBuf,
    package_relative: PathBuf,
}

fn select_package(options: &PublicationOptions) -> Result<SelectedPackage, Error> {
    if !is_package_name(&options.name) {
        return Err(PublicationError::InvalidName {
            name: options.name.clone(),
        }
        .into());
    }
    let current = env::current_dir().map_err(|source| PublicationError::ResolveSource {
        path: PathBuf::from("."),
        source,
    })?;
    let manifest = if let Some(path) = &options.path {
        let supplied = if path.is_absolute() {
            path.clone()
        } else {
            current.join(path)
        };
        let source_root =
            supplied
                .canonicalize()
                .map_err(|source| PublicationError::ResolveSource {
                    path: supplied.clone(),
                    source,
                })?;
        if !source_root.is_dir() {
            return Err(PublicationError::SourceNotDirectory { path: source_root }.into());
        }
        let manifest = source_root.join("Cargo.toml");
        if !manifest.is_file() {
            return Err(PublicationError::MissingPackageManifest { path: source_root }.into());
        }
        manifest
    } else {
        select_implicit_manifest(&current, &options.name)?
    };
    selected_from_manifest(manifest, options)
}

fn select_implicit_manifest(start: &Path, name: &str) -> Result<PathBuf, Error> {
    let canonical_start =
        start
            .canonicalize()
            .map_err(|source| PublicationError::ResolveSource {
                path: start.to_owned(),
                source,
            })?;
    let mut cursor = if canonical_start.is_file() {
        canonical_start
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| PublicationError::PackageNotFound {
                name: name.to_owned(),
                start: start.to_owned(),
            })?
    } else {
        canonical_start
    };
    let mut workspace_manifest = None;
    loop {
        let manifest = cursor.join("Cargo.toml");
        if manifest.is_file() {
            let value = read_manifest(&manifest)?;
            if value
                .get("package")
                .and_then(toml::Value::as_table)
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)
                .is_some_and(|package_name| package_name == name)
            {
                return Ok(manifest);
            }
            if value.get("workspace").is_some_and(toml::Value::is_table) {
                workspace_manifest = Some(manifest);
                break;
            }
        }
        if !cursor.pop() {
            break;
        }
    }
    let Some(workspace_manifest) = workspace_manifest else {
        return Err(PublicationError::PackageNotFound {
            name: name.to_owned(),
            start: start.to_owned(),
        }
        .into());
    };
    let workspace_root =
        workspace_manifest
            .parent()
            .ok_or_else(|| PublicationError::PackageNotFound {
                name: name.to_owned(),
                start: start.to_owned(),
            })?;
    let workspace = read_manifest(&workspace_manifest)?;
    let members = workspace_members(workspace_root, &workspace)?;
    let mut matches = members
        .into_iter()
        .filter_map(|member| {
            let manifest = member.join("Cargo.toml");
            let value = read_manifest(&manifest).ok()?;
            let member_name = value
                .get("package")
                .and_then(toml::Value::as_table)
                .and_then(|package| package.get("name"))
                .and_then(toml::Value::as_str)?;
            (member_name == name).then_some(manifest)
        })
        .collect::<Vec<_>>();
    matches.sort();
    matches.dedup();
    match matches.as_slice() {
        [] => Err(PublicationError::PackageNotFound {
            name: name.to_owned(),
            start: start.to_owned(),
        }
        .into()),
        [manifest] => Ok(manifest.clone()),
        _ => Err(PublicationError::AmbiguousPackage {
            name: name.to_owned(),
            workspace: workspace_root.to_owned(),
            candidates: matches
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
        }
        .into()),
    }
}

fn selected_from_manifest(
    manifest: PathBuf,
    options: &PublicationOptions,
) -> Result<SelectedPackage, Error> {
    let manifest = manifest
        .canonicalize()
        .map_err(|source| PublicationError::ResolveSource {
            path: manifest.clone(),
            source,
        })?;
    let source_root = manifest
        .parent()
        .ok_or_else(|| PublicationError::MissingPackageManifest {
            path: manifest.clone(),
        })?
        .to_owned();
    let manifest_value = read_manifest(&manifest)?;
    let package = manifest_value
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| PublicationError::MissingPackageManifest {
            path: manifest.clone(),
        })?;
    let actual_name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| PublicationError::MissingPackageField {
            path: manifest.clone(),
            field: "name",
        })?;
    if actual_name != options.name {
        return Err(PublicationError::PackageNameMismatch {
            expected: options.name.clone(),
            actual: actual_name.to_owned(),
            path: manifest,
        }
        .into());
    }
    let version = package
        .get("version")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| PublicationError::MissingPackageField {
            path: manifest.clone(),
            field: "version",
        })?
        .to_owned();
    let workspace = find_workspace(&source_root, &manifest)?;
    let role = classify_package(&source_root, &manifest_value, actual_name, options.kind)?;
    if !options.kind.accepts(role.publication_kind()) {
        return Err(PublicationError::WrongPublicationKind {
            package: actual_name.to_owned(),
            path: source_root,
            actual: role.to_string(),
            requested: options.kind,
        }
        .into());
    }
    Ok(SelectedPackage {
        package: actual_name.to_owned(),
        version,
        source_root,
        manifest,
        workspace,
        manifest_value,
        role,
    })
}

fn read_manifest(path: &Path) -> Result<toml::Value, Error> {
    let text = fs::read_to_string(path).map_err(|source| PublicationError::CaptureSource {
        path: path.to_owned(),
        source,
    })?;
    toml::from_str(&text)
        .map_err(|source| PublicationError::ParsePublicationManifest {
            path: path.to_owned(),
            source,
        })
        .map_err(Into::into)
}

fn is_package_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn classify_package(
    source_root: &Path,
    manifest: &toml::Value,
    package: &str,
    requested: PublicationKind,
) -> Result<PackageRole, Error> {
    let targets = target_shape(source_root, manifest);
    if source_root.join("component.yaml").is_file()
        && !matches!(requested, PublicationKind::Component)
    {
        let actual = if targets.library {
            PackageRole::RustComponent
        } else {
            PackageRole::PassiveComponent
        };
        return Err(PublicationError::WrongPublicationKind {
            package: package.to_owned(),
            path: source_root.to_owned(),
            actual: actual.to_string(),
            requested,
        }
        .into());
    }
    if matches!(requested, PublicationKind::Component) {
        require_component_definition(source_root, package, manifest)?;
        return if targets.binaries && !targets.library {
            Ok(PackageRole::RustComponent)
        } else if targets.library || has_authored_target(source_root, manifest) {
            Err(PublicationError::InvalidPackageShape {
                package: package.to_owned(),
                kind: "component".to_owned(),
                requirement:
                    "API-bearing components must expose a runnable binary and packaged API"
                        .to_owned(),
            }
            .into())
        } else {
            Ok(PackageRole::PassiveComponent)
        };
    }
    let (role, requirement) = match requested {
        PublicationKind::Service if targets.binaries && !targets.library => {
            return Ok(PackageRole::Service);
        }
        PublicationKind::Service => (
            PackageRole::Service,
            "service packages must expose a runnable binary and packaged API",
        ),
        PublicationKind::Library if targets.library && !targets.proc_macro => {
            return Ok(PackageRole::Library);
        }
        PublicationKind::Library => (
            PackageRole::Library,
            "library packages must expose a non-proc-macro library target",
        ),
        PublicationKind::ProcMacro if targets.proc_macro => return Ok(PackageRole::ProcMacro),
        PublicationKind::ProcMacro => (
            PackageRole::ProcMacro,
            "proc-macro packages must expose a proc-macro library target",
        ),
        PublicationKind::SimulatorApplication if targets.binaries => {
            return Ok(PackageRole::SimulatorApplication);
        }
        PublicationKind::SimulatorApplication => (
            PackageRole::SimulatorApplication,
            "simulator packages must expose a binary target",
        ),
        PublicationKind::Application if targets.binaries => return Ok(PackageRole::Application),
        PublicationKind::Application => (
            PackageRole::Application,
            "application packages must expose a binary target",
        ),
        PublicationKind::Tool if targets.binaries => return Ok(PackageRole::Tool),
        PublicationKind::Tool => (
            PackageRole::Tool,
            "tool packages must expose a binary target",
        ),
        PublicationKind::Component => unreachable!("component classification returned above"),
    };
    Err(PublicationError::InvalidPackageShape {
        package: package.to_owned(),
        kind: role.registry_kind().to_owned(),
        requirement: requirement.to_owned(),
    }
    .into())
}

/// Validates a package selected as a runtime service or component.
///
/// Publication owns the canonical package-role classifier, including the
/// requested runtime role, target shape, and component definition root checks.
#[derive(Clone, Copy)]
struct TargetShape {
    library: bool,
    binaries: bool,
    proc_macro: bool,
}

fn target_shape(source_root: &Path, manifest: &toml::Value) -> TargetShape {
    let package = manifest.get("package").and_then(toml::Value::as_table);
    let automatic_library = package
        .and_then(|package| package.get("autolib"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let automatic_binaries = package
        .and_then(|package| package.get("autobins"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true);
    let library_table = manifest.get("lib").and_then(toml::Value::as_table);
    let library =
        library_table.is_some() || (automatic_library && source_root.join("src/lib.rs").is_file());
    let binaries = manifest
        .get("bin")
        .and_then(toml::Value::as_array)
        .is_some_and(|targets| !targets.is_empty())
        || (automatic_binaries && source_root.join("src/main.rs").is_file());
    let proc_macro = library_table
        .and_then(|library| library.get("proc-macro"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);
    TargetShape {
        library,
        binaries,
        proc_macro,
    }
}

fn require_component_definition(
    source_root: &Path,
    package: &str,
    _manifest: &toml::Value,
) -> Result<PathBuf, Error> {
    let relative = "component.yaml";
    let path =
        safe_source_path(source_root, relative).map_err(|_| PublicationError::UnsafeAssetPath {
            reference: relative.to_owned(),
            definition: source_root.join("Cargo.toml"),
            root: source_root.to_owned(),
        })?;
    if !path.is_file() {
        return Err(PublicationError::MissingComponentDefinition {
            package: package.to_owned(),
            path,
        }
        .into());
    }
    Ok(path)
}

fn has_authored_target(source_root: &Path, manifest: &toml::Value) -> bool {
    let Some(package) = manifest.get("package").and_then(toml::Value::as_table) else {
        return false;
    };
    if package
        .get("build")
        .is_some_and(|build| build.as_bool().is_none_or(|value| value))
    {
        return true;
    }
    if ["lib", "bin", "example", "test", "bench"]
        .iter()
        .any(|key| manifest.get(*key).is_some())
    {
        return true;
    }
    let Ok(entries) = walk_files(source_root) else {
        return true;
    };
    entries.iter().any(|path| {
        path.extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
    })
}

fn find_workspace(source_root: &Path, manifest: &Path) -> Result<Option<WorkspaceContext>, Error> {
    let mut cursor = source_root.to_owned();
    loop {
        let candidate = cursor.join("Cargo.toml");
        if candidate == manifest || candidate.is_file() {
            let value = read_manifest(&candidate)?;
            if candidate == manifest
                && let Some(workspace) = value
                    .get("package")
                    .and_then(toml::Value::as_table)
                    .and_then(|package| package.get("workspace"))
                    .and_then(toml::Value::as_str)
            {
                let root = safe_dependency_path(source_root, workspace).map_err(|_| {
                    PublicationError::UnsafeAssetPath {
                        reference: workspace.to_owned(),
                        definition: manifest.to_owned(),
                        root: source_root.to_owned(),
                    }
                })?;
                let root =
                    root.canonicalize()
                        .map_err(|source| PublicationError::CaptureSource {
                            path: root.clone(),
                            source,
                        })?;
                let manifest = root.join("Cargo.toml");
                let root_value = read_manifest(&manifest)?;
                if root_value
                    .get("workspace")
                    .is_some_and(toml::Value::is_table)
                {
                    let package_relative = source_root
                        .strip_prefix(&root)
                        .ok()
                        .filter(|relative| !relative.as_os_str().is_empty())
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."));
                    return Ok(Some(WorkspaceContext {
                        root,
                        manifest,
                        package_relative,
                    }));
                }
            }
            if value.get("workspace").is_some_and(toml::Value::is_table) {
                let relative = source_root
                    .strip_prefix(&cursor)
                    .map(PathBuf::from)
                    .map_err(|_| PublicationError::CaptureSource {
                        path: source_root.to_owned(),
                        source: io::Error::other("workspace root is not an ancestor"),
                    })?;
                let relative = if relative.as_os_str().is_empty() {
                    PathBuf::from(".")
                } else {
                    relative
                };
                return Ok(Some(WorkspaceContext {
                    root: cursor,
                    manifest: candidate,
                    package_relative: relative,
                }));
            }
        }
        if !cursor.pop() {
            return Ok(None);
        }
    }
}

fn workspace_members(
    workspace_root: &Path,
    workspace: &toml::Value,
) -> Result<Vec<PathBuf>, Error> {
    let table = workspace
        .get("workspace")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| PublicationError::CaptureSource {
            path: workspace_root.join("Cargo.toml"),
            source: io::Error::other("missing workspace table"),
        })?;
    let patterns = table
        .get("members")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    let excluded = table
        .get("exclude")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(PathBuf::from)
        .collect::<BTreeSet<_>>();
    let mut members = BTreeSet::new();
    for pattern in patterns {
        for path in expand_member_pattern(workspace_root, pattern)? {
            let relative = path
                .strip_prefix(workspace_root)
                .map(PathBuf::from)
                .unwrap_or_else(|_| path.clone());
            if !excluded.contains(&relative) {
                members.insert(path);
            }
        }
    }
    Ok(members.into_iter().collect())
}

fn expand_member_pattern(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, Error> {
    let pattern_path = Path::new(pattern);
    if pattern_path.is_absolute()
        || pattern_path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(PublicationError::UnsafeAssetPath {
            reference: pattern.to_owned(),
            definition: root.join("Cargo.toml"),
            root: root.to_owned(),
        }
        .into());
    }
    let components = pattern_path.components().collect::<Vec<_>>();
    let mut paths = vec![root.to_owned()];
    for component in components {
        let Component::Normal(component) = component else {
            continue;
        };
        let component = component.to_string_lossy();
        let wildcard = component.contains('*') || component.contains('?');
        let mut next = Vec::new();
        for path in paths {
            if wildcard {
                let entries =
                    fs::read_dir(&path).map_err(|source| PublicationError::CaptureSource {
                        path: path.clone(),
                        source,
                    })?;
                for entry in entries {
                    let entry = entry.map_err(|source| PublicationError::CaptureSource {
                        path: path.clone(),
                        source,
                    })?;
                    if entry
                        .file_type()
                        .map_err(|source| PublicationError::CaptureSource {
                            path: entry.path(),
                            source,
                        })?
                        .is_dir()
                        && wildcard_match(&component, &entry.file_name().to_string_lossy())
                    {
                        next.push(entry.path());
                    }
                }
            } else {
                next.push(path.join(component.as_ref()));
            }
        }
        paths = next;
    }
    Ok(paths
        .into_iter()
        .filter(|path| path.is_dir() && path.join("Cargo.toml").is_file())
        .collect())
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    fn inner(pattern: &[u8], value: &[u8]) -> bool {
        match pattern.split_first() {
            None => value.is_empty(),
            Some((b'*', rest)) => {
                inner(rest, value) || (!value.is_empty() && inner(pattern, &value[1..]))
            }
            Some((b'?', rest)) => !value.is_empty() && inner(rest, &value[1..]),
            Some((character, rest)) => {
                value
                    .first()
                    .is_some_and(|value_character| character == value_character)
                    && inner(rest, &value[1..])
            }
        }
    }
    inner(pattern.as_bytes(), value.as_bytes())
}

#[derive(Debug, Clone)]
struct CapturedSource {
    manifest: PathBuf,
    workspace_root: PathBuf,
    /// Staged source directories outside the selected package root.
    ///
    /// These paths are excluded from a non-workspace package archive while
    /// remaining available as ordinary Cargo path dependencies during
    /// isolated packaging.
    excluded_paths: BTreeSet<PathBuf>,
}

#[derive(Debug, Default)]
struct CaptureState {
    /// Canonical authored source roots and their staged counterparts.
    locations: BTreeMap<PathBuf, PathBuf>,
    /// Next unique directory within this one temporary capture.
    next_external_id: usize,
    /// External staged roots that must not become selected-package archive
    /// content.
    excluded_paths: BTreeSet<PathBuf>,
    /// External owning workspaces already captured into the staging tree.
    workspaces: BTreeMap<PathBuf, ExternalWorkspace>,
}

impl CaptureState {
    fn external_root(&mut self, staging_root: &Path) -> PathBuf {
        let id = self.next_external_id;
        self.next_external_id += 1;
        staging_root
            .join("_phoxal_path_dependencies")
            .join(format!("external-{id}"))
    }
}

#[derive(Debug, Clone)]
struct ExternalWorkspace {
    staged_root: PathBuf,
    manifest: toml::Value,
}

#[derive(Debug, Clone)]
struct DependencyOccurrence {
    table_path: Vec<String>,
    key: String,
    value: toml::Value,
}

fn capture_source(
    selected: &SelectedPackage,
    staging_root: &Path,
) -> Result<CapturedSource, Error> {
    let mut state = CaptureState::default();
    let package_root =
        if let Some(workspace) = &selected.workspace {
            let root = staging_root.to_owned();
            let workspace_value = read_manifest(&workspace.manifest)?;
            let mut workspace_value = workspace_value;
            let workspace_table = workspace_value
                .get_mut("workspace")
                .and_then(toml::Value::as_table_mut)
                .ok_or_else(|| PublicationError::CaptureSource {
                    path: workspace.manifest.clone(),
                    source: io::Error::other("missing workspace table"),
                })?;
            let member = if workspace.package_relative == Path::new(".") {
                ".".to_owned()
            } else {
                workspace.package_relative.display().to_string()
            };
            workspace_table.insert(
                "members".to_owned(),
                toml::Value::Array(vec![toml::Value::String(member)]),
            );
            let excludes = workspace_table
                .entry("exclude".to_owned())
                .or_insert_with(|| toml::Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| PublicationError::CaptureSource {
                    path: workspace.manifest.clone(),
                    source: io::Error::other("workspace exclude changed shape while staging"),
                })?;
            if !excludes
                .iter()
                .filter_map(toml::Value::as_str)
                .any(|exclude| exclude == "_phoxal_path_dependencies")
            {
                excludes.push(toml::Value::String("_phoxal_path_dependencies".to_owned()));
            }
            workspace_table.remove("default-members");
            let workspace_manifest = root.join("Cargo.toml");
            let workspace_text = toml::to_string_pretty(&workspace_value)
                .map_err(|source| PublicationError::SerializeStagedManifest { source })?;
            let root_package = workspace.package_relative == Path::new(".");
            if !root_package {
                write_staged_file(&workspace_manifest, workspace_text.as_bytes())?;
            }
            validate_cargo_config(&workspace.root.join(".cargo"))?;
            copy_tree(&workspace.root.join(".cargo"), &root.join(".cargo"), true)?;
            copy_optional_file(&workspace.root.join("Cargo.lock"), &root.join("Cargo.lock"))?;
            let target = root.join(&workspace.package_relative);
            validate_cargo_config(&selected.source_root.join(".cargo"))?;
            copy_tree(&selected.source_root, &target, false)?;
            capture_external_package_files(selected, &workspace.root, &root)?;
            if root_package {
                write_staged_file(&workspace_manifest, workspace_text.as_bytes())?;
            }
            let workspace_canonical = workspace.root.canonicalize().map_err(|source| {
                PublicationError::CaptureSource {
                    path: workspace.root.clone(),
                    source,
                }
            })?;
            state.locations.insert(workspace_canonical, root.clone());
            let selected_canonical = selected.source_root.canonicalize().map_err(|source| {
                PublicationError::CaptureSource {
                    path: selected.source_root.clone(),
                    source,
                }
            })?;
            state.locations.insert(selected_canonical, target.clone());
            capture_path_dependencies(
                &selected.source_root,
                &target,
                &workspace.root,
                &workspace_value,
                &root,
                Some(&workspace_manifest),
                &mut state,
            )?;
            capture_path_dependencies(
                &workspace.root,
                &root,
                &workspace.root,
                &workspace_value,
                &root,
                Some(&workspace_manifest),
                &mut state,
            )?;
            target
        } else {
            let target = staging_root.to_owned();
            validate_cargo_config(&selected.source_root.join(".cargo"))?;
            copy_tree(&selected.source_root, &target, false)?;
            let selected_canonical = selected.source_root.canonicalize().map_err(|source| {
                PublicationError::CaptureSource {
                    path: selected.source_root.clone(),
                    source,
                }
            })?;
            state.locations.insert(selected_canonical, target.clone());
            capture_path_dependencies(
                &selected.source_root,
                &target,
                &selected.source_root,
                &toml::Value::Table(toml::map::Map::new()),
                staging_root,
                None,
                &mut state,
            )?;
            target
        };
    let manifest = package_root.join("Cargo.toml");
    Ok(CapturedSource {
        manifest,
        workspace_root: staging_root.to_owned(),
        excluded_paths: state.excluded_paths,
    })
}

fn copy_optional_file(source: &Path, destination: &Path) -> Result<(), Error> {
    if source.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| PublicationError::CaptureSource {
                path: parent.to_owned(),
                source: error,
            })?;
        }
        fs::copy(source, destination).map_err(|error| PublicationError::CaptureSource {
            path: source.to_owned(),
            source: error,
        })?;
    }
    Ok(())
}

fn capture_external_package_files(
    selected: &SelectedPackage,
    workspace_root: &Path,
    staging_root: &Path,
) -> Result<(), Error> {
    let package = selected
        .manifest_value
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or_else(|| PublicationError::MissingPackageManifest {
            path: selected.manifest.clone(),
        })?;
    let canonical_workspace =
        workspace_root
            .canonicalize()
            .map_err(|source| PublicationError::CaptureSource {
                path: workspace_root.to_owned(),
                source,
            })?;
    for field in ["license-file", "readme"] {
        let Some(reference) = package.get(field).and_then(toml::Value::as_str) else {
            continue;
        };
        let source = selected.source_root.join(reference);
        let canonical_source =
            source
                .canonicalize()
                .map_err(|source_error| PublicationError::CaptureSource {
                    path: source.clone(),
                    source: source_error,
                })?;
        let relative = canonical_source
            .strip_prefix(&canonical_workspace)
            .map_err(|_| PublicationError::UnsafeAssetPath {
                reference: reference.to_owned(),
                definition: selected.manifest.clone(),
                root: workspace_root.to_owned(),
            })?;
        let metadata = fs::symlink_metadata(&canonical_source).map_err(|source| {
            PublicationError::CaptureSource {
                path: canonical_source.clone(),
                source,
            }
        })?;
        if metadata.file_type().is_symlink() {
            return Err(PublicationError::SymbolicLink {
                path: canonical_source,
            }
            .into());
        }
        if !metadata.is_file() {
            return Err(PublicationError::MissingAsset {
                reference: reference.to_owned(),
                definition: selected.manifest.clone(),
            }
            .into());
        }
        copy_optional_file(&canonical_source, &staging_root.join(relative))?;
    }
    Ok(())
}

fn write_staged_file(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| PublicationError::WriteStagedFile {
            path: parent.to_owned(),
            source,
        })?;
    }
    fs::write(path, bytes).map_err(|source| PublicationError::WriteStagedFile {
        path: path.to_owned(),
        source,
    })?;
    Ok(())
}

fn owning_workspace_root(package_root: &Path) -> Result<Option<PathBuf>, Error> {
    let manifest = package_root.join("Cargo.toml");
    let value = read_manifest(&manifest)?;
    if let Some(workspace) = value
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("workspace"))
        .and_then(toml::Value::as_str)
    {
        let root = safe_dependency_path(package_root, workspace).map_err(|_| {
            PublicationError::UnsafeAssetPath {
                reference: workspace.to_owned(),
                definition: manifest.clone(),
                root: package_root.to_owned(),
            }
        })?;
        let root = root
            .canonicalize()
            .map_err(|source| PublicationError::CaptureSource {
                path: root.clone(),
                source,
            })?;
        if root.join("Cargo.toml").is_file() {
            return Ok(Some(root));
        }
        return Err(PublicationError::MissingPackageManifest {
            path: root.join("Cargo.toml"),
        }
        .into());
    }
    if value.get("workspace").is_some_and(toml::Value::is_table) {
        return Ok(Some(package_root.canonicalize().map_err(|source| {
            PublicationError::CaptureSource {
                path: package_root.to_owned(),
                source,
            }
        })?));
    }
    let mut cursor = package_root.parent().map(Path::to_path_buf);
    while let Some(root) = cursor {
        let candidate = root.join("Cargo.toml");
        if candidate.is_file() {
            let value = read_manifest(&candidate)?;
            if value.get("workspace").is_some_and(toml::Value::is_table) {
                return Ok(Some(root.canonicalize().map_err(|source| {
                    PublicationError::CaptureSource {
                        path: root.clone(),
                        source,
                    }
                })?));
            }
        }
        cursor = root.parent().map(Path::to_path_buf);
    }
    Ok(None)
}

fn capture_external_workspace(
    workspace_root: &Path,
    package_root: &Path,
    staging_root: &Path,
    state: &mut CaptureState,
) -> Result<(PathBuf, toml::Value, PathBuf), Error> {
    let workspace_root =
        workspace_root
            .canonicalize()
            .map_err(|source| PublicationError::CaptureSource {
                path: workspace_root.to_owned(),
                source,
            })?;
    let package_root =
        package_root
            .canonicalize()
            .map_err(|source| PublicationError::CaptureSource {
                path: package_root.to_owned(),
                source,
            })?;
    let (staged_root, manifest) = if let Some(workspace) = state.workspaces.get(&workspace_root) {
        (workspace.staged_root.clone(), workspace.manifest.clone())
    } else {
        let staged_root = state.external_root(staging_root);
        let mut manifest = read_manifest(&workspace_root.join("Cargo.toml"))?;
        let relative = package_root
            .strip_prefix(&workspace_root)
            .map(path_string)
            .map_err(|_| PublicationError::CaptureSource {
                path: package_root.clone(),
                source: io::Error::other("external package is outside its owning workspace"),
            })?;
        manifest
            .get_mut("workspace")
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| PublicationError::CaptureSource {
                path: workspace_root.join("Cargo.toml"),
                source: io::Error::other("owning workspace has no workspace table"),
            })?
            .insert(
                "members".to_owned(),
                toml::Value::Array(vec![toml::Value::String(relative)]),
            );
        if let Some(workspace) = manifest
            .get_mut("workspace")
            .and_then(toml::Value::as_table_mut)
        {
            workspace.remove("default-members");
        }
        copy_optional_file(
            &workspace_root.join("Cargo.lock"),
            &staged_root.join("Cargo.lock"),
        )?;
        validate_cargo_config(&workspace_root.join(".cargo"))?;
        copy_tree(
            &workspace_root.join(".cargo"),
            &staged_root.join(".cargo"),
            true,
        )?;
        let workspace_manifest = staged_root.join("Cargo.toml");
        let text = toml::to_string_pretty(&manifest)
            .map_err(|source| PublicationError::SerializeStagedManifest { source })?;
        write_staged_file(&workspace_manifest, text.as_bytes())?;
        state.workspaces.insert(
            workspace_root.clone(),
            ExternalWorkspace {
                staged_root: staged_root.clone(),
                manifest: manifest.clone(),
            },
        );
        state
            .locations
            .insert(workspace_root.clone(), staged_root.clone());
        state.excluded_paths.insert(staged_root.clone());
        (staged_root, manifest)
    };
    let relative = package_root
        .strip_prefix(&workspace_root)
        .map(path_string)
        .map_err(|_| PublicationError::CaptureSource {
            path: package_root.clone(),
            source: io::Error::other("external package is outside its owning workspace"),
        })?;
    if let Some(workspace) = state.workspaces.get_mut(&workspace_root) {
        let table = workspace
            .manifest
            .get_mut("workspace")
            .and_then(toml::Value::as_table_mut)
            .ok_or_else(|| PublicationError::CaptureSource {
                path: workspace_root.join("Cargo.toml"),
                source: io::Error::other("owning workspace has no workspace table"),
            })?;
        let members = table
            .entry("members".to_owned())
            .or_insert_with(|| toml::Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| PublicationError::CaptureSource {
                path: workspace_root.join("Cargo.toml"),
                source: io::Error::other("workspace members changed shape while staging"),
            })?;
        if !members
            .iter()
            .filter_map(toml::Value::as_str)
            .any(|member| member == relative)
        {
            members.push(toml::Value::String(relative.clone()));
            let text = toml::to_string_pretty(&workspace.manifest)
                .map_err(|source| PublicationError::SerializeStagedManifest { source })?;
            write_staged_file(&workspace.staged_root.join("Cargo.toml"), text.as_bytes())?;
        }
    }
    let staged_package = staged_root.join(&relative);
    state
        .locations
        .insert(package_root.clone(), staged_package.clone());
    if !staged_package.exists() {
        validate_cargo_config(&package_root.join(".cargo"))?;
        copy_tree(&package_root, &staged_package, false)?;
    }
    Ok((staged_package, manifest, staged_root.join("Cargo.toml")))
}

fn capture_path_dependencies(
    package_root: &Path,
    staged_package_root: &Path,
    workspace_root: &Path,
    workspace_manifest: &toml::Value,
    staging_root: &Path,
    staged_workspace_manifest: Option<&Path>,
    state: &mut CaptureState,
) -> Result<(), Error> {
    let manifest_path = package_root.join("Cargo.toml");
    let manifest = read_manifest(&manifest_path)?;
    let mut dependencies = Vec::new();
    collect_dependency_tables(&manifest, &mut dependencies);
    let workspace_dependencies = workspace_manifest
        .get("workspace")
        .and_then(toml::Value::as_table)
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml::Value::as_table);
    for occurrence in dependencies {
        let key = occurrence.key.clone();
        let dependency = occurrence.value.clone();
        let effective = if dependency.as_table().is_some_and(|table| {
            table.get("workspace").and_then(toml::Value::as_bool) == Some(true)
        }) {
            workspace_dependencies.and_then(|table| table.get(&key))
        } else {
            Some(&dependency)
        };
        let Some(path_value) = effective
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get("path"))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let uses_workspace = !effective.is_some_and(|value| std::ptr::eq(value, &dependency));
        let dependency_base = if !uses_workspace {
            package_root
        } else {
            workspace_root
        };
        let dependency_root = safe_dependency_path(dependency_base, path_value).map_err(|_| {
            PublicationError::UnsafeAssetPath {
                reference: path_value.to_owned(),
                definition: manifest_path.clone(),
                root: dependency_base.to_owned(),
            }
        })?;
        if !dependency_root.is_dir() {
            return Err(PublicationError::MissingAsset {
                reference: path_value.to_owned(),
                definition: manifest_path.clone(),
            }
            .into());
        }
        let canonical =
            dependency_root
                .canonicalize()
                .map_err(|source| PublicationError::CaptureSource {
                    path: dependency_root.clone(),
                    source,
                })?;
        let owner = owning_workspace_root(&canonical)?;
        let (staged_dependency, next_workspace_root, next_workspace_manifest, next_staged_manifest) =
            match owner {
                Some(owner) if owner == workspace_root => {
                    let owner_staging_root = state
                        .locations
                        .get(workspace_root)
                        .cloned()
                        .unwrap_or_else(|| staging_root.to_owned());
                    let staged = if let Some(staged) = state.locations.get(&canonical) {
                        if canonical == workspace_root {
                            let staged_manifest = staged_workspace_manifest.ok_or_else(|| {
                                PublicationError::CaptureSource {
                                    path: manifest_path.clone(),
                                    source: io::Error::other(
                                        "workspace root package has no captured workspace manifest",
                                    ),
                                }
                            })?;
                            let manifest_bytes = fs::read(staged_manifest).map_err(|source| {
                                PublicationError::CaptureSource {
                                    path: staged_manifest.to_owned(),
                                    source,
                                }
                            })?;
                            validate_cargo_config(&canonical.join(".cargo"))?;
                            copy_tree(&canonical, staged, false)?;
                            write_staged_file(staged_manifest, &manifest_bytes)?;
                        }
                        staged.clone()
                    } else {
                        let relative = canonical
                            .strip_prefix(workspace_root)
                            .map(PathBuf::from)
                            .map_err(|_| PublicationError::CaptureSource {
                                path: canonical.clone(),
                                source: io::Error::other(
                                    "path dependency escapes staging workspace",
                                ),
                            })?;
                        let staged = owner_staging_root.join(relative);
                        state.locations.insert(canonical.clone(), staged.clone());
                        validate_cargo_config(&canonical.join(".cargo"))?;
                        copy_tree(&canonical, &staged, false)?;
                        staged
                    };
                    (
                        staged,
                        workspace_root.to_owned(),
                        workspace_manifest.clone(),
                        staged_workspace_manifest.map(Path::to_path_buf),
                    )
                }
                Some(owner) => {
                    let (staged, manifest, staged_manifest) =
                        capture_external_workspace(&owner, &canonical, staging_root, state)?;
                    (staged, owner, manifest, Some(staged_manifest))
                }
                None => {
                    let staged = if let Some(staged) = state.locations.get(&canonical) {
                        staged.clone()
                    } else if canonical.starts_with(workspace_root) {
                        let relative = canonical
                            .strip_prefix(workspace_root)
                            .map(PathBuf::from)
                            .map_err(|_| PublicationError::CaptureSource {
                                path: canonical.clone(),
                                source: io::Error::other(
                                    "path dependency escapes staging workspace",
                                ),
                            })?;
                        let staged = staging_root.join(relative);
                        state.locations.insert(canonical.clone(), staged.clone());
                        validate_cargo_config(&canonical.join(".cargo"))?;
                        copy_tree(&canonical, &staged, false)?;
                        staged
                    } else {
                        let destination = state.external_root(staging_root);
                        validate_cargo_config(&canonical.join(".cargo"))?;
                        copy_tree(&canonical, &destination, false)?;
                        state.excluded_paths.insert(destination.clone());
                        destination
                    };
                    state.locations.insert(canonical.clone(), staged.clone());
                    if !staged.exists() {
                        validate_cargo_config(&canonical.join(".cargo"))?;
                        copy_tree(&canonical, &staged, false)?;
                    }
                    (
                        staged,
                        canonical.clone(),
                        toml::Value::Table(toml::map::Map::new()),
                        None,
                    )
                }
            };
        let staged_path_base = if uses_workspace {
            staged_workspace_manifest
                .and_then(Path::parent)
                .ok_or_else(|| PublicationError::CaptureSource {
                    path: manifest_path.clone(),
                    source: io::Error::other(
                        "workspace path dependency has no captured workspace manifest",
                    ),
                })?
        } else {
            staged_package_root
        };
        let staged_path = relative_path(staged_path_base, &staged_dependency).ok_or_else(|| {
            PublicationError::CaptureSource {
                path: staged_dependency.clone(),
                source: io::Error::other("staged path dependency is outside the capture root"),
            }
        })?;
        if uses_workspace {
            let workspace_manifest =
                staged_workspace_manifest.ok_or_else(|| PublicationError::CaptureSource {
                    path: manifest_path.clone(),
                    source: io::Error::other(
                        "workspace path dependency has no captured workspace manifest",
                    ),
                })?;
            rewrite_manifest_dependency_occurrence(
                workspace_manifest,
                &["workspace".to_owned(), "dependencies".to_owned()],
                &key,
                &staged_path,
            )?;
        } else {
            rewrite_manifest_dependency_occurrence(
                &staged_package_root.join("Cargo.toml"),
                &occurrence.table_path,
                &key,
                &staged_path,
            )?;
        }
        capture_path_dependencies(
            &canonical,
            &staged_dependency,
            &next_workspace_root,
            &next_workspace_manifest,
            staging_root,
            next_staged_manifest.as_deref(),
            state,
        )?;
    }
    Ok(())
}

fn rewrite_manifest_dependency_occurrence(
    manifest: &Path,
    table_path: &[String],
    key: &str,
    path: &Path,
) -> Result<(), Error> {
    let mut value = read_manifest(manifest)?;
    let changed = rewrite_dependency_table_at(&mut value, table_path, key, path);
    if !changed {
        return Err(PublicationError::CaptureSource {
            path: manifest.to_owned(),
            source: io::Error::other(format!(
                "captured Cargo manifest has no path dependency named '{key}'"
            )),
        }
        .into());
    }
    let text = toml::to_string_pretty(&value)
        .map_err(|source| PublicationError::SerializeStagedManifest { source })?;
    write_staged_file(manifest, text.as_bytes())
}

fn rewrite_dependency_table_at(
    value: &mut toml::Value,
    table_path: &[String],
    key: &str,
    path: &Path,
) -> bool {
    let Some((last, parents)) = table_path.split_last() else {
        return false;
    };
    let mut current = value;
    for parent in parents {
        let Some(next) = current.get_mut(parent) else {
            return false;
        };
        current = next;
    }
    let Some(table) = current.get_mut(last) else {
        return false;
    };
    rewrite_dependency_table(table, key, path)
}

fn rewrite_dependency_table(value: &mut toml::Value, key: &str, path: &Path) -> bool {
    let Some(table) = value.as_table_mut() else {
        return false;
    };
    let Some(dependency) = table.get_mut(key).and_then(toml::Value::as_table_mut) else {
        return false;
    };
    if dependency.get("path").is_none() {
        return false;
    }
    dependency.insert("path".to_owned(), toml::Value::String(path_string(path)));
    true
}

fn relative_path(from: &Path, to: &Path) -> Option<PathBuf> {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return None;
    }
    let mut relative = PathBuf::new();
    for component in &from[common..] {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to[common..] {
        if let Component::Normal(component) = component {
            relative.push(component);
        }
    }
    Some(relative)
}

fn collect_dependency_tables(manifest: &toml::Value, dependencies: &mut Vec<DependencyOccurrence>) {
    collect_dependency_tables_at(manifest, dependencies, &[]);
}

fn collect_dependency_tables_at(
    manifest: &toml::Value,
    dependencies: &mut Vec<DependencyOccurrence>,
    prefix: &[String],
) {
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(section).and_then(toml::Value::as_table) {
            let mut table_path = prefix.to_vec();
            table_path.push(section.to_owned());
            dependencies.extend(table.iter().map(|(key, value)| DependencyOccurrence {
                table_path: table_path.clone(),
                key: key.clone(),
                value: value.clone(),
            }));
        }
    }
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for (target_name, target) in targets {
            let mut target_path = prefix.to_vec();
            target_path.push("target".to_owned());
            target_path.push(target_name.clone());
            collect_dependency_tables_at(target, dependencies, &target_path);
        }
    }
    if let Some(overrides) = manifest.get("patch").and_then(toml::Value::as_table) {
        for (namespace, table) in overrides {
            let mut table_path = prefix.to_vec();
            table_path.extend(["patch".to_owned(), namespace.clone()]);
            dependencies.extend(table.as_table().into_iter().flat_map(|table| {
                table.iter().map(|(key, value)| DependencyOccurrence {
                    table_path: table_path.clone(),
                    key: key.clone(),
                    value: value.clone(),
                })
            }));
        }
    }
    if let Some(replacements) = manifest.get("replace").and_then(toml::Value::as_table) {
        let mut table_path = prefix.to_vec();
        table_path.push("replace".to_owned());
        dependencies.extend(
            replacements
                .iter()
                .map(|(key, value)| DependencyOccurrence {
                    table_path: table_path.clone(),
                    key: key.clone(),
                    value: value.clone(),
                }),
        );
    }
}

fn copy_tree(source: &Path, destination: &Path, optional: bool) -> Result<(), Error> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(source_error) if optional && source_error.kind() == io::ErrorKind::NotFound => {
            return Ok(());
        }
        Err(source_error) => {
            return Err(PublicationError::CaptureSource {
                path: source.to_owned(),
                source: source_error,
            }
            .into());
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(PublicationError::SymbolicLink {
            path: source.to_owned(),
        }
        .into());
    }
    if metadata.is_file() {
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|error| PublicationError::CaptureSource {
                path: parent.to_owned(),
                source: error,
            })?;
        }
        fs::copy(source, destination).map_err(|error| PublicationError::CaptureSource {
            path: source.to_owned(),
            source: error,
        })?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(PublicationError::CaptureSource {
            path: source.to_owned(),
            source: io::Error::other("publication source is not a regular file or directory"),
        }
        .into());
    }
    fs::create_dir_all(destination).map_err(|error| PublicationError::CaptureSource {
        path: destination.to_owned(),
        source: error,
    })?;
    let mut entries = fs::read_dir(source)
        .map_err(|error| PublicationError::CaptureSource {
            path: source.to_owned(),
            source: error,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| PublicationError::CaptureSource {
            path: source.to_owned(),
            source: error,
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name == ".git"
            || name == ".cargo-ok"
            || name == ".cargo_vcs_info.json"
            || name == "credentials"
            || name == "credentials.toml"
            || name == "target"
            || name == ".codex"
            || name == ".DS_Store"
        {
            continue;
        }
        copy_tree(&entry.path(), &destination.join(name), false)?;
    }
    Ok(())
}

fn validate_cargo_config(root: &Path) -> Result<(), Error> {
    if !root.is_dir() {
        return Ok(());
    }
    for path in walk_files(root)? {
        if path.extension().is_none_or(|extension| extension != "toml") {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|source| PublicationError::CaptureSource {
            path: path.clone(),
            source,
        })?;
        if text.lines().any(|line| {
            let line = line.trim_start().to_ascii_lowercase();
            line.starts_with("token") || line.starts_with("password") || line.starts_with("secret")
        }) {
            return Err(PublicationError::UnsafeCargoConfiguration {
                path,
                message: "Cargo configuration contains credential material".to_owned(),
            }
            .into());
        }
    }
    Ok(())
}

fn stage_package(
    selected: &SelectedPackage,
    captured: &CapturedSource,
) -> Result<BTreeSet<String>, Error> {
    exclude_captured_dependencies(selected, captured)?;
    let definition = definition_for_package(selected)?;
    let mut expected = BTreeSet::new();
    if let Some(definition) = definition.as_ref() {
        let relative = definition
            .strip_prefix(&selected.source_root)
            .map_err(|_| PublicationError::UnsafeAssetPath {
                reference: definition.display().to_string(),
                definition: selected.manifest.clone(),
                root: selected.source_root.clone(),
            })?;
        expected.insert(path_string(relative));
        let assets = asset_closure(&selected.source_root, definition)?;
        for asset in assets {
            let relative = asset.strip_prefix(&selected.source_root).map_err(|_| {
                PublicationError::UnsafeAssetPath {
                    reference: asset.display().to_string(),
                    definition: definition.to_owned(),
                    root: selected.source_root.clone(),
                }
            })?;
            expected.insert(path_string(relative));
        }
    }
    if selected.role == PackageRole::PassiveComponent {
        let authored_files = walk_files(&selected.source_root)?;
        if let Some(rust_file) = authored_files.iter().find(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("rs"))
        }) {
            return Err(PublicationError::TargetlessRustSource {
                package: selected.package.clone(),
                path: rust_file.clone(),
            }
            .into());
        }
        let mut manifest = read_manifest(&captured.manifest)?;
        {
            let package = manifest
                .get_mut("package")
                .and_then(toml::Value::as_table_mut)
                .ok_or_else(|| PublicationError::MissingPackageManifest {
                    path: selected.manifest.clone(),
                })?;
            package.insert(
                "publish".to_owned(),
                toml::Value::Array(vec![toml::Value::String("phoxal".to_owned())]),
            );
        }
        let staged_manifest = captured.manifest.clone();
        let root =
            staged_manifest
                .parent()
                .ok_or_else(|| PublicationError::MissingPackageManifest {
                    path: staged_manifest.clone(),
                })?;
        let definition = definition.as_ref();
        let mut include = expected
            .iter()
            .cloned()
            .map(toml::Value::String)
            .collect::<Vec<_>>();
        include.push(toml::Value::String(GENERATED_LIB.to_owned()));
        include.sort_by(|left, right| left.as_str().cmp(&right.as_str()));
        include.dedup();
        {
            let manifest_table = manifest.as_table_mut().ok_or_else(|| {
                PublicationError::MissingPackageManifest {
                    path: staged_manifest.clone(),
                }
            })?;
            manifest_table.insert(
                "lib".to_owned(),
                toml::Value::Table(toml::map::Map::from_iter([(
                    "path".to_owned(),
                    toml::Value::String(GENERATED_LIB.to_owned()),
                )])),
            );
            let package = manifest_table
                .get_mut("package")
                .and_then(toml::Value::as_table_mut)
                .ok_or_else(|| PublicationError::MissingPackageManifest {
                    path: staged_manifest.clone(),
                })?;
            package.insert("include".to_owned(), toml::Value::Array(include));
        }
        {
            let package = manifest
                .get_mut("package")
                .and_then(toml::Value::as_table_mut)
                .ok_or_else(|| PublicationError::MissingPackageManifest {
                    path: selected.manifest.clone(),
                })?;
            let metadata = package
                .entry("metadata".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            let phoxal = metadata
                .as_table_mut()
                .ok_or_else(|| PublicationError::MissingPackageManifest {
                    path: staged_manifest.clone(),
                })?
                .entry("phoxal".to_owned())
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            let phoxal =
                phoxal
                    .as_table_mut()
                    .ok_or_else(|| PublicationError::MissingPackageManifest {
                        path: staged_manifest.clone(),
                    })?;
            phoxal.insert(
                "kind".to_owned(),
                toml::Value::String("component".to_owned()),
            );
            if let Some(definition) = definition {
                phoxal.insert(
                    "definition".to_owned(),
                    toml::Value::String(path_string(
                        definition
                            .strip_prefix(&selected.source_root)
                            .map_err(|_| PublicationError::UnsafeAssetPath {
                                reference: definition.display().to_string(),
                                definition: selected.manifest.clone(),
                                root: selected.source_root.clone(),
                            })?,
                    )),
                );
            }
        }
        let text = toml::to_string_pretty(&manifest)
            .map_err(|source| PublicationError::SerializeStagedManifest { source })?;
        fs::write(&staged_manifest, text).map_err(|source| PublicationError::WriteStagedFile {
            path: staged_manifest.clone(),
            source,
        })?;
        let generated = root.join(GENERATED_LIB);
        if let Some(parent) = generated.parent() {
            fs::create_dir_all(parent).map_err(|source| PublicationError::WriteStagedFile {
                path: parent.to_owned(),
                source,
            })?;
        }
        fs::write(
            &generated,
            "//! Tool-generated inert carrier for a passive Phoxal package.\n#![no_std]\n\n",
        )
        .map_err(|source| PublicationError::WriteStagedFile {
            path: generated,
            source,
        })?;
        expected.insert(GENERATED_LIB.to_owned());
    }
    Ok(expected)
}

fn exclude_captured_dependencies(
    selected: &SelectedPackage,
    captured: &CapturedSource,
) -> Result<(), Error> {
    let package_root =
        captured
            .manifest
            .parent()
            .ok_or_else(|| PublicationError::MissingPackageManifest {
                path: captured.manifest.clone(),
            })?;
    let mut exclusions = captured
        .excluded_paths
        .iter()
        .filter_map(|path| relative_path(package_root, path))
        .filter(|path| {
            !path
                .components()
                .any(|component| component == Component::ParentDir)
        })
        .map(|path| format!("{}/**", path_string(&path)))
        .collect::<BTreeSet<_>>();
    if exclusions.is_empty() {
        return Ok(());
    }
    let mut manifest = read_manifest(&captured.manifest)?;
    let package = manifest
        .get_mut("package")
        .and_then(toml::Value::as_table_mut)
        .ok_or_else(|| PublicationError::MissingPackageManifest {
            path: selected.manifest.clone(),
        })?;
    let existing = package
        .get("exclude")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    exclusions.extend(existing);
    package.insert(
        "exclude".to_owned(),
        toml::Value::Array(exclusions.into_iter().map(toml::Value::String).collect()),
    );
    let text = toml::to_string_pretty(&manifest)
        .map_err(|source| PublicationError::SerializeStagedManifest { source })?;
    write_staged_file(&captured.manifest, text.as_bytes())
}

fn definition_for_package(selected: &SelectedPackage) -> Result<Option<PathBuf>, Error> {
    match selected.role {
        PackageRole::PassiveComponent | PackageRole::RustComponent => {
            Ok(Some(require_component_definition(
                &selected.source_root,
                &selected.package,
                &selected.manifest_value,
            )?))
        }
        PackageRole::Service
        | PackageRole::Library
        | PackageRole::ProcMacro
        | PackageRole::SimulatorApplication
        | PackageRole::Application
        | PackageRole::Tool => Ok(None),
    }
}

fn asset_closure(root: &Path, _definition: &Path) -> Result<BTreeSet<PathBuf>, Error> {
    let canonical = root.canonicalize().map_err(|source| Error::ArtifactFile {
        path: root.to_owned(),
        source,
    })?;
    Ok(super::component_assets::read(root)?
        .into_iter()
        .map(|relative| canonical.join(relative))
        .collect())
}

fn safe_source_path(root: &Path, reference: &str) -> Result<PathBuf, ()> {
    let relative = Path::new(reference);
    if reference.is_empty()
        || relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(());
    }
    let canonical_root = root.canonicalize().map_err(|_| ())?;
    let mut path = canonical_root.clone();
    for component in relative.components() {
        path.push(component.as_os_str());
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Err(()),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err(()),
        }
    }
    if path.exists()
        && !path
            .canonicalize()
            .map_err(|_| ())?
            .starts_with(&canonical_root)
    {
        return Err(());
    }
    Ok(path)
}

fn safe_dependency_path(root: &Path, reference: &str) -> Result<PathBuf, ()> {
    let relative = Path::new(reference);
    if relative.is_absolute()
        || relative
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
    {
        return Err(());
    }
    let path = root.join(relative);
    if !path.exists() {
        return Ok(path);
    }
    path.canonicalize().map_err(|_| ())
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let metadata =
        fs::symlink_metadata(root).map_err(|source| PublicationError::CaptureSource {
            path: root.to_owned(),
            source,
        })?;
    if metadata.file_type().is_symlink() {
        return Err(PublicationError::SymbolicLink {
            path: root.to_owned(),
        }
        .into());
    }
    if metadata.is_file() {
        return Ok(vec![root.to_owned()]);
    }
    let mut entries = fs::read_dir(root)
        .map_err(|source| PublicationError::CaptureSource {
            path: root.to_owned(),
            source,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| PublicationError::CaptureSource {
            path: root.to_owned(),
            source,
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.path();
        if entry.file_name() == ".git"
            || entry.file_name() == ".cargo-ok"
            || entry.file_name() == ".cargo_vcs_info.json"
            || entry.file_name() == "credentials"
            || entry.file_name() == "credentials.toml"
            || entry.file_name() == "target"
            || entry.file_name() == ".codex"
        {
            continue;
        }
        files.extend(walk_files(&path)?);
    }
    Ok(files)
}

fn package_with_cargo(package: &str, captured: &CapturedSource) -> Result<PathBuf, Error> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let target = captured.workspace_root.join("target");
    let mut command = Command::new(cargo);
    command
        .current_dir(&captured.workspace_root)
        .args([
            "package",
            "--manifest-path",
            &captured.manifest.display().to_string(),
            "--allow-dirty",
            "--target-dir",
            &target.display().to_string(),
        ])
        .env_remove("CARGO_REGISTRIES_PHOXAL_TOKEN")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_TOKEN")
        .env_remove("PHOXAL_GITHUB_TOKEN");
    let output = command
        .output()
        .map_err(|source| PublicationError::CargoPackage {
            package: package.to_owned(),
            message: source.to_string(),
        })?;
    if !output.status.success() {
        let mut message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if message.is_empty() {
            message = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        }
        return Err(PublicationError::CargoPackage {
            package: package.to_owned(),
            message,
        }
        .into());
    }
    let archive = target.join("package").join(format!(
        "{package}-{}.crate",
        package_version(&captured.manifest)?
    ));
    if !archive.is_file() {
        return Err(PublicationError::MissingArchive {
            package: package.to_owned(),
            path: archive,
        }
        .into());
    }
    Ok(archive)
}

fn package_version(manifest: &Path) -> Result<String, Error> {
    read_manifest(manifest)?
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            PublicationError::MissingPackageField {
                path: manifest.to_owned(),
                field: "version",
            }
            .into()
        })
}

#[derive(Debug, Clone)]
struct VerifiedArchive {
    checksum: String,
    bytes: u64,
}

fn verify_archive(
    archive_path: &Path,
    package: &str,
    version: &str,
    role: PackageRole,
    expected_assets: &BTreeSet<String>,
) -> Result<VerifiedArchive, Error> {
    let archive_bytes = fs::metadata(archive_path)
        .map_err(|source| PublicationError::CaptureSource {
            path: archive_path.to_owned(),
            source,
        })?
        .len();
    if archive_bytes > MAX_ARCHIVE_BYTES {
        return Err(PublicationError::ArchiveTooLarge {
            path: archive_path.to_owned(),
            bytes: archive_bytes,
        }
        .into());
    }
    let checksum = archive_checksum(archive_path)?;
    let root_name = format!("{package}-{version}");
    let file = File::open(archive_path).map_err(|source| PublicationError::CaptureSource {
        path: archive_path.to_owned(),
        source,
    })?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut paths = BTreeSet::new();
    let mut cargo_manifest = None;
    let mut total = 0_u64;
    let entries = archive
        .entries()
        .map_err(|source| invalid_archive(archive_path, source))?;
    for entry in entries {
        let mut entry = entry.map_err(|source| invalid_archive(archive_path, source))?;
        let path = entry
            .path()
            .map_err(|source| invalid_archive(archive_path, source))?
            .into_owned();
        let relative = validate_archive_path(&path, &root_name).map_err(|message| {
            PublicationError::InvalidArchive {
                path: archive_path.to_owned(),
                message,
            }
        })?;
        let relative_string = path_string(&relative);
        if !paths.insert(relative_string.clone()) {
            return Err(PublicationError::InvalidArchive {
                path: archive_path.to_owned(),
                message: format!("duplicate archive path {relative_string}"),
            }
            .into());
        }
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(PublicationError::InvalidArchive {
                path: archive_path.to_owned(),
                message: format!("archive path {relative_string} is a link"),
            }
            .into());
        }
        if entry_type.is_dir() {
            continue;
        }
        if !entry_type.is_file() {
            return Err(PublicationError::InvalidArchive {
                path: archive_path.to_owned(),
                message: format!("archive path {relative_string} is not a regular file"),
            }
            .into());
        }
        let entry_bytes = entry.size();
        if entry_bytes > MAX_ENTRY_BYTES || total.saturating_add(entry_bytes) > MAX_ARCHIVE_BYTES {
            return Err(PublicationError::ArchiveTooLarge {
                path: archive_path.to_owned(),
                bytes: total.saturating_add(entry_bytes),
            }
            .into());
        }
        let mut buffer = [0_u8; 64 * 1024];
        let mut read_bytes = 0_u64;
        let mut manifest_bytes = Vec::new();
        loop {
            let count = entry
                .read(&mut buffer)
                .map_err(|source| invalid_archive(archive_path, source))?;
            if count == 0 {
                break;
            }
            read_bytes += count as u64;
            if read_bytes > entry_bytes || read_bytes > MAX_ENTRY_BYTES {
                return Err(PublicationError::InvalidArchive {
                    path: archive_path.to_owned(),
                    message: format!("archive entry {relative_string} exceeds its declared size"),
                }
                .into());
            }
            if relative_string == "Cargo.toml" {
                if manifest_bytes.len() + count > MAX_MANIFEST_BYTES as usize {
                    return Err(PublicationError::InvalidArchive {
                        path: archive_path.to_owned(),
                        message: "normalized Cargo.toml is too large".to_owned(),
                    }
                    .into());
                }
                manifest_bytes.extend_from_slice(&buffer[..count]);
            }
        }
        if read_bytes != entry_bytes {
            return Err(PublicationError::InvalidArchive {
                path: archive_path.to_owned(),
                message: format!("archive entry {relative_string} was truncated"),
            }
            .into());
        }
        total += read_bytes;
        if relative_string == "Cargo.toml" {
            cargo_manifest = Some(manifest_bytes);
        }
    }
    let Some(cargo_manifest) = cargo_manifest else {
        return Err(PublicationError::InvalidArchive {
            path: archive_path.to_owned(),
            message: "archive has no normalized Cargo.toml".to_owned(),
        }
        .into());
    };
    let manifest = toml::from_slice::<toml::Value>(&cargo_manifest).map_err(|source| {
        PublicationError::InvalidArchive {
            path: archive_path.to_owned(),
            message: format!("normalized Cargo.toml is invalid: {source}"),
        }
    })?;
    let archived_package = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str);
    let archived_version = manifest
        .get("package")
        .and_then(toml::Value::as_table)
        .and_then(|package| package.get("version"))
        .and_then(toml::Value::as_str);
    if archived_package != Some(package) || archived_version != Some(version) {
        return Err(PublicationError::InvalidArchive {
            path: archive_path.to_owned(),
            message: format!(
                "archive identity is {:?} {:?}, expected {package} {version}",
                archived_package, archived_version
            ),
        }
        .into());
    }
    let archived_paths = paths.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for expected in expected_assets {
        if !archived_paths.contains(expected.as_str()) {
            return Err(PublicationError::MissingArchivedAsset {
                package: package.to_owned(),
                asset: expected.clone(),
            }
            .into());
        }
    }
    if role == PackageRole::PassiveComponent && !archived_paths.contains(GENERATED_LIB) {
        return Err(PublicationError::MissingArchivedAsset {
            package: package.to_owned(),
            asset: GENERATED_LIB.to_owned(),
        }
        .into());
    }
    Ok(VerifiedArchive {
        checksum,
        bytes: archive_bytes,
    })
}

fn archive_checksum(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path).map_err(|source| PublicationError::CaptureSource {
        path: path.to_owned(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|source| PublicationError::CaptureSource {
                path: path.to_owned(),
                source,
            })?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn invalid_archive(path: &Path, source: impl std::fmt::Display) -> PublicationError {
    PublicationError::InvalidArchive {
        path: path.to_owned(),
        message: source.to_string(),
    }
}

fn validate_archive_path(path: &Path, root_name: &str) -> Result<PathBuf, String> {
    let mut components = path.components();
    match components.next() {
        Some(Component::Normal(root)) if root == root_name => {}
        Some(Component::Normal(root)) => {
            return Err(format!("archive root {root:?} does not match {root_name}"));
        }
        _ => return Err("archive path is absolute or has no package root".to_owned()),
    }
    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(component) => relative.push(component),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("unsafe archive path {path:?}"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err("archive contains an empty package path".to_owned());
    }
    Ok(relative)
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive_paths(result: &PublicationResult) -> Result<BTreeSet<String>, Error> {
        let file =
            File::open(result.archive()).map_err(|source| PublicationError::CaptureSource {
                path: result.archive().to_owned(),
                source,
            })?;
        let mut archive = Archive::new(GzDecoder::new(file));
        let root = format!("{}-{}/", result.package(), result.version());
        let mut paths = BTreeSet::new();
        for entry in archive
            .entries()
            .map_err(|source| invalid_archive(result.archive(), source))?
        {
            let entry = entry.map_err(|source| invalid_archive(result.archive(), source))?;
            let path = entry
                .path()
                .map_err(|source| invalid_archive(result.archive(), source))?;
            let path = path_string(&path);
            if let Some(relative) = path.strip_prefix(&root) {
                paths.insert(relative.to_owned());
            }
        }
        Ok(paths)
    }

    fn write(path: &Path, text: &str) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, text)
    }

    #[test]
    fn exact_package_selection_does_not_use_fuzzy_names() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"exact-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        let options = PublicationOptions {
            kind: PublicationKind::Service,
            name: "exact-packag".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        };
        let error = select_package(&options).expect_err("fuzzy names must fail");
        assert!(matches!(
            error,
            Error::Publication(PublicationError::PackageNameMismatch { .. })
        ));
        Ok(())
    }

    #[test]
    fn workspace_detection_keeps_root_packages_and_explicit_package_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let root_manifest = root.path().join("Cargo.toml");
        let member = root.path().join("member");
        write(
            &root_manifest,
            "[package]\nname = \"root-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\nmembers = [\"member\"]\n",
        )?;
        write(
            &member.join("Cargo.toml"),
            "[package]\nname = \"member-package\"\nversion = \"0.1.0\"\nedition = \"2024\"\nworkspace = \"..\"\n",
        )?;
        let root_context = find_workspace(root.path(), &root_manifest)?
            .ok_or("root package workspace was not detected")?;
        assert_eq!(root_context.root, root.path());
        assert_eq!(root_context.package_relative, Path::new("."));
        let member_manifest = member.join("Cargo.toml");
        let member_context =
            find_workspace(&member.canonicalize()?, &member_manifest.canonicalize()?)?
                .ok_or("explicit package workspace was not detected")?;
        assert_eq!(member_context.root, root.path().canonicalize()?);
        assert_eq!(member_context.package_relative, Path::new("member"));
        Ok(())
    }

    #[test]
    fn root_package_workspace_publication_is_packaged_as_one_member()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"root-passive\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\nmembers = []\n",
        )?;
        write(
            &directory.path().join("component.yaml"),
            "schema: phoxal/component/v0\nmodel: {file: model.xml, root_body: mount}\ncapabilities: {}\n",
        )?;
        write(
            &directory.path().join("model.xml"),
            "<mujoco><worldbody><body name=\"mount\"/></worldbody></mujoco>\n",
        )?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Component,
            name: "root-passive".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        })?;
        assert!(result.archive().is_file());
        assert!(archive_paths(&result)?.contains("Cargo.toml"));
        let staging = result.staging.as_ref().unwrap().path();
        assert!(!staging.join("review-inventory.json").exists());
        assert!(!staging.join("archive.sha256").exists());
        Ok(())
    }

    #[test]
    fn staged_sources_exclude_desktop_metadata_files() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"junk-carrying\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\nmembers = []\n",
        )?;
        write(
            &directory.path().join("component.yaml"),
            "schema: phoxal/component/v0\nmodel: {file: model.xml, root_body: mount}\ncapabilities: {}\n",
        )?;
        write(&directory.path().join(".DS_Store"), "junk")?;
        write(&directory.path().join("src/.DS_Store"), "junk")?;
        write(
            &directory.path().join("model.xml"),
            "<mujoco><worldbody><body name=\"mount\"/></worldbody></mujoco>\n",
        )?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Component,
            name: "junk-carrying".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        })?;
        let paths = archive_paths(&result)?;
        assert!(
            !paths.iter().any(|path| path.ends_with(".DS_Store")),
            "desktop metadata must not enter the archive: {paths:?}"
        );
        Ok(())
    }

    #[test]
    fn a_library_cannot_be_published_as_a_service() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"plain-library\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )?;
        write(
            &directory.path().join("src/lib.rs"),
            "pub struct Library;\n",
        )?;
        let options = PublicationOptions {
            kind: PublicationKind::Service,
            name: "plain-library".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        };
        let error =
            select_package(&options).expect_err("a library selected as a service must fail");
        assert!(matches!(
            error,
            Error::Publication(PublicationError::InvalidPackageShape { .. })
        ));
        Ok(())
    }

    #[test]
    fn publication_uses_the_explicit_library_command_role() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"owned-library\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"Owned library\"\nlicense = \"MIT\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )?;
        write(
            &directory.path().join("src/lib.rs"),
            "pub struct Library;\n",
        )?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Library,
            name: "owned-library".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        })?;
        assert_eq!(result.kind(), PublicationKind::Library);
        assert_eq!(result.registry_kind(), "library");
        Ok(())
    }

    #[test]
    fn every_registry_role_serializes_to_its_admission_spelling() {
        for (kind, expected) in [
            (PublicationKind::Component, "component"),
            (PublicationKind::Service, "service"),
            (PublicationKind::Library, "library"),
            (PublicationKind::ProcMacro, "proc-macro"),
            (PublicationKind::SimulatorApplication, "simulator"),
            (PublicationKind::Application, "application"),
            (PublicationKind::Tool, "tool"),
        ] {
            assert_eq!(
                serde_json::to_value(kind).expect("registry role serializes"),
                expected
            );
        }
    }

    #[test]
    fn component_archive_contains_only_explicit_model_and_assets()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"explicit-assets\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT\"\ndescription = \"Explicit assets proof\"\n",
        )?;
        write(
            &directory.path().join("component.yaml"),
            "schema: phoxal/component/v0\nmodel: {file: native-entry, root_body: mount}\ncapabilities: {}\nassets: [blobs]\n",
        )?;
        write(
            &directory.path().join("native-entry"),
            "<mujoco><worldbody><body name=\"mount\"/></worldbody></mujoco>\n",
        )?;
        write(
            &directory.path().join("blobs/extensionless"),
            "declared resource\n",
        )?;
        write(
            &directory.path().join("model.xml"),
            "unrelated model must not be included\n",
        )?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Component,
            name: "explicit-assets".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        })?;
        let paths = archive_paths(&result)?;
        assert!(paths.contains("native-entry"));
        assert!(paths.contains("blobs/extensionless"));
        assert!(
            !paths.contains("model.xml"),
            "conventional filenames are not declarations"
        );
        Ok(())
    }

    #[test]
    fn asset_paths_reject_parent_escape_and_missing_files() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("model.xml"),
            "<mujoco><worldbody><body name=\"mount\"/></worldbody></mujoco>\n",
        )?;
        let definition = directory.path().join("component.yaml");
        write(
            &definition,
            "schema: phoxal/component/v0\nmodel: {file: model.xml, root_body: mount}\ncapabilities: {}\nassets: [../outside.obj]\n",
        )?;
        let error =
            asset_closure(directory.path(), &definition).expect_err("parent escape must fail");
        assert!(matches!(
            error,
            Error::Publication(PublicationError::UnsafeAssetPath { .. })
        ));

        write(
            &definition,
            "schema: phoxal/component/v0\nmodel: {file: model.xml, root_body: mount}\ncapabilities: {}\nassets: [missing.obj]\n",
        )?;
        let error =
            asset_closure(directory.path(), &definition).expect_err("missing asset must fail");
        assert!(matches!(
            error,
            Error::Publication(PublicationError::MissingAsset { .. })
        ));
        Ok(())
    }

    #[test]
    fn wildcard_workspace_member_selection_is_deterministic()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"services/*\"]\n",
        )?;
        for (member, name) in [("one", "one-service"), ("two", "two-service")] {
            write(
                &directory
                    .path()
                    .join(format!("services/{member}/Cargo.toml")),
                &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n"),
            )?;
        }
        let members = workspace_members(
            directory.path(),
            &read_manifest(&directory.path().join("Cargo.toml"))?,
        )?;
        assert_eq!(members.len(), 2);
        assert!(members[0].ends_with("services/one"));
        Ok(())
    }

    #[test]
    fn passive_dry_run_packages_only_declared_content_and_keeps_authored_tree_unchanged()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"passive-caster\"\nversion = \"0.1.0\"\nedition = \"2024\"\ndescription = \"Passive caster\"\nlicense = \"MIT\"\n",
        )?;
        write(
            &directory.path().join("component.yaml"),
            "schema: phoxal/component/v0\nmodel: {file: assets/model.txt, root_body: mount}\ncapabilities: {}\nassets: [assets/model.txt]\n",
        )?;
        write(&directory.path().join("assets/model.txt"), "model\n")?;
        write(&directory.path().join("unrelated.txt"), "do not package\n")?;
        let before = snapshot_tree(directory.path())?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Component,
            name: "passive-caster".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        })?;
        let after = snapshot_tree(directory.path())?;
        assert_eq!(before, after);
        assert!(result.archive().is_file());
        let paths = archive_paths(&result)?;
        assert!(paths.contains(GENERATED_LIB));
        assert!(paths.contains("component.yaml"));
        assert!(paths.contains("assets/model.txt"));
        assert!(!paths.contains("unrelated.txt"));
        Ok(())
    }

    #[test]
    fn real_service_dry_run_preserves_the_rust_contract_binary()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"example-service\"\nversion = \"0.2.0\"\nedition = \"2024\"\ndescription = \"Example service\"\nlicense = \"MIT\"\n\n[[bin]]\nname = \"example-service\"\npath = \"src/main.rs\"\n",
        )?;
        write(&directory.path().join("src/main.rs"), "fn main() {}\n")?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Service,
            name: "example-service".to_owned(),
            path: Some(directory.path().to_owned()),
            dry_run: true,
        })?;
        assert_eq!(result.kind(), PublicationKind::Service);
        let paths = archive_paths(&result)?;
        assert!(paths.contains("src/main.rs"));
        assert!(!paths.contains(GENERATED_LIB));
        Ok(())
    }

    #[test]
    fn workspace_member_publication_captures_inherited_manifest_context()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[workspace]\nmembers = [\"services/example\"]\n[workspace.package]\nedition = \"2024\"\n",
        )?;
        write(&directory.path().join("LICENSE"), "Example license\n")?;
        write(
            &directory.path().join("services/example/Cargo.toml"),
            "[package]\nname = \"workspace-service\"\nversion = \"0.3.0\"\nedition.workspace = true\nlicense-file = \"../../LICENSE\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[[bin]]\nname = \"workspace-service\"\npath = \"src/main.rs\"\n",
        )?;
        write(
            &directory.path().join("services/example/src/main.rs"),
            "fn main() {}\n",
        )?;
        write(
            &directory.path().join("services/example/src/lib.rs"),
            "pub struct Service;\n",
        )?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Tool,
            name: "workspace-service".to_owned(),
            path: Some(directory.path().join("services/example")),
            dry_run: true,
        })?;
        let paths = archive_paths(&result)?;
        assert!(paths.contains("src/main.rs"));
        assert!(paths.contains("LICENSE"));
        Ok(())
    }

    #[test]
    fn nested_tool_publication_captures_its_parent_package_dependency()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        write(
            &directory.path().join("Cargo.toml"),
            "[package]\nname = \"itoa\"\nversion = \"1.0.18\"\nedition = \"2024\"\nlicense = \"MIT OR Apache-2.0\"\npublish = false\n\n[workspace]\nmembers = [\"cargo\"]\n",
        )?;
        write(
            &directory.path().join("src/lib.rs"),
            "pub struct Buffer;\nimpl Buffer { pub const fn new() -> Self { Self } }\n",
        )?;
        write(
            &directory.path().join("cargo/Cargo.toml"),
            "[package]\nname = \"publication-tool\"\nversion = \"0.1.0\"\nedition = \"2024\"\nlicense = \"MIT\"\nworkspace = \"..\"\n\n[[bin]]\nname = \"publication-tool\"\npath = \"src/main.rs\"\n\n[dependencies]\npublication-sdk = { package = \"itoa\", path = \"..\", version = \"1.0.18\" }\n",
        )?;
        write(
            &directory.path().join("cargo/src/main.rs"),
            "fn main() { let _ = publication_sdk::Buffer::new(); }\n",
        )?;
        let result = prepare_publication(&PublicationOptions {
            kind: PublicationKind::Tool,
            name: "publication-tool".to_owned(),
            path: Some(directory.path().join("cargo")),
            dry_run: true,
        })?;
        assert!(archive_paths(&result)?.contains("src/main.rs"));
        Ok(())
    }

    #[test]
    fn workspace_publication_relocates_nested_external_paths_and_runs_cargo()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let parent = directory
            .path()
            .parent()
            .ok_or("publication fixture has no temporary parent")?;
        let leaf = tempfile::Builder::new()
            .prefix("phoxal-publication-leaf-")
            .tempdir_in(parent)?;
        let helper = tempfile::Builder::new()
            .prefix("phoxal-publication-helper-")
            .tempdir_in(parent)?;
        let leaf_name = leaf
            .path()
            .file_name()
            .ok_or("leaf has no directory name")?
            .to_string_lossy();
        let helper_name = helper
            .path()
            .file_name()
            .ok_or("helper has no directory name")?
            .to_string_lossy();
        let workspace_manifest =
            "[workspace]\nmembers = [\"services/example\"]\n[workspace.package]\nedition = \"2024\"\n\n[workspace.dependencies]\npublication-helper = { path = \"../HELPER\", version = \"0.1.0\" }\n"
                .replace("../HELPER", &format!("../{helper_name}"));
        write(&directory.path().join("Cargo.toml"), &workspace_manifest)?;
        write(
            &directory.path().join("services/example/Cargo.toml"),
            "[package]\nname = \"publication-workspace-service\"\nversion = \"0.1.0\"\nedition.workspace = true\n\n[lib]\npath = \"src/lib.rs\"\n\n[[bin]]\nname = \"publication-workspace-service\"\npath = \"src/main.rs\"\n\n[dependencies]\npublication-helper = { workspace = true }\n",
        )?;
        write(
            &directory.path().join("services/example/src/lib.rs"),
            "pub fn value() -> u32 { publication_helper::value() }\n",
        )?;
        write(
            &directory.path().join("services/example/src/main.rs"),
            "fn main() { let _ = publication_helper::value(); }\n",
        )?;
        write(
            &leaf.path().join("Cargo.toml"),
            "[package]\nname = \"publication-leaf\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )?;
        write(
            &leaf.path().join("src/lib.rs"),
            "pub const VALUE: u32 = 11;\n",
        )?;
        write(
            &helper.path().join("Cargo.toml"),
            &format!(
                "[package]\nname = \"publication-helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[dependencies]\npublication-leaf = {{ path = \"../{leaf_name}\", version = \"0.1.0\" }}\n"
            ),
        )?;
        write(
            &helper.path().join("src/lib.rs"),
            "pub fn value() -> u32 { publication_leaf::VALUE }\n",
        )?;

        let options = PublicationOptions {
            kind: PublicationKind::Tool,
            name: "publication-workspace-service".to_owned(),
            path: Some(directory.path().join("services/example")),
            dry_run: true,
        };
        let selected = selected_from_manifest(
            directory.path().join("services/example/Cargo.toml"),
            &options,
        )?;
        let staging = tempfile::Builder::new()
            .prefix("phoxal-publication-capture-")
            .tempdir()?;
        let captured = capture_source(&selected, staging.path())?;
        let staging_root = captured.workspace_root.as_path();
        let workspace_manifest = fs::read_to_string(staging_root.join("Cargo.toml"))?;
        let member_manifest = fs::read_to_string(staging_root.join("services/example/Cargo.toml"))?;
        assert!(workspace_manifest.contains("_phoxal_path_dependencies/"));
        assert!(!workspace_manifest.contains(&directory.path().display().to_string()));
        assert!(!workspace_manifest.contains(&helper.path().display().to_string()));
        assert!(member_manifest.contains("workspace = true"));
        let external_root = staging_root.join("_phoxal_path_dependencies");
        assert_eq!(fs::read_dir(external_root)?.count(), 2);

        let relocated = tempfile::tempdir()?;
        let relocated_root = relocated.path().join("capture");
        copy_tree(staging_root, &relocated_root, false)?;
        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let target = relocated.path().join("target");
        let output = Command::new(cargo)
            .current_dir(&relocated_root)
            .args([
                "check",
                "--offline",
                "--manifest-path",
                &relocated_root
                    .join("services/example/Cargo.toml")
                    .display()
                    .to_string(),
                "--target-dir",
                &target.display().to_string(),
            ])
            .output()?;
        assert!(
            output.status.success(),
            "relocated publication capture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[test]
    fn publication_captures_nested_external_workspace_inheritance()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let parent = directory
            .path()
            .parent()
            .ok_or("publication fixture has no temporary parent")?;
        let leaf = tempfile::Builder::new()
            .prefix("phoxal-publication-workspace-leaf-")
            .tempdir_in(parent)?;
        let external = tempfile::Builder::new()
            .prefix("phoxal-publication-workspace-external-")
            .tempdir_in(parent)?;
        let leaf_name = leaf
            .path()
            .file_name()
            .ok_or("leaf has no directory name")?
            .to_string_lossy();
        let external_name = external
            .path()
            .file_name()
            .ok_or("external workspace has no directory name")?
            .to_string_lossy();
        write(
            &directory.path().join("Cargo.toml"),
            &format!(
                "[workspace]\nmembers = [\"service\"]\n[workspace.package]\nedition = \"2024\"\n\n[workspace.dependencies]\nexternal-helper = {{ path = \"../{external_name}/packages/helper\" }}\n"
            ),
        )?;
        write(
            &directory.path().join("service/Cargo.toml"),
            "[package]\nname = \"publication-nested-workspace-service\"\nversion = \"0.1.0\"\nedition.workspace = true\n\n[lib]\npath = \"src/lib.rs\"\n\n[[bin]]\nname = \"publication-nested-workspace-service\"\npath = \"src/main.rs\"\n\n[dependencies]\nexternal-helper = { workspace = true }\n",
        )?;
        write(
            &directory.path().join("service/src/lib.rs"),
            "pub fn value() -> u32 { external_helper::value() }\n",
        )?;
        write(
            &directory.path().join("service/src/main.rs"),
            "fn main() { let _ = external_helper::value(); }\n",
        )?;
        write(
            &external.path().join("Cargo.toml"),
            &format!(
                "[workspace]\nmembers = [\"packages/helper\"]\n\n[workspace.dependencies]\nexternal-leaf = {{ path = \"../{leaf_name}\" }}\n"
            ),
        )?;
        write(
            &external.path().join("packages/helper/Cargo.toml"),
            "[package]\nname = \"external-helper\"\nversion = \"0.1.0\"\nedition = \"2024\"\nworkspace = \"../..\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[dependencies]\nexternal-leaf = { workspace = true }\n",
        )?;
        write(
            &external.path().join("packages/helper/src/lib.rs"),
            "pub fn value() -> u32 { external_leaf::VALUE }\n",
        )?;
        write(
            &leaf.path().join("Cargo.toml"),
            "[package]\nname = \"external-leaf\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[lib]\npath = \"src/lib.rs\"\n",
        )?;
        write(
            &leaf.path().join("src/lib.rs"),
            "pub const VALUE: u32 = 17;\n",
        )?;

        let options = PublicationOptions {
            kind: PublicationKind::Tool,
            name: "publication-nested-workspace-service".to_owned(),
            path: Some(directory.path().join("service")),
            dry_run: true,
        };
        let selected =
            selected_from_manifest(directory.path().join("service/Cargo.toml"), &options)?;
        let staging = tempfile::Builder::new()
            .prefix("phoxal-publication-nested-workspace-")
            .tempdir()?;
        let captured = capture_source(&selected, staging.path())?;
        let root = captured.workspace_root.clone();
        let external_roots =
            fs::read_dir(root.join("_phoxal_path_dependencies"))?.collect::<Result<Vec<_>, _>>()?;
        assert_eq!(external_roots.len(), 2);
        let helper_manifest = walk_files(&root)
            .map_err(|error| io::Error::other(error.to_string()))?
            .into_iter()
            .find(|path| {
                path.file_name().and_then(|name| name.to_str()) == Some("Cargo.toml")
                    && fs::read_to_string(path)
                        .is_ok_and(|text| text.contains("name = \"external-helper\""))
            })
            .ok_or("captured external helper manifest missing")?;
        let helper_text = fs::read_to_string(helper_manifest)?;
        assert!(helper_text.contains("workspace = \"../..\""));
        let relocated = tempfile::tempdir()?;
        let relocated_root = relocated.path().join("capture");
        copy_tree(&root, &relocated_root, false)?;
        let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
        let target = relocated.path().join("target");
        let output = Command::new(cargo)
            .current_dir(&relocated_root)
            .args([
                "check",
                "--offline",
                "--manifest-path",
                &relocated_root
                    .join("service/Cargo.toml")
                    .display()
                    .to_string(),
                "--target-dir",
                &target.display().to_string(),
            ])
            .output()?;
        assert!(
            output.status.success(),
            "relocated nested workspace capture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    fn snapshot_tree(root: &Path) -> Result<BTreeMap<String, Vec<u8>>, io::Error> {
        let mut snapshot = BTreeMap::new();
        for path in walk_files(root).map_err(|error| io::Error::other(error.to_string()))? {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| io::Error::other(error.to_string()))?;
            snapshot.insert(path_string(relative), fs::read(path)?);
        }
        Ok(snapshot)
    }
}
