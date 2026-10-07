//! Consume the actual conversion endpoints retained by the compiled brain.
use super::Error;
use phoxal::artifact::RuntimeRecord;
use phoxal::artifact::document::RobotDocument;

pub(crate) fn lower(
    authored: &RobotDocument,
    compiled: &RuntimeRecord,
) -> Result<RobotDocument, Error> {
    let RuntimeRecord::V0 { conversions, .. } = compiled;
    let mut executable = authored.clone();
    let connections = authored.connection_sources();
    let mut consumers = std::collections::BTreeSet::new();
    for route in conversions {
        if !consumers.insert(route.consumer.as_str())
            || connections.get(&route.consumer) != Some(&vec![route.producer.clone()])
        {
            return Err(Error::DeclarationCheck {
                message: format!(
                    "compiled conversion for {} does not match authored bindings",
                    route.consumer
                ),
            });
        }
        if !executable.set_binding(
            &route.consumer,
            vec![format!("brain.{}", route.output_endpoint)],
        ) || !executable.set_binding(
            &format!("brain.{}", route.input_endpoint),
            vec![route.producer.clone()],
        ) {
            return Err(Error::DeclarationCheck {
                message: "compiled conversion has no consuming runtime".into(),
            });
        }
    }
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowering_uses_compiled_endpoints_and_refuses_stale_authored_edges() {
        let authored: RobotDocument = serde_json::from_value(serde_json::json!({
            "schema": "phoxal/robot/v0", "robot": { "id": "proof", "services": {"receiver": {"source": {"path": "receiver"}, "bindings": {"capture": "source.status"}}} },
            "supervisor": { "source": { "path": "supervisor" } },


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
        let connections = executable.connection_sources();
        assert_eq!(
            connections["brain.actual_generated_in_47"],
            ["source.status"]
        );
        assert_eq!(
            connections["receiver.capture"],
            ["brain.actual_generated_out_23"]
        );
        assert!(!connections.contains_key("brain.phoxal_conversion_in_0"));
        let mut changed = authored;
        changed.set_binding("receiver.capture", vec!["other.status".into()]);
        assert!(
            lower(&changed, &compiled)
                .unwrap_err()
                .to_string()
                .contains("does not match authored bindings")
        );
    }
}
