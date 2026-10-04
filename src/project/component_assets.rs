//! The explicit package-relative resource inventory shared by acquisition,
//! bundle staging, and publication. Native model interpretation stays native-owned.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

use phoxal::artifact::document::ComponentDocument;

use super::document::ValidateComponentDocument as _;
use super::{Error, PublicationError};

pub(crate) fn read(root: &Path) -> Result<BTreeSet<PathBuf>, Error> {
    let definition = root.join("component.yaml");
    let metadata = fs::symlink_metadata(&definition).map_err(|source| Error::ArtifactFile {
        path: definition.clone(),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid(
            &definition,
            "component declaration must be a regular non-symlink file".to_owned(),
        ));
    }
    let text = fs::read_to_string(&definition).map_err(|source| Error::ArtifactFile {
        path: definition.clone(),
        source,
    })?;
    let document: ComponentDocument =
        serde_yaml::from_str(&text).map_err(|error| invalid(&definition, error.to_string()))?;
    files(root, &document)
}

pub(crate) fn files(root: &Path, document: &ComponentDocument) -> Result<BTreeSet<PathBuf>, Error> {
    let definition = root.join("component.yaml");
    document
        .validate()
        .map_err(|message| invalid(&definition, message))?;
    let ComponentDocument::V0 { model, assets, .. } = document;
    let canonical_root = root.canonicalize().map_err(|source| Error::ArtifactFile {
        path: root.to_owned(),
        source,
    })?;
    let mut files = BTreeSet::new();
    collect(&canonical_root, &definition, &model.file, true, &mut files)?;
    for reference in assets {
        collect(&canonical_root, &definition, reference, false, &mut files)?;
    }
    Ok(files)
}

fn invalid(path: &Path, message: String) -> Error {
    PublicationError::InvalidComponentDefinition {
        path: path.to_owned(),
        message,
    }
    .into()
}

fn collect(
    root: &Path,
    definition: &Path,
    reference: &Path,
    model: bool,
    files: &mut BTreeSet<PathBuf>,
) -> Result<(), Error> {
    let unsafe_path = || PublicationError::UnsafeAssetPath {
        reference: reference.display().to_string(),
        definition: definition.to_owned(),
        root: root.to_owned(),
    };
    if reference.as_os_str().is_empty() || reference.to_string_lossy().contains('\\') {
        return Err(unsafe_path().into());
    }
    let mut path = root.to_owned();
    for component in reference.components() {
        let Component::Normal(name) = component else {
            return Err(unsafe_path().into());
        };
        path.push(name);
        let metadata = fs::symlink_metadata(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::Publication(PublicationError::MissingAsset {
                    reference: reference.display().to_string(),
                    definition: definition.to_owned(),
                })
            } else {
                Error::ArtifactFile {
                    path: path.clone(),
                    source,
                }
            }
        })?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_path().into());
        }
    }
    let metadata = fs::symlink_metadata(&path).map_err(|source| Error::ArtifactFile {
        path: path.clone(),
        source,
    })?;
    if metadata.is_file() {
        files.insert(reference.to_owned());
    } else if metadata.is_dir() && !model {
        for entry in fs::read_dir(&path).map_err(|source| Error::ArtifactFile {
            path: path.clone(),
            source,
        })? {
            let entry = entry.map_err(|source| Error::ArtifactFile {
                path: path.clone(),
                source,
            })?;
            collect(
                root,
                definition,
                &reference.join(entry.file_name()),
                false,
                files,
            )?;
        }
    } else {
        return Err(invalid(
            definition,
            format!(
                "declared {} `{}` must be {}",
                if model { "model entry" } else { "asset" },
                reference.display(),
                if model {
                    "a regular file"
                } else {
                    "a regular file or directory"
                }
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_nested_model_and_explicit_resources_only() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("models")).unwrap();
        fs::create_dir_all(root.path().join("blobs")).unwrap();
        fs::create_dir_all(root.path().join("assets")).unwrap();
        fs::write(root.path().join("component.yaml"), "schema: phoxal/component/v0\nmodel: {file: models/entry, root_body: mount}\ncapabilities: {}\nassets: [blobs]\n").unwrap();
        for name in ["models/entry", "blobs/extensionless", "assets/unrelated"] {
            fs::write(root.path().join(name), "resource").unwrap();
        }
        assert_eq!(
            read(root.path()).unwrap(),
            BTreeSet::from([
                PathBuf::from("models/entry"),
                PathBuf::from("blobs/extensionless")
            ])
        );
    }

    #[test]
    fn refuses_symlinked_asset_ancestors_and_directory_model() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        fs::write(root.path().join("real/resource"), "resource").unwrap();
        std::os::unix::fs::symlink("real", root.path().join("alias")).unwrap();
        fs::write(root.path().join("component.yaml"), "schema: phoxal/component/v0\nmodel: {file: real/resource, root_body: mount}\ncapabilities: {}\nassets: [alias/resource]\n").unwrap();
        assert!(matches!(
            read(root.path()),
            Err(Error::Publication(PublicationError::UnsafeAssetPath { .. }))
        ));
        fs::write(root.path().join("component.yaml"), "schema: phoxal/component/v0\nmodel: {file: real, root_body: mount}\ncapabilities: {}\n").unwrap();
        assert!(
            read(root.path())
                .unwrap_err()
                .to_string()
                .contains("regular file")
        );
    }
}
