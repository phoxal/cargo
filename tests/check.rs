//! Integration tests for `cargo phoxal check`.
//!
//! Each test stages a committed fixture under `tests/fixtures/` into a
//! fresh tempdir, spawns the compiled `cargo-phoxal` binary, and asserts
//! on its exit code and captured output. The integration tests treat the
//! binary as an opaque subprocess the same way a downstream user would;
//! they do not import anything from `cargo_phoxal` or `phoxal`.

mod support;

use std::fs;

use support::stage;

#[test]
fn check_passes_a_valid_robot_and_writes_a_lockfile() {
    let (_guard, root) = stage("check-valid");
    let populate = support::invoke(&root, &["check", "--offline"]);
    assert!(
        populate.status.success(),
        "first check must populate the lockfile:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&populate.stdout),
        String::from_utf8_lossy(&populate.stderr),
    );
    assert!(
        fs::read_to_string(root.join("Cargo.lock"))
            .unwrap_or_else(|error| panic!("read generated Cargo.lock: {error}"))
            .contains("[[package]]"),
        "first check must emit a Cargo.lock at the project root"
    );

    let locked_run = support::invoke(&root, &["check", "--locked", "--offline"]);
    assert!(
        locked_run.status.success(),
        "second check must honour --locked against the populated lockfile:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&locked_run.stdout),
        String::from_utf8_lossy(&locked_run.stderr),
    );
}

#[test]
fn check_validates_the_compiled_brain_contract() {
    let (_guard, root) = stage("check-missing-artifact");
    // The brain's compiled contract is validated by check itself: a brain
    // without an embedded Runtime record fails here, exactly as build
    // would reject it.
    let checked = support::invoke(&root, &["check", "--offline"]);
    assert!(
        !checked.status.success(),
        "check must reject a brain without a compiled Runtime contract"
    );
    let checked_stderr = String::from_utf8_lossy(&checked.stderr);
    assert!(
        checked_stderr.contains("Phoxal Runtime contract"),
        "stderr must mention the missing contract, got:\n{checked_stderr}"
    );
    let output = support::invoke(&root, &["build", "--offline"]);
    assert!(
        !output.status.success(),
        "missing artifact must yield a non-zero exit, got {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Phoxal Runtime contract"),
        "stderr must mention the missing contract, got:\n{stderr}"
    );
}

#[test]
fn check_refuses_locked_mode_when_initialization_is_missing() {
    let (_guard, root) = stage("check-locked-no-init");
    let output = support::invoke(&root, &["check", "--locked", "--offline"]);
    assert!(
        !output.status.success(),
        "locked mode with no Cargo.lock must yield a non-zero exit, got {}",
        output.status
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Cargo.lock") && stderr.contains("--locked"),
        "stderr must identify the missing lockfile and locked mode, got:\n{stderr}"
    );
}

