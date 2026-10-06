//! Real source preparation and ordinary generated-API compilation for two
//! configured instances sharing one executable template.
mod support;
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(root: &Path, cli: bool, arguments: &[&str]) -> Output {
    let program = if cli {
        std::ffi::OsString::from(env!("CARGO_BIN_EXE_cargo-phoxal"))
    } else {
        std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
    };
    Command::new(program)
        .current_dir(root)
        .args(arguments)
        .env("PHOXAL_HOME", root.join("phoxal-home"))
        .env(
            "CARGO_TARGET_DIR",
            Path::new(env!("CARGO_MANIFEST_DIR")).join("target/suites/configured-outputs"),
        )
        .output()
        .unwrap_or_else(|error| panic!("real command could not start: {error}"))
}
fn assert_success(output: Output) {
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn shared_executable_has_instance_bound_apis_and_stale_config_requires_prepare() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    support::copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/output-family"),
        &root.join("provider"),
    )
    .unwrap();
    let dependency = format!(
        "phoxal = {{ path = {:?}, default-features = false, features = [\"runtime\"] }}",
        support::sdk::sdk_root()
    );
    // Use the SDK selected by the tool under test, including its matching build
    // helper through the SDK's local-input-only build feature.
    fs::write(root.join("provider/Cargo.toml"), format!("[package]\nname=\"configured-output-provider\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\n{dependency}\nserde={{version=\"1\",features=[\"derive\"]}}\n[build-dependencies]\nphoxal={{path={:?},default-features=false,features=[\"build\"]}}\n", support::sdk::sdk_root())).unwrap();
    fs::write(
        root.join("provider/build.rs"),
        "fn main()->Result<(),phoxal::build::Error>{phoxal::build::api(Default::default())}",
    )
    .unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("Cargo.toml"), format!("[package]\nname=\"configured-output-robot\"\nversion=\"0.1.0\"\nedition=\"2024\"\n[dependencies]\n{dependency}\n[build-dependencies]\nphoxal={{path={:?},default-features=false,features=[\"build\"]}}\n", support::sdk::sdk_root())).unwrap();
    fs::write(
        root.join("build.rs"),
        "fn main()->Result<(),phoxal::build::Error>{phoxal::build::api(Default::default())}",
    )
    .unwrap();
    fs::write(root.join("src/main.rs"), "phoxal::api!();\n#[phoxal::endpoints]struct BrainApi{}\nstruct Brain;\n#[phoxal::runtime(contract=BrainApi,period_ms=20)]impl Brain{#[init]fn init(_:())->phoxal::Result<Self>{Ok(Self)}}\nfn main()->phoxal::Result<()>{let _=(api::first::FRONT_ACTUATOR,api::second::REAR_ACTUATOR);phoxal::runtime::run::<Brain>()}").unwrap();
    let document = "schema: phoxal/robot/v0\nrobot: {id: configured-output-robot}\nsupervisor: {source: {path: supervisor}}\nservices:\n  first: {source: {path: provider}, config: {wheels: {front: null}}}\n  second: {source: {path: provider}, config: {wheels: {rear: null}}}\nconnections: []\n";
    fs::write(root.join("robot.yaml"), document).unwrap();
    assert_success(run(root, true, &["prepare", "--offline"]));
    assert_success(run(root, false, &["check", "--offline"]));
    fs::write(
        root.join("robot.yaml"),
        document.replace("front: null", "new_front: null"),
    )
    .unwrap();
    let stale = run(root, false, &["check", "--offline"]);
    assert!(!stale.status.success());
    assert!(
        String::from_utf8_lossy(&stale.stderr)
            .contains("does not match its current configuration/template"),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert_success(run(root, true, &["prepare", "--offline"]));
    let source = fs::read_to_string(root.join("src/main.rs"))
        .unwrap()
        .replace("FRONT_ACTUATOR", "NEW_FRONT_ACTUATOR");
    fs::write(root.join("src/main.rs"), source).unwrap();
    assert_success(run(root, false, &["check", "--offline"]));
}
