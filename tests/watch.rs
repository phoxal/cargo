//! Public, inert watch journeys through the ordinary Cargo builder.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "owned subprocess acceptance fixtures"
)]
#[path = "support/pty.rs"]
mod pty;
mod support;
use phoxal::artifact::application::*;
use std::os::unix::process::CommandExt;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

fn fixture() -> (tempfile::TempDir, PathBuf) {
    let (guard, root) = support::stage("check-valid");
    let bytes = encode_application_contract(&ApplicationContract {
        bundle: Some(BUNDLE_CONTRACT),
        launch: SUPERVISOR_LAUNCH_CONTRACT,
        execution: Some(EXECUTION_PROTOCOL_CONTRACT),
        simulation: Some(SIMULATION_PROTOCOL_CONTRACT),
        target: HOST_EXECUTION_TARGET,
    });
    fs::write(
        root.join("supervisor/Cargo.toml"),
        "[package]\nname='phoxal-supervisor'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(root.join("supervisor/src/main.rs"), format!(
        "#[used]\n#[cfg_attr(target_os=\"macos\", unsafe(link_section=\"__DATA,__phoxal_app\"))]\n#[cfg_attr(target_os=\"linux\", unsafe(link_section=\".phoxal_app\"))]\nstatic APP:[u8;512]={bytes:?};\nfn main() {{ std::hint::black_box(&APP); println!(\"SUPERVISOR\"); }}\n"
    )).unwrap();
    let yaml = fs::read_to_string(root.join("robot.yaml")).unwrap();
    fs::write(
        root.join("robot.yaml"),
        yaml.replace(
            "  id: check-valid-robot",
            "  id: check-valid-robot\n  model: model.xml",
        ),
    )
    .unwrap();
    fs::write(root.join("model.xml"), "<mujoco><worldbody/></mujoco>\n").unwrap();
    (guard, root)
}

