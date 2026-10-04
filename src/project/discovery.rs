use std::path::{Path, PathBuf};

use crate::project::error::DiscoveryError;

const ROBOT_FILE: &str = "robot.yaml";
const CARGO_FILE: &str = "Cargo.toml";

/// The canonical source paths for one discovered robot project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectLayout {
    root: PathBuf,
    robot_manifest: PathBuf,
    cargo_manifest: PathBuf,
}

impl ProjectLayout {
    /// Finds the nearest ancestor containing `robot.yaml`.
    ///
    /// The Cargo package is required beside that document so the project owns
    /// its root-local brain. The Cargo lock remains workspace-owned and is
    /// resolved after metadata preparation.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, DiscoveryError> {
        let supplied = start.as_ref().to_owned();
        let canonical = supplied
            .canonicalize()
            .map_err(|source| DiscoveryError::Resolve {
                path: supplied.clone(),
                source,
            })?;
        let mut cursor = if canonical.is_file() {
            canonical.parent().map(Path::to_path_buf).ok_or_else(|| {
                DiscoveryError::MissingRobot {
                    start: supplied.clone(),
                }
            })?
        } else {
            canonical
        };

        loop {
            let robot_manifest = cursor.join(ROBOT_FILE);
            if robot_manifest.is_file() {
                let cargo_manifest = cursor.join(CARGO_FILE);
                if !cargo_manifest.is_file() {
                    return Err(DiscoveryError::MissingManifest { root: cursor });
                }
                return Ok(Self {
                    root: cursor,
                    robot_manifest: robot_manifest.canonicalize().map_err(|source| {
                        DiscoveryError::Resolve {
                            path: robot_manifest,
                            source,
                        }
                    })?,
                    cargo_manifest: cargo_manifest.canonicalize().map_err(|source| {
                        DiscoveryError::Resolve {
                            path: cargo_manifest,
                            source,
                        }
                    })?,
                });
            }
            if !cursor.pop() {
                return Err(DiscoveryError::MissingRobot { start: supplied });
            }
        }
    }

    /// Directory containing the authored robot and root Cargo package.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Authored `robot.yaml` path.
    #[must_use]
    pub fn robot_manifest(&self) -> &Path {
        &self.robot_manifest
    }

    /// Root package `Cargo.toml` path.
    #[must_use]
    pub fn cargo_manifest(&self) -> &Path {
        &self.cargo_manifest
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_walks_from_a_nested_source_directory() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("robot");
        let nested = root.join("src/nested");
        std::fs::create_dir_all(&nested)?;
        std::fs::write(root.join("robot.yaml"), "robot: {}\n")?;
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='robot'\nversion='0.1.0'\n",
        )?;

        let layout = ProjectLayout::discover(&nested)?;
        assert_eq!(layout.root(), root.canonicalize()?);
        assert_eq!(
            layout.robot_manifest(),
            &root.join("robot.yaml").canonicalize()?
        );
        assert_eq!(
            layout.cargo_manifest(),
            &root.join("Cargo.toml").canonicalize()?
        );
        Ok(())
    }

    #[test]
    fn a_robot_without_a_root_package_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join("robot");
        std::fs::create_dir_all(&root)?;
        std::fs::write(root.join("robot.yaml"), "robot: {}\n")?;

        let error = ProjectLayout::discover(&root).expect_err("package is required");
        assert!(matches!(error, DiscoveryError::MissingManifest { .. }));
        Ok(())
    }
}
