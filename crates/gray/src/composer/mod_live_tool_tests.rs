use super::*;
use ratatui::text::{Line, Span};

fn live(id: &str, text: &str, running: bool) -> LiveTool {
    LiveTool {
        id: id.to_string(),
        name: id.to_string(),
        header: vec![Line::from(vec![Span::raw(text.to_string())])],
        running,
        tail: LiveToolTail::default(),
    }
}

fn row_text(l: &Line<'_>) -> String {
    l.spans.iter().map(|s| s.content.as_ref()).collect()
}

#[test]
fn live_rows_preserve_order_and_cap_at_three() {
    let tools: Vec<LiveTool> = (0..5)
        .map(|i| live(&format!("t{i}"), &format!("tool {i}"), false))
        .collect();
    let rows = live_tool_rows(&tools, Duration::ZERO, 80);
    assert_eq!(rows.len(), 3, "cap mirrors the queued preview");
    assert_eq!(row_text(&rows[0]), "tool 0");
    assert_eq!(row_text(&rows[2]), "tool 2");
    assert_eq!(live_tool_overflow(tools.len()), 2);
    assert_eq!(live_tool_overflow(3), 0);
    assert_eq!(live_tool_overflow(0), 0);
}

fn bash_live(command: &str, running: bool) -> LiveTool {
    LiveTool {
        id: "bash".into(),
        name: "bash".into(),
        header: crate::tool_fmt::format_tool_call_lines(
            "bash",
            &serde_json::json!({"command": command}),
            None,
            None,
        ),
        running,
        tail: LiveToolTail::default(),
    }
}

#[test]
fn live_rows_mark_running_cards() {
    let tool = bash_live("cargo test", true);
    let original = tool.header.clone();
    let rows = live_tool_rows(std::slice::from_ref(&tool), Duration::ZERO, 80);
    assert_eq!(row_text(&rows[0]), "⬡ Running cargo test");
    assert_eq!(row_text(&tool.header[0]), row_text(&original[0]));
    assert_eq!(rows[0].spans.last(), original[0].spans.last());
    let later = live_tool_rows(&[tool], Duration::from_secs(1), 80);
    assert_eq!(row_text(&rows[0]), row_text(&later[0]));
    assert_ne!(
        rows[0], later[0],
        "label must shimmer without changing text"
    );
    let preparing = bash_live("cargo test", false);
    assert_eq!(
        live_tool_rows(std::slice::from_ref(&preparing), Duration::ZERO, 80)[0],
        preparing.header[0]
    );
}

#[test]
fn live_rows_update_in_place_without_reorder() {
    // pi `updateArgs`: same id replaces the header, first-seen order kept.
    let mut tools = vec![live("a", "old", false), live("b", "bee", false)];
    // mirror of `upsert_live_tool` ordering (pure, no Tui::new/TTY):
    if let Some(slot) = tools.iter_mut().find(|t| t.id == "a") {
        slot.header = vec![Line::from(vec![Span::raw("new".to_string())])];
        slot.running = true;
    }
    let rows = live_tool_rows(&tools, Duration::ZERO, 80);
    assert_eq!(row_text(&rows[0]), "Running new");
    assert_eq!(row_text(&rows[1]), "bee");
}

#[test]
fn running_label_does_not_orphan_a_trailing_status() {
    for command in ["sleep 5", "echo café 日本語", &"x".repeat(100)] {
        let tool = bash_live(command, true);
        // The replacement verb needs four more cells than "Ran", not a tail row.
        let width = tool.header[0].width() + 4;
        let rows = live_tool_rows(&[tool], Duration::ZERO, 80);
        let wrapped = crate::composer::transcript::wrap_styled_line(rows[0].clone(), width);
        assert_eq!(wrapped.len(), 1, "{wrapped:?}");
        for width in [12, 40, 80] {
            let wrapped = crate::composer::transcript::wrap_styled_line(rows[0].clone(), width);
            assert!(
                wrapped
                    .iter()
                    .all(|row| !row_text(row).contains("running…"))
            );
        }
    }
}

#[test]
fn other_tools_keep_their_identity_and_empty_headers_are_safe() {
    let tool = LiveTool {
        id: "plugin".into(),
        name: "custom".into(),
        header: crate::tool_fmt::format_tool_call_lines(
            "custom",
            &serde_json::json!({}),
            None,
            None,
        ),
        running: true,
        tail: LiveToolTail::default(),
    };
    let rows = live_tool_rows(&[tool], Duration::ZERO, 80);
    assert!(row_text(&rows[0]).starts_with("⬡ Running custom"));
    assert_eq!(
        row_text(&live_tool_rows(&[live("empty", "", true)], Duration::ZERO, 80)[0]),
        "Running "
    );
}

