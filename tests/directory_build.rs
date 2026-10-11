//! Public directory-build acceptance: dev defaults, publication slots,
//! generations, the path query, and failure preservation.

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use support::{invoke, stage};

fn build(root: &Path, arguments: &[&str]) -> std::process::Output {
    let mut all = vec!["build", "--offline"];
    all.extend_from_slice(arguments);
    invoke(root, &all)
}

fn assert_success(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
#[allow(
    clippy::unwrap_used,
    reason = "acceptance fixture setup and assertions"
)]
fn compile_only_tests_keep_build_wording_and_do_not_execute_the_harness() {
    let (_guard, root) = stage("check-valid");
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        format!(
            "{main}\n#[test] fn execution_is_forbidden() {{ panic!(\"HARNESS_EXECUTED\"); }}\n"
        ),
    )
    .unwrap();
    let compiled = invoke(
        &root,
        &["test", "--offline", "--no-run", "--", "--nocapture"],
    );
    assert_success(&compiled, "compile-only tests");
    assert!(stderr(&compiled).contains("building robot tests"));
    assert!(!stderr(&compiled).contains("testing the robot project"));
    let tested = invoke(&root, &["test", "--offline", "--", "--nocapture"]);
    assert!(!tested.status.success());
    assert!(stderr(&tested).contains("testing the robot project"));
    assert!(format!("{}{}", stdout(&tested), stderr(&tested)).contains("HARNESS_EXECUTED"));
}

#[test]
#[allow(
    clippy::unwrap_used,
    reason = "acceptance fixture setup and assertions"
)]
fn check_resolves_ordered_config_overrides_for_root_and_local_participant() {
    let (_guard, root) = stage("check-valid");
    for (folder, value) in [("first override", 20), ("selected override", 21)] {
        let dependency = root.join(folder);
        fs::create_dir_all(dependency.join("src")).unwrap();
        fs::write(
            dependency.join("Cargo.toml"),
            "[package]\nname='config-only-dependency'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(
            dependency.join("src/lib.rs"),
            format!("pub const fn selected() -> u8 {{ {value} }}\n"),
        )
        .unwrap();
    }
    let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        format!("{manifest}\n[dependencies]\nconfig-only-dependency='0.1.0'\n"),
    )
    .unwrap();
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        main.replace(
            "std::hint::black_box(&PHOXAL_ARTIFACT);",
            "std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"OVERRIDE_{}\", config_only_dependency::selected());",
        ),
    )
    .unwrap();
    let provider = root.join("provider");
    fs::create_dir_all(provider.join("src")).unwrap();
    for file in ["Cargo.toml", "build.rs", "src/main.rs"] {
        fs::copy(root.join(file), provider.join(file)).unwrap();
    }
    let manifest = fs::read_to_string(provider.join("Cargo.toml")).unwrap();
    fs::write(
        provider.join("Cargo.toml"),
        manifest.replace("check-valid-robot", "config-provider"),
    )
    .unwrap();
    let document = fs::read_to_string(root.join("robot.yaml")).unwrap();
    fs::write(
        root.join("robot.yaml"),
        document.replace(
            "  services: {}",
            "  services:\n    provider: {source: {path: provider}}",
        ),
    )
    .unwrap();
    let overlays = ["first override", "selected override"].map(|folder| {
        format!(
            "patch.crates-io.config-only-dependency.path='{}'",
            root.join(folder).display()
        )
    });
    let checked = invoke(
        &root,
        &[
            "check",
            "--offline",
            "--",
            "--config",
            &overlays[0],
            "--config",
            &overlays[1],
        ],
    );
    assert_success(&checked, "Check with local dependency graph overrides");
    // Each independent graph must resolve the selected local package, and
    // both prepared executables must actually contain its last-overlay bytes.
    for directory in [&root, &provider] {
        let metadata = Command::new("cargo")
            .current_dir(directory)
            .args([
                "metadata",
                "--offline",
                "--format-version",
                "1",
                "--config",
                &overlays[0],
                "--config",
                &overlays[1],
            ])
            .output()
            .unwrap();
        assert_success(&metadata, "selected Cargo graph");
        let graph: serde_json::Value = serde_json::from_slice(&metadata.stdout).unwrap();
        assert!(graph["packages"].as_array().unwrap().iter().any(|package| {
            package["name"] == "config-only-dependency"
                && package["manifest_path"]
                    == root.join("selected override/Cargo.toml").to_str().unwrap()
        }));
    }
    fn executables(directory: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                executables(&path, found);
            } else if matches!(
                path.file_name().and_then(|name| name.to_str()),
                Some("check-valid-robot" | "config-provider")
            ) {
                found.push(path);
            }
        }
    }
    let mut built = Vec::new();
    executables(&root.join("target/phoxal/.selected"), &mut built);
    executables(&provider.join("target/phoxal/.selected"), &mut built);
    assert_eq!(built.len(), 2);
    for executable in built {
        let executed = Command::new(executable).output().unwrap();
        assert_success(&executed, "inert prepared executable");
        assert_eq!(stdout(&executed).trim(), "OVERRIDE_21");
    }
}

