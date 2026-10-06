//! Project discovery, authored composition validation, and Cargo orchestration.
//!
//! This module is the source-development boundary used by `cargo-phoxal`.
//! It consumes the shared SDK contracts, without owning supervisor, service,
//! or native simulator implementations.

mod application;
mod archive;
pub mod artifact;
mod bundle;
mod cargo;
mod component_assets;
mod conversions;
mod discovery;
mod document;
mod error;
mod file_lock;
pub(crate) mod manifest_check;
pub(crate) mod participant;
mod passive;
pub mod scenario;
mod selection;
mod simulation;
pub(crate) use simulation::selected_simulator_executable;
mod supervisor;
mod validation;

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

pub use bundle::{CompiledBundle, SimulationModelFacts};
pub use cargo::{CargoOperation, CargoOptions, CargoOutput, CargoSelection, LockMode};
pub use discovery::ProjectLayout;
pub use document::RobotDocument;
pub use error::{DiscoveryError, Error, SourceError};
pub(crate) use participant::phoxal_home;

pub use selection::{SelectedTarget, SourceSelection};
pub use simulation::{
    SimulationBound, SimulationPresentation, SimulationRunOptions, SimulationRunReport,
};

use std::path::{Path, PathBuf};
use std::process::Command;

/// A discovered project with its authored document and root Cargo manifest.
#[derive(Debug, Clone)]
pub struct Project {
    layout: ProjectLayout,
    document: RobotDocument,
}

impl Project {
    /// Discovers and loads a project from a directory, file, or nested source path.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, Error> {
        let layout = ProjectLayout::discover(start)?;
        Self::from_layout(layout)
    }

    /// Loads a project from already discovered canonical paths.
    pub fn from_layout(layout: ProjectLayout) -> Result<Self, Error> {
        let text = std::fs::read_to_string(layout.robot_manifest()).map_err(|source| {
            Error::ReadRobot {
                path: layout.robot_manifest().to_owned(),
                source,
            }
        })?;
        let document = document::parse_and_validate(&text, layout.robot_manifest())?;
        let manifest_text = std::fs::read_to_string(layout.cargo_manifest()).map_err(|source| {
            Error::ReadManifest {
                path: layout.cargo_manifest().to_owned(),
                source,
            }
        })?;
        let manifest = toml::from_str::<toml::Value>(&manifest_text).map_err(|source| {
            Error::ParseManifest {
                path: layout.cargo_manifest().to_owned(),
                source,
            }
        })?;
        if !manifest.get("package").is_some_and(toml::Value::is_table) {
            return Err(Error::VirtualManifest {
                path: layout.cargo_manifest().to_owned(),
            });
        }
        Ok(Self { layout, document })
    }

    /// Prepares the authored Cargo graph and resolves all explicit sources.
    ///
    /// Participant selections come from the authored document. The supervisor
    /// is acquired separately when assembling a bundle, never inserted into
    /// the robot's Rust dependency graph.
    pub fn prepare(&self, options: &CargoOptions) -> Result<PreparedProject, Error> {
        let (_, document) = participant::prepare_graph(&self.layout, options, &self.document)?;
        let metadata = cargo::load_metadata_at(
            self.layout.cargo_manifest(),
            self.layout.root(),
            None,
            options,
        )?;
        let cargo_sources =
            selection::resolve_prepared_sources(&self.layout, &document, &metadata, options)?;
        let cargo_root_package = metadata
            .root_package()
            .cloned()
            .ok_or(SourceError::MissingBrain)?;
        let mut prepared = PreparedProject {
            layout: self.layout.clone(),
            document,
            cargo_metadata: metadata,
            cargo_root_package,
            cargo_sources,
        };
        let target = &prepared.cargo_sources.brain;
        let output = cargo::build_target(&prepared, target, options)?;
        let executable = cargo::artifact_path(&output.stdout, target)?;
        match artifact::inspect_file(&executable) {
            Ok(contract) => {
                prepared.document =
                    conversions::lower(&prepared.document, &contract.summary().runtime)?
            }
            Err(artifact::Error::MissingRecord) => {}
            Err(error) => {
                return Err(Error::ArtifactInvalid {
                    path: executable,
                    message: error.to_string(),
                });
            }
        }
        Ok(prepared)
    }

    /// Compile one simulation bundle, freezing inputs before its single native probe.
    pub fn build_simulation(
        &self,
        options: &CargoOptions,
        scene: &Path,
        output: Option<PathBuf>,
    ) -> Result<(CompiledBundle, PathBuf), Error> {
        let mut request = SimulationRunOptions::new(
            scene,
            SimulationPresentation::Headless,
            SimulationBound::Steps(1),
        )?;
        if let Some(output) = output {
            request = request.with_output(output);
        }
        let snapshot = simulation::prepare_simulation(self, options, &request)?;
        let relative_scene = snapshot
            .frozen
            .scene
            .strip_prefix(snapshot.frozen.staging.path())
            .map_err(|error| Error::SimulationInvalid {
                message: format!("frozen scene escapes its closure: {error}"),
            })?;
        let bundle = simulation::finalize_prepared(&snapshot, &request, None)?;
        let scene = bundle.root().join(relative_scene);
        Ok((bundle, scene))
    }
}

