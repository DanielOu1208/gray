//! The engine: owns the live name index, follows FSEvents, keeps the content
//! index current, and answers searches. The daemon runs one; so can any app
//! that links this crate.

use crate::content::{self, Content, Grep, GrepResult};
use crate::fsevents::{self, HISTORY_DONE, KERNEL_DROPPED, MUST_SCAN_SUBDIRS, USER_DROPPED};
use crate::index::Index;
use crate::live::{Applied, Live};
use crate::query::{Query, Searcher};
use crate::walk::{self, KIND_DIR, KIND_FILE, Listing, NONE, RawEnt};
use std::collections::HashMap;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, RwLock, RwLockReadGuard};
use std::time::{Duration, Instant};

// Every search scans the whole overlay (~1 ms per 100k entries), and a
// follower starting up replays everything since the save, so fold it into
// the base once it grows: ~1 s of CPU and a ~280 MB write, about hourly on a
// busy disk. Otherwise only twice a day; restart replays FSEvents anyway.
const COMPACT_PENDING: usize = 50_000;
const COMPACT_EVERY: Duration = Duration::from_secs(12 * 3600);
const SCAN_THREADS: usize = 8;
/// Matches a pruning query may return before its answer is called partial.
const MATCH_CAP: usize = 1 << 20;
const CONTENT_QUIET: Duration = Duration::from_secs(2);
const CONTENT_MAX_WAIT: Duration = Duration::from_secs(300);
/// How often a follower checks whether it can take over or reload.
const FOLLOW_EVERY: Duration = Duration::from_secs(10);
/// Seconds before the last known-good moment that a relist also covers.
const SYNC_MARGIN: u32 = 120;
/// How long FSEvents must go without dropping events before the gap is
/// replayed: restarting mid-burst only drops more.
const REPLAY_QUIET: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct Options {
    /// Where the index lives (`index.bin`, `content/`).
    pub dir: PathBuf,
    pub home: String,
    /// The tree to index; `None` scans `/`. `in:` scopes narrow inside it.
    pub root: Option<PathBuf>,
    /// Folders never to open. `None` decides from Full Disk Access: without
    /// it, the consent-gated folders are skipped, since opening one pops a
    /// privacy prompt and blocks until someone answers it.
    pub skip: Option<Vec<PathBuf>>,
}

/// One name-search result.
pub struct Found {
    pub path: PathBuf,
    /// `walk::KIND_*` in the low 2 bits, `walk::FLAG_*` above.
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    pub score: i32,
}

#[derive(serde::Serialize)]
pub struct Status {
    #[serde(skip)]
    pub ready: bool,
    pub entries: usize,
    pub dirs: usize,
    pub overlay: usize,
    pub removed: usize,
    pub event_id: u64,
    pub index_bytes: usize,
    pub content_docs: usize,
    pub content_segments: usize,
    pub content_bytes: usize,
    pub content_pending: usize,
    /// The scanned root is gone (unmounted): the tree is held offline until
    /// it reappears, which triggers a rescan.
    pub root_missing: bool,
    pub full_disk_access: bool,
    /// Writes the index files (false: following another process's).
    pub owner: bool,
}

/// A folder listed on its own on the first run (see `Shared::with_names`).
struct Early {
    dir: Vec<u8>,
    live: Live,
    /// Its files' content build was asked for.
    indexing: AtomicBool,
}

#[derive(Clone)]
pub struct Engine {
    s: Arc<Shared>,
}

struct Shared {
    live: RwLock<Option<Live>>,
    /// A saved index is still being read in: searches wait for it rather
    /// than answer "indexing".
    loading: (Mutex<bool>, Condvar),
    content: RwLock<Content>,
    home: String,
    /// The directory the index hangs under (`/` unless `Options.root`
    /// narrowed it). Absolute; listings join names onto it.
    root: Vec<u8>,
    dir: PathBuf,
    /// Wakes the apply loop; an empty batch is a no-op wake-up.
    wake: Sender<Vec<fsevents::Event>>,
    save_requested: AtomicBool,
    content_tx: Sender<Resync>,
    content_rx: Mutex<Option<Receiver<Resync>>>,
    content_pending: AtomicUsize,
    /// Files changed since their folder's last content sync.
    dirty: content::Dirty,
    /// Holding `lock`: this engine writes the index files. Another process
    /// may own them (the daemon, an app); then this one follows: it reads
    /// the saved index, keeps it live in memory, and takes over when the
    /// owner goes away.
    owner: AtomicBool,
    lock: std::fs::File,
    stream: Mutex<Option<fsevents::Stream>>,
    /// Number of the current stream (events carry the one they came from)
    /// and the event id it started from.
    streams: std::sync::atomic::AtomicU32,
    since: AtomicU64,
    /// The stream is still replaying history (until HISTORY_DONE).
    replaying: AtomicBool,
    /// Follower: content dir mtime when its segments were last opened.
    content_seen: Mutex<Option<std::time::SystemTime>>,
    /// First run, until the whole disk is listed: indexes of just the
    /// folders searches asked about (see `with_names`).
    early: Mutex<Vec<Arc<Early>>>,
    /// The content index holds every file it should: false while its first
    /// build runs, when greps read the files directly instead...
    content_ready: AtomicBool,
    /// ...except in these folders, built first from their early indexes.
    content_early: Mutex<Vec<Vec<u8>>>,
    /// Told the folders of each batch of disk changes (`Engine::on_change`).
    #[allow(clippy::type_complexity)]
    on_change: RwLock<Vec<Box<dyn Fn(&[(&[u8], bool)]) + Send + Sync>>>,
}

fn try_lock(f: &std::fs::File) -> bool {
    unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(f), libc::LOCK_EX | libc::LOCK_NB) == 0 }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("{} {}", crate::query::now_secs(), msg.as_ref());
}

/// Never let indexing download iCloud placeholders: on the calling thread,
/// opening or listing a dataless file fails fast instead of materializing it.
#[cfg(target_os = "macos")]
pub fn no_materialize() {
    unsafe extern "C" {
        fn setiopolicy_np(iotype: i32, scope: i32, policy: i32) -> i32;
    }
    // IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES, IOPOL_SCOPE_THREAD, OFF
    unsafe { setiopolicy_np(3, 1, 1) };
}

/// No dataless files to dodge elsewhere.
#[cfg(not(target_os = "macos"))]
pub fn no_materialize() {}

/// Thread QoS classes: macOS only. Linux has no per-thread QoS worth
/// spending a syscall on — nice levels want privileges — so calls are
/// no-ops there.
#[cfg(target_os = "macos")]
type Qos = libc::qos_class_t;
#[cfg(not(target_os = "macos"))]
type Qos = u32;
#[cfg(target_os = "macos")]
const QOS_INTERACTIVE: Qos = libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE;
#[cfg(target_os = "macos")]
const QOS_UTILITY: Qos = libc::qos_class_t::QOS_CLASS_UTILITY;
#[cfg(target_os = "macos")]
const QOS_INITIATED: Qos = libc::qos_class_t::QOS_CLASS_USER_INITIATED;
#[cfg(not(target_os = "macos"))]
const QOS_INTERACTIVE: Qos = 0;
#[cfg(not(target_os = "macos"))]
const QOS_UTILITY: Qos = 0;
#[cfg(not(target_os = "macos"))]
const QOS_INITIATED: Qos = 0;

fn set_qos(#[cfg_attr(not(target_os = "macos"), allow(unused))] qos: Qos) {
    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(qos, 0);
    }
}