#[test]
#[allow(
    clippy::unwrap_used,
    reason = "acceptance fixture setup and assertions"
)]
fn shared_compiler_root_keeps_brain_participant_and_supervisor_sources_distinct() {
    for configured in [false, true] {
        let (_a_guard, a) = stage("check-valid");
        let (_b_guard, b) = stage("check-valid");
        let shared = tempfile::tempdir().unwrap();
        // Author both equal-name/version source trees before compiling either.
        for (root, marker, period) in [(&a, "SOURCE_A", 10), (&b, "SOURCE_B", 11)] {
            if configured {
                fs::create_dir_all(root.join(".cargo")).unwrap();
                fs::write(
                    root.join(".cargo/config.toml"),
                    format!(
                        "[build]\nbuild-dir={}\n",
                        toml::Value::String(
                            shared
                                .path()
                                .join("intermediates")
                                .to_string_lossy()
                                .into_owned()
                        )
                    ),
                )
                .unwrap();
            }
            let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
            fs::write(
                root.join("src/main.rs"),
                main.replace(
                    "std::hint::black_box(&PHOXAL_ARTIFACT);",
                    &format!("std::hint::black_box(&PHOXAL_ARTIFACT); println!(\"{marker}\");"),
                ),
            )
            .unwrap();
            let script = fs::read_to_string(root.join("build.rs")).unwrap();
            fs::write(
                root.join("build.rs"),
                script.replace("period_ms\\\":10", &format!("period_ms\\\":{period}")),
            )
            .unwrap();
            let provider = root.join("provider");
            fs::create_dir_all(provider.join("src")).unwrap();
            fs::write(
                provider.join("Cargo.toml"),
                "[package]\nname='source-provider'\nversion='0.1.0'\nedition='2024'\n",
            )
            .unwrap();
            fs::copy(root.join("src/main.rs"), provider.join("src/main.rs")).unwrap();
            fs::copy(root.join("build.rs"), provider.join("build.rs")).unwrap();
            let document = fs::read_to_string(root.join("robot.yaml")).unwrap();
            fs::write(
                root.join("robot.yaml"),
                document.replace(
                    "  services: {}",
                    "  services:\n    provider: {source: {path: provider}}\n",
                ),
            )
            .unwrap();
            let supervisor = fs::read_to_string(root.join("supervisor/src/main.rs")).unwrap();
            fs::write(
                root.join("supervisor/src/main.rs"),
                supervisor.replace("phoxal-supervisor 0.0.0-dev.8", marker),
            )
            .unwrap();
        }
        let mut original_generation = None;
        for (root, marker, period) in [
            (&a, "SOURCE_A", 10),
            (&b, "SOURCE_B", 11),
            (&a, "SOURCE_A", 10),
        ] {
            // One-off overlays must not bypass the source namespace either.
            let overlay = format!(
                "build.build-dir={}",
                toml::Value::String(
                    shared
                        .path()
                        .join("overlay intermediates")
                        .to_string_lossy()
                        .into_owned()
                )
            );
            for operation in ["check", "test"] {
                let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
                command
                    .current_dir(root)
                    .env("CARGO_TARGET_DIR", shared.path().join("target"))
                    .env("PHOXAL_HOME", shared.path().join("home"));
                if !configured {
                    command.env("CARGO_BUILD_BUILD_DIR", shared.path().join("intermediates"));
                }
                command.args([operation, "--offline"]);
                if operation == "test" {
                    command.arg("--no-run");
                }
                if operation == "check" {
                    command.args(["--message-format", "json", "--", "--config", &overlay]);
                } else {
                    // Test's delimiter owns harness arguments, not Cargo overlays.
                    command.args(["--", "--nocapture"]);
                }
                let output = command.output().unwrap();
                assert_success(&output, "source-specific compiler cache");
                if operation == "check" {
                    let paths = stdout(&output)
                        .lines()
                        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                        .filter(|record| {
                            record["reason"] == "compiler-artifact"
                                && matches!(
                                    record["target"]["name"].as_str(),
                                    Some("check-valid-robot" | "source-provider")
                                )
                        })
                        .filter_map(|record| record["executable"].as_str().map(PathBuf::from))
                        .collect::<std::collections::BTreeSet<_>>();
                    assert_eq!(paths.len(), 2);
                    for path in paths {
                        let executed = Command::new(path).output().unwrap();
                        assert_success(&executed, "prepared Check executable");
                        assert_eq!(stdout(&executed).trim(), marker);
                    }
                }
            }
            let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
            command
                .current_dir(root)
                .env("CARGO_TARGET_DIR", shared.path().join("target"))
                .env("PHOXAL_HOME", shared.path().join("home"))
                .args(["build", "--offline", "--output", "publication"]);
            if !configured {
                command.env("CARGO_BUILD_BUILD_DIR", shared.path().join("intermediates"));
            }
            let output = command.output().unwrap();
            assert_success(&output, "shared target build");
            let published = generation(&root.join("publication"));
            let manifest: serde_json::Value =
                serde_json::from_slice(&fs::read(published.join("manifest.json")).unwrap())
                    .unwrap();
            for artifact in manifest["artifacts"].as_array().unwrap() {
                let execution = Command::new(published.join(artifact["path"].as_str().unwrap()))
                    .output()
                    .unwrap();
                assert_success(&execution, "inert selected executable");
                assert_eq!(stdout(&execution).trim(), marker);
                assert_eq!(artifact["runtime"]["period_ms"].as_u64(), Some(period));
            }
            let supervisor = Command::new(published.join("bin/supervisor"))
                .output()
                .unwrap();
            assert_success(&supervisor, "inert supervisor");
            assert_eq!(stdout(&supervisor).trim(), marker);
            if root == &a {
                if let Some(original) = &original_generation {
                    assert_eq!(&published, original);
                    assert!(stdout(&output).contains("Build unchanged:"));
                } else {
                    original_generation = Some(published);
                }
            }
        }
    }
}

/// Resolves the selected generation once, like a reader.
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "acceptance publication assertions"
)]
fn generation(stable: &Path) -> PathBuf {
    let target = fs::read_link(stable).expect("stable root is a symlink");
    stable
        .parent()
        .unwrap()
        .join(target)
        .canonicalize()
        .expect("selected generation")
}

#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "acceptance publication assertions"
)]
fn default_slot(root: &Path) -> PathBuf {
    let query = invoke(root, &["build", "--offline", "--print-output-path"]);
    assert_success(&query, "path query");
    let path = stdout(&query);
    PathBuf::from(path.trim_end())
}

#[test]
fn default_dev_build_publishes_generations_and_reports_unchanged() {
    let (_guard, root) = stage("check-valid");
    let first = build(&root, &[]);
    assert_success(&first, "first build");
    assert!(
        stdout(&first).contains("Build published: "),
        "{}",
        stdout(&first)
    );
    let stable = default_slot(&root);
    assert!(stable.to_string_lossy().contains("/dev/"), "{stable:?}");
    let first_generation = generation(&stable);
    assert!(first_generation.join("manifest.json").is_file());
    let brain = first_generation
        .join("bin")
        .read_dir()
        .unwrap()
        .find(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("check-valid-robot-")
            })
        })
        .unwrap()
        .unwrap()
        .path();
    let mode = fs::metadata(&brain).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "payload executables stay runnable");

    // Zero service/driver participants are selected: the successful build
    // claims only the executable build (brain and supervisor Cargo work),
    // never a participant compilation that does not exist. A local path
    // supervisor always builds through Cargo, warm or cold; only the cached
    // Git selection of its own regression merely selects.
    let first_transcript = stderr(&first);
    assert!(
        first_transcript.contains("Build: building selected executables."),
        "the executable build seam is named: {first_transcript}"
    );
    assert!(
        !first_transcript.contains("compiling selected participants"),
        "a zero-participant robot never claims participant compilation: {first_transcript}"
    );
    assert!(
        first_transcript.contains("Build: building the supervisor."),
        "the local selection owns its actual supervisor build: {first_transcript}"
    );

    let second = build(&root, &[]);
    assert_success(&second, "identical rebuild");
    let second_stdout = stdout(&second);
    assert!(
        second_stdout.contains("Build unchanged: "),
        "{second_stdout}"
    );
    assert!(
        !second_stdout.contains("Build published"),
        "unchanged never also reports published: {second_stdout}"
    );
    let second_transcript = stderr(&second);
    assert!(
        second_transcript.contains("Build: selecting the supervisor."),
        "the supervisor selection seam is named: {second_transcript}"
    );
    assert!(
        second_transcript.contains("Build: building the supervisor."),
        "a warm local selection still builds through Cargo: {second_transcript}"
    );
    let (building, assembling) = (
        second_transcript
            .rfind("Build: building the supervisor.")
            .expect("build claim position"),
        second_transcript
            .rfind("Build: assembling the runnable directory.")
            .expect("assembly claim position"),
    );
    assert!(
        building < assembling,
        "assembly resumes after the local supervisor build: {second_transcript}"
    );
    assert_eq!(generation(&stable), first_generation);
    assert!(
        !stdout(&second).contains("Completed"),
        "no duplicate generic completion footer: {}",
        stdout(&second)
    );
}

