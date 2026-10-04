//! Tool-owned supervisor executable acquisition from the authored
//! selection.
//!
//! `robot.yaml` is the sole supervisor selection authority: its
//! `supervisor.source` uses the participants' `Source` type and the same
//! acquisition facilities. The tool acquires or reuses exactly that
//! selection, validates the embedded application contract (target and
//! required interfaces) from the file without executing it, and copies the
//! executable into each deployment bundle. Installation state is cache and
//! provenance only; different robots may select different releases
//! concurrently.
//!
//! Registry and Git selections reuse the participant acquisition store
//! layout and Cargo invocation semantics. Local path selections build
//! incrementally from the resolved source so implementation edits are
//! always compiled and their embedded contract re-inspected before use.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::project::cargo::CargoOptions;
use crate::project::{Error, phoxal_home};
use phoxal::artifact::application::{
    BUNDLE_CONTRACT, EXECUTION_PROTOCOL_CONTRACT, SIMULATION_PROTOCOL_CONTRACT,
    SUPERVISOR_LAUNCH_CONTRACT,
};
use phoxal::artifact::document::{GitSourceSelection, PackageSourceSelection, Source};

/// The registry a default-registry supervisor selection acquires from.
const DEFAULT_REGISTRY: &str = "phoxal";

/// One validated supervisor executable selection.
#[derive(Clone, Debug)]
pub(crate) struct SupervisorSelection {
    /// Absolute supervisor executable path.
    pub(crate) executable: PathBuf,
    /// Human release provenance for diagnostics.
    pub(crate) provenance: String,
    /// Keeps the selected installation stable until its bundle copy exists.
    _guard: Option<std::sync::Arc<super::file_lock::ExclusiveFileLock>>,
}

/// The supervisor selection authored in the robot document.
#[derive(Clone, Debug)]
pub(crate) struct AuthoredSupervisor {
    /// The authored source selection.
    source: Source,
    /// The optional explicit binary target.
    binary: Option<String>,
}

impl AuthoredSupervisor {
    /// Captures the authored selection from the robot document.
    pub(crate) fn capture(
        document: &phoxal::artifact::document::RobotDocument,
    ) -> Result<Self, Error> {
        let phoxal::artifact::document::RobotDocument::V0 { supervisor, .. } = document;
        Ok(Self {
            source: supervisor.source.clone(),
            binary: supervisor.binary.clone(),
        })
    }

    /// The complete resolved selection identity.
    ///
    /// Local paths are resolved canonically against the robot root so
    /// two unrelated roots sharing a relative path never share a cache.
    /// Registry selections include the effective registry index; Git
    /// selections include the full URL, revision, and package path. Build
    /// policy (target, profile, features) completes the identity.
    fn identity(
        &self,
        root: &Path,
        options: &CargoOptions,
        registry_index: &str,
    ) -> Result<String, Error> {
        use std::fmt::Write as _;
        let mut identity = String::new();
        match &self.source {
            Source::Path(path) => {
                let resolved =
                    root.join(path)
                        .canonicalize()
                        .map_err(|source| Error::ArtifactFile {
                            path: root.join(path),
                            source,
                        })?;
                let _ = write!(identity, "path\x1f{}", resolved.display());
            }
            Source::Package(PackageSourceSelection {
                name,
                version,
                registry: _,
            }) => {
                let _ = write!(
                    identity,
                    "registry\x1f{}\x1f{}\x1f{}",
                    registry_index, name, version
                );
            }
            Source::Git(GitSourceSelection {
                name,
                url,
                rev,
                path,
            }) => {
                let _ = write!(
                    identity,
                    "git\x1f{}\x1f{}\x1f{}\x1f{}",
                    name,
                    url,
                    rev,
                    path.as_deref().unwrap_or("")
                );
            }
        }
        let _ = write!(
            identity,
            "\x1fbin\x1f{}",
            self.binary.as_deref().unwrap_or("default")
        );
        let build =
            serde_json::to_string(&crate::project::cargo::BuildSelection::for_source(options))
                .map_err(|error| Error::InvalidOptions {
                    message: error.to_string(),
                })?;
        let _ = write!(identity, "\x1fbuild\x1f{build}");
        Ok(identity)
    }

