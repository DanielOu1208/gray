//! Interactive agents panel — the keyboard-driven sibling of the
//! subagents widget. A plugin's `agent_picker` command outcome opens it;
//! rows and every action ride that plugin's own CLI (`entries` for the
//! list, `view`/`open`/`stop`/`chat` for the verbs), so the panel works
//! for any plugin speaking that surface. Enter opens a floating window on
//! the run's own session (live while it works); 'o' on a finished run hands
//! its child session to `/resume` — the subagent then chats as its own
//! session, the same model opencode and Claude Code use.

use gray_core::message::{ContentBlock, Role};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::composer::TranscriptEntry;
use crate::session_store::SessionEntry;

/// What the dispatch arm does after the modal closes.
pub(crate) enum PanelAction {
    /// `/resume` this session — a finished run's own child session.
    /// The name rides along so the receipt can say whose session it is.
    Resume { session: String, name: String },
    /// Print text to scrollback (open card, view tail, receipts).
    Say(String),
}

#[derive(Debug, Clone)]
struct Row {
    name: String,
    task: String,
    icon: String,
    status: String,
    stats: String,
    activity: String,
    session: String,
    target: String,
    managed: bool,
    running: bool,
    resumable: bool,
}

/// `<plugin> <verb>` argv resolved from the install lock: `cli_argv`
/// first (the command shim), then the sidecar `argv`.
fn plugin_argv(plugin: &str) -> Option<Vec<String>> {
    let home = crate::plugin_cli::home().ok()?;
    let lock: Value =
        serde_json::from_slice(&std::fs::read(home.join("plugins").join("lock.json")).ok()?)
            .ok()?;
    let entry = &lock["plugins"][plugin];
    for key in ["cli_argv", "argv"] {
        if let Some(argv) = entry[key].as_array() {
            let argv: Vec<String> = argv
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            if !argv.is_empty() {
                return Some(argv);
            }
        }
    }
    None
}