struct Watch {
    child: Child,
    lines: Arc<Mutex<Vec<(f64, bool, String)>>>,
    readers: Vec<std::thread::JoinHandle<()>>,
    started: Instant,
    evidence: Option<PathBuf>,
    edits: Vec<(f64, String)>,
}
impl Watch {
    fn start(root: &Path, args: &[&str], label: &str) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(root)
            .args(args)
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env("PHOXAL_HOME", root.join(".phoxal-home"))
            .env("PHOXAL_SIMULATOR", root.join("absent-simulator"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let evidence = std::env::var_os("PHOXAL_WATCH_EVIDENCE_DIR").map(|p| {
            let p = PathBuf::from(p).join(label);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("command.txt"), format!("{command:?}\n")).unwrap();
            p
        });
        let started = Instant::now();
        let mut child = command.spawn().unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();
        for (stderr, pipe) in [
            (
                false,
                Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
            ),
            (true, Box::new(child.stderr.take().unwrap())),
        ] {
            let lines = Arc::clone(&lines);
            let e = evidence.clone();
            readers.push(std::thread::spawn(move || {
                let mut log = e.as_ref().map(|p| {
                    fs::File::create(p.join(if stderr { "stderr" } else { "stdout" })).unwrap()
                });
                for line in BufReader::new(pipe).lines() {
                    let line = line.unwrap();
                    if let Some(log) = &mut log {
                        writeln!(log, "{line}").unwrap();
                    }
                    lines
                        .lock()
                        .unwrap()
                        .push((started.elapsed().as_secs_f64(), stderr, line));
                }
            }));
        }
        Self {
            child,
            lines,
            readers,
            started,
            evidence,
            edits: Vec::new(),
        }
    }
    fn mark(&mut self, label: &str) {
        self.edits
            .push((self.started.elapsed().as_secs_f64(), label.to_owned()));
    }
    fn count(&self, text: &str) -> usize {
        self.lines
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.contains(text))
            .count()
    }
    fn wait(&mut self, text: &str, count: usize) {
        let start = Instant::now();
        while self.count(text) < count {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "watch exited: {:?}",
                self.lines.lock().unwrap()
            );
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "waiting for {text}: {:?}",
                self.lines.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn idle(&mut self, count: usize) {
        self.wait("Watching robot sources.", count);
    }
    fn published_idle(&mut self) {
        self.wait("Build published:", 1);
        // Publication uses stdout and quiet feedback uses stderr. Do not
        // infer emission order from the two reader threads' merged events.
        // Each failed/stale initial attempt has its own idle; success needs
        // one additional idle after those outcomes on the stderr channel.
        let start = Instant::now();
        while self.count("Watching robot sources.")
            <= self.count("Inputs changed during build. Rebuilding.")
                + self.count("No build published yet.")
        {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "watch exited before published idle: {:?}",
                self.lines.lock().unwrap()
            );
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "waiting for published idle: {:?}",
                self.lines.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(6),
                "watch did not stop: {:?}",
                self.lines.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
        if let Some(e) = &self.evidence {
            fs::write(
                e.join("events.json"),
                serde_json::to_vec_pretty(&*self.lines.lock().unwrap()).unwrap(),
            )
            .unwrap();
            fs::write(
                e.join("edits.json"),
                serde_json::to_vec_pretty(&self.edits).unwrap(),
            )
            .unwrap();
            fs::write(
                e.join("status.txt"),
                format!(
                    "{status}\npid={}\nwall={:?}\n",
                    self.child.id(),
                    self.started.elapsed()
                ),
            )
            .unwrap();
        }
        status
    }
    fn stop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGINT);
        }
        assert_eq!(self.exit().code(), Some(130));
        assert_eq!(self.count("Stopping build watcher..."), 1);
        assert_eq!(self.count("Build watcher stopped."), 1);
        assert_eq!(self.count("Completed."), 0);
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGINT);
            }
            let start = Instant::now();
            while self.child.try_wait().unwrap().is_none()
                && start.elapsed() < Duration::from_secs(6)
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            if self.child.try_wait().unwrap().is_none() {
                self.child.kill().unwrap();
                self.child.wait().unwrap();
            }
        }
        for reader in self.readers.drain(..) {
            reader.join().unwrap();
        }
    }
}
fn selected(root: &Path) -> PathBuf {
    root.join("publication").canonicalize().unwrap()
}
fn brain_executable(root: &Path) -> PathBuf {
    let generation = selected(root);
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(generation.join("manifest.json")).unwrap()).unwrap();
    let id = &manifest["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["role"] == "brain")
        .unwrap()["artifact"];
    let artifact = manifest["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| &a["id"] == id)
        .unwrap();
    generation.join(artifact["path"].as_str().unwrap())
}
fn preserve(root: &Path, w: &Watch) {
    if let Some(e) = &w.evidence {
        // Keep authored sources and immutable reader generations, not compiler caches.
        support::copy_tree(root, &e.join("fixture")).unwrap();
    }
}

#[test]
fn immediate_edits_recovery_noop_atomic_replacement_and_idle_ownership() {
    let (_guard, root) = fixture();
    let mut w = Watch::start(
        &root,
        &["build", "--watch", "--offline", "--output", "publication"],
        "editing",
    );
    w.idle(1);
    assert_eq!(w.count("Build published:"), 1);
    let first = selected(&root);
    let old_manifest = fs::read(first.join("manifest.json")).unwrap();
    let query = support::invoke(
        &root,
        &[
            "build",
            "--watch",
            "--offline",
            "--output",
            "publication",
            "--print-output-path",
        ],
    );
    assert!(query.status.success());
    assert_eq!(
        String::from_utf8(query.stdout).unwrap().trim(),
        root.canonicalize()
            .unwrap()
            .join("publication")
            .to_str()
            .unwrap()
    );
    assert!(!String::from_utf8_lossy(&query.stderr).contains("Watching"));
    let config = support::invoke(&root, &["prepare", "--offline"]);
    assert!(
        config.status.success(),
        "idle project admission: {config:?}"
    );
    let conflict = support::invoke(&root, &["build", "--offline", "--output", "publication"]);
    assert!(!conflict.status.success());
    let conflict_advice = String::from_utf8_lossy(&conflict.stderr);
    assert!(conflict_advice.contains(&format!(
        "another cargo phoxal command owns this output {}",
        root.canonicalize().unwrap().join("publication").display()
    )));
    assert!(conflict_advice.contains("let that command exit or stop its watcher, then retry"));
    assert!(!conflict_advice.contains("wait for it to finish"));
    if let Some(e) = &w.evidence {
        fs::write(e.join("idle-conflict.stderr"), &conflict.stderr).unwrap();
        fs::write(e.join("idle-conflict.stdout"), &conflict.stdout).unwrap();
        fs::write(e.join("idle-conflict.status"), conflict.status.to_string()).unwrap();
    }
    let model = root.join("model.xml");
    fs::write(&model, "<mujoco><!--model edit--><worldbody/></mujoco>\n").unwrap();
    w.idle(2);
    assert_ne!(selected(&root), first);
    let revision = selected(&root);
    fs::write(&model, fs::read(&model).unwrap()).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(w.count("Watching robot sources."), 2);
    assert_eq!(selected(&root), revision);
    fs::write(root.join("robot.unselected.yaml"), "broken: [\n").unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(w.count("Watching robot sources."), 2);
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(
        root.join("src/replacement"),
        main.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"CODE_EDIT\");",
        ),
    )
    .unwrap();
    fs::rename(root.join("src/replacement"), root.join("src/main.rs")).unwrap();
    w.idle(3);
    let code = selected(&root);
    assert_ne!(code, revision);
    fs::write(root.join("src/main.rs"), "invalid rust\n").unwrap();
    w.idle(4);
    assert_eq!(selected(&root), code);
    assert!(w.count("Previous build preserved.") >= 1);
    fs::write(root.join("src/main.rs"), main).unwrap();
    w.idle(5);
    let yaml = fs::read_to_string(root.join("robot.yaml")).unwrap();
    fs::write(root.join("robot.yaml"), "invalid: [\n").unwrap();
    w.idle(6);
    let before = selected(&root);
    fs::remove_file(root.join("robot.yaml")).unwrap();
    w.idle(7);
    assert_eq!(selected(&root), before);
    fs::write(root.join("robot.yaml"), yaml).unwrap();
    w.idle(8);
    assert_eq!(fs::read(first.join("manifest.json")).unwrap(), old_manifest);
    w.stop();

    preserve(&root, &w);
}

