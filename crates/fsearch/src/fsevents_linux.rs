//! Linux stand-in for the FSEvents stream.
//!
//! There is no persistent, replayable filesystem event log to vend on
//! Linux: inotify wants one watch per directory (hundreds of thousands on
//! a whole disk), and fanotify's filesystem marks want CAP_SYS_ADMIN. So
//! this stream emits nothing past HISTORY_DONE. Freshness instead comes
//! from the apply loop's minute tick, which relists folders whose mtimes
//! moved — the same recovery path macOS takes when FSEvents drops history
//! (`relist_changed` in engine.rs).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

pub const MUST_SCAN_SUBDIRS: u32 = 0x1;
pub const USER_DROPPED: u32 = 0x2;
pub const KERNEL_DROPPED: u32 = 0x4;
pub const HISTORY_DONE: u32 = 0x10;

pub struct Event {
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
    /// Which `watch` delivered it (its `stream` number).
    pub stream: u32,
}

/// An id, not a position: monotonic within the process and ordered across
/// restarts (saves compare `saved > ours`), so it rides the wall clock and
/// steps one per call within a millisecond.
pub fn current_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    NEXT.fetch_max(now, Ordering::Relaxed);
    NEXT.fetch_add(1, Ordering::Relaxed) + 1
}

pub struct Stream {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Open a stream. `since` is advisory — nothing can be replayed — so the
/// only event is HISTORY_DONE, ending the replaying state. The apply
/// loop's periodic `relist_changed` supplies freshness after that.
pub fn watch(_since: u64, _latency: f64, tx: Sender<Vec<Event>>, stream: u32) -> Stream {
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name(format!("fsearch-events-{stream}"))
            .spawn(move || {
                let _ = tx.send(vec![Event {
                    path: b"/".to_vec(),
                    flags: HISTORY_DONE,
                    id: current_id(),
                    stream,
                }]);
                while !stop.load(Ordering::Relaxed) {
                    std::thread::park_timeout(Duration::from_secs(3600));
                }
            })
            .expect("spawn")
    };
    Stream {
        stop,
        thread: Some(thread),
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}