/// Raw stdout of one plugin CLI call; stderr tail on failure.
fn spawn_raw(argv: &[String], args: &[&str]) -> Option<String> {
    let (prog, base) = argv.split_first()?;
    let out = std::process::Command::new(prog)
        .args(base)
        .args(args)
        .env(
            "GRAY_BIN",
            crate::exe_watch::on_disk_exe().unwrap_or_else(|_| PathBuf::from("gray")),
        )
        .output()
        .ok()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let msg = err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .unwrap_or("plugin command failed");
        return Some(msg.to_string());
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Most verbs answer `{"content": "…"}`; unwrap that, else the raw text.
fn spawn_text(argv: &[String], args: &[&str]) -> Option<String> {
    let raw = spawn_raw(argv, args)?;
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    Some(
        v["content"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or(raw)
            .trim_end()
            .to_string(),
    )
}

/// Read-only slash-command view used while a turn owns the agent. Bare
/// `/subagents` opens the picker below; explicit read verbs print here.
pub(crate) fn run_subagents_view(args: &[String]) -> Option<String> {
    // Bare `settings` is the modal's job — mid-turn it declines politely;
    // flagged `settings --x v` still writes and prints here.
    if args == ["settings"] {
        return Some(
            "subagents settings: the modal opens when the turn finishes — rerun `/subagents settings`, or set directly with `settings --model/--effort/--context/--max-running`"
                .into(),
        );
    }
    let argv = plugin_argv("subagents")?;
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    spawn_text(&argv, &refs)
}

fn fetch_rows(argv: &[String], all: bool) -> Vec<Row> {
    let mut args = vec!["entries"];
    if all {
        args.push("--all");
    }
    let Some(raw) = spawn_raw(argv, &args) else {
        return Vec::new();
    };
    let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let Some(entries) = v["entries"].as_array() else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|e| Row {
            name: e["name"].as_str().unwrap_or("agent").into(),
            task: e["task"].as_str().unwrap_or("").into(),
            icon: e["icon"].as_str().unwrap_or("·").into(),
            status: e["status"].as_str().unwrap_or("").into(),
            stats: e["stats"].as_str().unwrap_or("").into(),
            activity: e["activity"].as_str().unwrap_or("").into(),
            session: e["session"].as_str().unwrap_or("").into(),
            target: e["target"].as_str().unwrap_or("").into(),
            managed: e["managed"].as_bool().unwrap_or(false),
            running: e["running"].as_bool().unwrap_or(false),
            resumable: e["resumable"].as_bool().unwrap_or(false),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Floating run viewer — the child session as a small gray window.
// ---------------------------------------------------------------------------

/// The one-line chat composer shared by the list and the viewer.
struct ChatInput {
    /// The plugin-verb target (`chat <target> <msg>`).
    target: String,
    /// Display name for the `chat NAME:` prompt.
    name: String,
    buf: String,
}

/// Incremental reader for `sessions/<id>.jsonl`. The file is append-only, so
/// each poll reads only the bytes after the last consumed offset and keeps
/// complete lines — a torn tail waits for the next poll. The viewer replays
/// the WHOLE session from entry 0 — compaction boundaries don't truncate the
/// history the way `resume` (active-suffix only) does.
struct SessionFeed {
    path: PathBuf,
    offset: u64,
    lines_seen: usize,
    cwd: PathBuf,
    entries: Vec<SessionEntry>,
    /// The file shrank or was replaced: already-built content is stale.
    reset: bool,
}

impl SessionFeed {
    /// `None` on an unusable slug (the plugin's own `[A-Za-z0-9_-]{1,128}`
    /// rule) or a missing sessions root. The file itself may not exist yet.
    fn open(session: &str) -> Option<Self> {
        let ok = !session.is_empty()
            && session.len() <= 128
            && session
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !ok {
            return None;
        }
        Some(Self {
            path: crate::session_store::default_root()?.join(format!("{session}.jsonl")),
            offset: 0,
            lines_seen: 0,
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            entries: Vec::new(),
            reset: false,
        })
    }

    /// Consume newly completed lines. True when entries were added.
    fn poll(&mut self) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            self.offset = 0;
            self.lines_seen = 0;
            self.entries.clear();
            self.reset = true;
        }
        if len <= self.offset || file.seek(SeekFrom::Start(self.offset)).is_err() {
            return false;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            return false;
        }
        let Some(last_nl) = buf.iter().rposition(|b| *b == b'\n') else {
            return false;
        };
        self.offset += last_nl as u64 + 1;
        let mut changed = false;
        for line in String::from_utf8_lossy(&buf[..=last_nl]).lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            self.lines_seen += 1;
            if self.lines_seen == 1 {
                // Line 0 is the session header; only `cwd` matters here —
                // tool headers relativize paths against it.
                if let Ok(v) = serde_json::from_str::<Value>(line)
                    && let Some(c) = v["cwd"].as_str()
                {
                    self.cwd = PathBuf::from(c);
                }
                continue;
            }
            if let Ok(entry) = serde_json::from_str::<SessionEntry>(line) {
                self.entries.push(entry);
                changed = true;
            }
        }
        changed
    }
}

/// The open run window: the row's identity for the title bar, the feed that
/// fills `transcript`, the rendered `lines` cache, and the scroll state.
/// `tool_calls` mirrors `replay_session_history`'s pairing map — unmatched
/// calls hang off the tail as empty boxes until their result lands.
struct Viewer {
    target: String,
    session: String,
    name: String,
    icon: String,
    status: String,
    stats: String,
    running: bool,
    managed: bool,
    resumable: bool,
    feed: Option<SessionFeed>,
    transcript: Vec<TranscriptEntry>,
    /// `feed.entries` index up to which `transcript` is built.
    built: usize,
    tool_calls: HashMap<String, (String, Value)>,
    /// `label`/`preview` per wire tool name, resolved from the enabled
    /// plugins' persisted manifests — the live turn's `tool_display_maps`
    /// for a session that has no live agent.
    tool_labels: HashMap<String, String>,
    tool_previews: HashMap<String, String>,
    /// Maps resolve at first feed open; empty is a valid answer for a
    /// builtin-only session, so the flag, not emptiness, marks "tried".
    tools_loaded: bool,
    gap_owed: bool,
    lines: Vec<Line<'static>>,
    /// Width the transcript was built for; markdown is baked at build time,
    /// so a width change replays `entries` from scratch.
    layout_w: u16,
    dirty: bool,
    scroll: u16,
    /// Pinned to the tail: new lines autoscroll until the user scrolls up.
    stick: bool,
    max_scroll: u16,
    view_h: u16,
    last_tick: Instant,
    /// One-shot footer note (verb receipts, refusals).
    note: Option<String>,
}

impl Viewer {
    fn open(row: &Row) -> Self {
        let mut v = Self {
            target: row.target.clone(),
            session: row.session.clone(),
            name: row.name.clone(),
            icon: row.icon.clone(),
            status: row.status.clone(),
            stats: row.stats.clone(),
            running: row.running,
            managed: row.managed,
            resumable: row.resumable,
            feed: None,
            transcript: Vec::new(),
            built: 0,
            tool_calls: HashMap::new(),
            tool_labels: HashMap::new(),
            tool_previews: HashMap::new(),
            tools_loaded: false,
            gap_owed: false,
            lines: Vec::new(),
            layout_w: 0,
            dirty: true,
            scroll: 0,
            stick: true,
            max_scroll: 0,
            view_h: 1,
            last_tick: Instant::now(),
            note: None,
        };
        v.refresh_feed();
        v
    }

    /// Re-sync identity fields against the refreshed row — the run keeps
    /// living while watched: status, stats, a late-arriving session id.
    fn sync_row(&mut self, row: Option<&Row>) {
        let Some(r) = row else {
            return;
        };
        self.name = r.name.clone();
        self.icon = r.icon.clone();
        self.status = r.status.clone();
        self.stats = r.stats.clone();
        self.running = r.running;
        self.managed = r.managed;
        self.resumable = r.resumable;
        if !r.session.is_empty() {
            self.session = r.session.clone();
        }
    }

    /// Open the feed lazily (the session id can arrive after the run starts)
    /// and pull whatever the child appended.
    fn refresh_feed(&mut self) {
        if self.session.is_empty() {
            return;
        }
        if self.feed.is_none() {
            self.feed = SessionFeed::open(&self.session);
        }
        if !self.tools_loaded && self.feed.is_some() {
            let cwd = self
                .feed
                .as_ref()
                .map(|f| f.cwd.clone())
                .unwrap_or_default();
            (self.tool_labels, self.tool_previews) = viewer_tool_maps(&cwd);
            self.tools_loaded = true;
        }
        if let Some(feed) = self.feed.as_mut() {
            feed.poll();
        }
    }

    /// Fold `entries[built..]` into `transcript`. Always the FULL session —
    /// compaction boundaries don't truncate (scrolling up reaches the first
    /// prompt); only a replaced file rebuilds from zero.
    fn build_pending(&mut self) {
        {
            let Some(feed) = self.feed.as_ref() else {
                return;
            };
            if feed.reset || self.built > feed.entries.len() {
                self.transcript.clear();
                self.tool_calls.clear();
                self.gap_owed = false;
                self.built = 0;
            }
        }
        if let Some(feed) = self.feed.as_mut() {
            feed.reset = false;
        }
        let new: Vec<SessionEntry> = self
            .feed
            .as_ref()
            .map(|f| f.entries[self.built..].to_vec())
            .unwrap_or_default();
        if new.is_empty() {
            return;
        }
        let cwd = self
            .feed
            .as_ref()
            .map(|f| f.cwd.clone())
            .unwrap_or_default();
        let w = usize::from(self.layout_w.max(20));
        for e in &new {
            replay_entry(self, e, &cwd, w);
        }
        self.built += new.len();
        self.dirty = true;
    }

    /// Produce `lines` at width `w` through `layout_history` — the margin
    /// funnel the setup backdrop shares with the live transcript. Pending
    /// tool calls hang off the tail as empty boxes, like replay's end flush.
    fn layout(&mut self, w: usize) {
        let w16 = u16::try_from(w).unwrap_or(u16::MAX);
        if self.layout_w != w16 {
            self.layout_w = w16;
            self.transcript.clear();
            self.tool_calls.clear();
            self.gap_owed = false;
            self.built = 0;
        }
        self.build_pending();
        if !self.dirty {
            return;
        }
        let mut blocks = self.transcript.clone();
        let cwd = self.feed.as_ref().map(|f| f.cwd.clone());
        let display = crate::tool_fmt::ToolDisplay {
            labels: Some(&self.tool_labels),
            previews: Some(&self.tool_previews),
        };
        for (name, args) in self.tool_calls.values() {
            blocks.push(TranscriptEntry::Gap(1));
            blocks.push(TranscriptEntry::ToolBoxLines {
                header: crate::tool_fmt::format_tool_card_lines(
                    name,
                    args,
                    cwd.as_deref(),
                    display.label(name),
                    display.preview(name),
                ),
                body: Vec::new(),
            });
            blocks.push(TranscriptEntry::Gap(1));
        }
        self.lines = crate::composer::transcript::layout_history(&blocks, w, 0);
        self.dirty = false;
    }
}

/// `ensure_gap` on the viewer's transcript: one owed boundary, paid once.
fn gap_mark(v: &mut Viewer) {
    if !v.gap_owed {
        v.transcript.push(TranscriptEntry::Gap(1));
        v.gap_owed = true;
    }
}

fn push_block(v: &mut Viewer, e: TranscriptEntry) {
    v.transcript.push(e);
    v.gap_owed = false;
}

/// `Tui::replay_session_history` mirrored onto a `Viewer`'s transcript: same
/// block types, same pairing of tool calls to results, minus the terminal
/// side effects (input-history seeding, usage gauge).
fn replay_entry(v: &mut Viewer, entry: &SessionEntry, cwd: &Path, w: usize) {
    match entry.message.role {
        Role::User => {
            let mut user_text = String::new();
            let mut attached: Vec<PathBuf> = Vec::new();
            for block in &entry.message.content {
                match block {
                    ContentBlock::Text { text } => {
                        if !user_text.is_empty() {
                            user_text.push('\n');
                        }
                        user_text.push_str(text);
                    }
                    ContentBlock::StructuredInput { kind, .. } => {
                        if !user_text.is_empty() {
                            user_text.push('\n');
                        }
                        user_text.push_str("[component event: ");
                        user_text.push_str(kind);
                        user_text.push(']');
                    }
                    ContentBlock::Image { media_type, data }
                    | ContentBlock::Media {
                        media_type, data, ..
                    } => {
                        if let Some(p) = gray_core::paths::materialize_media(media_type, data) {
                            attached.push(p);
                        }
                    }
                    ContentBlock::ToolResult {
                        id,
                        content,
                        is_error,
                    } => replay_tool_result(v, id, content, *is_error, cwd),
                    _ => {}
                }
            }
            if !user_text.is_empty() || !attached.is_empty() {
                gap_mark(v);
                push_block(v, TranscriptEntry::UserPrompt(user_text, attached));
                gap_mark(v);
            }
        }
        Role::Assistant => {
            for block in &entry.message.content {
                match block {
                    ContentBlock::Thinking { text, .. } => {
                        let rows = crate::composer::transcript::thinking_replay_lines(text);
                        if !rows.is_empty() {
                            gap_mark(v);
                            push_block(
                                v,
                                TranscriptEntry::StyledLines {
                                    lines: rows,
                                    hyperlinks: Vec::new(),
                                },
                            );
                        }
                    }
                    ContentBlock::Text { text } => {
                        let (lines, hyperlinks) =
                            crate::composer::transcript::render_markdown_lines(text, Some(w));
                        if !lines.is_empty() {
                            gap_mark(v);
                            push_block(v, TranscriptEntry::StyledLines { lines, hyperlinks });
                        }
                    }
                    ContentBlock::ToolUse { id, name, args } => {
                        v.tool_calls
                            .insert(id.clone(), (name.clone(), args.clone()));
                    }
                    _ => {}
                }
            }
        }
        Role::System => {
            for block in &entry.message.content {
                if let ContentBlock::ToolResult {
                    id,
                    content,
                    is_error,
                } = block
                {
                    replay_tool_result(v, id, content, *is_error, cwd);
                }
            }
        }
    }
}

/// The viewed run's `label`/`preview` per wire tool name: the live turn's
/// `tool_display_maps`, but built without a live agent — read the enabled
/// plugins' persisted manifests (builtins declare neither field, so only
/// plugin tools land in the maps). Display metadata only: a tool missing
/// here still renders its wire-name header.
fn viewer_tool_maps(cwd: &Path) -> (HashMap<String, String>, HashMap<String, String>) {
    let mut defs = Vec::new();
    let Ok(home) = crate::setup::gray_home() else {
        return crate::repl::session::tool_display_maps(&defs);
    };
    let load_lock = |path: PathBuf| {
        gray_plugin::lock::LockFile::load(&path).unwrap_or_else(|_| gray_plugin::lock::LockFile {
            schema: 0,
            plugins: Default::default(),
        })
    };
    let user = load_lock(gray_plugin::lock::lock_path(&home));
    let project = load_lock(gray_plugin::lock::project_lock_path(cwd));
    for name in user.plugins.keys().chain(project.plugins.keys()) {
        if !gray_plugin::lock::effective_enabled(&user, &project, name) {
            continue;
        }
        for dir in [home.join("plugins"), cwd.join(".gray/plugins")] {
            let Ok(raw) = std::fs::read_to_string(dir.join(format!("{name}-manifest.json"))) else {
                continue;
            };
            if let Ok(json) = serde_json::from_str::<Value>(&raw) {
                defs.extend(gray_plugin::manifest_tools(&json));
                break;
            }
        }
    }
    crate::repl::session::tool_display_maps(&defs)
}

/// The replay twin of `replay_tool_result` in transcript/boxes.rs: resolve
/// the matching ToolUse, render header + body, push the card.
fn replay_tool_result(v: &mut Viewer, id: &str, content: &str, is_error: bool, cwd: &Path) {
    let (name, args) = v
        .tool_calls
        .remove(id)
        .map(|(n, a)| (n, Some(a)))
        .unwrap_or_else(|| ("tool".to_string(), None));
    let display = crate::tool_fmt::ToolDisplay {
        labels: Some(&v.tool_labels),
        previews: Some(&v.tool_previews),
    };
    let header = args
        .as_ref()
        .map(|a| {
            crate::tool_fmt::format_tool_card_lines(
                &name,
                a,
                Some(cwd),
                display.label(&name),
                display.preview(&name),
            )
        })
        .unwrap_or_else(|| vec![Line::from(name.clone())]);
    let body = crate::tool_fmt::format_tool_result_lines_with_context(
        &name,
        args.as_ref(),
        content,
        is_error,
        Some(cwd),
    );
    gap_mark(v);
    push_block(v, TranscriptEntry::ToolBoxLines { header, body });
    gap_mark(v);
}

/// Scroll by `delta` rows (negative = up) and recompute bottom-stickiness:
/// landing on the last row re-engages autoscroll, leaving it disengages.
/// Visible window over `lines` as `(top_pad, start, end)`: `top_pad` blank
/// rows above `lines[start..end]`, together filling `view` rows exactly.
/// Bottom-anchored semantics — content shorter than the viewport pins to the
/// bottom edge (pad above); longer content is a contiguous slice and `start`
/// clamps to `len - view`. Every viewport row is assigned, so no hole can
/// open mid-viewport the way `Paragraph::scroll` (top-anchored) left one.
fn viewport_window(len: usize, view: usize, scroll: usize) -> (usize, usize, usize) {
    let start = scroll.min(len.saturating_sub(view));
    let end = (start + view).min(len);
    (view.saturating_sub(end - start), start, end)
}

fn scroll_by(v: &mut Viewer, delta: i64) {
    let next = (i64::from(v.scroll) + delta).clamp(0, i64::from(v.max_scroll));
    v.scroll = next as u16;
    v.stick = v.scroll >= v.max_scroll;
}

/// The run as a borderless, full-screen session view — the "gray session
/// iframe": a one-line header, the full transcript, the chat row (always
/// there: steering and stopping are plugin CLI calls that work mid-turn),
/// and a one-line dim key footer. No frame lines, no backdrop.
fn paint_viewer(frame: &mut ratatui::Frame, v: &mut Viewer, chat: Option<&ChatInput>) {
    let area = frame.area();
    let theme = crate::theme::theme();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Block::default().style(Style::default().bg(theme.surface_bg)),
        area,
    );

    let state = if v.running {
        "running".to_string()
    } else {
        v.status.clone()
    };
    let title = format!(" {} {} · {} · {} ", v.icon, v.name, state, v.stats);
    let title: String = title.chars().take(area.width as usize).collect();
    let footer = if chat.is_some() {
        " enter send · esc unfocus · ↑↓/PgUp/PgDn/Home/End scroll "
    } else {
        " ↑↓/jk scroll · PgUp/PgDn · Home/End · c/i chat · x stop · o resume · esc "
    };
    let footer: String = footer.chars().take(area.width as usize).collect();

    // Header on the top row, footer on the bottom row; the transcript and the
    // chat row fill the band between them. Nothing is drawn as a frame.
    let header_rect = Rect::new(area.x, area.y, area.width, 1);
    let footer_rect = Rect::new(
        area.x,
        area.y + area.height.saturating_sub(1),
        area.width,
        1,
    );
    let inner = Rect::new(
        area.x,
        area.y + 1,
        area.width,
        area.height.saturating_sub(2),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            title,
            Style::default()
                .fg(theme.text_bright)
                .add_modifier(Modifier::BOLD)
                .bg(theme.surface_bg),
        ))),
        header_rect,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            footer,
            Style::default().fg(theme.text_dim).bg(theme.surface_bg),
        ))),
        footer_rect,
    );

    // The chat row is the last line of the band; the transcript is above it.
    let body_h = inner.height.saturating_sub(1);
    let body = Rect {
        height: body_h,
        ..inner
    };
    let input = Rect {
        y: inner.y + body_h,
        height: 1,
        ..inner
    };

    v.view_h = body_h;
    v.layout(inner.width as usize);
    let max = v.lines.len().saturating_sub(body_h as usize);
    let max16 = u16::try_from(max).unwrap_or(u16::MAX);
    if v.stick {
        v.scroll = max16;
    }
    v.scroll = v.scroll.min(max16);
    v.max_scroll = max16;

    if v.lines.is_empty() {
        let hint = if v.session.is_empty() {
            "(no child session yet — nothing to show)"
        } else {
            "(empty transcript)"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  {hint}"),
                Style::default().fg(theme.text_dim).bg(theme.surface_bg),
            ))),
            body,
        );
    } else {
        // Bottom-anchored slice: blank pad above short content, contiguous
        // rows otherwise — every body row is assigned, no mid-viewport hole.
        let (pad, start, end) = viewport_window(v.lines.len(), body_h as usize, v.scroll as usize);
        let mut rows = vec![Line::default(); pad];
        rows.extend(v.lines[start..end].iter().cloned());
        frame.render_widget(Paragraph::new(rows), body);
    }

    let spans = if let Some(c) = chat {
        let mut spans = vec![
            Span::styled(
                " chat ▸ ",
                Style::default().fg(theme.accent).bg(theme.surface_bg),
            ),
            Span::styled(
                c.buf.clone(),
                Style::default().fg(theme.text_body).bg(theme.surface_bg),
            ),
            Span::styled("_", Style::default().fg(theme.accent).bg(theme.surface_bg)),
        ];
        if let Some(n) = &v.note {
            spans.push(Span::styled(
                format!("  {n}"),
                Style::default().fg(theme.text_dim).bg(theme.surface_bg),
            ));
        }
        spans
    } else {
        let mut spans = vec![Span::styled(
            " chat ▸ ",
            Style::default().fg(theme.text_dim).bg(theme.surface_bg),
        )];
        if let Some(n) = &v.note {
            spans.push(Span::styled(
                n.clone(),
                Style::default().fg(theme.text_dim).bg(theme.surface_bg),
            ));
        }
        spans
    };
    frame.render_widget(Paragraph::new(Line::from(spans)), input);
}