    /// The canonical store for one complete resolved selection.
    fn store(&self, home: &Path, identity: &str) -> PathBuf {
        use sha2::Digest as _;
        let mut hasher = sha2::Sha256::new();
        hasher.update(b"phoxal-supervisor-selection-v2\x1f");
        hasher.update(identity.as_bytes());
        let slug = format!("{:016x}", hasher.finalize());
        home.join("applications/supervisor").join(slug)
    }
}

/// The deployment target triple from the effective selection.
fn effective_target(options: &CargoOptions) -> String {
    crate::project::cargo::effective_target(options)
}

/// Selects the supervisor executable for one bundle assembly.
///
/// Local path selections build incrementally from the resolved source and
/// always re-inspect the current embedded contract, so implementation and
/// interface edits are visible. Registry and Git selections reuse the
/// cached installation for the complete resolved selection and acquire
/// otherwise through the ordinary Cargo install path.
pub(crate) fn select(
    root: &Path,
    authored: &AuthoredSupervisor,
    options: &CargoOptions,
    requires_simulation: bool,
) -> Result<SupervisorSelection, Error> {
    let registry_index = if let Source::Package(package) = &authored.source {
        let registry = package.registry.as_deref().unwrap_or(DEFAULT_REGISTRY);
        crate::project::cargo::registry_index(root, registry).ok_or_else(|| {
            Error::InvalidOptions {
                message: format!("registry `{registry}` has no configured index"),
            }
        })?
    } else {
        String::new()
    };
    let identity = authored.identity(root, options, &registry_index)?;
    let home = phoxal_home()?;
    let store = authored.store(&home, &identity);
    std::fs::create_dir_all(&store).map_err(|source| Error::ArtifactFile {
        path: store.clone(),
        source,
    })?;
    let lock_path = store.join(".acquire.lock");
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::ArtifactFile {
            path: lock_path.clone(),
            source,
        })?;
    let guard = super::file_lock::ExclusiveFileLock::acquire(lock).map_err(|source| {
        Error::ArtifactFile {
            path: lock_path,
            source,
        }
    })?;

    let mut selection = select_guarded(root, authored, options, &store, requires_simulation)?;
    selection._guard = Some(std::sync::Arc::new(guard));
    Ok(selection)
}

fn select_guarded(
    root: &Path,
    authored: &AuthoredSupervisor,
    options: &CargoOptions,
    store: &Path,
    requires_simulation: bool,
) -> Result<SupervisorSelection, Error> {
    if let Source::Path(path) = &authored.source {
        return select_local(
            root,
            Path::new(path),
            authored,
            options,
            store,
            requires_simulation,
        );
    }
    let binary_name = authored
        .binary
        .clone()
        .unwrap_or_else(|| package_name(authored));
    let executable = store.join("product/bin").join(&binary_name);
    if executable.is_file() {
        validate_embedded_contract(&executable, &effective_target(options), requires_simulation)?;
        return Ok(SupervisorSelection {
            executable,
            provenance: read_provenance(&store.join("product"))?,
            _guard: None,
        });
    }
    if options.offline || options.lock == crate::project::cargo::LockMode::Frozen {
        return Err(Error::SupervisorLaunch {
            message: format!(
                "no cached supervisor installation for the authored selection; acquisition is \
                 forbidden by --offline or --frozen (store {})",
                store.display()
            ),
        });
    }
    acquire_locked(root, authored, options, store, requires_simulation)
}

