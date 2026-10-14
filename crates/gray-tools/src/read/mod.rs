pub mod args;
mod bulk;
mod dedup;
mod guard;
pub mod hygiene;
pub mod notices;
mod resolve;
pub mod stream;
mod tail;

#[doc(hidden)]
pub mod testkit;
pub mod window;

use std::sync::Arc;
use std::time::{Instant, SystemTime};

use async_trait::async_trait;
use gray_core::agent::{ToolContext, ToolOutput};
use gray_core::message::ToolDef;
use serde_json::Value;
use serde_json::json;

use crate::{FileLedger, LedgerEntry, Tool, fail, get_opt_u64, get_str, resolve_path};

pub struct ReadTool {
    ledger: Arc<FileLedger>,
}

impl ReadTool {

    pub fn new(ledger: Arc<FileLedger>) -> Self {
        Self { ledger }
    }
}

impl Default for ReadTool {
    fn default() -> Self {
        Self {
            ledger: Arc::new(FileLedger::new()),
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "read",
            "Read a UTF-8 text file. Returns file contents, capped at \
             2000 lines / 50 KiB. Lines are prefixed with `<n>\\t` like \
             cat -n; do not include the prefix when quoting text for edit. \
             Image files (png/jpg/gif/webp) are shown to you as vision. \
             Pass `paths` (files/globs) to read several files at once \
             (limit applies per file).",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path (absolute or relative to cwd)"
                    },
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Files and/or globs to read in one call (sorted, max 200)"
                    },
                    "exclude": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Glob patterns to exclude from paths expansion"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "1-based starting line number (negative -N reads the last N lines)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return (default 2000)"
                    }
                },
                "required": []
            }),
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolOutput {

        if let Some(out) = self.execute_bulk(ctx, &args).await {
            return out;
        }

        let path = match get_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };

        let offset = match tail::get_offset(&args) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let limit = match get_opt_u64(&args, "limit") {
            Ok(v) => v,
            Err(e) => return e,
        };

        let given = resolve_path(&ctx.cwd, &path);
        let full = resolve::resolve_existing(&given).unwrap_or_else(|| given.clone());
        let repaired = (full != given).then(|| {
            notices::repaired_note(&full.display().to_string(), &given.display().to_string())
        });

        if limit == Some(0) {
            return ToolOutput::ok(with_repaired(&repaired, args::LIMIT_ZERO_NOTE.to_string()));
        }

        let display = full.display().to_string();
        if let Err(msg) = guard::check_name(&full, None, &display) {
            return fail(msg);
        }
        if let Err(msg) = guard::check_name(
            &full,
            std::fs::canonicalize(&full).ok().as_deref(),
            &display,
        ) {
            return fail(msg);
        }
        if let Ok(meta) = std::fs::metadata(&full) {
            match guard::check_metadata(&meta, &display) {
                Err(msg) => return fail(msg),

                Ok(guard::MetadataDecision::Directory) => {
                    return ToolOutput::ok(with_repaired(&repaired, notices::directory(&display)));
                }
                Ok(guard::MetadataDecision::RegularFile) => {}
            }
        }

        let win_off = offset.unwrap_or(1);
        if let Some(stub) = dedup::check(
            &self.ledger,
            &full,
            &display,
            win_off,
            limit,
            dedup::enabled(),
        ) {
            return ToolOutput::ok(with_repaired(&repaired, stub.content));
        }

        self.execute_streamed(ctx, &full, &display, &path, offset, limit, &repaired)
            .await
    }
}

impl ReadTool {

