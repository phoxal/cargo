//! Watch and native-probe subprocess capture and bounded cancellation.
//!
//! Other command workflows retain their existing output/exec semantics.
//! Per-attempt inherited capabilities identify supported descendants even
//! after fast reparenting; kernel birth identities qualify every signal.
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read, Seek},
    os::{
        fd::AsRawFd,
        unix::{
            fs::MetadataExt,
            process::{CommandExt, ExitStatusExt},
        },
    },
    process::{Command, Output, Stdio},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();
static ANNOUNCED: AtomicBool = AtomicBool::new(false);
static CLEAN: AtomicBool = AtomicBool::new(true);
static DIAGNOSTICS: std::sync::Mutex<Vec<(Vec<u8>, Vec<u8>)>> = std::sync::Mutex::new(Vec::new());
pub(crate) fn retain_cancelled(stdout: &[u8], stderr: &[u8]) {
    if cancelled() && (!stdout.is_empty() || !stderr.is_empty()) {
        DIAGNOSTICS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((stdout.to_vec(), stderr.to_vec()));
    }
}
pub(crate) fn flush_cancelled() {
    for (stdout, stderr) in DIAGNOSTICS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .drain(..)
    {
        crate::output::bytes(&stdout, false);
        crate::output::bytes(&stderr, true);
    }
}

