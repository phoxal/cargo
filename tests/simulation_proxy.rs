//! Prepared-build execution delegates streams and exit status to the simulator.
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[allow(
    clippy::unwrap_used,
    reason = "isolated executable fixture setup and assertions"
)]
fn simulator_fixture(directory: &Path) -> PathBuf {
    use phoxal::artifact::application::*;
    let record = ApplicationContract {
        bundle: Some(BUNDLE_CONTRACT),
        launch: SIMULATOR_LAUNCH_CONTRACT,
        execution: None,
        simulation: Some(SIMULATION_PROTOCOL_CONTRACT),
        target: HOST_EXECUTION_TARGET,
    };
    let bytes = encode_application_contract(&record);
    let source = directory.join("simulator.rs");
    let executable = directory.join("simulator");
    fs::write(
        &source,
        format!(
            r#"
#[used]
#[cfg_attr(target_os = "macos", unsafe(link_section = "__DATA,__phoxal_app"))]
#[cfg_attr(target_os = "linux", unsafe(link_section = ".phoxal_app"))]
static RECORD: [u8; 512] = {bytes:?};
fn main() {{
    use std::io::Read;
    for argument in std::env::args().skip(1) {{ println!("{{argument}}"); }}
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    print!("{{input}}");
    eprintln!("native diagnostic");
    std::process::exit(23);
}}
"#
        ),
    )
    .unwrap();
    let compiled = Command::new("rustc")
        .args(["--edition=2024", "--crate-name", "simulator_fixture"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    executable
}

#[test]
fn prepared_build_proxy_preserves_paths_standard_streams_and_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let executable = simulator_fixture(&root);
    let mut child = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(&root)
        .env("PHOXAL_SIMULATOR", executable)
        .env("CARGO", "/missing/cargo-must-not-run")
        .args([
            "phoxal",
            "simulation",
            "a scene with spaces.xml",
            "--build",
            "a build with spaces",
            "--headless",
            "--duration",
            "250ms",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"input stream\n")
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert_eq!(result.status.code(), Some(23));
    assert_eq!(
        String::from_utf8_lossy(&result.stdout),
        format!(
            "run\n{}\n--build\n{}\n--headless\n--duration\n250ms\ninput stream\n",
            root.join("a scene with spaces.xml").display(),
            root.join("a build with spaces").display()
        )
    );
    assert_eq!(result.stderr, b"native diagnostic\n");
}

#[test]
fn absent_executable_names_the_normal_install_command() {
    let result = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .env("PHOXAL_SIMULATOR", "/missing/phoxal-simulator")
        .args(["simulation", "scene.xml", "--build", "build"])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("cargo install phoxal-simulator"));
}
