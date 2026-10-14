use std::path::{Path, PathBuf};
use std::time::Instant;

use gray_core::agent::ToolOutput;

use crate::ledger::{FileLedger, LedgerEntry};
use crate::shell::contract::INLINE_BUDGET_BYTES;
use crate::{finish, resolve_path};

pub fn enabled() -> bool {
    std::env::var("GRAY_READ_DEDUP").as_deref() != Ok("0")
}

#[derive(Debug, PartialEq)]
pub struct PlainRead {

    pub path: PathBuf,

    pub command: String,

    pub window: (i64, Option<u64>),

    pub whole_file: bool,
}

pub fn plain_read(command: &str, cwd: &Path) -> Option<PlainRead> {
    let command = command.trim();

    if command.is_empty() || command.contains(['|', ';', '&', '>', '<', '`', '*', '?', '\n']) {
        return None;
    }
    let words: Vec<&str> = command.split_whitespace().collect();
    let (path, window, whole_file) = match words.as_slice() {
        ["cat", path] => (*path, (1, None), true),

        ["head", "-n", n, path] => (*path, (1, Some(line_count(n)?)), false),
        ["tail", "-n", n, path] => (*path, (0, Some(line_count(n)?)), false),

        ["sed", "-n", program, path] => sed_window(program.trim_matches('\''), path)?,
        _ => return None,
    };
    if !literal_operand(path) {
        return None;
    }
    let full = resolve_path(cwd, path);

    if !std::fs::metadata(&full).is_ok_and(|m| m.is_file()) {
        return None;
    }
    Some(PlainRead {
        path: full,
        command: command.to_string(),
        window,
        whole_file,
    })
}

const REWRITTEN_BY_SHELL: [char; 12] =
    ['~', '$', '"', '\'', '\\', '(', ')', '{', '}', '[', ']', '#'];

fn literal_operand(path: &str) -> bool {
    !path.starts_with('-') && !path.contains(REWRITTEN_BY_SHELL)
}

fn line_count(n: &str) -> Option<u64> {
    let n: u64 = n.parse().ok()?;
    (n > 0).then_some(n)
}

type SedWindow<'a> = (&'a str, (i64, Option<u64>), bool);

#[allow(clippy::type_complexity)]
fn sed_window<'a>(program: &'a str, path: &'a str) -> Option<SedWindow<'a>> {
    let range = program.strip_suffix('p')?;
    let (from, to) = range.split_once(',')?;
    if from.is_empty() || to.is_empty() || !from.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if to == "$" && from == "1" {
        return Some((path, (1, None), true));
    }
    if !to.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (from, to): (i64, i64) = (from.parse().ok()?, to.parse().ok()?);
    (from >= 1 && to > from).then_some((path, (from, Some((to - from + 1) as u64)), false))
}

pub fn check(ledger: &FileLedger, read: &PlainRead, enable: bool) -> Option<ToolOutput> {
    if !enable {
        return None;
    }
    let entry = ledger.get(&read.path)?;
    if !entry.dedup_armed || entry.window != read.window {
        return None;
    }

    if read.whole_file && !entry.full_view {
        return None;
    }
    let meta = std::fs::metadata(&read.path).ok()?;
    if meta.modified().ok()? != entry.mtime || meta.len() != entry.size {
        return None;
    }
    let stub = stub(&read.command, &read.path);
    ledger.record_read(
        &read.path,
        LedgerEntry {
            dedup_armed: false,
            ..entry
        },
    );
    Some(finish(stub))
}

pub fn record(ledger: &FileLedger, read: &PlainRead, shown_bytes: u64) {
    if shown_bytes > INLINE_BUDGET_BYTES as u64 {
        return;
    }
    let Ok(meta) = std::fs::metadata(&read.path) else {
        return;
    };
    let Ok(mtime) = meta.modified() else {
        return;
    };

    let full_view = read.whole_file && shown_bytes == meta.len();
    let lines = full_view.then(|| count_lines(&read.path));
    let (first_line, last_line) = match read.window {
        (_, None) => (1, lines.unwrap_or(0)),
        (0, Some(n)) => {
            let total = lines.unwrap_or(0);
            (total.saturating_sub(n as usize) + 1, total)
        }
        (first, Some(n)) => (
            first.max(1) as usize,
            (first.max(1) as usize) + n as usize - 1,
        ),
    };
    ledger.record_read(
        &read.path,
        LedgerEntry {
            mtime,
            size: meta.len(),

            content_hash: None,
            full_view,
            window: read.window,
            first_line,
            last_line,
            dedup_armed: true,
            read_at: Instant::now(),
        },
    );
}

fn count_lines(path: &Path) -> usize {
    std::fs::read(path)
        .map(|b| b.iter().filter(|&&c| c == b'\n').count())
        .unwrap_or(0)
}

fn stub(command: &str, path: &Path) -> String {
    format!(
        "[bash: `{command}` showed {} unchanged since your previous read above; \
         content omitted. If that result is no longer visible (compacted or \
         masked), run the command again and it returns in full.]",
        path.display()
    )
}

#[path = "read_dedup_tests.rs"]
#[cfg(test)]
mod tests;