/// Modal width: the terminal less a cell of margin each side, so rows get
/// every column. Never narrower than 42 nor wider than the screen.
fn picker_modal_w(term_w: u16) -> u16 {
    term_w.saturating_sub(2).max(42).min(term_w)
}

/// List rows the picker holds: about three-fifths of the screen less the
/// title, status and keys rows. Never more than there are runs, never fewer
/// than three, so a short terminal scrolls instead of hiding runs.
fn picker_list_h(term_h: u16, runs: usize) -> usize {
    (usize::from(term_h) * 3 / 5)
        .saturating_sub(7)
        .max(3)
        .min(runs.max(1))
}

/// The top of the `visible`-row window: the old top clamped into the list,
/// then moved just enough to keep `sel` inside it.
fn picker_scroll_top(runs: usize, sel: usize, top: usize, visible: usize) -> usize {
    let visible = visible.max(1);
    let mut top = top.min(runs.saturating_sub(visible));
    if sel < top {
        top = sel;
    } else if sel >= top + visible {
        top = sel + 1 - visible;
    }
    top
}

/// `1–7 of 8` while the list overflows its window; nothing when it fits.
fn picker_range_label(runs: usize, top: usize, visible: usize) -> Option<String> {
    (runs > visible).then(|| format!("{}–{} of {runs}", top + 1, (top + visible).min(runs)))
}

