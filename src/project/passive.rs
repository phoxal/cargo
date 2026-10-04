//! Source-selected passive component data, independent of the robot Rust graph.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::document::Source;
use super::selection::PackageSource;
use super::{CargoOptions, Error, ProjectLayout};

#[derive(Serialize, Deserialize)]
pub(crate) struct SelectedData {
    pub(crate) package: String,
    pub(crate) version: String,
    pub(crate) package_id: String,
    pub(crate) source: PackageSource,
    pub(crate) source_root: PathBuf,
}

fn invalid(path: &Path, message: impl Into<String>) -> Error {
    Error::ArtifactInvalid {
        path: path.to_owned(),
        message: message.into(),
    }
}

pub(crate) fn select(
    layout: &ProjectLayout,
    source: &Source,
    options: &CargoOptions,
) -> Result<SelectedData, Error> {
    if let Source::Path(path) = source {
        let root =
            layout
                .root()
                .join(path)
                .canonicalize()
                .map_err(|source| Error::ArtifactFile {
                    path: layout.root().join(path),
                    source,
                })?;
        let manifest = root.join("Cargo.toml");
        let text = fs::read_to_string(&manifest).map_err(|source| Error::ArtifactFile {
            path: manifest.clone(),
            source,
        })?;
        let value: toml::Value =
            toml::from_str(&text).map_err(|error| invalid(&manifest, error.to_string()))?;
        let package = value
            .get("package")
            .and_then(|value| value.get("name"))
            .and_then(toml::Value::as_str)
            .ok_or_else(|| invalid(&manifest, "component package has no name"))?
            .to_owned();
        let version_value = value.get("package").and_then(|value| value.get("version"));
        let version = if let Some(version) = version_value.and_then(toml::Value::as_str) {
            version.to_owned()
        } else if version_value
            .and_then(|value| value.get("workspace"))
            .and_then(toml::Value::as_bool)
            == Some(true)
        {
            workspace_version(&root)?
        } else {
            return Err(invalid(&manifest, "component package has no version"));
        };
        super::component_assets::read(&root)?;
        return Ok(SelectedData {
            package_id: format!("path+{}#{package}@{version}", root.display()),
            package,
            version,
            source: PackageSource::Local {
                manifest_path: manifest,
            },
            source_root: root,
        });
    }
    let home = super::phoxal_home()?;
    let identity =
        super::participant::installation_store(layout.root(), &home, source, None, options)?;
    let key = identity
        .file_name()
        .ok_or_else(|| invalid(&identity, "missing source selection identity"))?;
    let base = home.join("packages/passive");
    fs::create_dir_all(&base).map_err(|source| Error::ArtifactFile {
        path: base.clone(),
        source,
    })?;
    let store = base.join(key);
    let lock_path = base.join(format!("{}.lock", key.to_string_lossy()));
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|source| Error::ArtifactFile {
            path: lock_path.clone(),
            source,
        })?;
    let _guard = super::file_lock::ExclusiveFileLock::acquire(file).map_err(|source| {
        Error::ArtifactFile {
            path: lock_path,
            source,
        }
    })?;
    let record = store.join("selection.json");
    if record.is_file() {
        let bytes = fs::read(&record).map_err(|source| Error::ArtifactFile {
            path: record.clone(),
            source,
        })?;
        let mut selected: SelectedData =
            serde_json::from_slice(&bytes).map_err(|error| invalid(&record, error.to_string()))?;
        selected.source_root = store.join("source");
        super::component_assets::read(&selected.source_root)?;
        return Ok(selected);
    }
    if options.offline || options.lock == super::LockMode::Frozen {
        return Err(invalid(
            &record,
            "passive component selection is not installed; acquire it before offline/frozen preparation",
        ));
    }
    let staging = tempfile::tempdir_in(&base).map_err(|source| Error::ArtifactFile {
        path: base.clone(),
        source,
    })?;
    let resolver = staging.path().join("resolver");
    fs::create_dir_all(resolver.join("src")).map_err(|source| Error::ArtifactFile {
        path: resolver.clone(),
        source,
    })?;
    let (name, dependency) = match source {
        Source::Package(package) => (
            package.name.clone(),
            toml::Value::try_from(
                serde_json::json!({"package":package.name,"version":format!("={}",package.version),"registry":package.registry.as_deref().unwrap_or("phoxal"),"default-features":false}),
            ),
        ),
        Source::Git(git) => (
            git.name.clone(),
            toml::Value::try_from(
                serde_json::json!({"package":git.name,"git":git.url,"rev":git.rev,"default-features":false}),
            ),
        ),
        Source::Path(_) => unreachable!("local selection handled above"),
    };
    let dependency = dependency.map_err(|error| invalid(&resolver, error.to_string()))?;
    let manifest = toml::Value::try_from(serde_json::json!({"workspace":{},"package":{"name":"phoxal-passive-acquisition","version":"0.0.0","edition":"2024"},"dependencies":{"selected":dependency}})).map_err(|error| invalid(&resolver, error.to_string()))?;
    fs::write(
        resolver.join("Cargo.toml"),
        toml::to_string(&manifest).map_err(|error| invalid(&resolver, error.to_string()))?,
    )
    .and_then(|()| fs::write(resolver.join("src/lib.rs"), ""))
    .map_err(|source| Error::ArtifactFile {
        path: resolver.clone(),
        source,
    })?;
    // Cargo resolves/checks the authored source and archive; no build or robot manifest mutation.
    // The temporary resolver lock belongs to this acquisition, never to the robot.
    let mut acquisition = options.clone();
    acquisition.features.clear();
    acquisition.all_features = false;
    acquisition.no_default_features = false;
    acquisition.lock = super::LockMode::Unlocked;
    let metadata = super::cargo::load_metadata_at(
        &resolver.join("Cargo.toml"),
        layout.root(),
        None,
        &acquisition,
    )?;
    let root = metadata
        .root_package()
        .ok_or_else(|| invalid(&resolver, "Cargo acquisition has no root"))?;
    let node = metadata
        .resolve
        .as_ref()
        .and_then(|resolve| resolve.nodes.iter().find(|node| node.id == root.id))
        .ok_or_else(|| invalid(&resolver, "Cargo acquisition has no resolved source"))?;
    let id = node
        .deps
        .iter()
        .find(|dependency| dependency.name == "selected")
        .ok_or_else(|| invalid(&resolver, "Cargo acquisition omitted selected component"))?
        .pkg
        .clone();
    let package = metadata
        .packages
        .iter()
        .find(|package| package.id == id)
        .ok_or_else(|| invalid(&resolver, "Cargo acquisition omitted selected package"))?;
    if package.name != name {
        return Err(invalid(
            &resolver,
            "Cargo resolved a different component package",
        ));
    }
    let source_root = package
        .manifest_path
        .as_std_path()
        .parent()
        .ok_or_else(|| invalid(&resolver, "component manifest has no parent"))?;
    if let Source::Git(git) = source {
        let source_id = package
            .source
            .as_ref()
            .map(|source| source.repr.as_str())
            .unwrap_or_default();
        if !source_id.ends_with(&format!("#{}", git.rev)) {
            return Err(invalid(
                source_root,
                "component Git revision differs from robot.yaml",
            ));
        }
        if let Some(path) = &git.path {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "--show-toplevel"])
                .current_dir(source_root)
                .output()
                .map_err(|error| invalid(source_root, error.to_string()))?;
            let checkout = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
            if !output.status.success()
                || checkout.join(path).canonicalize().ok() != source_root.canonicalize().ok()
            {
                return Err(invalid(
                    source_root,
                    "component Git package path differs from robot.yaml",
                ));
            }
        }
    }
    let mut files = super::component_assets::read(source_root)?;
    files.insert(PathBuf::from("component.yaml"));
    let retained = staging.path().join("source");
    for relative in files {
        let destination = retained.join(&relative);
        fs::create_dir_all(
            destination
                .parent()
                .ok_or_else(|| invalid(&destination, "asset has no parent"))?,
        )
        .and_then(|()| fs::copy(source_root.join(&relative), &destination).map(|_| ()))
        .map_err(|source| Error::ArtifactFile {
            path: destination,
            source,
        })?;
    }
    let selected = SelectedData {
        package: name,
        version: package.version.to_string(),
        package_id: package.id.to_string(),
        source: super::selection::package_source(package),
        source_root: store.join("source"),
    };
    fs::write(
        staging.path().join("selection.json"),
        serde_json::to_vec(&selected).map_err(|error| invalid(&record, error.to_string()))?,
    )
    .map_err(|source| Error::ArtifactFile {
        path: record,
        source,
    })?;
    fs::remove_dir_all(&resolver).map_err(|source| Error::ArtifactFile {
        path: resolver,
        source,
    })?;
    fs::rename(staging.path(), &store).map_err(|source| Error::ArtifactFile {
        path: store,
        source,
    })?;
    Ok(selected)
}

fn workspace_version(root: &Path) -> Result<String, Error> {
    for directory in root.ancestors() {
        let path = directory.join("Cargo.toml");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let value: toml::Value =
            toml::from_str(&text).map_err(|error| invalid(&path, error.to_string()))?;
        if let Some(workspace) = value.get("workspace") {
            return workspace
                .get("package")
                .and_then(|package| package.get("version"))
                .and_then(toml::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| invalid(&path, "workspace has no package.version"));
        }
    }
    Err(invalid(
        root,
        "component inherited version has no workspace",
    ))
}
