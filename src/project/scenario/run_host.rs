//! Finite simulation run host used by ordinary Cargo tests.

use std::path::{Path, PathBuf};

use phoxal::artifact::simulation::{
    ScenarioExecutionReport, ScenarioStepEvidence, SimulatorTerminalEvidence,
};
use phoxal::scenario::fixture_protocol::{
    CaptureRecordRef, CommandReplyRef, LifecycleReport, NativeBodyRef, StepOutcomeRecord,
};
use phoxal::scenario::plan_support::{
    Action, Capture, CaptureRecord, CapturedObservation, Program, StepOutcome,
};

use crate::project::simulation::{
    SimulationBound, SimulationPresentation, SimulationRunOptions, SimulationRunReport,
};
use crate::project::{Error, SimulationModelFacts};

pub(super) fn build_probe_request(
    test_identity: &str,
    scene: PathBuf,
    simulator_executable: Option<&Path>,
) -> Result<SimulationRunOptions, Error> {
    let mut request = SimulationRunOptions::new(
        scene,
        SimulationPresentation::Headless,
        SimulationBound::Steps(1),
    )
    .map_err(|error| Error::SimulationInvalid {
        message: format!(
            "simulation test `{test_identity}` cannot build its probe request: {error}"
        ),
    })?;
    if let Some(executable) = simulator_executable {
        request = request.with_simulator_executable(executable);
    }
    Ok(request)
}

pub(super) fn drive_lifecycle(
    test_identity: &str,
    snapshot: crate::project::simulation::PreparedSimulation,
    program: &Program,
    simulator_executable: Option<&Path>,
    headless: bool,
) -> Result<SimulationRunReport, Error> {
    let presentation = if headless {
        SimulationPresentation::Headless
    } else {
        SimulationPresentation::Desktop
    };
    let mut request = SimulationRunOptions::new(
        snapshot.frozen.scene.clone(),
        presentation,
        SimulationBound::Steps(u64::from(program.transition_count())),
    )
    .map_err(|error| Error::SimulationInvalid {
        message: format!(
            "simulation test `{test_identity}` cannot construct its run request: {error}"
        ),
    })?
    .with_identity("scenarios", "run-host", sanitize_run_id(test_identity))
    .with_auto_run();
    if let Some(executable) = simulator_executable {
        request = request.with_simulator_executable(executable);
    }
    crate::project::simulation::run_prepared(snapshot, &request, Some(program))
}

pub(super) fn build_lifecycle_report(
    facts: &SimulationModelFacts,
    program: &Program,
    report: &SimulationRunReport,
) -> LifecycleReport {
    let SimulationRunReport::V0 {
        terminal: report_terminal,
        scenario: report_scenario,
        provider_contract_verified,
        supervisor_ready,
        ..
    } = report;
    let terminal_v0 = report_terminal.as_ref().map(|terminal| match terminal {
        SimulatorTerminalEvidence::V0 {
            quantum_ns,
            completed_steps,
            requested_steps,
            execution_id,
            native_body,
            ..
        } => (
            *quantum_ns,
            *completed_steps,
            *requested_steps,
            execution_id.clone(),
            native_body,
        ),
    });
    let (quantum_ns, completed_steps, final_cut, execution_id, terminal_native_body) =
        match terminal_v0 {
            Some((quantum_ns, completed, requested, execution_id, body)) => (
                quantum_ns,
                completed,
                completed == requested,
                execution_id,
                Some(body),
            ),
            None => (facts.quantum_ns, 0, false, String::new(), None),
        };
    let final_drain = final_cut && *provider_contract_verified && *supervisor_ready;
    let mut step_outcomes = Vec::new();
    let mut capture_records = Vec::new();
    let mut command_replies = Vec::new();
    if let Some(scenario) = report_scenario {
        let ScenarioExecutionReport::V0 {
            steps,
            captures,
            command_replies: replies,
            ..
        } = scenario;
        step_outcomes.extend(steps.iter().map(|observed| StepOutcomeRecord {
            label: observed.label.clone(),
            outcome: observed_step_outcome(program, scenario, observed),
        }));
        capture_records.extend(captures.iter().map(|capture| {
            CaptureRecordRef {
                name: capture.name.clone(),
                record: CaptureRecord::Observations {
                    kind: capture.kind.clone(),
                    records: capture
                        .records
                        .iter()
                        .map(|record| CapturedObservation {
                            payload: record.payload.clone(),
                            source: record.source.clone(),
                            capture_time_ns: record.capture_time_ns,
                            sequence: record.sequence,
                        })
                        .collect(),
                    gap_before_first: capture.gap_before_first,
                    complete: capture.complete,
                    terminal: capture.terminal,
                },
            }
        }));
        command_replies.extend(replies.iter().map(|(label, payload)| CommandReplyRef {
            label: label.clone(),
            response_bytes: payload.clone(),
        }));
    }
    let native_body = terminal_native_body.and_then(|samples| {
        let capture_name = program
            .captures()
            .iter()
            .find_map(|capture| match capture {
                Capture::NativeBody { name, .. } => Some(name.clone()),
                _ => None,
            })?;
        if samples.is_empty() {
            None
        } else {
            Some(NativeBodyRef {
                capture_name,
                payload: serde_json::to_vec(samples).ok()?,
            })
        }
    });
    LifecycleReport {
        execution_id,
        quantum_ns,
        completed_steps,
        final_observation_cut_observed: final_cut,
        final_capture_drain_observed: final_drain,
        step_outcomes,
        capture_records,
        command_replies,
        native_body,
    }
}

fn observed_step_outcome(
    program: &Program,
    report: &ScenarioExecutionReport,
    observed: &ScenarioStepEvidence,
) -> StepOutcome {
    let ScenarioExecutionReport::V0 {
        command_replies, ..
    } = report;
    let Some(step) = program
        .steps()
        .iter()
        .find(|step| step.label == observed.label)
    else {
        return StepOutcome::Rejected {
            reason: format!("supervisor reported undeclared step `{}`", observed.label),
        };
    };
    match (observed.kind.as_str(), &step.action) {
        ("setpoint", Action::Setpoint { .. }) => StepOutcome::SetpointDelivered {
            production: observed.production_boundary,
            eligibility: observed.eligible_boundary,
        },
        ("withdraw", Action::Withdraw { .. }) => StepOutcome::WithdrawAccepted,
        (
            "command",
            Action::Command {
                label,
                simulated_deadline,
                ..
            },
        ) => {
            let quantum_micros = u64::from(program.quantum().micros());
            let simulated_budget = u64::try_from(simulated_deadline.as_micros())
                .unwrap_or(u64::MAX)
                .div_ceil(quantum_micros);
            StepOutcome::CommandIssued {
                label: label.clone(),
                reply_pending: !command_replies.contains_key(label),
                simulated_deadline_boundary: observed
                    .production_boundary
                    .saturating_add(simulated_budget),
            }
        }
        (actual, _) => StepOutcome::Rejected {
            reason: format!(
                "supervisor reported step `{}` with incompatible action `{actual}`",
                observed.label
            ),
        },
    }
}

fn sanitize_run_id(identity: &str) -> String {
    let mut out = String::with_capacity(identity.len().min(128));
    for byte in identity.bytes().take(128) {
        let accepted =
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_');
        out.push(if accepted { byte as char } else { '-' });
    }
    if out.is_empty() {
        "test".to_owned()
    } else {
        out
    }
}
