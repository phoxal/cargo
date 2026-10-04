//! Native artifact contract inspection and connection validation.
//!
//! This module reads linker sections from ELF and Mach-O files through the
//! object-file parser. It never executes an inspected binary and it does not
//! parse Protobuf source into a second schema model.
//!
//! The inert record family (`ArtifactSummary`, `DescriptorSummary`,
//! `RuntimeRecord`, `InputRecord`, `OutputRecord`, `MethodShape`, `InputDelivery`,
//! `MethodSignature`) is owned by `phoxal::artifact` and
//! re-exported here so internal call sites continue to compile unchanged.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use object::{Object, ObjectSection};
use prost::Message;
use prost_reflect::DescriptorPool;
use sha2::{Digest, Sha256};

#[cfg(test)]
use crate::project::document::RobotDocument;

// Re-exports from the framework artifact module. The module is the source of
// truth; this module re-exports the inert record family so
// existing internal references continue to use `crate::project::artifact::*`.
pub use phoxal::artifact::{
    ArtifactSummary, DescriptorSummary, InputDelivery, MethodShape, OutputRecord, RUNTIME_RECORD,
    RuntimeRecord,
};
#[cfg(test)]
pub use phoxal::artifact::{InputRecord, MethodSignature};

const ARTIFACT_SECTION_NAMES: [&str; 2] = [".phoxal_art", "__phoxal_art"];
const DESCRIPTOR_SECTION_NAMES: [&str; 2] = [".phoxal_desc", "__phoxal_desc"];
const SCHEMA_SECTION_NAMES: [&str; 2] = [".phoxal_schema", "__phoxal_schema"];
const ARTIFACT_MAGIC: &[u8; 8] = b"PHXART0\n";
const DESCRIPTOR_MAGIC: &[u8; 8] = &phoxal::contracts::DESCRIPTOR_FRAME_MAGIC;
const MAX_SECTION_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 65_536;
const MAX_RECORDS: usize = 64;
const MAX_DESCRIPTOR_FILES: usize = 1_024;

/// A native artifact contract extracted without executing the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContract {
    /// Runtime timing, configuration, and binding metadata of the
    /// executable's primary runtime.
    pub runtime: RuntimeRecord,
    /// Original descriptor closures embedded by generated contract owners.
    pub descriptors: Vec<DescriptorInfo>,
    /// Schema records retained by Rust-authored messages in this artifact.
    pub schemas: Vec<phoxal::schema::DecodedRecord>,
}

impl ArtifactContract {
    /// Returns a manifest-safe summary while retaining no native bytes.
    #[must_use]
    pub fn summary(&self) -> ArtifactSummary {
        ArtifactSummary {
            runtime: self.runtime.clone(),
            descriptors: self
                .descriptors
                .iter()
                .map(DescriptorInfo::summary)
                .collect(),
        }
    }
}

/// One unchanged standard FileDescriptorSet retained by a contract owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescriptorInfo {
    /// SHA-256 digest of the original encoded descriptor bytes.
    pub sha256: String,
    /// Exact descriptor-set byte count.
    pub bytes: u64,
    /// File names retained in the descriptor closure.
    pub files: Vec<String>,
    raw: Vec<u8>,
}

impl DescriptorInfo {
    /// Returns the retained descriptor-set bytes.
    #[must_use]
    pub fn raw_bytes(&self) -> &[u8] {
        &self.raw
    }

    fn summary(&self) -> DescriptorSummary {
        DescriptorSummary {
            sha256: self.sha256.clone(),
            bytes: self.bytes,
            files: self.files.clone(),
        }
    }
}