#[test]
fn same_name_selected_sources_in_a_shared_target_directory_publish_their_own_bytes() {
    // Two robots author local supervisors with the same package name and
    // version but different bytes, before either builds, and share one
    // explicit Cargo target directory. Cargo's unit fingerprints cannot
    // distinguish such selections, so the tool builds each selected source
    // below its own subdirectory of the resolved target directory.
    let shared = tempfile::tempdir().unwrap();
    let target = shared.path().join("shared-target");
    let build_in = |letter: &str, robot: &Path, home: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(robot)
            .env("PHOXAL_HOME", home)
            .env("CARGO_TARGET_DIR", &target)
            .env_remove("RUSTC")
            .env_remove("CARGO_BUILD_RUSTC")
            .args(["build", "--offline"]);
        let output = command.output().unwrap();
        assert_success(&output, letter);
        output
    };
    let mut robots = Vec::new();
    for letter in ["a", "b"] {
        let (_guard, root) = stage("check-valid");
        let manifest = fs::read_to_string(root.join("Cargo.toml")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            manifest.replace("check-valid-robot", &format!("identity-robot-{letter}")),
        )
        .unwrap();
        let document = fs::read_to_string(root.join("robot.yaml")).unwrap();
        fs::write(
            root.join("robot.yaml"),
            document.replace("check-valid-robot", &format!("identity-robot-{letter}")),
        )
        .unwrap();
        let supervisor = root.join("supervisor/src/main.rs");
        let original = fs::read_to_string(&supervisor).unwrap();
        let replaced = original.replace(
            "println!(\"phoxal-supervisor 0.0.0-dev.8\");",
            &format!("println!(\"SOURCE_{}\");", letter.to_ascii_uppercase()),
        );
        assert_ne!(original, replaced, "fixture supervisor marker located");
        fs::write(&supervisor, replaced).unwrap();
        robots.push((_guard, root.clone(), root.join(".phoxal-home")));
    }
    let (_, robot_a, home_a) = &robots[0];
    let (_, robot_b, home_b) = &robots[1];

    let first = build_in("robot A first build", robot_a, home_a);
    assert!(
        stdout(&first).contains("Build published: "),
        "{}",
        stdout(&first)
    );
    let second = build_in("robot B first build", robot_b, home_b);
    assert!(
        stdout(&second).contains("Build published: "),
        "{}",
        stdout(&second)
    );
    let third = build_in("robot A second build", robot_a, home_a);
    assert!(
        stdout(&third).contains("Build unchanged: "),
        "the same selected source keeps its warm incremental reuse: {}",
        stdout(&third)
    );

    let published = |robot: &Path, home: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(robot)
            .env("PHOXAL_HOME", home)
            .env("CARGO_TARGET_DIR", &target)
            .env_remove("RUSTC")
            .env_remove("CARGO_BUILD_RUSTC")
            .args(["build", "--offline", "--print-output-path"]);
        let output = command.output().unwrap();
        assert_success(&output, "publication path query");
        PathBuf::from(stdout(&output).trim_end())
    };
    let supervisor_bytes = |robot: &Path, home: &Path| {
        let binary = published(robot, home).join("bin/supervisor");
        assert!(binary.is_file(), "published supervisor exists: {binary:?}");
        fs::read(&binary).unwrap()
    };
    let (bytes_a, bytes_b) = (
        supervisor_bytes(robot_a, home_a),
        supervisor_bytes(robot_b, home_b),
    );
    assert_ne!(bytes_a, bytes_b, "distinct sources publish distinct bytes");
    for (robot, home, marker) in [(robot_a, home_a, "SOURCE_A"), (robot_b, home_b, "SOURCE_B")] {
        let supervisor = published(robot, home).join("bin/supervisor");
        let run = Command::new(&supervisor).output().unwrap();
        assert_success(&run, "published supervisor");
        assert_eq!(
            stdout(&run).trim(),
            marker,
            "the published bytes are the robot's own selection"
        );
    }
}

#[test]
fn git_supervisor_cache_reuse_selects_without_claiming_a_build() {
    let (_guard, root) = stage("check-valid");
    // Author a Git supervisor selection from the committed fixture crate
    // (registry dependency and lockfile included).
    let repository = tempfile::tempdir().unwrap();
    let source = repository.path().join("supervisor");
    support::copy_tree(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/robot-base/supervisor"),
        &source,
    )
    .unwrap();
    for arguments in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
            "commit",
            "--quiet",
            "-m",
            "supervisor",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(&arguments)
                .current_dir(&source)
                .status()
                .unwrap()
                .success()
        );
    }
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&source)
        .output()
        .unwrap();
    assert!(revision.status.success());
    let revision = String::from_utf8_lossy(&revision.stdout).trim().to_owned();
    let document = fs::read_to_string(root.join("robot.yaml")).unwrap();
    let git_selection = format!(
        "supervisor:\n  source:\n    git:\n      name: phoxal-supervisor\n      url: file://{}\n      rev: {revision}\n",
        source.display()
    );
    fs::write(
        root.join("robot.yaml"),
        document.replace(
            "supervisor:\n  source:\n    path: supervisor\n",
            &git_selection,
        ),
    )
    .unwrap();

    // The first build acquires the authored selection and owns a real
    // supervisor build; the cached second build only selects it.
    let first = invoke(&root, &["build"]);
    assert_success(&first, "acquiring build");
    assert!(
        stderr(&first).contains("Build: building the supervisor."),
        "acquisition owns an actual supervisor build: {}",
        stderr(&first)
    );

    let second = build(&root, &[]);
    assert_success(&second, "cached build");
    let transcript = stderr(&second);
    assert!(
        transcript.contains("Build: selecting the supervisor."),
        "cached selection is named as selection: {transcript}"
    );
    assert!(
        !transcript.contains("Build: building the supervisor."),
        "a cached supervisor selection never claims a build: {transcript}"
    );
    let (selecting, assembling) = (
        transcript
            .rfind("Build: selecting the supervisor.")
            .expect("selection claim position"),
        transcript
            .rfind("Build: assembling the runnable directory.")
            .expect("assembly claim position"),
    );
    assert!(
        selecting < assembling,
        "assembly resumes after the cached selection: {transcript}"
    );
    assert!(
        stdout(&second).contains("Build unchanged: "),
        "the cached build still reports its outcome: {}",
        stdout(&second)
    );
}

#[test]
fn run_transitions_from_publication_to_the_owned_runtime() {
    let (_guard, root) = stage("check-valid");
    // A finite authored supervisor: it announces its owned runtime, holds
    // the terminal past one liveness boundary, and exits cleanly.
    let supervisor = root.join("supervisor/src/main.rs");
    let original = fs::read_to_string(&supervisor).unwrap();
    let replaced = original.replace(
        "fn main() { std::hint::black_box(&EMBEDDED_APPLICATION_CONTRACT); println!(\"phoxal-supervisor 0.0.0-dev.8\"); }",
        "fn main() { std::hint::black_box(&EMBEDDED_APPLICATION_CONTRACT); println!(\"OWNED_RUNTIME_BEGIN\"); std::thread::sleep(std::time::Duration::from_secs(6)); println!(\"OWNED_RUNTIME_END\"); }",
    );
    assert_ne!(original, replaced, "fixture supervisor main located");
    fs::write(&supervisor, replaced).unwrap();

    let run = invoke(&root, &["run", "--offline"]);
    assert_success(&run, "finite owned runtime");
    let child_stdout = stdout(&run);
    assert!(
        child_stdout.contains("OWNED_RUNTIME_BEGIN") && child_stdout.contains("OWNED_RUNTIME_END"),
        "the owned supervisor's output reaches the terminal: {child_stdout}"
    );
    let transcript = stderr(&run);
    let (publishing, running) = (
        transcript
            .find("Run: publishing.")
            .expect("publication claim position"),
        transcript
            .find("Run: running the robot.")
            .expect("runtime claim position"),
    );
    assert!(
        publishing < running,
        "the runtime claim follows publication: {transcript}"
    );
    assert!(
        transcript.contains("elapsed. (running the robot)"),
        "liveness during the owned runtime names the runtime: {transcript}"
    );
    assert!(
        !transcript.contains("(publishing)"),
        "liveness never claims publication during the owned runtime: {transcript}"
    );
    assert!(
        child_stdout.contains("compiled bundle: "),
        "the complete final path stays reported: {child_stdout}"
    );
}

