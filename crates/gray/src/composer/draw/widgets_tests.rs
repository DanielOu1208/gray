use super::*;

fn row_texts(ibox: &InputBox) -> Vec<String> {
    ibox.lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn input_box_wraps_at_word_boundaries() {
    // Narrow box forces a wrap inside "...with colors." — the word must
    // move whole to the next row, never split as "c" / "olors.".
    let text = "aa bb cc dd ee ff with colors.";
    let ibox = build_input_box(text, text.len(), 20, None, None);
    let rows = row_texts(&ibox);
    let joined = rows.join("\n");
    assert!(
        joined.contains("colors."),
        "word must survive whole: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.ends_with('c') && r.contains("with ")),
        "must not split mid-word: {rows:?}"
    );
    // Cursor at end must land on the last content row (both margin rows
    // excluded from cur_row's target, the top pad's offset included).
    assert_eq!(ibox.cur_row, rows.len() - 2, "rows: {rows:?}");
}

#[test]
fn input_box_ghost_hint_shows_only_when_empty() {
    // Resume pending: empty box paints the dim ghost hint, same 3 rows.
    let ibox = build_input_box("", 0, 80, Some("Please continue…"), None);
    let rows = row_texts(&ibox);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[1].contains("Please continue"), "{rows:?}");
    // No resume: unchanged bare prompt.
    let ibox = build_input_box("", 0, 80, None, None);
    assert_eq!(row_texts(&ibox)[1].trim(), "❯");
    // Typed text never shows the ghost.
    let ibox = build_input_box("hi", 2, 80, Some("Please continue…"), None);
    assert!(
        !row_texts(&ibox).iter().any(|r| r.contains("Please")),
        "{:?}",
        row_texts(&ibox)
    );
}

#[test]
fn home_box_is_the_flat_band_with_hints_stats_tip_and_notice_under_it() {
    let home = HomeRows {
        notice: Some(
            "⚠ 3 interrupted sessions in ~/gray · /sessions to pick up · /sessions dismiss to clear",
        ),
        stats: Some("65 plugins · 65 loaded"),
        above: None,
    };
    let w = 100;
    let ibox = build_input_box("hi", 2, w, None, Some(home));
    let rows = row_texts(&ibox);
    // The box itself is the old 3-row band (pad, `❯`, pad), no frame, on
    // the welcome's centered `min(78, w-8)` column.
    assert_eq!(ibox.band_h, 3, "{rows:?}");
    assert_eq!((ibox.x, ibox.width), (11, 78));
    assert_eq!(rows[1], " ❯ hi");
    assert_eq!((ibox.cur_row, ibox.cur_col), (1, 2));
    // Row under the box: tip left, hints flush right on the same row.
    // Then the notice's headline with the stats at the right edge, then
    // one row per command.
    assert_eq!(rows.len(), 7, "{rows:?}");
    assert!(rows[3].starts_with("● Tip"), "{rows:?}");
    assert!(rows[3].ends_with("/ commands"), "{rows:?}");
    assert_eq!(
        display_width(&rows[3]),
        78,
        "tip + hints span the box's width"
    );
    assert!(
        rows[4].starts_with("⚠ 3 interrupted sessions in ~/gray"),
        "{rows:?}"
    );
    assert!(rows[4].ends_with("65 plugins · 65 loaded"), "{rows:?}");
    assert_eq!(
        display_width(&rows[4]),
        78,
        "headline + stats span the box's width"
    );
    assert_eq!(rows[5], "  /sessions           pick up");
    assert_eq!(rows[6], "  /sessions dismiss   clear");
    // Without the notice the stats take the left of row 2.
    let quiet = build_input_box(
        "",
        0,
        w,
        None,
        Some(HomeRows {
            stats: Some("65 plugins · 65 loaded"),
            ..HomeRows::default()
        }),
    );
    let quiet_rows = row_texts(&quiet);
    assert_eq!(quiet_rows.len(), 5, "{quiet_rows:?}");
    assert_eq!(quiet_rows[4], "65 plugins · 65 loaded");
    // Off the home screen the box is the full-width band alone.
    let chat = build_input_box("hi", 2, w, None, None);
    assert_eq!((chat.lines.len(), chat.x, chat.width), (3, 0, w));
    // The band the welcome centers over: box, one home row, status bar.
    assert_eq!(home_band_h(w), 3 + 1 + 1);
}

#[test]
fn home_box_wraps_at_the_column_width() {
    // 89 chars fit one chat row at w=100 (96 content cols) but wrap in the
    // 78-col home box (74 content cols).
    let text = "word ".repeat(18);
    let text = text.trim_end();
    let home = build_input_box(text, text.len(), 100, None, Some(HomeRows::default()));
    let chat = build_input_box(text, text.len(), 100, None, None);
    assert_eq!(home.band_h, 4, "{:?}", row_texts(&home));
    assert_eq!(chat.band_h, 3, "{:?}", row_texts(&chat));
    assert!(
        row_texts(&home)[..home.band_h]
            .iter()
            .all(|r| display_width(r) <= 78)
    );
    assert_eq!(home.cur_row, 2);
}
