//! Emits a minimal Phoxal artifact frame into the brain binary.

fn main() {
    let payload = "{\"schema\":\"phoxal/artifact/v0\",\"record\":\"runtime\",\"period_ms\":10,\"timeout_ms\":20,\"init_timeout_ms\":30,\"config_schema\":{\"type\":\"null\"},\"inputs\":[],\"outputs\":[]}";
    let length = payload.len() as u32;
    let mut frame: Vec<u8> = Vec::new();
    frame.extend(b"PHXART0\n");
    frame.extend(length.to_le_bytes());
    frame.extend(payload.as_bytes());
    let total = frame.len();
    let bytes: Vec<String> = frame.iter().map(|byte| byte.to_string()).collect();
    let bytes = bytes.join(", ");
    let artifact = format!(
        "#[used]\n#[cfg_attr(target_os = \"macos\", unsafe(link_section = \"__DATA,__phoxal_art\"))]\n#[cfg_attr(target_os = \"linux\", unsafe(link_section = \".phoxal_art\"))]\nstatic PHOXAL_ARTIFACT: [u8; {total}] = [{bytes}];\n"
    );
    let out_dir = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    std::fs::write(out_dir.join("artifact.rs"), artifact).expect("write artifact.rs");
    println!("cargo:rerun-if-changed=build.rs");
}
