//! Exact participant installation and compiled contract preparation.
use super::cargo::CargoOptions;
use super::document::{RobotDocument, Source};
use super::file_lock::ExclusiveFileLock;
use super::selection::PackageSource;
use super::{Error, ProjectLayout};
use fs4::TryLockError;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
#[derive(Debug, Clone)]
pub(crate) struct InstalledSelection {
    pub(crate) package: String,
    pub(crate) version: String,
    pub(crate) binary: String,
    pub(crate) executable: Option<PathBuf>,
    pub(crate) package_id: String,
    pub(crate) source_path: Option<PathBuf>,
    pub(crate) manifest_path: Option<PathBuf>,
    pub(crate) source_root: PathBuf,
    pub(crate) source: PackageSource,
}
pub(crate) fn selected_installations(
    layout: &ProjectLayout,
    document: &RobotDocument,
    options: &CargoOptions,
) -> Result<BTreeMap<String, InstalledSelection>, Error> {
    let home = phoxal_home()?;
    let target = options.target.clone().map_or_else(host_target, Ok)?;
    let RobotDocument::V0 {
        robot, services, ..
    } = document;
    let mut selected = BTreeMap::new();
    for (instance, selection) in services {
        selected.insert(
            instance.clone(),
            selected_installation(
                layout,
                &home,
                &target,
                SelectionRequest {
                    binary: selection.binary.as_deref(),
                    source: &selection.source,
                },
                options,
            )?,
        );
    }
    for (instance, component) in &robot.components {
        if component.driver.is_some() {
            selected.insert(
                instance.clone(),
                selected_installation(
                    layout,
                    &home,
                    &target,
                    SelectionRequest {
                        binary: component
                            .driver
                            .as_ref()
                            .and_then(|driver| driver.binary.as_deref()),
                        source: &component.source,
                    },
                    options,
                )?,
            );
        }
    }
    Ok(selected)
}
struct SelectionRequest<'a> {
    binary: Option<&'a str>,
    source: &'a Source,
}
/// A cache key for the complete resolved source and actual build selection.
pub(crate) fn installation_store(
    root: &Path,
    home: &Path,
    source: &Source,
    binary: Option<&str>,
    options: &CargoOptions,
) -> Result<PathBuf, Error> {
    use sha2::{Digest as _, Sha256};
    let selection = serde_json::to_vec(&(
        source,
        binary,
        super::cargo::BuildSelection::for_source(options),
    ))
    .map_err(|error| invalid(root, error.to_string()))?;
    Ok(home
        .join("packages/selections")
        .join(format!("{:x}", Sha256::digest(selection))))
}
fn selected_installation(
    layout: &ProjectLayout,
    home: &Path,
    _target: &str,
    request: SelectionRequest<'_>,
    options: &CargoOptions,
) -> Result<InstalledSelection, Error> {
    let SelectionRequest { binary, source } = request;
    if let Source::Path(path) = source {
        return local_selection(layout, Path::new(path), binary, options);
    }
    let store = installation_store(layout.root(), home, source, binary, options)?;
    let (package, source) = match source {
        Source::Git(git) => (
            git.name.as_str(),
            PackageSource::Git {
                source: format!("git+{}#{}", git.url, git.rev),
            },
        ),
        Source::Path(_) => unreachable!("handled above"),
    };
    let binary = binary.unwrap_or(package);
    let executable = store.join("bin").join(binary);
    let source_root = store.join("source");
    if !executable.is_file()
        || !store.join("package-id").is_file()
        || !store.join(".crates.toml").is_file()
    {
        return Err(invalid(
            layout.robot_manifest(),
            format!("{package} is not prepared; run `cargo phoxal prepare`"),
        ));
    }
    let version = fs::read_to_string(store.join("package-version")).map_err(|source| {
        Error::ArtifactFile {
            path: store.join("package-version"),
            source,
        }
    })?;
    Ok(InstalledSelection {
        package: package.to_owned(),
        version,
        binary: binary.to_owned(),
        executable: Some(executable),
        package_id: fs::read_to_string(store.join("package-id")).map_err(|source| {
            Error::ArtifactFile {
                path: store.join("package-id"),
                source,
            }
        })?,
        source_path: None,
        manifest_path: None,
        source_root,
        source,
    })
}
fn local_selection(
    layout: &ProjectLayout,
    path: &Path,
    binary: Option<&str>,
    options: &CargoOptions,
) -> Result<InstalledSelection, Error> {
    if path.as_os_str().is_empty() || !path.is_relative() {
        return Err(invalid(
            layout.robot_manifest(),
            "local source must be a nonempty relative path",
        ));
    }
    let source_root =
        layout
            .root()
            .join(path)
            .canonicalize()
            .map_err(|source| Error::ArtifactFile {
                path: layout.root().join(path),
                source,
            })?;
    package_selection(&source_root, binary, options)
}
/// Resolves the Cargo package at `source_root` and its selected binary.
fn package_selection(
    source_root: &Path,
    binary: Option<&str>,
    options: &CargoOptions,
) -> Result<InstalledSelection, Error> {
    let manifest_path = source_root.join("Cargo.toml");
    let mut local_options = options.clone();
    local_options.features.clear();
    local_options.all_features = false;
    local_options.no_default_features = false;
    local_options.cargo_args.clear();
    let metadata =
        super::cargo::load_metadata_at(&manifest_path, source_root, None, &local_options)?;
    let selected = metadata
        .packages
        .iter()
        .find(|candidate| candidate.manifest_path.as_std_path() == manifest_path)
        .ok_or_else(|| invalid(&manifest_path, "local participant is not a Cargo package"))?;
    let binary = binary.unwrap_or(&selected.name);
    let target = selected
        .targets
        .iter()
        .find(|target| target.name == binary && target.is_bin())
        .ok_or_else(|| {
            invalid(
                &manifest_path,
                format!("local participant has no binary `{binary}`"),
            )
        })?;
    Ok(InstalledSelection {
        package: selected.name.to_string(),
        version: selected.version.to_string(),
        binary: binary.to_owned(),
        executable: None,
        package_id: selected.id.to_string(),
        source_path: Some(target.src_path.as_std_path().to_owned()),
        manifest_path: Some(manifest_path.clone()),
        source_root: source_root.to_path_buf(),
        source: PackageSource::Local { manifest_path },
    })
}
/// Prepares the exact selected executable contracts and configured output families.
pub(crate) fn prepare(
    layout: &ProjectLayout,
    options: &CargoOptions,
    robot: &RobotDocument,
) -> Result<Vec<String>, Error> {
    options.validate()?;
    let _lock = preparation_lock(layout.root())?;
    let home = phoxal_home()?;
    let _installation_lock = installation_lock(&home)?;
    let target = match &options.target {
        Some(target) => target.clone(),
        None => host_target()?,
    };
    let RobotDocument::V0 {
        robot, services, ..
    } = robot;
    let root = layout
        .root()
        .canonicalize()
        .map_err(|source| Error::ArtifactFile {
            path: layout.root().to_owned(),
            source,
        })?;
    let is_self = |source: &Source| {
        matches!(
            source, Source::Path(path) if layout.root().join(Path::new(path))
            .canonicalize().is_ok_and(| resolved | resolved == root)
        )
    };
    let mut selections: Vec<(&String, &Source, Option<&str>, Option<&serde_json::Value>)> =
        services
            .iter()
            .map(|(instance, selection)| {
                (
                    instance,
                    &selection.source,
                    selection.binary.as_deref(),
                    selection.config.as_ref(),
                )
            })
            .chain(
                robot
                    .components
                    .iter()
                    .filter(|(_, component)| component.driver.is_some())
                    .map(|(instance, component)| {
                        (
                            instance,
                            &component.source,
                            component
                                .driver
                                .as_ref()
                                .and_then(|driver| driver.binary.as_deref()),
                            component
                                .driver
                                .as_ref()
                                .and_then(|driver| driver.config.as_ref()),
                        )
                    }),
            )
            .collect();
    selections.sort_by_key(|(_, source, _, _)| is_self(source));
    let mut changes = Vec::new();
    let mut seen = BTreeSet::new();
    for (instance, source, binary, config) in selections {
        if seen.insert(format!("{source:?}:{binary:?}"))
            && let Some(change) =
                prepare_selection(layout, options, &home, &target, instance, source, binary)?
        {
            changes.push(change);
        }
        let identity = match source {
            Source::Path(path) => phoxal_build::PreparedSelection::Path { path: path.clone() },
            Source::Git(git) => phoxal_build::PreparedSelection::Git {
                name: git.name.clone(),
                revision: git.rev.clone(),
                url: git.url.clone(),
                path: git.path.clone(),
            },
        };
        let directory = phoxal_build::prepared_dir(layout.root(), &identity, binary)?;
        let prepared = phoxal_build::read_prepared_for(&directory, &identity, binary)?;
        let runtime: phoxal::artifact::RuntimeRecord =
            serde_json::from_value(prepared.file.runtime.clone())
                .map_err(|error| invalid(&directory, error.to_string()))?;
        let phoxal::artifact::RuntimeRecord::V0 { outputs, .. } = &runtime;
        if outputs.iter().any(|output| output.family.is_some()) {
            let config = config.cloned().unwrap_or(serde_json::Value::Null);
            let resolved = runtime.resolve_outputs(&config).map_err(|error| {
                invalid(layout.robot_manifest(), format!("{instance}: {error}"))
            })?;
            phoxal_build::write_prepared_instance(
                layout.root(),
                instance,
                &config,
                &prepared.file,
                serde_json::to_value(resolved)
                    .map_err(|error| invalid(&directory, error.to_string()))?,
                &prepared.descriptors,
            )?;
        }
    }
    Ok(changes)
}
/// Builds one local Rust-contract participant and prepares its extracted
/// contract products under the robot's configured Cargo prepared-input store.
///
/// The prepared directory is keyed by the selection's declared source path
/// and binary spelling, so two binaries of one package keep distinct
/// products.
fn prepare_local_contract(
    layout: &ProjectLayout,
    options: &CargoOptions,
    path: &Path,
    declared_binary: Option<&str>,
    selection: &InstalledSelection,
) -> Result<(), Error> {
    let selection_identity = phoxal_build::PreparedSelection::Path {
        path: path.to_string_lossy().into_owned(),
    };
    let contract_dir =
        phoxal_build::prepared_dir(layout.root(), &selection_identity, declared_binary)?;
    prepare_selection_products(
        options,
        selection,
        &selection_identity,
        declared_binary,
        &contract_dir,
    )
    .map(|_| ())
}
/// Builds one selected participant and writes its extracted contract
/// products to `contract_dir`, skipping the write while the recorded
/// provenance still matches the compiled executable.
///
/// Returns whether the products were written afresh.
fn prepare_selection_products(
    options: &CargoOptions,
    selection: &InstalledSelection,
    selection_identity: &phoxal_build::PreparedSelection,
    selection_binary: Option<&str>,
    contract_dir: &Path,
) -> Result<bool, Error> {
    let manifest_path = selection
        .manifest_path
        .clone()
        .unwrap_or_else(|| selection.source_root.join("Cargo.toml"));
    let workdir = manifest_path
        .parent()
        .ok_or_else(|| Error::ArtifactCapture {
            package: selection.package.clone(),
            target: selection.binary.clone(),
            message: "local participant manifest has no parent".to_owned(),
        })?
        .to_owned();
    let mut command = Command::new(options.cargo_program());
    command
        .current_dir(&workdir)
        .args(["build", "--manifest-path"]);
    command.arg(&manifest_path);
    let mut local_options = options.clone();
    local_options.features.clear();
    local_options.all_features = false;
    local_options.no_default_features = false;
    local_options.cargo_args.clear();
    local_options.append_common(&mut command, false, false);
    command.args([
        "--package",
        &selection.package,
        "--bin",
        &selection.binary,
        "--message-format",
        "json",
    ]);
    let output = command.output().map_err(|source| Error::ArtifactFile {
        path: options.cargo_program(),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::CargoCommand {
            operation: "build".to_owned(),
            status: output.status.to_string(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let executable = artifact_executable(&output.stdout, &selection.binary).ok_or_else(|| {
        Error::ArtifactCapture {
            package: selection.package.clone(),
            target: selection.binary.clone(),
            message: "cargo reported no executable for the Rust-contract participant".to_owned(),
        }
    })?;
    let executable = workdir.join(executable.strip_prefix(&workdir).unwrap_or(&executable));
    prepare_executable_contract(
        &executable,
        &selection.package,
        &selection.version,
        selection_identity,
        selection_binary,
        contract_dir,
    )
}
/// Restores project-local metadata from an existing executable without
/// acquiring or compiling its package again.
fn prepare_executable_contract(
    executable: &Path,
    package: &str,
    version: &str,
    selection_identity: &phoxal_build::PreparedSelection,
    selection_binary: Option<&str>,
    contract_dir: &Path,
) -> Result<bool, Error> {
    let executable_record = phoxal_build::PreparedExecutable {
        package: package.to_owned(),
        version: Some(version.to_owned()),
    };
    let contract =
        super::artifact::inspect_file(executable).map_err(|error| Error::ContractPreparation {
            message: format!("cannot inspect {}: {error}", executable.display()),
        })?;
    let descriptors = merge_descriptor_closures(&contract)?;
    phoxal_build::write_prepared(
        contract_dir,
        selection_identity,
        selection_binary,
        &executable_record,
        serde_json::to_value(&contract.runtime).map_err(|error| Error::ContractPreparation {
            message: error.to_string(),
        })?,
        &descriptors,
    )
    .map_err(|error| Error::ContractPreparation {
        message: error.to_string(),
    })
}
/// Extracts the executable path of one binary from Cargo's JSON messages.
fn artifact_executable(stdout: &[u8], binary: &str) -> Option<PathBuf> {
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value["reason"] == "compiler-artifact"
            && value["target"]["name"] == binary
            && value["executable"].is_string()
        {
            return Some(PathBuf::from(value["executable"].as_str()?));
        }
    }
    None
}
/// Folds every retained descriptor closure of one artifact into a single
/// standard descriptor set, collapsing identical files and rejecting
/// conflicting definitions.
pub(crate) fn merge_descriptor_closures(
    contract: &super::artifact::ArtifactContract,
) -> Result<prost_types::FileDescriptorSet, Error> {
    use prost::Message as _;
    let mut files = Vec::<prost_types::FileDescriptorProto>::new();
    let mut by_name = BTreeMap::<String, Vec<u8>>::new();
    for descriptor in &contract.descriptors {
        let set =
            prost_types::FileDescriptorSet::decode(descriptor.raw_bytes()).map_err(|error| {
                Error::ContractPreparation {
                    message: format!("retained descriptor closure is malformed: {error}"),
                }
            })?;
        for file in set.file {
            let identity = file.name().to_owned();
            let encoded = file.encode_to_vec();
            match by_name.get(&identity) {
                Some(existing) if *existing == encoded => {}
                Some(_) => {
                    return Err(Error::ContractPreparation {
                        message: format!("conflicting retained descriptor file `{identity}`"),
                    });
                }
                None => {
                    by_name.insert(identity, encoded);
                    files.push(file);
                }
            }
        }
    }
    files.sort_by(|left, right| left.name().cmp(right.name()));
    Ok(prost_types::FileDescriptorSet { file: files })
}
fn prepare_selection(
    layout: &ProjectLayout,
    options: &CargoOptions,
    home: &Path,
    _target: &str,
    instance: &str,
    source: &Source,
    binary: Option<&str>,
) -> Result<Option<String>, Error> {
    let manifest = layout.robot_manifest();
    if !identifier(instance) || binary.is_some_and(|binary| !identifier(binary)) {
        return Err(invalid(
            manifest,
            format!("{instance} has an invalid participant or binary name"),
        ));
    }
    if let Source::Path(path) = source {
        let path = Path::new(path);
        if path.as_os_str().is_empty() || !path.is_relative() {
            return Err(invalid(
                manifest,
                format!("{instance} local source must be a nonempty relative path"),
            ));
        }
        let selection = local_selection(layout, path, binary, options)?;
        prepare_local_contract(layout, options, path, binary, &selection)?;
        return Ok(None);
    }
    let selection_identity = match source {
        Source::Git(git) => phoxal_build::PreparedSelection::Git {
            name: git.name.clone(),
            revision: git.rev.clone(),
            url: git.url.clone(),
            path: git.path.clone(),
        },
        Source::Path(_) => unreachable!("path selections are handled above"),
    };
    let store = installation_store(layout.root(), home, source, binary, options)?;
    let (package, expected_version) = match source {
        Source::Git(git) => (git.name.as_str(), None),
        Source::Path(_) => unreachable!("handled above"),
    };
    let declared_binary = binary;
    let binary = binary.unwrap_or(package);
    let installed = store.join("bin").join(binary);
    if installed.is_file()
        && store.join(".crates.toml").is_file()
        && store.join("package-id").is_file()
        && store.join("package-version").is_file()
    {
        let version_path = store.join("package-version");
        let version = fs::read_to_string(&version_path).map_err(|source| Error::ArtifactFile {
            path: version_path,
            source,
        })?;
        prepare_executable_contract(
            &installed,
            package,
            &version,
            &selection_identity,
            declared_binary,
            &phoxal_build::prepared_dir(layout.root(), &selection_identity, declared_binary)?,
        )?;
        return Ok(None);
    }
    let staging = home.join("packages/.staging");
    fs::create_dir_all(&staging).map_err(|source| Error::ArtifactFile {
        path: staging.clone(),
        source,
    })?;
    let install = tempfile::tempdir_in(&staging).map_err(|source| Error::ArtifactFile {
        path: staging.clone(),
        source,
    })?;
    let mut command = super::cargo::source_install_command(
        layout.root(),
        source,
        binary,
        install.path(),
        options,
    );
    let cache_key = store
        .file_name()
        .ok_or_else(|| invalid(&store, "selection store has no build-cache key"))?;
    command.env(
        "CARGO_TARGET_DIR",
        home.join("cache/participant-build").join(cache_key),
    );
    let operation = format!("install {package}");
    let output = command.output().map_err(|source| Error::CargoSpawn {
        operation: operation.clone(),
        source,
    })?;
    if !output.status.success() {
        return Err(Error::CargoCommand {
            operation,
            status: output
                .status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string()),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let (source_root, package_id, version) =
        captured_source(&output.stdout, binary, package, expected_version, options)?;
    for (name, value) in [
        ("package-id", package_id.as_str()),
        ("package-version", version.as_str()),
    ] {
        let path = install.path().join(name);
        fs::write(&path, value).map_err(|source| Error::ArtifactFile { path, source })?;
    }
    if let Source::Git(git) = source
        && let Some(path) = &git.path
        && !source_root.ends_with(path)
    {
        return Err(invalid(
            &source_root,
            format!("Git package {package} is not at selected path {path}"),
        ));
    }
    if !install.path().join("bin").join(binary).is_file() {
        return Err(invalid(
            install.path(),
            format!("Cargo did not install binary `{binary}`"),
        ));
    }
    retain_package_files(&source_root, install.path())?;
    prepare_executable_contract(
        &install.path().join("bin").join(binary),
        package,
        &version,
        &selection_identity,
        declared_binary,
        &phoxal_build::prepared_dir(layout.root(), &selection_identity, declared_binary)?,
    )?;
    if let Some(parent) = store.parent() {
        fs::create_dir_all(parent).map_err(|source| Error::ArtifactFile {
            path: parent.to_owned(),
            source,
        })?;
        if store.exists() {
            let retired = tempfile::tempdir_in(parent).map_err(|source| Error::ArtifactFile {
                path: parent.to_owned(),
                source,
            })?;
            let previous = retired.path().join("previous");
            fs::rename(&store, &previous).map_err(|source| Error::ArtifactFile {
                path: store.clone(),
                source,
            })?;
            if let Err(source) = fs::rename(install.path(), &store) {
                fs::rename(&previous, &store)
                    .map_err(|restore| {
                        invalid(
                            &store,
                            format!(
                                "cannot publish installation: {source}; cannot restore previous installation: {restore}"
                            ),
                        )
                    })?;
                return Err(Error::ArtifactFile {
                    path: store,
                    source,
                });
            }
            return Ok(Some(format!("{instance} {package} {version}")));
        }
    }
    fs::rename(install.path(), &store).map_err(|source| Error::ArtifactFile {
        path: store.clone(),
        source,
    })?;
    Ok(Some(format!("{instance} {package} {version}")))
}
pub(crate) fn captured_source(
    output: &[u8],
    binary: &str,
    package: &str,
    expected_version: Option<&str>,
    options: &CargoOptions,
) -> Result<(PathBuf, String, String), Error> {
    for line in output.split(|byte| *byte == b'\n') {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
            continue;
        };
        if value["reason"] != "compiler-artifact" || value["target"]["name"] != binary {
            continue;
        }
        if !value["target"]["kind"]
            .as_array()
            .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        {
            continue;
        }
        let Some(source) = value["target"]["src_path"].as_str() else {
            continue;
        };
        let Some(reported_id) = value["package_id"].as_str() else {
            continue;
        };
        for directory in Path::new(source).ancestors().skip(1) {
            let manifest = directory.join("Cargo.toml");
            if !manifest.is_file() {
                continue;
            }
            let text = fs::read_to_string(&manifest).map_err(|source| Error::ReadManifest {
                path: manifest.clone(),
                source,
            })?;
            let document =
                toml::from_str::<toml::Value>(&text).map_err(|source| Error::ParseManifest {
                    path: manifest.clone(),
                    source,
                })?;
            if document["package"]["name"].as_str() == Some(package) {
                let mut command = cargo_metadata::MetadataCommand::new();
                command.cargo_path(options.cargo_program());
                command
                    .manifest_path(&manifest)
                    .current_dir(directory)
                    .no_deps();
                if options.offline || matches!(options.lock, super::cargo::LockMode::Frozen) {
                    command.other_options(vec!["--offline".to_owned()]);
                }
                let metadata = command.exec().map_err(|error| {
                    invalid(
                        &manifest,
                        format!("cannot read Cargo package information: {error}"),
                    )
                })?;
                if let Some(candidate) = metadata.packages.iter().find(|candidate| {
                    candidate.manifest_path.as_std_path() == manifest
                        && candidate.name == package
                        && expected_version
                            .is_none_or(|version| candidate.version.to_string() == version)
                }) {
                    return Ok((
                        directory.to_owned(),
                        reported_id.to_owned(),
                        candidate.version.to_string(),
                    ));
                }
            }
        }
    }
    Err(invalid(
        Path::new("Cargo.toml"),
        format!(
            "Cargo did not report a binary artifact for package {package} {expected_version:?} ({binary})"
        ),
    ))
}
fn retain_package_files(source: &Path, installed: &Path) -> Result<(), Error> {
    let retained = installed.join("source");
    fs::create_dir_all(&retained).map_err(|error| Error::ArtifactFile {
        path: retained.clone(),
        source: error,
    })?;
    let component = source.join("component.yaml");
    if component.is_file() {
        fs::copy(&component, retained.join("component.yaml")).map_err(|error| {
            Error::ArtifactFile {
                path: component,
                source: error,
            }
        })?;
        super::bundle::retain_component_resources(source, &retained)?;
    }
    Ok(())
}
fn preparation_lock(root: &Path) -> Result<ExclusiveFileLock, Error> {
    let directory = root.join("target/phoxal");
    let path = directory.join("preparation.lock");
    fs::create_dir_all(&directory).map_err(|source| Error::ArtifactFile {
        path: directory.clone(),
        source,
    })?;
    let file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })?;
    match ExclusiveFileLock::try_acquire(file) {
        Ok(lock) => Ok(lock),
        Err(TryLockError::WouldBlock) => Err(invalid(
            &path,
            "another cargo phoxal command is preparing this project",
        )),
        Err(TryLockError::Error(error)) => Err(invalid(&path, error.to_string())),
    }
}
fn installation_lock(home: &Path) -> Result<ExclusiveFileLock, Error> {
    let directory = home.join("packages");
    fs::create_dir_all(&directory).map_err(|source| Error::ArtifactFile {
        path: directory.clone(),
        source,
    })?;
    let path = directory.join(".installation.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })?;
    ExclusiveFileLock::acquire(file).map_err(|source| Error::ArtifactFile { path, source })
}
pub(crate) fn phoxal_home() -> Result<PathBuf, Error> {
    if let Some(path) = std::env::var_os("PHOXAL_HOME") {
        if path.is_empty() {
            return Err(invalid(
                Path::new("PHOXAL_HOME"),
                "PHOXAL_HOME cannot be empty",
            ));
        }
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| invalid(Path::new("HOME"), "HOME is unavailable; set PHOXAL_HOME"))?;
    #[cfg(target_os = "macos")]
    {
        Ok(PathBuf::from(home).join("Library/Application Support/Phoxal"))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(home).join(".local/share"))
            .join("phoxal"))
    }
}
fn host_target() -> Result<String, Error> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .map_err(|source| Error::CargoSpawn {
            operation: "read rustc host target".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(invalid(Path::new("rustc"), "cannot read the host target"));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_owned)
        .ok_or_else(|| invalid(Path::new("rustc"), "rustc did not report a host target"))
}
fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}
fn invalid(path: &Path, message: impl Into<String>) -> Error {
    Error::ManifestPreparation {
        path: path.to_owned(),
        message: message.into(),
    }
}
pub(crate) fn operation_lock(root: &Path) -> Result<ExclusiveFileLock, Error> {
    let directory = root.join("target/phoxal");
    fs::create_dir_all(&directory).map_err(|source| Error::ArtifactFile {
        path: directory.clone(),
        source,
    })?;
    let path = directory.join("operation.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })?;
    ExclusiveFileLock::try_acquire(file).map_err(|error| {
        invalid(
            &path,
            format!("another cargo phoxal operation owns this project: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn git_selection_with_package_path_remains_git() {
        let selected: Source = serde_yaml::from_str(
                "git:\n  name: acme-motion\n  url: https://example.test/motion.git\n  rev: 0123456789abcdef0123456789abcdef01234567\n  path: services/motion\n",
            )
            .expect("Git selection");
        assert!(matches!(selected, Source::Git(_)));
    }
    #[test]
    fn captured_source_accepts_workspace_inherited_package_version() {
        let directory = tempfile::tempdir().expect("temporary workspace");
        let root = directory.path();
        let provider = root.join("provider");
        fs::create_dir_all(provider.join("src")).expect("provider source directory");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"provider\"]\n[workspace.package]\nversion = \"1.2.3\"\n",
        )
        .expect("workspace manifest");
        fs::write(
            provider.join("Cargo.toml"),
            "[package]\nname = \"proof-provider\"\nversion.workspace = true\nedition = \"2024\"\n",
        )
        .expect("provider manifest");
        let source = provider.join("src/main.rs");
        fs::write(&source, "fn main() {}\n").expect("provider source");
        let message = serde_json::json!(
            { "reason" : "compiler-artifact", "package_id" :
            "path+file:///proof-provider#1.2.3", "target" : { "name" : "proof-provider",
            "kind" : ["bin"], "src_path" : source } }
        )
        .to_string();
        let options = CargoOptions {
            offline: true,
            ..CargoOptions::default()
        };
        let (captured, package_id, version) = captured_source(
            message.as_bytes(),
            "proof-provider",
            "proof-provider",
            Some("1.2.3"),
            &options,
        )
        .expect("inherited version");
        assert_eq!(captured, provider);
        assert_eq!(package_id, "path+file:///proof-provider#1.2.3");
        assert_eq!(version, "1.2.3");
    }
    #[test]
    fn retained_component_contains_runtime_resources_without_package_source() {
        let directory = tempfile::tempdir().expect("temporary package");
        let source = directory.path().join("source");
        let installed = directory.path().join("installed");
        fs::create_dir_all(source.join("assets")).expect("asset directory");
        fs::create_dir_all(source.join("src")).expect("source directory");
        fs::write(
                source.join("component.yaml"),
                "schema: phoxal/component/v0\nmodel: { file: model.xml, root_body: mount }\ncapabilities: {}\nassets: [assets]\n",
            )
            .expect("component definition");
        fs::write(source.join("model.xml"), "<mujoco model=\"proof\"/>").expect("model");
        fs::write(source.join("assets/mesh.obj"), "proof mesh").expect("resource");
        fs::write(source.join("src/main.rs"), "fn main() {}").expect("Rust source");
        fs::write(
            source.join("Cargo.toml"),
            "[package]\nname = \"proof\"\nversion = \"1.0.0\"\n",
        )
        .expect("manifest");
        retain_package_files(&source, &installed).expect("retain runtime resources");
        for path in ["component.yaml", "model.xml", "assets/mesh.obj"] {
            assert!(
                installed.join("source").join(path).is_file(),
                "missing {path}"
            );
        }
        assert!(!installed.join("source/Cargo.toml").exists());
        assert!(!installed.join("source/src").exists());
    }
}