#[test]
fn template_second_line_stays_plain_under_a_running_headline() {
    // A template preview's second line is its own row under the headline.
    // The running shimmer marks the headline (the first row) only.
    let tool = LiveTool {
        id: "rlm".into(),
        name: "rlm_spawn".into(),
        header: vec![
            Line::from(vec![
                Span::raw("\u{2b22} ".to_string()),
                Span::raw("Started agent".to_string()),
                Span::raw(" e2e-theme".to_string()),
            ]),
            Line::from(vec![Span::raw("  verify the theme fixes".to_string())]),
        ],
        running: true,
        tail: LiveToolTail::default(),
    };
    let rows = live_tool_rows(&[tool], Duration::ZERO, 80);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        row_text(&rows[0]).starts_with("\u{2b21} Running "),
        "{}",
        row_text(&rows[0])
    );
    assert_eq!(row_text(&rows[1]), "  verify the theme fixes");
}

#[test]
fn multi_line_card_draws_both_header_rows_then_body() {
    let headers = vec![
        Line::from(vec![Span::raw(
            "\u{2b22} Started agent e2e-theme".to_string(),
        )]),
        Line::from(vec![Span::raw("  verify the theme fixes".to_string())]),
    ];
    let body = vec![Line::from(vec![Span::raw("ok".to_string())])];
    let rows = crate::composer::transcript::format_tool_box_lines_multi(&headers, &body, 80);
    let texts: Vec<String> = rows.iter().map(row_text).collect();
    let head = texts
        .iter()
        .position(|t| t.contains("Started agent"))
        .expect("headline row");
    assert!(
        texts
            .get(head + 1)
            .is_some_and(|t| t.contains("verify the theme fixes")),
        "second header row must sit under the headline: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("ok")),
        "body missing: {texts:?}"
    );
}

/// Emission order across one step: assistant text streamed between
/// `ToolCallStart` and `ToolResult` is still in the reveal buffer / unfrozen
/// markdown tail when the result card commits. `insert_tool_box_lines` must
/// drain it first so `transcript` (live rows) and `history_entries` (reflow
/// replay) both keep text-before-cards, and stay ordered when a later
/// step's card arrives.
#[test]
fn tool_card_commit_keeps_emission_order_with_stream_text() {
    let mut tui = Tui::test_tui(80, 30);
    tui.is_task_running = true;
    tui.enable_reveal_pacing();

    let card = |tag: &str| vec![Line::from(vec![Span::raw(format!("bash {tag} card"))])];
    let row_idx = |tui: &Tui, needle: &str| {
        tui.transcript.iter().position(|l| row_text(l).contains(needle))
    };
    let entry_idx = |tui: &Tui, needle: &str| {
        tui.history_entries.iter().position(|e| match e {
            TranscriptEntry::StyledLines { lines, .. } => {
                lines.iter().any(|l| row_text(l).contains(needle))
            }
            TranscriptEntry::ToolBoxLines { header, .. } => {
                header.iter().any(|l| row_text(l).contains(needle))
            }
            _ => false,
        })
    };

    // Prose, then the ToolCallStart arm (flush + boundary + live card).
    tui.stream_text("alpha paragraph\n\n");
    tui.flush_markdown();
    tui.end_thinking();
    tui.mark_stream_round_boundary();
    tui.upsert_live_tool("t1", "bash", card("one"), false);
    // The same message keeps emitting text while the tool runs; it is still
    // uncommitted when ToolResult pushes the scrollback card.
    tui.stream_text("beta paragraph");
    tui.remove_live_tool("t1");
    tui.push_tool_box_lines(card("one"), vec![Line::from("done")]);

    // A later step repeats the shape; ordering must hold across both.
    tui.stream_text("gamma paragraph");
    tui.remove_live_tool("t2");
    tui.push_tool_box_lines(card("two"), vec![Line::from("done")]);

    let needles = [
        "alpha paragraph",
        "beta paragraph",
        "one card",
        "gamma paragraph",
        "two card",
    ];
    let rows: Vec<usize> = needles
        .iter()
        .map(|n| row_idx(&tui, n).unwrap_or_else(|| panic!("transcript row missing: {n}")))
        .collect();
    let entries: Vec<usize> = needles
        .iter()
        .map(|n| {
            entry_idx(&tui, n).unwrap_or_else(|| panic!("history entry missing: {n}"))
        })
        .collect();
    assert!(
        rows.windows(2).all(|w| w[0] < w[1]),
        "transcript rows out of emission order: {rows:?}"
    );
    assert!(
        entries.windows(2).all(|w| w[0] < w[1]),
        "history entries out of emission order: {entries:?}"
    );
}
