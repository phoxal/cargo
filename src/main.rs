#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

use std::process::ExitCode;

mod cli;
mod commands;
mod feedback;
mod output;
mod owned_process;
mod project;

use cli::Command;

fn main() -> ExitCode {
    // Terminal parsing first: --help and --version answer before any
    // filesystem discovery, even from an invocation directory that no
    // longer exists.
    let cli = cli::parse();
    let invocation_directory = match std::env::current_dir() {
        Ok(directory) => directory,
        Err(source) => {
            // The diagnostics policy still resolves from the parsed
            // selections alone; the placeholder directory is never used
            // because this invocation fails before any path matters.
            let diagnostics_json = cli
                .resolve(std::path::Path::new(""))
                .map(|invocation| invocation.diagnostics_json)
                .unwrap_or(false);
            return failure(
                project::Error::Discovery(project::DiscoveryError::Resolve {
                    path: ".".into(),
                    source,
                }),
                diagnostics_json,
                &[],
            );
        }
    };
    let invocation = match cli.resolve(&invocation_directory) {
        Ok(invocation) => invocation,
        Err(error) => return failure(error, false, &[]),
    };
    let diagnostics_json = invocation.diagnostics_json;
    let selected_files = invocation.files.clone();
    let label = feedback_label(&invocation.command, diagnostics_json);
    let succeeded = invocation.command.clone();
    let result = match label {
        Some(label) => {
            // Run and Scenario can inherit child terminal output internally.
            let live = !matches!(
                &invocation.command,
                Command::Run { .. } | Command::Scenario { .. }
            );
            feedback::run(label, live, |progress| {
                commands::run(invocation, Some(progress))
            })
        }
        None => commands::run(invocation, None),
    };
    match result {
        Ok(()) => {
            // Build reports its own typed publication outcome; the generic
            // completion footer stays silent for it.
            if let Some(label) = label
                && !matches!(succeeded, Command::Build { .. })
            {
                feedback::line(&format!("{label}: Completed."));
            }
            ExitCode::SUCCESS
        }
        Err(project::Error::WatchCancelled) => ExitCode::from(130),
        Err(project::Error::WatchFailure { source, preserved }) => {
            let status = failure(*source, diagnostics_json, &selected_files);
            if let Some(previous) = preserved {
                feedback::line(if previous {
                    "Previous build preserved."
                } else {
                    "No build published yet."
                });
            }
            status
        }
        Err(error) => failure(error, diagnostics_json, &selected_files),
    }
}

/// The live progress label of one command, suppressed for commands without
/// progress phases and in JSON diagnostics mode.
fn feedback_label(command: &Command, diagnostics_json: bool) -> Option<&'static str> {
    if diagnostics_json {
        return None;
    }
    match command {
        Command::Config { .. } | Command::Simulation(_) => None,
        // The path query is resolution only; it never shows liveness.
        Command::Build {
            print_output_path: true,
            ..
        }
        | Command::Build { watch: true, .. } => None,
        Command::Prepare { .. } => Some("Prepare"),
        Command::Check { .. } => Some("Check"),
        Command::Build { .. } => Some("Build"),
        Command::Run { .. } => Some("Run"),
        Command::Test { .. } => Some("Test"),
        Command::Scenario { .. } => Some("Scenario"),
    }
}

fn failure(
    error: project::Error,
    diagnostics_json: bool,
    selected_files: &[std::path::PathBuf],
) -> ExitCode {
    output::error(&error, diagnostics_json, selected_files);
    ExitCode::FAILURE
}
