//! Parallel whole-disk enumeration for Linux: getdents64(2) + statx(2).
//!
//! getdents64 returns hundreds of entries per syscall with name and type
//! already attached; statx then supplies size, mtime and device — Linux has
//! no getattrlistbulk, so each entry costs one statx and there is no bulk
//! alternative. Directories fan out over a rayon pool and children are
//! opened with openat() relative to the parent's fd, so paths are never
//! rebuilt and PATH_MAX never bites.
//!
//! Mount points are crossed, not stopped at: on Linux the interesting
//! trees (/home, /tmp) are routinely separate mounts, while the ones to
//! avoid (/proc, /sys, /dev, /run) are excluded by path via `walk::SKIP`.
//! A device id that differs from the parent's still sets FLAG_MOUNT, for
//! display.

use rayon::Scope;
use std::cell::RefCell;
use std::ffi::CString;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

pub const NONE: u32 = u32::MAX;

/// Folders never to open: on Linux `engine::gated` supplies the
/// pseudo-filesystem set (/proc, /sys, /dev, /run, ...).
pub static SKIP: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();

/// Folders a scan was refused (EACCES/EPERM). Recorded only while paths
/// are tracked, i.e. while SKIP is set.
pub static DENIED: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());

/// Entries listed by scans so far: the first scan's progress.
pub static LISTED: AtomicUsize = AtomicUsize::new(0);

pub fn blocked(path: &[u8]) -> bool {
    SKIP.get().is_some_and(|v| {
        v.iter()
            .any(|s| path.starts_with(s) && (path.len() == s.len() || path[s.len()] == b'/'))
    })
}

pub const KIND_FILE: u8 = 0;
pub const KIND_DIR: u8 = 1;
pub const KIND_LINK: u8 = 2;
pub const KIND_OTHER: u8 = 3;

/// Entry flag bit: the name starts with a dot (Linux's UF_HIDDEN).
pub const FLAG_HIDDEN: u8 = 1 << 2;
/// Directory on a different device than its parent. Noted for display;
/// unlike macOS it does not stop the descent (see the module doc).
pub const FLAG_MOUNT: u8 = 1 << 3;

#[derive(Clone, Copy)]
pub struct RawEnt {
    pub name_off: u32,
    pub name_len: u16,
    /// kind in the low 2 bits, FLAG_* above.
    pub kind: u8,
    pub size: u64,
    pub mtime: u32,
    /// Temp id of the listing for this directory, or NONE.
    pub child: u32,
}

pub struct Listing {
    pub id: u32,
    pub names: Vec<u8>,
    pub ents: Vec<RawEnt>,
}

struct Ctx {
    next_id: AtomicU32,
    out: Vec<Mutex<Vec<Listing>>>,
}

thread_local! {
    static BUF: RefCell<Vec<u8>> = RefCell::new(vec![0u8; 256 * 1024]);
}

/// Scan `root` recursively. Listing id 0 is `root` itself.
pub fn scan(root: &[u8], threads: usize) -> Vec<Listing> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .start_handler(|_| crate::no_materialize())
        .build()
        .unwrap();
    let ctx = Ctx {
        next_id: AtomicU32::new(1),
        out: (0..threads + 1).map(|_| Mutex::new(Vec::new())).collect(),
    };
    raise_fd_limit();
    let fd = if blocked(root) {
        -1
    } else {
        CString::new(root).map_or(-1, |c| unsafe { libc::open(c.as_ptr(), OPEN_DIR) })
    };
    // Paths are only tracked when there is something to skip.
    let path = SKIP
        .get()
        .is_some_and(|v| !v.is_empty())
        .then(|| root.to_vec());
    pool.scope(|s| finish_dir(s, fd, path, 0, &ctx));
    let parts: Vec<Vec<Listing>> = ctx
        .out
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect();
    let mut out = Vec::with_capacity(parts.iter().map(Vec::len).sum());
    for p in parts {
        out.extend(p);
    }
    out
}

fn raise_fd_limit() {
    let mut r: libc::rlimit = unsafe { std::mem::zeroed() };
    unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) };
    r.rlim_cur = r.rlim_max.min(65536);
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &r) };
}

/// List a single directory (no recursion). Subdirectories come back with
/// `child == NONE`. Used by the live updater.
pub fn list_one(path: &[u8]) -> Option<Listing> {
    if blocked(path) {
        return None;
    }
    let mut l = Listing {
        id: 0,
        names: Vec::new(),
        ents: Vec::new(),
    };
    if !list_into(path, &mut l) {
        return None;
    }
    Some(l)
}

const OPEN_DIR: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