/// Longest live activity a picker row prints; the task keeps the rest.
const PICKER_ACTIVITY_MAX: usize = 60;

/// `s` cut to at most `max` display cells, a cut ending in `…`.
fn cut_cells(s: &str, max: usize) -> String {
    use crate::text_width::{char_width, display_width};
    if display_width(s) <= max {
        return s.to_string();
    }
    if max <= 1 {
        return "…".chars().take(max).collect();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > max - 1 {
            break;
        }
        used += w;
        out.push(c);
    }
    out.push('…');
    out
}

/// One picker row, exactly `width` cells: `▸ ● name — task · stats · activity`
/// (a finished run ends `· status`). The fixed parts come first, then the task
/// keeps its first 40 cells; the live activity takes what is left (at most 60
/// cells) and is dropped when that is under a dozen; the task takes the rest.
fn picker_row_text(row: &Row, selected: bool, width: usize) -> String {
    use crate::text_width::display_width;
    let mark = if selected { "▸" } else { " " };
    let task = row.task.lines().next().unwrap_or("").trim();
    let activity = row.activity.lines().next().unwrap_or("").trim();
    let head = format!(" {mark} {} {}", row.icon, row.name);
    let sep = if task.is_empty() { "" } else { " — " };
    let stats = format!(" · {}", row.stats);
    let state = if row.running {
        String::new()
    } else {
        format!(" · {}", row.status)
    };
    let spare = width.saturating_sub(
        display_width(&head) + display_width(sep) + display_width(&stats) + display_width(&state),
    );
    let act_room = spare
        .saturating_sub(display_width(task).min(40))
        .min(PICKER_ACTIVITY_MAX);
    let act = if row.running && !activity.is_empty() && act_room >= 15 {
        format!(" · {}", cut_cells(activity, act_room - 3))
    } else {
        String::new()
    };
    let task_room = spare.saturating_sub(display_width(&act));
    let task = if task.is_empty() {
        String::new()
    } else {
        cut_cells(task, task_room)
    };
    let line = cut_cells(&format!("{head}{sep}{task}{stats}{act}{state}"), width);
    let pad = width.saturating_sub(display_width(&line));
    format!("{line}{}", " ".repeat(pad))
}