#[test]
fn operation_wording_matches_the_delegated_operation() {
    let (_guard, root) = stage("check-valid");
    let check = invoke(&root, &["check", "--offline"]);
    assert_success(&check, "check");
    let check_transcript = stderr(&check);
    assert!(
        check_transcript.contains("Check: checking the robot project."),
        "checking owns its own wording: {check_transcript}"
    );
    assert!(
        !check_transcript.contains("compiling the robot project"),
        "checking never claims compilation: {check_transcript}"
    );
    let test = invoke(&root, &["test", "--offline"]);
    assert_success(&test, "test");
    assert!(
        stderr(&test).contains("Test: testing the robot project."),
        "testing owns its own wording: {}",
        stderr(&test)
    );

    // A compile-only request keeps its compile-only meaning at the
    // normalized boundary: the executables are built and nothing runs.
    let compile_only = invoke(&root, &["test", "--offline", "--no-run"]);
    assert_success(&compile_only, "compile-only test");
    let compile_only_transcript = stderr(&compile_only) + &stdout(&compile_only);
    assert!(
        compile_only_transcript.contains("Test: building robot tests."),
        "compile-only test owns its own wording: {compile_only_transcript}"
    );
    assert!(
        !compile_only_transcript.contains("testing the robot project"),
        "compile-only test never claims running: {compile_only_transcript}"
    );
    assert!(
        !compile_only_transcript.contains("Running unittests"),
        "no test harness executes under --no-run: {compile_only_transcript}"
    );
}

#[test]
fn phase_claims_match_the_work_that_actually_runs() {
    let (_guard, root) = stage("check-valid");
    let document = fs::read_to_string(root.join("robot.yaml")).unwrap();
    fs::write(
        root.join("robot.yaml"),
        document.replace("  brain: {}", "  model: model.xml\n  brain: {}"),
    )
    .unwrap();
    fs::remove_file(root.join("model.xml")).unwrap();
    let failed = build(&root, &[]);
    assert!(
        !failed.status.success(),
        "a robot without its authored model cannot build"
    );
    let transcript = stderr(&failed);
    assert!(
        transcript.contains("Build: staging model resources."),
        "the failing staging seam is named: {transcript}"
    );
    assert!(
        !transcript.contains("Build: building selected executables."),
        "no executable-build claim before the failing staging seam: {transcript}"
    );
    assert!(
        !transcript.contains("Build: compiling selected participants."),
        "no participant-compilation claim exists to make: {transcript}"
    );
    assert!(
        !transcript.contains("Build: selecting the supervisor."),
        "no supervisor claim after the failed staging: {transcript}"
    );
}

#[test]
fn changed_payload_publishes_a_new_generation_and_retains_the_old_reader_snapshot() {
    fn snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, (u32, Vec<u8>)> {
        let mut entries = std::collections::BTreeMap::new();
        let mut pending = vec![root.to_owned()];
        while let Some(path) = pending.pop() {
            let metadata = fs::symlink_metadata(&path).unwrap();
            let bytes = if metadata.is_dir() {
                pending.extend(
                    fs::read_dir(&path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
                Vec::new()
            } else if metadata.is_symlink() {
                fs::read_link(&path)
                    .unwrap()
                    .as_os_str()
                    .as_encoded_bytes()
                    .to_vec()
            } else {
                fs::read(&path).unwrap()
            };
            entries.insert(
                path.strip_prefix(root).unwrap().to_owned(),
                (metadata.permissions().mode(), bytes),
            );
        }
        entries
    }
    let brain = |generation: &Path| {
        fs::read_dir(generation.join("bin"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("check-valid-robot-")
            })
            .unwrap()
    };
    let (_guard, root) = stage("check-valid");
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    let anchor = "std::hint::black_box(&PHOXAL_ARTIFACT);";
    assert_eq!(main.matches(anchor).count(), 1);
    let old_source = main.replace(
        anchor,
        &format!("{anchor} println!(\"DIRECTORY_PAYLOAD_OLD\");"),
    );
    fs::write(root.join("src/main.rs"), &old_source).unwrap();
    let first = build(&root, &[]);
    assert_success(&first, "first build");
    let stable = default_slot(&root);
    let first_generation = generation(&stable);
    let first_brain = brain(&first_generation);
    let first_bytes = fs::read(&first_brain).unwrap();
    let first_run = Command::new(&first_brain).output().unwrap();
    assert_success(&first_run, "old inert brain");
    assert_eq!(
        stdout(&first_run),
        "DIRECTORY_PAYLOAD_OLD\n",
        "old brain stdout={} stderr={}",
        stdout(&first_run),
        stderr(&first_run)
    );
    let old_snapshot = snapshot(&first_generation);

    // An observable executable edit changes the payload, not source/slot identity.
    fs::write(
        root.join("src/main.rs"),
        old_source.replace("DIRECTORY_PAYLOAD_OLD", "DIRECTORY_PAYLOAD_NEW"),
    )
    .unwrap();
    let second = build(&root, &[]);
    assert_success(&second, "changed build");
    let diagnostics = format!(
        "changed build stdout:\n{}\nstderr:\n{}",
        stdout(&second),
        stderr(&second)
    );
    assert!(
        stdout(&second).contains("Build published: "),
        "{diagnostics}"
    );
    assert!(
        !stdout(&second).contains("Build unchanged: "),
        "{diagnostics}"
    );
    assert_eq!(
        default_slot(&root),
        stable,
        "payload edits keep the resolved slot: {diagnostics}"
    );
    let second_generation = generation(&stable);
    assert_ne!(second_generation, first_generation, "{diagnostics}");
    let second_brain = brain(&second_generation);
    let second_bytes = fs::read(&second_brain).unwrap();
    assert!(
        second_bytes != first_bytes,
        "selected brain bytes must change: {diagnostics}"
    );
    let second_run = Command::new(&second_brain).output().unwrap();
    assert_success(&second_run, "new inert brain");
    assert_eq!(
        stdout(&second_run),
        "DIRECTORY_PAYLOAD_NEW\n",
        "new brain stdout={} stderr={}; {diagnostics}",
        stdout(&second_run),
        stderr(&second_run)
    );

    // Every old reader entry retains its exact bytes, paths and modes.
    assert!(
        snapshot(&first_generation) == old_snapshot,
        "complete old reader snapshot changed: {diagnostics}"
    );
    let old_reader = Command::new(&first_brain).output().unwrap();
    assert_success(&old_reader, "retained old brain reader");
    assert_eq!(
        stdout(&old_reader),
        "DIRECTORY_PAYLOAD_OLD\n",
        "retained brain stdout={} stderr={}; {diagnostics}",
        stdout(&old_reader),
        stderr(&old_reader)
    );
    use sha2::Digest;
    eprintln!(
        "payload proof: old_brain={first_brain:?} new_brain={second_brain:?} old_sha={:x} new_sha={:x} old_reader_entries={} modes_and_bytes_preserved=true\nold marker={}new marker={}retained old marker={}\nfirst build stdout:\n{}\nstderr:\n{}\n{diagnostics}",
        sha2::Sha256::digest(&first_bytes),
        sha2::Sha256::digest(&second_bytes),
        old_snapshot.len(),
        stdout(&first_run),
        stdout(&second_run),
        stdout(&old_reader),
        stdout(&first),
        stderr(&first)
    );
}

#[test]
fn a_chmod_degraded_payload_is_repaired_not_certified() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let selected = generation(&stable);
    let brain = selected
        .join("bin")
        .read_dir()
        .unwrap()
        .find(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("check-valid-robot-")
            })
        })
        .unwrap()
        .unwrap()
        .path();
    let mut permissions = fs::metadata(&brain).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&brain, permissions).unwrap();

    let repaired = build(&root, &[]);
    assert_success(&repaired, "repair build");
    assert!(
        stdout(&repaired).contains("Build published: "),
        "an invalid payload is never certified unchanged: {}",
        stdout(&repaired)
    );
    let restored = generation(&stable)
        .join("bin")
        .read_dir()
        .unwrap()
        .find(|entry| {
            entry.as_ref().is_ok_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("check-valid-robot-")
            })
        })
        .unwrap()
        .unwrap()
        .path();
    assert_ne!(
        fs::metadata(&restored).unwrap().permissions().mode() & 0o111,
        0,
        "the runnable executable permission is restored"
    );
}