#[test]
#[allow(
    clippy::expect_used,
    reason = "acceptance fixture setup and assertions"
)]
fn independently_selected_participant_keeps_its_version_and_shared_artifact() {
    let (_guard, root) = stage("check-valid");
    let provider = root.join("provider");
    fs::create_dir_all(provider.join("src")).expect("provider directory");
    fs::write(
        provider.join("Cargo.toml"),
        "[workspace]\n[package]\nname = \"selection-provider\"\nversion = \"0.7.0\"\nedition = \"2024\"\n",
    ).expect("provider manifest");
    fs::copy(root.join("build.rs"), provider.join("build.rs")).expect("provider contract");
    fs::copy(root.join("src/main.rs"), provider.join("src/main.rs")).expect("provider main");
    let yaml = fs::read_to_string(root.join("robot.yaml")).expect("robot document");
    fs::write(root.join("robot.yaml"), yaml.replace("services: {}", "services:\n  first: { source: { path: provider } }\n  second: { source: { path: provider } }"))
        .expect("participant selections");
    let bundle = root.join("bundle");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
        .current_dir(&root)
        .env("PHOXAL_HOME", root.join(".phoxal-home"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .args(["build", "--offline", "--output"])
        .arg(&bundle)
        .output()
        .expect("bundle compilation");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(bundle.join("manifest.json")).expect("manifest"))
            .expect("resolved manifest");
    let artifacts = manifest["artifacts"].as_array().expect("artifacts");
    let provider = artifacts
        .iter()
        .find(|artifact| artifact["provenance"]["package"] == "selection-provider")
        .expect("provider provenance");
    assert_eq!(provider["provenance"]["version"], "0.7.0");
    let instances = manifest["instances"].as_array().expect("instances");
    for name in ["first", "second"] {
        let instance = instances
            .iter()
            .find(|instance| instance["id"] == name)
            .expect("selected instance");
        assert_eq!(instance["artifact"], provider["id"]);
    }
    assert_eq!(
        artifacts.len(),
        2,
        "one brain and one shared participant output"
    );
}

#[test]
fn passive_path_component_does_not_require_a_robot_rust_dependency() {
    let (_guard, root) = stage("check-valid");
    let (_component_guard, component) = stage("passive-component");
    let yaml = fs::read_to_string(root.join("robot.yaml")).unwrap();
    fs::create_dir(root.join("caster")).unwrap();
    for name in ["Cargo.toml", "component.yaml", "model.xml"] {
        fs::copy(component.join(name), root.join("caster").join(name)).unwrap();
    }
    let selection =
        "components:\n    caster:\n      source: { path: caster }\n      mount_site: caster_mount";
    fs::write(
        root.join("robot.yaml"),
        yaml.replace("components: {}", selection),
    )
    .unwrap();
    let output = support::invoke(&root, &["check", "--offline"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let built = support::invoke(&root, &["build", "--offline", "--output", "passive-bundle"]);
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("passive-bundle/manifest.json")).unwrap())
            .unwrap();
    assert_eq!(manifest["components"][0]["instance"], "caster");
    assert_eq!(manifest["components"][0]["driver"], false);
    assert_eq!(
        manifest["artifacts"].as_array().unwrap().len(),
        1,
        "passive components do not create executable artifacts"
    );
    assert!(
        !fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .contains("passive-component-fixture")
    );
}

#[path = "support/registry.rs"]
mod registry;

#[test]
fn passive_registry_and_git_selections_reuse_retained_assets_offline()
-> Result<(), Box<dyn std::error::Error>> {
    use sha2::{Digest as _, Sha256};
    use std::process::Command;
    let (_guard, root) = stage("check-valid");
    let (_component_guard, component) = stage("passive-component");
    // The publication carrier is an ordinary Cargo library, not a process.
    let manifest = fs::read_to_string(component.join("Cargo.toml"))?
        .replace("autolib = false", "autolib = true");
    fs::write(component.join("Cargo.toml"), manifest)?;
    fs::create_dir(component.join("src"))?;
    fs::write(component.join("src/lib.rs"), "")?;
    let registry_root = root.join("registry");
    let archive_path = registry_root.join("api/v1/crates/passive-component-fixture/0.1.0/download");
    fs::create_dir_all(archive_path.parent().ok_or("archive parent")?)?;
    let archive = flate2::write::GzEncoder::new(
        fs::File::create(&archive_path)?,
        flate2::Compression::default(),
    );
    let mut tar = tar::Builder::new(archive);
    tar.append_dir_all("passive-component-fixture-0.1.0", &component)?;
    tar.into_inner()?.finish()?;
    let checksum = format!("{:x}", Sha256::digest(fs::read(&archive_path)?));
    fs::create_dir_all(registry_root.join("pa/ss"))?;
    fs::write(
        registry_root.join("pa/ss/passive-component-fixture"),
        format!(
            "{}\n",
            serde_json::json!({"name":"passive-component-fixture","vers":"0.1.0","deps":[],"cksum":checksum,"features":{},"yanked":false})
        ),
    )?;
    let server = registry::RegistryServer::start(&registry_root)?;
    fs::write(
        registry_root.join("config.json"),
        format!(
            "{{\"dl\":\"http://127.0.0.1:{}/api/v1/crates\"}}",
            server.port
        ),
    )?;
    let checkout = root.join("git-source");
    let git_component = checkout.join("component");
    fs::create_dir_all(git_component.join("src"))?;
    for name in ["Cargo.toml", "component.yaml", "model.xml", "src/lib.rs"] {
        fs::copy(component.join(name), git_component.join(name))?;
    }
    for args in [
        vec!["init", "--quiet"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "passive component",
        ],
    ] {
        let output = Command::new("git")
            .current_dir(&checkout)
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let revision = Command::new("git")
        .current_dir(&checkout)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let revision = String::from_utf8(revision.stdout)?.trim().to_owned();
    let yaml = fs::read_to_string(root.join("robot.yaml"))?;
    let selection = format!(
        "components:\n    registry_caster:\n      source: {{ package: {{ name: passive-component-fixture, version: '0.1.0', registry: proof }} }}\n      mount_site: registry_mount\n    git_caster:\n      source: {{ git: {{ name: passive-component-fixture, url: 'file://{}', rev: '{revision}', path: component }} }}\n      mount_site: git_mount",
        checkout.display()
    );
    fs::write(
        root.join("robot.yaml"),
        yaml.replace("components: {}", &selection),
    )?;
    let cargo_home = root.join("cargo-home");
    let run = |offline: bool| -> Result<std::process::Output, std::io::Error> {
        Command::new(env!("CARGO_BIN_EXE_cargo-phoxal"))
            .current_dir(&root)
            .env_remove("CARGO_TARGET_DIR")
            .env("PHOXAL_HOME", root.join(".phoxal-home"))
            .env("CARGO_HOME", &cargo_home)
            .env(
                "CARGO_REGISTRIES_PROOF_INDEX",
                format!("sparse+http://127.0.0.1:{}/", server.port),
            )
            .arg("check")
            .args(offline.then_some("--offline"))
            .output()
    };
    let acquired = run(false)?;
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    // Source-cache loss does not force acquisition once the explicit assets are retained.
    fs::remove_dir_all(&cargo_home)?;
    fs::remove_dir_all(&component)?;
    fs::remove_dir_all(&checkout)?;
    let reused = run(true)?;
    assert!(
        reused.status.success(),
        "{}",
        String::from_utf8_lossy(&reused.stderr)
    );
    let entries = fs::read_dir(root.join(".phoxal-home/packages/passive"))?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("source/model.xml").is_file())
        .count();
    assert_eq!(
        entries, 2,
        "independent registry and Git selections retain their declared assets"
    );
    Ok(())
}

#[test]
fn test_filter_and_no_run_do_not_reach_preparation_builds() {
    let (_guard, root) = stage("check-valid");
    let output = support::invoke(&root, &["test", "named_filter", "--no-run", "--offline"]);
    assert!(
        output.status.success(),
        "the normal filtered test command must prepare and compile successfully:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
