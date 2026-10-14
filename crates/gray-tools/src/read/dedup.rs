use std::path::Path;

use gray_core::agent::ToolOutput;

use crate::ledger::{FileLedger, LedgerEntry};

pub fn enabled() -> bool {
    std::env::var("GRAY_READ_DEDUP").as_deref() != Ok("0")
}

pub fn check(
    ledger: &FileLedger,
    path: &Path,
    display: &str,
    offset: i64,
    limit: Option<u64>,
    enable: bool,
) -> Option<ToolOutput> {
    if !enable {
        return None;
    }
    let entry = ledger.get(path)?;
    if entry.window != (offset, limit) || !entry.dedup_armed {
        return None;
    }

    if (0..=1).contains(&offset) && !entry.full_view {
        return None;
    }

    if entry.last_line < entry.first_line {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if meta.modified().ok()? != entry.mtime || meta.len() != entry.size {
        return None;
    }
    let stub = super::notices::dedup_stub(display, entry.first_line, entry.last_line);
    ledger.record_read(
        path,
        LedgerEntry {
            dedup_armed: false,
            ..entry
        },
    );
    Some(ToolOutput::ok(stub))
}

#[path = "dedup_tests.rs"]
#[cfg(test)]
mod tests;
