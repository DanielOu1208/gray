//! Unit tests for the run-viewer machinery: the incremental session feed,
/// the replay mirror, gap accounting, and scroll stickiness.
use super::*;
use crate::text_width::display_width;
use gray_core::message::Message;

fn entry(role: Role, blocks: Vec<ContentBlock>) -> SessionEntry {
    SessionEntry {
        compaction_boundary: false,
        entry_id: 0,
        parent_id: None,
        timestamp: 0,
        message: Message::new(role, blocks),
        usage: None,
        duration_ms: None,
    }
}

fn entry_line(e: &SessionEntry) -> String {
    serde_json::to_string(e).unwrap()
}

fn feed_at(path: PathBuf) -> SessionFeed {
    SessionFeed {
        path,
        offset: 0,
        lines_seen: 0,
        cwd: PathBuf::from("/cwd"),
        entries: Vec::new(),
        reset: false,
    }
}

fn viewer_stub() -> Viewer {
    Viewer {
        target: "t".into(),
        session: "s".into(),
        name: "run".into(),
        icon: "✓".into(),
        status: "done".into(),
        stats: "1s".into(),
        running: false,
        managed: true,
        resumable: false,
        feed: None,
        transcript: Vec::new(),
        built: 0,
        tool_calls: HashMap::new(),
        tool_labels: HashMap::new(),
        tool_previews: HashMap::new(),
        tools_loaded: true,
        gap_owed: false,
        lines: Vec::new(),
        layout_w: 40,
        dirty: false,
        scroll: 0,
        stick: true,
        max_scroll: 0,
        view_h: 1,
        last_tick: Instant::now(),
        note: None,
    }
}

fn tmp_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gray-viewer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn feed_consumes_complete_lines_and_waits_on_torn_tail() {
    let path = tmp_path("tail.jsonl");
    let e = entry(Role::User, vec![ContentBlock::text("hi")]);
    let mut text = String::from("{\"cwd\":\"/tmp/child\"}\n");
    text.push_str(&entry_line(&e));
    text.push('\n');
    text.push_str(&entry_line(&e)); // no trailing newline: a torn tail
    std::fs::write(&path, &text).unwrap();

    let mut feed = feed_at(path.clone());
    assert!(feed.poll());
    assert_eq!(feed.entries.len(), 1);
    assert_eq!(feed.cwd, PathBuf::from("/tmp/child"));

    // Completing the tail lets the next poll take the second entry.
    std::fs::write(&path, format!("{text}\n")).unwrap();
    assert!(feed.poll());
    assert_eq!(feed.entries.len(), 2);
    assert!(!feed.poll());
}

#[test]
fn feed_keeps_the_full_session_past_a_compaction_boundary() {
    // The viewer replays EVERYTHING — pre-boundary history included — so
    // scrolling up reaches the first prompt. `resume` loads only the active
    // suffix; the viewer is a transcript, not a resume.
    let path = tmp_path("boundary.jsonl");
    let mut text = String::from("{\"cwd\":\"/c\"}\n");
    for _ in 0..3 {
        text.push_str(&entry_line(&entry(
            Role::Assistant,
            vec![ContentBlock::text("old")],
        )));
        text.push('\n');
    }
    let mut boundary = entry(Role::System, vec![ContentBlock::text("summary")]);
    boundary.compaction_boundary = true;
    text.push_str(&entry_line(&boundary));
    text.push('\n');
    text.push_str(&entry_line(&entry(
        Role::Assistant,
        vec![ContentBlock::text("new")],
    )));
    text.push('\n');
    std::fs::write(&path, &text).unwrap();

    let mut feed = feed_at(path);
    assert!(feed.poll());
    assert_eq!(feed.entries.len(), 5, "nothing dropped at the boundary");
    assert!(feed.entries[3].compaction_boundary);
}