/// A validated project and the Cargo graph used for its selected sources.
#[derive(Debug, Clone)]
pub struct PreparedProject {
    layout: ProjectLayout,
    document: RobotDocument,
    cargo_metadata: cargo_metadata::Metadata,
    cargo_root_package: cargo_metadata::Package,
    cargo_sources: SourceSelection,
}

impl PreparedProject {
    /// Returns the project layout used for this preparation.
    #[must_use]
    pub fn layout(&self) -> &ProjectLayout {
        &self.layout
    }

    /// Returns the validated authored document.
    #[must_use]
    pub fn document(&self) -> &RobotDocument {
        &self.document
    }

    pub(crate) fn cargo_metadata(&self) -> &cargo_metadata::Metadata {
        &self.cargo_metadata
    }

    pub(crate) fn cargo_root_package(&self) -> &cargo_metadata::Package {
        &self.cargo_root_package
    }

    pub(crate) fn cargo_sources(&self) -> &SourceSelection {
        &self.cargo_sources
    }

    pub(crate) fn cargo_manifest_path(&self) -> &Path {
        self.layout.cargo_manifest()
    }

    pub(crate) fn cargo_workdir(&self) -> &Path {
        self.layout.root()
    }

    /// Runs one supported Cargo source-development operation.
    pub fn run(
        &self,
        operation: CargoOperation,
        options: &CargoOptions,
    ) -> Result<Vec<CargoOutput>, Error> {
        cargo::run(self, operation, options)
    }

    /// Validates compiled participant contracts and checks the robot project.
    pub fn check(&self, options: &CargoOptions) -> Result<Vec<CargoOutput>, Error> {
        let (prepared, brain) = manifest_check::validate_prepared_connections(self, options)?;
        if prepared > 0 || brain {
            eprintln!(
                "cargo phoxal: declaration check validated the compiled contracts of {prepared} prepared participant(s){}",
                if brain { " and the compiled brain" } else { "" }
            );
        }
        self.run(CargoOperation::Check, options)
    }

    /// Builds and atomically publishes the complete selected executable bundle.
    ///
    /// The output is a source-side compiled directory containing the selected
    /// brain, service, and component-driver binaries plus the exact supervisor
    /// executable and inspectable manifest records.
    pub fn build_bundle(
        &self,
        options: &CargoOptions,
        output: impl AsRef<Path>,
    ) -> Result<CompiledBundle, Error> {
        bundle::assemble_with_inputs(self, options, output, true, None, None)
    }

    /// Builds an immutable bundle and launches its selected supervisor in the
    /// isolated local namespace.
    pub fn run_local(
        &self,
        options: &CargoOptions,
        output: impl AsRef<Path>,
    ) -> Result<CompiledBundle, Error> {
        let bundle = self.build_bundle(options, output)?;
        let supervisor = bundle.executable("supervisor");
        let state_dir = bundle.root().parent().unwrap_or(Path::new(".")).join("run");
        std::fs::create_dir_all(&state_dir).map_err(|source| Error::BundleDirectory {
            path: state_dir.clone(),
            source,
        })?;
        let status = Command::new(&supervisor)
            .arg(bundle.root())
            .arg("--state-dir")
            .arg(&state_dir)
            .args(["--scope", "local", "--supervisor-id", "local"])
            .status()
            .map_err(|source| Error::SupervisorLaunch {
                message: format!("cannot start {}: {source}", supervisor.display()),
            })?;
        if !status.success() {
            let status = status.code().map_or_else(
                || "terminated by signal".to_owned(),
                |code| code.to_string(),
            );
            return Err(Error::SupervisorLaunch {
                message: format!("{} exited with status {status}", supervisor.display()),
            });
        }
        Ok(bundle)
    }

    /// Returns the default bundle path under Cargo's target directory.
    #[must_use]
    pub fn default_bundle_path(&self, options: &CargoOptions) -> PathBuf {
        let RobotDocument::V0 { robot, .. } = &self.document;
        self.cargo_metadata
            .target_directory
            .as_std_path()
            .join("phoxal")
            .join(&robot.id)
            .join(cargo::effective_target(options))
            .join(options.profile.as_deref().unwrap_or(if options.release {
                "release"
            } else {
                "dev"
            }))
            .join("build")
    }

    pub(crate) fn assembly_targets(&self) -> Vec<(String, &SelectedTarget)> {
        let mut targets = vec![(String::from("brain"), &self.cargo_sources.brain)];
        targets.extend(
            self.cargo_sources
                .services
                .iter()
                .map(|(instance, service)| (instance.clone(), &service.binary)),
        );
        targets.extend(
            self.cargo_sources
                .components
                .iter()
                .filter_map(|(instance, component)| {
                    component
                        .driver
                        .as_ref()
                        .map(|driver| (instance.clone(), &driver.binary))
                }),
        );
        targets
    }

    pub(crate) fn executable_role(&self, instance: &str) -> String {
        if instance == "brain" {
            return "brain".to_owned();
        }
        if self.cargo_sources.services.contains_key(instance) {
            return "service".to_owned();
        }
        if self
            .cargo_sources
            .components
            .get(instance)
            .and_then(|component| component.driver.as_ref())
            .is_some()
        {
            return "driver".to_owned();
        }
        "execution".to_owned()
    }
}