#[test]
fn pinned_robot_and_cargo_target_changes_require_restart() {
    for (label, change) in [
        ("pin-id", false),
        ("pin-target", true),
        ("pin-toolchain", true),
        ("pin-config-env", true),
    ] {
        let (_guard, root) = fixture();
        // Hold the initial compiler attempt at the existing build-script seam
        // so a declared model edit deterministically makes that attempt stale.
        if label == "pin-target" {
            let build = fs::read_to_string(root.join("build.rs")).unwrap();
            fs::write(root.join("build-gate"), "wait").unwrap();
            fs::write(root.join("build.rs"), build.replace("fn main() {", "fn main() { let root=std::path::PathBuf::from(std::env::var_os(\"CARGO_MANIFEST_DIR\").unwrap()); if root.join(\"build-gate\").exists() { std::fs::write(root.join(\"build-started\"),b\"ready\").unwrap(); while root.join(\"build-gate\").exists() { std::thread::sleep(std::time::Duration::from_millis(20)); } } ")).unwrap();
        }
        let mut w = Watch::start(
            &root,
            &["build", "--watch", "--offline", "--output", "publication"],
            label,
        );
        if label == "pin-target" {
            let start = Instant::now();
            while !root.join("build-started").exists() {
                assert!(
                    w.child.try_wait().unwrap().is_none(),
                    "watch exited: {:?}",
                    w.lines.lock().unwrap()
                );
                assert!(
                    start.elapsed() < Duration::from_secs(30),
                    "waiting for initial compiler: {:?}",
                    w.lines.lock().unwrap()
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            w.mark("model edited during initial compiler attempt");
            fs::write(
                root.join("model.xml"),
                "<mujoco><worldbody><body name='initial-edit'/></worldbody></mujoco>\n",
            )
            .unwrap();
            fs::remove_file(root.join("build-gate")).unwrap();
            w.wait("Inputs changed during build. Rebuilding.", 1);
        }
        w.published_idle();
        if label == "pin-target" {
            let lines = w.lines.lock().unwrap();
            let stale = lines
                .iter()
                .position(|(_, _, line)| line.contains("Inputs changed during build."))
                .unwrap();
            let quiet = lines
                .iter()
                .position(|(_, _, line)| line.contains("Watching robot sources."))
                .unwrap();
            assert!(
                stale < quiet,
                "stale initial attempt must reach quiet feedback: {lines:?}"
            );
            drop(lines);
            assert_eq!(w.count("Build published:"), 1);
            assert!(w.count("Watching robot sources.") >= 2);
        }
        w.mark("successful publication followed by quiet watching; snapshotting prior generation");
        let prior = selected(&root);
        if label == "pin-config-env" {
            fs::create_dir_all(root.join(".cargo")).unwrap();
            fs::write(
                root.join(".cargo/config.toml"),
                "[env]\nWATCH_PRIVATE_SETTING='selected-secret-not-for-diagnostics'\n",
            )
            .unwrap();
        } else if label == "pin-toolchain" {
            fs::write(
                root.join("rust-toolchain.toml"),
                "[toolchain]\nchannel='missing-phoxal-test-toolchain'\n",
            )
            .unwrap();
        } else if change {
            fs::create_dir_all(root.join(".cargo")).unwrap();
            fs::write(
                root.join(".cargo/config.toml"),
                "[build]\ntarget='changed-watch-target'\n",
            )
            .unwrap();
        } else {
            let yaml = fs::read_to_string(root.join("robot.yaml")).unwrap();
            fs::write(
                root.join("robot.yaml"),
                yaml.replace("check-valid-robot", "changed-robot-id"),
            )
            .unwrap();
        }
        assert!(!w.exit().success());
        assert_eq!(w.count("selected-secret-not-for-diagnostics"), 0);
        assert!(
            w.count("restart cargo phoxal build --watch") > 0,
            "{:?}",
            w.lines.lock().unwrap()
        );
        assert_eq!(selected(&root), prior);
        preserve(&root, &w);
    }
}

#[test]
fn independent_transitive_topology_and_local_supervisor_graph_edits() {
    let (_guard, root) = fixture();
    let external = tempfile::tempdir().unwrap();
    let provider = external.path().join("independent provider");
    let leaf = external.path().join("outside leaf");
    fs::create_dir_all(provider.join("src")).unwrap();
    fs::create_dir_all(leaf.join("src")).unwrap();
    fs::write(
        leaf.join("Cargo.toml"),
        "[package]\nname='watch-leaf'\nversion='0.1.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(leaf.join("src/lib.rs"), "pub fn value()->u64 { 1 }\n").unwrap();
    let manifest = "[package]\nname='watch-provider'\nversion='0.1.0'\nedition='2024'\n[dependencies]\nwatch-leaf={path='../outside leaf'}\n";
    fs::write(provider.join("Cargo.toml"), manifest).unwrap();
    fs::copy(root.join("build.rs"), provider.join("build.rs")).unwrap();
    fs::write(provider.join("src/main.rs"),"include!(concat!(env!(\"OUT_DIR\"),\"/artifact.rs\"));\nfn main(){std::hint::black_box(&PHOXAL_ARTIFACT);println!(\"{}\",watch_leaf::value());}\n").unwrap();
    let yaml=fs::read_to_string(root.join("robot.yaml")).unwrap().replace("  services: {}",&format!("  services:\n    independent:\n      source: {{path: '../{}/independent provider'}}",external.path().file_name().unwrap().to_string_lossy()));
    fs::write(root.join("robot.yaml"), &yaml).unwrap();
    let wrapper = root.join("owned-cargo");
    assert!(
        Command::new("rustc")
            .args(["--edition=2024"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/process/watch_owned_cargo.rs")
            )
            .arg("-o")
            .arg(&wrapper)
            .status()
            .unwrap()
            .success()
    );
    let mut w = Watch::start(
        &root,
        &[
            "build",
            "--watch",
            "--offline",
            "--output",
            "publication",
            "--cargo",
            wrapper.to_str().unwrap(),
        ],
        "independent-graph",
    );
    w.wait("Build published:", 1);
    let execute = |root: &Path| {
        let executable = fs::read_dir(selected(root).join("bin"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("watch-provider-")
            })
            .unwrap();
        String::from_utf8(Command::new(executable).output().unwrap().stdout).unwrap()
    };
    assert_eq!(execute(&root).trim(), "1");
    let first = selected(&root);
    fs::write(leaf.join("src/lib.rs"), "pub fn value()->u64 { 2 }\n").unwrap();
    w.wait("Build published:", 2);
    assert_eq!(execute(&root).trim(), "2");
    assert_ne!(selected(&root), first);
    let next = external.path().join("new leaf");
    fs::create_dir_all(next.join("src")).unwrap();
    fs::copy(leaf.join("Cargo.toml"), next.join("Cargo.toml")).unwrap();
    fs::write(next.join("src/lib.rs"), "pub fn value()->u64 { 3 }\n").unwrap();
    fs::write(
        provider.join("Cargo.toml"),
        manifest.replace("outside leaf", "new leaf"),
    )
    .unwrap();
    w.wait("Build published:", 3);
    assert_eq!(execute(&root).trim(), "3");
    fs::write(next.join("src/lib.rs"), "pub fn value()->u64 { 4 }\n").unwrap();
    w.wait("Build published:", 4);
    assert_eq!(execute(&root).trim(), "4");
    // A newly introduced graph root can initially be absent, then recover
    // through resolver-only rescans without repeated failed-build spam.
    fs::write(
        provider.join("Cargo.toml"),
        manifest.replace("outside leaf", "missing leaf"),
    )
    .unwrap();
    w.wait("Previous build preserved.", 1);
    let preserved = selected(&root);
    fs::write(root.join("owned-gate"), "metadata").unwrap();
    let ready = Instant::now();
    while !root.join("owned-ready").exists()
        || fs::read_to_string(root.join("owned-pids.log"))
            .unwrap_or_default()
            .lines()
            .count()
            < 4
    {
        assert!(ready.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(w.count("Build: resolving watch inputs.") >= 2);
    w.mark("recovery-metadata-ready-held-start");
    std::thread::sleep(Duration::from_secs(6));
    assert!(
        w.lines
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, line)| line.contains("Still working")
                && line.contains("resolving watch inputs"))
    );
    assert_eq!(selected(&root), preserved);
    let quiet = w.count("Watching for changes.");
    let missing = external.path().join("missing leaf");
    fs::create_dir_all(missing.join("src")).unwrap();
    fs::copy(leaf.join("Cargo.toml"), missing.join("Cargo.toml")).unwrap();
    fs::write(missing.join("src/lib.rs"), "pub fn value()->u64 { 5 }\n").unwrap();
    fs::remove_file(root.join("owned-gate")).unwrap();
    w.wait("Watching for changes.", quiet + 1);
    for pid in fs::read_to_string(root.join("owned-pids.log"))
        .unwrap()
        .lines()
    {
        assert_eq!(unsafe { libc::kill(pid.parse().unwrap(), 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    w.wait("Build published:", 5);
    assert_eq!(execute(&root).trim(), "5");
    assert_ne!(selected(&root), preserved);
    // Supervisor dependencies are independent of the root and provider graphs.
    let supervisor_manifest = fs::read_to_string(root.join("supervisor/Cargo.toml")).unwrap();
    fs::write(
        root.join("supervisor/Cargo.toml"),
        format!(
            "{supervisor_manifest}\n[dependencies]\nwatch-leaf={{path={:?}}}\n",
            missing
        ),
    )
    .unwrap();
    let supervisor = fs::read_to_string(root.join("supervisor/src/main.rs")).unwrap();
    fs::write(
        root.join("supervisor/src/main.rs"),
        supervisor.replace(
            "println!(\"SUPERVISOR\")",
            "println!(\"SUPERVISOR {}\", watch_leaf::value())",
        ),
    )
    .unwrap();
    w.wait("Build published:", 6);
    let before = selected(&root);
    fs::write(missing.join("src/lib.rs"), "pub fn value()->u64 { 6 }\n").unwrap();
    w.idle(8);
    assert_ne!(selected(&root), before);
    let supervisor_exec = selected(&root).join("bin/supervisor");
    assert_eq!(
        String::from_utf8(Command::new(supervisor_exec).output().unwrap().stdout)
            .unwrap()
            .trim(),
        "SUPERVISOR 6"
    );
    w.stop();
    preserve(&root, &w);
    if let Some(e) = &w.evidence {
        support::copy_tree(external.path(), &e.join("external")).unwrap();
    }
}

#[test]
fn cancellation_reaps_resistant_group_and_observed_detached_descendants() {
    for operation in ["metadata", "build"] {
        let (_guard, root) = fixture();
        let wrapper = root.join("owned-cargo");
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/process/watch_owned_cargo.rs");
        assert!(
            Command::new("rustc")
                .args(["--edition=2024"])
                .arg(source)
                .arg("-o")
                .arg(&wrapper)
                .status()
                .unwrap()
                .success()
        );
        if operation == "metadata" {
            fs::write(root.join("owned-gate"), operation).unwrap();
        }
        let mut w = Watch::start(
            &root,
            &[
                "build",
                "--watch",
                "--offline",
                "--output",
                "publication",
                "--cargo",
                wrapper.to_str().unwrap(),
            ],
            &format!("cancel-{operation}"),
        );
        let previous = if operation == "build" {
            w.idle(1);
            let previous = selected(&root);
            fs::write(root.join("owned-gate"), operation).unwrap();
            let source = fs::read_to_string(root.join("src/main.rs")).unwrap();
            fs::write(
                root.join("src/main.rs"),
                format!("{source}\n// trigger owned compilation\n"),
            )
            .unwrap();
            Some(previous)
        } else {
            None
        };
        let started = Instant::now();
        while !root.join("owned-ready").exists()
            || fs::read_to_string(root.join("owned-pids.log"))
                .unwrap_or_default()
                .lines()
                .count()
                < 4
        {
            assert!(started.elapsed() < Duration::from_secs(20));
            std::thread::sleep(Duration::from_millis(20));
        }
        if operation == "metadata" {
            assert!(w.count("Build: resolving watch inputs.") > 0);
            assert_eq!(w.count("Watching robot sources."), 0);
            w.mark("initial-metadata-ready-held-start");
            std::thread::sleep(Duration::from_secs(6));
            assert!(
                w.lines
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|(_, _, line)| line.contains("Still working")
                        && line.contains("resolving watch inputs"))
            );
            if let Some(e) = &w.evidence {
                let stderr = w
                    .lines
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|(_, stderr, _)| *stderr)
                    .map(|(_, _, line)| format!("{line}\n"))
                    .collect::<String>();
                fs::write(e.join("initial-ready-held.stderr"), stderr).unwrap();
            }
        }
        // Ensure ancestry observation sees the fully detached tree before cancellation.
        std::thread::sleep(Duration::from_millis(100));
        let pids = fs::read_to_string(root.join("owned-pids.log"))
            .unwrap()
            .lines()
            .map(|p| p.parse::<i32>().unwrap())
            .collect::<Vec<_>>();
        let unrelated = root.join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        let mut decoy = Command::new(&wrapper)
            .arg("--owned-child")
            .current_dir(&unrelated)
            .env("PHOXAL_BUILD_OWNER", "same-name-unrelated-value")
            .spawn()
            .unwrap();
        w.stop();
        assert!(
            decoy.try_wait().unwrap().is_none(),
            "unrelated shared environment name was signaled"
        );
        decoy.kill().unwrap();
        decoy.wait().unwrap();
        if let Some(previous) = previous {
            assert_eq!(selected(&root), previous);
            assert!(w.count("Previous build preserved.") > 0);
        } else {
            assert_eq!(w.count("Build published:"), 0);
            assert!(w.count("No build published yet.") > 0);
        }
        for &pid in &pids {
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "owned PID {pid} survived cancellation"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
        if let Some(e) = &w.evidence {
            fs::write(
                e.join("owned-descendant-proof.json"),
                serde_json::to_vec_pretty(&pids).unwrap(),
            )
            .unwrap();
        }
        preserve(&root, &w);
    }
}

#[test]
fn stale_work_publication_failure_and_selected_overlay_recover_without_false_success() {
    let (_guard, root) = fixture();
    fs::write(
        root.join("selected.yaml"),
        "schema: phoxal/robot/v0\nrobot: {brain: {}}\n",
    )
    .unwrap();
    let mut w = Watch::start(
        &root,
        &[
            "-r",
            "robot.yaml",
            "-r",
            "selected.yaml",
            "build",
            "--watch",
            "--offline",
            "--output",
            "publication",
        ],
        "stale-overlay-publication",
    );
    w.idle(1);
    let initial = selected(&root);
    let initial_bytes = fs::read(initial.join("manifest.json")).unwrap();
    let build = fs::read_to_string(root.join("build.rs")).unwrap();
    fs::write(root.join("build-gate"), "wait").unwrap();
    fs::write(root.join("build.rs"),build.replace("fn main() {","fn main() { let root=std::path::PathBuf::from(std::env::var_os(\"CARGO_MANIFEST_DIR\").unwrap()); if root.join(\"build-gate\").exists() { std::fs::write(root.join(\"build-started\"),b\"ready\").unwrap(); while root.join(\"build-gate\").exists() { std::thread::sleep(std::time::Duration::from_millis(20)); } } ")).unwrap();
    let start = Instant::now();
    while !root.join("build-started").exists() {
        assert!(start.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(20));
    }
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        main.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"LATEST_EDIT\");",
        ),
    )
    .unwrap();
    fs::remove_file(root.join("build-gate")).unwrap();
    w.wait("Inputs changed during build. Rebuilding.", 1);
    w.idle(3);
    assert_eq!(
        w.count("Build published:"),
        2,
        "stale attempt must not report publication: {:?}",
        w.lines.lock().unwrap()
    );
    assert_eq!(
        fs::read(initial.join("manifest.json")).unwrap(),
        initial_bytes
    );
    let good = selected(&root);
    fs::write(root.join("selected.yaml"), "robot: [broken\n").unwrap();
    w.idle(4);
    assert_eq!(selected(&root), good);
    fs::remove_file(root.join("selected.yaml")).unwrap();
    w.idle(5);
    assert_eq!(selected(&root), good);
    fs::write(
        root.join("selected.yaml"),
        "schema: phoxal/robot/v0\nrobot: {brain: {}}\n",
    )
    .unwrap();
    w.idle(6);
    assert_eq!(selected(&root), good);
    let generations = root.join("publication.generations");
    let block = generations.join(format!(".pointer-{}", w.child.id()));
    fs::create_dir(&block).unwrap();
    fs::write(
        root.join("model.xml"),
        "<mujoco><!--blocked swap--><worldbody/></mujoco>\n",
    )
    .unwrap();
    w.idle(7);
    assert_eq!(selected(&root), good);
    assert!(w.count("cannot publish compiled bundle") > 0);
    fs::remove_dir(block).unwrap();
    fs::write(
        root.join("model.xml"),
        "<mujoco><!--recovered swap--><worldbody/></mujoco>\n",
    )
    .unwrap();
    w.idle(8);
    assert_ne!(selected(&root), good);
    assert!(!fs::read_dir(&root).unwrap().any(|e| {
        {
            let name = e.unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with(".phoxal-bundle-") && name != ".phoxal-bundle-locks"
        }
    }));
    w.stop();
    preserve(&root, &w);
}

#[test]
fn fast_daemonization_cleanup_boundary() {
    for mode in [
        "fast-metadata",
        "fast-metadata-drop-env",
        "fast-metadata-drop-fd",
        "fast-metadata-no-exec-drop-fd",
        "fast-metadata-drop-all",
    ] {
        let (_guard, root) = fixture();
        let wrapper = root.join("owned-cargo");
        assert!(
            Command::new("rustc")
                .args(["--edition=2024"])
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/process/watch_owned_cargo.rs")
                )
                .arg("-o")
                .arg(&wrapper)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("owned-gate"), mode).unwrap();
        let mut w = Watch::start(
            &root,
            &[
                "build",
                "--watch",
                "--offline",
                "--output",
                "publication",
                "--cargo",
                wrapper.to_str().unwrap(),
            ],
            mode,
        );
        w.idle(1);
        let pids = fs::read_to_string(root.join("owned-pids.log"))
            .unwrap()
            .lines()
            .map(|p| p.parse::<i32>().unwrap())
            .collect::<Vec<_>>();
        w.stop();
        let survivors = pids
            .into_iter()
            .filter(|&pid| unsafe { libc::kill(pid, 0) } == 0)
            .collect::<Vec<_>>();
        if let Some(e) = &w.evidence {
            fs::write(
                e.join("escaped-before-harness-cleanup.json"),
                serde_json::to_vec_pretty(&survivors).unwrap(),
            )
            .unwrap();
        }
        // The fixture alone owns these explicitly recorded PIDs.
        for &pid in &survivors {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while survivors
            .iter()
            .any(|&pid| unsafe { libc::kill(pid, 0) } == 0)
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            survivors
                .iter()
                .all(|&pid| unsafe { libc::kill(pid, 0) } == -1),
            "fixture-owned cleanup failed"
        );
        preserve(&root, &w);
        assert!(
            mode == "fast-metadata-drop-all" || survivors.is_empty(),
            "supported ownership survived but owned PIDs escaped: {mode} {survivors:?}"
        );
    }
}

#[test]
fn compiler_json_channels_remain_nonempty_clean_and_unduplicated() {
    let (_guard, root) = fixture();
    let mut w = Watch::start(
        &root,
        &[
            "build",
            "--watch",
            "--offline",
            "--output",
            "publication",
            "--message-format=json",
        ],
        "compiler-json",
    );
    w.idle(1);
    let first = selected(&root);
    fs::write(root.join("src/main.rs"), "invalid rust\n").unwrap();
    w.idle(2);
    assert_eq!(selected(&root), first);
    w.stop();
    let lines = w.lines.lock().unwrap();
    let records = lines
        .iter()
        .filter(|(_, stderr, _)| !*stderr)
        .map(|(_, _, line)| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(!records.is_empty());
    assert!(records.iter().all(|r| r.get("reason").is_some()));
    assert_eq!(
        records
            .iter()
            .filter(|r| r["reason"] == "compiler-message" && r["message"]["level"] == "error")
            .count(),
        1
    );
    assert!(records.iter().any(|r| r["reason"] == "compiler-artifact"));
    assert!(
        records
            .iter()
            .any(|r| r["reason"] == "build-finished" && r["success"] == false)
    );
    assert!(
        !lines
            .iter()
            .filter(|(_, stderr, _)| *stderr)
            .any(|(_, _, s)| s.contains("expected one of")),
        "compiler diagnostic duplicated on stderr"
    );
    drop(lines);
    preserve(&root, &w);
}

#[test]
fn measured_code_yaml_model_noop_and_retained_history() {
    let (_guard, root) = fixture();
    fs::write(
        root.join("model-two.xml"),
        "<mujoco><worldbody/></mujoco>\n",
    )
    .unwrap();
    let mut w = Watch::start(
        &root,
        &["build", "--watch", "--offline", "--output", "publication"],
        "measured-editing",
    );
    w.idle(1);
    let cold = selected(&root);
    let reader = fs::read(cold.join("manifest.json")).unwrap();
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    w.mark("warm-code");
    fs::write(
        root.join("src/main.rs"),
        main.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"MEASURED_CODE\");",
        ),
    )
    .unwrap();
    w.idle(2);
    let code = selected(&root);
    w.mark("yaml-only");
    let yaml = fs::read_to_string(root.join("robot.yaml")).unwrap();
    fs::write(
        root.join("robot.yaml"),
        yaml.replace("model.xml", "model-two.xml"),
    )
    .unwrap();
    w.idle(3);
    assert_ne!(selected(&root), code);
    w.mark("model-only");
    fs::write(
        root.join("model-two.xml"),
        "<mujoco><worldbody><body name='model-edit'/></worldbody></mujoco>\n",
    )
    .unwrap();
    w.idle(4);
    let model = selected(&root);
    w.mark("identical-save-noop");
    let model_bytes = fs::read(root.join("model-two.xml")).unwrap();
    fs::write(root.join("model-two.xml"), model_bytes).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    assert_eq!(w.count("Watching robot sources."), 4);
    assert_eq!(selected(&root), model);
    for i in 0..5 {
        w.mark(&format!("history-model-{i}"));
        fs::write(
            root.join("model-two.xml"),
            format!("<mujoco><worldbody><body name='revision-{i}'/></worldbody></mujoco>\n"),
        )
        .unwrap();
        w.idle(5 + i);
        assert_eq!(
            w.count("Build published:"),
            5 + i,
            "history revision failed rather than published"
        );
    }
    w.mark("history-return-A");
    fs::write(root.join("robot.yaml"), yaml).unwrap();
    w.idle(10);
    assert_eq!(selected(&root), code);
    assert_eq!(fs::read(cold.join("manifest.json")).unwrap(), reader);
    w.stop();
    preserve(&root, &w);
}

#[test]
fn watch_pty_work_idle_resize_plain_environments_and_child_output() {
    let (_guard, root) = fixture();
    let build = fs::read_to_string(root.join("build.rs")).unwrap();
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    for (index, columns, term, ci, no_color) in [
        (0, 100, "xterm-256color", false, true),
        (1, 40, "xterm-256color", false, false),
        (2, 100, "dumb", false, false),
        (3, 100, "xterm-256color", true, false),
    ] {
        fs::write(root.join("src/main.rs"), &main).unwrap();
        fs::write(root.join("build.rs"),format!("{}\n// terminal case {index}\n",build.replace("fn main() {","fn main() { let root=std::path::PathBuf::from(std::env::var_os(\"CARGO_MANIFEST_DIR\").unwrap()); std::fs::write(root.join(\"pty-ready\"),b\"ready\").unwrap(); while root.join(\"pty-gate\").exists() { std::thread::sleep(std::time::Duration::from_millis(20)); } "))).unwrap();
        fs::write(root.join("pty-gate"), "wait").unwrap();
        let _ = fs::remove_file(root.join("pty-ready"));
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(&root)
            .args(["build", "--watch", "--offline", "--output", "publication"])
            .env_remove("CARGO_TARGET_DIR")
            .env_remove("CARGO_BUILD_BUILD_DIR")
            .env("PHOXAL_HOME", root.join(".phoxal-home"))
            .process_group(0);
        let mut terminal = pty::Pty::attach(&mut command, columns);
        command.env("TERM", term);
        if ci {
            command.env("CI", "true");
        }
        if no_color {
            command.env("NO_COLOR", "1");
        }
        let started = Instant::now();
        let mut owner = Watch {
            child: command.spawn().unwrap(),
            lines: Arc::new(Mutex::new(Vec::new())),
            readers: Vec::new(),
            started,
            evidence: None,
            edits: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("pty-ready").exists() {
            assert!(Instant::now() < deadline);
            terminal.drain();
            std::thread::sleep(Duration::from_millis(20));
        }
        terminal.until("compiling the robot brain", 3);
        if index == 0 {
            terminal.until("Still working", 3);
            assert!(terminal.bytes.contains(&27));
            terminal.resize(40);
            let offset = terminal.bytes.len();
            terminal.until("Still working -", 7);
            terminal.drain();
            assert!(!terminal.bytes[offset..].contains(&27));
        } else {
            assert!(!terminal.bytes.contains(&27));
        }
        fs::remove_file(root.join("pty-gate")).unwrap();
        terminal.until("Watching robot sources.", 20);
        std::thread::sleep(Duration::from_millis(100));
        terminal.drain();
        let idle = terminal.bytes.len();
        std::thread::sleep(Duration::from_millis(1100));
        terminal.drain();
        assert_eq!(terminal.bytes.len(), idle, "idle renderer drew new output");
        let previous = selected(&root);
        fs::write(root.join("src/main.rs"), "invalid rust\n").unwrap();
        terminal.until("Previous build preserved.", 20);
        assert_eq!(selected(&root), previous);
        unsafe {
            libc::kill(owner.child.id() as i32, libc::SIGINT);
        }
        assert_eq!(owner.exit().code(), Some(130));
        terminal.drain();
        let text = String::from_utf8_lossy(&terminal.bytes);
        assert!(text.contains("expected one of"));
        assert!(!text.split("error:").nth(1).unwrap().contains('\u{1b}'));
        assert!(text.contains("Build watcher stopped. Previous build preserved."));
        assert!(!text.contains("Completed."));
        if let Some(e) = std::env::var_os("PHOXAL_WATCH_EVIDENCE_DIR") {
            let e = PathBuf::from(e).join(format!("pty-{index}"));
            fs::create_dir_all(&e).unwrap();
            fs::write(e.join("terminal.raw"), &terminal.bytes).unwrap();
            fs::write(
                e.join("settings.json"),
                serde_json::to_vec_pretty(&(
                    columns,
                    term,
                    ci,
                    no_color,
                    owner.child.id(),
                    "exit130,reaped,idle quiet,prior pointer preserved",
                ))
                .unwrap(),
            )
            .unwrap();
        }
    }
}

#[test]
fn public_directory_replacement_and_symlink_target_edits_rebuild_actual_bytes() {
    let (_guard, root) = fixture();
    let external = tempfile::tempdir().unwrap();
    let original = fs::read_to_string(root.join("src/main.rs")).unwrap();
    let mut w = Watch::start(
        &root,
        &["build", "--watch", "--offline", "--output", "publication"],
        "directory-symlink",
    );
    w.idle(1);
    fs::rename(root.join("src"), root.join("old-src")).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        original.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"REPLACED_DIRECTORY\");",
        ),
    )
    .unwrap();
    w.idle(2);
    assert!(
        String::from_utf8(
            Command::new(brain_executable(&root))
                .output()
                .unwrap()
                .stdout
        )
        .unwrap()
        .contains("REPLACED_DIRECTORY")
    );
    let leaf = external.path().join("external.rs");
    fs::write(&leaf, &original).unwrap();
    fs::remove_file(root.join("src/main.rs")).unwrap();
    std::os::unix::fs::symlink(&leaf, root.join("src/main.rs")).unwrap();
    w.idle(3);
    fs::write(
        &leaf,
        original.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"EXTERNAL_TARGET_EDIT\");",
        ),
    )
    .unwrap();
    w.idle(4);
    assert!(
        String::from_utf8(
            Command::new(brain_executable(&root))
                .output()
                .unwrap()
                .stdout
        )
        .unwrap()
        .contains("EXTERNAL_TARGET_EDIT")
    );
    let second = external.path().join("second.rs");
    fs::write(
        &second,
        original.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"SYMLINK_RETARGET\");",
        ),
    )
    .unwrap();
    std::os::unix::fs::symlink(&second, root.join("src/replacement-link")).unwrap();
    fs::rename(root.join("src/replacement-link"), root.join("src/main.rs")).unwrap();
    w.idle(5);
    assert!(
        String::from_utf8(
            Command::new(brain_executable(&root))
                .output()
                .unwrap()
                .stdout
        )
        .unwrap()
        .contains("SYMLINK_RETARGET")
    );
    w.stop();
    preserve(&root, &w);
}