/// Why native artifact contract inspection or graph validation failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The input is not a supported native object file.
    #[error("cannot parse native artifact: {0}")]
    Object(String),
    /// A native contract section is larger than the bounded inspector input.
    #[error("native artifact section {section} is too large ({bytes} bytes)")]
    SectionTooLarge {
        /// Section being inspected.
        section: &'static str,
        /// Observed size.
        bytes: usize,
    },
    /// The binary did not carry the required runtime artifact record.
    #[error("native artifact has no phoxal runtime contract record")]
    MissingRecord,
    /// A framed record was malformed.
    #[error("malformed {kind} artifact frame: {message}")]
    MalformedFrame {
        /// Frame family.
        kind: &'static str,
        /// Specific structural issue.
        message: String,
    },
    /// A JSON record could not be decoded.
    #[error("invalid runtime artifact JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// A retained descriptor set could not be decoded.
    #[error("invalid retained descriptor set: {0}")]
    Descriptor(#[from] prost_reflect::DescriptorError),
    /// A runtime record violated its own bounds or role checks.
    #[error("invalid runtime artifact contract: {0}")]
    InvalidContract(String),
    /// A connected endpoint could not be matched to a checked binding.
    #[error("invalid connected endpoint {consumer} <- {producer}: {message}")]
    InvalidConnection {
        /// Consumer endpoint.
        consumer: String,
        /// Producer endpoint.
        producer: String,
        /// Specific mismatch.
        message: String,
    },
}

/// Inspects one native binary without loading or executing it.
pub fn inspect_file(path: &Path) -> Result<ArtifactContract, Error> {
    let bytes = fs::read(path).map_err(|error| Error::Object(error.to_string()))?;
    inspect_bytes(&bytes)
}

