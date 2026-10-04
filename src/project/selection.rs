//! Exact participants prepared outside the robot Cargo graph.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::project::document::{
    BrainSelection, ComponentDocument, RobotDocument, ValidateComponentDocument,
};
use crate::project::error::SourceError;
use crate::project::participant;
use crate::project::{CargoOptions, Error, ProjectLayout};
use cargo_metadata::{Metadata, Package, PackageId, Target};

/// Where Cargo obtained a selected package.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PackageSource {
    /// A package in the robot's local Cargo workspace.
    Local { manifest_path: PathBuf },
    /// A pinned Git package.
    Git { source: String },
}

/// One Cargo target selected for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedTarget {
    /// Package identity returned by Cargo.
    pub package_id: String,
    /// Package name.
    pub package: String,
    /// Target name passed to Cargo's `--bin` selector.
    pub target: String,
    /// Main source file reported by Cargo, when this target is built from source.
    pub source_path: PathBuf,
    /// A participant built in its own Cargo workspace, outside the robot graph.
    pub manifest_path: Option<PathBuf>,
    /// A prepared registry or Git executable supplied without a Cargo build.
    pub executable: Option<PathBuf>,
    /// Features required by the target.
    pub required_features: Vec<String>,
}

/// One selected service package and its executable/library targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedService {
    /// `robot.yaml` instance identity.
    pub instance: String,
    /// Cargo package identity.
    pub package_id: String,
    /// Cargo package name.
    pub package: String,
    /// Version reported by Cargo for the selected package.
    pub version: String,
    /// Package source and provenance class.
    pub source: PackageSource,
    /// Service executable selected for assembly.
    pub binary: SelectedTarget,
}

/// One selected component package. Components may be passive and therefore do
/// not need an executable target in the project graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedComponent {
    /// `robot.components.<id>` instance identity.
    pub instance: String,
    /// Cargo package identity.
    pub package_id: String,
    /// Cargo package name.
    pub package: String,
    /// Version reported by Cargo for the selected package.
    pub version: String,
    /// Package source and provenance class.
    pub source: PackageSource,
    /// Persistent site in the parent robot model receiving this instance.
    pub mount_site: String,
    /// Parsed component-owned semantic and native binding declaration.
    pub definition: ComponentDocument,
    /// Authored or prepared files used to stage the component's model.
    pub source_root: PathBuf,
    /// The component-owned driver selected by this instance, when its
    /// authored `driver` block requests a real process.
    pub driver: Option<SelectedDriver>,
}

/// One component-owned driver selected by the authored component source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedDriver {
    /// Cargo package identity.
    pub package_id: String,
    /// Cargo package name.
    pub package: String,
    /// Package source and provenance class.
    pub source: PackageSource,
    /// Executable selected for this mounted component instance.
    pub binary: SelectedTarget,
}

/// The complete Cargo-backed selection made by one validated robot document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSelection {
    /// The mandatory root-local brain binary.
    pub brain: SelectedTarget,
    /// Explicit behavioral service selections in authored map order.
    pub services: BTreeMap<String, SelectedService>,
    /// Mounted component package selections in authored map order.
    pub components: BTreeMap<String, SelectedComponent>,
}

pub(crate) fn resolve_prepared_sources(
    layout: &ProjectLayout,
    document: &RobotDocument,
    metadata: &Metadata,
    options: &CargoOptions,
) -> Result<SourceSelection, Error> {
    let RobotDocument::V0 {
        brain: authored_brain,
        services: authored_services,
        robot,
        ..
    } = document;
    let root = metadata.root_package().ok_or(SourceError::MissingBrain)?;
    let brain = resolve_brain(root, authored_brain.as_ref(), metadata)?;
    let installed = participant::selected_installations(layout, document, options)?;
    let mut services = BTreeMap::new();
    for instance in authored_services.keys() {
        let selected = installed
            .get(instance)
            .ok_or_else(|| Error::ArtifactCapture {
                package: instance.clone(),
                target: instance.clone(),
                message: "participant installation missing".to_owned(),
            })?;
        let binary = installed_target(selected);
        services.insert(
            instance.clone(),
            SelectedService {
                instance: instance.clone(),
                package_id: binary.package_id.clone(),
                package: selected.package.clone(),
                version: selected.version.clone(),
                source: selected.source.clone(),
                binary,
            },
        );
    }
    let mut components = BTreeMap::new();
    for (instance, component) in &robot.components {
        if component.driver.is_none() {
            let selected = super::passive::select(layout, &component.source, options)?;
            let definition = load_component_definition(instance, &selected.source_root)?;
            components.insert(
                instance.clone(),
                SelectedComponent {
                    instance: instance.clone(),
                    package_id: selected.package_id,
                    package: selected.package,
                    version: selected.version,
                    source: selected.source,
                    mount_site: component.mount_site.clone(),
                    definition,
                    source_root: selected.source_root,
                    driver: None,
                },
            );
            continue;
        }
        let selected = installed
            .get(instance)
            .ok_or_else(|| Error::ArtifactCapture {
                package: instance.clone(),
                target: instance.clone(),
                message: "component installation missing".to_owned(),
            })?;
        let definition = load_installed_component_definition(instance, selected)?;
        let binary = installed_target(selected);
        components.insert(
            instance.clone(),
            SelectedComponent {
                instance: instance.clone(),
                package_id: binary.package_id.clone(),
                package: selected.package.clone(),
                version: selected.version.clone(),
                source: selected.source.clone(),
                mount_site: component.mount_site.clone(),
                definition,
                source_root: selected.source_root.clone(),
                driver: component.driver.as_ref().map(|_| SelectedDriver {
                    package_id: binary.package_id.clone(),
                    package: selected.package.clone(),
                    source: selected.source.clone(),
                    binary,
                }),
            },
        );
    }
    Ok(SourceSelection {
        brain,
        services,
        components,
    })
}

