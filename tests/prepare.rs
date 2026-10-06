use std::fs;
use std::path::Path;
use std::process::Command;

/// The build script of a dependency-free Rust-contract participant whose
/// binary retains one observation endpoint (`__ENDPOINT__`, publishing
/// `proof.registry.v1.Status`) in its artifact sections.
const RUST_PROVIDER_BUILD_TEMPLATE: &str = r##"//! Emits this package's compiled Phoxal artifact frames.
fn main() {
    let runtime = "{\"schema\":\"phoxal/artifact/v0\",\"record\":\"runtime\",\"period_ms\":20,\"timeout_ms\":100,\"init_timeout_ms\":1000,\"config_schema\":{\"type\":\"null\"},\"inputs\":[],\"outputs\":[{\"name\":\"__ENDPOINT__\",\"port\":\"__ENDPOINT__\",\"signature\":{\"endpoint\":\"__ENDPOINT__\",\"service\":\"proof.registry.v1.Provider\",\"method\":\"__ENDPOINT__\",\"shape\":\"observation\",\"request\":\"google.protobuf.Empty\",\"response\":\"proof.registry.v1.Status\",\"retained_latest\":true,\"lease_valid_for_ms\":null},\"max_items\":null,\"max_bytes\":1024,\"max_request_bytes\":null,\"every_steps\":null,\"bootstrap\":false,\"timeout_ms\":null}]}";
    let descriptor: &[u8] = &[10, 84, 10, 23, 112, 114, 111, 111, 102, 46, 114, 101, 103, 105, 115, 116, 114, 121, 46, 118, 49, 46, 112, 114, 111, 116, 111, 18, 17, 112, 114, 111, 111, 102, 46, 114, 101, 103, 105, 115, 116, 114, 121, 46, 118, 49, 34, 30, 10, 6, 83, 116, 97, 116, 117, 115, 18, 20, 10, 5, 114, 101, 97, 100, 121, 24, 1, 32, 1, 40, 8, 82, 5, 114, 101, 97, 100, 121, 98, 6, 112, 114, 111, 116, 111, 51];
    let mut artifact: Vec<u8> = Vec::new();
    artifact.extend(b"PHXART0\n");
    artifact.extend((runtime.len() as u32).to_le_bytes());
    artifact.extend(runtime.as_bytes());
    let mut descriptors: Vec<u8> = Vec::new();
    descriptors.extend(b"PHXDESC1");
    descriptors.extend((descriptor.len() as u64).to_le_bytes());
    descriptors.extend(descriptor);
    let render = |section: &str, bytes: &[u8]| -> String {
        let values: Vec<String> = bytes.iter().map(|byte| byte.to_string()).collect();
        let values = values.join(", ");
        format!(
            "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_{section}\"))]\n#[cfg_attr(target_os = \"linux\", unsafe(link_section = \".phoxal_{section}\"))]\nstatic PHOXAL_SECTION_{STATIC}: [u8; {len}] = [{values}];\n",
            STATIC = section.to_uppercase(),
            len = bytes.len(),
        )
    };
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(
        out_dir.join("artifact.rs"),
        format!(
            "{}\n{}\n",
            render("art", &artifact),
            render("desc", &descriptors)
        ),
    )
    .expect("write artifact.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
"##;

/// Writes a Rust-contract participant package retaining one observation
/// endpoint (`endpoint`) in its compiled artifact sections.
fn write_rust_contract_provider(
    source: &Path,
    package: &str,
    version: &str,
    binary: Option<&str>,
    endpoint: &str,
    cargo_home: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    fs::create_dir_all(source.join("src"))?;
    let bins = binary.map_or_else(String::new, |binary| {
        format!("[[bin]]\nname = {binary:?}\npath = \"src/main.rs\"\n")
    });
    fs::write(
        source.join("Cargo.toml"),
        format!(
            "[package]\nname = {package:?}\nversion = {version:?}\nedition = \"2024\"\nbuild = \"build.rs\"\n{bins}"
        ),
    )?;
    fs::write(
        source.join("build.rs"),
        RUST_PROVIDER_BUILD_TEMPLATE.replace("__ENDPOINT__", endpoint),
    )?;
    fs::write(
        source.join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/artifact.rs\"));\n\nfn main() {}\n",
    )?;
    fs::write(source.join("LICENSE"), "Test fixture only.\n")?;
    let lock = Command::new("cargo")
        .args(["generate-lockfile", "--offline", "--manifest-path"])
        .arg(source.join("Cargo.toml"))
        .env("CARGO_HOME", cargo_home)
        .output()?;
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    Ok(())
}

/// Finds the unique prepared-contract directory whose key contains the
/// given fragment, under a project's configured prepared-input root.
fn find_prepared(project: &std::path::Path, fragment: &str) -> std::io::Result<std::path::PathBuf> {
    let root = phoxal_build::prepared_input_root(project).map_err(std::io::Error::other)?;
    let mut matches = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && name.contains(fragment) {
            matches.push(entry.path());
        }
    }
    let listing = fs::read_dir(&root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(
        matches.len(),
        1,
        "expected exactly one prepared contract matching {fragment:?}; prepared-input root holds {listing:?}"
    );
    Ok(matches.remove(0))
}

#[test]
fn local_participant_prepares_from_its_compiled_artifact() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = tempfile::tempdir()?;
    let robot = directory.path().join("robot");
    let provider = robot.join("provider");
    let home = directory.path().join("phoxal-home");
    let cargo_home = directory.path().join("cargo-home");
    fs::create_dir_all(robot.join("src"))?;
    fs::create_dir_all(&cargo_home)?;
    fs::write(
        robot.join("Cargo.toml"),
        "[package]\nname = \"proof-robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(robot.join("src/main.rs"), "fn main() {}\n")?;
    fs::write(
        robot.join("robot.yaml"),
        "schema: phoxal/robot/v0\nrobot: { id: proof-robot }\nsupervisor:\n  source: { path: supervisor }\nservices:\n  motion:\n    source: { path: provider }\n",
    )?;
    write_rust_contract_provider(
        &provider,
        "proof-provider",
        "0.1.0",
        None,
        "status",
        &cargo_home,
    )?;

    fs::create_dir_all(robot.join(".cargo"))?;
    fs::write(
        robot.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir = {:?}\n",
            directory.path().join("caller-target").to_string_lossy()
        ),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .arg("prepare")
        .arg("--offline")
        .current_dir(&robot)
        .env("CARGO_HOME", &cargo_home)
        .env("PHOXAL_HOME", &home)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoints =
        fs::read_to_string(find_prepared(&robot, "path-provider")?.join("contract.json"))?;
    assert!(
        endpoints.contains("proof.registry.v1.Status"),
        "local preparation extracts the participant's compiled contract: {endpoints}"
    );
    let prepared = find_prepared(&robot, "path-provider")?;
    let contract_path = prepared.join("contract.json");
    let descriptor_path = prepared.join("descriptors.pb");
    let contract_before = fs::read(&contract_path)?;
    let descriptor_before = fs::read(&descriptor_path)?;
    let contract_modified = fs::metadata(&contract_path)?.modified()?;
    let descriptor_modified = fs::metadata(&descriptor_path)?.modified()?;
    // Change executable behavior without changing its interface. Prepared
    // products are compared by their actual contract content, so neither
    // the files nor their publication times change on an implementation edit.
    fs::write(
        provider.join("src/main.rs"),
        "include!(concat!(env!(\"OUT_DIR\"), \"/artifact.rs\"));\nfn main() { println!(\"implementation changed\"); }\n",
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .args(["prepare", "--offline"])
        .current_dir(&robot)
        .env("CARGO_HOME", &cargo_home)
        .env("PHOXAL_HOME", &home)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&contract_path)?, contract_before);
    assert_eq!(fs::read(&descriptor_path)?, descriptor_before);
    assert_eq!(fs::metadata(&contract_path)?.modified()?, contract_modified);
    assert_eq!(
        fs::metadata(&descriptor_path)?.modified()?,
        descriptor_modified
    );
    // A real endpoint change refreshes the contract while preserving the
    // unchanged descriptor closure.
    fs::write(
        provider.join("build.rs"),
        RUST_PROVIDER_BUILD_TEMPLATE.replace("__ENDPOINT__", "changed_status"),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .args(["prepare", "--offline"])
        .current_dir(&robot)
        .env("CARGO_HOME", &cargo_home)
        .env("PHOXAL_HOME", &home)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_ne!(fs::read(&contract_path)?, contract_before);
    assert!(fs::read_to_string(&contract_path)?.contains("changed_status"));
    assert_eq!(fs::read(&descriptor_path)?, descriptor_before);
    assert!(!home.join("packages/local").exists());
    assert!(!fs::read_to_string(robot.join("Cargo.toml"))?.contains("proof-provider"));
    Ok(())
}

fn contains_file(root: &Path, name: &str) -> std::io::Result<bool> {
    if !root.exists() {
        return Ok(false);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if contains_file(&path, name)? {
                return Ok(true);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[test]
fn exact_git_revision_prepares_the_alternate_binary_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let package = "proof-git-provider";
    let version = "0.1.0";
    let source = directory.path().join("source");
    let robot = directory.path().join("robot");
    let cargo_home = directory.path().join("cargo-home");
    let phoxal_home = directory.path().join("phoxal-home");
    fs::create_dir_all(&robot)?;
    fs::create_dir_all(&cargo_home)?;
    write_rust_contract_provider(
        &source,
        package,
        version,
        Some("provider-daemon"),
        "telemetry",
        &cargo_home,
    )?;
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&source)
        .status()?;
    assert!(init.success());
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
            "add",
            ".",
        ])
        .current_dir(&source)
        .status()?;
    assert!(commit.success());
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=Proof",
            "-c",
            "user.email=proof@example.test",
            "commit",
            "--quiet",
            "-m",
            "proof",
        ])
        .current_dir(&source)
        .status()?;
    assert!(commit.success());
    let revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&source)
        .output()?;
    assert!(revision.status.success());
    let revision = String::from_utf8(revision.stdout)?.trim().to_owned();
    fs::write(
        robot.join("Cargo.toml"),
        "[package]\nname = \"proof-robot\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(
        robot.join("robot.yaml"),
        format!(
            "schema: phoxal/robot/v0\nrobot: {{ id: proof-robot }}\nsupervisor:\n  source: {{ path: supervisor }}\nservices:\n  provider:\n    binary: provider-daemon\n    source:\n      git:\n        name: {package}\n        url: file://{}\n        rev: {revision}\n",
            source.display()
        ),
    )?;
    fs::create_dir_all(robot.join(".cargo"))?;
    fs::write(
        robot.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir = {:?}\n",
            directory.path().join("caller-target").to_string_lossy()
        ),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .arg("prepare")
        .current_dir(&robot)
        .env("CARGO_HOME", &cargo_home)
        .env("PHOXAL_HOME", &phoxal_home)
        .env("CARGO_TARGET_DIR", directory.path().join("caller-target"))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoints = fs::read_to_string(
        find_prepared(&robot, &format!("git-{package}@"))?.join("contract.json"),
    )?;
    assert!(
        endpoints.contains("\"telemetry\""),
        "the selected binary's compiled contract names its own endpoint: {endpoints}"
    );
    assert!(contains_file(
        &phoxal_home.join("packages/selections"),
        "provider-daemon"
    )?);
    assert!(!contains_file(
        &phoxal_home.join("packages/selections"),
        "Cargo.toml"
    )?);
    fs::remove_dir_all(phoxal_build::prepared_input_root(&robot)?)?;
    fs::remove_dir_all(cargo_home.join("git"))?;
    let recovered = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .args(["prepare", "--offline"])
        .current_dir(&robot)
        .env("CARGO_HOME", &cargo_home)
        .env("PHOXAL_HOME", &phoxal_home)
        .output()?;
    assert!(
        recovered.status.success(),
        "offline recovery: {}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert!(
        fs::read_to_string(
            find_prepared(&robot, &format!("git-{package}@"))?.join("contract.json")
        )?
        .contains("telemetry")
    );
    // An independent checkout of the same package/version/binary must
    // retain its own compiled contract even when the caller shares a target directory.
    let alternate = directory.path().join("alternate-source");
    let alternate_home = directory.path().join("alternate-cargo-home");
    write_rust_contract_provider(
        &alternate,
        package,
        version,
        Some("provider-daemon"),
        "alternate_telemetry",
        &alternate_home,
    )?;
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
            "alternate",
        ],
    ] {
        assert!(
            Command::new("git")
                .args(arguments)
                .current_dir(&alternate)
                .status()?
                .success()
        );
    }
    let alternate_revision = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(&alternate)
        .output()?;
    assert!(alternate_revision.status.success());
    let alternate_revision = String::from_utf8(alternate_revision.stdout)?
        .trim()
        .to_owned();
    let yaml = fs::read_to_string(robot.join("robot.yaml"))?;
    fs::write(
        robot.join("robot.yaml"),
        yaml.replace(
            &source.display().to_string(),
            &alternate.display().to_string(),
        )
        .replace(&revision, &alternate_revision),
    )?;
    fs::remove_dir_all(phoxal_build::prepared_input_root(&robot)?)?;
    fs::create_dir_all(robot.join(".cargo"))?;
    fs::write(
        robot.join(".cargo/config.toml"),
        format!(
            "[build]\ntarget-dir = {:?}\n",
            directory.path().join("caller-target").to_string_lossy()
        ),
    )?;
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .arg("prepare")
        .current_dir(&robot)
        .env("CARGO_HOME", &alternate_home)
        .env("PHOXAL_HOME", &phoxal_home)
        .env("CARGO_TARGET_DIR", directory.path().join("caller-target"))
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoints = fs::read_to_string(
        find_prepared(&robot, &format!("git-{package}@"))?.join("contract.json"),
    )?;
    assert!(
        endpoints.contains("\"alternate_telemetry\""),
        "the new source's contract must survive a shared caller cache: {endpoints}"
    );
    Ok(())
}
