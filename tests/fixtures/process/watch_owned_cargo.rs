//! Inert Cargo wrapper with explicitly recorded, resistant owned descendants.
use std::os::unix::process::CommandExt;
use std::{fs::OpenOptions, io::Write, path::PathBuf, process::Command, time::Duration};
fn record(root:&std::path::Path) {
    let mut file=OpenOptions::new().create(true).append(true).open(root.join("owned-pids.log")).unwrap();
    file.write_all(format!("{}\n",std::process::id()).as_bytes()).unwrap();
}
fn main() {
    let args=std::env::args().skip(1).collect::<Vec<_>>();
    let root=std::env::current_dir().unwrap();
    if args.first().is_some_and(|a|a.starts_with("--owned-")) {
        unsafe {libc_free_signal();}
        if args[0]=="--owned-detached" {unsafe {setsid();}}
        record(&root);
        if args[0]=="--owned-daemon" {std::fs::write(root.join("fast-ready"),"ready\n").unwrap();}
        if args[0]=="--owned-detached" {
            Command::new(std::env::current_exe().unwrap()).arg("--owned-grandchild").spawn().unwrap();
        }
        loop {std::thread::sleep(Duration::from_secs(60));}
    }
    let gate=root.join("owned-gate");
    let gate_operation=std::fs::read_to_string(&gate).unwrap_or_default();
    if args.first().is_some_and(|a| a==gate_operation.trim()) {
        record(&root);
        Command::new(std::env::current_exe().unwrap()).arg("--owned-child").spawn().unwrap();
        Command::new(std::env::current_exe().unwrap()).arg("--owned-detached").spawn().unwrap();
        unsafe {libc_free_signal();}
        std::fs::write(root.join("owned-ready"),"ready\n").unwrap();
        while gate.exists() {std::thread::sleep(Duration::from_millis(20));}
    }
    let cargo=std::env::var_os("WATCH_REAL_CARGO").map(PathBuf::from).unwrap_or_else(||PathBuf::from("cargo"));
    if gate_operation.trim().starts_with("fast-metadata") && args.first().is_some_and(|a|a=="metadata") {
        std::fs::remove_file(&gate).unwrap();
        let output=Command::new(&cargo).args(&args).output().unwrap();
        std::io::stdout().write_all(&output.stdout).unwrap();
        std::io::stderr().write_all(&output.stderr).unwrap();
        record(&root);
        if gate_operation.contains("drop-env") || gate_operation.contains("drop-all") {unsafe {std::env::remove_var("PHOXAL_BUILD_OWNER");}}
        if gate_operation.contains("drop-fd") || gate_operation.contains("drop-all") {for fd in 3..65536 {unsafe {close(fd);}}}
        unsafe {
            let child=fork();
            if child==0 {
                setsid();
                let grandchild=fork();
                if grandchild>0 {_exit(0);}
                if gate_operation.contains("no-exec") {
                    libc_free_signal();record(&root);std::fs::write(root.join("fast-ready"),"ready\n").unwrap();
                    loop {sleep(60);}
                }
                let error=Command::new(std::env::current_exe().unwrap()).arg("--owned-daemon").exec();
                panic!("owned daemon exec failed: {error}");
            }
        }
        std::process::exit(output.status.code().unwrap_or(1));
    }
    let status=Command::new(cargo).args(args).status().unwrap();
    std::process::exit(status.code().unwrap_or(1));
}
unsafe extern "C" {fn signal(sig:i32,handler:usize)->usize;fn setsid()->i32;fn fork()->i32;fn _exit(status:i32)->!;fn close(fd:i32)->i32;fn sleep(seconds:u32)->u32;}
unsafe fn libc_free_signal() {
    // SIGINT=2 and SIGTERM=15 on both supported host platforms; SIG_IGN=1.
    unsafe {signal(2,1);signal(15,1);}
}