/// The keyboard-navigable overlay: ↑↓/j/k move, Enter opens the floating
/// run viewer (a small gray window on the child session, live while the
/// run works), o resumes a finished run's session, c chats into the run,
/// x stops, v prints the transcript tail, a widens to every session, r
/// refreshes. Synchronous like the other pickers — plugin calls are local
/// CLI spawns, fast enough to run between key events.
pub(crate) fn run_agents_picker(
    plugin: &str,
    bg: Option<&crate::setup::BackgroundSnapshot>,
) -> anyhow::Result<Option<PanelAction>> {
    run_agents_panel(plugin, bg, false)
}

/// The same picker, opened from inside a running turn. Chat, stop and view
/// all work mid-turn — they ride the plugin CLI, not the parent session — so
/// only `o` (resume, which switches the parent session) waits for the turn
/// to end.
pub(crate) fn run_agents_viewer(
    plugin: &str,
    bg: Option<&crate::setup::BackgroundSnapshot>,
) -> anyhow::Result<Option<PanelAction>> {
    run_agents_panel(plugin, bg, true)
}

fn run_agents_panel(
    plugin: &str,
    bg: Option<&crate::setup::BackgroundSnapshot>,
    in_turn: bool,
) -> anyhow::Result<Option<PanelAction>> {
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, poll, read};
    use ratatui::layout::Rect;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Clear, Paragraph};
    use std::time::Duration;

    let Some(argv) = plugin_argv(plugin) else {
        return Ok(Some(PanelAction::Say(format!(
            "⬢ Agents\n  ⎿ {plugin}: no CLI in the plugin lock — install it first"
        ))));
    };

    let bg_snapshot = bg
        .cloned()
        .unwrap_or_else(crate::setup::BackgroundSnapshot::default_initial);
    let (_session, mut terminal) = crate::setup::open_modal()?;

    let box_bg = crate::theme::theme().surface_bg;
    let accent_peach = crate::theme::theme().accent;
    let text_dim = crate::theme::theme().text_dim;
    let on_selection = crate::theme::theme().on_selection;

    let mut all = false;
    let mut rows = fetch_rows(&argv, all);
    let mut sel: usize = 0;
    let mut scroll_top: usize = 0;
    let mut status = String::new();
    let mut chat_in: Option<ChatInput> = None;
    let mut viewer: Option<Viewer> = None;
    let mut result: Option<PanelAction> = None;

    let out: anyhow::Result<()> = (|| {
        loop {
            if rows.is_empty() {
                sel = 0;
            } else if sel >= rows.len() {
                sel = rows.len() - 1;
            }
            if let Some(vw) = viewer.as_mut()
                && vw.last_tick.elapsed() >= Duration::from_secs(1)
            {
                vw.last_tick = Instant::now();
                if vw.running {
                    rows = fetch_rows(&argv, all);
                    let upd = rows.iter().find(|r| r.target == vw.target).cloned();
                    vw.sync_row(upd.as_ref());
                }
                vw.refresh_feed();
            }
            terminal.draw(|frame| {
                let area = frame.area();
                if area.width < 40 || area.height < 10 {
                    return;
                }
                if let Some(vw) = viewer.as_mut() {
                    paint_viewer(frame, vw, chat_in.as_ref());
                    return;
                }
                crate::setup::render_dimmed_background(frame, &bg_snapshot);

                // Height hugs the rows (three-fifths of the screen at most):
                // title + blank + list + blank + status + keys, plus the
                // one-cell pad top and bottom.
                let list_h = picker_list_h(area.height, rows.len());
                let modal_w = picker_modal_w(area.width);
                let modal_h = (list_h as u16 + 7)
                    .min(area.height.saturating_sub(2))
                    .max(8)
                    .min(area.height);
                let modal_x = (area.width.saturating_sub(modal_w)) / 2;
                let modal_y = (area.height.saturating_sub(modal_h)) / 4;
                let modal_rect = Rect::new(modal_x, modal_y, modal_w, modal_h);

                frame.render_widget(Clear, modal_rect);
                frame.render_widget(
                    Block::default().style(Style::default().bg(box_bg)),
                    modal_rect,
                );

                let pad_x = 2u16;
                let inner = Rect::new(
                    modal_x + pad_x,
                    modal_y + 1,
                    modal_w.saturating_sub(pad_x * 2),
                    modal_h.saturating_sub(2),
                );
                let mut put = |y: u16, line: Line| {
                    frame.render_widget(
                        Paragraph::new(line),
                        Rect::new(inner.x, inner.y + y, inner.width, 1),
                    );
                };

                // The list band holds what the modal's height leaves; the
                // window follows the selection, and the header names it.
                let visible = list_h.min(usize::from(modal_h.saturating_sub(7))).max(1);
                scroll_top = picker_scroll_top(rows.len(), sel, scroll_top, visible);
                let range = picker_range_label(rows.len(), scroll_top, visible)
                    .map(|label| format!("  {label}"))
                    .unwrap_or_default();

                let title = if all {
                    "Agents — all sessions"
                } else {
                    "Agents"
                };
                let esc_str = "esc";
                let pad_len = (inner.width as usize).saturating_sub(
                    title.chars().count() + range.chars().count() + esc_str.chars().count() + 8,
                );
                put(
                    0,
                    Line::from(vec![
                        Span::styled(
                            title,
                            Style::default()
                                .fg(crate::theme::theme().text_bright)
                                .add_modifier(Modifier::BOLD)
                                .bg(box_bg),
                        ),
                        Span::styled(range, Style::default().fg(text_dim).bg(box_bg)),
                        Span::styled(" ".repeat(pad_len), Style::default().bg(box_bg)),
                        Span::styled("a: all/cwd  ", Style::default().fg(text_dim).bg(box_bg)),
                        Span::styled(esc_str, Style::default().fg(text_dim).bg(box_bg)),
                    ]),
                );

                // List band: the visible window, then the status/input line,
                // then the keys hint.
                let list_y = 2u16;
                if rows.is_empty() {
                    put(
                        list_y,
                        Line::from(Span::styled(
                            "  no runs yet — gray subagents run 'task' to start one",
                            Style::default().fg(text_dim).bg(box_bg),
                        )),
                    );
                } else {
                    for r in 0..visible {
                        let idx = scroll_top + r;
                        if idx >= rows.len() {
                            break;
                        }
                        let is_sel = idx == sel;
                        let row_str = picker_row_text(&rows[idx], is_sel, inner.width as usize);
                        let line = if is_sel {
                            Line::from(Span::styled(
                                row_str,
                                Style::default()
                                    .fg(on_selection)
                                    .bg(accent_peach)
                                    .add_modifier(Modifier::BOLD),
                            ))
                        } else {
                            Line::from(Span::styled(
                                row_str,
                                Style::default()
                                    .fg(crate::theme::theme().text_bright)
                                    .bg(box_bg),
                            ))
                        };
                        put(list_y + r as u16, line);
                    }
                }

                // Status / chat-input band, then the keys hint.
                let status_y = inner.height.saturating_sub(2);
                if let Some(c) = &chat_in {
                    let prompt = format!(" chat {}: {}_", c.name, c.buf);
                    let clipped: String = prompt.chars().take(inner.width as usize).collect();
                    put(
                        status_y,
                        Line::from(Span::styled(
                            clipped,
                            Style::default()
                                .fg(accent_peach)
                                .add_modifier(Modifier::BOLD)
                                .bg(box_bg),
                        )),
                    );
                } else {
                    // Idle: the band spells out what the selected row can
                    // do — the affordance, not just the key names.
                    let hint = rows.get(sel).map_or("", |r| {
                        if r.running {
                            " ⏎ watch live · c follow-up · x stop · v tail"
                        } else if r.resumable && in_turn {
                            " ⏎ view · c continues · v tail · o resumes after the turn"
                        } else if r.resumable {
                            " ⏎ view · o steps into its session · c continues · v tail"
                        } else if r.managed {
                            " ⏎ view · v tail · c starts a new turn"
                        } else {
                            " ⏎ view · v tail"
                        }
                    });
                    let text = if status.is_empty() {
                        hint
                    } else {
                        status.as_str()
                    };
                    let clipped: String = text.chars().take(inner.width as usize).collect();
                    if !clipped.is_empty() {
                        put(
                            status_y,
                            Line::from(Span::styled(
                                format!(" {clipped}"),
                                Style::default().fg(text_dim).bg(box_bg),
                            )),
                        );
                    }
                }
                put(
                    inner.height.saturating_sub(1),
                    Line::from(Span::styled(
                        " ↑↓ move · ⏎ view · o resume · c chat · x stop · v tail · a all · r refresh",
                        Style::default().fg(text_dim).bg(box_bg),
                    )),
                );
            })?;

            if !poll(Duration::from_millis(250))? {
                continue;
            }
            let Event::Key(KeyEvent {
                code,
                modifiers,
                kind,
                ..
            }) = read()?
            else {
                continue;
            };
            if kind == KeyEventKind::Release {
                continue;
            }
            use crossterm::event::KeyModifiers;
            if chat_in.is_some() {
                match code {
                    KeyCode::Esc => chat_in = None,
                    KeyCode::Enter => {
                        let Some(c) = chat_in.take() else {
                            continue;
                        };
                        let msg = c.buf.trim().to_string();
                        let receipt = if msg.is_empty() {
                            Some("chat cancelled".to_string())
                        } else {
                            let r = spawn_text(&argv, &["chat", &c.target, &msg])
                                .unwrap_or_else(|| "chat failed".into());
                            rows = fetch_rows(&argv, all);
                            Some(r)
                        };
                        if let Some(vw) = viewer.as_mut() {
                            vw.note = receipt;
                            vw.running = true;
                            let upd = rows.iter().find(|r| r.target == vw.target).cloned();
                            vw.sync_row(upd.as_ref());
                            // The viewer's box is a chat, not a one-shot:
                            // stay focused for the next steer (Esc unfocuses).
                            if !msg.is_empty() {
                                chat_in = Some(ChatInput {
                                    target: c.target,
                                    name: c.name,
                                    buf: String::new(),
                                });
                            }
                        } else if let Some(r) = receipt {
                            status = r;
                        }
                    }
                    // Arrows keep scrolling while the box is focused — it's
                    // an input row, not a mode that swallows the viewport.
                    KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::PageUp
                    | KeyCode::PageDown
                    | KeyCode::Home
                    | KeyCode::End => {
                        if let Some(vw) = viewer.as_mut() {
                            let d = match code {
                                KeyCode::Up => -1,
                                KeyCode::Down => 1,
                                KeyCode::PageUp => -i64::from(vw.view_h.max(1)),
                                KeyCode::PageDown => i64::from(vw.view_h.max(1)),
                                KeyCode::Home => -i64::from(vw.scroll),
                                _ => i64::from(vw.max_scroll),
                            };
                            scroll_by(vw, d);
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(c) = chat_in.as_mut() {
                            c.buf.pop();
                        }
                    }
                    KeyCode::Char(ch) if !modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Some(c) = chat_in.as_mut() {
                            c.buf.push(ch)
                        }
                    }
                    _ => {}
                }
                continue;
            }
            if let Some(vw) = viewer.as_mut() {
                if matches!(code, KeyCode::Esc | KeyCode::Char('q')) {
                    viewer = None;
                    continue;
                }
                match code {
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Up | KeyCode::Char('k') => scroll_by(vw, -1),
                    KeyCode::Down | KeyCode::Char('j') => scroll_by(vw, 1),
                    KeyCode::PageUp => scroll_by(vw, -i64::from(vw.view_h.max(1))),
                    KeyCode::PageDown => scroll_by(vw, i64::from(vw.view_h.max(1))),
                    KeyCode::Home => scroll_by(vw, -i64::from(vw.scroll)),
                    KeyCode::End => scroll_by(vw, i64::from(vw.max_scroll)),
                    KeyCode::Char('c') | KeyCode::Char('i') => {
                        if !vw.managed {
                            vw.note = Some(format!(
                                "{} is a raw worker — nothing to chat through",
                                vw.name
                            ));
                        } else {
                            chat_in = Some(ChatInput {
                                target: vw.target.clone(),
                                name: vw.name.clone(),
                                buf: String::new(),
                            });
                        }
                    }
                    KeyCode::Char('x') => {
                        if vw.running {
                            let receipt = spawn_text(&argv, &["stop", &vw.target])
                                .unwrap_or_else(|| "stop failed".into());
                            rows = fetch_rows(&argv, all);
                            let upd = rows.iter().find(|r| r.target == vw.target).cloned();
                            vw.note = Some(receipt);
                            vw.sync_row(upd.as_ref());
                        } else {
                            vw.note = Some(format!("{} already {}", vw.name, vw.status));
                        }
                    }
                    KeyCode::Char('o') => {
                        if in_turn {
                            vw.note = Some("resume after the current turn".into());
                        } else if !vw.running && vw.resumable {
                            result = Some(PanelAction::Resume {
                                session: vw.session.clone(),
                                name: vw.name.clone(),
                            });
                            break;
                        } else if vw.running {
                            vw.note = Some(format!("{} is still running", vw.name));
                        } else {
                            vw.note = Some(format!("{} has no session to resume", vw.name));
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match code {
                KeyCode::Esc | KeyCode::Char('q') => break,
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Up | KeyCode::Char('k') => {
                    sel = sel.checked_sub(1).unwrap_or(rows.len().saturating_sub(1));
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if !rows.is_empty() {
                        sel = (sel + 1) % rows.len();
                    }
                }
                KeyCode::Enter => {
                    let Some(row) = rows.get(sel) else {
                        continue;
                    };
                    viewer = Some(Viewer::open(row));
                }
                KeyCode::Char('o') => {
                    if in_turn {
                        status = "resume after the current turn".into();
                    } else if let Some(row) = rows.get(sel) {
                        if !row.running && row.resumable {
                            result = Some(PanelAction::Resume {
                                session: row.session.clone(),
                                name: row.name.clone(),
                            });
                            break;
                        }
                        status = if row.running {
                            format!("{} is still running", row.name)
                        } else {
                            format!("{} has no session to resume", row.name)
                        };
                    }
                }
                KeyCode::Char('v') => {
                    if let Some(row) = rows.get(sel) {
                        let text = spawn_text(&argv, &["view", &row.target, "--lines", "200"])
                            .unwrap_or_else(|| "(no transcript)".into());
                        result = Some(PanelAction::Say(text));
                        break;
                    }
                }
                KeyCode::Char('x') => {
                    if let Some(row) = rows.get(sel).cloned() {
                        if row.running {
                            status = spawn_text(&argv, &["stop", &row.target])
                                .unwrap_or_else(|| "stop failed".into());
                        } else {
                            status = format!("{} already {}", row.name, row.status);
                        }
                        rows = fetch_rows(&argv, all);
                    }
                }
                KeyCode::Char('c') => {
                    if let Some(row) = rows.get(sel) {
                        if row.managed {
                            chat_in = Some(ChatInput {
                                target: row.target.clone(),
                                name: row.name.clone(),
                                buf: String::new(),
                            });
                        } else {
                            status =
                                format!("{} is a raw worker — nothing to chat through", row.name);
                        }
                    }
                }
                KeyCode::Char('a') => {
                    all = !all;
                    rows = fetch_rows(&argv, all);
                }
                KeyCode::Char('r') => {
                    rows = fetch_rows(&argv, all);
                    status = "refreshed".into();
                }
                _ => {}
            }
        }
        Ok(())
    })();
    out?;
    Ok(result)
}

/// `/subagents settings` pop-out: the current values are read through the
/// plugin's CLI (`settings` prints its JSON), edited in the ask modal,
/// and saved back through `settings --flags` — the same surface every
/// panel verb rides. Esc closes with no write; "keep" answers change
/// nothing. Returns the line to print, or None on Esc.
pub(crate) fn run_subagent_settings(
    plugin: &str,
    shared: &crate::composer::SharedTui,
) -> Option<String> {
    use crate::ask::{AskOption, AskQuestion};
    let argv = plugin_argv(plugin)?;
    let raw = spawn_raw(&argv, &["settings"])?;
    let Ok(cur) = serde_json::from_str::<Value>(raw.trim()) else {
        return Some(format!("couldn't read {plugin} settings: {}", raw.trim()));
    };
    let cur_of = |k: &str| {
        cur[k]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| "inherit".to_string())
    };
    let cur_max = cur["max_running"].as_u64().unwrap_or(128);

    let opt = |label: &str, desc: &str| AskOption {
        label: label.to_string(),
        description: desc.to_string(),
        opt_in: false,
    };
    let q = |id: &str, question: String, options: Vec<AskOption>| AskQuestion {
        id: id.to_string(),
        header: "subagents".to_string(),
        question,
        options,
        multi: false,
        default_all: false,
    };
    let inherit = || {
        opt(
            "inherit",
            "no subagent default — children follow the session",
        )
    };
    let questions = vec![
        q(
            "model",
            format!("default subagent model? (now: {})", cur_of("model")),
            vec![
                opt("keep", "leave unchanged"),
                inherit(),
                // "None of the above" + notes carry a free-form model id.
            ],
        ),
        q(
            "effort",
            format!("default reasoning effort? (now: {})", cur_of("effort")),
            vec![
                opt("keep", "leave unchanged"),
                opt("off", ""),
                opt("minimal", ""),
                opt("low", ""),
                opt("medium", ""),
                opt("high", ""),
                opt("xhigh", ""),
                opt("max", ""),
                inherit(),
            ],
        ),
        q(
            "context",
            format!("context ceiling per child? (now: {})", cur_of("context")),
            vec![
                opt("keep", "leave unchanged"),
                opt("auto", "model default"),
                opt("64k", ""),
                opt("90k", ""),
                opt("128k", ""),
                opt("200k", ""),
                opt("256k", ""),
                opt("300k", ""),
                opt("1m", ""),
                inherit(),
            ],
        ),
        q(
            "max_running",
            format!("max concurrent subagents? (now: {cur_max})"),
            vec![
                opt("keep", "leave unchanged"),
                opt("8", ""),
                opt("16", ""),
                opt("32", ""),
                opt("64", ""),
                opt("128", ""),
                opt("256", ""),
            ],
        ),
    ];
    let answers = crate::ask::ask_questions_modal(shared, questions)?;

    // A typed note ("Other") beats the picked label — typing means intent.
    let mut flags: Vec<String> = Vec::new();
    for a in &answers {
        let flag = match a.id.as_str() {
            "model" => "--model",
            "effort" => "--effort",
            "context" => "--context",
            "max_running" => "--max-running",
            _ => continue,
        };
        let note = a.answers.iter().find_map(|s| s.strip_prefix("user_note: "));
        let pick = a.answers.iter().find(|s| !s.starts_with("user_note:"));
        let value = note
            .map(str::to_owned)
            .or_else(|| match pick.map(String::as_str) {
                Some("keep") | Some("None of the above") | None => None,
                Some("inherit") => Some("none".to_string()),
                Some(l) => Some(l.to_string()),
            });
        if let Some(v) = value {
            flags.push(flag.to_string());
            flags.push(v);
        }
    }
    if flags.is_empty() {
        return Some("subagents settings — unchanged".to_string());
    }
    let args: Vec<&str> = ["settings"]
        .into_iter()
        .chain(flags.iter().map(String::as_str))
        .collect();
    let out = spawn_raw(&argv, &args).unwrap_or_default();
    match serde_json::from_str::<Value>(out.trim()) {
        Ok(v) if v.is_object() => Some(format!(
            "subagents settings saved — model {} · effort {} · context {} · max-running {}",
            v["model"].as_str().unwrap_or("inherit"),
            v["effort"].as_str().unwrap_or("inherit"),
            v["context"].as_str().unwrap_or("inherit"),
            v["max_running"]
                .as_u64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "?".into())
        )),
        _ => Some(format!("settings failed: {}", out.trim())),
    }
}

#[path = "agents_panel_tests.rs"]
#[cfg(test)]
mod tests;
