//! Tool-side authored-file validation.
//!
//! The inert record family (`RobotDocument`, `ComponentDocument`,
//! `BrainSelection`, capability declarations, etc.) lives in
//! `phoxal::artifact::document`. This module re-exports those
//! types and adds the project-side validation logic that the inert
//! framework module intentionally does not own.
//!
//! Authored YAML parsing is owned here; tool callers use [`parse_and_validate`]
//! to combine parsing and validation.
#![deny(unsafe_code)]
use crate::project::error::{ValidationError, ValidationErrors};
pub use phoxal::artifact::document::{
    BrainSelection, ComponentDocument, PortReference, RobotDocument, Source,
};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
/// Parses and validates a `robot.yaml` document with its authored path
/// attached to errors. The YAML parse and the validation belong
/// together at this boundary because a malformed file must surface both
/// structural and semantic errors in one round-trip.
pub(crate) fn parse_and_validate(
    text: &str,
    path: &Path,
) -> Result<RobotDocument, crate::project::error::Error> {
    let document: RobotDocument =
        serde_yaml::from_str(text).map_err(|source| crate::project::error::Error::ParseRobot {
            path: path.to_owned(),
            source,
        })?;
    document
        .validate()
        .map_err(|errors| crate::project::error::Error::InvalidRobot {
            path: path.to_owned(),
            errors: ValidationErrors(errors),
        })?;
    Ok(document)
}
/// Validates source-language and composition rules for a `RobotDocument`.
///
/// Validation remains tool-owned and is implemented as a trait over the inert
/// framework record so callers can read `document.validate()?`.
pub trait ValidateDocument {
    /// Run all source-language and composition checks and collect any
    /// validation errors into a single `Vec`.
    fn validate(&self) -> Result<(), Vec<ValidationError>>;
}
impl ValidateDocument for RobotDocument {
    fn validate(&self) -> Result<(), Vec<ValidationError>> {
        let mut errors = Vec::new();
        validate_schema(self, &mut errors);
        validate_robot(self, &mut errors);
        validate_brain(self, &mut errors);
        validate_supervisor(self, &mut errors);
        validate_services(self, &mut errors);
        validate_connections(self, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
/// Validates a single `ComponentDocument` against the project-side
/// rules the framework record module does not own.
pub trait ValidateComponentDocument {
    /// Run all component-side checks and return any errors as a single
    /// joined string for the supervisor and publication tooling.
    fn validate(&self) -> Result<(), String>;
}
impl ValidateComponentDocument for ComponentDocument {
    fn validate(&self) -> Result<(), String> {
        let mut errors: Vec<String> = Vec::new();
        validate_component_model(self, &mut errors);
        validate_component_capabilities(self, &mut errors);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
fn validate_schema(_document: &RobotDocument, _errors: &mut Vec<ValidationError>) {}
fn validate_robot(document: &RobotDocument, errors: &mut Vec<ValidationError>) {
    let RobotDocument::V0 { robot, .. } = document;
    let phoxal::artifact::document::RobotSection { services, .. } = robot;
    if robot.id.trim().is_empty() {
        errors.push(ValidationError::EmptyRobotId);
    } else if !is_identifier(&robot.id) {
        errors.push(ValidationError::InvalidIdentifier {
            field: "robot.id".to_owned(),
            value: robot.id.clone(),
        });
    }
    for (instance, component) in &robot.components {
        push_identifier_error(&format!("robot.components.{instance}"), instance, errors);
        if instance.contains("__") {
            errors.push(ValidationError::ReservedNamespaceSeparator {
                field: format!("robot.components.{instance}"),
                value: instance.clone(),
            });
        }
        if services.contains_key(instance) {
            errors.push(ValidationError::InstanceCollision {
                instance: instance.clone(),
            });
        }
        if instance == "brain" {
            errors.push(ValidationError::ReservedBrainId {
                field: "robot.components".to_owned(),
            });
        }
        validate_source(
            &format!("robot.components.{instance}.source"),
            &component.source,
            errors,
        );
        if component.mount_site.trim().is_empty() {
            errors.push(ValidationError::EmptySourceKey {
                field: format!("robot.components.{instance}.mount_site"),
            });
        }
        if let Some(driver) = &component.driver {
            if let Some(binary) = &driver.binary {
                push_identifier_error(
                    &format!("robot.components.{instance}.driver.binary"),
                    binary,
                    errors,
                );
            }
            if let Some(config) = &driver.config {
                push_config_errors(
                    config,
                    &format!("robot.components.{instance}.driver.config"),
                    errors,
                );
            }
        }
    }
}
fn validate_brain(document: &RobotDocument, errors: &mut Vec<ValidationError>) {
    let RobotDocument::V0 { robot, .. } = document;
    let phoxal::artifact::document::RobotSection { brain, .. } = robot;
    if let Some(brain) = brain
        && let Some(binary) = &brain.binary
        && (binary.trim().is_empty() || !is_identifier(binary))
    {
        errors.push(ValidationError::InvalidIdentifier {
            field: "robot.brain.binary".to_owned(),
            value: binary.clone(),
        });
    }
    if let Some(config) = brain.as_ref().and_then(|brain| brain.config.as_ref()) {
        push_config_errors(config, "robot.brain.config", errors);
    }
}
/// Validates the authored supervisor selection.
///
/// The supervisor shares the participants' source type while remaining an
/// application: it is never a service instance or connection participant.
fn validate_supervisor(document: &RobotDocument, errors: &mut Vec<ValidationError>) {
    let RobotDocument::V0 { robot, supervisor } = document;
    let services = &robot.services;
    validate_source("supervisor.source", &supervisor.source, errors);
    if let Some(binary) = &supervisor.binary
        && (binary.trim().is_empty() || !is_identifier(binary))
    {
        errors.push(ValidationError::InvalidIdentifier {
            field: "supervisor.binary".to_owned(),
            value: binary.clone(),
        });
    }
    if services.contains_key("supervisor") {
        errors.push(ValidationError::InstanceCollision {
            instance: "supervisor".to_owned(),
        });
    }
}
fn validate_services(document: &RobotDocument, errors: &mut Vec<ValidationError>) {
    let RobotDocument::V0 { robot, .. } = document;
    let phoxal::artifact::document::RobotSection { services, .. } = robot;
    for (service, selection) in services {
        push_identifier_error(&format!("robot.services.{service}"), service, errors);
        validate_source(
            &format!("robot.services.{service}.source"),
            &selection.source,
            errors,
        );
        if service == "brain" {
            errors.push(ValidationError::ReservedBrainId {
                field: "robot.services".to_owned(),
            });
        }
        if let Some(binary) = &selection.binary
            && (binary.trim().is_empty() || !is_identifier(binary))
        {
            errors.push(ValidationError::InvalidService {
                service: service.clone(),
                message: format!("binary '{binary}' is not a valid target name"),
            });
        }
        if let Some(config) = &selection.config {
            push_config_errors(config, &format!("robot.services.{service}.config"), errors);
        }
    }
}
pub(super) fn validate_source(field: &str, source: &Source, errors: &mut Vec<ValidationError>) {
    match source {
        Source::Path(path) => {
            if path.trim().is_empty() || !Path::new(path).is_relative() {
                errors.push(ValidationError::InvalidIdentifier {
                    field: format!("{field}.path"),
                    value: path.clone(),
                });
            }
        }
        Source::Git(git) => {
            push_identifier_error(&format!("{field}.git.name"), &git.name, errors);
            if git.url.trim().is_empty()
                || git.rev.len() != 40
                || !git.rev.bytes().all(|byte| byte.is_ascii_hexdigit())
                || git.path.as_ref().is_some_and(|path| {
                    let path = Path::new(path);
                    path.as_os_str().is_empty()
                        || !path.is_relative()
                        || path
                            .components()
                            .any(|part| !matches!(part, std::path::Component::Normal(_)))
                })
            {
                errors.push(ValidationError::InvalidSource {
                    field: field.to_owned(),
                    message: "Git source needs a URL, full commit, and safe package path"
                        .to_owned(),
                });
            }
        }
    }
}
fn validate_connections(document: &RobotDocument, errors: &mut Vec<ValidationError>) {
    let known = document.instance_ids();
    let RobotDocument::V0 { robot, .. } = document;
    let phoxal::artifact::document::RobotSection { services, .. } = robot;
    let connections = document.connection_sources();
    for (consumer_text, sources) in &connections {
        if sources.is_empty() {
            errors.push(ValidationError::EmptySourceKey {
                field: document.binding_path(consumer_text),
            });
        }
        let consumer = match PortReference::parse(consumer_text) {
            Ok(reference) => reference,
            Err(_) => {
                errors.push(ValidationError::InvalidPortReference {
                    field: document.binding_path(consumer_text),
                    value: consumer_text.clone(),
                });
                continue;
            }
        };
        if !known.contains(&consumer.instance) {
            errors.push(ValidationError::UnknownConnectionInstance {
                field: document.binding_path(consumer_text),
                instance: consumer.instance.clone(),
            });
        } else if consumer.instance != "brain"
            && !services.contains_key(&consumer.instance)
            && robot
                .components
                .get(&consumer.instance)
                .is_none_or(|component| component.driver.is_none())
        {
            errors.push(ValidationError::InvalidConnectionConsumer {
                field: document.binding_path(consumer_text),
                instance: consumer.instance.clone(),
            });
        }
        let source_values = sources.as_slice();
        let mut seen = BTreeSet::new();
        for source_text in source_values {
            let source = match PortReference::parse(source_text) {
                Ok(reference) => reference,
                Err(_) => {
                    errors.push(ValidationError::InvalidPortReference {
                        field: document.binding_path(consumer_text),
                        value: source_text.clone(),
                    });
                    continue;
                }
            };
            if !known.contains(&source.instance) {
                errors.push(ValidationError::UnknownConnectionInstance {
                    field: document.binding_path(consumer_text),
                    instance: source.instance,
                });
            }
            if !seen.insert(source_text) {
                errors.push(ValidationError::DuplicateConnectionSource {
                    field: document.binding_path(consumer_text),
                    producer: source_text.clone(),
                });
            }
        }
    }
}
fn validate_component_model(document: &ComponentDocument, errors: &mut Vec<String>) {
    let ComponentDocument::V0 { model, .. } = document;
    if model.file.as_os_str().is_empty() || model.file.to_string_lossy().contains("..") {
        errors.push(format!(
            "component model path must be a relative, parent-free POSIX path: got '{}'",
            model.file.display()
        ));
    }
    if !is_identifier(&model.root_body) {
        errors.push(format!(
            "component root_body '{}' must be a valid native body name",
            model.root_body
        ));
    }
}
fn validate_component_capabilities(document: &ComponentDocument, errors: &mut Vec<String>) {
    let ComponentDocument::V0 { capabilities, .. } = document;
    let mut seen_names = BTreeSet::new();
    for (name, capability) in capabilities {
        if !is_identifier(name) {
            errors.push(format!(
                "capability name '{name}' must be a valid identifier"
            ));
        }
        if !seen_names.insert(name) {
            errors.push(format!("capability name '{name}' is duplicated"));
        }
        if !is_identifier(&capability.target.id) {
            errors.push(format!(
                "capability '{name}' binds native id '{}' which is not a valid native identifier",
                capability.target.id
            ));
        }
    }
}
fn push_identifier_error(field: &str, value: &str, errors: &mut Vec<ValidationError>) {
    if !is_identifier(value) {
        errors.push(ValidationError::InvalidIdentifier {
            field: field.to_owned(),
            value: value.to_owned(),
        });
    }
}
fn push_config_errors(value: &Value, field: &str, errors: &mut Vec<ValidationError>) {
    match value {
        Value::Null => errors.push(ValidationError::NullConfiguration {
            field: field.to_owned(),
        }),
        Value::Object(_) => {}
        _ => errors.push(ValidationError::NonMappingConfiguration {
            field: field.to_owned(),
        }),
    }
}
pub(crate) fn is_identifier(value: &str) -> bool {
    phoxal::artifact::document::is_identifier(value)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_and_validate_accepts_a_minimal_document() {
        let yaml = r#"
schema: phoxal/robot/v0
robot:
  id: rover
supervisor:
  source: { path: supervisor }
"#;
        let doc = parse_and_validate(yaml, Path::new("robot.yaml")).expect("valid");
        let RobotDocument::V0 { robot, .. } = &doc;
        assert_eq!(robot.id, "rover");
    }
    #[test]
    fn parse_and_validate_collects_identifier_errors() {
        let yaml = r#"
schema: phoxal/robot/v0
robot:
  id: "Rover"
supervisor:
  source: { path: supervisor }
"#;
        let err =
            parse_and_validate(yaml, Path::new("robot.yaml")).expect_err("invalid identifier");
        assert!(matches!(
            err,
            crate::project::error::Error::InvalidRobot { .. }
        ));
    }
    #[test]
    fn component_validation_accepts_explicit_native_capabilities_and_refuses_bad_root() {
        let text = "schema: phoxal/component/v0\nmodel: {file: models/entry, root_body: mount}\ncapabilities:\n  encoder:\n    kind: encoder\n    publish_rate_hz: 50.0\n    target: {kind: joint, id: wheel_joint}\nassets: [resources]\n";
        let component: ComponentDocument = serde_yaml::from_str(text).expect("component");
        component
            .validate()
            .expect("typed native binding validates");
        let invalid: ComponentDocument =
            serde_yaml::from_str(&text.replace("root_body: mount", "root_body: ../escape"))
                .expect("component");
        assert!(invalid.validate().is_err());
    }
}
