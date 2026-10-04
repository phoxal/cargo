//! Passive model resources acquired directly from authored paths or pinned Git.
//! No Cargo package, compilation, resolver project or synthetic library is required.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    match source {
        Source::Path(path) => {
            let root =
                layout
                    .root()
                    .join(path)
                    .canonicalize()
                    .map_err(|source| Error::ArtifactFile {
                        path: layout.root().join(path),
                        source,
                    })?;
            super::component_assets::read(&root)?;
            let name = root
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| invalid(&root, "component source has no name"))?
                .to_owned();
            Ok(SelectedData {
                package: name.clone(),
                version: "local".into(),
                package_id: format!("path+{}#{name}", root.display()),
                source: PackageSource::Local {
                    manifest_path: root.join("component.yaml"),
                },
                source_root: root,
            })
        }
        Source::Git(git) => select_git(source, git, options, &super::phoxal_home()?),
    }
}

fn select_git(
    source: &Source,
    git: &phoxal::artifact::document::GitSourceSelection,
    options: &CargoOptions,
    home: &Path,
) -> Result<SelectedData, Error> {
    let base = home.join("packages/passive");
    fs::create_dir_all(&base).map_err(|source| Error::ArtifactFile {
        path: base.clone(),
        source,
    })?;
    let selection = serde_json::to_vec(source).map_err(|e| invalid(&base, e.to_string()))?;
    let identity = format!("{:x}", Sha256::digest(selection));
    let store = base.join(&identity);
    let lock_path = base.join(format!("{identity}.lock"));
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
    if !store.is_dir() {
        if options.offline || options.lock == super::LockMode::Frozen {
            return Err(invalid(
                &store,
                "pinned passive source is not cached; acquire it before offline/frozen preparation",
            ));
        }
        let staging = tempfile::tempdir_in(&base).map_err(|source| Error::ArtifactFile {
            path: base.clone(),
            source,
        })?;
        let checkout = staging.path().join("checkout");
        fs::create_dir(&checkout).map_err(|source| Error::ArtifactFile {
            path: checkout.clone(),
            source,
        })?;
        for args in [
            vec!["init", "--quiet"],
            vec!["fetch", "--quiet", "--depth", "1", "--", &git.url, &git.rev],
            vec!["checkout", "--quiet", "--detach", "FETCH_HEAD"],
        ] {
            let output = Command::new("git")
                .current_dir(&checkout)
                .args(args)
                .output()
                .map_err(|e| invalid(&checkout, e.to_string()))?;
            if !output.status.success() {
                return Err(invalid(
                    &checkout,
                    format!(
                        "pinned Git acquisition failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ));
            }
        }
        let revision = Command::new("git")
            .current_dir(&checkout)
            .args(["rev-parse", "HEAD"])
            .output()
            .map_err(|e| invalid(&checkout, e.to_string()))?;
        if !revision.status.success() || String::from_utf8_lossy(&revision.stdout).trim() != git.rev
        {
            return Err(invalid(
                &checkout,
                "Git checkout does not match the authored full revision",
            ));
        }
        let selected = checkout.join(git.path.as_deref().unwrap_or(""));
        let root = selected
            .canonicalize()
            .map_err(|source| Error::ArtifactFile {
                path: selected.clone(),
                source,
            })?;
        let canonical_checkout = checkout
            .canonicalize()
            .map_err(|source| Error::ArtifactFile {
                path: checkout.clone(),
                source,
            })?;
        if !root.starts_with(canonical_checkout) {
            return Err(invalid(
                &selected,
                "selected passive path escapes its Git checkout",
            ));
        }
        let mut files = super::component_assets::read(&root)?;
        files.insert(PathBuf::from("component.yaml"));
        let retained = staging.path().join("source");
        for relative in files {
            let destination = retained.join(&relative);
            fs::create_dir_all(
                destination
                    .parent()
                    .ok_or_else(|| invalid(&destination, "resource has no parent"))?,
            )
            .and_then(|()| fs::copy(root.join(relative), &destination).map(|_| ()))
            .map_err(|source| Error::ArtifactFile {
                path: destination,
                source,
            })?;
        }
        fs::rename(retained, &store).map_err(|source| Error::ArtifactFile {
            path: store.clone(),
            source,
        })?;
    }
    super::component_assets::read(&store)?;
    Ok(SelectedData {
        package: git.name.clone(),
        version: git.rev.clone(),
        package_id: format!(
            "git+{}#{}:{}",
            git.url,
            git.rev,
            git.path.as_deref().unwrap_or("")
        ),
        source: PackageSource::Git {
            source: format!("git+{}#{}", git.url, git.rev),
        },
        source_root: store,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assets(path: &Path) {
        fs::create_dir_all(path).expect("component directory");
        fs::write(path.join("component.yaml"), "schema: phoxal/component/v0\nmodel: { file: model.xml, root_body: mount }\ncapabilities: {}\nassets: [texture.txt]\n").expect("declaration");
        fs::write(
            path.join("model.xml"),
            "<mujoco><worldbody><body name=\"mount\"/></worldbody></mujoco>",
        )
        .expect("model");
        fs::write(path.join("texture.txt"), "resource").expect("asset");
    }

    #[test]
    fn local_assets_require_no_cargo_package() {
        let home = tempfile::tempdir().expect("root");
        let robot = home.path().join("robot");
        assets(&home.path().join("passive"));
        fs::create_dir(&robot).expect("robot");
        fs::write(
            robot.join("Cargo.toml"),
            "[package]\nname='robot'\nversion='0.1.0'\n",
        )
        .expect("robot manifest");
        fs::write(robot.join("robot.yaml"), "schema: phoxal/robot/v0\nrobot: { id: robot }\nsupervisor: { source: { path: ../supervisor } }\n").expect("robot document");
        let layout = ProjectLayout::discover(&robot).expect("layout");
        let selected = select(
            &layout,
            &Source::Path("../passive".into()),
            &CargoOptions::default(),
        )
        .expect("passive selection");
        assert_eq!(
            selected.source_root,
            home.path().join("passive").canonicalize().expect("root")
        );
        assert!(!selected.source_root.join("Cargo.toml").exists());
    }

    #[test]
    fn pinned_git_assets_recover_offline_without_a_manifest_or_origin() {
        let home = tempfile::tempdir().expect("home");
        let repo = home.path().join("repository");
        assets(&repo.join("models/passive"));
        for arguments in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "assets",
            ],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(&repo)
                    .args(arguments)
                    .status()
                    .expect("git")
                    .success()
            );
        }
        let output = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("revision");
        let git = phoxal::artifact::document::GitSourceSelection {
            name: "passive".into(),
            url: format!("file://{}", repo.display()),
            rev: String::from_utf8(output.stdout)
                .expect("revision")
                .trim()
                .into(),
            path: Some("models/passive".into()),
        };
        let source = Source::Git(git.clone());
        let options = CargoOptions::default();
        let selected =
            select_git(&source, &git, &options, home.path()).expect("direct acquisition");
        assert_eq!(
            fs::read_to_string(selected.source_root.join("texture.txt")).expect("resource"),
            "resource"
        );
        assert!(!selected.source_root.join("Cargo.toml").exists());
        fs::remove_dir_all(repo).expect("remove origin");
        let cached = select_git(
            &source,
            &git,
            &CargoOptions {
                offline: true,
                ..options
            },
            home.path(),
        )
        .expect("offline reuse");
        assert_eq!(cached.source_root, selected.source_root);
    }
}
