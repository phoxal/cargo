//! Test-owned child process whose working directory is deleted before the
//! real binary runs, reproducing an inherited deleted invocation directory.

use std::{fs, path::PathBuf, process::Command};

fn main() {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    let evidence = PathBuf::from(&arguments[0]);
    let binary = arguments[1].clone();
    let forwarded = &arguments[2..];
    let cwd = evidence.join("deleted-empty-cwd");
    fs::create_dir(&cwd).expect("create the doomed directory");
    std::env::set_current_dir(&cwd).expect("enter the doomed directory");
    fs::remove_dir(&cwd).expect("delete the working directory");
    let output = Command::new(&binary).args(forwarded).output().expect("run");
    fs::write(
        evidence.join("status"),
        format!("{}\n", output.status.code().unwrap_or(-1)),
    )
    .expect("status");
    fs::write(evidence.join("stdout"), output.stdout).expect("stdout");
    fs::write(evidence.join("stderr"), output.stderr).expect("stderr");
}
