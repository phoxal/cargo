// The build.rs is intentionally empty, so no `artifact.rs` is emitted
// into OUT_DIR. The compiled binary therefore has no `.phoxal_art`
// section, and `cargo phoxal check` must surface a missing
// runtime contract.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

fn main() {}
