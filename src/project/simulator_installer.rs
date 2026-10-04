//! Acquisition of one independent native-free simulator installer application.
use super::{Error, cargo::CargoOptions, file_lock::ExclusiveFileLock};
use phoxal::artifact::application::{HOST_EXECUTION_TARGET, SIMULATOR_INSTALL_CONTRACT};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct Selection {
    pub(crate) executable: PathBuf,
    _guard: Option<ExclusiveFileLock>,
}

pub(crate) fn select(
    options: &CargoOptions,
    upgrade: bool,
    release: Option<&str>,
) -> Result<Selection, Error> {
    if let Some(path) = std::env::var_os("PHOXAL_SIMULATOR_INSTALLER") {
        let path = PathBuf::from(path);
        validate(&path)?;
        return Ok(Selection {
            executable: path,
            _guard: None,
        });
    }
    let root = super::phoxal_home()?.join("applications/simulator-installer");
    fs::create_dir_all(&root).map_err(|source| Error::ArtifactFile {
        path: root.clone(),
        source,
    })?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("installation.lock"))
        .map_err(|source| Error::ArtifactFile {
            path: root.clone(),
            source,
        })?;
    let guard = ExclusiveFileLock::try_acquire(lock)
        .map_err(|e| invalid(format!("simulator installer is in use: {e}")))?;
    let installed = root.join("selected/bin/phoxal-simulator-install");
    if installed.is_file() && !upgrade && release.is_none() {
        validate(&installed)?;
        return Ok(Selection {
            executable: installed,
            _guard: Some(guard),
        });
    }
    if options.offline || options.lock == super::cargo::LockMode::Frozen {
        return Err(invalid(
            "simulator installer acquisition is disabled by offline/frozen; install it first",
        ));
    }
    let stage = tempfile::Builder::new()
        .prefix("candidate-")
        .tempdir_in(&root)
        .map_err(|source| Error::ArtifactFile {
            path: root.clone(),
            source,
        })?;
    let candidate = stage.path().join("application");
    let robot_root = std::env::current_dir().map_err(|source| Error::ArtifactFile {
        path: PathBuf::from("."),
        source,
    })?;
    let version = match release {
        Some(version) => version.to_owned(),
        None => resolve_release(&robot_root, options)?,
    };
    let source = phoxal::artifact::document::Source::Package(
        phoxal::artifact::document::PackageSourceSelection {
            name: "phoxal-simulator-install".into(),
            version,
            registry: None,
        },
    );
    // Installer build flags belong to the host application, not the robot participant graph.
    let build = CargoOptions {
        cargo_path: options.cargo_path.clone(),
        ..CargoOptions::default()
    };
    let mut command = super::cargo::source_install_command(
        &robot_root,
        &source,
        "phoxal-simulator-install",
        &candidate,
        &build,
    );
    // Cargo test retains its robot build lock through test execution. Application
    // acquisition uses an installer-owned cache and must not inherit that target directory.
    command.env("CARGO_TARGET_DIR", root.join("build"));
    let output = command.output().map_err(|source| Error::CargoSpawn {
        operation: "acquire simulator installer".into(),
        source,
    })?;
    if !output.status.success() {
        return Err(invalid(format!(
            "simulator installer acquisition failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    validate(&candidate.join("bin/phoxal-simulator-install"))?;
    let selected = root.join("selected");
    let previous = stage.path().join("previous");
    if selected.exists() {
        fs::rename(&selected, &previous).map_err(|source| Error::ArtifactFile {
            path: selected.clone(),
            source,
        })?;
    }
    if let Err(source) = fs::rename(&candidate, &selected) {
        if previous.exists() {
            fs::rename(&previous, &selected).map_err(|source| Error::ArtifactFile {
                path: selected.clone(),
                source,
            })?;
        }
        return Err(Error::ArtifactFile {
            path: selected,
            source,
        });
    }
    Ok(Selection {
        executable: installed,
        _guard: Some(guard),
    })
}

fn validate(path: &Path) -> Result<(), Error> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::ArtifactFile {
        path: path.into(),
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(invalid(
            "simulator installer must be a regular non-symlink file",
        ));
    }
    let record = super::application::read_embedded_contract(path)?;
    if record.launch != SIMULATOR_INSTALL_CONTRACT || record.target != HOST_EXECUTION_TARGET {
        return Err(invalid(
            "incompatible simulator installer interface or target",
        ));
    }
    Ok(())
}
fn invalid(message: impl Into<String>) -> Error {
    Error::SimulationInvalid {
        message: message.into(),
    }
}

/// Cargo resolves the current registry release, including pre-releases, independently of this tool.
fn resolve_release(root: &Path, options: &CargoOptions) -> Result<String, Error> {
    let mut command = Command::new(options.cargo_program());
    command.current_dir(root).args([
        "info",
        "phoxal-simulator-install",
        "--registry",
        "phoxal",
        "--color",
        "never",
    ]);
    if let Some(config) = super::cargo::registry_config(root) {
        command.args(["--config", &config]);
    }
    let output = command.output().map_err(|source| Error::CargoSpawn {
        operation: "resolve simulator installer release".into(),
        source,
    })?;
    if !output.status.success() {
        return Err(invalid(format!(
            "cannot resolve simulator installer release: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout
        .lines()
        .find_map(|line| line.strip_prefix("version: "))
        .and_then(|line| line.split_whitespace().next())
        .ok_or_else(|| invalid("Cargo info returned no installer release"))?;
    semver::Version::parse(version)
        .map_err(|e| invalid(format!("invalid installer release: {e}")))?;
    Ok(version.into())
}
