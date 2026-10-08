#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Phoxal supports Linux and macOS only");

include!(concat!(env!("OUT_DIR"), "/artifact.rs"));

fn main() {
    std::hint::black_box(&PHOXAL_ARTIFACT);
}
