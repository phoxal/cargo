// Explicit test-only native staging/probe behavior; no production backend switch.
fn main() {
    use std::{
        env, fs,
        path::Path,
        process::{Command, Stdio},
    };
    let args: Vec<_> = env::args().collect();
    if args[1] == "stage-scene" {
        let scene = Path::new(&args[3]);
        let output = Path::new(&args[5]);
        fs::create_dir_all(output).unwrap();
        fs::copy(scene, output.join(scene.file_name().unwrap())).unwrap();
        let part = scene.parent().unwrap().join("part.xml");
        if part.is_file() {
            fs::copy(part, output.join("part.xml")).unwrap();
        }
        return;
    }
    let scene = &args[2];
    let mut digest = if cfg!(target_os = "macos") {
        let mut command = Command::new("shasum");
        command.args(["-a", "256"]);
        command
    } else {
        Command::new("sha256sum")
    };
    let output = digest
        .stdin(Stdio::from(fs::File::open(scene).unwrap()))
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let identity = stdout.split_whitespace().next().unwrap();
    if let Some(live) = LIVE_SCENE {
        fs::write(live, "<mujoco model=\"probe-mutated\"/>").unwrap();
    }
    println!(
        "{{\"model_identity\":\"{identity}\",\"quantum_ns\":10000000,\"providers\":[{{\"rate_microhertz\":50000000,\"service_instance\":\"d1\",\"port\":\"encoder\",\"shape\":\"observation\",\"retained_latest\":false,\"lease_valid_for_ms\":null,\"input_fqn\":\"google.protobuf.Empty\",\"payload_fqn\":\"phoxal.robotics.v1.EncoderSample\"}}],\"actuation_bindings\":[{{\"service_instance\":\"brain\",\"port\":\"actuators\",\"payload_fqn\":\"phoxal.component.actuator.v1.ActuatorCommand\",\"actuator_ids\":[\"d1.motor\"]}}]}}"
    );
}
