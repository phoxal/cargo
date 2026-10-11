//! Command-owned feedback; live drawing yields before any permanent output.
#[path = "feedback_terminal.rs"]
mod terminal;
use std::{
    io::{self, Write},
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

/// Live renderer state shared with the ticker thread: completion and the
/// owning phase recorded at the real workflow boundaries.
static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
pub(crate) fn has_progress() -> bool {
    ACTIVE.load(std::sync::atomic::Ordering::SeqCst) != 0
}
struct ProgressOwner;
impl Drop for ProgressOwner {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}
struct Live {
    done: bool,
    phase: Option<String>,
}

pub(crate) struct Progress<'a> {
    label: &'a str,
    live: &'a Mutex<Live>,
    terminal: &'a Mutex<terminal::Terminal>,
}
impl Progress<'_> {
    /// Stops the live renderer, keeping later output permanent.
    pub(crate) fn suspend(&self) {
        self.terminal
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .suspend();
    }
    /// Records entering one owning workflow phase.
    ///
    /// One narrow permanent line names the actual activity, and later
    /// liveness lines carry the phase until the next boundary. Phases stay
    /// bounded: one claim per real resolution, preparation, compilation,
    /// assembly, or publication seam, never invented progress.
    pub(crate) fn phase(&self, phase: &str) {
        // The permanent boundary line is the visible claim; the liveness
        // ticker picks the phase up on its own bounded cadence.
        self.live.lock().unwrap_or_else(|e| e.into_inner()).phase = Some(phase.to_owned());
        let mut terminal = self.terminal.lock().unwrap_or_else(|e| e.into_inner());
        terminal.line(&format!("{}: {}.", self.label, phase));
    }
}
impl crate::project::PhaseReporter for Progress<'_> {
    fn phase(&self, phase: &str) {
        // Inherent method, spelled fully qualified against the same-named
        // trait method.
        Progress::phase(self, phase)
    }
}
pub(crate) fn run<T>(
    label: &str,
    live_allowed: bool,
    operation: impl FnOnce(&Progress<'_>) -> T,
) -> T {
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _progress_owner = ProgressOwner;
    let shared = (
        Mutex::new(Live {
            done: false,
            phase: None,
        }),
        Condvar::new(),
    );
    let started = Instant::now();
    let mut terminal = terminal::Terminal::new();
    if !live_allowed {
        terminal.suspend();
    }
    terminal.line(&format!("{label}: Starting."));
    let terminal = Mutex::new(terminal);
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| {
            let (live_state, changed) = &shared;
            let mut live = live_state.lock().unwrap_or_else(|e| e.into_inner());
            let mut next_plain = started + Duration::from_secs(5);
            while !live.done {
                drop(live);
                let terminal_live = terminal.lock().unwrap_or_else(|e| e.into_inner()).live();
                live = live_state.lock().unwrap_or_else(|e| e.into_inner());
                if live.done {
                    break;
                }
                let interval = if crate::owned_process::active() {
                    Duration::from_millis(50)
                } else if terminal_live {
                    Duration::from_millis(200)
                } else {
                    Duration::from_secs(5)
                };
                let (current, _) = changed
                    .wait_timeout(live, interval)
                    .unwrap_or_else(|e| e.into_inner());
                live = current;
                if live.done {
                    break;
                }
                let phase = live.phase.clone();
                drop(live);
                let mut output = terminal.lock().unwrap_or_else(|e| e.into_inner());
                if crate::owned_process::cancelled() {
                    output.suspend();
                    crate::owned_process::announce_stop();
                    drop(output);
                    live = live_state.lock().unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let elapsed = started.elapsed().as_secs();
                if output.live() {
                    let frame = output.frame();
                    let suffix = phase
                        .as_deref()
                        .map_or(String::new(), |phase| format!(" ({phase})"));
                    if !output.update(&format!(
                        "{label}: {frame} Still working - {elapsed}s elapsed.{suffix}"
                    )) {
                        output.line(&format!(
                            "{label}: Still working - {elapsed}s elapsed.{suffix}"
                        ));
                        next_plain = Instant::now() + Duration::from_secs(5);
                    }
                } else if Instant::now() >= next_plain {
                    let suffix = phase
                        .as_deref()
                        .map_or(String::new(), |phase| format!(" ({phase})"));
                    output.line(&format!(
                        "{label}: Still working - {elapsed}s elapsed.{suffix}"
                    ));
                    next_plain = Instant::now() + Duration::from_secs(5);
                }
                drop(output);
                live = live_state.lock().unwrap_or_else(|e| e.into_inner());
            }
        });
        let finish = Finish(&shared);
        let result = operation(&Progress {
            label,
            live: &shared.0,
            terminal: &terminal,
        });
        drop(finish);
        let _ = owner.join();
        terminal.lock().unwrap_or_else(|e| e.into_inner()).clear();
        result
    })
}
struct Finish<'a>(&'a (Mutex<Live>, Condvar));
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).done = true;
        self.0.1.notify_all();
    }
}
pub(crate) fn line(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{text}");
    let _ = stderr.flush();
}
