//! Admission of authored connections against executable contracts.

use super::{ArtifactContract, Error, InputDelivery, MethodShape};
use crate::project::document::{PortReference, RobotDocument};
use phoxal::artifact::RuntimeRecord;
use std::collections::BTreeMap;

#[cfg(test)]
pub fn validate_connected_endpoints(
    document: &RobotDocument,
    contracts: &BTreeMap<String, ArtifactContract>,
) -> Result<(), Error> {
    validate_connected_endpoints_with_virtual_producers(document, contracts, &[])
}

pub fn validate_connected_endpoints_with_virtual_producers(
    document: &RobotDocument,
    contracts: &BTreeMap<String, ArtifactContract>,
    virtual_producers: &[&str],
) -> Result<(), Error> {
    let RobotDocument::V0 {
        robot, connections, ..
    } = document;
    for (instance, contract) in contracts {
        let RuntimeRecord::V0 { inputs, .. } = &contract.runtime;
        for input in inputs {
            if matches!(
                input.delivery,
                InputDelivery::CallIngress | InputDelivery::CallCompletions
            ) {
                continue;
            }
            let consumer = format!("{instance}.{}", input.name);
            if !connections.contains_key(&consumer) {
                return Err(Error::InvalidConnection {
                    consumer,
                    producer: String::new(),
                    message: "required runtime input has no authored connection".into(),
                });
            }
        }
    }
    for (consumer_text, sources) in connections {
        let consumer =
            PortReference::parse(consumer_text).map_err(|error| Error::InvalidConnection {
                consumer: consumer_text.clone(),
                producer: String::new(),
                message: error.to_string(),
            })?;
        let consumer_contract =
            contracts
                .get(&consumer.instance)
                .ok_or_else(|| Error::InvalidConnection {
                    consumer: consumer_text.clone(),
                    producer: String::new(),
                    message: "consumer has no executable runtime artifact".into(),
                })?;
        let RuntimeRecord::V0 {
            inputs: consumer_inputs,
            ..
        } = &consumer_contract.runtime;
        let input = consumer_inputs
            .iter()
            .find(|input| input.name == consumer.port)
            .ok_or_else(|| Error::InvalidConnection {
                consumer: consumer_text.clone(),
                producer: String::new(),
                message: "consumer input is absent from the runtime artifact".to_owned(),
            })?;
        if sources.as_slice().is_empty()
            || (matches!(
                input.delivery,
                InputDelivery::ObservationLatest
                    | InputDelivery::LeasedValue
                    | InputDelivery::CallResult
                    | InputDelivery::CallTarget
                    | InputDelivery::CallCompletions
            ) && sources.as_slice().len() != 1)
        {
            return Err(Error::InvalidConnection {
                consumer: consumer_text.clone(),
                producer: String::new(),
                message: "input requires exactly one publisher or request target".into(),
            });
        }
        for producer_text in sources.as_slice() {
            let producer =
                PortReference::parse(producer_text).map_err(|error| Error::InvalidConnection {
                    consumer: consumer_text.clone(),
                    producer: producer_text.clone(),
                    message: error.to_string(),
                })?;
            let Some(producer_contract) = contracts.get(&producer.instance) else {
                if virtual_producers.contains(&producer.instance.as_str()) {
                    continue;
                }
                if robot.components.contains_key(&producer.instance) {
                    // Simulation providers are admitted from the selected
                    // component contracts during native bundle preparation.
                    continue;
                }
                return Err(Error::InvalidConnection {
                    consumer: consumer_text.clone(),
                    producer: producer_text.clone(),
                    message: "producer has no executable runtime artifact".into(),
                });
            };
            let RuntimeRecord::V0 {
                inputs: producer_inputs,
                outputs,
                ..
            } = &producer_contract.runtime;
            let signature = if matches!(
                input.delivery,
                InputDelivery::CallTarget | InputDelivery::CallCompletions
            ) {
                producer_inputs
                    .iter()
                    .find(|input| {
                        input.delivery == InputDelivery::CallIngress
                            && input.port.as_deref() == Some(producer.port.as_str())
                    })
                    .and_then(|input| input.signature.as_ref())
            } else {
                outputs
                    .iter()
                    .find(|output| output.port.as_deref() == Some(producer.port.as_str()))
                    .and_then(|output| output.signature.as_ref())
            }
            .ok_or_else(|| Error::InvalidConnection {
                consumer: consumer_text.clone(),
                producer: producer_text.clone(),
                message:
                    "producer port is absent or has no complete signature in its runtime artifact"
                        .into(),
            })?;
            let expected_shape = input_method_shape(input.delivery);
            if expected_shape.is_some() && expected_shape != Some(signature.shape) {
                return Err(Error::InvalidConnection {
                    consumer: consumer_text.clone(),
                    producer: producer_text.clone(),
                    message: format!(
                        "consumer {:?} requires {:?}, producer supplies {:?}",
                        input.delivery, expected_shape, signature.shape
                    ),
                });
            }
            let payload_matches = if input.delivery == InputDelivery::LeasedValue {
                // A leased setpoint flows producer -> consumer; its payload
                // rides the request side of a call-shaped signature and the
                // response side of an observation-shaped one.
                let consumer_payload =
                    [input.request_fqn.as_deref(), input.response_fqn.as_deref()]
                        .into_iter()
                        .flatten()
                        .find(|fqn| *fqn != "google.protobuf.Empty");
                let producer_payload = if signature.shape == MethodShape::Call {
                    &signature.request
                } else {
                    &signature.response
                };
                consumer_payload == Some(producer_payload.as_str())
            } else {
                input
                    .request_fqn
                    .as_ref()
                    .is_none_or(|request| request == &signature.request)
                    && input
                        .response_fqn
                        .as_ref()
                        .is_none_or(|response| response == &signature.response)
            };
            if !payload_matches {
                return Err(Error::InvalidConnection { consumer: consumer_text.clone(), producer: producer_text.clone(),
                    message: "generated Protobuf request or response type differs from the consumer input".into() });
            }
            if let Some(input_signature) = &input.signature {
                // A required operation compares its declared contract identity
                // and exchange messages; the enclosing services and the local
                // endpoint spellings may differ on either side of the binding.
                // A leased setpoint compares its payload identity and lease
                // interval across the shape difference.  Every other role
                // keeps the full-signature comparison.
                let compatible = if input.delivery == InputDelivery::CallCompletions {
                    input_signature.service == signature.service
                        && input_signature.request == signature.request
                        && input_signature.response == signature.response
                        && input_signature.shape == signature.shape
                } else if input.delivery == InputDelivery::LeasedValue {
                    input_signature.lease_valid_for_ms == signature.lease_valid_for_ms
                        && [
                            input_signature.request.as_str(),
                            input_signature.response.as_str(),
                        ]
                        .contains(&if signature.shape == MethodShape::Call {
                            signature.request.as_str()
                        } else {
                            signature.response.as_str()
                        })
                } else {
                    *input_signature == *signature
                };
                if !compatible {
                    return Err(Error::InvalidConnection {
                        consumer: consumer_text.clone(),
                        producer: producer_text.clone(),
                        message: "request, response, service, or method identity differs"
                            .to_owned(),
                    });
                }
            }
        }
    }
    Ok(())
}

