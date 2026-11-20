//! Soak testing: `YTM_SOAK=<seconds>` skips to the next song that often while
//! playing and logs the process's memory, to measure changes over long runs.

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use slint::{Timer, TimerMode};
use windows_sys::Win32::System::{
    ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
    Threading::GetCurrentProcess,
};

use super::App;

impl App {
    /// The soak timer, held by `main`; none unless `YTM_SOAK` is set.
    pub fn soak(self: &Rc<Self>) -> Option<Timer> {
        let secs = std::env::var("YTM_SOAK").ok()?.parse::<u64>().ok()?;
        if secs == 0 {
            return None;
        }
        let started = Instant::now();
        let songs = Cell::new(0u32);
        let weak = Rc::downgrade(self);
        let timer = Timer::default();
        timer.start(TimerMode::Repeated, Duration::from_secs(secs), move || {
            let Some(app) = weak.upgrade() else { return };
            // Paused, or stopped (as at startup or the queue's end), holds the run.
            if app.player.is_paused() || app.stopped() {
                return;
            }
            songs.set(songs.get() + 1);
            let (private, working) = memory();
            // Warn, so release builds write it to `lily.log` without `YTM_LOG`.
            // MB as Task Manager counts them, of 2^20 bytes.
            log::warn!(
                "soak: song {} at {} s, private {:.1} MB, working set {:.1} MB",
                songs.get(),
                started.elapsed().as_secs(),
                private as f64 / 1048576.0,
                working as f64 / 1048576.0
            );
            app.next(true);
        });
        log::warn!("soak: skipping every {secs} s while playing");
        Some(timer)
    }
}

/// The process's private bytes (commit) and working set.
fn memory() -> (usize, usize) {
    let mut counters = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    // SAFETY: the pseudo-handle needs no closing, and `counters` is a live
    // PROCESS_MEMORY_COUNTERS_EX whose size `cb` gives, which the call accepts
    // in place of the shorter struct.
    let ok = unsafe {
        K32GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            counters.cb,
        )
    };
    if ok == 0 {
        return (0, 0);
    }
    (counters.PrivateUsage, counters.WorkingSetSize)
}
