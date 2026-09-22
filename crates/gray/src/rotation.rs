//! Size-capped log rotation: `gray.log` → `.1` → `.2`, best-effort, never panics.
use std::path::Path;

pub(crate) const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Exclusive cross-process guard for the rotate shuffle (audit #19):
/// two processes rotating `gray.log` concurrently each rename `.1`->`.2`
/// and `gray.log`->`.1`, silently losing one segment per renaming loser.
/// Advisory, best-effort: no flock means no guard, exactly like the rest
/// of rotation.
fn lock_rotation(path: &Path) -> Option<std::fs::File> {
    let lock_path = path.with_extension("log.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .ok()?;
    let _ = f.try_lock();
    Some(f)
}

/// True when `handle` no longer writes to `path` — another process rotated
/// the log under us, so our lines are landing in a renamed-away inode
/// (audit #19, second half). Unix compares dev+ino; other platforms skip
/// the check and keep the handle.
#[cfg(unix)]
pub(crate) fn handle_drifted(path: &Path, handle: &std::fs::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(want) = std::fs::metadata(path) else {
        return false; // unreadable path: not a drift we can prove
    };
    let Ok(got) = handle.metadata() else {
        return false;
    };
    (got.dev(), got.ino()) != (want.dev(), want.ino())
}

#[cfg(not(unix))]
pub(crate) fn handle_drifted(_path: &Path, _handle: &std::fs::File) -> bool {
    false
}

/// If `path` exceeds `LOG_MAX_BYTES`, shift `.1`→`.2`, `path`→`.1`, truncate `path`.
/// Missing/small files are left alone. All errors swallowed (logging must not crash boot).
pub(crate) fn rotate_if_needed(path: &Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= LOG_MAX_BYTES {
        return;
    }
    // Serialize the shuffle across processes, then re-anchor: whoever wins
    // the renames creates the fresh file its own handle writes to.
    let _guard = lock_rotation(path);
    let _ = std::fs::remove_file(path.with_extension("log.2"));
    let _ = std::fs::rename(path.with_extension("log.1"), path.with_extension("log.2"));
    let _ = std::fs::rename(path, path.with_extension("log.1"));
    let _ = std::fs::File::create(path);
}

#[path = "rotation_tests.rs"]
#[cfg(test)]
mod tests;
