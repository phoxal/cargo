// Simulator process-output fixture; no native/control behavior.
use std::io::Write;
fn main() {
    std::io::stdout().write_all(b"{\"raw\":true}\n\xff\x1b[31m\n").unwrap();
    std::io::stderr().write_all(b"Next: child-owned\n\xfe\x1b[0m\n").unwrap();
    std::process::exit(7);
}