/// Builds a local path selection incrementally, always inspecting the
/// current embedded contract.
///
/// Local development must see source edits: the build runs through Cargo's
/// normal incremental compilation in the source workspace, then the
/// resulting executable is validated and copied into the selection store
/// for bundling. No-change invocations reuse Cargo's own build cache.
fn select_local(
    root: &Path,
    path: &Path,
    authored: &AuthoredSupervisor,
    options: &CargoOptions,
    store: &Path,
    requires_simulation: bool,
) -> Result<SupervisorSelection, Error> {
    let resolved = root
        .join(path)
        .canonicalize()
        .map_err(|source| Error::ArtifactFile {
            path: root.join(path),
            source,
        })?;
    let package = local_package_name(&resolved)?;
    let binary_name = authored.binary.clone().unwrap_or(package);

    // Build incrementally in the source workspace; Cargo's cache makes
    // no-change invocations fast. Offline/frozen is honored: a local
    // source can still have registry dependencies.
    let cargo = options.cargo_program();
    let mut command = Command::new(&cargo);
    command
        .current_dir(root)
        .args(["build", "--manifest-path"])
        .arg(resolved.join("Cargo.toml"))
        .args(["--bin", &binary_name, "--message-format", "json"]);
    crate::project::cargo::BuildSelection::for_source(options).apply(&mut command);
    if let Some(config) = crate::project::cargo::registry_config(root) {
        command.args(["--config", &config]);
    }
    command.args(options.lock.flags());
    if options.offline {
        command.arg("--offline");
    }
    let output = command.output().map_err(|source| Error::SupervisorLaunch {
        message: format!("cannot run {}: {source}", cargo.display()),
    })?;
    if !output.status.success() {
        return Err(Error::SupervisorLaunch {
            message: format!(
                "building the authored local supervisor selection failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    let built = cargo_metadata::Message::parse_stream(output.stdout.as_slice())
        .filter_map(Result::ok)
        .find_map(|message| match message {
            cargo_metadata::Message::CompilerArtifact(artifact)
                if artifact.target.name == binary_name =>
            {
                artifact.executable.map(|path| path.into_std_path_buf())
            }
            _ => None,
        })
        .ok_or_else(|| Error::SupervisorLaunch {
            message: format!("Cargo reported no executable for supervisor binary `{binary_name}`"),
        })?;
    ensure_regular_executable(&built)?;

    // Always re-inspect the current embedded contract; incompatible
    // interface changes refuse here, preserving previous good bundles.
    validate_embedded_contract(&built, &effective_target(options), requires_simulation)?;

    let staging = tempfile::Builder::new()
        .prefix(".phoxal-supervisor-build-")
        .tempdir_in(store.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|source| Error::BundleDirectory {
            path: store.to_owned(),
            source,
        })?;
    std::fs::create_dir(staging.path().join("bin")).map_err(|source| Error::ArtifactFile {
        path: staging.path().to_owned(),
        source,
    })?;
    let candidate = staging.path().join("bin").join(&binary_name);
    std::fs::copy(&built, &candidate).map_err(|source| Error::ArtifactFile {
        path: candidate.clone(),
        source,
    })?;
    make_executable(&candidate)?;
    write_provenance(staging.path(), &format!("local {}", resolved.display()))?;
    publish_candidate(store, staging)?;
    Ok(SupervisorSelection {
        executable: store.join("product/bin").join(binary_name),
        provenance: read_provenance(&store.join("product"))?,
        _guard: None,
    })
}

fn acquire_locked(
    root: &Path,
    authored: &AuthoredSupervisor,
    options: &CargoOptions,
    store: &Path,
    requires_simulation: bool,
) -> Result<SupervisorSelection, Error> {
    // A concurrent acquirer may have installed this exact selection while
    // this process waited for the lock.
    let binary_name = authored
        .binary
        .clone()
        .unwrap_or_else(|| package_name(authored));
    let executable = store.join("product/bin").join(&binary_name);
    if executable.is_file() {
        validate_embedded_contract(&executable, &effective_target(options), requires_simulation)?;
        return Ok(SupervisorSelection {
            executable,
            provenance: read_provenance(&store.join("product"))?,
            _guard: None,
        });
    }
    let staging = tempfile::Builder::new()
        .prefix(".phoxal-supervisor-install-")
        .tempdir_in(store.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|source| Error::BundleDirectory {
            path: store.to_owned(),
            source,
        })?;
    let cargo = options.cargo_program();
    let mut command = crate::project::cargo::source_install_command(
        root,
        &authored.source,
        &binary_name,
        staging.path(),
        options,
    );
    command.env("CARGO_TARGET_DIR", store.join("build"));
    let output = command.output().map_err(|source| Error::SupervisorLaunch {
        message: format!("cannot run {}: {source}", cargo.display()),
    })?;
    if !output.status.success() {
        return Err(Error::SupervisorLaunch {
            message: format!(
                "acquiring the authored supervisor selection failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            ),
        });
    }
    let candidate = staging.path().join("bin").join(&binary_name);
    validate_embedded_contract(&candidate, &effective_target(options), requires_simulation)?;
    if let Source::Git(git) = &authored.source {
        let (source_root, _, _) = crate::project::participant::captured_source(
            &output.stdout,
            &binary_name,
            &git.name,
            None,
            options,
        )?;
        if let Some(path) = &git.path
            && !source_root.ends_with(path)
        {
            return Err(Error::SupervisorLaunch {
                message: format!("Git supervisor package is not at selected path `{path}`"),
            });
        }
    }
    write_authored_provenance(staging.path(), authored)?;
    publish_candidate(store, staging)?;
    Ok(SupervisorSelection {
        executable,
        provenance: read_provenance(&store.join("product"))?,
        _guard: None,
    })
}

/// Publishes a validated application and its provenance together under the store lock.
fn publish_candidate(store: &Path, staging: tempfile::TempDir) -> Result<(), Error> {
    let product = store.join("product");
    let previous = tempfile::Builder::new()
        .prefix(".phoxal-supervisor-previous-")
        .tempdir_in(store.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|source| Error::BundleDirectory {
            path: store.to_owned(),
            source,
        })?;
    let backup = previous.path().join("product");
    if product.exists() {
        std::fs::rename(&product, &backup).map_err(|source| Error::ArtifactFile {
            path: product.clone(),
            source,
        })?;
    }
    if let Err(source) = std::fs::rename(staging.path(), &product) {
        if backup.exists()
            && let Err(restore) = std::fs::rename(&backup, &product)
        {
            let retained = previous.keep();
            return Err(Error::SupervisorLaunch {
                message: format!(
                    "application publication failed: {source}; restoration failed: {restore}; previous installation preserved at {}",
                    retained.display()
                ),
            });
        }
        return Err(Error::ArtifactFile {
            path: product,
            source,
        });
    }
    Ok(())
}

/// Reads the Cargo `[package].name` from one local crate manifest.
fn local_package_name(source_root: &Path) -> Result<String, Error> {
    let manifest_path = source_root.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest_path).map_err(|source| Error::ArtifactFile {
        path: manifest_path.clone(),
        source,
    })?;
    let document = toml::from_str::<toml::Value>(&text).map_err(|source| Error::ParseManifest {
        path: manifest_path.clone(),
        source,
    })?;
    document
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::SupervisorLaunch {
            message: format!(
                "the authored supervisor source at {} declares no [package].name",
                source_root.display()
            ),
        })
}

/// The Cargo package name for a registry or Git selection.
fn package_name(authored: &AuthoredSupervisor) -> String {
    match &authored.source {
        Source::Path(_) => unreachable!("local selections resolve their package name"),
        Source::Package(package) => package.name.clone(),
        Source::Git(git) => git.name.clone(),
    }
}

/// Writes the selection provenance record.
fn write_provenance(store: &Path, text: &str) -> Result<(), Error> {
    let staged = store.join("provenance");
    std::fs::write(&staged, format!("{text}\n")).map_err(|source| Error::ArtifactFile {
        path: staged,
        source,
    })
}

/// Writes the provenance for an authored selection.
fn write_authored_provenance(store: &Path, authored: &AuthoredSupervisor) -> Result<(), Error> {
    let text = match &authored.source {
        Source::Path(path) => format!("local {path}"),
        Source::Package(package) => format!(
            "registry {} {}{}",
            package.registry.as_deref().unwrap_or(DEFAULT_REGISTRY),
            package.name,
            package.version
        ),
        Source::Git(git) => format!("git {} @{}", git.url, git.rev),
    };
    write_provenance(store, &text)
}

/// Reads the cached selection's provenance line.
fn read_provenance(store: &Path) -> Result<String, Error> {
    let path = store.join("provenance");
    std::fs::read_to_string(&path)
        .map(|content| content.trim().to_owned())
        .map_err(|source| Error::ArtifactFile { path, source })
}

/// Marks a file executable.
fn make_executable(path: &Path) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    let metadata = std::fs::metadata(path).map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })?;
    let mut permissions = metadata.permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(path, permissions).map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })
}

