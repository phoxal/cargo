//! Ordered tool-owned YAML composition before strict authored validation.
use super::{Error, RobotDocument};
use serde_yaml::{Mapping, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct Composition {
    pub(crate) document: RobotDocument,
    pub(crate) inputs: Vec<(PathBuf, Vec<u8>)>,
}
impl Composition {
    pub(crate) fn load(root: &Path, files: &[PathBuf]) -> Result<Self, Error> {
        let inputs = if files.is_empty() {
            vec![root.join("robot.yaml")]
        } else {
            files.to_vec()
        };
        let mut origins = BTreeMap::new();
        let mut captured = Vec::new();
        let mut resolved = Value::Mapping(Mapping::new());
        for path in &inputs {
            let canonical = path.canonicalize().map_err(|source| Error::ReadRobot {
                path: path.clone(),
                source,
            })?;
            if !canonical.starts_with(root) {
                return Err(invalid(
                    path,
                    "",
                    "layer files must belong to the discovered robot project",
                ));
            }
            let text = std::fs::read_to_string(path).map_err(|source| Error::ReadRobot {
                path: path.clone(),
                source,
            })?;
            captured.push((path.clone(), text.as_bytes().to_vec()));
            // The Value deserializer rejects repeated mapping keys, including nested keys.
            let layer: Value = serde_yaml::from_str(&text).map_err(|source| Error::ParseRobot {
                path: path.clone(),
                source,
            })?;
            let schema = layer
                .as_mapping()
                .and_then(|map| map.get(Value::String("schema".into())))
                .and_then(Value::as_str);
            if schema != Some("phoxal/robot/v0") {
                return Err(invalid(
                    path,
                    "schema",
                    "each layer must declare schema: phoxal/robot/v0",
                ));
            }
            record_origins(&layer, path, "", &mut origins);
            merge(&mut resolved, layer, path, "")?;
        }
        let text = serde_yaml::to_string(&resolved).map_err(|source| Error::ParseRobot {
            path: root.join("robot.yaml"),
            source,
        })?;
        let document = super::document::parse_and_validate(
            &text,
            inputs.last().unwrap_or(&root.join("robot.yaml")),
        )
        .map_err(|error| match error {
            Error::InvalidRobot { errors, .. } => {
                let messages = errors
                    .0
                    .into_iter()
                    .map(|error| {
                        let message = error.to_string();
                        let local = match &error {
                            super::error::ValidationError::EmptyRobotId => "robot.id",
                            super::error::ValidationError::InvalidIdentifier { field, .. }
                            | super::error::ValidationError::ReservedNamespaceSeparator {
                                field,
                                ..
                            }
                            | super::error::ValidationError::ReservedBrainId { field }
                            | super::error::ValidationError::EmptySourceKey { field }
                            | super::error::ValidationError::NullConfiguration { field }
                            | super::error::ValidationError::NonMappingConfiguration { field }
                            | super::error::ValidationError::InvalidPortReference {
                                field, ..
                            }
                            | super::error::ValidationError::DuplicateConnectionSource {
                                field,
                                ..
                            }
                            | super::error::ValidationError::UnknownConnectionInstance {
                                field,
                                ..
                            }
                            | super::error::ValidationError::InvalidConnectionConsumer {
                                field,
                                ..
                            } => field.as_str(),
                            _ => "services",
                        };
                        let origin = origins
                            .iter()
                            .filter(|(field, _)| {
                                local == field.as_str() || local.starts_with(&format!("{field}."))
                            })
                            .max_by_key(|(field, _)| field.len())
                            .map(|(_, file)| file)
                            .unwrap_or(&inputs[0]);
                        format!("{}: {message}", origin.display())
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                Error::DeclarationCheck { message: messages }
            }
            Error::ParseRobot { source, .. } => Error::DeclarationCheck {
                message: format!(
                    "resolved composition (selected inputs: {}): {source}",
                    inputs
                        .iter()
                        .map(|path| path.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            error => error,
        })?;
        Ok(Self {
            document,
            inputs: captured,
        })
    }
}
fn record_origins(
    value: &Value,
    file: &Path,
    local: &str,
    origins: &mut BTreeMap<String, PathBuf>,
) {
    // Replacements and deletions erase prior descendant provenance too.
    if !matches!(value, Value::Mapping(_)) {
        origins.retain(|field, _| field != local && !field.starts_with(&format!("{local}.")));
    }
    if !local.is_empty() {
        origins.insert(local.into(), file.to_owned());
    }
    match value {
        Value::Tagged(tag) => record_origins(&tag.value, file, local, origins),
        Value::Mapping(mapping) => {
            for (key, value) in mapping {
                if let Some(name) = key.as_str() {
                    let child = if local.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{local}.{name}")
                    };
                    record_origins(value, file, &child, origins);
                }
            }
        }
        _ => {}
    }
}
fn invalid(path: &Path, local: &str, message: &str) -> Error {
    Error::DeclarationCheck {
        message: format!("{}:{local}: {message}", path.display()),
    }
}
fn merge(target: &mut Value, layer: Value, file: &Path, local: &str) -> Result<(), Error> {
    match layer {
        Value::Tagged(tag) if tag.tag == "replace" => {
            let mut value = Value::Null;
            merge(&mut value, tag.value, file, local)?;
            *target = value;
        }
        Value::Tagged(_) => {
            return Err(invalid(
                file,
                local,
                "unsupported or misplaced composition operator",
            ));
        }
        Value::Mapping(mapping) => {
            if !target.is_mapping() {
                *target = Value::Mapping(Mapping::new());
            }
            let Some(destination) = target.as_mapping_mut() else {
                unreachable!()
            };
            for (key, value) in mapping {
                let Some(name) = key.as_str() else {
                    return Err(invalid(file, local, "mapping keys must be strings"));
                };
                let child = if local.is_empty() {
                    name.to_owned()
                } else {
                    format!("{local}.{name}")
                };
                if let Value::Tagged(tag) = &value
                    && tag.tag == "delete"
                {
                    if !tag.value.is_null() {
                        return Err(invalid(file, &child, "!delete must have no value"));
                    }
                    destination.remove(&key);
                    continue;
                }
                merge(
                    destination.entry(key).or_insert(Value::Null),
                    value,
                    file,
                    &child,
                )?;
            }
        }
        Value::Sequence(sequence) => {
            let mut normalized = Vec::with_capacity(sequence.len());
            for (index, value) in sequence.into_iter().enumerate() {
                if matches!(value, Value::Tagged(_)) {
                    return Err(invalid(
                        file,
                        local,
                        "composition operators cannot appear in sequences",
                    ));
                }
                let mut item = Value::Null;
                merge(&mut item, value, file, &format!("{local}[{index}]"))?;
                normalized.push(item);
            }
            *target = Value::Sequence(normalized);
        }
        value => *target = value,
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_and_deletion_discard_descendant_origins() {
        let mut origins = BTreeMap::new();
        let base: Value =
            serde_yaml::from_str("services: {motion: {bindings: {manual: absent.intent}}}")
                .unwrap();
        record_origins(&base, Path::new("base.yaml"), "", &mut origins);
        let replacement: Value =
            serde_yaml::from_str("services: {motion: !replace {source: {path: ../motion}}}")
                .unwrap();
        record_origins(&replacement, Path::new("replace.yaml"), "", &mut origins);
        assert!(!origins.contains_key("services.motion.bindings.manual"));
        assert_eq!(
            origins["services.motion.source.path"],
            Path::new("replace.yaml")
        );
        let deletion: Value = serde_yaml::from_str("services:\n  motion: !delete\n").unwrap();
        record_origins(&deletion, Path::new("delete.yaml"), "", &mut origins);
        assert!(!origins.contains_key("services.motion.source.path"));
        assert_eq!(origins["services.motion"], Path::new("delete.yaml"));
    }
    #[test]
    fn recursive_merge_and_explicit_operators() {
        let mut base: Value =
            serde_yaml::from_str("map: {a: 1, b: 2}\nlist: [1, 2]\nvalue: 2\n").unwrap();
        let layer = serde_yaml::from_str(
            "map: {a: !delete, c: 3}\nlist: []\nvalue: null\nmissing: !delete\n",
        )
        .unwrap();
        merge(&mut base, layer, Path::new("layer.yaml"), "").unwrap();
        assert_eq!(
            base,
            serde_yaml::from_str::<Value>("map: {b: 2, c: 3}\nlist: []\nvalue: null\n").unwrap()
        );
        merge(
            &mut base,
            serde_yaml::from_str("map: {}\n").unwrap(),
            Path::new("layer"),
            "",
        )
        .unwrap();
        assert_eq!(base["map"]["c"].as_i64(), Some(3));
        merge(
            &mut base,
            serde_yaml::from_str("map: !replace {}\n").unwrap(),
            Path::new("layer"),
            "",
        )
        .unwrap();
        assert_eq!(base["map"].as_mapping().unwrap().len(), 0);
        assert!(
            merge(
                &mut base,
                serde_yaml::from_str("list: [!delete null]\n").unwrap(),
                Path::new("layer"),
                ""
            )
            .is_err()
        );
    }
}