#[test]
fn release_and_custom_profiles_use_their_own_slots() {
    let (_guard, root) = stage("check-valid");
    let release = build(&root, &["--profile", "release"]);
    assert_success(&release, "release build");
    let release_query = invoke(
        &root,
        &[
            "build",
            "--offline",
            "--print-output-path",
            "--profile",
            "release",
        ],
    );
    assert_success(&release_query, "release query");
    assert!(
        PathBuf::from(stdout(&release_query).trim_end())
            .to_string_lossy()
            .contains("/release/"),
        "query agrees with the built profile"
    );

    let missing = build(&root, &["--profile", "does-not-exist"]);
    assert!(
        !missing.status.success(),
        "an unavailable custom profile must fail without fallback"
    );
    let missing_stderr = stderr(&missing);
    assert!(
        missing_stderr.contains("does-not-exist"),
        "{missing_stderr}"
    );
    // The previous successful publication survives the failure.
    assert!(
        generation(&PathBuf::from(stdout(&release_query).trim_end()))
            .join("manifest.json")
            .is_file()
    );
}

#[test]
fn ordered_overlays_and_features_occupy_distinct_slots() {
    let (_guard, root) = stage("check-valid");
    fs::write(
        root.join("overlay.yaml"),
        "schema: phoxal/robot/v0\nrobot: {}\n",
    )
    .unwrap();
    assert_success(&build(&root, &["-r", "robot.yaml"]), "base selection build");
    let base_slot = default_slot(&root);
    assert_success(
        &build(&root, &["-r", "robot.yaml", "-r", "overlay.yaml"]),
        "overlay selection build",
    );
    let overlay_query = invoke(
        &root,
        &[
            "build",
            "--offline",
            "--print-output-path",
            "-r",
            "robot.yaml",
            "-r",
            "overlay.yaml",
        ],
    );
    assert_success(&overlay_query, "overlay query");
    let overlay_slot = PathBuf::from(stdout(&overlay_query).trim_end());
    assert_ne!(
        base_slot, overlay_slot,
        "ordered selections get distinct slots"
    );

    // The query performs no acquisition, preparation, or publication.
    let manifest_before = fs::read(generation(&overlay_slot).join("manifest.json")).unwrap();
    let repeat_query = invoke(
        &root,
        &[
            "build",
            "--offline",
            "--print-output-path",
            "-r",
            "robot.yaml",
            "-r",
            "overlay.yaml",
        ],
    );
    assert_success(&repeat_query, "repeat query");
    assert_eq!(stdout(&repeat_query), stdout(&overlay_query));
    assert_eq!(
        fs::read(generation(&overlay_slot).join("manifest.json")).unwrap(),
        manifest_before,
        "the query never mutates the publication"
    );
}

#[test]
fn configured_and_environment_target_directories_with_spaces_are_respected() {
    let (_guard, root) = stage("check-valid");
    fs::create_dir_all(root.join(".cargo")).unwrap();
    fs::write(
        root.join(".cargo/config.toml"),
        "[build]\ntarget-dir = 'configured target with spaces'\n",
    )
    .unwrap();
    let configured = build(&root, &[]);
    assert_success(&configured, "configured target-dir build");
    let query = invoke(&root, &["build", "--offline", "--print-output-path"]);
    assert_success(&query, "configured target-dir query");
    let slot = PathBuf::from(stdout(&query).trim_end());
    assert!(
        slot.to_string_lossy()
            .contains("configured target with spaces"),
        "{slot:?}"
    );
    assert!(generation(&slot).join("manifest.json").is_file());

    // The environment override wins over the configuration and resolves
    // relative to the same invocation context.
    let environment = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(&root)
        .env("PHOXAL_HOME", root.join(".phoxal-home"))
        .env_remove("CARGO_TARGET_DIR")
        .env("CARGO_TARGET_DIR", "environment target with spaces")
        .args(["build", "--offline", "--print-output-path"])
        .output()
        .unwrap();
    assert_success(&environment, "environment target-dir query");
    let env_slot = PathBuf::from(stdout(&environment).trim_end());
    assert!(
        env_slot
            .to_string_lossy()
            .contains("environment target with spaces"),
        "{env_slot:?}"
    );
}

#[test]
fn explicit_output_and_query_agree_and_unmanaged_destinations_are_refused() {
    let (_guard, root) = stage("check-valid");
    let explicit = root.canonicalize().unwrap().join("chosen bundle");
    let built = build(&root, &["--output", "chosen bundle"]);
    assert_success(&built, "explicit output build");
    assert!(
        stdout(&built).contains(&format!("Build published: {}", explicit.display())),
        "{}",
        stdout(&built)
    );
    assert!(generation(&explicit).join("manifest.json").is_file());
    let query = invoke(
        &root,
        &[
            "build",
            "--offline",
            "--print-output-path",
            "--output",
            "chosen bundle",
        ],
    );
    assert_success(&query, "explicit output query");
    assert_eq!(PathBuf::from(stdout(&query).trim_end()), explicit);

    // A pre-existing unmanaged directory destination is refused, not adopted
    // or deleted.
    let unmanaged = root.join("user directory");
    fs::create_dir(&unmanaged).unwrap();
    fs::write(unmanaged.join("user file"), b"keep me").unwrap();
    let refused = build(&root, &["--output", "user directory"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("refusing unmanaged output"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        fs::read(unmanaged.join("user file")).unwrap(),
        b"keep me",
        "user artifacts are never deleted"
    );

    // A regular file at the destination is refused too.
    let file_destination = root.join("file destination");
    fs::write(&file_destination, b"user file").unwrap();
    let refused_file = build(&root, &["--output", "file destination"]);
    assert!(!refused_file.status.success());
    assert_eq!(fs::read(&file_destination).unwrap(), b"user file");
}

#[test]
fn failures_preserve_the_previous_generation_and_pointer() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let selected = generation(&stable);
    let manifest_before = fs::read(selected.join("manifest.json")).unwrap();

    // An invalid selected overlay fails the attempt without publication.
    fs::write(
        root.join("broken.yaml"),
        "schema: phoxal/robot/v0\nrobot:\n  id: 123\n",
    )
    .unwrap();
    let invalid = invoke(
        &root,
        &[
            "build",
            "--offline",
            "-r",
            "robot.yaml",
            "-r",
            "broken.yaml",
        ],
    );
    assert!(!invalid.status.success());
    assert!(stdout(&invalid).is_empty(), "failure keeps stdout clean");

    // A compiler failure preserves the previous publication.
    fs::write(root.join("src/main.rs"), "fn main() { absent(); }\n").unwrap();
    let compiled_failure = build(&root, &[]);
    assert!(!compiled_failure.status.success());
    let failure_stderr = stderr(&compiled_failure);
    assert!(
        failure_stderr.contains("cannot find function"),
        "the real compiler diagnostic reaches the user: {failure_stderr}"
    );
    assert_eq!(
        fs::read(generation(&stable).join("manifest.json")).unwrap(),
        manifest_before,
        "failures never change the selected generation"
    );
}

#[test]
fn conflicting_managed_writers_are_refused() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let lock_parent = stable.parent().unwrap().join(".phoxal-bundle-locks");
    let lock_path = lock_parent.join("build.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    fs4::FileExt::lock(&lock).unwrap();
    let conflicted = build(&root, &[]);
    fs4::FileExt::unlock(&lock).unwrap();
    assert!(
        !conflicted.status.success(),
        "a conflicting managed writer is refused"
    );
    assert!(
        stderr(&conflicted).contains("another cargo phoxal command"),
        "{}",
        stderr(&conflicted)
    );
}

#[test]
fn json_diagnostics_and_query_channels_stay_separated() {
    let (_guard, root) = stage("check-valid");
    let json_build = invoke(&root, &["build", "--offline", "--message-format", "json"]);
    assert_success(&json_build, "json-mode build");
    assert!(
        stdout(&json_build).contains("compiler-artifact"),
        "compiler records reach stdout in JSON mode"
    );
    assert!(
        stderr(&json_build).contains("Build published: "),
        "status reaches stderr in JSON mode: {}",
        stderr(&json_build)
    );

    let query = invoke(&root, &["build", "--offline", "--print-output-path"]);
    let query_stdout = stdout(&query);
    assert_eq!(
        query_stdout.trim_end().parse::<PathBuf>().unwrap(),
        default_slot(&root),
        "the query prints exactly one absolute path"
    );
    assert!(
        !query_stdout.contains("\n\n") && query_stdout.ends_with('\n'),
        "one path and one newline: {query_stdout:?}"
    );
    assert!(
        !stderr(&query).contains("Build published"),
        "the query never reports a publication"
    );
}

/// One shell wrapper resolving a distinct toolchain identity while
/// forwarding everything else to the real Cargo.
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "fixture wrapper assertions"
)]
fn toolchain_wrapper(directory: &std::path::Path, name: &str, version: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join(name);
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then printf '%s\\n' '{version}'; exit 0; fi\nexec cargo \"$@\"\n"
        ),
    )
    .expect("wrapper source");
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