    #[allow(clippy::too_many_arguments)]
    async fn execute_streamed(
        &self,
        ctx: &ToolContext,
        full: &std::path::Path,
        display: &str,
        path: &str,
        offset: Option<i64>,
        limit: Option<u64>,
        repaired: &Option<String>,
    ) -> ToolOutput {
        use stream::LineStream;
        let read_failed =
            |e: std::io::Error| fail(format!("read failed for {}: {e}", full.display()));
        let mut s = match LineStream::open(full, display, ctx.cancel.clone()).await {
            Ok(s) => s,
            Err(e) => return read_failed(e),
        };

        if let Some(note) = s.binary_note() {

            if crate::images::is_image_extension(full)
                && let Ok(bytes) = std::fs::read(full)
                && let Ok((mime, out)) = crate::images::normalize_image_bytes(&bytes)
            {
                use base64::Engine as _;
                return ToolOutput::image(
                    with_repaired(
                        repaired,
                        format!("Image read successfully: {}", full.display()),
                    ),
                    mime,
                    base64::engine::general_purpose::STANDARD.encode(&out),
                );
            }
            return ToolOutput::ok(with_repaired(repaired, note.to_string()));
        }
        let file_size = s.file_size();
        let max_lines = window::MAX_LINES;
        let max_bytes = window::MAX_BYTES;
        let max_chars = window::MAX_LINE_CHARS;
        let win_off = offset.unwrap_or(1);

        if file_size == 0 {

            if let Ok(meta) = std::fs::metadata(full) {
                self.ledger.record_read(
                    full,
                    LedgerEntry {
                        mtime: meta.modified().unwrap_or_else(|_| SystemTime::now()),
                        size: 0,
                        content_hash: s.content_hash(),
                        full_view: true,
                        window: (win_off, limit),
                        first_line: 1,
                        last_line: 0,
                        dedup_armed: true,
                        read_at: Instant::now(),
                    },
                );
            }
            return ToolOutput::ok(with_repaired(repaired, notices::empty(display)));
        }

        let tail_n = offset.filter(|o| *o < 0).map(|o| o.unsigned_abs());

        let (raws, first_n, total, has_more, peeked_no, tail_mode);
        if let Some(n) = tail_n {

            let ring = match tail::drain_tail(&mut s, n).await {
                Ok(r) => r,
                Err(e) => return read_failed(e),
            };
            if s.cancelled() {
                return ToolOutput::ok(with_repaired(
                    repaired,
                    notices::cancelled_note(s.line_no()),
                ));
            }
            let total_lines = s.line_no();
            first_n = total_lines.saturating_sub(ring.len()) + 1;
            raws = ring
                .into_iter()
                .map(|rl| window::WindowLine {
                    text: rl.text().into_owned(),
                    overflow_chars: rl.overflow_chars,
                })
                .collect();
            total = Some(total_lines);
            has_more = false;
            peeked_no = None;
            tail_mode = true;
        } else {
            let start = offset.unwrap_or(1).max(1).saturating_sub(1) as usize;

            let mut skipped = 0usize;
            let mut eof = false;
            while skipped < start {
                match s.next_line().await {
                    Err(e) => return read_failed(e),
                    Ok(None) => {
                        eof = true;
                        break;
                    }
                    Ok(Some(_)) => skipped += 1,
                }
            }
            if s.cancelled() {
                return ToolOutput::ok(with_repaired(
                    repaired,
                    notices::cancelled_note(s.line_no()),
                ));
            }

            let mut pending: Option<stream::RawLine> = None;
            if !eof {
                match s.next_line().await {
                    Err(e) => return read_failed(e),
                    Ok(None) => eof = true,
                    Ok(Some(rl)) => pending = Some(rl),
                }
            }
            if s.cancelled() {
                return ToolOutput::ok(with_repaired(
                    repaired,
                    notices::cancelled_note(s.line_no()),
                ));
            }
            if eof {

                let total_lines = s.line_no();
                if start > 0 && start >= total_lines {

                    return ToolOutput::ok(with_repaired(
                        repaired,
                        notices::offset_past_eof(display, start as u64 + 1, total_lines),
                    ));
                }
            }

            let cap = match limit {
                Some(lim) => (lim.min(max_lines as u64)) as usize,
                None => max_lines,
            };
            let mut collected: Vec<window::WindowLine> = Vec::new();
            if let Some(rl) = pending {
                collected.push(window::WindowLine {
                    text: rl.text().into_owned(),
                    overflow_chars: rl.overflow_chars,
                });
            }
            while !eof && collected.len() < cap {
                match s.next_line().await {
                    Err(e) => return read_failed(e),
                    Ok(None) => {
                        eof = true;
                        break;
                    }
                    Ok(Some(rl)) => collected.push(window::WindowLine {
                        text: rl.text().into_owned(),
                        overflow_chars: rl.overflow_chars,
                    }),
                }
            }
            if s.cancelled() {
                return ToolOutput::ok(with_repaired(
                    repaired,
                    notices::cancelled_note(s.line_no()),
                ));
            }

            let mut more = false;
            let mut peeked = None;
            if !eof && collected.len() == cap {
                match s.next_line().await {
                    Err(e) => return read_failed(e),
                    Ok(None) => eof = true,
                    Ok(Some(rl)) => {
                        more = true;
                        peeked = Some(rl.line_no);
                    }
                }
            }
            if s.cancelled() {
                return ToolOutput::ok(with_repaired(
                    repaired,
                    notices::cancelled_note(s.line_no()),
                ));
            }
            first_n = start + 1;
            if eof {
                total = Some(s.line_no());
            } else if stream::should_count_exact(file_size) {
                match s.count_rest_lines_capped(stream::MAX_COUNT_LINES).await {
                    Err(e) => return read_failed(e),
                    Ok((rest, false)) => total = Some(s.line_no() + rest as usize),
                    Ok((_, true)) => {

                        s.discard_hash();
                        total = None;
                    }
                }
                if s.cancelled() {
                    return ToolOutput::ok(with_repaired(
                        repaired,
                        notices::cancelled_note(s.line_no()),
                    ));
                }
            } else {

                total = None;
            }
            raws = collected;
            has_more = more;
            peeked_no = peeked;
            tail_mode = false;
        }

        let w = window::window(first_n, &raws, max_lines, max_bytes, max_chars, has_more);
        let last = first_n.saturating_add(w.shown.len()).saturating_sub(1);

        let mut output = w
            .shown
            .iter()
            .map(|l| gray_core::redaction::redact_for_disclosure(l).into_text())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(cut) = w.cut {

            let next = w.next_offset.unwrap_or(first_n);
            let hint = match cut {
                window::Cut::Lines => match total {

                    Some(t) => notices::line_cap(first_n, last, t),
                    None => notices::line_cap_count_skipped(first_n, last, next, file_size, next),
                },

                window::Cut::Bytes => notices::byte_cap(first_n, last, next),
            };
            output = notices::join(&output, &hint);
        } else if !tail_mode && limit.is_some_and(|lim| raws.len() as u64 == lim) && has_more {

            let lim = limit
                .map(|v| v.min(usize::MAX as u64) as usize)
                .unwrap_or(0);
            let next_offset = first_n + lim;
            match total {
                Some(t) => {
                    let remaining = t.saturating_sub(first_n - 1 + lim);
                    output = notices::join(
                        &output,
                        &format!(
                            "[{remaining} more lines in file. Use offset={next_offset} to continue.]"
                        ),
                    );
                }
                None => {
                    let rest_min =
                        peeked_no.map_or(1, |p| p.saturating_sub(first_n - 1 + lim).max(1));
                    output = notices::join(
                        &output,
                        &format!(
                            "[≥{rest_min} more lines in file (count skipped). \
                             Use offset={next_offset} to continue.]"
                        ),
                    );
                }
            }
        }

        if tail_mode && let Some(t) = total {
            let mut notes = vec![notices::tail_note(raws.len() as u64, t)];
            if let Some(lim) = limit {
                notes.push(notices::limit_ignored_note(lim));
            }
            output = notices::join(&output, &notes.join("\n"));
        }

        if w.clamped > 0 {
            output = notices::join(&output, &notices::clamped(w.clamped));
        }

        let content_hash = s.content_hash();
        let full_view = first_n == 1
            && total.is_some_and(|t| last == t)
            && w.cut.is_none()
            && w.clamped == 0
            && content_hash.is_some();
        if let Ok(meta) = std::fs::metadata(full) {
            self.ledger.record_read(
                full,
                LedgerEntry {
                    mtime: meta.modified().unwrap_or_else(|_| SystemTime::now()),
                    size: file_size,
                    content_hash,
                    full_view,
                    window: (win_off, limit),
                    first_line: first_n,
                    last_line: last,
                    dedup_armed: true,
                    read_at: Instant::now(),
                },
            );
        }

        crate::stats::ToolStats {
            tool: "read",
            class: crate::stats::CLASS_RETRIEVAL,
            path,
            bytes: output.len() as u64,
            lines: output.lines().count() as u64,
            truncated_by: match w.cut {
                Some(window::Cut::Lines) => crate::stats::CUT_LINES,
                Some(window::Cut::Bytes) => crate::stats::CUT_BYTES,
                None => crate::stats::CUT_NONE,
            },
        }
        .report();

        ToolOutput::ok(with_repaired(repaired, output))
    }
}

