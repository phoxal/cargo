//! Command-owned feedback; live drawing yields before any permanent output.
#[path = "feedback_terminal.rs"]
mod terminal;
use std::{
    io::{self, Write},
    sync::{Condvar, Mutex},
    time::{Duration, Instant},
};

pub(crate) struct Progress<'a>(&'a Mutex<terminal::Terminal>);
impl Progress<'_> {
    pub(crate) fn suspend(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).suspend();
    }
}
pub(crate) fn run<T>(
    label: &str,
    live_allowed: bool,
    operation: impl FnOnce(&Progress<'_>) -> T,
) -> T {
    let shared = (Mutex::new(false), Condvar::new());
    let started = Instant::now();
    let mut terminal = terminal::Terminal::new();
    if !live_allowed {
        terminal.suspend();
    }
    terminal.line(&format!("{label}: Starting."));
    let terminal = Mutex::new(terminal);
    std::thread::scope(|scope| {
        let owner = scope.spawn(|| {
            let (done, changed) = &shared;
            let mut done = done.lock().unwrap_or_else(|e| e.into_inner());
            let mut next_plain = started + Duration::from_secs(5);
            while !*done {
                drop(done);
                let live = terminal.lock().unwrap_or_else(|e| e.into_inner()).live();
                done = shared.0.lock().unwrap_or_else(|e| e.into_inner());
                if *done {
                    break;
                }
                let interval = if live {
                    Duration::from_millis(200)
                } else {
                    Duration::from_secs(5)
                };
                let (current, _) = changed
                    .wait_timeout(done, interval)
                    .unwrap_or_else(|e| e.into_inner());
                done = current;
                if *done {
                    break;
                }
                drop(done);
                let mut output = terminal.lock().unwrap_or_else(|e| e.into_inner());
                if output.live() {
                    let frame = output.frame();
                    if !output.update(&format!(
                        "{label}: {frame} Still working - {}s elapsed.",
                        started.elapsed().as_secs()
                    )) {
                        output.line(&format!(
                            "{label}: Still working - {}s elapsed.",
                            started.elapsed().as_secs()
                        ));
                        next_plain = Instant::now() + Duration::from_secs(5);
                    }
                } else if Instant::now() >= next_plain {
                    output.line(&format!(
                        "{label}: Still working - {}s elapsed.",
                        started.elapsed().as_secs()
                    ));
                    next_plain = Instant::now() + Duration::from_secs(5);
                }
                drop(output);
                done = shared.0.lock().unwrap_or_else(|e| e.into_inner());
            }
        });
        let finish = Finish(&shared);
        let result = operation(&Progress(&terminal));
        drop(finish);
        let _ = owner.join();
        terminal.lock().unwrap_or_else(|e| e.into_inner()).clear();
        result
    })
}
struct Finish<'a>(&'a (Mutex<bool>, Condvar));
impl Drop for Finish<'_> {
    fn drop(&mut self) {
        *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.0.1.notify_all();
    }
}
pub(crate) fn line(text: &str) {
    let mut stderr = io::stderr().lock();
    let _ = writeln!(stderr, "{text}");
    let _ = stderr.flush();
}