#[test]
fn corrupted_generation_bytes_are_never_certified_unchanged() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let selected = generation(&stable);
    let manifest = selected.join("manifest.json");
    fs::write(&manifest, b"corrupted bytes").expect("corrupt the owned manifest");

    let rebuilt = build(&root, &[]);
    assert_success(&rebuilt, "rebuild over corrupted generation");
    assert!(
        stdout(&rebuilt).contains("Build published: "),
        "byte corruption is never unchanged: {}",
        stdout(&rebuilt)
    );
    let repaired = generation(&stable);
    assert_ne!(repaired, selected, "repairs install fresh generations");
    let text = fs::read_to_string(repaired.join("manifest.json")).unwrap();
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_ok(),
        "the selected manifest is valid JSON again"
    );
    assert_eq!(
        fs::read(selected.join("manifest.json")).unwrap(),
        b"corrupted bytes",
        "the prior generation path and its contents are retained"
    );
}

#[test]
fn arbitrary_user_links_and_shaped_directories_are_refused_not_replaced() {
    let (_guard, root) = stage("check-valid");
    // An arbitrary symlink destination.
    let user_target = root.join("user-target");
    fs::write(&user_target, b"user data").unwrap();
    let link_destination = root.join("user-link");
    std::os::unix::fs::symlink(&user_target, &link_destination).unwrap();
    let refused = build(&root, &["--output", "user-link"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("refusing unmanaged output"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        fs::read_link(&link_destination).unwrap(),
        user_target,
        "the user's link is untouched"
    );

    // A user directory shaped like a publication is refused too; shape is
    // not ownership.
    let shaped = root.join("user-shaped");
    fs::create_dir_all(shaped.join("bin")).unwrap();
    fs::write(shaped.join("manifest.json"), b"user text").unwrap();
    fs::write(shaped.join("bin/tool"), b"user tool").unwrap();
    let refused_shape = build(&root, &["--output", "user-shaped"]);
    assert!(!refused_shape.status.success());
    assert!(stderr(&refused_shape).contains("refusing unmanaged output"));
    assert_eq!(
        fs::read(shaped.join("manifest.json")).unwrap(),
        b"user text",
        "user data is never moved"
    );
    assert!(shaped.join("bin/tool").is_file());

    // No abandoned staging trees remain after refusals; the publication
    // lock storage beside the output is legitimate managed state.
    for entry in fs::read_dir(&root).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let abandoned = name.starts_with(".phoxal-bundle-") && name != ".phoxal-bundle-locks";
        assert!(!abandoned, "refusal leaves no staging behind: {name}");
    }
}

#[test]
fn a_failed_pointer_swap_preserves_prior_generation_contents_and_cleanup() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let selected = generation(&stable);
    let brain = selected
        .join("bin")
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("check-valid-robot-"))
        })
        .unwrap();
    let manifest_before = fs::read(selected.join("manifest.json")).unwrap();
    let brain_before = fs::read(&brain).unwrap();
    let inode_before = fs::metadata(&brain).unwrap().ino();

    // Degrade the payload so the next build must install a fresh
    // generation, and deterministically block the pointer temp path with an
    // owned directory: the CLI keeps the shell's PID through exec.
    let mut permissions = fs::metadata(&brain).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&brain, permissions).unwrap();
    let degraded_mode = fs::metadata(&brain).unwrap().permissions().mode();
    let pointer_before = fs::read_link(&stable).unwrap();
    let stable_display = stable.display().to_string();
    let script = format!(
        "generations=\"{stable_display}.generations\"; \
         mkdir \"$generations/.pointer-$$\"; \
         exec \"$@\"",
    );
    let failed = Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg("sh")
        .arg(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .arg("build")
        .arg("--offline")
        .current_dir(&root)
        .env("PHOXAL_HOME", root.join(".phoxal-home"))
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .expect("run blocked publication");
    assert!(
        !failed.status.success(),
        "the blocked pointer swap fails: {}",
        String::from_utf8_lossy(&failed.stderr)
    );
    // The prior generation keeps its exact contents, inodes, and modes.
    assert_eq!(
        fs::read(selected.join("manifest.json")).unwrap(),
        manifest_before
    );
    let brain_after = fs::metadata(&brain).unwrap();
    assert_eq!(fs::read(&brain).unwrap(), brain_before);
    assert_eq!(brain_after.ino(), inode_before);
    assert_eq!(
        brain_after.permissions().mode(),
        degraded_mode,
        "publication failure never rewrites prior generations"
    );
    assert_eq!(fs::read_link(&stable).unwrap(), pointer_before);
    // No abandoned command staging remains beside the output; the lock
    // storage is legitimate managed state.
    for entry in fs::read_dir(stable.parent().unwrap()).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let abandoned = name.starts_with(".phoxal-bundle-") && name != ".phoxal-bundle-locks";
        assert!(!abandoned, "failed publication cleans its staging: {name}");
    }
}

#[test]
fn effective_toolchain_identity_separates_slots_in_query_and_build() {
    let (_guard, root) = stage("check-valid");
    let wrapper_a = toolchain_wrapper(&root, "cargo-98", "fake cargo 1.98.0");
    let wrapper_b = toolchain_wrapper(&root, "cargo-99", "fake cargo 1.99.0");
    let query = |cargo: &std::path::Path| {
        let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
            .current_dir(&root)
            .env("PHOXAL_HOME", root.join(".phoxal-home"))
            .env_remove("CARGO_TARGET_DIR")
            .args(["build", "--offline", "--print-output-path", "--cargo"])
            .arg(cargo)
            .output()
            .unwrap();
        assert_success(&output, "toolchain query");
        PathBuf::from(stdout(&output).trim_end())
    };
    let slot_a = query(&wrapper_a);
    let slot_b = query(&wrapper_b);
    assert_ne!(slot_a, slot_b, "distinct toolchains get distinct slots");
    assert_eq!(slot_a, query(&wrapper_a), "the same toolchain is stable");
    // Source-byte edits keep one toolchain's slot stable.
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(root.join("src/main.rs"), format!("{main}\n// edited\n")).unwrap();
    assert_eq!(slot_a, query(&wrapper_a));
}