/// Directory fd shared by the tasks that still need to openat() a child.
struct Fd(i32);
impl Drop for Fd {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

// Children are opened with openat() relative to the parent's fd, so paths
// never get rebuilt. No Endpoint Security tax on Linux: open+close is ~2us,
// and the macOS version's entrycount trick (skip opening empty dirs) has no
// equivalent, so every child dir is opened.
fn finish_dir<'s>(s: &Scope<'s>, fd: i32, path: Option<Vec<u8>>, id: u32, ctx: &'s Ctx) {
    let mut l = Listing {
        id,
        names: Vec::new(),
        ents: Vec::new(),
    };
    if fd < 0 {
        push(l, ctx);
        return;
    }
    list_fd(fd, &mut l);
    let me = std::sync::Arc::new(Fd(fd));
    let mut kids = Vec::new();
    for e in l.ents.iter_mut() {
        if e.kind & 3 == KIND_DIR {
            let name = &l.names[e.name_off as usize..e.name_off as usize + e.name_len as usize];
            let child_path = path.as_ref().map(|p| crate::live::join(p, name));
            if child_path.as_deref().is_some_and(blocked) {
                continue;
            }
            e.child = ctx.next_id.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(name).unwrap_or_default();
            kids.push((name, e.child, child_path));
        }
    }
    push(l, ctx);
    for (name, cid, child_path) in kids {
        let parent = me.clone();
        s.spawn(move |s| {
            let fd = unsafe { libc::openat(parent.0, name.as_ptr(), OPEN_DIR) };
            drop(parent);
            if fd < 0
                && matches!(
                    std::io::Error::last_os_error().raw_os_error(),
                    Some(libc::EPERM) | Some(libc::EACCES)
                )
                && let Some(p) = &child_path
            {
                DENIED.lock().unwrap().push(p.clone());
            }
            finish_dir(s, fd, child_path, cid, ctx);
        });
    }
}

fn push(l: Listing, ctx: &Ctx) {
    LISTED.fetch_add(l.ents.len(), Ordering::Relaxed);
    let slot = rayon::current_thread_index().unwrap_or(ctx.out.len() - 1);
    ctx.out[slot].lock().unwrap().push(l);
}

/// Returns false if the directory could not be opened.
fn list_into(path: &[u8], l: &mut Listing) -> bool {
    let Ok(cpath) = CString::new(path) else {
        return false;
    };
    let fd = unsafe { libc::open(cpath.as_ptr(), OPEN_DIR) };
    if fd < 0 {
        return false;
    }
    list_fd(fd, l);
    unsafe { libc::close(fd) };
    true
}

/// The fields one walk needs from a stat, whichever libc call supplied
/// them: musl has no statx, so it answers through fstatat/fstat.
struct Stat {
    dev: u64,
    mode: u32,
    size: u64,
    mtime: i64,
}

/// The directory's own device id, for FLAG_MOUNT on children. 0 = unknown
/// (stat refused): children then never flag.
#[cfg(not(target_env = "musl"))]
fn dev_of(fd: i32) -> u64 {
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_BASIC_STATS,
            &mut st,
        )
    } != 0
    {
        return 0;
    }
    libc::makedev(st.stx_dev_major, st.stx_dev_minor)
}

#[cfg(target_env = "musl")]
fn dev_of(fd: i32) -> u64 {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return 0;
    }
    st.st_dev
}

#[cfg(not(target_env = "musl"))]
fn stat_at(dir: i32, name: &CString) -> Option<Stat> {
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::statx(
            dir,
            name.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT,
            libc::STATX_ALL,
            &mut st,
        )
    } != 0
    {
        return None;
    }
    Some(Stat {
        dev: libc::makedev(st.stx_dev_major, st.stx_dev_minor),
        mode: st.stx_mode as u32,
        size: st.stx_size,
        mtime: st.stx_mtime.tv_sec,
    })
}

#[cfg(target_env = "musl")]
fn stat_at(dir: i32, name: &CString) -> Option<Stat> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstatat(dir, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
        return None;
    }
    Some(Stat {
        dev: st.st_dev,
        mode: st.st_mode as u32,
        size: st.st_size as u64,
        mtime: st.st_mtime as i64,
    })
}

/// linux_dirent64: ino(8) off(8) reclen(2) type(1) name[].
const DIRENT_HEADER: usize = 19;

fn list_fd(fd: i32, l: &mut Listing) {
    let dev = dev_of(fd);
    BUF.with_borrow_mut(|buf| {
        loop {
            let n = unsafe { libc::syscall(libc::SYS_getdents64, fd, buf.as_mut_ptr(), buf.len()) };
            if n <= 0 {
                break;
            }
            let n = n as usize;
            // Size the listing exactly once the dirents are in: names sit
            // in each record's tail.
            l.ents.reserve(n / 48);
            l.names.reserve(n / 2);
            let mut p = 0usize;
            while p < n {
                let reclen = u16::from_ne_bytes(buf[p + 16..p + 18].try_into().unwrap()) as usize;
                let name = &buf[p + DIRENT_HEADER..p + reclen];
                let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                parse_entry(fd, name, dev, l);
                p += reclen;
            }
        }
    });
}

fn parse_entry(dir: i32, name: &[u8], dev: u64, l: &mut Listing) {
    if name.is_empty() || name == b"." || name == b".." || name.len() > u16::MAX as usize {
        return;
    }
    let Ok(c) = CString::new(name) else { return };
    // Vanished between the listing and the stat.
    let Some(st) = stat_at(dir, &c) else { return };
    let mut kind = match st.mode & libc::S_IFMT {
        libc::S_IFREG => KIND_FILE,
        libc::S_IFDIR => KIND_DIR,
        libc::S_IFLNK => KIND_LINK,
        _ => KIND_OTHER,
    };
    if name[0] == b'.' {
        kind |= FLAG_HIDDEN;
    }
    if kind == KIND_DIR && dev != 0 && st.dev != dev {
        kind |= FLAG_MOUNT;
    }
    // macOS's datalength covers files and links; directories report 0.
    let size = if kind & 3 == KIND_DIR { 0 } else { st.size };
    let mtime = st.mtime.clamp(0, u32::MAX as i64) as u32;
    l.ents.push(RawEnt {
        name_off: l.names.len() as u32,
        name_len: name.len() as u16,
        kind,
        size,
        mtime,
        child: NONE,
    });
    l.names.extend_from_slice(name);
}
