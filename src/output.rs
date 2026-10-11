//! Output ownership: final diagnostics, channel policy, and result printing.
//!
//! Human feedback keeps the live renderer in [`crate::feedback`]; this module
//! owns the permanent output boundary: which channel a message belongs to,
//! how errors render in plain and JSON diagnostics modes, and how captured
//! child output is forwarded. Channel order is fixed here so command
//! workflows never decide presentation details.

use std::path::PathBuf;

use crate::feedback;
use crate::project;

/// Prints one status message to its channel: stderr in JSON diagnostics
/// mode so compiler JSON stays alone on stdout, stdout otherwise.
pub(crate) fn status(diagnostics_json: bool, message: &str) {
    if diagnostics_json {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

/// Forwards captured child output on its original channel.
pub(crate) fn bytes(bytes: &[u8], stderr: bool) {
    if bytes.is_empty() {
        return;
    }
    if stderr {
        eprint!("{}", String::from_utf8_lossy(bytes));
    } else {
        print!("{}", String::from_utf8_lossy(bytes));
    }
}

/// Prints the final error, with recovery advice in plain mode and a
/// structured Phoxal diagnostic in JSON mode.
///
/// Structured diagnostics carry a span only when the parser located the
/// failure in real authored text; directories and locationless errors never
/// get a fabricated span.
pub(crate) fn error(error: &project::Error, diagnostics_json: bool, selected_files: &[PathBuf]) {
    if !diagnostics_json {
        feedback::line(&format!("error: {error:#}"));
        match error {
            project::Error::Discovery(project::DiscoveryError::MissingRobot { .. }) => {
                feedback::line(
                    "Next: run this command from a robot project containing robot.yaml and Cargo.toml.",
                )
            }
            project::Error::Discovery(project::DiscoveryError::MissingManifest { .. }) => {
                feedback::line("Next: restore the robot's root Cargo.toml, then retry.")
            }
            project::Error::ReadRobot { .. } | project::Error::ParseRobot { .. } => {
                // An executable recovery command preserving every selected
                // file in order: running it reproduces the same selection,
                // so it fails on the same missing file instead of silently
                // validating an unrelated default robot.yaml.
                let mut command = String::from("cargo phoxal");
                for file in selected_files {
                    command.push_str(" -r ");
                    command.push_str(&shell_single_quoted(&file.display().to_string()));
                }
                command.push_str(" config");
                feedback::line(&format!(
                    "Next: correct the selected robot files, then inspect the composition with `{command}`."
                ));
            }
            _ => {}
        }
        return;
    }
    let span = authored_location(error).map(|(path, location)| {
        serde_json::json!({
            "file_name": path,
            "byte_start": location.index(),
            "byte_end": location.index(),
            "line_start": location.line(),
            "line_end": location.line(),
            "column_start": location.column(),
            "column_end": location.column(),
            "is_primary": true,
            "label": "Phoxal project diagnostic"
        })
    });
    let diagnostic = serde_json::json!({
        "reason": "phoxal-diagnostic",
        "message": format!("{error:#}"),
        "level": "error",
        "spans": span.into_iter().collect::<Vec<_>>(),
        "children": [],
        "rendered": format!("error: {error:#}\n")
    });
    eprintln!("{diagnostic}");
}

/// Quotes one path for a POSIX shell command line.
fn shell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The real parser location of an authored YAML failure, when the parser
/// reported one.
fn authored_location(error: &project::Error) -> Option<(PathBuf, serde_yaml::Location)> {
    match error {
        project::Error::ParseRobot { path, source } => {
            source.location().map(|location| (path.clone(), location))
        }
        _ => None,
    }
}