/// Searches run here, at user-interactive QoS: an app's background executor
/// (or any low-QoS caller) would otherwise put the scan on efficiency cores.
/// Each search gets it here, which notes when (`LAST_SEARCH`).
fn search_pool() -> &'static rayon::ThreadPool {
    LAST_SEARCH.store(now_ns(), Ordering::Relaxed);
    pool()
}

/// When a search last started (`now_ns`): a minute on, the search threads'
/// kept blocks go back (`alloc::drain`) if the warmer didn't send them.
static LAST_SEARCH: AtomicU64 = AtomicU64::new(0);

fn pool() -> &'static rayon::ThreadPool {
    static POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .thread_name(|i| format!("fsearch-search-{i}"))
            .start_handler(|_| {
                set_qos(QOS_INTERACTIVE);
                crate::alloc::keep_on_this_thread();
            })
            .build()
            .unwrap()
    })
}

/// The keep-warm window: spin from `WARM_FROM` to `WARM_UNTIL` (`now_ns`).
static WARM_FROM: AtomicU64 = AtomicU64::new(0);
static WARM_UNTIL: AtomicU64 = AtomicU64::new(0);
/// The warmer is parked until the next `keep_warm`.
static WARM_IDLE: AtomicBool = AtomicBool::new(false);

fn now_ns() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

fn warmer() -> &'static std::thread::Thread {
    static T: std::sync::OnceLock<std::thread::Thread> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let t = std::thread::Builder::new().name("fsearch-warm".into()).spawn(|| {
            set_qos(QOS_INTERACTIVE);
            loop {
                let (from, until, now) = (WARM_FROM.load(Ordering::SeqCst), WARM_UNTIL.load(Ordering::SeqCst), now_ns());
                if now >= until {
                    WARM_IDLE.store(true, Ordering::SeqCst);
                    if now_ns() >= WARM_UNTIL.load(Ordering::SeqCst) {
                        // Searching stopped: the blocks the search threads
                        // kept go back (`alloc`).
                        if crate::alloc::kept() {
                            pool().spawn_broadcast(|_| crate::alloc::drain());
                        }
                        std::thread::park();
                    }
                    WARM_IDLE.store(false, Ordering::SeqCst);
                } else if now < from {
                    std::thread::park_timeout(Duration::from_nanos(from - now));
                } else {
                    std::hint::spin_loop();
                }
            }
        });
        t.expect("spawn").thread().clone()
    })
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            no_materialize();
            f()
        })
        .expect("spawn");
}

impl Engine {
    /// Start indexing in the background and return at once. Searches wait
    /// while a saved index loads, and answer `Err` while the very first run
    /// builds one.
    pub fn start(opts: Options) -> Result<Engine, String> {
        std::fs::create_dir_all(&opts.dir).map_err(|e| e.to_string())?;
        // `home` meets canonical indexed paths in in_scope/wants, so it
        // needs the same canonicalization `root` gets below (/var →
        // /private/var on macOS).
        let mut opts = opts;
        if let Ok(c) = std::fs::canonicalize(&opts.home) {
            opts.home = c.to_string_lossy().into_owned();
        }
        // One writer per index: a second one would race index writes. The
        // lock dies with the process.
        let lock = std::fs::File::create(opts.dir.join("daemon.lock")).map_err(|e| e.to_string())?;
        let owner = try_lock(&lock);
        let skip: Vec<Vec<u8>> = match opts.skip {
            Some(v) => v.into_iter().map(|p| p.as_os_str().as_bytes().to_vec()).collect(),
            None if has_full_disk_access() && std::env::var_os("FSEARCH_RESTRICT").is_none() => Vec::new(),
            None => {
                log(if cfg!(target_os = "macos") {
                    "no Full Disk Access: skipping consent-gated folders (grant it to fsearch to index everything)"
                } else {
                    "skipping pseudo-filesystems (/proc, /sys, /dev, /run, ...)"
                });
                gated(&opts.home)
            }
        };
        if !skip.is_empty() {
            let _ = walk::SKIP.set(skip);
        }
        // A trailing slash would double every path joined onto the root.
        let root = {
            let mut r = opts.root.unwrap_or_else(|| PathBuf::from("/")).as_os_str().as_bytes().to_vec();
            while r.len() > 1 && r.ends_with(b"/") {
                r.pop();
            }
            // Canonical form, so `in:` scopes (resolved with real_path) and
            // indexed paths agree on symlinked roots like /tmp -> /private/tmp.
            match std::fs::canonicalize(std::ffi::OsStr::from_bytes(&r)) {
                Ok(c) => c.as_os_str().as_bytes().to_vec(),
                Err(_) => r,
            }
        };
        let dir = opts.dir;
        let loading = Index::load(&dir.join("index.bin"));
        let (tx, rx) = std::sync::mpsc::channel();
        let (ctx, crx) = std::sync::mpsc::channel();
        let content = if owner { Content::open(dir.join("content")) } else { Content::open_shared(dir.join("content")) };
        let content_ready = AtomicBool::new(!owner || !content.segs.is_empty());
        let shared = Arc::new(Shared {
            live: RwLock::new(None),
            loading: (Mutex::new(loading.is_some()), Condvar::new()),
            content: RwLock::new(content),
            home: opts.home,
            root,
            dir,
            wake: tx,
            save_requested: AtomicBool::new(false),
            content_tx: ctx,
            content_rx: Mutex::new(Some(crx)),
            content_pending: AtomicUsize::new(0),
            dirty: Default::default(),
            owner: AtomicBool::new(owner),
            lock,
            stream: Mutex::new(None),
            streams: Default::default(),
            since: AtomicU64::new(0),
            replaying: AtomicBool::new(true),
            content_seen: Mutex::new(None),
            early: Mutex::new(Vec::new()),
            content_ready,
            content_early: Mutex::new(Vec::new()),
            on_change: RwLock::new(Vec::new()),
        });
        if owner {
            start_content(&shared);
        }
        let s = shared.clone();
        // Starting the stream (loading CoreServices) and faulting a saved
        // index in take ms each: done once searches run on the index as
        // mapped, so a restart's first answer needn't wait (`fsearch <query>`
        // with no daemon running: 33 -> 27 ms on the M6).
        spawn("fsearch-apply", move || {
            #[cfg(target_os = "linux")]
            let was_loaded = loading.is_some();
            let (live, replay) = match loading {
                Some(b) => {
                    log(format!("loaded {} entries, replaying events since {}", b.n, b.event_id));
                    let from = b.event_id;
                    (Live::lazy(b).rooted(&s.root), Some(from))
                }
                None if owner => {
                    // Watch before scanning so nothing that changes mid-scan
                    // is missed; replaying it afterwards is harmless (diffs
                    // are idempotent).
                    let since = fsevents::current_id();
                    s.watch(since);
                    (Live::lazy(full_build(&s, since)).rooted(&s.root), None)
                }
                None => {
                    // The owner is building it; follow once it exists.
                    let b = wait_for_index(&s.dir);
                    s.watch(b.event_id);
                    (Live::lazy(b).rooted(&s.root), None)
                }
            };
            *s.live.write().unwrap() = Some(live);
            s.early.lock().unwrap().clear();
            *s.loading.0.lock().unwrap() = false;
            s.loading.1.notify_all();
            if let Some(from) = replay {
                // (A save from before event ids were stamped: from now.)
                s.watch(if owner && from == 0 { fsevents::current_id() } else { from });
            }
            if let Some(l) = s.live.read().unwrap().as_ref() {
                l.base.prefault();
            }
            if owner {
                rescan_unskipped(&s);
            }
            // A saved index is already stale (no event replay on Linux):
            // catch up what changed while the process was off.
            #[cfg(target_os = "linux")]
            if was_loaded {
                relist_changed(&s, "restart without an event log", 0);
            }
            apply_loop(&s, rx);
        });
        Ok(Engine { s: shared })
    }