fn input_method_shape(role: InputDelivery) -> Option<MethodShape> {
    match role {
        InputDelivery::ObservationLatest | InputDelivery::ObservationHistory => {
            Some(MethodShape::Observation)
        }
        InputDelivery::CallIngress
        | InputDelivery::CallTarget
        | InputDelivery::CallResult
        | InputDelivery::CallCompletions => Some(MethodShape::Call),
        InputDelivery::LeasedValue => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn contract(inputs: serde_json::Value, outputs: serde_json::Value) -> ArtifactContract {
        ArtifactContract {
            runtime: serde_json::from_value(json!({
                "schema": "phoxal/artifact/v0", "record": "runtime", "period_ms": 20,
                "timeout_ms": 100, "init_timeout_ms": 1000, "config_schema": {},
                "inputs": inputs, "outputs": outputs,
            }))
            .unwrap(),
            descriptors: Vec::new(),
            schemas: Vec::new(),
        }
    }
    fn document(connections: serde_json::Value) -> RobotDocument {
        serde_json::from_value(json!({
            "schema": "phoxal/robot/v0",
            "robot": {"id": "test"},
            "supervisor": {"source": {"path": "supervisor"}},
            "services": {},
            "connections": connections,
        }))
        .unwrap()
    }

    #[test]
    fn missing_required_connection_is_rejected_even_when_the_graph_is_empty() {
        let contracts = BTreeMap::from([(
            "motion".into(),
            contract(
                json!([{"name":"safety", "delivery":"observation_latest"}]),
                json!([]),
            ),
        )]);
        assert!(validate_connected_endpoints(&document(json!({})), &contracts).is_err());
    }

    #[test]
    fn request_targets_the_server_command_input() {
        let signature = json!({"endpoint":"commands", "service":"fixture.Service", "method":"Commands", "shape":"call", "request":"fixture.Request", "response":"fixture.Response", "retained_latest":false, "lease_valid_for_ms":null});
        let contracts = BTreeMap::from([
            (
                "client".into(),
                contract(
                    json!([{"name":"request", "delivery":"call_target"}]),
                    json!([]),
                ),
            ),
            (
                "server".into(),
                contract(
                    json!([{"name":"incoming", "delivery":"call_ingress", "port":"commands", "signature":signature}]),
                    json!([]),
                ),
            ),
        ]);
        validate_connected_endpoints(
            &document(json!({"client.request":"server.commands"})),
            &contracts,
        )
        .unwrap();
    }

    #[test]
    fn latest_rejects_multiple_publishers_instead_of_competing_for_one_slot() {
        let contracts = BTreeMap::from([(
            "consumer".into(),
            contract(
                json!([{"name":"state", "delivery":"observation_latest"}]),
                json!([]),
            ),
        )]);
        assert!(
            validate_connected_endpoints(
                &document(json!({"consumer.state":["first.state","second.state"]})),
                &contracts
            )
            .is_err()
        );
    }
    #[test]
    fn generated_request_and_response_identity_are_checked_before_launch() {
        let signature = json!({"endpoint":"commands", "service":"fixture.Service", "method":"Commands", "shape":"call", "request":"fixture.Request", "response":"fixture.Response", "retained_latest":false, "lease_valid_for_ms":null});
        for (request, response) in [
            ("fixture.Other", "fixture.Response"),
            ("fixture.Request", "fixture.Other"),
        ] {
            let contracts = BTreeMap::from([
                (
                    "client".into(),
                    contract(
                        json!([{"name":"request", "delivery":"call_target", "request_fqn": request, "response_fqn": response}]),
                        json!([]),
                    ),
                ),
                (
                    "server".into(),
                    contract(
                        json!([{"name":"incoming", "delivery":"call_ingress", "port":"commands", "signature":signature}]),
                        json!([]),
                    ),
                ),
            ]);
            let error = validate_connected_endpoints(
                &document(json!({"client.request":"server.commands"})),
                &contracts,
            )
            .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Protobuf request or response type differs")
            );
        }
    }
}