/// Checks one path is a regular executable file.
fn ensure_regular_executable(path: &Path) -> Result<(), Error> {
    let metadata = std::fs::symlink_metadata(path).map_err(|source| Error::ArtifactFile {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::SupervisorLaunch {
            message: format!(
                "supervisor executable {} is not a regular file",
                path.display()
            ),
        });
    }
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(Error::SupervisorLaunch {
            message: format!("supervisor executable {} is not executable", path.display()),
        });
    }
    Ok(())
}

/// Reads one executable's embedded application contract from its link
/// section, without executing it.
/// Validates the embedded contract of one supervisor executable for one
/// deployment target.
fn validate_embedded_contract(
    path: &Path,
    target: &str,
    requires_simulation: bool,
) -> Result<(), Error> {
    let record = super::application::read_embedded_contract(path)?;
    let required = phoxal::artifact::application::ApplicationContract {
        bundle: Some(BUNDLE_CONTRACT),
        launch: SUPERVISOR_LAUNCH_CONTRACT,
        execution: Some(EXECUTION_PROTOCOL_CONTRACT),
        simulation: requires_simulation.then_some(SIMULATION_PROTOCOL_CONTRACT),
        target,
    };
    record
        .validate_for(&required)
        .map_err(|error| Error::SupervisorLaunch {
            message: format!("supervisor executable {}: {error}", path.display()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_identities_are_canonically_resolved() {
        let options = CargoOptions::default();
        let first = AuthoredSupervisor {
            source: Source::Path("supervisor".to_owned()),
            binary: None,
        };
        let second = AuthoredSupervisor {
            source: Source::Path("supervisor".to_owned()),
            binary: None,
        };
        // Same relative path from the SAME root: same identity.
        let root_a = tempfile::tempdir().expect("root A");
        std::fs::create_dir(root_a.path().join("supervisor")).expect("source A");
        let id_a = first
            .identity(root_a.path(), &options, "sparse+https://test/")
            .expect("identity A");
        let id_b = second
            .identity(root_a.path(), &options, "sparse+https://test/")
            .expect("identity B");
        assert_eq!(id_a, id_b, "same root and path share identity");
        // Same relative path from a DIFFERENT root: different identity.
        let root_b = tempfile::tempdir().expect("root B");
        std::fs::create_dir(root_b.path().join("supervisor")).expect("source B");
        let id_c = second
            .identity(root_b.path(), &options, "sparse+https://test/")
            .expect("identity C");
        assert_ne!(
            id_a, id_c,
            "different roots with same relative path must not share"
        );
    }

    #[test]
    fn registry_identities_include_the_effective_index() {
        let options = CargoOptions::default();
        let selection = AuthoredSupervisor {
            source: Source::Package(PackageSourceSelection {
                name: "phoxal-supervisor".to_owned(),
                version: "1.0.0".to_owned(),
                registry: None,
            }),
            binary: None,
        };
        let root = tempfile::tempdir().expect("root");
        let first = selection
            .identity(root.path(), &options, "sparse+https://a.test/")
            .expect("identity A");
        let second = selection
            .identity(root.path(), &options, "sparse+https://b.test/")
            .expect("identity B");
        assert_ne!(first, second, "different registry indexes must not share");
    }

    #[test]
    fn build_policy_distinguishes_identities() {
        let root = tempfile::tempdir().expect("root");
        let selection = AuthoredSupervisor {
            source: Source::Package(PackageSourceSelection {
                name: "phoxal-supervisor".to_owned(),
                version: "1.0.0".to_owned(),
                registry: None,
            }),
            binary: None,
        };
        let base = CargoOptions::default();
        let release = CargoOptions {
            release: true,
            ..base.clone()
        };
        let features = CargoOptions {
            features: vec!["extra".to_owned()],
            ..base.clone()
        };
        let id_base = selection
            .identity(root.path(), &base, "sparse+https://t/")
            .expect("base");
        let id_release = selection
            .identity(root.path(), &release, "sparse+https://t/")
            .expect("release");
        let id_features = selection
            .identity(root.path(), &features, "sparse+https://t/")
            .expect("features");
        assert_ne!(id_base, id_release, "profile distinguishes");
        assert_eq!(
            id_base, id_features,
            "robot features do not alter independently selected applications"
        );
    }
}
