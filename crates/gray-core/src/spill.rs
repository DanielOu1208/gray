use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

pub const HANDLE_LEN: usize = 16;

pub const MAX_ENTRIES: usize = 256;

const MAX_METER_BYTES: u64 = 1024 * 1024;

#[derive(Debug)]
pub enum SpillError {
    NoHome,

    BadHandle(String),

    NotFound(String),

    Io(String),
}

impl std::fmt::Display for SpillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpillError::NoHome => write!(f, "no $GRAY_HOME to store tool output under"),
            SpillError::BadHandle(h) => write!(
                f,
                "not a spill handle: {h:?} (handles are {HANDLE_LEN} hex characters, e.g. from a `[spilled … gray spill …]` footer)"
            ),
            SpillError::NotFound(h) => write!(
                f,
                "spill {h} is gone — it was evicted (the store keeps the newest {MAX_ENTRIES} results) or belongs to another $GRAY_HOME. Re-run the command to get it back."
            ),
            SpillError::Io(e) => write!(f, "spill store I/O failed: {e}"),
        }
    }
}

impl std::error::Error for SpillError {}

pub fn dir() -> Option<PathBuf> {
    crate::paths::gray_home().map(|home| home.join("spill"))
}

pub fn footer(handle: &str, raw_bytes: usize, raw_lines: usize) -> String {
    format!(
        "\n[spilled {raw_lines} lines / {} B — this is a preview. \
         Read it back with: gray spill head {handle} · gray spill tail {handle} · \
         gray spill grep {handle} <pattern> (-n CONTEXT)]",
        fmt_bytes(raw_bytes)
    )
}

pub fn store(text: &str) -> Option<String> {
    if std::env::var_os("GRAY_NO_SPILL").is_some() {
        return None;
    }
    let dir = dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        log::warn!(target: "gray_spill", "cannot create {}: {e}", dir.display());
        return None;
    }
    restrict_dir(&dir);
    let handle = handle_for(text);
    let path = dir.join(format!("{handle}.txt"));
    write_private(&path, text.as_bytes())?;
    prune(&dir);
    Some(handle)
}

pub fn handle_for(text: &str) -> String {
    let digest = Sha256::digest(text.as_bytes());
    hex(&digest)[..HANDLE_LEN].to_string()
}

pub fn path(handle: &str) -> Result<PathBuf, SpillError> {
    if !is_handle(handle) {
        return Err(SpillError::BadHandle(handle.to_string()));
    }
    let dir = dir().ok_or(SpillError::NoHome)?;
    let path = dir.join(format!("{handle}.txt"));
    if path.is_file() {
        Ok(path)
    } else {
        Err(SpillError::NotFound(handle.to_string()))
    }
}

pub fn read(handle: &str) -> Result<String, SpillError> {
    std::fs::read_to_string(path(handle)?).map_err(|e| SpillError::Io(format!("{e}")))
}

fn is_handle(handle: &str) -> bool {
    handle.len() == HANDLE_LEN
        && handle
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut stored: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();

            if path.extension().is_none_or(|x| x != "txt") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            Some((mtime, path))
        })
        .collect();
    if stored.len() <= MAX_ENTRIES {
        return;
    }
    stored.sort_by_key(|(mtime, _)| *mtime);
    let excess = stored.len() - MAX_ENTRIES;
    for (_, path) in stored.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MeterEvent {
    pub ts: u64,

    pub rule: String,

    pub raw: u64,

    pub sent: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MeterTotals {
    pub events: u64,
    pub raw_bytes: u64,
    pub sent_bytes: u64,

    pub by_rule: std::collections::BTreeMap<String, (u64, u64, u64)>,
}

impl MeterTotals {
    pub fn saved(&self) -> u64 {
        self.raw_bytes.saturating_sub(self.sent_bytes)
    }
    pub fn saved_pct(&self) -> f64 {
        if self.raw_bytes == 0 {
            0.0
        } else {
            self.saved() as f64 / self.raw_bytes as f64 * 100.0
        }
    }
    fn add(&mut self, e: &MeterEvent) {
        self.events += 1;
        self.raw_bytes += e.raw;
        self.sent_bytes += e.sent;
        let slot = self.by_rule.entry(e.rule.clone()).or_default();
        slot.0 += 1;
        slot.1 += e.raw;
        slot.2 += e.sent;
    }
}

pub fn record(event: MeterEvent) {
    let Some(dir) = dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    restrict_dir(&dir);
    let path = dir.join("meter.jsonl");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() >= MAX_METER_BYTES) {
        return;
    }
    if let Ok(line) = serde_json::to_string(&event) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

pub fn events() -> Vec<MeterEvent> {
    let Some(dir) = dir() else { return Vec::new() };
    let Ok(text) = std::fs::read_to_string(dir.join("meter.jsonl")) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

pub fn totals() -> MeterTotals {
    let mut totals = MeterTotals::default();
    for event in events() {
        totals.add(&event);
    }
    totals
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn write_private(path: &Path, bytes: &[u8]) -> Option<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = match opts.open(path) {
        Ok(f) => f,
        Err(e) => {
            log::warn!(target: "gray_spill", "cannot write {}: {e}", path.display());
            return None;
        }
    };
    if f.write_all(bytes).is_ok() {
        Some(())
    } else {
        log::warn!(target: "gray_spill", "short write to {}", path.display());
        None
    }
}

fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Err(err) = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)) {
            log::warn!(target: "gray_spill", "spill dir not restricted to owner: {err}");
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

pub fn fmt_bytes(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let kib = bytes as f64 / KIB;
    if kib < 1024.0 {
        return format!("{kib:.1} KiB");
    }
    format!("{:.1} MiB", kib / KIB)
}
