// Rust string constants preserve literal paths that shell interpolation cannot.
fn main() {
    use std::{env, fs::OpenOptions, io::Write, os::unix::process::CommandExt, process::Command};
    let arguments: Vec<_> = env::args_os().skip(1).collect();
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG)
        .unwrap();
    writeln!(log, "{:?}", arguments).unwrap();
    drop(log);
    let error = Command::new(REAL_CARGO).args(arguments).exec();
    eprintln!("failed to execute Cargo: {error}");
    std::process::exit(if error.kind() == std::io::ErrorKind::NotFound {
        127
    } else {
        126
    });
}