#[test]
fn an_explicit_output_query_still_resolves_the_selected_cargo() {
    let (_guard, root) = stage("check-valid");
    let failed = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(&root)
        .env("PHOXAL_HOME", root.join(".phoxal-home"))
        .env_remove("CARGO_TARGET_DIR")
        .args([
            "build",
            "--offline",
            "--print-output-path",
            "--output",
            "../explicit-query",
            "--cargo",
            "/not/a/cargo-executable",
        ])
        .output()
        .unwrap();
    assert!(
        !failed.status.success(),
        "a missing selected Cargo fails the explicit query too"
    );
    assert!(stdout(&failed).is_empty(), "failure keeps stdout clean");
}

#[test]
fn the_advised_recovery_command_reproduces_the_actual_selection() {
    let (_guard, root) = stage("check-valid");
    let missing = invoke(
        &root,
        &[
            "build",
            "--offline",
            "-r",
            "robot.yaml",
            "-r",
            "absent-selected.yaml",
        ],
    );
    assert!(!missing.status.success());
    let missing_stderr = stderr(&missing);
    let advised = missing_stderr
        .lines()
        .find_map(|line| line.strip_prefix("Next: "))
        .unwrap_or_else(|| panic!("advice line missing: {missing_stderr}"));
    let command = advised
        .strip_prefix("correct the selected robot files, then inspect the composition with `")
        .and_then(|rest| rest.strip_suffix("`."))
        .unwrap_or_else(|| panic!("advice shape unexpected: {advised}"));
    // Run the exact advised command through a shell from the same root.
    let binary = PathBuf::from(env!("CARGO_BIN_EXE_cargo-phoxal"));
    let binary_directory = binary.parent().unwrap().to_path_buf();
    let path = std::env::join_paths(std::iter::once(binary_directory).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let replayed = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(&root)
        .env("PHOXAL_HOME", root.join(".phoxal-home"))
        .env_remove("CARGO_TARGET_DIR")
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        !replayed.status.success(),
        "the advised command must reproduce the same missing selection: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert!(
        String::from_utf8_lossy(&replayed.stderr).contains("absent-selected.yaml"),
        "the replay fails on the actually missing file"
    );
}

#[test]
fn public_help_and_output_shorthand_are_truthful() {
    let (_guard, root) = stage("check-valid");
    let help = invoke(&root, &["build", "--help"]);
    assert_success(&help, "help");
    let help_text = stdout(&help);
    assert!(
        help_text.contains("-o, --output <DIRECTORY>"),
        "{help_text}"
    );
    assert!(help_text.contains("--profile <PROFILE>"), "{help_text}");
    assert!(help_text.contains("runnable directory"), "{help_text}");
    assert!(!help_text.contains("release mode"), "{help_text}");
    assert!(!help_text.contains("ZIP"), "{help_text}");

    // The shorthand still selects an explicit destination.
    let built = build(&root, &["-o", "shorthand bundle"]);
    assert_success(&built, "shorthand build");
    assert!(stdout(&built).contains("Build published:"));
    assert!(root.join("shorthand bundle").is_symlink());
}

#[test]
fn contention_actions_distinguish_slot_and_project_ownership() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let lock_path = stable
        .parent()
        .unwrap()
        .join(".phoxal-bundle-locks/build.lock");
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    fs4::FileExt::lock(&lock).unwrap();
    let slot_conflict = build(&root, &[]);
    fs4::FileExt::unlock(&lock).unwrap();
    assert!(!slot_conflict.status.success());
    let slot_stderr = stderr(&slot_conflict);
    assert!(
        slot_stderr.contains("let that command exit or stop its watcher, then retry"),
        "slot contention names the owning command's lifetime: {slot_stderr}"
    );

    let project_lock = root.join("target/phoxal/operation.lock");
    let held = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&project_lock)
        .unwrap();
    fs4::FileExt::lock(&held).unwrap();
    let project_conflict = build(&root, &[]);
    fs4::FileExt::unlock(&held).unwrap();
    assert!(!project_conflict.status.success());
    let project_stderr = stderr(&project_conflict);
    assert!(
        project_stderr.contains("owns this project"),
        "project contention names the project owner: {project_stderr}"
    );
    assert!(
        project_stderr.contains("let that operation finish or stop that command, then retry"),
        "project contention advises waiting, stopping the owning command, and retrying: {project_stderr}"
    );
    assert!(
        !project_stderr.to_lowercase().contains("delete"),
        "project contention never suggests deleting locks: {project_stderr}"
    );
}

#[test]
fn an_identical_rebuild_after_repair_reports_unchanged() {
    let (_guard, root) = stage("check-valid");
    assert_success(&build(&root, &[]), "first build");
    let stable = default_slot(&root);
    let selected = generation(&stable);
    let brain = selected
        .join("bin")
        .read_dir()
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("check-valid-robot-"))
        })
        .unwrap();
    let mut permissions = fs::metadata(&brain).unwrap().permissions();
    permissions.set_mode(0o644);
    fs::set_permissions(&brain, permissions).unwrap();
    let repaired = build(&root, &[]);
    assert_success(&repaired, "repair");
    assert!(stdout(&repaired).contains("Build published: "));
    let repaired_generation = generation(&stable);
    assert_ne!(repaired_generation, selected);

    // The next identical build is a verified no-op: no reload, no new
    // generation, pointer stays on the disambiguated repair.
    let noop = build(&root, &[]);
    assert_success(&noop, "no-op");
    assert!(
        stdout(&noop).contains("Build unchanged: "),
        "{}",
        stdout(&noop)
    );
    assert_eq!(generation(&stable), repaired_generation);
}

#[test]
fn compiler_json_records_reach_stdout_for_success_and_failure() {
    let (_guard, root) = stage("check-valid");

    fn compiler_records(stdout: &str) -> (bool, bool, bool) {
        let mut artifact = false;
        let mut message = false;
        let mut finished = false;
        for line in stdout.lines() {
            let value: serde_json::Value = serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("stdout line is JSON: {error}: {line}"));
            match value["reason"].as_str() {
                Some("compiler-artifact") => artifact = true,
                Some("compiler-message") => message = true,
                Some("build-finished") => finished = true,
                _ => {}
            }
        }
        (artifact, message, finished)
    }

    // check success: real records on stdout, status on stderr.
    let checked = invoke(&root, &["check", "--offline", "--message-format", "json"]);
    assert_success(&checked, "json check");
    let (artifact, _message, finished) = compiler_records(&stdout(&checked));
    assert!(artifact, "compiler-artifact records observed");
    assert!(finished, "build-finished observed");

    // check failure: records still on stdout, diagnostic on stderr.
    let main = fs::read_to_string(root.join("src/main.rs")).unwrap();
    fs::write(
        root.join("src/main.rs"),
        "fn main() { json_review_missing_function(); }\n",
    )
    .unwrap();
    let failed = invoke(&root, &["check", "--offline", "--message-format", "json"]);
    fs::write(root.join("src/main.rs"), &main).unwrap();
    assert!(!failed.status.success());
    let failed_stdout = stdout(&failed);
    assert!(!failed_stdout.is_empty(), "failure stdout carries records");
    let (_artifact, message, finished) = compiler_records(&failed_stdout);
    assert!(message, "compiler-message records observed on failure");
    assert!(
        finished,
        "build-finished observed on failure: {failed_stdout}"
    );
    let failed_stderr = stderr(&failed);
    assert!(
        failed_stderr.contains("phoxal-diagnostic"),
        "{failed_stderr}"
    );
    assert!(
        !failed_stdout.contains("phoxal-diagnostic"),
        "project diagnostics stay off the compiler channel"
    );
    // The delivered records are not embedded again in the project error.
    assert!(
        !failed_stderr.contains("compiler-message"),
        "no escaped compiler records in the project error: {failed_stderr}"
    );
    assert!(
        !failed_stderr.contains("build-finished"),
        "no escaped build-finished in the project error: {failed_stderr}"
    );

    // Generic build success: the owning compilation paths forward records.
    let built = invoke(&root, &["build", "--offline", "--message-format", "json"]);
    assert_success(&built, "json build");
    assert!(!stdout(&built).is_empty(), "build stdout has records");
    let (artifact, _message, finished) = compiler_records(&stdout(&built));
    assert!(artifact && finished, "build records observed");
    assert!(stderr(&built).contains("Build published: "));

    // Failure of the generic build keeps the same channels.
    fs::write(
        root.join("src/main.rs"),
        "fn main() { json_review_missing_function(); }\n",
    )
    .unwrap();
    let build_failed = invoke(&root, &["build", "--offline", "--message-format", "json"]);
    fs::write(root.join("src/main.rs"), &main).unwrap();
    assert!(!build_failed.status.success());
    let build_failed_stdout = stdout(&build_failed);
    let (_artifact, message, finished) = compiler_records(&build_failed_stdout);
    assert!(message && finished, "build failure records observed");
    let build_failed_stderr = stderr(&build_failed);
    assert!(build_failed_stderr.contains("phoxal-diagnostic"));
    assert!(
        !build_failed_stderr.contains("compiler-message")
            && !build_failed_stderr.contains("build-finished"),
        "no duplicate escaped records in the build-failure project error: {build_failed_stderr}"
    );

    // Pure outputs stay pure.
    let config = invoke(&root, &["config", "--json"]);
    assert_success(&config, "config json");
    let value: serde_json::Value =
        serde_json::from_str(&stdout(&config)).expect("config stdout is one JSON document");
    assert!(value.get("schema").is_some());
    let query = invoke(&root, &["build", "--offline", "--print-output-path"]);
    let query_stdout = stdout(&query);
    assert!(query_stdout.trim_end().ends_with("/build"));
    assert!(!query_stdout.contains("compiler-artifact"));
}

