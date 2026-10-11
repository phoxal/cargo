// Explicit test-only native staging/probe behavior; no production backend switch.
unsafe extern "C" {
    fn fork() -> i32;
    fn setsid() -> i32;
    fn _exit(status: i32) -> !;
    fn signal(sig: i32, handler: usize) -> usize;
    fn close(fd: i32) -> i32;
}
fn record(exe: &std::path::Path) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(exe.with_extension("pids"))
        .unwrap();
    file.write_all(format!("{}\n", std::process::id()).as_bytes())
        .unwrap();
}
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
    let exe = std::path::PathBuf::from(FIXTURE_CONTROL);
    let mode = std::fs::read_to_string(exe.with_extension("mode")).unwrap_or_default();
    if args[1] == "owned-probe-daemon" {
        unsafe {
            signal(2, 1);
            signal(15, 1);
        }
        record(&exe);
        fs::write(exe.with_extension("ready"), "ready").unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
    if args[1] != "probe" {
        std::fs::write(exe.with_extension("run-started"), "started").unwrap();
        std::process::exit(24);
    }
    if mode.starts_with("stall") || mode.starts_with("valid-detached") {
        record(&exe);
        if mode.starts_with("stall") {
            println!("probe stdout retained");
            eprintln!("probe stderr retained");
        }
        if mode.contains("resistant") {
            unsafe {
                signal(2, 1);
                signal(15, 1);
            }
        }
        if mode.contains("drop-env") {
            unsafe {
                std::env::remove_var("PHOXAL_BUILD_OWNER");
            }
        }
        if mode.contains("drop-fd") {
            for fd in 3..65536 {
                unsafe {
                    close(fd);
                }
            }
        }
        unsafe {
            if fork() == 0 {
                if mode.contains("detached") {
                    setsid();
                    if fork() > 0 {
                        _exit(0);
                    }
                }
                if mode.contains("exec") && !mode.contains("no-exec") {
                    use std::os::unix::process::CommandExt;
                    let error = Command::new(std::env::current_exe().unwrap())
                        .arg("owned-probe-daemon")
                        .exec();
                    panic!("fixture daemon exec: {error}");
                }
                record(&exe);
                std::fs::write(exe.with_extension("ready"), "ready").unwrap();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                }
            }
        }
        if mode.starts_with("stall") {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
    }
    if mode == "malformed" {
        println!("not JSON");
        return;
    }
    if mode == "incomplete" {
        println!(
            "{{\"model_identity\":\"\",\"quantum_ns\":0,\"providers\":[],\"actuation_bindings\":[]}}"
        );
        return;
    }
    if mode == "nonzero" {
        eprintln!("probe refused fixture");
        std::process::exit(17);
    }
    if mode == "oversized" {
        record(&exe);
        use std::io::Write;
        let bytes = vec![b'x'; 1024 * 1024];
        loop {
            std::io::stdout().write_all(&bytes).unwrap();
        }
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
