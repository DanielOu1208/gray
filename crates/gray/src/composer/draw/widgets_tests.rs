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
    let ibox = build_input_box(text, text.len(), 20, None, None, &[]);
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
    let ibox = build_input_box("", 0, 80, Some("Please continue…"), None, &[]);
    let rows = row_texts(&ibox);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[1].contains("Please continue"), "{rows:?}");
    // No resume: unchanged bare prompt.
    let ibox = build_input_box("", 0, 80, None, None, &[]);
    assert_eq!(row_texts(&ibox)[1].trim(), "❯");
    // Typed text never shows the ghost.
    let ibox = build_input_box("hi", 2, 80, Some("Please continue…"), None, &[]);
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
        qol: None,
    };
    let w = 100;
    let ibox = build_input_box("hi", 2, w, None, Some(home), &[]);
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
    assert_eq!(rows.len(), 8, "{rows:?}");
    assert!(rows[3].starts_with("● Tip"), "{rows:?}");
    assert!(rows[3].ends_with("/ commands"), "{rows:?}");
    assert_eq!(
        display_width(&rows[3]),
        78,
        "tip + hints span the box's width"
    );
    assert!(
        rows[5].starts_with("⚠ 3 interrupted sessions in ~/gray"),
        "{rows:?}"
    );
    assert!(rows[5].ends_with("65 plugins · 65 loaded"), "{rows:?}");
    assert_eq!(
        display_width(&rows[5]),
        78,
        "headline + stats span the box's width"
    );
    assert_eq!(rows[4], "");
    assert_eq!(rows[6], "  /sessions           pick up");
    assert_eq!(rows[7], "  /sessions dismiss   clear");
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
        &[],
    );
    let quiet_rows = row_texts(&quiet);
    assert_eq!(quiet_rows.len(), 6, "{quiet_rows:?}");
    assert_eq!(quiet_rows[5], "65 plugins · 65 loaded");
    // Off the home screen the box is the full-width band alone.
    let chat = build_input_box("hi", 2, w, None, None, &[]);
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
    let home = build_input_box(text, text.len(), 100, None, Some(HomeRows::default()), &[]);
    let chat = build_input_box(text, text.len(), 100, None, None, &[]);
    assert_eq!(home.band_h, 4, "{:?}", row_texts(&home));
    assert_eq!(chat.band_h, 3, "{:?}", row_texts(&chat));
    assert!(
        row_texts(&home)[..home.band_h]
            .iter()
            .all(|r| display_width(r) <= 78)
    );
    assert_eq!(home.cur_row, 2);
}

fn plain() -> Style {
    Style::default()
}

fn one_chip() -> Vec<InputCell> {
    chip_cells(
        &[("Image", "clipboard-ziacTH.png")],
        plain(),
        plain(),
        plain(),
    )
}

fn two_chips() -> Vec<InputCell> {
    chip_cells(
        &[("Image", "clipboard-ziacTH.png"), ("File", "notes.txt")],
        plain(),
        plain(),
        plain(),
    )
}

fn cells_text(cells: &[InputCell]) -> String {
    cells.iter().map(|c| c.0).collect()
}

#[test]
fn chip_cells_are_pill_then_label_with_a_gap_between_chips() {
    let cells = two_chips();
    assert_eq!(
        cells_text(&cells),
        " Image  clipboard-ziacTH.png   File  notes.txt "
    );
    assert_eq!(cells.len(), 7 + 22 + 1 + 6 + 11);
}

#[test]
fn chips_open_the_prompt_row_and_the_draft_follows_them() {
    let chips = one_chip();
    let ibox = build_input_box("wtf is this", 11, 100, None, None, &chips);
    let rows = row_texts(&ibox);
    // Top pad, one prompt row, bottom pad: the chips add no row of their own.
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1], format!(" ❯ {}wtf is this", cells_text(&chips)));
    assert_eq!((ibox.cur_row, ibox.cur_col), (1, chips.len() + 11));
}

#[test]
fn the_cursor_at_start_middle_and_end_of_the_draft_sits_after_the_chips() {
    let chips = one_chip();
    for (cursor, col) in [(0, 0), (5, 5), (11, 11)] {
        let ibox = build_input_box("hello world", cursor, 100, None, None, &chips);
        assert_eq!(
            (ibox.cur_row, ibox.cur_col),
            (1, chips.len() + col),
            "cursor {cursor}"
        );
    }
}

#[test]
fn an_empty_draft_puts_the_cursor_right_after_two_chips_and_before_the_ghost() {
    let chips = two_chips();
    let ibox = build_input_box("", 0, 100, Some("Please continue…"), None, &chips);
    assert_eq!(
        row_texts(&ibox)[1],
        format!(" ❯ {}Please continue…", cells_text(&chips))
    );
    assert_eq!((ibox.cur_row, ibox.cur_col), (1, chips.len()));
}

#[test]
fn the_cursor_on_a_later_line_ignores_the_chips() {
    let chips = one_chip();
    let ibox = build_input_box("a\nbc", 4, 100, None, None, &chips);
    let rows = row_texts(&ibox);
    assert_eq!(rows[1], format!(" ❯ {}a", cells_text(&chips)));
    assert_eq!(rows[2], "   bc");
    assert_eq!((ibox.cur_row, ibox.cur_col), (2, 2));
}

#[test]
fn the_draft_wraps_after_the_chips_on_the_first_row_and_indents_below() {
    let chips = one_chip();
    let text = "wtf is this and then some more";
    // content width 36: the first row has 36 - 29 = 7 cells left for the draft.
    let ibox = build_input_box(text, text.len(), 40, None, None, &chips);
    let rows = row_texts(&ibox);
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[1], format!(" ❯ {}wtf is ", cells_text(&chips)));
    assert_eq!(rows[2], "   this and then some more");
    let tail = "this and then some more".chars().count();
    assert_eq!((ibox.cur_row, ibox.cur_col), (2, tail));
}

#[test]
fn chips_wider_than_the_line_wrap_onto_their_own_rows_without_losing_a_cell() {
    let chips = two_chips();
    for w in [5usize, 8, 12, 20, 60] {
        let ibox = build_input_box("", 0, w, None, None, &chips);
        let rows = row_texts(&ibox);
        let body_rows = &rows[1..rows.len() - 1];
        let body: String = body_rows
            .iter()
            .map(|r| r.chars().skip(3).collect::<String>())
            .collect();
        assert_eq!(body, cells_text(&chips), "w {w}");
        assert!(body_rows.iter().all(|r| r.chars().count() <= w), "w {w}");
        assert!(body_rows[1..].iter().all(|r| r.starts_with("   ")), "w {w}");
        let (cur_row, cur_col) = (ibox.cur_row, ibox.cur_col);
        assert_eq!(cur_row, rows.len() - 2, "w {w}");
        assert_eq!(cur_col, rows[cur_row].chars().count() - 3, "w {w}");
    }
}