fn get_str_list(args: &Value, key: &str) -> Vec<String> {
    match args.get(key) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        Some(_) => Vec::new(),
    }
}

fn with_repaired(repaired: &Option<String>, content: String) -> String {
    match repaired {
        Some(note) => format!("{note}\n\n{content}"),
        None => content,
    }
}

impl ReadTool {

    async fn execute_bulk(&self, ctx: &ToolContext, args: &Value) -> Option<ToolOutput> {
        let paths = get_str_list(args, "paths");
        if paths.is_empty() {
            if args.get("path").is_none() || args.get("path").is_some_and(Value::is_null) {
                return Some(fail(notices::MISSING_INPUT_MESSAGE.to_string()));
            }
            return None;
        }
        let excludes = get_str_list(args, "exclude");
        let rels = bulk::expand(&ctx.cwd, &paths, &excludes);
        if rels.is_empty() {
            return Some(fail(notices::no_files_matched(&paths)));
        }

        let mut prior: Vec<(String, Option<LedgerEntry>)> = Vec::with_capacity(rels.len());
        for rel in &rels {
            prior.push((rel.clone(), self.ledger.get(&ctx.cwd.join(rel))));
        }
        let mut rendered: Vec<(String, ToolOutput)> = Vec::with_capacity(rels.len());
        for (i, rel) in rels.iter().enumerate() {
            let mut obj = serde_json::Map::with_capacity(3);
            obj.insert("path".to_string(), Value::String(rel.clone()));
            if let Some(o) = args.get("offset") {
                obj.insert("offset".to_string(), o.clone());
            }
            if let Some(l) = args.get("limit") {
                obj.insert("limit".to_string(), l.clone());
            }
            let single = Value::Object(obj);
            let mut out = self.execute(ctx, single.clone()).await;

            if i == 0 && !out.is_error && out.content.contains("unchanged since your previous read")
            {
                out = self.execute(ctx, single).await;
            }
            rendered.push((rel.clone(), out));
        }
        let sizes: Vec<(String, u64)> = rendered
            .iter()
            .map(|(n, o)| (n.clone(), o.content.len() as u64))
            .collect();
        let (shown, skipped) = bulk::fit_within_cap(&sizes);

        for (rel, old) in &prior {
            if skipped.contains(rel) {
                let full = ctx.cwd.join(rel);
                match old {
                    Some(entry) => self.ledger.record_read(&full, entry.clone()),
                    None => self.ledger.remove(&full),
                }
            }
        }
        let mut blocks = Vec::with_capacity(shown.len());
        let mut images = Vec::new();
        let mut any_error = false;
        for name in &shown {
            let body = rendered.iter().find(|(n, _)| n == name).unwrap();
            if body.1.is_error {
                any_error = true;
            }
            images.extend(body.1.images.iter().cloned());
            blocks.push(format!("{}\n{}", bulk::header(name), body.1.content));
        }

        if !any_error
            && rendered
                .iter()
                .any(|(n, o)| skipped.contains(n) && o.is_error)
        {
            any_error = true;
        }
        let mut out = blocks.join("\n\n");
        if !skipped.is_empty() {
            let note = notices::aggregate_note(shown.len(), rendered.len(), &skipped);
            out = if out.is_empty() {
                note
            } else {
                format!("{out}\n\n{note}")
            };
        }
        Some(ToolOutput {
            content: out,
            is_error: any_error,
            images,
            media: Vec::new(),
        })
    }
}

#[path = "mod_capped_count_tests.rs"]
#[cfg(test)]
mod capped_count_tests;

#[path = "mod_bulk_wiring_tests.rs"]
#[cfg(test)]
mod bulk_wiring_tests;

#[path = "mod_redact_tests.rs"]
#[cfg(test)]
mod redact_tests;