#[test]
fn build_pending_replays_the_full_history() {
    // Full transcript, compaction history and all: pre-boundary entries
    // render so scrolling up reaches the session's start.
    let mut v = viewer_stub();
    let mut feed = feed_at(PathBuf::from("/nonexistent"));
    feed.entries
        .push(entry(Role::Assistant, vec![ContentBlock::text("stale")]));
    let mut boundary = entry(Role::System, vec![ContentBlock::text("boundary")]);
    boundary.compaction_boundary = true;
    feed.entries.push(boundary);
    feed.entries
        .push(entry(Role::Assistant, vec![ContentBlock::text("fresh")]));
    v.feed = Some(feed);
    v.build_pending();
    let text: String = v
        .transcript
        .iter()
        .flat_map(|e| match e {
            TranscriptEntry::StyledLines { lines, .. } => lines
                .iter()
                .flat_map(|l| l.spans.iter().map(|s| s.content.to_string()))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect();
    assert!(text.contains("stale"), "transcript: {text}");
    assert!(text.contains("fresh"), "transcript: {text}");
}

#[test]
fn replay_pairs_tool_calls_with_results() {
    let mut v = viewer_stub();
    let cwd = Path::new("/c");
    replay_entry(
        &mut v,
        &entry(
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "t1",
                "bash",
                serde_json::json!({"command": "ls"}),
            )],
        ),
        cwd,
        60,
    );
    // The call is pending: registered, not yet boxed.
    assert!(v.tool_calls.contains_key("t1"));
    assert!(!v.transcript.iter().any(|e| {
        matches!(
            e,
            TranscriptEntry::ToolBox { .. } | TranscriptEntry::ToolBoxLines { .. }
        )
    }));

    replay_entry(
        &mut v,
        &entry(
            Role::User,
            vec![ContentBlock::ToolResult {
                id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        ),
        cwd,
        60,
    );
    assert!(v.tool_calls.is_empty());
    assert_eq!(
        v.transcript
            .iter()
            .filter(|e| matches!(e, TranscriptEntry::ToolBoxLines { .. }))
            .count(),
        1
    );
}

#[test]
fn replayed_tool_cards_carry_the_session_tool_labels() {
    let mut v = viewer_stub();
    v.tool_labels = HashMap::from([("my_tool".to_string(), "Spawn agent".to_string())]);
    let cwd = Path::new("/c");
    replay_entry(
        &mut v,
        &entry(
            Role::Assistant,
            vec![ContentBlock::tool_use(
                "t1",
                "my_tool",
                serde_json::json!({"query": "x"}),
            )],
        ),
        cwd,
        60,
    );
    replay_entry(
        &mut v,
        &entry(
            Role::User,
            vec![ContentBlock::ToolResult {
                id: "t1".into(),
                content: "ok".into(),
                is_error: false,
            }],
        ),
        cwd,
        60,
    );
    let header = v
        .transcript
        .iter()
        .find_map(|e| match e {
            TranscriptEntry::ToolBoxLines { header, .. } => Some(header),
            _ => None,
        })
        .expect("a tool card");
    assert!(header[0].to_string().contains("Spawn agent"), "{header:?}");
}

#[test]
fn replay_user_prompt_gets_card_and_gaps() {
    let mut v = viewer_stub();
    replay_entry(
        &mut v,
        &entry(Role::User, vec![ContentBlock::text("do the thing")]),
        Path::new("/c"),
        60,
    );
    assert!(matches!(v.transcript[0], TranscriptEntry::Gap(_)));
    assert!(matches!(
        &v.transcript[1],
        TranscriptEntry::UserPrompt(t, a) if t == "do the thing" && a.is_empty()
    ));
    assert!(matches!(v.transcript[2], TranscriptEntry::Gap(_)));
}

#[test]
fn gap_marks_collapse_between_blocks() {
    let mut v = viewer_stub();
    gap_mark(&mut v);
    gap_mark(&mut v);
    push_block(
        &mut v,
        TranscriptEntry::StyledLines {
            lines: vec![Line::from("x")],
            hyperlinks: Vec::new(),
        },
    );
    gap_mark(&mut v);
    gap_mark(&mut v);
    let gaps = v
        .transcript
        .iter()
        .filter(|e| matches!(e, TranscriptEntry::Gap(_)))
        .count();
    assert_eq!(gaps, 2);
}

#[test]
fn viewport_window_bottom_anchors_short_content() {
    // Shorter than the viewport: blank pad above, content pinned to the
    // bottom edge — and scroll can't move it anywhere.
    assert_eq!(viewport_window(10, 15, 0), (5, 0, 10));
    assert_eq!(viewport_window(10, 15, 5), (5, 0, 10));
    assert_eq!(viewport_window(0, 15, 0), (15, 0, 0));
}

#[test]
fn viewport_window_slices_long_content_contiguously() {
    // Longer than the viewport: contiguous slice, clamped at both ends —
    // Home reaches the first prompt, the tail sticks at the bottom.
    assert_eq!(viewport_window(100, 15, 0), (0, 0, 15));
    assert_eq!(viewport_window(100, 15, 80), (0, 80, 95));
    assert_eq!(viewport_window(100, 15, 95), (0, 85, 100));
    assert_eq!(viewport_window(100, 15, 200), (0, 85, 100));
    assert_eq!(viewport_window(15, 15, 0), (0, 0, 15));
}

#[test]
fn scroll_by_sticks_to_bottom_until_user_leaves() {
    let mut v = viewer_stub();
    v.max_scroll = 10;
    v.scroll = 10;
    v.stick = true;

    scroll_by(&mut v, -3);
    assert_eq!(v.scroll, 7);
    assert!(!v.stick);

    scroll_by(&mut v, 3);
    assert_eq!(v.scroll, 10);
    assert!(v.stick);

    // Clamped at the ends; bottom keeps the stick.
    scroll_by(&mut v, 100);
    assert_eq!(v.scroll, 10);
    assert!(v.stick);
    scroll_by(&mut v, -100);
    assert_eq!(v.scroll, 0);
    assert!(!v.stick);
}

// ---------------------------------------------------------------------------
// Picker layout: width, height, the scroll window and its header label, and
// the row text at 80 and 200 columns.
// ---------------------------------------------------------------------------

fn picker_row(name: &str, running: bool) -> Row {
    Row {
        name: name.into(),
        task: "Find every place the session file is written and summarize the locking discipline each writer uses".into(),
        icon: if running { "●" } else { "✓" }.into(),
        status: if running { "running" } else { "completed" }.into(),
        stats: "2m36s · Haiku 5.5".into(),
        activity: "rg -n 'session' src/session_store.rs | head -40".into(),
        session: String::new(),
        target: name.into(),
        managed: true,
        running,
        resumable: false,
    }
}

#[test]
fn picker_spans_the_terminal_width() {
    assert_eq!(picker_modal_w(200), 198);
    assert_eq!(picker_modal_w(80), 78);
    // Narrow screens keep a 42-cell floor and never exceed the screen.
    assert_eq!(picker_modal_w(44), 42);
    assert_eq!(picker_modal_w(30), 30);
}

#[test]
fn picker_height_follows_the_screen() {
    // Eight runs fit a 40-row screen; a 24-row one shows seven and scrolls.
    assert_eq!(picker_list_h(40, 8), 8);
    assert_eq!(picker_list_h(24, 8), 7);
    // Tall screens never add rows beyond the run count.
    assert_eq!(picker_list_h(120, 8), 8);
    assert_eq!(picker_list_h(80, 50), 41);
    // Never fewer than three rows; an empty list still reserves one.
    assert_eq!(picker_list_h(10, 8), 3);
    assert_eq!(picker_list_h(40, 0), 1);
}

#[test]
fn scrolling_reveals_the_eighth_run() {
    // Eight runs in a seven-row window: walking down from the top, run 8
    // (index 7) pulls the window down until it is the last row shown.
    let (runs, visible) = (8, 7);
    let mut top = 0;
    for sel in 0..runs {
        top = picker_scroll_top(runs, sel, top, visible);
        assert!(
            (top..top + visible).contains(&sel),
            "run {} outside rows {top}..",
            sel + 1
        );
    }
    assert_eq!(top, 1);
    // Wrapping back to run 1 pulls the window up again.
    assert_eq!(picker_scroll_top(runs, 0, top, visible), 0);
    // A window left past the end (runs went away, the screen grew) snaps back.
    assert_eq!(picker_scroll_top(runs, 7, 5, 8), 0);
}

#[test]
fn range_label_names_the_window_only_when_it_scrolls() {
    assert_eq!(picker_range_label(8, 0, 7).as_deref(), Some("1–7 of 8"));
    assert_eq!(picker_range_label(8, 1, 7).as_deref(), Some("2–8 of 8"));
    assert_eq!(picker_range_label(8, 0, 8), None);
    assert_eq!(picker_range_label(0, 0, 1), None);
}

#[test]
fn picker_rows_use_every_column_at_200() {
    let run = picker_row("fan-audit", true);
    let wide = picker_row_text(&run, true, 200);
    assert_eq!(display_width(&wide), 200, "{wide}");
    assert!(
        wide.starts_with(" ▸ ● fan-audit — Find every place"),
        "{wide}"
    );
    assert!(wide.contains("each writer uses · 2m36s"), "{wide}");
    assert!(
        wide.contains("rg -n 'session' src/session_store.rs | head -40"),
        "{wide}"
    );
    assert!(!wide.contains('…'), "nothing is cut at 200 columns: {wide}");
}

#[test]
fn picker_rows_cut_with_an_ellipsis_at_80() {
    let run = picker_row("fan-audit", true);
    let narrow = picker_row_text(&run, false, 80);
    assert_eq!(display_width(&narrow), 80, "{narrow}");
    assert!(
        narrow.starts_with("   ● fan-audit — Find every"),
        "{narrow}"
    );
    assert!(narrow.contains('…'), "{narrow}");
    // The task keeps its first cells and the stats survive; with too little
    // room left the live activity is dropped rather than squeezing the task.
    assert!(narrow.contains("Find every place the session"), "{narrow}");
    assert!(narrow.contains("· 2m36s · Haiku 5.5"), "{narrow}");
    assert!(!narrow.contains("rg -n"), "{narrow}");
}

#[test]
fn picker_shows_activity_once_the_task_has_its_room() {
    let run = picker_row("fan-audit", true);
    let mid = picker_row_text(&run, false, 120);
    assert_eq!(display_width(&mid), 120, "{mid}");
    assert!(mid.contains("Find every place the session file"), "{mid}");
    assert!(
        mid.contains("· 2m36s · Haiku 5.5 · rg -n 'session'"),
        "{mid}"
    );
    assert!(mid.contains('…'), "the activity is cut too: {mid}");
}

#[test]
fn finished_rows_end_with_their_status_and_no_activity() {
    let done = picker_row("fan-math", false);
    let text = picker_row_text(&done, false, 200);
    assert!(
        text.trim_end().ends_with("· 2m36s · Haiku 5.5 · completed"),
        "{text}"
    );
    assert!(
        !text.contains("rg -n"),
        "a finished run shows no live activity: {text}"
    );
}

#[test]
fn selected_row_is_marked_and_fills_the_width() {
    let run = picker_row("fan-count", true);
    assert!(picker_row_text(&run, true, 60).starts_with(" ▸ "));
    assert!(picker_row_text(&run, false, 60).starts_with("   "));
    assert_eq!(display_width(&picker_row_text(&run, true, 60)), 60);
}

#[test]
fn cut_cells_ends_a_cut_inside_the_budget() {
    assert_eq!(cut_cells("short", 10), "short");
    assert_eq!(cut_cells("abcdefgh", 5), "abcd…");
    assert_eq!(cut_cells("日本語テキスト", 5), "日本…");
    assert_eq!(cut_cells("abc", 1), "…");
    assert_eq!(cut_cells("abc", 0), "");
}
