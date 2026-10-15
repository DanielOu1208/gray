use super::*;

fn row_texts(ibox: &InputBox) -> Vec<String> {
    ibox.lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn input_box_wraps_at_word_boundaries() {
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

    assert_eq!(ibox.cur_row, rows.len() - 2, "rows: {rows:?}");
}

#[test]
fn input_box_ghost_hint_shows_only_when_empty() {
    let ibox = build_input_box("", 0, 80, Some("Please continue…"), None, &[]);
    let rows = row_texts(&ibox);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[1].contains("Please continue"), "{rows:?}");

    let ibox = build_input_box("", 0, 80, None, None, &[]);
    assert_eq!(row_texts(&ibox)[1].trim(), "❯");

    let ibox = build_input_box("hi", 2, 80, Some("Please continue…"), None, &[]);
    assert!(
        !row_texts(&ibox).iter().any(|r| r.contains("Please")),
        "{:?}",
        row_texts(&ibox)
    );
}

#[test]
fn home_box_is_the_flat_band_with_hints_stats_and_notice_under_it() {
    let home = HomeRows {
        notice: Some(
            "⚠ 3 interrupted sessions in ~/gray · /sessions to pick up · /sessions dismiss to clear",
        ),
        stats: Some("65/65 plugins loaded"),
        model: Some("SWE 2"),
        model_tail: Some(" max"),
        above: None,
        session: None,
        qol: None,
    };
    let w = 100;
    let ibox = build_input_box("hi", 2, w, None, Some(home), &[]);
    let rows = row_texts(&ibox);

    assert_eq!(ibox.band_h, 3, "{rows:?}");
    assert_eq!((ibox.x, ibox.width), (11, 78));
    assert_eq!(rows[1], " ❯ hi");
    assert_eq!((ibox.cur_row, ibox.cur_col), (1, 2));

    assert_eq!(rows.len(), 9, "{rows:?}");
    assert_eq!(rows[3], "");
    assert!(rows[4].starts_with("SWE 2 max"), "{rows:?}");
    assert!(rows[4].ends_with("65/65 plugins loaded"), "{rows:?}");
    assert_eq!(
        display_width(&rows[4]),
        78,
        "model + stats span the box's width"
    );
    assert_eq!(rows[5], "");
    assert_eq!(rows[6], "⚠ 3 interrupted sessions in ~/gray");
    assert_eq!(rows[7], "  /sessions           pick up");
    assert_eq!(rows[8], "  /sessions dismiss   clear");

    let quiet = build_input_box(
        "",
        0,
        w,
        None,
        Some(HomeRows {
            stats: Some("65/65 plugins loaded"),
            model: Some("SWE 2 max"),
            ..HomeRows::default()
        }),
        &[],
    );
    let quiet_rows = row_texts(&quiet);
    assert_eq!(quiet_rows.len(), 5, "{quiet_rows:?}");
    assert!(quiet_rows[4].starts_with("SWE 2 max"), "{quiet_rows:?}");
    assert!(
        quiet_rows[4].ends_with("65/65 plugins loaded"),
        "{quiet_rows:?}"
    );

    let chat = build_input_box("hi", 2, w, None, None, &[]);
    assert_eq!((chat.lines.len(), chat.x, chat.width), (3, 0, w));

    assert_eq!(home_band_h(w), 3 + 2 + 1);
}

#[test]
fn home_box_wraps_at_the_column_width() {
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
fn an_inline_attachment_marker_renders_as_a_chip_not_literal_text() {
    let text = "see [image: pic.png] here";
    let ibox = build_input_box(text, text.len(), 100, None, None, &[]);
    assert_eq!(row_texts(&ibox)[1], " ❯ see  IMAGE  pic.png  here");

    let text = "watch [video: clip.mp4] now";
    let ibox = build_input_box(text, text.len(), 100, None, None, &[]);
    assert_eq!(row_texts(&ibox)[1], " ❯ watch  VIDEO  clip.mp4  now");
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
