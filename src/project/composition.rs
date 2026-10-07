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
        resolve_sources(&mut resolved, &origins, &inputs[0])?;
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
                            | super::error::ValidationError::InvalidSource { field, .. }
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
                            _ => "robot.services",
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

/// Authored indirection is tool-owned and never enters prepared/runtime records.
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AuthoredSource {
    Concrete(super::document::Source),
    Reference(SourceReference),
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceReference {
    r#ref: String,
}

fn resolve_sources(
    document: &mut Value,
    origins: &BTreeMap<String, PathBuf>,
    fallback: &Path,
) -> Result<(), Error> {
    let origin = |local: &str| {
        origins
            .iter()
            .filter(|(key, _)| local == key.as_str() || local.starts_with(&format!("{key}.")))
            .max_by_key(|(key, _)| key.len())
            .map_or(fallback, |(_, file)| file.as_path())
    };
    let Some(root) = document.as_mapping_mut() else {
        return Ok(());
    };
    let definitions = root
        .remove(Value::String("sources".into()))
        .unwrap_or_else(|| Value::Mapping(Mapping::new()));
    let definitions = definitions.as_mapping().ok_or_else(|| {
        invalid(
            origin("sources"),
            "sources",
            "expected a named mapping of concrete sources",
        )
    })?;
    let mut sources = BTreeMap::new();
    for (name, value) in definitions {
        let name = name
            .as_str()
            .filter(|name| super::document::is_identifier(name))
            .ok_or_else(|| {
                invalid(
                    origin("sources"),
                    "sources",
                    "source names must be project identifiers",
                )
            })?;
        let local = format!("sources.{name}");
        let source: super::document::Source =
            serde_yaml::from_value(value.clone()).map_err(|error| {
                invalid(
                    origin(&local),
                    &local,
                    &format!("expected a concrete path or Git selection, not a reference: {error}"),
                )
            })?;
        let mut errors = Vec::new();
        super::document::validate_source(&local, &source, &mut errors);
        if !errors.is_empty() {
            return Err(invalid(
                origin(&local),
                &local,
                &errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        sources.insert(name.to_owned(), source);
    }
    let resolve = |value: &mut Value, local: &str| -> Result<(), Error> {
        let source: AuthoredSource = serde_yaml::from_value(value.clone()).map_err(|error| {
            invalid(
                origin(local),
                local,
                &format!("expected a concrete selection or {{ref: name}}: {error}"),
            )
        })?;
        let source = match source {
            AuthoredSource::Concrete(source) => source,
            AuthoredSource::Reference(reference) => {
                sources.get(&reference.r#ref).cloned().ok_or_else(|| {
                    invalid(
                        origin(local),
                        local,
                        &format!("unknown source reference '{}'", reference.r#ref),
                    )
                })?
            }
        };
        *value = serde_yaml::to_value(source)
            .map_err(|error| invalid(origin(local), local, &error.to_string()))?;
        Ok(())
    };
    if let Some(source) = root
        .get_mut(Value::String("supervisor".into()))
        .and_then(Value::as_mapping_mut)
        .and_then(|map| map.get_mut(Value::String("source".into())))
    {
        resolve(source, "supervisor.source")?;
    }
    if let Some(robot) = root
        .get_mut(Value::String("robot".into()))
        .and_then(Value::as_mapping_mut)
    {
        for group in ["services", "components"] {
            if let Some(instances) = robot
                .get_mut(Value::String(group.into()))
                .and_then(Value::as_mapping_mut)
            {
                for (name, instance) in instances {
                    if let (Some(name), Some(source)) = (
                        name.as_str(),
                        instance
                            .as_mapping_mut()
                            .and_then(|map| map.get_mut(Value::String("source".into()))),
                    ) {
                        resolve(source, &format!("robot.{group}.{name}.source"))?;
                    }
                }
            }
        }
    }
    Ok(())
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
    fn references_resolve_after_ordered_composition_and_leave_no_alias_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let base = root.as_path().join("robot.yaml");
        let layer = root.as_path().join("later.yaml");
        std::fs::write(&base, "schema: phoxal/robot/v0\nsources:\n  motor: {path: ../old}\n  app: {path: ../supervisor}\nrobot:\n  id: rover\n  brain: {}\n  services:\n    motion: {source: {ref: motor}}\n  components:\n    wheel: {source: {ref: motor}, mount_site: Front.Left}\nsupervisor: {source: {ref: app}}\n").unwrap();
        std::fs::write(
            &layer,
            "schema: phoxal/robot/v0\nsources:\n  motor: !replace {path: ../new}\n",
        )
        .unwrap();
        let composition =
            Composition::load(root.as_path(), &[base.clone(), layer.clone()]).unwrap();
        let value = serde_json::to_value(&composition.document).unwrap();
        assert!(value.get("sources").is_none());
        assert_eq!(
            value["robot"]["services"]["motion"]["source"],
            serde_json::json!({"path":"../new"})
        );
        assert_eq!(
            value["robot"]["components"]["wheel"]["source"],
            value["robot"]["services"]["motion"]["source"]
        );
        assert_eq!(
            value["supervisor"]["source"],
            serde_json::json!({"path":"../supervisor"})
        );
        let inline = std::fs::read_to_string(&base)
            .unwrap()
            .replace("{ref: motor}", "{path: ../new}")
            .replace("{ref: app}", "{path: ../supervisor}");
        std::fs::write(root.as_path().join("inline.yaml"), inline).unwrap();
        assert_eq!(
            Composition::load(root.as_path(), &[root.as_path().join("inline.yaml")])
                .unwrap()
                .document,
            composition.document
        );
        for declaration in [
            "motor: !delete",
            "motor: {ref: app}",
            "motor: !replace {ref: app}",
            "motor: {path: ../new, git: {name: bad}}",
            "motor: {path: /absolute}",
        ] {
            std::fs::write(
                &layer,
                format!("schema: phoxal/robot/v0\nsources:\n  {declaration}\n"),
            )
            .unwrap();
            let error = Composition::load(root.as_path(), &[base.clone(), layer.clone()])
                .unwrap_err()
                .to_string();
            assert!(error.contains("source"), "{error}");
            if declaration == "motor: !replace {ref: app}" {
                assert!(error.contains("later.yaml:sources.motor"), "{error}");
                assert!(error.contains("not a reference"), "{error}");
            }
        }
        std::fs::write(&layer, "schema: phoxal/robot/v0\nrobot:\n  services:\n    motion:\n      source: !replace {ref: absent}\n").unwrap();
        let error = Composition::load(root.as_path(), &[base, layer])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("later.yaml:robot.services.motion.source") && error.contains("absent"),
            "{error}"
        );
    }
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
