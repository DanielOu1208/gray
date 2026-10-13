//! Shared tool-output policy: truncation caps and arg-validation helpers.
//!
//! Truncation policy (applied to every tool output): results are capped at
//! 2000 lines / 12 KiB, keeping head + tail with a `[truncated ...]`
//! annotation; error outputs are additionally hard-capped at 2 KiB.
//! (Moved from `gray-tools` so non-core tool crates share
//! the policy without depending on the whole builtin toolset.)
//!
//! A cap is a lossy last resort, so an over-budget result is no longer just
//! amputated: the full original goes to the [`crate::spill`] store and the
//! preview grows a footer naming the handle that reads it back. Same preview,
//! no longer a lie — and when the store cannot be written (no `$GRAY_HOME`,
//! read-only disk) it degrades to the old annotation instead of failing the
//! call.

use serde_json::Value;

use crate::agent::ToolOutput;
use crate::spill::{self, MeterEvent};

/// Maximum number of lines kept in a successful tool output.
pub const MAX_LINES: usize = 2000;
/// Maximum size in bytes of a successful tool output. Sized to the bash
/// tool's inline-view budget: every later request re-reads this, and the
/// spill store keeps the original a `gray spill` call away.
pub const MAX_BYTES: usize = 12 * 1024;
/// Hard cap for error outputs (applied after the general truncation).
pub const MAX_ERROR_BYTES: usize = 2048;

/// Error text for an unknown tool name. The full inventory used to ride
/// this string: for a bridged-in agent it listed the *host's* tools (the
/// wrong set entirely) and for everyone it rendered as a wall of names in
/// the UI. Now terse — with a `did you mean` tail when the name is close
/// to a real one.
pub fn unknown_tool(name: &str, available: &[String]) -> String {
    let mut scored: Vec<(&str, f64)> = available
        .iter()
        .map(|n| (n.as_str(), strsim::jaro_winkler(name, n)))
        .filter(|(_, score)| *score >= 0.87)
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut msg = format!("Tool '{name}' does not exist.");
    let mut names = scored.iter().take(3).map(|(n, _)| format!("'{n}'"));
    if let Some(first) = names.next() {
        let rest: Vec<String> = names.collect();
        msg.push_str(&format!(
            " Did you mean {first}{}?",
            if rest.is_empty() {
                String::new()
            } else {
                format!(", {}", rest.join(", "))
            }
        ));
    }
    msg
}

/// Truncates a successful output: 2000-line / 12 KiB cap, head + tail kept,
/// with a `[truncated N lines / M bytes]` annotation in the middle.
pub fn truncate_output(text: &str) -> String {
    let mut notes: Vec<String> = Vec::new();

    // Line cap: keep first half + last half of the allowed budget. Slice
    // at newline offsets instead of collecting a Vec of every line.
    let total_lines = text.lines().count();
    let body = if total_lines > MAX_LINES {
        let dropped = total_lines - MAX_LINES;
        notes.push(format!("{dropped} lines"));
        let keep = MAX_LINES / 2;
        let nth_nl = |n: usize| {
            text.match_indices('\n')
                .nth(n)
                .map(|(i, _)| i + 1)
                .unwrap_or(text.len())
        };
        // Head = through the keep-th newline; tail = from the line that
        // begins keep lines before the last. One copy, no rejoin.
        let head_end = nth_nl(keep - 1);
        let tail_start = nth_nl(total_lines - keep - 1);
        let mut b = String::with_capacity(head_end + text.len() - tail_start);
        b.push_str(&text[..head_end]);
        b.push_str(&text[tail_start..]);
        if b.ends_with('\n') {
            b.pop();
        }
        b
    } else {
        // `lines()` drops the trailing newline; preserve it verbatim.
        text.to_string()
    };

    // Byte cap on what remains (head + tail around the annotation).
    if body.len() > MAX_BYTES {
        let dropped_bytes = body.len() - MAX_BYTES;
        notes.push(format!("{dropped_bytes} bytes"));
        let half = MAX_BYTES / 2;
        let head_end = body.floor_char_boundary(half);
        let tail_start = body.ceil_char_boundary(body.len() - half);
        format!(
            "{}\n{}\n{}",
            &body[..head_end],
            annotation(&notes),
            &body[tail_start..]
        )
    } else if notes.is_empty() {
        body
    } else {
        // Line-truncated but within byte budget: insert the annotation in
        // the middle without touching the rest of the content.
        let mid = body.lines().count() / 2;
        let split = body
            .match_indices('\n')
            .nth(mid - 1)
            .map(|(i, _)| i + 1)
            .unwrap_or(body.len());
        let mut out = String::with_capacity(body.len() + 40);
        out.push_str(&body[..split]);
        out.push_str(&annotation(&notes));
        out.push('\n');
        out.push_str(&body[split..]);
        out
    }
}

/// Error outputs: general truncation, then a hard 2 KiB head cap.
pub fn truncate_error(text: &str) -> String {
    let truncated = truncate_output(text);
    if truncated.len() > MAX_ERROR_BYTES {
        let cut = truncated.floor_char_boundary(MAX_ERROR_BYTES);
        format!("{}\n[error truncated to 2KiB]", &truncated[..cut])
    } else {
        truncated
    }
}

/// Wraps raw stdout-like text into a successful [`ToolOutput`]. Over budget,
/// the preview carries a handle to the stored original.
pub fn finish(raw: String) -> ToolOutput {
    ToolOutput::ok(recoverable(&raw, truncate_output(&raw)))
}

/// Wraps raw failure text into an error [`ToolOutput`] (capped at 2 KiB, and
/// recoverable when the cap actually cut something).
pub fn fail(raw: String) -> ToolOutput {
    ToolOutput::error(recoverable(&raw, truncate_error(&raw)))
}

/// Grow a truncated preview into a recoverable one: store the original, then
/// name the handle. Returns the preview unchanged when nothing was cut, and
/// also when the store is unavailable — the cap is the floor, never a
/// regression into a hard error.
fn recoverable(original: &str, shown: String) -> String {
    if shown == original {
        return shown;
    }
    let Some(handle) = spill::store(original) else {
        return shown;
    };
    spill::record(MeterEvent {
        ts: spill::now_millis(),
        rule: String::new(),
        raw: original.len() as u64,
        sent: shown.len() as u64,
    });
    let lines = original.lines().count();
    format!("{shown}{}", spill::footer(&handle, original.len(), lines))
}

fn annotation(notes: &[String]) -> String {
    format!("[truncated {}]", notes.join(" / "))
}

/// Required string argument.
pub fn get_str(args: &Value, key: &str) -> Result<String, ToolOutput> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(fail(format!("invalid argument '{key}': expected string"))),
        None => Err(fail(format!("missing required argument '{key}'"))),
    }
}

/// Optional unsigned integer argument (`null`/absent -> `None`).
pub fn get_opt_u64(args: &Value, key: &str) -> Result<Option<u64>, ToolOutput> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            fail(format!(
                "invalid argument '{key}': expected non-negative integer"
            ))
        }),
        Some(_) => Err(fail(format!("invalid argument '{key}': expected integer"))),
    }
}

/// Optional boolean argument (`null`/absent -> `None`).
pub fn get_opt_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolOutput> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(fail(format!("invalid argument '{key}': expected boolean"))),
    }
}

/// Resolves a user-supplied path against the execution cwd; absolute paths
/// are used verbatim.
pub fn resolve_path(cwd: &std::path::Path, p: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}