    pub fn home(&self) -> &str {
        &self.s.home
    }

    /// Keep the cores clocked up for `d` from now. After ~20 ms idle macOS
    /// slows them down, and a search then runs 3-6x slower while they ramp
    /// back up; one spinning thread keeps them up, so the next keystroke's
    /// search starts fast. The spinning stops when a search starts (the
    /// search keeps the cores busy itself) and when `d` runs out.
    pub fn keep_warm(&self, d: Duration) {
        let w = warmer();
        let now = now_ns();
        // They stay up for the first few ms anyway.
        WARM_FROM.store(now + 2_000_000, Ordering::SeqCst);
        WARM_UNTIL.store(now + d.as_nanos() as u64, Ordering::SeqCst);
        if WARM_IDLE.load(Ordering::SeqCst) {
            w.unpark();
        }
    }

    /// Name search.
    pub fn search(&self, q: &Query) -> Result<Vec<Found>, String> {
        WARM_UNTIL.store(0, Ordering::Relaxed);
        let (found, early) = self.s.with_names(q.scope.as_deref(), |live| {
            let mut p = Vec::new();
            search_pool()
                .install(|| Searcher { live }.search(q))
                .into_iter()
                .map(|h| {
                    let (kind, size, mtime) = match &h.over {
                        Some(path) => {
                            let o = live.over[path];
                            p = path.clone();
                            (o.kind, o.size, o.mtime)
                        }
                        None => {
                            let i = h.idx as usize;
                            live.base.path(i, &mut p);
                            (live.base.kind(i), live.base.size_of(i), live.base.mtime(i))
                        }
                    };
                    Found { path: PathBuf::from(std::ffi::OsStr::from_bytes(&p)), kind, size, mtime, score: h.score }
                })
                .collect()
        })?;
        self.s.index_first(early);
        Ok(found)
    }

    /// Is `path` an indexed directory — usable as an `in:` scope? A scope
    /// that resolves to nothing answers empty rather than an error, which an
    /// embedder cannot tell from a genuinely empty scope (and a dir added
    /// since the last compaction lives in the overlay, invisible to it).
    pub fn covers(&self, path: &[u8]) -> bool {
        let g = self.s.live();
        let Some(live) = g.as_ref() else { return false };
        live.base.lookup(path).and_then(|e| live.base.dir_of(e)).is_some()
    }

    /// Content search: `g` is the pattern, `q` narrows which files are read.
    /// The bool says whether the content index answered (false: files were
    /// picked from the name index and read, for folders it doesn't cover).
    pub fn grep(&self, q: &Query, g: &Grep) -> Result<(GrepResult, bool), String> {
        WARM_UNTIL.store(0, Ordering::Relaxed);
        let home = self.s.home.as_bytes();
        let indexed = q.scope.as_ref().is_none_or(|s| content::in_scope(s, home));
        let built = indexed && self.s.content_holds(q.scope.as_deref());
        if built {
            let fresh = self.s.dirty.files(q);
            let c = self.s.content.read().unwrap();
            // A folder the index holds nothing in (one its .gitignore leaves
            // out, say) is read directly, below.
            if q.scope.as_ref().is_none_or(|s| c.holds(s)) {
                // Content search brings its own reader threads; running it
                // here skips waking a pool thread just to hand the work over.
                return Ok((c.search_with(g, q, &fresh), true));
            }
        }
        // Pick files under the lock, read them after releasing it: reading can
        // be slow and a waiting writer would stall every other query.
        let scope = q.scope.as_deref();
        if indexed && !built {
            // The content index's first build is running: read the files it
            // will hold instead, so the answer is complete (and the same).
            // None there: read the folder directly, below, as once built.
            let (docs, early) = self.s.with_names(scope, |live| content::wanted(live, home, scope.unwrap_or(home), true))?;
            if !docs.is_empty() {
                let hold = walk::Hold::new();
                let r = content::grep_unindexed(g, docs, q);
                drop(hold);
                self.s.index_first(early);
                return Ok((r, false));
            }
            self.s.index_first(early);
        }
        let (paths, _) = self.s.with_names(scope, |live| search_pool().install(|| content::scan_paths(live, q.clone_for_scan())))?;
        let _hold = walk::Hold::new();
        Ok((content::verify(g, &paths, q.limit), false))
    }

    /// Regular files under `scope` the content index does not hold as they
    /// are now (size and mtime from the name index): every grep must read
    /// these directly, whatever its pattern. `None` until the content index
    /// holds `scope`. Only an answer once `status().content_pending == 0`.
    pub fn unheld_files(&self, scope: &[u8]) -> Option<Vec<Vec<u8>>> {
        if !self.s.content_holds(Some(scope)) {
            return None;
        }
        let home = self.s.home.as_bytes();
        let (out, early) = self.s.with_names(Some(scope), |live| {
            let mut want = content::wanted(live, home, scope, true);
            want.sort();
            let c = self.s.content.read().unwrap();
            let mut out: Vec<Vec<u8>> = c.unheld(scope, true, &want).into_iter().map(|k| want.path(k).to_vec()).collect();
            out.extend(c.not_text(scope, true));
            drop(c);
            out.extend(files_under(live, scope).into_iter().filter(|p| want.find(p).is_none()));
            out.sort_unstable();
            out.dedup();
            out
        }).ok()?;
        self.s.index_first(early);
        Some(out)
    }

    /// Held files under `scope` whose content has one of `pats` (each taken
    /// literally; `ignore_case` folds Unicode case). A superset of what a
    /// grep for the same literals finds in the files the index holds as they
    /// are. `None` when the answer may be incomplete or the index does not
    /// hold `scope`.
    pub fn held_matches(&self, scope: &[u8], pats: &[String], ignore_case: bool) -> Option<Vec<Vec<u8>>> {
        if !self.s.content_holds(Some(scope)) {
            return None;
        }
        let case = if ignore_case { content::Case::Insensitive } else { content::Case::Sensitive };
        let mut g = Grep::any_of(pats, case).ok()?;
        g.max_per_file = 1;
        g.budget = None;
        let q = Query { scope: Some(scope.to_vec()), limit: MATCH_CAP, size: (0, u64::MAX), mtime: (0, u32::MAX), ..Default::default() };
        let (res, _) = self.grep(&q, &g).ok()?;
        if !res.complete || res.next.is_some() || res.files.len() >= MATCH_CAP {
            return None;
        }
        Some(res.files.into_iter().map(|f| f.path).collect())
    }

    pub fn status(&self) -> Status {
        let l = self.s.live.read().unwrap();
        let c = self.s.content.read().unwrap();
        Status {
            ready: l.is_some(),
            entries: l.as_ref().map_or(0, |l| l.base.n),
            dirs: l.as_ref().map_or(0, |l| l.base.d),
            overlay: l.as_ref().map_or(0, |l| l.over.len()),
            removed: l.as_ref().map_or(0, |l| l.dead_count),
            event_id: l.as_ref().map_or(0, |l| l.event_id),
            index_bytes: l.as_ref().map_or(0, |l| l.base.bytes()),
            content_docs: c.docs(),
            content_segments: c.segs.len(),
            content_bytes: c.bytes(),
            // Until the first sync finishes there is pending work its
            // counter doesn't hold yet: report it so `pending == 0` means settled.
            content_pending: self.s.content_pending.load(Ordering::Relaxed) + usize::from(!self.s.content_ready.load(Ordering::Acquire)),
            root_missing: l.as_ref().is_some_and(|l| l.root_missing.load(Ordering::Relaxed)),
            full_disk_access: walk::SKIP.get().is_none(),
            owner: self.s.owner(),
        }
    }

