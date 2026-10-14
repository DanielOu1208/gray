use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::Hasher;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

pub const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct LedgerEntry {
    pub mtime: SystemTime,

    pub size: u64,

    pub content_hash: Option<u64>,

    pub full_view: bool,

    pub window: (i64, Option<u64>),

    pub first_line: usize,

    pub last_line: usize,

    pub dedup_armed: bool,

    pub read_at: Instant,
}

#[derive(Debug, Default)]
pub struct FileLedger {
    inner: Mutex<HashMap<PathBuf, LedgerEntry>>,
}

impl FileLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    pub fn hash_bytes(bytes: &[u8]) -> Option<u64> {
        if bytes.len() as u64 > MAX_HASH_BYTES {
            return None;
        }
        let mut h = DefaultHasher::new();
        h.write(bytes);
        Some(h.finish())
    }

    pub fn record_read(&self, path: &Path, entry: LedgerEntry) {
        self.inner
            .lock()
            .expect("FileLedger lock poisoned")
            .insert(Self::key(path), entry);
    }

    pub fn get(&self, path: &Path) -> Option<LedgerEntry> {
        self.inner
            .lock()
            .expect("FileLedger lock poisoned")
            .get(&Self::key(path))
            .cloned()
    }

    pub fn remove(&self, path: &Path) {
        self.inner
            .lock()
            .expect("FileLedger lock poisoned")
            .remove(&Self::key(path));
    }

    pub fn mark_written(&self, path: &Path, new_bytes: &[u8]) {
        let (mtime, size) = std::fs::metadata(path)
            .map(|m| (m.modified().unwrap_or_else(|_| SystemTime::now()), m.len()))
            .unwrap_or_else(|_| (SystemTime::now(), new_bytes.len() as u64));
        let newlines = new_bytes.iter().filter(|&&b| b == b'\n').count();
        let lines = if new_bytes.is_empty() {
            0
        } else {
            newlines + usize::from(!new_bytes.ends_with(b"\n"))
        };
        self.record_read(
            path,
            LedgerEntry {
                mtime,
                size,
                content_hash: Self::hash_bytes(new_bytes),
                full_view: true,
                window: (1, None),
                first_line: 1,
                last_line: lines,
                dedup_armed: false,
                read_at: Instant::now(),
            },
        );
    }

    pub fn disarm_all_dedup(&self) {
        for entry in self
            .inner
            .lock()
            .expect("FileLedger lock poisoned")
            .values_mut()
        {
            entry.dedup_armed = false;
        }
    }

    pub fn clear(&self) {
        self.inner.lock().expect("FileLedger lock poisoned").clear();
    }
}

#[path = "ledger_tests.rs"]
#[cfg(test)]
mod tests;
