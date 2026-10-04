//! Connection validation against selected compiled participant contracts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use phoxal::artifact::document::{PortReference, RobotDocument, Source};

use super::{Error, PreparedProject};
pub(crate) fn validate_prepared_connections(
    project: &PreparedProject,
    options: &super::CargoOptions,
) -> Result<(usize, bool), Error> {
    use phoxal::artifact::RuntimeRecord;

    let root = project.layout.root();
    let RobotDocument::V0 {
        services, robot, ..
    } = &project.document;

    let selected = |instance: &str| -> Option<(&Source, Option<&str>)> {
        services
            .get(instance)
            .map(|selection| (&selection.source, selection.binary.as_deref()))
            .or_else(|| {
                robot
                    .components
                    .get(instance)
                    .filter(|component| component.driver.is_some())
                    .map(|component| (&component.source, component.binary.as_deref()))
            })
    };

    let mut contracts: BTreeMap<String, super::artifact::ArtifactContract> = BTreeMap::new();
    for instance in services.keys().chain(
        robot
            .components
            .iter()
            .filter(|(_, component)| component.driver.is_some())
            .map(|(instance, _)| instance),
    ) {
        let Some((source, binary)) = selected(instance) else {
            continue;
        };
        let contract_dir = prepared_root(root, source, binary);
        if !contract_dir.join(phoxal_build::CONTRACT_FILE).is_file() {
            continue;
        }
        let prepared =
            phoxal_build::read_prepared_for(&contract_dir, &selection_identity(source), binary)
                .map_err(|error| Error::DeclarationCheck {
                    message: format!("participant {instance}: {error}"),
                })?;
        let runtime: RuntimeRecord =
            serde_json::from_value(prepared.file.runtime).map_err(|error| {
                Error::DeclarationCheck {
                    message: format!(
                        "participant {instance}: prepared runtime is invalid: {error}"
                    ),
                }
            })?;
        contracts.insert(
            instance.clone(),
            super::artifact::ArtifactContract {
                runtime,
                descriptors: Vec::new(),
                schemas: Vec::new(),
            },
        );
    }
    let prepared_count = contracts.len();

    // The brain declares its own Rust contract; always build it and extract
    // the actual record so its endpoints, bounds, outputs, and unwired
    // required inputs validate exactly as bundle construction would — an
    // empty brain contract passes, an unwired required input fails. Nothing
    // is ever inferred from the connections themselves.
    {
        let target = &project.cargo_sources().brain;
        let output = super::cargo::build_target(project, target, options)?;
        let executable = super::cargo::artifact_path(&output.stdout, target)?;
        let contract = super::artifact::inspect_file(&executable).map_err(|error| match error {
            super::artifact::Error::MissingRecord => Error::MissingArtifactContract {
                role: "brain".to_owned(),
                instance: "brain".to_owned(),
                package: target.package.clone(),
                target: target.target.clone(),
            },
            other => Error::ArtifactInvalid {
                path: executable.clone(),
                message: other.to_string(),
            },
        })?;
        contracts.insert("brain".to_owned(), contract);
    }
    let brain_validated = true;

    // Edges whose consumer is neither the brain nor a prepared participant
    // stay deferred to bundle-time validation; edges to participants
    // without prepared contracts defer their producer side.
    let mut filtered = project.document.clone();
    let RobotDocument::V0 {
        connections: filtered_connections,
        ..
    } = &mut filtered;
    filtered_connections.retain(|field, _| {
        let Ok(consumer) = PortReference::parse(field) else {
            return true;
        };
        consumer.instance == "brain" || contracts.contains_key(&consumer.instance)
    });
    let virtual_producers: Vec<String> = services
        .keys()
        .chain(
            robot
                .components
                .iter()
                .filter(|(_, component)| component.driver.is_some())
                .map(|(instance, _)| instance),
        )
        .filter(|instance| !contracts.contains_key(instance.as_str()))
        .cloned()
        .collect();
    let virtual_producers: Vec<&str> = virtual_producers.iter().map(String::as_str).collect();
    super::artifact::validate_connected_endpoints_with_virtual_producers(
        &filtered,
        &contracts,
        &virtual_producers,
    )
    .map_err(|error| Error::DeclarationCheck {
        message: error.to_string(),
    })?;
    Ok((prepared_count, brain_validated))
}

/// The selection identity of one authored source.
pub(crate) fn selection_identity(source: &Source) -> phoxal_build::PreparedSelection {
    match source {
        Source::Path(path) => phoxal_build::PreparedSelection::Path { path: path.clone() },
        Source::Package(package) => phoxal_build::PreparedSelection::Registry {
            registry: package
                .registry
                .clone()
                .unwrap_or_else(|| "phoxal".to_owned()),
            name: package.name.clone(),
            version: package.version.clone(),
        },
        Source::Git(git) => phoxal_build::PreparedSelection::Git {
            name: git.name.clone(),
            revision: git.rev.clone(),
            url: git.url.clone(),
            path: git.path.clone(),
        },
    }
}

/// The prepared-contract directory of one selection, when composition
/// prepared its Rust contract from a compiled artifact.
pub(crate) fn prepared_root(root: &Path, source: &Source, binary: Option<&str>) -> PathBuf {
    phoxal_build::prepared_dir(root, &selection_identity(source), binary)
}