#[test]
fn cargo_compiler_config_and_env_selections_change_the_slot() {
    // One fixed fixture and invocation context throughout: only the selected
    // configuration or environment differs between queries, so every slot
    // difference below is caused by the compiler identity alone.
    let (_guard, root) = stage("check-valid");
    let install_wrapper = |name: &str, version: &str| {
        let wrapper = root.join(name);
        fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '{version}'; exit 0; fi\nexec rustc \"$@\"\n"
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&wrapper, permissions).unwrap();
        wrapper
    };
    let rustc_98 = install_wrapper("rustc-98", "rustc 1.98.0 (selection)");
    let rustc_97 = install_wrapper("rustc-97", "rustc 1.97.0 (selection)");
    // The default identity: the compiler beside the Cargo running the tests.
    let host_rustc = PathBuf::from(std::env::var_os("CARGO").expect("CARGO under test"))
        .parent()
        .expect("cargo directory")
        .join("rustc");
    assert!(
        host_rustc.is_file(),
        "host rustc beside cargo: {host_rustc:?}"
    );

    let set_configs = |files: &[(&str, String)]| {
        fs::create_dir_all(root.join(".cargo")).unwrap();
        for name in ["config.toml", "config"] {
            let _ = fs::remove_file(root.join(".cargo").join(name));
        }
        for (name, text) in files {
            fs::write(root.join(".cargo").join(name), text).unwrap();
        }
    };
    let config_for = |rustc: &Path| format!("[build]\nrustc = {:?}\n", rustc.display().to_string());
    let query = |context: &str, envs: &[(&str, &Path)]| {
        PathBuf::from({
            let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
            command
                .current_dir(&root)
                .env("PHOXAL_HOME", root.join(".phoxal-home"))
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("RUSTC")
                .env_remove("CARGO_BUILD_RUSTC")
                .args(["build", "--offline", "--print-output-path"]);
            for (name, value) in envs {
                command.env(name, value);
            }
            let output = command.output().unwrap();
            assert_success(&output, context);
            stdout(&output).trim_end().to_owned()
        })
    };

    // Distinct compiler identities separate slots.
    set_configs(&[]);
    let default = query("default query", &[]);
    set_configs(&[("config.toml", config_for(&rustc_98))]);
    let configured_98 = query("configured 1.98 query", &[]);
    assert_ne!(
        default, configured_98,
        "a configured compiler identity separates the slot"
    );
    set_configs(&[("config.toml", config_for(&rustc_97))]);
    let configured_97 = query("configured 1.97 query", &[]);
    assert_ne!(
        configured_97, configured_98,
        "distinct configured identities separate the slot"
    );

    // Equivalent config and environment selections agree.
    set_configs(&[]);
    let env_98 = query("env 1.98 query", &[("CARGO_BUILD_RUSTC", &rustc_98)]);
    assert_eq!(
        configured_98, env_98,
        "the CARGO_BUILD_RUSTC overlay agrees with the same configured identity"
    );
    let rustc_selected = query("RUSTC 1.98 query", &[("RUSTC", &rustc_98)]);
    assert_eq!(
        configured_98, rustc_selected,
        "the RUSTC selection agrees with the same configured identity"
    );

    // Competing environment precedence: RUSTC outranks CARGO_BUILD_RUSTC.
    let competing = query(
        "competing precedence query",
        &[("RUSTC", &host_rustc), ("CARGO_BUILD_RUSTC", &rustc_98)],
    );
    assert_eq!(
        default, competing,
        "RUSTC outranks the CARGO_BUILD_RUSTC overlay"
    );

    // A bare executable name searches PATH instead of joining the robot root.
    set_configs(&[("config.toml", "[build]\nrustc = \"rustc\"\n".to_owned())]);
    let bare = query("bare executable query", &[]);
    assert_eq!(
        default, bare,
        "a bare rustc name resolves through PATH to the default identity"
    );

    // The legacy extensionless configuration wins when both files exist.
    set_configs(&[
        ("config.toml", config_for(&host_rustc)),
        ("config", config_for(&rustc_98)),
    ]);
    let legacy = query("legacy config query", &[]);
    assert_eq!(
        configured_98, legacy,
        "the extensionless configuration outranks config.toml"
    );

    // Source-byte edits stay slot-stable: only selections move the slot.
    set_configs(&[]);
    let source = root.join("src/main.rs");
    let original = fs::read_to_string(&source).unwrap();
    fs::write(&source, format!("{original}// selection stability probe\n")).unwrap();
    assert_eq!(
        default,
        query("source-stability query", &[]),
        "source bytes do not move the slot"
    );
}
#[test]
fn effective_compiler_selection_separates_slots() {
    let (_guard, root) = stage("check-valid");
    // A distinct effective rustc (reported through a wrapper that only
    // answers --version) changes the resolved identity; the query compiles
    // nothing, so the wrapper never builds.
    let wrapper = root.join("rustc-98");
    fs::write(
        &wrapper,
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'rustc 1.98.0 (fake)'; exit 0; fi\nexec rustc \"$@\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&wrapper).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&wrapper, permissions).unwrap();
    let query = |rustc: Option<&std::path::Path>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"));
        command
            .current_dir(&root)
            .env("PHOXAL_HOME", root.join(".phoxal-home"))
            .env_remove("CARGO_TARGET_DIR")
            .args(["build", "--offline", "--print-output-path"]);
        if let Some(rustc) = rustc {
            command.env("RUSTC", rustc);
        } else {
            command.env_remove("RUSTC");
        }
        let output = command.output().unwrap();
        assert_success(&output, "compiler query");
        PathBuf::from(stdout(&output).trim_end())
    };
    let default = query(None);
    let overridden = query(Some(&wrapper));
    assert_ne!(
        default, overridden,
        "an explicit effective rustc selection changes the slot"
    );
    assert_eq!(default, query(None), "the default selection is stable");
}