/// Inspects native artifact sections from an in-memory ELF or Mach-O image.
pub fn inspect_bytes(bytes: &[u8]) -> Result<ArtifactContract, Error> {
    let file = object::File::parse(bytes).map_err(|error| Error::Object(error.to_string()))?;
    let mut artifact_sections = Vec::new();
    let mut descriptor_sections = Vec::new();
    let mut schema_sections = Vec::new();
    for section in file.sections() {
        let name = section.name().unwrap_or_default();
        if ARTIFACT_SECTION_NAMES.contains(&name) {
            artifact_sections.push(
                section
                    .data()
                    .map_err(|error| Error::Object(error.to_string()))?,
            );
        } else if DESCRIPTOR_SECTION_NAMES.contains(&name) {
            descriptor_sections.push(
                section
                    .data()
                    .map_err(|error| Error::Object(error.to_string()))?,
            );
        } else if SCHEMA_SECTION_NAMES.contains(&name) {
            schema_sections.push(
                section
                    .data()
                    .map_err(|error| Error::Object(error.to_string()))?,
            );
        }
    }
    let records = artifact_sections
        .iter()
        .try_fold(Vec::new(), |mut records, section| {
            if section.len() > MAX_SECTION_BYTES {
                return Err(Error::SectionTooLarge {
                    section: ".phoxal_art",
                    bytes: section.len(),
                });
            }
            records.extend(parse_artifact_records(section)?);
            Ok(records)
        })?;
    if records.len() != 1 {
        return Err(if records.is_empty() {
            Error::MissingRecord
        } else {
            Error::MalformedFrame {
                kind: "runtime",
                message: format!(
                    "expected exactly one primary record, found {}",
                    records.len()
                ),
            }
        });
    }
    let runtime: RuntimeRecord = serde_json::from_slice(records[0])?;
    validate_runtime(&runtime)?;
    let schemas = schema_sections
        .iter()
        .try_fold(Vec::new(), |mut schemas, section| {
            if section.len() > MAX_SECTION_BYTES {
                return Err(Error::SectionTooLarge {
                    section: ".phoxal_schema",
                    bytes: section.len(),
                });
            }
            schemas.extend(phoxal::schema::decode_section(section).map_err(|error| {
                Error::MalformedFrame {
                    kind: "schema",
                    message: error.to_string(),
                }
            })?);
            Ok(schemas)
        })?;
    // Rust-authored schema records assemble into one additional standard
    // descriptor closure for this artifact, exactly like a compiled
    // descriptor set would be retained.
    let assembled_descriptors = if schemas.is_empty() {
        Vec::new()
    } else {
        let set = phoxal::schema::assemble_file_descriptors(&schemas)
            .map_err(|error| Error::InvalidContract(error.to_string()))?;
        let raw = prost::Message::encode_to_vec(&set);
        let pool = DescriptorPool::decode(raw.as_slice())?;
        let files = pool
            .files()
            .map(|file| file.name().to_owned())
            .collect::<Vec<_>>();
        vec![DescriptorInfo {
            sha256: {
                let mut hasher = Sha256::new();
                hasher.update(&raw);
                format!("{:x}", hasher.finalize())
            },
            bytes: raw.len() as u64,
            files,
            raw,
        }]
    };
    let assembled_sections = assembled_descriptors_into_sections(assembled_descriptors);
    descriptor_sections.extend(assembled_sections.iter().map(|bytes| bytes.as_slice()));
    let descriptors =
        descriptor_sections
            .iter()
            .try_fold(Vec::new(), |mut descriptors, section| {
                if section.len() > MAX_SECTION_BYTES {
                    return Err(Error::SectionTooLarge {
                        section: ".phoxal_desc",
                        bytes: section.len(),
                    });
                }
                descriptors.extend(parse_descriptor_frames(section)?);
                Ok(descriptors)
            })?;
    let descriptors = descriptors
        .into_iter()
        .map(|raw| {
            let pool = DescriptorPool::decode(raw.as_slice())?;
            let files = pool
                .files()
                .map(|file| file.name().to_owned())
                .collect::<Vec<_>>();
            if files.len() > MAX_DESCRIPTOR_FILES {
                return Err(Error::InvalidContract(format!(
                    "descriptor closure contains {} files, limit is {MAX_DESCRIPTOR_FILES}",
                    files.len()
                )));
            }
            let mut hasher = Sha256::new();
            hasher.update(&raw);
            Ok(DescriptorInfo {
                sha256: format!("{:x}", hasher.finalize()),
                bytes: raw.len() as u64,
                files,
                raw,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(ArtifactContract {
        runtime,
        descriptors,
        schemas,
    })
}

/// Wraps assembled descriptor bytes as pseudo-sections for the shared
/// descriptor-frame fold.
fn assembled_descriptors_into_sections(infos: Vec<DescriptorInfo>) -> Vec<Vec<u8>> {
    infos
        .into_iter()
        .map(|info| {
            let mut section = phoxal::contracts::DESCRIPTOR_FRAME_MAGIC.to_vec();
            section.extend_from_slice(&(info.bytes.to_le_bytes()));
            section.extend_from_slice(&info.raw);
            section
        })
        .collect()
}

fn parse_artifact_records(section: &[u8]) -> Result<Vec<&[u8]>, Error> {
    let mut records = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = section[cursor..]
        .windows(ARTIFACT_MAGIC.len())
        .position(|window| window == ARTIFACT_MAGIC)
    {
        let magic = cursor + relative;
        let length_start = magic + ARTIFACT_MAGIC.len();
        let payload_start = length_start + 4;
        if payload_start > section.len() {
            return Err(Error::MalformedFrame {
                kind: "runtime",
                message: "truncated length header".to_owned(),
            });
        }
        let length = u32::from_le_bytes(section[length_start..payload_start].try_into().map_err(
            |_| Error::MalformedFrame {
                kind: "runtime",
                message: "invalid length header".to_owned(),
            },
        )?) as usize;
        if length > MAX_RECORD_BYTES {
            return Err(Error::MalformedFrame {
                kind: "runtime",
                message: format!("record length {length} exceeds {MAX_RECORD_BYTES}"),
            });
        }
        let end = payload_start
            .checked_add(length)
            .ok_or_else(|| Error::MalformedFrame {
                kind: "runtime",
                message: "record length overflow".to_owned(),
            })?;
        if end > section.len() {
            return Err(Error::MalformedFrame {
                kind: "runtime",
                message: "record extends beyond its section".to_owned(),
            });
        }
        records.push(&section[payload_start..end]);
        if records.len() > MAX_RECORDS {
            return Err(Error::MalformedFrame {
                kind: "runtime",
                message: format!("record count exceeds {MAX_RECORDS}"),
            });
        }
        cursor = end;
    }
    Ok(records)
}

fn parse_descriptor_frames(section: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut frames = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = section[cursor..]
        .windows(DESCRIPTOR_MAGIC.len())
        .position(|window| window == DESCRIPTOR_MAGIC)
    {
        let magic = cursor + relative;
        let length_start = magic + DESCRIPTOR_MAGIC.len();
        let payload_start = length_start + 8;
        if payload_start > section.len() {
            return Err(Error::MalformedFrame {
                kind: "descriptor",
                message: "truncated length header".to_owned(),
            });
        }
        let length = u64::from_le_bytes(section[length_start..payload_start].try_into().map_err(
            |_| Error::MalformedFrame {
                kind: "descriptor",
                message: "invalid length header".to_owned(),
            },
        )?) as usize;
        if length > MAX_SECTION_BYTES {
            return Err(Error::MalformedFrame {
                kind: "descriptor",
                message: format!("descriptor length {length} exceeds {MAX_SECTION_BYTES}"),
            });
        }
        let end = payload_start
            .checked_add(length)
            .ok_or_else(|| Error::MalformedFrame {
                kind: "descriptor",
                message: "descriptor length overflow".to_owned(),
            })?;
        if end > section.len() {
            return Err(Error::MalformedFrame {
                kind: "descriptor",
                message: "descriptor frame extends beyond its section".to_owned(),
            });
        }
        let payload = section[payload_start..end].to_vec();
        if !frames.iter().any(|existing| existing == &payload) {
            frames.push(payload);
        }
        cursor = end;
    }
    Ok(frames)
}

fn validate_runtime(runtime: &RuntimeRecord) -> Result<(), Error> {
    phoxal::artifact::bundle::validate_runtime_record(runtime, "compiled artifact")
        .map_err(Error::InvalidContract)?;
    let RuntimeRecord::V0 {
        record,
        period_ms,
        timeout_ms,
        init_timeout_ms,
        config_schema,
        inputs,
        outputs,
        ..
    } = runtime;
    if record != RUNTIME_RECORD {
        return Err(Error::InvalidContract(format!(
            "record '{record}' is not {RUNTIME_RECORD}"
        )));
    }
    if *period_ms == 0 || *timeout_ms == 0 || *init_timeout_ms == 0 {
        return Err(Error::InvalidContract(
            "period and deadlines must be positive".to_owned(),
        ));
    }
    if !config_schema.is_object() {
        return Err(Error::InvalidContract(
            "config_schema must be a JSON schema object".to_owned(),
        ));
    }
    let mut input_names = BTreeSet::new();
    for input in inputs {
        if !input_names.insert(input.name.as_str()) {
            return Err(Error::InvalidContract(format!(
                "duplicate input binding '{}'",
                input.name
            )));
        }
        let requires_request = matches!(
            input.delivery,
            InputDelivery::CallResult | InputDelivery::CallTarget | InputDelivery::CallIngress
        ) || (input.delivery == InputDelivery::LeasedValue
            && input.signature.is_some());
        let requires_response = !matches!(input.delivery, InputDelivery::CallCompletions);
        if input.request_fqn.is_some() != requires_request
            || input.response_fqn.is_some() != requires_response
            || input
                .request_fqn
                .iter()
                .chain(input.response_fqn.iter())
                .any(|name| name.is_empty() || !name.is_ascii())
        {
            return Err(Error::InvalidContract(format!(
                "input '{}' must retain its concrete generated Protobuf message identities",
                input.name
            )));
        }
        if input.port.is_some() != input.signature.is_some() {
            return Err(Error::InvalidContract(format!(
                "input '{}' must retain port and signature together",
                input.name
            )));
        }
        if let Some(signature) = &input.signature {
            if input.port.as_deref() != Some(signature.endpoint.as_str()) {
                return Err(Error::InvalidContract(format!(
                    "input '{}' port name does not match its signature",
                    input.name
                )));
            }
            if input.delivery == InputDelivery::CallIngress && signature.shape != MethodShape::Call
            {
                return Err(Error::InvalidContract(format!(
                    "call ingress '{}' is bound to {:?}",
                    input.name, signature.shape
                )));
            }
            if input.delivery == InputDelivery::LeasedValue
                && signature.lease_valid_for_ms.is_none()
            {
                return Err(Error::InvalidContract(format!(
                    "leased input '{}' is bound to a method without a lease",
                    input.name
                )));
            }
        }
    }
    let mut output_names = BTreeSet::new();
    for output in outputs.iter() {
        if !output_names.insert(output.name.as_str()) {
            return Err(Error::InvalidContract(format!(
                "duplicate output binding '{}'",
                output.name
            )));
        }
        if output.port.is_some() != output.signature.is_some() {
            return Err(Error::InvalidContract(format!(
                "output '{}' must retain port and signature together",
                output.name
            )));
        }
        if output.port.is_some() && output.max_bytes.is_none_or(|bound| bound == 0) {
            return Err(Error::InvalidContract(format!(
                "served output '{}' has no positive response/publication byte bound",
                output.name
            )));
        }
        if output
            .signature
            .as_ref()
            .is_some_and(|signature| signature.shape == MethodShape::Call)
            && output.max_request_bytes.is_none_or(|bound| bound == 0)
        {
            return Err(Error::InvalidContract(format!(
                "read output '{}' has no positive request byte bound",
                output.name
            )));
        }
        if let Some(signature) = &output.signature
            && output.port.as_deref() != Some(signature.endpoint.as_str())
        {
            return Err(Error::InvalidContract(format!(
                "output '{}' port name does not match its signature",
                output.name
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
pub use connections::validate_connected_endpoints;
/// Validates all graph connections for which both endpoint artifacts exist.
///
/// The validation is intentionally endpoint-first: kind and complete request /
/// response identities are compared before any descriptor message-root
/// filtering can remove service evidence.
pub use connections::validate_connected_endpoints_with_virtual_producers;

/// Rejects conflicting imported definitions across the executable contract
/// closures admitted into one bundle.
///
/// Source locations and comments are excluded from the definition identity.
/// Qualified symbols must have one definition across consumer and provider
/// packages even when packages reuse an import filename independently.
pub fn validate_descriptor_closure_consistency<'a>(
    contracts: impl IntoIterator<Item = (&'a str, &'a ArtifactContract)>,
) -> Result<(), Error> {
    let mut symbols = BTreeMap::<String, (String, Vec<u8>)>::new();
    for (instance, contract) in contracts {
        for descriptor in &contract.descriptors {
            let pool = DescriptorPool::decode(descriptor.raw.as_slice())?;
            let definitions = pool
                .all_messages()
                .map(|item| {
                    (
                        item.full_name().to_owned(),
                        item.descriptor_proto().encode_to_vec(),
                    )
                })
                .chain(pool.all_enums().map(|item| {
                    (
                        item.full_name().to_owned(),
                        item.enum_descriptor_proto().encode_to_vec(),
                    )
                }))
                .chain(pool.services().map(|item| {
                    (
                        item.full_name().to_owned(),
                        item.service_descriptor_proto().encode_to_vec(),
                    )
                }))
                .chain(pool.all_extensions().map(|item| {
                    (
                        item.full_name().to_owned(),
                        item.field_descriptor_proto().encode_to_vec(),
                    )
                }));
            for (name, encoded) in definitions {
                if let Some((owner, accepted)) = symbols.get(&name) {
                    if accepted != &encoded {
                        return Err(Error::InvalidContract(format!(
                            "qualified Protobuf definition `{name}` differs between `{owner}` and `{instance}`"
                        )));
                    }
                } else {
                    symbols.insert(name, (instance.to_owned(), encoded));
                }
            }
        }
    }
    Ok(())
}

mod connections;

#[cfg(test)]
mod tests {
    use object::write::Object;
    use object::{Architecture, BinaryFormat, Endianness, SectionKind};
    use prost_types::{
        DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
        field_descriptor_proto,
    };

    use super::*;

    fn frame(json: &str) -> Vec<u8> {
        let mut bytes = ARTIFACT_MAGIC.to_vec();
        bytes.extend_from_slice(&(json.len() as u32).to_le_bytes());
        bytes.extend_from_slice(json.as_bytes());
        bytes
    }

    fn native_artifact(json: &str, section_name: &[u8]) -> Vec<u8> {
        let mut object = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let section =
            object.add_section(Vec::new(), section_name.to_vec(), SectionKind::ReadOnlyData);
        object.append_section_data(section, &frame(json), 1);
        object.write().expect("synthetic object")
    }

    fn descriptor(file: &str, field_type: field_descriptor_proto::Type) -> DescriptorInfo {
        let raw = FileDescriptorSet {
            file: vec![FileDescriptorProto {
                name: Some(file.to_owned()),
                package: Some("example.shared.v1".to_owned()),
                syntax: Some("proto3".to_owned()),
                message_type: vec![DescriptorProto {
                    name: Some("Shared".to_owned()),
                    field: vec![FieldDescriptorProto {
                        name: Some("value".to_owned()),
                        number: Some(1),
                        label: Some(prost_types::field_descriptor_proto::Label::Optional.into()),
                        r#type: Some(field_type.into()),
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        }
        .encode_to_vec();
        let mut hasher = Sha256::new();
        hasher.update(&raw);
        DescriptorInfo {
            sha256: format!("{:x}", hasher.finalize()),
            bytes: raw.len() as u64,
            files: vec![file.to_owned()],
            raw,
        }
    }

    const EMPTY_RUNTIME: &str = r#"{
        "schema":"phoxal/artifact/v0",
        "record":"runtime",
        "period_ms":20,
        "timeout_ms":100,
        "init_timeout_ms":1000,
        "config_schema":{"type":"object"},
        "inputs":[],
        "outputs":[]
    }"#;

    #[test]
    fn extracts_a_record_from_an_elf_section_without_execution() {
        let bytes = native_artifact(EMPTY_RUNTIME, b".phoxal_art");
        let contract = inspect_bytes(&bytes).expect("native artifact contract");
        let RuntimeRecord::V0 { period_ms, .. } = &contract.runtime;
        assert_eq!(*period_ms, 20);
        assert!(contract.descriptors.is_empty());
    }

    fn multi_record_artifact(jsons: &[&str]) -> Vec<u8> {
        let mut object = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
        let section = object.add_section(
            Vec::new(),
            b".phoxal_art".to_vec(),
            SectionKind::ReadOnlyData,
        );
        for json in jsons {
            object.append_section_data(section, &frame(json), 1);
        }
        object.write().expect("synthetic object")
    }

    #[test]
    fn two_primary_records_are_rejected() {
        let bytes = multi_record_artifact(&[EMPTY_RUNTIME, EMPTY_RUNTIME]);
        let error = inspect_bytes(&bytes).expect_err("a binary has exactly one primary record");
        assert!(
            error.to_string().contains("exactly one primary record"),
            "{error}"
        );
    }

    #[test]
    fn reads_the_descriptor_frame_emitted_by_the_sdk() {
        let descriptor = descriptor("shared.proto", field_descriptor_proto::Type::String);
        let mut section = phoxal::contracts::DESCRIPTOR_FRAME_MAGIC.to_vec();
        section.extend_from_slice(&(descriptor.raw.len() as u64).to_le_bytes());
        section.extend_from_slice(&descriptor.raw);
        let parsed = parse_descriptor_frames(&section).expect("SDK descriptor frame parses");
        assert_eq!(parsed, vec![descriptor.raw]);
    }

    #[test]
    fn rejects_an_oversized_record_before_json_parsing() {
        let mut section = ARTIFACT_MAGIC.to_vec();
        section.extend_from_slice(&((MAX_RECORD_BYTES + 1) as u32).to_le_bytes());
        section.resize(section.len() + MAX_RECORD_BYTES + 1, b'x');
        let error = parse_artifact_records(&section).expect_err("bounded parser must reject");
        assert!(matches!(
            error,
            Error::MalformedFrame {
                kind: "runtime",
                ..
            }
        ));
    }

    #[test]
    fn rejects_unknown_public_method_shapes_in_artifact_json() {
        let mut record: serde_json::Value = serde_json::from_str(EMPTY_RUNTIME).unwrap();
        record["outputs"] = serde_json::json!([{
            "name": "status", "port": "status", "bootstrap": false,
            "signature": { "endpoint": "status", "service": "fixture.Service", "method": "Status",
                "shape": "future", "request": "google.protobuf.Empty", "response": "fixture.Status",
                "retained_latest": true, "lease_valid_for_ms": null }
        }]);
        let bytes = native_artifact(&record.to_string(), b".phoxal_art");
        assert!(matches!(inspect_bytes(&bytes), Err(Error::Json(_))));
    }

    #[test]
    fn rejects_call_observation_incompatibility_before_payload_filtering() {
        let document: RobotDocument = serde_yaml::from_str(
            r#"
schema: phoxal/robot/v0
robot:
  id: rover
  components: {}
supervisor:
  source: { path: supervisor }
services:
  consumer:
    source: { path: ../consumer }
  producer:
    source: { path: ../producer }
connections:
  consumer.input: producer.output
"#,
        )
        .expect("document parses");
        let signature = MethodSignature {
            endpoint: "output".to_owned(),
            service: "example.Service".to_owned(),
            method: "Output".to_owned(),
            shape: MethodShape::Observation,
            request: "google.protobuf.Empty".to_owned(),
            response: "example.Payload".to_owned(),
            retained_latest: false,
            lease_valid_for_ms: None,
        };
        let producer = ArtifactContract {
            runtime: RuntimeRecord::V0 {
                record: RUNTIME_RECORD.to_owned(),
                conversions: Vec::new(),
                period_ms: 1,
                timeout_ms: 1,
                init_timeout_ms: 1,
                config_schema: serde_json::json!({"type":"object"}),
                inputs: Vec::new(),
                outputs: vec![OutputRecord {
                    name: "output".to_owned(),
                    port: Some("output".to_owned()),
                    signature: Some(signature),
                    max_items: None,
                    max_bytes: None,
                    max_request_bytes: None,
                    every_steps: None,
                    bootstrap: false,
                    timeout_ms: None,
                }],
            },
            descriptors: Vec::new(),
            schemas: Vec::new(),
        };
        let consumer = ArtifactContract {
            runtime: RuntimeRecord::V0 {
                record: RUNTIME_RECORD.to_owned(),
                conversions: Vec::new(),
                period_ms: 1,
                timeout_ms: 1,
                init_timeout_ms: 1,
                config_schema: serde_json::json!({"type":"object"}),
                inputs: vec![InputRecord {
                    name: "input".to_owned(),
                    delivery: InputDelivery::CallResult,
                    max_age_ms: None,
                    max_items: None,
                    max_bytes: None,
                    port: None,
                    signature: None,
                    request_fqn: Some("google.protobuf.Empty".to_owned()),
                    response_fqn: Some("example.Payload".to_owned()),
                    response_max_bytes: None,
                    response_max_items: None,
                }],
                outputs: Vec::new(),
            },
            descriptors: Vec::new(),
            schemas: Vec::new(),
        };
        let contracts = BTreeMap::from([
            ("consumer".to_owned(), consumer),
            ("producer".to_owned(), producer),
        ]);
        let error = validate_connected_endpoints(&document, &contracts)
            .expect_err("an observation cannot satisfy a call input");
        assert!(error.to_string().contains("requires Some(Call)"));
    }

    #[test]
    fn rejects_incompatible_imported_definitions_across_bundle_artifacts() {
        let contract = |descriptor| ArtifactContract {
            runtime: serde_json::from_str(EMPTY_RUNTIME).expect("runtime"),
            descriptors: vec![descriptor],
            schemas: Vec::new(),
        };
        let consumer = contract(descriptor(
            "example/shared/v1/shared.proto",
            field_descriptor_proto::Type::Uint64,
        ));
        let provider = contract(descriptor(
            "example/shared/v1/shared.proto",
            field_descriptor_proto::Type::String,
        ));
        let error = validate_descriptor_closure_consistency([
            ("consumer", &consumer),
            ("provider", &provider),
        ])
        .expect_err("different imported definitions must be rejected");
        assert!(error.to_string().contains("differs between"));
    }

    #[test]
    fn accepts_identical_imported_definitions_across_bundle_artifacts() {
        let contract = |descriptor| ArtifactContract {
            runtime: serde_json::from_str(EMPTY_RUNTIME).expect("runtime"),
            descriptors: vec![descriptor],
            schemas: Vec::new(),
        };
        let consumer = contract(descriptor(
            "example/shared/v1/shared.proto",
            field_descriptor_proto::Type::Uint64,
        ));
        let provider = contract(descriptor(
            "example/shared/v1/shared.proto",
            field_descriptor_proto::Type::Uint64,
        ));
        validate_descriptor_closure_consistency([("consumer", &consumer), ("provider", &provider)])
            .expect("identical imported definitions are admitted");
    }
}