fn installed_target(selected: &participant::InstalledSelection) -> SelectedTarget {
    SelectedTarget {
        package_id: selected.package_id.clone(),
        package: selected.package.clone(),
        target: selected.binary.clone(),
        source_path: selected.source_path.clone().unwrap_or_default(),
        manifest_path: selected.manifest_path.clone(),
        executable: selected.executable.clone(),
        required_features: Vec::new(),
    }
}

fn load_installed_component_definition(
    instance: &str,
    selected: &participant::InstalledSelection,
) -> Result<ComponentDocument, Error> {
    load_component_definition(instance, &selected.source_root)
}

fn load_component_definition(
    instance: &str,
    source_root: &std::path::Path,
) -> Result<ComponentDocument, Error> {
    let path = source_root.join("component.yaml");
    let text = std::fs::read_to_string(&path).map_err(|source| Error::ArtifactFile {
        path: path.clone(),
        source,
    })?;
    let definition: ComponentDocument =
        serde_yaml::from_str(&text).map_err(|source| Error::ArtifactInvalid {
            path: path.clone(),
            message: source.to_string(),
        })?;
    definition
        .validate()
        .map_err(|message| Error::ArtifactInvalid {
            path: path.clone(),
            message,
        })?;
    let ComponentDocument::V0 { model, .. } = &definition;
    if !source_root.join(&model.file).is_file() {
        return Err(Error::ArtifactInvalid {
            path,
            message: format!("component {instance} model is missing"),
        });
    }
    Ok(definition)
}

pub(crate) fn resolve_brain(
    root: &Package,
    selection: Option<&BrainSelection>,
    metadata: &Metadata,
) -> Result<SelectedTarget, SourceError> {
    let binaries = root
        .targets
        .iter()
        .filter(|target| target.is_bin())
        .collect::<Vec<_>>();
    if let Some(selection) = selection.and_then(|selection| selection.binary.as_deref()) {
        let target = binaries
            .iter()
            .find(|target| target.name == selection)
            .copied()
            .ok_or_else(|| SourceError::InvalidBrainBinary {
                binary: selection.to_owned(),
            })?;
        let selected = selected_target(root, target);
        ensure_target_features(&selected, root)?;
        return Ok(selected);
    }

    let enabled = enabled_features(metadata, &root.id);
    let eligible = binaries
        .iter()
        .filter(|target| {
            target
                .required_features
                .iter()
                .all(|feature| enabled.contains(feature))
        })
        .copied()
        .collect::<Vec<_>>();
    match eligible.as_slice() {
        [] if binaries.is_empty() => Err(SourceError::MissingBrain),
        [] => {
            let required = binaries
                .iter()
                .flat_map(|target| target.required_features.iter().cloned())
                .collect::<Vec<_>>();
            Err(SourceError::BrainFeatures {
                binary: binaries
                    .iter()
                    .map(|target| target.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                features: required.join(", "),
            })
        }
        [target] => Ok(selected_target(root, target)),
        _ => Err(SourceError::AmbiguousBrain {
            candidates: eligible
                .iter()
                .map(|target| target.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

fn selected_target(package: &Package, target: &Target) -> SelectedTarget {
    SelectedTarget {
        package_id: package.id.to_string(),
        package: package.name.to_string(),
        target: target.name.clone(),
        source_path: PathBuf::from(target.src_path.as_std_path()),
        manifest_path: None,
        executable: None,
        required_features: target.required_features.clone(),
    }
}

fn enabled_features(
    metadata: &Metadata,
    package_id: &PackageId,
) -> std::collections::BTreeSet<String> {
    // The feature list is a resolve-node fact, but callers without the complete
    // graph still receive an empty set and the target's requirements remain
    // visible in the selected target metadata.
    metadata
        .resolve
        .as_ref()
        .and_then(|resolve| resolve.nodes.iter().find(|node| node.id == *package_id))
        .map(|node| {
            node.features
                .iter()
                .map(|feature| feature.as_ref().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

fn ensure_target_features(target: &SelectedTarget, package: &Package) -> Result<(), SourceError> {
    let missing = target
        .required_features
        .iter()
        .filter(|feature| !package.features.contains_key(feature.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(SourceError::UndefinedRequiredFeatures {
            binary: target.target.clone(),
            features: missing.join(", "),
        })
    }
}