    /// The answer while the first index builds, with its progress.
    pub fn indexing(&self) -> String {
        indexing()
    }

    /// Compact and save the name index soon (on the background thread).
    pub fn save(&self) {
        self.s.save_requested.store(true, Ordering::Relaxed);
        let _ = self.s.wake.send(Vec::new());
    }

    /// Have `f` told the folders each batch of disk changes touched (true:
    /// and maybe anything below them), as the batch is applied (on the
    /// thread applying it: keep `f` quick).
    pub fn on_change(&self, f: impl Fn(&[(&[u8], bool)]) + Send + Sync + 'static) {
        self.s.on_change.write().unwrap().push(Box::new(f));
    }
}

/// (dirs, trees) for the content worker to re-sync.
type Resync = (Vec<Vec<u8>>, Vec<Vec<u8>>);

/// The answer while the first index builds, with its progress.
fn indexing() -> String {
    let n = walk::LISTED.load(Ordering::Relaxed);
    format!("indexing: {:.1}M entries so far (the first run lists the whole disk, ~25s)", n as f64 / 1e6)
}

impl Shared {
    fn live(&self) -> RwLockReadGuard<'_, Option<Live>> {
        let (loading, cv) = &self.loading;
        drop(cv.wait_while(loading.lock().unwrap(), |l| *l).unwrap());
        self.live.read().unwrap()
    }

    fn owner(&self) -> bool {
        self.owner.load(Ordering::Relaxed)
    }

    /// Run `f` on the name index. On the first run, before the whole disk
    /// is listed (~25 s), a search in a folder runs on an index of just
    /// that folder, listed for it: as fast as a tool that only ever scans
    /// that folder, and the same answer the whole index will give. That
    /// index comes back too (see `index_first`).
    fn with_names<R>(&self, scope: Option<&[u8]>, f: impl FnOnce(&Live) -> R) -> Result<(R, Option<Arc<Early>>), String> {
        loop {
            if let Some(live) = self.live().as_ref() {
                return Ok((f(live), None));
            }
            let Some(dir) = scope.filter(|s| s.len() > 1) else { return Err(indexing()) };
            if let Some(early) = self.early(dir) {
                return Ok((f(&early.live), Some(early)));
            }
        }
    }

    /// The early index of a folder holding `dir`, listing `dir` if none
    /// does; None once the whole disk is in.
    fn early(&self, dir: &[u8]) -> Option<Arc<Early>> {
        let mut early = self.early.lock().unwrap();
        if self.live.read().unwrap().is_some() {
            return None;
        }
        if let Some(e) = early.iter().find(|e| under(dir, &e.dir)) {
            return Some(e.clone());
        }
        let t = Instant::now();
        let hold = walk::Hold::new();
        let ls = folder_listings(dir);
        drop(hold);
        let live = Live::new(Index::build(ls, 0, 0, self.home.as_bytes()));
        log(format!("listed {} first: {} entries in {:.2?}", String::from_utf8_lossy(dir), live.base.n, t.elapsed()));
        let e = Arc::new(Early { dir: dir.to_vec(), live, indexing: AtomicBool::new(false) });
        early.push(e.clone());
        Some(e)
    }

    /// Once a search answered from an early index: index the contents of
    /// its folder first (from that index), so its next greps use the index
    /// within seconds instead of reading every file. Not sooner: that
    /// build reads the same files as the answer's direct scan.
    fn index_first(&self, early: Option<Arc<Early>>) {
        if let Some(e) = early
            && self.owner()
            && content::in_scope(&e.dir, self.home.as_bytes())
            && !e.indexing.swap(true, Ordering::Relaxed)
        {
            let _ = self.content_tx.send((Vec::new(), vec![e.dir.clone()]));
        }
    }

    /// Does the content index hold every file under `scope` (all it should
    /// hold, without one)?
    fn content_holds(&self, scope: Option<&[u8]>) -> bool {
        self.content_ready.load(Ordering::Acquire) || scope.is_some_and(|s| self.content_early.lock().unwrap().iter().any(|f| under(s, f)))
    }

    /// (Re)start the FSEvents stream from `since`, replacing any old one.
    fn watch(&self, since: u64) {
        self.replaying.store(true, Ordering::Relaxed);
        self.since.store(since, Ordering::Relaxed);
        let n = self.streams.fetch_add(1, Ordering::Relaxed) + 1;
        // 10 ms: a change shows in ~12 ms (fseventsd's own delay); at 0.1 s,
        // one made within 0.1 s of another (save then rename) took ~100 ms.
        // `Pace` keeps a busy folder from being listed that often.
        let new = fsevents::watch(since, 0.01, self.wake.clone(), n);
        *self.stream.lock().unwrap() = Some(new);
    }

    /// A follower picks up what the owner wrote: a newer name-index save
    /// (replaying FSEvents from it) and content segment changes.
    fn follow(&self) {
        let path = self.dir.join("index.bin");
        let saved = Index::saved_event_id(&path).unwrap_or(0);
        let ours = self.live.read().unwrap().as_ref().map_or(0, |l| l.base.event_id);
        if saved > ours
            && let Some(base) = Index::load(&path)
        {
            log(format!("following the owner's save: {} entries, replaying since {}", base.n, base.event_id));
            self.watch(base.event_id);
            *self.live.write().unwrap() = Some(Live::new(base).rooted(&self.root));
        }
        let cdir = self.dir.join("content");
        let changed = std::fs::metadata(&cdir).and_then(|m| m.modified()).ok();
        let mut seen = self.content_seen.lock().unwrap();
        if changed != *seen {
            *seen = changed;
            *self.content.write().unwrap() = Content::open_shared(cdir);
        }
    }
}

fn start_content(s: &Arc<Shared>) {
    let Some(rx) = s.content_rx.lock().unwrap().take() else {
        return;
    };
    let s = s.clone();
    spawn("fsearch-content", move || content_loop(&s, rx));
}

/// A follower takes over the index files once their owner is gone.
fn try_upgrade(s: &Arc<Shared>) -> bool {
    if s.owner() {
        return true;
    }
    if !try_lock(&s.lock) {
        return false;
    }
    s.owner.store(true, Ordering::Relaxed);
    log("took over the index from a previous owner");
    *s.content.write().unwrap() = Content::open(s.dir.join("content"));
    rescan_unskipped(s);
    start_content(s);
    true
}

/// Next to index.bin: the folders it lacks for want of access, one per
/// line (skipped, or refused by macOS).
const SKIPPED: &str = "skipped";

fn note_skipped(dir: &Path) {
    let denied = walk::DENIED.lock().unwrap();
    // (A folder listed early is listed again by the whole-disk crawl.)
    let mut all: Vec<&Vec<u8>> = walk::SKIP.get().into_iter().flatten().chain(denied.iter()).collect();
    all.sort();
    all.dedup();
    let mut out = Vec::new();
    for p in all {
        out.extend_from_slice(p);
        out.push(b'\n');
    }
    if let Err(e) = std::fs::write(dir.join(SKIPPED), out) {
        log(format!("save failed: {e}"));
    }
}

