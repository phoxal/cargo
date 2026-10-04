//! Consume the actual conversion endpoints retained by the compiled brain.
use super::Error;
use phoxal::artifact::RuntimeRecord;
use phoxal::artifact::document::{ConnectionSources, RobotDocument};

pub(crate) fn lower(
    authored: &RobotDocument,
    compiled: &RuntimeRecord,
) -> Result<RobotDocument, Error> {
    let RuntimeRecord::V0 { conversions, .. } = compiled;
    let mut executable = authored.clone();
    let RobotDocument::V0 { connections, .. } = &mut executable;
    let mut consumers = std::collections::BTreeSet::new();
    for route in conversions {
        if !consumers.insert(route.consumer.as_str())
            || connections.get(&route.consumer)
                != Some(&ConnectionSources::One(route.producer.clone()))
        {
            return Err(Error::DeclarationCheck {
                message: format!(
                    "compiled conversion for {} does not match robot.yaml",
                    route.consumer
                ),
            });
        }
        connections.insert(
            format!("brain.{}", route.input_endpoint),
            ConnectionSources::One(route.producer.clone()),
        );
        connections.insert(
            route.consumer.clone(),
            ConnectionSources::One(format!("brain.{}", route.output_endpoint)),
        );
    }
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_uses_compiled_endpoints_and_refuses_stale_authored_edges() {
        let authored: RobotDocument = serde_json::from_value(serde_json::json!({
            "schema": "phoxal/robot/v0", "robot": { "id": "proof" },
            "supervisor": { "source": { "path": "supervisor" } },
            "connections": { "receiver.capture": "source.status" }
        }))
        .unwrap();
        let compiled: RuntimeRecord = serde_json::from_value(serde_json::json!({
            "schema": "phoxal/artifact/v0", "record": "runtime", "period_ms": 20,
            "timeout_ms": 100, "init_timeout_ms": 1000, "config_schema": null,
            "inputs": [], "outputs": [], "conversions": [{
                "consumer": "receiver.capture", "producer": "source.status",
                "input_endpoint": "actual_generated_in_47", "output_endpoint": "actual_generated_out_23"
            }]
        })).unwrap();
        let executable = lower(&authored, &compiled).unwrap();
        let RobotDocument::V0 { connections, .. } = executable;
        assert_eq!(
            connections["brain.actual_generated_in_47"],
            ConnectionSources::One("source.status".into())
        );
        assert_eq!(
            connections["receiver.capture"],
            ConnectionSources::One("brain.actual_generated_out_23".into())
        );
        assert!(!connections.contains_key("brain.phoxal_conversion_in_0"));
        let mut changed = authored;
        let RobotDocument::V0 { connections, .. } = &mut changed;
        connections.insert(
            "receiver.capture".into(),
            ConnectionSources::One("other.status".into()),
        );
        assert!(
            lower(&changed, &compiled)
                .unwrap_err()
                .to_string()
                .contains("does not match robot.yaml")
        );
    }
}