pub(crate) struct WatchSignals(Vec<signal_hook::SigId>);
impl WatchSignals {
    pub(crate) fn install() -> io::Result<Self> {
        let flag = CANCEL.get_or_init(|| Arc::new(AtomicBool::new(false)));
        flag.store(false, Ordering::SeqCst);
        ANNOUNCED.store(false, Ordering::SeqCst);
        CLEAN.store(true, Ordering::SeqCst);
        let mut ids = Vec::new();
        for signal in [libc::SIGINT, libc::SIGTERM] {
            ids.push(signal_hook::flag::register(signal, Arc::clone(flag))?);
        }
        #[cfg(target_os = "linux")]
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        ACTIVE.store(true, Ordering::SeqCst);
        Ok(Self(ids))
    }
}
impl Drop for WatchSignals {
    fn drop(&mut self) {
        ACTIVE.store(false, Ordering::SeqCst);
        for id in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}
pub(crate) fn cancelled() -> bool {
    ACTIVE.load(Ordering::SeqCst) && CANCEL.get().is_some_and(|flag| flag.load(Ordering::SeqCst))
}
pub(crate) fn clean() -> bool {
    CLEAN.load(Ordering::SeqCst)
}
pub(crate) fn announce_stop() {
    if !ANNOUNCED.swap(true, Ordering::SeqCst) {
        crate::feedback::line("Stopping build watcher...");
    }
}

#[derive(Clone)]
struct Process {
    parent: i32,
    group: i32,
    birth: String,
}
#[cfg(target_os = "macos")]
fn processes() -> io::Result<BTreeMap<i32, Process>> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(io::Error::other(
            "kernel process inventory returned no processes; cannot certify cleanup",
        ));
    }
    let mut pids = vec![0_i32; count as usize + 256];
    let found = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            std::mem::size_of_val(pids.as_slice()) as i32,
        )
    };
    if found <= 0 {
        return Err(io::Error::other(
            "kernel process inventory could not be read; cannot certify cleanup",
        ));
    }
    let mut table = BTreeMap::new();
    for &pid in pids.iter().take(found as usize) {
        if pid <= 0 {
            continue;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let actual = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if actual != size {
            continue;
        } // Exited or inaccessible unrelated process.
        let info = unsafe { info.assume_init() };
        if info.pbi_uid != unsafe { libc::getuid() } {
            continue;
        }
        table.insert(
            pid,
            Process {
                parent: info.pbi_ppid as i32,
                group: info.pbi_pgid as i32,
                birth: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
            },
        );
    }
    if !table.contains_key(&(std::process::id() as i32)) {
        return Err(io::Error::other(
            "kernel process inventory omits owner identity; cannot certify owned subprocess cleanup",
        ));
    }
    Ok(table)
}
#[cfg(target_os = "linux")]
fn processes() -> io::Result<BTreeMap<i32, Process>> {
    let mut table = BTreeMap::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        if !entry
            .metadata()
            .is_ok_and(|m| m.uid() == unsafe { libc::getuid() })
        {
            continue;
        }
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        let Some((_, fields)) = stat.rsplit_once(") ") else {
            continue;
        };
        let fields = fields.split_whitespace().collect::<Vec<_>>();
        if fields.len() < 20 {
            continue;
        }
        if let (Ok(parent), Ok(group)) = (fields[1].parse(), fields[2].parse()) {
            table.insert(
                pid,
                Process {
                    parent,
                    group,
                    birth: fields[19].to_owned(),
                },
            );
        }
    }
    if !table.contains_key(&(std::process::id() as i32)) {
        return Err(io::Error::other(
            "kernel process inventory omits owner identity; cannot certify owned subprocess cleanup",
        ));
    }
    Ok(table)
}
// Two independent inherited markers cover helpers that close inherited file
// descriptors or rebuild their environment. Detaching and erasing both before
// observation is explicitly outside the supported helper contract.
const OWNERSHIP_ENV: &str = "PHOXAL_BUILD_OWNER";
struct Capability {
    _file: File,
    marker: Vec<u8>,
    token: String,
    device: u64,
    inode: u64,
}
impl Capability {
    fn new() -> io::Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix("phoxal-owner-")
            .rand_bytes(32)
            .tempfile()?;
        let token = temporary.path().to_string_lossy().into_owned();
        let file = temporary.into_file(); // Unlinked; cannot certify from a pathname.
        let metadata = file.metadata()?;
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) }
                != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _file: file,
            marker: format!("{OWNERSHIP_ENV}={token}").into_bytes(),
            token,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    fn matches(&self, pid: i32) -> io::Result<bool> {
        let environment = marker_environment(pid, &self.marker);
        if matches!(environment, Ok(true)) {
            return Ok(true);
        }
        let file = marker_file(pid, self);
        if matches!(file, Ok(true)) {
            return Ok(true);
        }
        match (environment, file) {
            (Ok(false), Ok(false)) => Ok(false),
            (Err(e), _) | (_, Err(e)) => Err(e),
            _ => Ok(true),
        }
    }
}
#[cfg(target_os = "macos")]
fn marker_environment(pid: i32, marker: &[u8]) -> io::Result<bool> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut bytes = vec![0_u8; 1024 * 1024];
    let mut size = bytes.len();
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 {
        return vanished(io::Error::last_os_error());
    }
    // Exact unguessable per-attempt value, never just a shared variable name.
    Ok(bytes[..size]
        .split(|&b| b == 0)
        .any(|field| field == marker))
}
#[cfg(target_os = "macos")]
fn marker_file(pid: i32, capability: &Capability) -> io::Result<bool> {
    #[repr(C)]
    struct FileInfo {
        flags: u32,
        status: u32,
        offset: i64,
        kind: i32,
        guards: u32,
    }
    #[repr(C)]
    struct VnodeFdInfo {
        file: FileInfo,
        vnode: libc::vnode_info,
    }
    fn matches(pid: i32, fd: i32, c: &Capability) -> bool {
        let mut info = std::mem::MaybeUninit::<VnodeFdInfo>::zeroed();
        let size = std::mem::size_of::<VnodeFdInfo>() as i32;
        // PROC_PIDFDVNODEINFO=1, defined by the public sys/proc_info.h ABI.
        if unsafe { libc::proc_pidfdinfo(pid, fd, 1, info.as_mut_ptr().cast(), size) } != size {
            return false;
        }
        let info = unsafe { info.assume_init() };
        info.file.status & 2 == 0
            && u64::from(info.vnode.vi_stat.vst_dev) == c.device
            && info.vnode.vi_stat.vst_ino == c.inode
    }
    if matches(pid, capability._file.as_raw_fd(), capability) {
        return Ok(true);
    }
    let size =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if size <= 0 {
        return vanished(io::Error::last_os_error());
    }
    let count = size as usize / std::mem::size_of::<libc::proc_fdinfo>() + 32;
    let mut fds = vec![
        libc::proc_fdinfo {
            proc_fd: 0,
            proc_fdtype: 0
        };
        count
    ];
    let bytes = std::mem::size_of_val(fds.as_slice()) as i32;
    let actual = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDLISTFDS,
            0,
            fds.as_mut_ptr().cast(),
            bytes,
        )
    };
    if actual < 0 {
        return vanished(io::Error::last_os_error());
    }
    Ok(fds
        .iter()
        .take(actual as usize / std::mem::size_of::<libc::proc_fdinfo>())
        .any(|fd| {
            fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32 && matches(pid, fd.proc_fd, capability)
        }))
}
#[cfg(target_os = "linux")]
fn marker_environment(pid: i32, marker: &[u8]) -> io::Result<bool> {
    match std::fs::read(format!("/proc/{pid}/environ")) {
        Ok(bytes) => Ok(bytes.split(|&b| b == 0).any(|field| field == marker)),
        Err(e) => vanished(e),
    }
}
#[cfg(target_os = "linux")]
fn marker_file(pid: i32, capability: &Capability) -> io::Result<bool> {
    let fds = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
        Ok(fds) => fds,
        Err(e) => return vanished(e),
    };
    for fd in fds.filter_map(Result::ok) {
        let same_file = || {
            std::fs::metadata(fd.path())
                .is_ok_and(|m| m.dev() == capability.device && m.ino() == capability.inode)
        };
        if !same_file() {
            continue;
        }
        let info = match std::fs::read_to_string(format!(
            "/proc/{pid}/fdinfo/{}",
            fd.file_name().to_string_lossy()
        )) {
            Ok(info) => info,
            Err(error) if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) => {
                continue;
            }
            Err(error) => return Err(error),
        };
        let flags = info
            .lines()
            .find_map(|line| {
                line.strip_prefix("flags:\t")
                    .and_then(|flags| u64::from_str_radix(flags.trim(), 8).ok())
            })
            .ok_or_else(|| {
                io::Error::other("cannot verify capability descriptor close-on-exec flags")
            })?;
        if flags & libc::O_CLOEXEC as u64 == 0 && same_file() {
            return Ok(true);
        }
    }
    Ok(false)
}
fn vanished(error: io::Error) -> io::Result<bool> {
    if matches!(
        error.raw_os_error(),
        Some(libc::ESRCH | libc::ENOENT | libc::EINVAL)
    ) {
        Ok(false)
    } else {
        Err(error)
    }
}
struct Owner {
    child: std::process::Child,
    group: i32,
    known: BTreeMap<i32, String>,
    done: bool,
    capability: Capability,
    baseline: BTreeMap<i32, String>,
    cleanup_timeout: Duration,
    cleanup_started: Option<Instant>,
}
impl Owner {
    fn observe(&mut self, table: &BTreeMap<i32, Process>) -> io::Result<()> {
        // The unreaped direct Child establishes the first ownership identity.
        if self.child.try_wait()?.is_none()
            && let Some(child) = table.get(&self.group)
        {
            self.known.insert(self.group, child.birth.clone());
        }
        self.follow_ancestry(table);
        for (&pid, process) in table {
            if pid == std::process::id() as i32
                || self.known.contains_key(&pid)
                || self.baseline.get(&pid) == Some(&process.birth)
            {
                continue;
            }
            // A new process can be visible before its exec argument region
            // becomes inspectable. Retry this bounded kernel transition;
            // never infer ownership or signal from a failed inspection.
            let mut ownership = self.capability.matches(pid);
            for _ in 0..5 {
                if ownership.is_ok() {
                    break;
                }
                if !processes()?
                    .get(&pid)
                    .is_some_and(|p| p.birth == process.birth)
                {
                    ownership = Ok(false);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
                ownership = self.capability.matches(pid);
            }
            match ownership {
                Ok(true) => {
                    self.known.insert(pid, process.birth.clone());
                }
                Ok(false) => {}
                Err(e) => {
                    // procargs/fd inspection can race exit; a vanished or
                    // replaced identity cannot retain this capability.
                    if processes()?
                        .get(&pid)
                        .is_some_and(|p| p.birth == process.birth)
                    {
                        return Err(io::Error::other(format!(
                            "cannot verify owned subprocess ownership for PID {pid}: {e}"
                        )));
                    }
                }
            }
        }
        self.follow_ancestry(table);
        Ok(())
    }
    fn follow_ancestry(&mut self, table: &BTreeMap<i32, Process>) {
        let group_alive = self.known.iter().any(|(pid, birth)| {
            table
                .get(pid)
                .is_some_and(|p| &p.birth == birth && p.group == self.group)
        });
        loop {
            let mut added = false;
            for (&pid, process) in table {
                if !self.known.contains_key(&pid)
                    && ((group_alive && process.group == self.group)
                        || self.known.get(&process.parent).is_some_and(|birth| {
                            table
                                .get(&process.parent)
                                .is_some_and(|p| &p.birth == birth)
                        }))
                {
                    self.known.insert(pid, process.birth.clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }
    fn signal(&self, table: &BTreeMap<i32, Process>, signal: i32) {
        // The live direct child owns this group; individual detached PIDs
        // qualify only while their observed birth identity still matches.
        if table
            .get(&self.group)
            .is_some_and(|p| self.known.get(&self.group) == Some(&p.birth))
        {
            unsafe {
                libc::kill(-self.group, signal);
            }
        }
        for (&pid, birth) in &self.known {
            if table.get(&pid).is_some_and(|p| &p.birth == birth) {
                unsafe {
                    libc::kill(pid, signal);
                }
            }
        }
    }
    fn finish(&mut self) -> io::Result<()> {
        let start = *self.cleanup_started.get_or_insert_with(Instant::now);
        let grace = Duration::from_millis(500).min(self.cleanup_timeout / 3);
        loop {
            let table = processes()?;
            #[cfg(target_os = "linux")]
            for (&pid, birth) in self.known.iter().filter(|(pid, _)| **pid != self.group) {
                if !table.get(&pid).is_some_and(|p| &p.birth == birth) {
                    continue;
                }
                // Only adopted known descendants qualify; waitpid refuses
                // every PID that is not this watch owner's child.
                unsafe {
                    libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
            self.observe(&table)?;
            let direct_alive = self.child.try_wait()?.is_none();
            let group_absent = unsafe { libc::kill(-self.group, 0) } == -1
                && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            // An omitted known PID is not proof of absence. Never signal
            // without birth corroboration, and fail instead of certifying
            // cleanup when the kernel can no longer expose a live identity.
            for pid in self.known.keys().filter(|pid| !table.contains_key(pid)) {
                let absent = unsafe { libc::kill(*pid, 0) } == -1
                    && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
                if !absent {
                    return Err(io::Error::other(format!(
                        "cannot verify birth identity or absence of known owned PID {pid}; cleanup is uncertain"
                    )));
                }
            }
            let survivors = self
                .known
                .iter()
                .any(|(pid, birth)| table.get(pid).is_some_and(|p| &p.birth == birth));
            if !direct_alive && !survivors && group_absent {
                self.done = true;
                return Ok(());
            }
            if direct_alive {
                // An unreaped direct Child makes its group ID non-reusable.
                unsafe {
                    libc::kill(
                        -self.group,
                        if start.elapsed() < grace {
                            libc::SIGTERM
                        } else {
                            libc::SIGKILL
                        },
                    );
                }
            }
            let fresh = processes()?;
            self.observe(&fresh)?;
            self.signal(
                &fresh,
                if start.elapsed() < grace {
                    libc::SIGTERM
                } else {
                    libc::SIGKILL
                },
            );
            if start.elapsed() >= self.cleanup_timeout {
                CLEAN.store(false, Ordering::SeqCst);
                return Err(io::Error::other(format!(
                    "owned subprocess cleanup did not finish within {} ms; cleanup is uncertain",
                    self.cleanup_timeout.as_millis()
                )));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if !self.done {
            if self.finish().is_err() {
                CLEAN.store(false, Ordering::SeqCst);
                let _ = self.child.kill();
            }
            let deadline = self.cleanup_started.unwrap_or_else(Instant::now) + self.cleanup_timeout;
            while matches!(self.child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if !matches!(self.child.try_wait(), Ok(Some(_))) {
                CLEAN.store(false, Ordering::SeqCst);
            }
        }
    }
}

/// Captures complete bounded Cargo channels for an active watcher.
/// Ordinary one-shot Cargo and raw Unix simulator exec remain unchanged.
pub(crate) fn output(command: &mut Command) -> io::Result<Output> {
    if !ACTIVE.load(Ordering::SeqCst) {
        return command.output();
    }
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "watch cancelled",
        ));
    }
    let flag = CANCEL.get_or_init(|| Arc::new(AtomicBool::new(false)));
    let (mut output, stopped) = capture(command, flag, None, Duration::from_secs(3), true)?;
    if matches!(stopped, Some(Stop::OutputLimit)) {
        return Err(io::Error::other(
            "watch subprocess exceeded the 128 MiB channel limit; refusing incomplete compiler channels",
        ));
    }
    if stopped.is_some() || cancelled() {
        output.status = std::process::ExitStatus::from_raw(130 << 8);
    }
    Ok(output)
}

// Probe interception is scoped to the capture, without registering persistent
// signal-hook slots. Restoring the exact sigaction preserves prior handlers,
// masks and registry actions, including across subsequent probe lifetimes.
static PROBE_CANCEL: AtomicBool = AtomicBool::new(false);
static PROBE_SIGNAL_OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
extern "C" fn cancel_probe(_: libc::c_int) {
    PROBE_CANCEL.store(true, Ordering::SeqCst);
}
struct ProbeSignals {
    previous: Vec<(libc::c_int, libc::sigaction)>,
    _owner: std::sync::MutexGuard<'static, ()>,
}
impl ProbeSignals {
    fn install() -> io::Result<Self> {
        let owner = PROBE_SIGNAL_OWNER.lock().unwrap_or_else(|e| e.into_inner());
        PROBE_CANCEL.store(false, Ordering::SeqCst);
        let mut signals = Self {
            previous: Vec::new(),
            _owner: owner,
        };
        for signal in [libc::SIGINT, libc::SIGTERM] {
            // SAFETY: sigaction and sigset_t are C structs initialized before
            // use; the handler only writes a process-lifetime atomic flag.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = cancel_probe as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaddset(&mut action.sa_mask, libc::SIGINT);
                libc::sigaddset(&mut action.sa_mask, libc::SIGTERM);
            }
            if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            signals.previous.push((signal, previous));
        }
        #[cfg(target_os = "linux")]
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(signals)
    }
    fn restore(&mut self) -> io::Result<()> {
        while let Some((signal, previous)) = self.previous.last() {
            // SAFETY: this is the exact disposition returned by sigaction
            // when this serialized guard installed its scoped interception.
            if unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            self.previous.pop();
        }
        Ok(())
    }
}
impl Drop for ProbeSignals {
    fn drop(&mut self) {
        // Normal completion checks restoration explicitly, including capture
        // failures. This also restores the first action on partial install.
        let _ = self.restore();
    }
}

/// Native probing shares inherited ownership and bounded capture with Cargo,
/// while applying the simulation request's execution and cleanup budgets.
pub(crate) fn probe_output(
    command: &mut Command,
    execution: Duration,
    cleanup: Duration,
) -> io::Result<Output> {
    let mut signals = ProbeSignals::install()?;
    let captured = capture(command, &PROBE_CANCEL, Some(execution), cleanup, false);
    signals.restore().map_err(|error| {
        io::Error::other(format!(
            "could not restore host signals after simulator probe: {error}"
        ))
    })?;
    let (output, stopped) = captured?;
    if let Some(reason) = stopped.or_else(|| {
        PROBE_CANCEL
            .load(Ordering::SeqCst)
            .then_some(Stop::Cancelled)
    }) {
        let (kind, message) = match reason {
            Stop::Cancelled => (io::ErrorKind::Interrupted, "simulator probe cancelled".to_owned()),
            Stop::Deadline => (io::ErrorKind::TimedOut, format!("simulator probe execution deadline of {} ms expired", execution.as_millis())),
            Stop::OutputLimit => (io::ErrorKind::Other, "simulator probe exceeded the 128 MiB channel limit; captured channels are truncated".to_owned()),
        };
        return Err(io::Error::new(
            kind,
            format!(
                "{message}; owned subprocesses reaped{}",
                captured_diagnostics(&output.stdout, &output.stderr)
            ),
        ));
    }
    Ok(output)
}
#[derive(Clone, Copy)]
enum Stop {
    Cancelled,
    Deadline,
    OutputLimit,
}
const CHANNEL_LIMIT: u64 = 128 * 1024 * 1024;
const DIAGNOSTIC_LIMIT: u64 = 64 * 1024;
fn captured_diagnostics(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::new();
    for (label, bytes) in [("stdout", stdout), ("stderr", stderr)] {
        if !bytes.is_empty() {
            let length = bytes.len().min(DIAGNOSTIC_LIMIT as usize);
            text.push_str(&format!(
                "\nprobe {label}:\n{}",
                String::from_utf8_lossy(&bytes[..length])
            ));
            if length < bytes.len() {
                text.push_str("\n[diagnostic truncated after 64 KiB]");
            }
        }
    }
    text
}
fn captured_bytes(file: &mut File, limit: u64) -> io::Result<Vec<u8>> {
    file.rewind()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}
fn capture(
    command: &mut Command,
    cancelled: &AtomicBool,
    execution: Option<Duration>,
    cleanup: Duration,
    watch: bool,
) -> io::Result<(Output, Option<Stop>)> {
    if cancelled.load(Ordering::SeqCst) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "subprocess cancelled before launch",
        ));
    }
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?));
    let capability = Capability::new()?;
    let baseline = processes()?
        .into_iter()
        .map(|(pid, p)| (pid, p.birth))
        .collect();
    command.env(OWNERSHIP_ENV, &capability.token);
    // Parent descriptors always remain CLOEXEC. Only this explicitly owned
    // child clears the flag, before exec. Keep the descriptor alive in the
    // callback so reusing a Command cannot refer to a closed/reused FD.
    let inherited = capability._file.try_clone()?;
    unsafe {
        command.pre_exec(move || {
            let fd = inherited.as_raw_fd();
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    let started = Instant::now();
    let group = child.id() as i32;
    let mut owner = Owner {
        child,
        group,
        known: BTreeMap::new(),
        done: false,
        capability,
        baseline,
        cleanup_timeout: cleanup,
        cleanup_started: None,
    };
    let result = (|| -> io::Result<_> {
        loop {
            owner.observe(&processes()?)?;
            let stopped = if cancelled.load(Ordering::SeqCst) {
                Some(Stop::Cancelled)
            } else if execution.is_some_and(|duration| started.elapsed() >= duration) {
                Some(Stop::Deadline)
            } else if stdout.metadata()?.len() > CHANNEL_LIMIT
                || stderr.metadata()?.len() > CHANNEL_LIMIT
            {
                Some(Stop::OutputLimit)
            } else {
                None
            };
            if stopped.is_some() {
                if watch && cancelled.load(Ordering::SeqCst) && !crate::feedback::has_progress() {
                    announce_stop();
                }
                owner.finish()?;
                return Ok((owner.child.wait()?, stopped));
            }
            if let Some(status) = owner.child.try_wait()? {
                owner.finish()?;
                return Ok((status, None));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    match result {
        Ok((status, stopped)) => {
            let stopped = if stdout.metadata()?.len() > CHANNEL_LIMIT
                || stderr.metadata()?.len() > CHANNEL_LIMIT
            {
                stopped.or(Some(Stop::OutputLimit))
            } else {
                stopped
            };
            let limit = if matches!(stopped, Some(Stop::OutputLimit)) {
                DIAGNOSTIC_LIMIT + 1
            } else {
                CHANNEL_LIMIT + 1
            };
            Ok((
                Output {
                    status,
                    stdout: captured_bytes(&mut stdout, limit)?,
                    stderr: captured_bytes(&mut stderr, limit)?,
                },
                stopped,
            ))
        }
        Err(error) => {
            // Cleanup failure is never replaced with a timeout/success claim.
            // Owner Drop stays within the same cleanup budget.
            drop(owner);
            if watch {
                return Err(error);
            }
            let stdout = captured_bytes(&mut stdout, DIAGNOSTIC_LIMIT + 1)?;
            let stderr = captured_bytes(&mut stderr, DIAGNOSTIC_LIMIT + 1)?;
            Err(io::Error::new(
                error.kind(),
                format!(
                    "{error}; owned subprocess cleanup could not be certified{}",
                    captured_diagnostics(&stdout, &stderr)
                ),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    static PRIOR_CALLED: AtomicBool = AtomicBool::new(false);
    extern "C" fn prior_handler(_: libc::c_int) {
        PRIOR_CALLED.store(true, Ordering::SeqCst);
    }

    // Signal disposition is process-global, so exercise production captures in
    // isolated copies of this test executable rather than changing test-host
    // signals or leaving a registry action behind in the shared test process.
    #[test]
    fn probe_signal_lifetime_child() {
        let Ok(mode) = std::env::var("PHOXAL_PROBE_SIGNAL_CASE") else {
            return;
        };
        let signal: i32 = std::env::var("PHOXAL_PROBE_SIGNAL_NUMBER")
            .unwrap()
            .parse()
            .unwrap();
        let root = std::path::PathBuf::from(std::env::var_os("PHOXAL_PROBE_SIGNAL_ROOT").unwrap());
        let prior = Arc::new(AtomicBool::new(false));
        let registry = match mode.as_str() {
            "registry" => Some(signal_hook::flag::register(signal, Arc::clone(&prior)).unwrap()),
            "custom" | "ignored" => {
                let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
                action.sa_sigaction = if mode == "ignored" {
                    libc::SIG_IGN
                } else {
                    prior_handler as *const () as usize
                };
                action.sa_flags = libc::SA_RESTART;
                unsafe {
                    libc::sigemptyset(&mut action.sa_mask);
                    libc::sigaddset(&mut action.sa_mask, libc::SIGUSR1);
                    assert_eq!(libc::sigaction(signal, &action, std::ptr::null_mut()), 0);
                }
                None
            }
            "default" => None,
            _ => panic!("unknown signal case"),
        };
        let mut before: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), &mut before) },
            0
        );
        for _ in 0..2 {
            assert!(
                probe_output(
                    &mut Command::new("/usr/bin/true"),
                    Duration::from_secs(3),
                    Duration::from_secs(1)
                )
                .unwrap()
                .status
                .success()
            );
        }
        let child_pid = root.join("cancelled-child.pid");
        let readiness = child_pid.clone();
        let sender = std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(3);
            while !readiness.exists() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(readiness.exists(), "probe child did not become ready");
            assert_eq!(unsafe { libc::kill(std::process::id() as i32, signal) }, 0);
        });
        let error = probe_output(
            Command::new("/bin/sh")
                .arg("-c")
                .arg(r#"echo $$ > "$1"; exec /bin/sleep 20"#)
                .arg("probe")
                .arg(&child_pid),
            Duration::from_secs(5),
            Duration::from_secs(1),
        )
        .unwrap_err();
        sender.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(error.to_string().contains("owned subprocesses reaped"));
        let pid: i32 = std::fs::read_to_string(&child_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        // A subsequent successful probe must reset cancellation and preserve
        // the same previous disposition, including signal-hook's live slot.
        assert!(
            probe_output(
                &mut Command::new("/usr/bin/true"),
                Duration::from_secs(3),
                Duration::from_secs(1)
            )
            .unwrap()
            .status
            .success()
        );
        let mut after: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), &mut after) },
            0
        );
        assert_eq!(before.sa_sigaction, after.sa_sigaction);
        assert_eq!(before.sa_flags, after.sa_flags);
        for number in 1..32 {
            assert_eq!(
                unsafe { libc::sigismember(&before.sa_mask, number) },
                unsafe { libc::sigismember(&after.sa_mask, number) }
            );
        }
        if mode != "default" {
            assert_eq!(unsafe { libc::raise(signal) }, 0);
            if mode == "registry" {
                assert!(prior.load(Ordering::SeqCst));
                prior.store(false, Ordering::SeqCst);
                let second = signal_hook::flag::register(signal, Arc::clone(&prior)).unwrap();
                assert_eq!(unsafe { libc::raise(signal) }, 0);
                assert!(prior.load(Ordering::SeqCst));
                signal_hook::low_level::unregister(second);
            } else if mode == "custom" {
                assert!(PRIOR_CALLED.load(Ordering::SeqCst));
            }
        }
        std::fs::write(
            root.join("ready"),
            format!(
                "pid={} cancelled_child={pid} absent=true\n",
                std::process::id()
            ),
        )
        .unwrap();
        if mode == "default" {
            loop {
                std::thread::park_timeout(Duration::from_secs(60));
            }
        }
        if let Some(id) = registry {
            signal_hook::low_level::unregister(id);
        }
    }

    #[test]
    fn probe_restores_host_signals_across_success_and_cancellation() {
        for signal in [libc::SIGINT, libc::SIGTERM] {
            for mode in ["default", "ignored", "custom", "registry"] {
                let guard = tempfile::tempdir().unwrap();
                let evidence = std::env::var_os("PHOXAL_PROBE_SIGNAL_EVIDENCE_DIR").map(|base| {
                    let path = std::path::PathBuf::from(base).join(format!("{mode}-{signal}"));
                    std::fs::create_dir_all(&path).unwrap();
                    path
                });
                let root = evidence.as_deref().unwrap_or(guard.path());
                let stdout = File::create(root.join("stdout")).unwrap();
                let stderr = File::create(root.join("stderr")).unwrap();
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "owned_process::tests::probe_signal_lifetime_child",
                        "--nocapture",
                    ])
                    .env("PHOXAL_PROBE_SIGNAL_CASE", mode)
                    .env("PHOXAL_PROBE_SIGNAL_NUMBER", signal.to_string())
                    .env("PHOXAL_PROBE_SIGNAL_ROOT", root)
                    .stdout(stdout)
                    .stderr(stderr)
                    .process_group(0);
                std::fs::write(root.join("command"), format!("{command:?}\n")).unwrap();
                let mut child = command.spawn().unwrap();
                let until = Instant::now() + Duration::from_secs(8);
                while !root.join("ready").exists()
                    && child.try_wait().unwrap().is_none()
                    && Instant::now() < until
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                let ready = root.join("ready").exists();
                if ready && mode == "default" {
                    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
                }
                let until = Instant::now() + Duration::from_secs(2);
                let mut status = child.try_wait().unwrap();
                while status.is_none() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                    status = child.try_wait().unwrap();
                }
                let rescue = status.is_none();
                if rescue {
                    unsafe {
                        libc::kill(-(child.id() as i32), libc::SIGKILL);
                    }
                }
                let status = status.unwrap_or_else(|| child.wait().unwrap());
                std::fs::write(
                    root.join("result"),
                    format!("{status:?} ready={ready} harness_rescue={rescue}\n"),
                )
                .unwrap();
                assert!(
                    ready,
                    "{mode}/{signal}: stdout={} stderr={}",
                    std::fs::read_to_string(root.join("stdout")).unwrap(),
                    std::fs::read_to_string(root.join("stderr")).unwrap()
                );
                assert!(!rescue, "host ignored signal after probe: {mode}/{signal}");
                if mode == "default" {
                    assert_eq!(status.signal(), Some(signal));
                } else {
                    assert!(status.success());
                }
                assert_eq!(unsafe { libc::kill(-(child.id() as i32), 0) }, -1);
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
            }
        }
    }

    #[test]
    fn explicitly_inherited_capability_is_observed_without_parent_flag_changes() {
        let capability = super::Capability::new().unwrap();
        let fd = capability._file.as_raw_fd();
        let mut command = Command::new("sleep");
        command.arg("10").env_remove(super::OWNERSHIP_ENV);
        unsafe {
            command.pre_exec(move || {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let matched = capability.matches(child.id() as i32).unwrap();
        let parent_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_ne!(parent_flags & libc::FD_CLOEXEC, 0);
        assert!(
            matched,
            "owned inherited file alone must remain discoverable"
        );
    }

    #[test]
    fn concurrent_capture_cleanup_leaves_unowned_sibling_alive() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let path = ready.clone();
        let owned = std::thread::spawn(move || {
            let mut command = Command::new("sh");
            command
                .args(["-c", "printf ready > \"$1\"; sleep 10", "fixture"])
                .arg(path);
            super::capture(
                &mut command,
                &std::sync::atomic::AtomicBool::new(false),
                Some(Duration::from_millis(500)),
                Duration::from_secs(2),
                false,
            )
            .unwrap()
        });
        let until = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready.exists());
        let mut sibling = Command::new("sleep").arg("10").spawn().unwrap();
        let (_, stop) = owned.join().unwrap();
        let survived = sibling.try_wait().unwrap().is_none();
        if survived {
            sibling.kill().unwrap();
        }
        sibling.wait().unwrap();
        assert!(matches!(stop, Some(super::Stop::Deadline)));
        assert!(survived, "owned cleanup signalled an unowned sibling");
    }

    #[test]
    fn capability_does_not_adopt_an_unowned_sibling_before_or_after_exec() {
        let capability = super::Capability::new().unwrap();
        let mut pipe = [0; 2];
        assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
        let writer = pipe[1];
        let spawn = std::thread::spawn(move || {
            let mut command = Command::new("sleep");
            command.arg("2");
            unsafe {
                command.pre_exec(move || {
                    let pid = libc::getpid();
                    libc::write(writer, (&pid as *const i32).cast(), 4);
                    libc::usleep(500_000);
                    Ok(())
                });
            }
            command.spawn().unwrap()
        });
        let mut pid: i32 = 0;
        assert_eq!(
            unsafe { libc::read(pipe[0], (&mut pid as *mut i32).cast(), 4) },
            4
        );
        let before_exec = capability.matches(pid).unwrap();
        let mut child = spawn.join().unwrap();
        let after_exec = capability.matches(child.id() as i32).unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        assert!(!before_exec, "unowned child adopted in its pre-exec window");
        assert!(!after_exec, "unowned executable inherited capability");
    }

    #[test]
    fn kernel_inventory_retains_current_process_birth_identity() -> std::io::Result<()> {
        let pid = std::process::id() as i32;
        let before = super::processes()?;
        let after = super::processes()?;
        assert_eq!(
            before.get(&pid).map(|p| &p.birth),
            after.get(&pid).map(|p| &p.birth)
        );
        assert!(before.contains_key(&pid));
        Ok(())
    }
}