/// Full Disk Access granted since the save: the folders it lacked stay
/// missing, since no FSEvents replay brings them back. Rescan them like a
/// must-scan-subdirs event would; ones still refused carry over to the next
/// save. (A save from before this was recorded rescans the gated folders.)
fn rescan_unskipped(shared: &Shared) {
    let was: Vec<Vec<u8>> = match std::fs::read(shared.dir.join(SKIPPED)) {
        Ok(b) => b.split(|&c| c == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect(),
        Err(_) => gated(&shared.home),
    };
    let mut now = Vec::new();
    for path in was {
        if walk::blocked(&path) {
            continue;
        }
        let readable = std::fs::read_dir(std::ffi::OsStr::from_bytes(&path)).map_or_else(|e| e.raw_os_error() != Some(libc::EPERM), |_| true);
        if readable {
            now.push(fsevents::Event { path, flags: MUST_SCAN_SUBDIRS, id: 0, stream: 0 });
        } else {
            walk::DENIED.lock().unwrap().push(path);
        }
    }
    if now.is_empty() {
        return;
    }
    log(format!("rescanning {} folders the saved index lacked", now.len()));
    let _ = shared.wake.send(now);
    // Saved soon, so the next start doesn't rescan them again.
    shared.save_requested.store(true, Ordering::Relaxed);
}

/// Is `path` `dir` or inside it?
/// Regular files under `scope` in the name index, base and overlay, by path
/// (unsorted, may repeat where the overlay shadows the base).
fn files_under(live: &Live, scope: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut p = Vec::new();
    if let Some(top) = live.base.lookup(scope).and_then(|e| live.base.dir_of(e)) {
        let mut stack = vec![top];
        while let Some(k) = stack.pop() {
            for i in live.base.children(k) {
                if live.is_dead(i as u32) {
                    continue;
                }
                match live.base.kind(i) & 3 {
                    KIND_DIR => stack.extend(live.base.dir_of(i as u32)),
                    KIND_FILE => {
                        live.base.path(i, &mut p);
                        out.push(p.clone());
                    }
                    _ => {}
                }
            }
        }
    }
    out.extend(live.over.range(scope.to_vec()..).take_while(|(p, _)| under(p, scope)).filter(|(_, o)| o.kind & 3 == KIND_FILE).map(|(p, _)| p.clone()));
    out
}

fn under(path: &[u8], dir: &[u8]) -> bool {
    path.starts_with(dir) && (path.len() == dir.len() || path[dir.len()] == b'/')
}

/// Threads for listing one folder a search waits on (the whole-disk crawl
/// pauses meanwhile): on the M6, Linux's 102k entries list in 131 ms on 8,
/// 106 on 12, 95 on 16.
const EARLY_THREADS: usize = 16;

/// Listings for an index of just `dir`: a level from "/" down to it, each
/// with the one entry its parent's listing has for the next, then `dir`'s
/// whole subtree. Where the whole-disk crawl would not descend (a mount
/// point, a file, a missing or skipped folder) the chain stops, and the
/// index has nothing under `dir` either.
fn folder_listings(dir: &[u8]) -> Vec<Listing> {
    let comps: Vec<&[u8]> = dir.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    let (mut out, mut path) = (Vec::new(), b"/".to_vec());
    for (i, &c) in comps.iter().enumerate() {
        let found = walk::list_one(&path).and_then(|l| l.ents.iter().copied().find(|e| &l.names[e.name_off as usize..][..e.name_len as usize] == c));
        let Some(e) = found else { return out };
        path = crate::live::join(&path, c);
        let descend = e.kind & 3 == KIND_DIR && e.kind & walk::FLAG_MOUNT == 0 && !walk::blocked(&path);
        let child = if descend { i as u32 + 1 } else { NONE };
        out.push(Listing { id: i as u32, names: c.to_vec(), ents: vec![RawEnt { name_off: 0, child, ..e }] });
        if !descend {
            return out;
        }
    }
    let k = comps.len() as u32;
    for mut l in walk::scan(dir, EARLY_THREADS) {
        l.id += k;
        for e in l.ents.iter_mut().filter(|e| e.child != NONE) {
            e.child += k;
        }
        out.push(l);
    }
    out
}

fn wait_for_index(dir: &Path) -> Index {
    loop {
        if let Some(b) = Index::load(&dir.join("index.bin")) {
            return b;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(target_os = "macos")]
#[rustfmt::skip]
const GATED_IN_HOME: &[&str] = &[
    "Desktop", "Documents", "Downloads", "Library/Mobile Documents", "Library/Containers", "Library/Group Containers",
    "Library/CloudStorage", "Pictures/Photos Library.photoslibrary", "Music/Music", "Movies/TV",
];

/// Folders macOS guards with a consent prompt (or that hold other volumes).
#[cfg(target_os = "macos")]
pub fn gated(home: &str) -> Vec<Vec<u8>> {
    GATED_IN_HOME.iter().map(|d| format!("{home}/{d}").into_bytes()).chain([b"/Volumes".to_vec()]).collect()
}

/// Linux's "gated" set is not consent but pseudo-filesystems and system
/// trees: nothing under them is a file to find, and /proc & friends fight
/// readers with contents that mutate while read.
#[cfg(target_os = "linux")]
pub fn gated(_home: &str) -> Vec<Vec<u8>> {
    ["/proc", "/sys", "/dev", "/run", "/boot/efi", "/var/lib/docker", "/var/lib/containers", "/snap"].iter().map(|s| s.as_bytes().to_vec()).collect()
}

/// The system TCC database is readable only with Full Disk Access, and
/// trying without it fails immediately (no prompt).
#[cfg(target_os = "macos")]
pub fn has_full_disk_access() -> bool {
    std::fs::File::open("/Library/Application Support/com.apple.TCC/TCC.db").is_ok()
}

/// No TCC on Linux: every engine runs "restricted", meaning the
/// pseudo-filesystem skip list applies (see `gated`).
#[cfg(not(target_os = "macos"))]
pub fn has_full_disk_access() -> bool {
    false
}

fn content_pool(threads: usize, qos: Qos) -> rayon::ThreadPool {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(move |_| {
            set_qos(qos);
            no_materialize();
        })
        .build()
        .unwrap()
}

fn content_loop(shared: &Shared, rx: Receiver<Resync>) {
    // Indexing file contents is background work: utility QoS keeps it off
    // the user's way (lower CPU priority and IO tier).
    set_qos(QOS_UTILITY);
    let pool = content_pool(4, QOS_UTILITY);
    let home = shared.home.as_bytes().to_vec();
    // First run, while the whole disk is listed: index the folders searches
    // ask about from their early indexes. (A request that comes as the
    // whole index lands is dropped: the sync of all of home covers it.)
    while shared.live.read().unwrap().is_none() {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((_, dirs)) => {
                for dir in dirs {
                    let Some(e) = shared.early.lock().unwrap().iter().find(|e| e.dir == dir).cloned() else { continue };
                    sync_wants(shared, &pool, content::wants(&e.live, &home, &[], std::slice::from_ref(&dir)));
                    shared.content_early.lock().unwrap().push(dir);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
    // Reconcile all of home once, right away (cheap when nothing changed),
    // then follow along with the name index's changes.
    sync(shared, &pool, &home, &[], std::slice::from_ref(&home));
    shared.content_ready.store(true, Ordering::Release);
    // Per-folder debounce: a folder is processed 2s after its last change,
    // or 5 min after its first pending one if it never goes quiet. A file you
    // save lands in ~2s; files apps rewrite every second (state, logs) cost
    // one reindex per 5 min instead of one per event batch.
    let mut pending: HashMap<(Vec<u8>, bool), (Instant, Instant)> = HashMap::new();
    loop {
        let wait = if pending.is_empty() { Duration::from_secs(3600) } else { Duration::from_millis(250) };
        match rx.recv_timeout(wait) {
            Ok(first) => {
                let now = Instant::now();
                let mut changed = Vec::new();
                for (d, t) in std::iter::once(first).chain(rx.try_iter()) {
                    for key in d.into_iter().map(|p| (p, false)).chain(t.into_iter().map(|p| (p, true))) {
                        // Most of the disk's churn (Library, caches) is outside the indexed area.
                        if content::in_scope(&key.0, &home) || (key.1 && home.starts_with(&key.0)) {
                            pending.entry(key.clone()).and_modify(|e| e.1 = now).or_insert((now, now));
                            changed.push(key);
                        }
                    }
                }
                // Until they are synced, searches read their changed files.
                changed.sort();
                changed.dedup();
                let wants = shared.live.read().unwrap().as_ref().map(|l| content::wanted_now(l, &home, &changed));
                if let Some(w) = wants {
                    shared.dirty.mark(&shared.content.read().unwrap(), w);
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let ripe: Vec<(Vec<u8>, bool)> = pending
            .iter()
            .filter(|(_, (first, last))| last.elapsed() >= CONTENT_QUIET || first.elapsed() >= CONTENT_MAX_WAIT)
            .map(|(k, _)| k.clone())
            .collect();
        if ripe.is_empty() {
            continue;
        }
        let (mut dirs, mut trees) = (Vec::<Vec<u8>>::new(), Vec::<Vec<u8>>::new());
        for k in &ripe {
            pending.remove(k);
            if k.1 { trees.push(k.0.clone()) } else { dirs.push(k.0.clone()) }
        }
        sync(shared, &pool, &home, &dirs, &trees);
        shared.dirty.clear(&ripe);
    }
}

/// Bring the content index in line with the name index for these folders
/// (direct children) and trees.
fn sync(shared: &Shared, pool: &rayon::ThreadPool, home: &[u8], dirs: &[Vec<u8>], trees: &[Vec<u8>]) {
    let wants = {
        let g = shared.live.read().unwrap();
        let Some(live) = g.as_ref() else { return };
        content::wants(live, home, dirs, trees)
    };
    sync_wants(shared, pool, wants);
}

/// `sync`, given what the name index wants.
fn sync_wants(shared: &Shared, pool: &rayon::ThreadPool, wants: Vec<(Vec<u8>, bool, content::Docs)>) {
    let t = Instant::now();
    // (A folder built early leaves segments behind; the build is still the first.)
    let first = !shared.content_ready.load(Ordering::Acquire);
    let todo = shared.content.write().unwrap().diff(wants);
    if todo.is_empty() {
        return;
    }
    let n = todo.len();
    shared.content_pending.store(n, Ordering::Relaxed);
    // A first build (fresh install, format change) is a one-time wait the
    // user is watching: 8 threads at user-initiated QoS. Each thread keeps
    // its next files opened ahead, and past 8 threads those opens only
    // contend in the kernel. Measured on HOME (730k files, M4 Max, with
    // speed-content's open-ahead build): 99-155 s on the 4 utility threads;
    // 8 threads 17.2-17.5 s (61 s system CPU), 12 threads 22.0-24.3 s, 16
    // threads 27.2-30.1 s (250-300 s system CPU).
    let fast;
    let pool = if first {
        let threads = std::thread::available_parallelism().map_or(8, |n| n.get()).min(8);
        fast = content_pool(threads, QOS_INITIATED);
        &fast
    } else {
        pool
    };
    build_batches(shared, pool, &todo);
    drop(todo);
    // Keep the segment count small: merge size tiers of 8.
    loop {
        let plan = shared.content.read().unwrap().merge_plan();
        let Some(ids) = plan else { break };
        let (dir, id) = {
            let mut c = shared.content.write().unwrap();
            (c.dir.clone(), c.alloc_id())
        };
        let merged = {
            let c = shared.content.read().unwrap();
            pool.install(|| content::merge(&dir, id, &c.segments(&ids)))
        };
        match merged {
            Some(seg) => shared.content.write().unwrap().replace(&ids, seg),
            None => break,
        }
    }
    if n > 100 {
        log(format!("content: indexed {n} files in {:.2?}", t.elapsed()));
    }
    release_memory();
}

/// Build `todo` into segments one batch (`Docs::batches`) at a time, each
/// pushed when done. Two in flight overlapped one's single-threaded tail
/// with the next one's reads: on the M6 (Linux, ABBA n=10) 3.07 s instead
/// of 3.33, but a first-run peak of 616 MB instead of 375.
fn build_batches(shared: &Shared, pool: &rayon::ThreadPool, todo: &content::Docs) {
    let dir = shared.content.read().unwrap().dir.clone();
    for range in todo.batches() {
        walk::yield_to_searches();
        let id = shared.content.write().unwrap().alloc_id();
        if let Some(seg) = pool.install(|| content::build_segment(&dir, id, todo, range.clone())) {
            shared.content.write().unwrap().push(seg);
        }
        shared.content_pending.fetch_sub(range.len(), Ordering::Relaxed);
    }
}

/// `walk::scan`, with the ancestors of a non-"/" root grafted on as
/// one-entry listings: `Index` requires listing 0 to be "/" and builds paths
/// from it, so a scan of /a/b gets synthetic `/` (holding `a`) and `/a`
/// (holding `b`) above it and every indexed path stays absolute.
/// `Live::fetch` never descends into them (they're outside the root).
fn scan_rooted(root: &[u8], threads: usize) -> Vec<walk::Listing> {
    // The whole disk is the disk crawl (LISTED, `yield_to_searches`).
    let mut ls = if root == b"/" { walk::scan_disk(threads) } else { walk::scan(root, threads) };
    let comps: Vec<&[u8]> = root.split(|&b| b == b'/').filter(|c| !c.is_empty()).collect();
    if comps.is_empty() {
        return ls;
    }
    // Fresh ids for the synthetics; the scanned root listing moves off 0 so
    // the synthetic "/" can take it (Index::build starts DFS at listing 0).
    let mut next = ls.iter().map(|l| l.id).max().unwrap_or(0) + 1;
    let real_root = next;
    next += 1;
    for l in &mut ls {
        if l.id == 0 {
            l.id = real_root;
        }
    }
    // ids[d] = the listing id for "/"+comps[..d]; ids[k] is the real root's.
    let mut ids = vec![0u32; comps.len() + 1];
    ids[comps.len()] = real_root;
    for d in ids.iter_mut().take(comps.len()).skip(1) {
        *d = next;
        next += 1;
    }
    let mut parent = vec![b'/'];
    for (d, comp) in comps.iter().enumerate() {
        let mtime = crate::live::lstat(&parent).map_or(0, |o| o.mtime);
        let kind = walk::KIND_DIR | (u8::from(comp[0] == b'.') * walk::FLAG_HIDDEN);
        ls.push(walk::Listing {
            id: ids[d],
            names: comp.to_vec(),
            ents: vec![walk::RawEnt { name_off: 0, name_len: comp.len() as u16, kind, size: 0, mtime, child: ids[d + 1] }],
        });
        parent.extend_from_slice(comp);
        parent.push(b'/');
    }
    ls
}

fn full_build(shared: &Shared, event_id: u64) -> Index {
    let t = Instant::now();
    let started = crate::query::now_secs();
    let ls = scan_rooted(&shared.root, SCAN_THREADS);
    let idx = Index::build(ls, event_id, started, shared.home.as_bytes());
    let path = shared.dir.join("index.bin");
    if let Err(e) = idx.save(&path) {
        log(format!("save failed: {e}"));
    }
    note_skipped(&shared.dir);
    log(format!("indexed {} entries in {:.2?}", idx.n, t.elapsed()));
    release_memory();
    // Re-map from the file so the index is clean, evictable page cache
    // rather than anonymous memory.
    Index::load(&path).unwrap_or(idx)
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
}

/// Hand freed allocator memory back to the OS after big transient work
/// (index builds, content batches) instead of letting malloc cache it.
fn release_memory() {
    #[cfg(target_os = "macos")]
    unsafe {
        malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    unsafe {
        libc::malloc_trim(0);
    }
}

fn compact(shared: &Shared) {
    let t = Instant::now();
    let (ls, eid, synced) = {
        let g = shared.live.read().unwrap();
        let live = g.as_ref().unwrap();
        (live.to_listings(), live.event_id, live.synced_at)
    };
    // Linux event ids are a clock, not a history position: stamp each
    // save fresh so followers see `saved > ours` and reload.
    let eid = if cfg!(target_os = "linux") { fsevents::current_id() } else { eid };
    let idx = Index::build(ls, eid, synced, shared.home.as_bytes());
    let path = shared.dir.join("index.bin");
    if let Err(e) = idx.save(&path) {
        log(format!("save failed: {e}"));
    }
    note_skipped(&shared.dir);
    let idx = Index::load(&path).unwrap_or(idx);
    let n = idx.n;
    *shared.live.write().unwrap() = Some(Live::new(idx).rooted(&shared.root));
    release_memory();
    log(format!("compacted to {n} entries in {:.2?}", t.elapsed()));
}

/// FSEvents lost track of / (dropped events, or no history back to our
/// save): relist every folder modified since we were last in sync, plus the
/// folders of indexed text files edited since (an edit in place doesn't
/// touch its folder). Seconds, instead of recrawling the whole disk.
fn relist_changed(shared: &Shared, why: &str, flags: u32) {
    let t = Instant::now();
    let started = crate::query::now_secs();
    // synced_at 0 (unknown) relists everything: a full crawl, done in place.
    // So does a root that went missing (unmounted): while it is away the
    // tree is offline — `apply` drops events under it instead of removing —
    // and once it is back mtimes cannot say what changed, so all is relisted.
    let (from, mut dirs) = {
        let g = shared.live.read().unwrap();
        let live = g.as_ref().unwrap();
        let missing = crate::live::lstat(&live.root).is_none();
        let was = live.root_missing.fetch_or(missing, Ordering::Relaxed);
        let from = if missing || was { 0 } else { live.synced_at.saturating_sub(SYNC_MARGIN) };
        (from, live.changed_dirs(from))
    };
    dirs.extend(shared.content.read().unwrap().changed_dirs(from));
    dirs.sort();
    dirs.dedup();
    let stat_time = t.elapsed();
    // Disk reads under the read lock, one folder per write, so searches keep
    // answering meanwhile.
    for d in &dirs {
        let f = shared.live.read().unwrap().as_ref().unwrap().fetch(d, false);
        shared.live.write().unwrap().as_mut().unwrap().apply(f);
    }
    let trees = {
        let mut g = shared.live.write().unwrap();
        let live = g.as_mut().unwrap();
        live.synced_at = started;
        std::mem::take(&mut live.trees)
    };
    let n = dirs.len();
    let _ = shared.content_tx.send((dirs, trees));
    shared.on_change.read().unwrap().iter().for_each(|f| f(&[(&b"/"[..], true)]));
    log(format!(
        "FSEvents lost track of / ({why}, flags {flags:#x}): relisted {n} folders changed since {from} in {:.2?} ({stat_time:.2?} checking)",
        t.elapsed()
    ));
}

/// Folders are listed again as their events come, but a folder may spend
/// at most 1/RELIST_SHARE of the time being listed, running at most
/// RELIST_AHEAD ahead of that: a folder changed a few times in a row
/// (save, rename, delete) is listed each time, while a hot one of 5000
/// entries (2.2 ms a listing) settles at ~9 listings a second.
const RELIST_SHARE: u32 = 50;
const RELIST_AHEAD: Duration = Duration::from_millis(2500);

/// Folders whose events wait because they were listed a lot lately.
#[derive(Default)]
struct Pace {
    /// Per folder: when the time its listings may take is paid for (listing
    /// time x RELIST_SHARE, from when it was listed).
    paid: HashMap<Vec<u8>, Instant>,
    /// Folders waiting for that: (recursive, oldest event id).
    held: HashMap<Vec<u8>, (bool, u64)>,
    /// `paid`'s size after it was last pruned of folders paid up.
    kept: usize,
}

impl Pace {
    /// May `dir` be listed now?
    fn free(&self, dir: &[u8], now: Instant) -> bool {
        self.paid.get(dir).is_none_or(|&t| t <= now + RELIST_AHEAD)
    }

    /// Hold back the folders of `dirs` that ran too far ahead; add the held
    /// ones now free.
    fn sort(&mut self, dirs: &mut HashMap<Vec<u8>, (bool, u64)>, now: Instant) {
        let early: Vec<Vec<u8>> = dirs.keys().filter(|d| !self.free(d, now)).cloned().collect();
        for d in early {
            let (r, id) = dirs.remove(&d).unwrap();
            let h = self.held.entry(d).or_insert((r, id));
            *h = (h.0 | r, h.1.min(id));
        }
        let due: Vec<Vec<u8>> = self.held.keys().filter(|d| self.free(d, now)).cloned().collect();
        for d in due {
            let (r, id) = self.held.remove(&d).unwrap();
            let e = dirs.entry(d).or_insert((r, id));
            *e = (e.0 | r, e.1.min(id));
        }
    }

    fn listed(&mut self, dir: &[u8], took: Duration, now: Instant) {
        if self.paid.len() > 2 * self.kept.max(2048) {
            self.paid.retain(|_, t| *t > now);
            self.kept = self.paid.len();
        }
        // (A listing that also scanned a big new subfolder holds it 1 s at most.)
        let from = self.paid.get(dir).map_or(now, |&t| t.max(now));
        self.paid.insert(dir.to_vec(), (from + took * RELIST_SHARE).min(now + RELIST_AHEAD + Duration::from_secs(1)));
    }

    /// Until the first held folder is free.
    fn wait(&self, now: Instant) -> Option<Duration> {
        let free_at = |d: &Vec<u8>| self.paid.get(d).map_or(now, |&t| t.checked_sub(RELIST_AHEAD).unwrap_or(now));
        self.held.keys().map(free_at).min().map(|t| t.saturating_duration_since(now))
    }

    /// Event ids from this one on are not all applied yet.
    fn floor(&self) -> u64 {
        self.held.values().map(|h| h.1).min().unwrap_or(u64::MAX)
    }
}

/// Events a stream replaced since still had queued (their `stream` number
/// is no longer current): dropped, the new stream replays what they saw.
/// Stream 0 is synthetic (see `rescan_unskipped`), never a real stream's.
fn drop_stale_events(events: &mut Vec<fsevents::Event>, current: u32) -> usize {
    let was = events.len();
    events.retain(|e| e.stream == 0 || e.stream == current);
    was - events.len()
}

fn apply_loop(shared: &Arc<Shared>, rx: Receiver<Vec<fsevents::Event>>) {
    let mut last_save = Instant::now();
    let mut last_follow = Instant::now();
    let ours = shared.dir.as_os_str().as_bytes();
    // Events fseventsd dropped before we read them: (replay from, last drop).
    let mut gap: Option<(u64, Instant)> = None;
    // Replays in a row that dropped again; the third falls back to a relist.
    let mut replays = 0;
    let mut pace = Pace::default();
    loop {
        let wait = if gap.is_some() { Duration::from_millis(250) } else { Duration::from_secs(60) };
        let wait = pace.wait(Instant::now()).map_or(wait, |w| w.min(wait));
        let mut events = match rx.recv_timeout(wait) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => {
                // Linux has no event stream to follow, so the minute tick
                // is the watcher: relist folders whose mtimes moved.
                #[cfg(target_os = "linux")]
                if shared.owner() && shared.live.read().unwrap().is_some() {
                    relist_changed(shared, "watcher tick", 0);
                }
                Vec::new()
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        events.extend(rx.try_iter().flatten());
        // A replaced stream can still have batches in flight; the stream
        // that took over replays what they carried, so they are dropped —
        // an old stream's HISTORY_DONE must not end the new one's replay.
        let stale = drop_stale_events(&mut events, shared.streams.load(Ordering::Relaxed));
        if stale > 0 {
            log(format!("dropped {stale} events from a replaced stream"));
        }
        // The owner wrote the index files: a follower picks that up now
        // rather than at its next periodic check.
        let owner_wrote = events.iter().any(|e| e.path.starts_with(ours));
        let (mut rebuild, mut root_flags, had_events) = (false, 0, !events.is_empty());
        let (mut dirs, mut max_id): (HashMap<Vec<u8>, (bool, u64)>, u64) = (HashMap::new(), 0);
        if had_events {
            let mut last_good = shared.live.read().unwrap().as_ref().map_or(0, |l| l.event_id);
            let current = shared.streams.load(Ordering::Relaxed);
            for e in events {
                if e.path == b"/" && e.flags & MUST_SCAN_SUBDIRS != 0 {
                    // fseventsd still logged what it dropped for us (a kernel
                    // drop never reached it): replay from the last event we
                    // did get, once the drops stop; a drop during a replay
                    // starts that replay over. A replaced stream's drop is
                    // covered by the stream that replaced it.
                    if e.flags & (USER_DROPPED | KERNEL_DROPPED) == USER_DROPPED {
                        if e.stream == current {
                            let from = if shared.replaying.load(Ordering::Relaxed) { shared.since.load(Ordering::Relaxed) } else { last_good };
                            gap = Some((gap.map_or(from, |(g, _)| g.min(from)), Instant::now()));
                        }
                        continue;
                    }
                    root_flags |= e.flags;
                }
                max_id = max_id.max(e.id);
                if gap.is_none() {
                    last_good = last_good.max(e.id);
                }
                if e.flags & HISTORY_DONE != 0 {
                    log("replay done");
                    shared.replaying.store(false, Ordering::Relaxed);
                    if gap.is_none() {
                        replays = 0;
                    }
                    continue;
                }
                let d = dirs.entry(crate::live::normalize(&e.path)).or_insert((false, e.id));
                *d = (d.0 | (e.flags & MUST_SCAN_SUBDIRS != 0), d.1.min(e.id));
            }
        }
        let now = Instant::now();
        pace.sort(&mut dirs, now);
        if !dirs.is_empty() {
            // The folders listed now (a held one is told when it is due).
            let hooks = shared.on_change.read().unwrap();
            if !hooks.is_empty() {
                let changed: Vec<(&[u8], bool)> = dirs.iter().map(|(p, &(r, _))| (p.as_slice(), r)).collect();
                hooks.iter().for_each(|f| f(&changed));
            }
            drop(hooks);
            let mut trees = Vec::new();
            // Read the disk under the read lock, then apply in memory: a
            // search never waits on a folder listing or a new subtree's scan.
            let fetched: Vec<_> = {
                let g = shared.live.read().unwrap();
                let live = g.as_ref().unwrap();
                dirs.iter()
                    .map(|(p, &(recursive, _))| {
                        let t = Instant::now();
                        let f = live.fetch(p, recursive);
                        pace.listed(p, t.elapsed(), now);
                        f
                    })
                    .collect()
            };
            {
                let mut g = shared.live.write().unwrap();
                let live = g.as_mut().unwrap();
                for f in fetched {
                    if let Applied::Rebuild = live.apply(f) {
                        rebuild = true;
                    }
                }
                // With a gap, everything is applied only up to its start;
                // with folders held back, only up to their first event.
                live.event_id = match gap {
                    Some((from, _)) => live.event_id.min(from),
                    None => live.event_id.max(max_id.min(pace.floor().saturating_sub(1))),
                };
                trees.append(&mut live.trees);
            }
            let (rec, flat): (Vec<_>, Vec<_>) = dirs.into_iter().partition(|(_, (r, _))| *r);
            trees.extend(rec.into_iter().map(|(p, _)| p));
            let _ = shared.content_tx.send((flat.into_iter().map(|(p, _)| p).collect(), trees));
        }
        let replay = gap.filter(|(_, at)| at.elapsed() >= REPLAY_QUIET).map(|(from, _)| from);
        if rebuild || (replay.is_some() && replays >= 3) {
            let why = match root_flags {
                f if f & KERNEL_DROPPED != 0 => "kernel dropped events",
                0 => "events dropped again while replaying",
                _ => "history unavailable",
            };
            relist_changed(shared, why, root_flags);
            (gap, replays) = (None, 0);
        } else if let Some(from) = replay {
            log(format!("FSEvents dropped events before we read them: replaying from {from}"));
            shared.watch(from);
            (gap, replays) = (None, replays + 1);
        } else if had_events && gap.is_none() && pace.held.is_empty() && !shared.replaying.load(Ordering::Relaxed) {
            // Everything up to this batch is applied (a change's event can
            // trail it by the stream latency; relisting keeps a margin).
            shared.live.write().unwrap().as_mut().unwrap().synced_at = crate::query::now_secs();
        }
        if let Some(l) = shared.live.read().unwrap().as_ref() {
            l.names_cache.trim_if_idle(Duration::from_secs(60));
        }
        if now_ns().saturating_sub(LAST_SEARCH.load(Ordering::Relaxed)) > 60_000_000_000 && crate::alloc::kept() {
            pool().spawn_broadcast(|_| crate::alloc::drain());
        }
        if !shared.owner() && (owner_wrote || last_follow.elapsed() > FOLLOW_EVERY) {
            last_follow = Instant::now();
            if !try_upgrade(shared) {
                shared.follow();
            }
        }
        if !shared.owner() {
            continue;
        }
        let (pending, stale) = {
            let g = shared.live.read().unwrap();
            let live = g.as_ref().unwrap();
            (live.over.len() + live.dead_count, live.event_id != live.base.event_id)
        };
        let asked = shared.save_requested.swap(false, Ordering::Relaxed);
        if asked || pending > COMPACT_PENDING || (stale && last_save.elapsed() > COMPACT_EVERY) {
            compact(shared);
            last_save = Instant::now();
        }
    }
}

/// Default data dir: `~/Library/Application Support/FSearch`.
#[cfg(target_os = "macos")]
pub fn default_dir(home: &str) -> PathBuf {
    Path::new(home).join("Library/Application Support/FSearch")
}

/// Default data dir: `$XDG_DATA_HOME/fsearch`, else `~/.local/share/fsearch`.
#[cfg(target_os = "linux")]
pub fn default_dir(home: &str) -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| Path::new(home).join(".local/share")).join("fsearch")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_replaced_stream_events() {
        let ev = |stream: u32| fsevents::Event { path: b"/x".to_vec(), flags: 0, id: 0, stream };
        // Stream 1 was replaced by 2; its queued events die. 0 is synthetic.
        let mut v = vec![ev(1), ev(0), ev(2), ev(1)];
        assert_eq!(drop_stale_events(&mut v, 2), 2);
        assert_eq!(v.iter().map(|e| e.stream).collect::<Vec<_>>(), [0, 2]);
    }
}
