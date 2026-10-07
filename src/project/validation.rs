//! Validation that requires the exact compiled Runtime contract.
//!
//! Cargo metadata identifies packages and targets, but it cannot expose the
//! associated `Runtime::Config` schema or port bindings.  This module builds
//! the selected executable targets, extracts their native contract records,
//! and validates authored values against the schema owned by that exact
//! artifact.  It never executes a target binary.

use std::collections::BTreeMap;

use crate::project::artifact::{self, ArtifactContract};
use crate::project::document::RobotDocument;
use crate::project::{Error, PreparedProject};
use phoxal::artifact::RuntimeRecord;

pub(crate) type ArtifactKey = (String, String);

/// Validate authored service and component-driver configuration against the
/// exact contract selected for each instance.
pub(crate) fn validate_configurations(
    prepared: &PreparedProject,
    contracts: &BTreeMap<ArtifactKey, ArtifactContract>,
) -> Result<(), Error> {
    for (instance, target) in prepared.assembly_targets() {
        let key = (target.package_id.clone(), target.target.clone());
        let Some(contract) = contracts.get(&key) else {
            return Err(Error::ArtifactCapture {
                package: target.package.clone(),
                target: target.target.clone(),
                message: "selected executable has no extracted contract".to_owned(),
            });
        };
        let role = prepared.executable_role(&instance);
        let (field, mut value) = authored_configuration(prepared, &instance, &role)?;
        let RuntimeRecord::V0 { config_schema, .. } = &contract.runtime;
        if schema_is_null(config_schema) && value.is_object() {
            // `Config = ()` has no authored fields.  Omission and an empty
            // object are the two source forms accepted for that declaration;
            // a non-empty object is still rejected by the exact schema below.
            if value.as_object().is_some_and(serde_json::Map::is_empty) {
                value = serde_json::Value::Null;
            }
        }
        let validator = jsonschema::validator_for(config_schema).map_err(|error| {
            Error::ConfigurationInvalid {
                role: role.clone(),
                instance: instance.clone(),
                field: format!("{field} (compiled config_schema)"),
                package: target.package.clone(),
                message: error.to_string(),
            }
        })?;
        if let Some(error) = validator.iter_errors(&value).next() {
            return Err(Error::ConfigurationInvalid {
                role,
                instance,
                field,
                package: target.package.clone(),
                message: error.to_string(),
            });
        }
    }
    Ok(())
}

fn authored_configuration(
    prepared: &PreparedProject,
    instance: &str,
    role: &str,
) -> Result<(String, serde_json::Value), Error> {
    match role {
        "brain" => {
            let RobotDocument::V0 { robot, .. } = prepared.document();
            let phoxal::artifact::document::RobotSection { brain, .. } = robot;
            Ok((
                "robot.brain.config".to_owned(),
                brain
                    .as_ref()
                    .and_then(|brain| brain.config.clone())
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
            ))
        }
        "service" => {
            let RobotDocument::V0 { robot, .. } = prepared.document();
            let phoxal::artifact::document::RobotSection { services, .. } = robot;
            Ok((
                format!("robot.services.{instance}.config"),
                services
                    .get(instance)
                    .and_then(|selection| selection.config.clone())
                    .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
            ))
        }
        "driver" => {
            let RobotDocument::V0 { robot, .. } = prepared.document();
            let value = robot
                .components
                .get(instance)
                .and_then(|component| component.driver.as_ref())
                .and_then(|driver| driver.config.clone())
                .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
            Ok((format!("robot.components.{instance}.driver.config"), value))
        }
        _ => Err(Error::ConfigurationInvalid {
            role: role.to_owned(),
            instance: instance.to_owned(),
            field: "configuration".to_owned(),
            package: "unknown".to_owned(),
            message: "selected target does not accept authored configuration".to_owned(),
        }),
    }
}

fn schema_is_null(schema: &serde_json::Value) -> bool {
    schema.get("type").and_then(serde_json::Value::as_str) == Some("null")
}

pub(crate) fn validate_connections_for_document(
    prepared: &PreparedProject,
    contracts: &BTreeMap<ArtifactKey, ArtifactContract>,
    document: &RobotDocument,
) -> Result<(), Error> {
    let mut instance_contracts = BTreeMap::new();
    for (instance, target) in prepared.assembly_targets() {
        let key = (target.package_id.clone(), target.target.clone());
        let Some(contract) = contracts.get(&key) else {
            continue;
        };
        instance_contracts.insert(instance, contract.clone());
    }
    artifact::validate_connected_endpoints(document, &instance_contracts).map_err(|error| {
        Error::ArtifactInvalid {
            path: prepared.layout().robot_manifest().to_owned(),
            message: error.to_string(),
        }
    })
}
