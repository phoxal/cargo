//! Explicit metadata-process fixture, released by its owning test via a socket.
use std::{io::Write, os::unix::net::UnixListener};
fn main() {
    let root = std::env::current_exe().unwrap().parent().unwrap().to_owned();
    let listener = UnixListener::bind(root.join("release.sock")).unwrap();
    std::fs::write(root.join("ready"), std::process::id().to_string()).unwrap();
    let _ = listener.accept().unwrap();
    std::io::stderr().write_all(b"RAW_CHILD\n").unwrap();
    std::process::exit(19);
}
