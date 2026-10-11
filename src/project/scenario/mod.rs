//! Command-scoped host for ordinary Rust simulation tests.
//!
//! [`fixture_host`] owns the bounded protocol shared with test processes.
//! `run_host` starts the test command, serves fixture requests, launches the
//! supervisor and simulator for an executing test, and returns the original
//! Cargo test outcome.

pub(crate) mod fixture_host;
mod run_host;

#[cfg(all(test, unix))]
mod producer_report_tests {
    use std::os::unix::process::ExitStatusExt;

    use phoxal::artifact::simulation::{ScenarioExecutionReport, SimulationRunReport};
    use phoxal::scenario::plan_support::{Action, CaptureRecord, Program, StepOutcome};

    #[test]
    fn genuine_public_producer_packets_cross_report_and_scenario_owners() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/reports/public-producer");
        let cases: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("packet-map.json")).unwrap()).unwrap();
        for case in cases.as_array().unwrap() {
            let name = case["case"].as_str().unwrap();
            let stdout = std::fs::read(root.join(format!("{name}.json"))).unwrap();
            use sha2::{Digest, Sha256};
            assert_eq!(format!("{:x}", Sha256::digest(&stdout)), case["sha256"]);
            let raw: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
            let code = case["report_exit"].as_i64().unwrap() as i32;
            let output = std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout,
                stderr: Vec::new(),
            };
            let report = crate::project::simulation::read_run_report(&output).unwrap();
            assert_eq!(report.success(), code == 0, "{name}");
            let SimulationRunReport::V0 {
                scope,
                supervisor_id,
                run_id,
                cleanup,
                simulator_stdout,
                simulator_stderr,
                scenario,
                ..
            } = &report;
            for (value, field) in [
                (scope, "scope"),
                (supervisor_id, "supervisor_id"),
                (run_id, "run_id"),
            ] {
                assert_eq!(serde_json::to_value(value).unwrap(), raw[field]);
            }
            assert_eq!(serde_json::to_value(cleanup).unwrap(), raw["cleanup"]);
            assert_eq!(simulator_stdout, raw["simulator_stdout"].as_str().unwrap());
            assert_eq!(simulator_stderr, raw["simulator_stderr"].as_str().unwrap());
            if code == 0 {
                let duplicate: serde_json::Value = serde_json::from_str(simulator_stdout).unwrap();
                assert_eq!(duplicate, raw["terminal"]);
                assert!(duplicate["native_bindings"].is_object());
                assert!(duplicate["provenance"].is_object());
            } else {
                assert!(raw["terminal"].is_null());
                assert!(!simulator_stderr.is_empty());
            }
            let program_name = if name == "turn_stop" {
                "turn_stop"
            } else {
                "forward_stop"
            };
            let program = Program::decode(
                &std::fs::read(root.join(format!("{program_name}-program.json"))).unwrap(),
            )
            .unwrap();
            let facts = crate::project::SimulationModelFacts {
                model_identity: String::new(),
                quantum_ns: 10_000_000,
                providers: Vec::new(),
                actuation_bindings: Vec::new(),
            };
            let lifecycle = super::run_host::build_lifecycle_report(&facts, &program, &report);
            assert_eq!(
                lifecycle.completed_steps,
                case["completed_steps"].as_u64().unwrap_or(0)
            );
            assert_eq!(lifecycle.final_capture_drain_observed, code == 0);
            if let Some(scenario) = scenario {
                let ScenarioExecutionReport::V0 {
                    steps,
                    captures,
                    command_replies,
                    ..
                } = scenario;
                assert_eq!(serde_json::to_value(scenario).unwrap(), raw["scenario"]);
                assert_eq!(lifecycle.step_outcomes.len(), steps.len());
                for (converted, actual) in lifecycle.step_outcomes.iter().zip(steps) {
                    assert_eq!(converted.label, actual.label);
                    match (&converted.outcome, actual.kind.as_str()) {
                        (
                            StepOutcome::SetpointDelivered {
                                production,
                                eligibility,
                            },
                            "setpoint",
                        ) => {
                            assert_eq!(*production, actual.production_boundary);
                            assert_eq!(*eligibility, actual.eligible_boundary);
                        }
                        (StepOutcome::WithdrawAccepted, "withdraw") => {}
                        (
                            StepOutcome::CommandIssued {
                                label,
                                reply_pending,
                                simulated_deadline_boundary,
                            },
                            "command",
                        ) => {
                            let step = program
                                .steps()
                                .iter()
                                .find(|step| step.label == actual.label)
                                .unwrap();
                            let Action::Command {
                                label: expected_label,
                                simulated_deadline,
                                ..
                            } = &step.action
                            else {
                                panic!("expected authored command")
                            };
                            assert_eq!(label, expected_label);
                            assert_eq!(*reply_pending, !command_replies.contains_key(label));
                            let budget = u64::try_from(simulated_deadline.as_micros())
                                .unwrap()
                                .div_ceil(u64::from(program.quantum().micros()));
                            assert_eq!(
                                *simulated_deadline_boundary,
                                actual.production_boundary + budget
                            );
                        }
                        _ => panic!("unexpected actual scenario action"),
                    }
                }
                assert_eq!(lifecycle.capture_records.len(), captures.len());
                for (converted, actual) in lifecycle.capture_records.iter().zip(captures) {
                    assert_eq!(converted.name, actual.name);
                    let CaptureRecord::Observations {
                        kind,
                        records,
                        gap_before_first,
                        complete,
                        terminal,
                    } = &converted.record
                    else {
                        panic!("expected observations")
                    };
                    assert_eq!(kind, &actual.kind);
                    assert_eq!(
                        (*gap_before_first, *complete, *terminal),
                        (actual.gap_before_first, actual.complete, actual.terminal)
                    );
                    assert_eq!(records.len(), actual.records.len());
                    for (converted, actual) in records.iter().zip(&actual.records) {
                        assert_eq!(
                            (
                                &converted.payload,
                                &converted.source,
                                converted.capture_time_ns,
                                converted.sequence
                            ),
                            (
                                &actual.payload,
                                &actual.source,
                                actual.capture_time_ns,
                                actual.sequence
                            )
                        );
                    }
                }
                assert_eq!(lifecycle.command_replies.len(), command_replies.len());
                let body = lifecycle.native_body.as_ref().unwrap();
                let body: Vec<phoxal::artifact::simulation::NativeBodySample> =
                    serde_json::from_slice(&body.payload).unwrap();
                let expected: Vec<phoxal::artifact::simulation::NativeBodySample> =
                    serde_json::from_value(raw["terminal"]["native_body"].clone()).unwrap();
                assert_eq!(body.len(), expected.len());
                for (actual, expected) in body.iter().zip(&expected) {
                    assert_eq!(actual.boundary, expected.boundary);
                    let actual_values = actual
                        .position_m
                        .iter()
                        .chain(&actual.orientation_wxyz)
                        .chain(&actual.linear_velocity_mps)
                        .chain(&actual.angular_velocity_radps);
                    let expected_values = expected
                        .position_m
                        .iter()
                        .chain(&expected.orientation_wxyz)
                        .chain(&expected.linear_velocity_mps)
                        .chain(&expected.angular_velocity_radps);
                    for (actual, expected) in actual_values.zip(expected_values) {
                        assert!(
                            (actual - expected).abs()
                                <= 4.0 * f64::EPSILON * expected.abs().max(f64::MIN_POSITIVE)
                        );
                    }
                }
            }
            let mut mismatch = output;
            mismatch.status = std::process::ExitStatus::from_raw((1 - code) << 8);
            assert!(crate::project::simulation::read_run_report(&mismatch).is_err());
            println!(
                "Actual producer {name}: Cargo typed reader, status refusal and scenario conversion pass"
            );
        }
    }
}
