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
    let document = "schema: phoxal/robot/v0\nrobot:\n  id: configured-output-robot\nsupervisor:\n  source:\n    path: supervisor\nservices:\n  first:\n    source:\n      path: provider\n    config:\n      wheels:\n        front: null\n  second:\n    source:\n      path: provider\n    config:\n      wheels:\n        rear: null\n";
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
        String::from_utf8_lossy(&stale.stderr).contains("authored composition changed"),
        "{}",
        String::from_utf8_lossy(&stale.stderr)
    );
    assert_success(run(root, true, &["prepare", "--offline"]));
    let source = fs::read_to_string(root.join("src/main.rs"))
        .unwrap()
        .replace("FRONT_ACTUATOR", "NEW_FRONT_ACTUATOR");
    fs::write(root.join("src/main.rs"), source).unwrap();
    assert_success(run(root, false, &["check", "--offline"]));
    // An explicit layer changes only the selected instance-expanded API.
    fs::write(root.join("alternate.yaml"), "schema: phoxal/robot/v0\nservices: {second: {config: {wheels: !replace {alternate: null}}}}\n").unwrap();
    let common_source = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        common_source.replace("REAR_ACTUATOR", "ALTERNATE_ACTUATOR"),
    )
    .unwrap();
    assert_success(run(
        root,
        true,
        &[
            "prepare",
            "--offline",
            "-f",
            "robot.yaml",
            "-f",
            "alternate.yaml",
        ],
    ));
    assert_success(run(root, false, &["check", "--offline"]));
    assert_success(run(
        root,
        true,
        &[
            "check",
            "--offline",
            "-f",
            "robot.yaml",
            "-f",
            "alternate.yaml",
        ],
    ));
    // A later default command must select the common file, never inherit the layer list.
    fs::write(root.join("src/main.rs"), &common_source).unwrap();
    assert_success(run(root, true, &["check", "--offline"]));
    assert_success(run(root, false, &["check", "--offline"]));
    // Concurrent tool commands refuse the project ownership lock before publication.
    let path = root.join("target/phoxal/operation.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    fs4::FileExt::lock(&lock).unwrap();
    let blocked = run(
        root,
        true,
        &[
            "prepare",
            "--offline",
            "-f",
            "robot.yaml",
            "-f",
            "alternate.yaml",
        ],
    );
    assert!(!blocked.status.success());
    assert!(
        String::from_utf8_lossy(&blocked.stderr)
            .contains("another cargo phoxal operation owns this project")
    );
    // Ordinary Cargo remains able to consume the existing immutable snapshot.
    assert_success(run(root, false, &["check", "--offline"]));
    fs4::FileExt::unlock(&lock).unwrap();
    // The brain's authored config is admitted against its actual compiled schema.
    let manifest = root.join("Cargo.toml");
    let text = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        text.replace(
            "[dependencies]\n",
            "[dependencies]\nserde={version=\"1\",features=[\"derive\"]}\n",
        ),
    )
    .unwrap();
    let configured_brain = common_source
        .replace("fn init(_:())->", "fn init(config:BrainConfig)->")
        .replace("{Ok(Self)}", "{let _=config.threshold;Ok(Self)}");
    fs::write(root.join("src/main.rs"), format!("#[derive(serde::Deserialize,phoxal::Config)]struct BrainConfig{{threshold:u32}}\n{configured_brain}")).unwrap();
    fs::write(
        root.join("brain.yaml"),
        "schema: phoxal/robot/v0\nbrain: {config: {threshold: 3}}\n",
    )
    .unwrap();
    assert_success(run(
        root,
        true,
        &["check", "--offline", "-f", "robot.yaml", "-f", "brain.yaml"],
    ));
    fs::write(
        root.join("brain.yaml"),
        "schema: phoxal/robot/v0\nbrain: {config: {threshold: invalid}}\n",
    )
    .unwrap();
    let invalid = run(
        root,
        true,
        &["check", "--offline", "-f", "robot.yaml", "-f", "brain.yaml"],
    );
    assert!(!invalid.status.success());
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("brain.config"));
}
