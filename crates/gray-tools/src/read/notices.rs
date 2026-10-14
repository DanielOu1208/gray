use crate::truncate::format_size;

pub fn directory(display: &str) -> String {
    format!("[read: {display} is a directory. Use ls or find.]")
}

pub fn empty(display: &str) -> String {
    format!("[read: {display} is empty (0 bytes)]")
}

pub fn tail_suggestion(total_lines: usize) -> usize {
    total_lines.saturating_sub(49).max(1)
}

pub fn offset_past_eof(display: &str, requested: u64, total_lines: usize) -> String {
    format!(
        "[read: offset {requested} is beyond the end of {display} ({total_lines} lines). \
         Retry with offset={} to see the tail, or offset=1.]",
        tail_suggestion(total_lines)
    )
}

pub fn line_cap(first: usize, last: usize, total: usize) -> String {
    format!(
        "[read: showing lines {first}-{last} of {total}. Continue with offset={}.]",
        last + 1
    )
}

pub fn byte_cap(first: usize, last: usize, next: usize) -> String {
    format!(
        "[read: showing lines {first}-{last} (50 KiB budget). \
         Continue with offset={next} — that line was not shown.]"
    )
}

pub fn line_cap_count_skipped(
    first: usize,
    last: usize,
    min_total: usize,
    file_size: u64,
    next: usize,
) -> String {
    format!(
        "[read: showing lines {first}-{last} of {}. Continue with offset={next}.]",
        count_skipped_total(min_total, file_size)
    )
}

pub fn clamped(count: usize) -> String {
    format!(
        "[read: {count} line(s) longer than 2000 chars were clamped; \
         use grep -n or bash cut -c to inspect a specific one.]"
    )
}

pub fn tail_note(shown: u64, total: usize) -> String {
    let first = total as u64 - shown + 1;
    format!("[read: last {shown} lines of {total} (lines {first}-{total})]")
}

pub fn limit_ignored_note(limit: u64) -> String {
    format!(
        "[read: limit={limit} ignored with negative offset; showing the tail instead. \
         Omit limit when offset is negative.]"
    )
}

pub fn nul_note(display: &str) -> String {
    format!("[read: {display} looks binary (NUL bytes), not shown]")
}

pub fn cancelled_note(lines_read: usize) -> String {
    format!("[read: cancelled after {lines_read} lines]")
}

pub fn count_skipped_total(min_total: usize, file_size: u64) -> String {
    format!(
        "≥{min_total} lines (file is {}, count skipped)",
        format_size(file_size as usize)
    )
}

pub fn repaired_note(actual: &str, given: &str) -> String {
    format!("[read: opened {actual} (path repaired from {given})]")
}

pub const MISSING_INPUT_MESSAGE: &str =
    "read: provide path (one file) or paths (list of files/globs)";

const MAX_NOTE_NAMES: usize = 10;

pub fn aggregate_note(shown: usize, total: usize, skipped: &[String]) -> String {
    let left = total.saturating_sub(shown);
    let names: Vec<&str> = skipped
        .iter()
        .take(MAX_NOTE_NAMES)
        .map(String::as_str)
        .collect();
    let list = if skipped.len() > MAX_NOTE_NAMES {
        format!("{}, …", names.join(", "))
    } else {
        names.join(", ")
    };
    format!(
        "[read: showed {shown} of {total} files; {left} skipped (over 100 KiB total): {list}. \
         Read them individually or narrow the glob.]"
    )
}

pub fn no_files_matched(paths: &[String]) -> String {
    let mut shown = paths.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 3 {
        shown.push_str(", …");
    }
    format!(
        "read failed: no files matched {} pattern(s): {shown}. \
         Check the globs and exclude[].",
        paths.len()
    )
}

pub fn write_unread(display: &str) -> String {
    format!(
        "write refused: {display} exists and has not been read this session. \
         Read it first (read {display}), or pass force=true to overwrite blind."
    )
}

pub fn write_changed(display: &str) -> String {
    format!("write refused: {display} changed on disk since you read it. Re-read it.")
}

pub fn edit_changed(display: &str) -> String {
    format!("edit refused: {display} changed on disk since you read it. Re-read it.")
}

pub fn write_partial(display: &str, first: usize, last: usize, total: usize) -> String {
    format!(
        "write refused: only part of {display} has been read \
         (lines {first}-{last} of {total}). Read the rest (offset={}) \
         or use edit for a targeted change.",
        last + 1
    )
}

pub fn dedup_stub(display: &str, first: usize, last: usize) -> String {
    format!(
        "[read: {display} lines {first}-{last} unchanged since your previous read above; \
         content omitted. If that result is no longer visible (compacted), \
         call read again and it will be returned in full.]"
    )
}

pub fn join(content: &str, note: &str) -> String {
    if content.is_empty() {
        note.to_string()
    } else {
        format!("{content}\n\n{note}")
    }
}

#[path = "notices_tests.rs"]
#[cfg(test)]
mod tests;
