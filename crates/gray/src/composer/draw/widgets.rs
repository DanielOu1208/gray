//! Viewport widgets: input box, shimmer, status dock (split from `draw`).

use super::*;

pub(crate) fn shimmer_spans(text: &str, elapsed: Duration) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let padding = 10usize;
    let period = chars.len() + padding * 2;
    let sweep_seconds = 2.0f32;
    let pos = ((elapsed.as_secs_f32() % sweep_seconds) / sweep_seconds * (period as f32)) as usize;
    let band_half_width = 5.0f32;
    // Theme-driven sweep: base role → bright role. Non-Rgb theme colors
    // (e.g. the `terminal` theme's Reset passthrough) fall back to the
    // generic blender, which holds the base color steady.
    let base = crate::theme::theme().shimmer_base;
    let hilite = crate::theme::theme().text_bright;
    chars
        .iter()
        .enumerate()
        .map(|(i, ch)| {
            let dist = ((i as isize + padding as isize) - pos as isize).abs() as f32;
            let t = if dist <= band_half_width {
                0.5 * (1.0 + (std::f32::consts::PI * (dist / band_half_width)).cos())
            } else {
                0.0
            };
            let k = t.clamp(0.0, 1.0) * 0.9;
            let style = Style::default()
                .fg(crate::tui::blend_color(base, hilite, k))
                .add_modifier(if t > 0.3 {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
            Span::styled(ch.to_string(), style)
        })
        .collect()
}

/// Input box render state: styled lines (own internal top/bottom margin
/// rows, `❯` prompt) plus the cursor position within them. `cur_row` is an
/// index into `lines`, so it carries the top pad's offset.
pub(crate) struct InputBox {
    pub(crate) lines: Vec<Line<'static>>,
    /// Leading rows that are the box itself (painted on `surface_bg`);
    /// any rows after them are the home rows under it.
    pub(crate) band_h: usize,
    /// Rows before the box rows that are home chrome on the screen bg
    /// (the update notice above the box), so `lines` keeps one vector.
    pub(crate) lead_h: usize,
    /// The box's column: `x` cells in from the left, `width` wide. Full
    /// width in chat; on the home screen the welcome's centered column.
    pub(crate) x: usize,
    pub(crate) width: usize,
    pub(crate) cur_row: usize,
    pub(crate) cur_col: usize,
}

/// One styled char of an input row: a chip's pill or label cell, or a draft
/// char. The layout runs on these, so chips wrap and take the cursor with the
/// draft text they sit in front of.
pub(crate) type InputCell = (char, Style);

/// Cells for the attachment chips, in order: a ` {kind} ` pill, a ` {name} `
/// label, and one gap cell between chips. Pure; the caller brings the styles.
pub(crate) fn chip_cells(
    chips: &[(&str, &str)],
    pill: Style,
    label: Style,
    gap: Style,
) -> Vec<InputCell> {
    let mut cells: Vec<InputCell> = Vec::new();
    for (i, (kind, name)) in chips.iter().enumerate() {
        if i > 0 {
            cells.push((' ', gap));
        }
        cells.extend(format!(" {kind} ").chars().map(|c| (c, pill)));
        cells.extend(format!(" {name} ").chars().map(|c| (c, label)));
    }
    cells
}

/// `wrap_line`'s return: each visual row's `start..end` char range plus
/// the cursor's `(row, col)` when a cursor position was given.
type WrapResult = (Vec<(usize, usize)>, Option<(usize, usize)>);

/// Splits one logical input line into visual rows no wider than `content_w`,
/// word-aware (break after the last space in the window, hard-cut only a word
/// too long for a row). Returns each row as a `start..end` char range, and the
/// `(row, col)` of `cursor` (a char index) when one is given. A cursor on a
/// wrap point stays at the end of the row before it. An empty line is one
/// empty row, so there is always a row to draw. Pure.
pub(crate) fn wrap_line(
    chars: &[char],
    content_w: usize,
    cursor: Option<usize>,
) -> WrapResult {
    let mut rows: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let end = crate::text_width::word_window_end(chars, start, content_w);
        rows.push((start, end));
        start = end;
    }
    if rows.is_empty() {
        rows.push((0, 0));
    }
    let at = cursor.map(|c| {
        let row = rows
            .iter()
            .position(|&(_, end)| c <= end)
            .unwrap_or(rows.len() - 1);
        let (s, e) = rows[row];
        (row, c.clamp(s, e) - s)
    });
    (rows, at)
}

/// The draft's logical line and char index under `cursor` (a byte offset): the
/// first line whose end reaches it, else the last.
fn draft_cursor(text: &str, cursor: usize) -> (usize, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut line_start = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let line_end = line_start + line.len();
        if cursor <= line_end || i + 1 == lines.len() {
            let within = cursor.saturating_sub(line_start).min(line.len());
            let chars = line.char_indices().take_while(|(b, _)| *b < within).count();
            return (i, chars);
        }
        line_start = line_end + 1;
    }
    (0, 0)
}

/// Groups cells into spans, one per run of equal style.
fn styled_runs(cells: &[InputCell]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut style: Option<Style> = None;
    for &(ch, st) in cells {
        if style != Some(st) {
            if let Some(prev) = style {
                spans.push(Span::styled(std::mem::take(&mut run), prev));
            }
            style = Some(st);
        }
        run.push(ch);
    }
    if let Some(prev) = style {
        spans.push(Span::styled(run, prev));
    }
    spans
}

/// Builds the prompt box lines from textarea state. Hoisted out of the draw
/// closure so the dock height (and thus the viewport) can be sized before
/// `terminal.draw` runs. `chips` open the first logical line, right after the
/// `❯`, and wrap with the draft; an empty draft shows the ghost hint after them.
pub(crate) fn build_input_box(
    text: &str,
    cursor: usize,
    w: usize,
    ghost: Option<&str>,
    home: Option<HomeRows>,
    chips: &[InputCell],
) -> InputBox {
    let (x, w) = home.map_or((0, w), |_| crate::composer::home_column(w));
    let content_w = w.saturating_sub(4).max(1);

    // Neutral Gray palette (no blue, transparent bg — text only)
    let prompt_color = crate::theme::theme().text_soft;
    let text_primary = crate::theme::theme().text_body;
    let text_dim = crate::theme::theme().text_dim;

    let mut box_lines: Vec<Line<'static>> = Vec::new();

    // Top padding inside the box: the blank row the box owns above its
    // `❯` row, so the prompt never sits flush against the row above it.
    box_lines.push(Line::from(""));

    // Prompt input rows. Continuation rows indent under the glyph.
    let arrow_span = Span::styled(
        " ❯ ",
        Style::default()
            .fg(prompt_color)
            .add_modifier(Modifier::BOLD),
    );
    let draft_style = Style::default().fg(text_primary);
    let ghost_style = Style::default().fg(text_dim);

    let (cur_line, cur_char) = draft_cursor(text, cursor);
    let mut cur_row = 0usize;
    let mut cur_col = 0usize;

    for (i, raw_line) in text.split('\n').enumerate() {
        let mut cells: Vec<InputCell> = Vec::new();
        if i == 0 {
            cells.extend_from_slice(chips);
        }
        let lead = cells.len();
        if i == 0 && text.is_empty() {
            // Ghost hint (display-only, never submitted): e.g. "Please continue…"
            // while a resume is pending.
            if let Some(hint) = ghost {
                cells.extend(hint.chars().map(|c| (c, ghost_style)));
            }
        } else {
            cells.extend(raw_line.chars().map(|c| (c, draft_style)));
        }

        // Word-aware wrap (question-panel `wrap_plain` parity): words never
        // split across rows. The chips count against the first row's room.
        let chars: Vec<char> = cells.iter().map(|c| c.0).collect();
        let at_here = (i == cur_line).then_some(lead + cur_char);
        let (rows, at) = wrap_line(&chars, content_w, at_here);
        let first_row = box_lines.len();
        for (r, &(s, e)) in rows.iter().enumerate() {
            let prefix = if i == 0 && r == 0 {
                arrow_span.clone()
            } else {
                Span::raw("   ")
            };
            let mut spans = vec![prefix];
            spans.extend(styled_runs(&cells[s..e]));
            box_lines.push(Line::from(spans));
        }
        if let Some((r, col)) = at {
            cur_row = first_row + r;
            cur_col = col;
        }
    }

    // Bottom padding inside the box
    box_lines.push(Line::from(""));

    let mut lead_h = 0usize;
    if let Some(above) = home.and_then(|h| h.above) {
        box_lines.insert(
            0,
            Line::from(Span::styled(
                above.to_string(),
                Style::default().fg(text_dim),
            )),
        );
        lead_h = 1;
    }
    let band_h = box_lines.len() - lead_h;
    if let Some(home) = home {
        box_lines.extend(home_rows(home, w));
    }
    InputBox {
        lines: box_lines,
        band_h,
        lead_h,
        x,
        width: w,
        // Rows were counted from the top of the box; the lead shifts them down.
        cur_row: cur_row + lead_h,
        cur_col,
    }
}

/// What devin's home screen shows under the box.
#[derive(Default, Clone, Copy)]
pub(crate) struct HomeRows<'a> {
    pub(crate) notice: Option<&'a str>,
    /// The plugin counts line from the startup probe (`N plugins · M
    /// loaded · …`); its slot stays empty until it lands.
    pub(crate) stats: Option<&'a str>,
    pub(crate) qol: Option<&'a str>,
    /// A dim row directly above the box (the update notice), flush with
    /// the box's left edge.
    pub(crate) above: Option<&'a str>,
}

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|s| display_width(s.content.as_ref()))
        .sum()
}

fn right_aligned_line(w: usize, spans: Vec<Span<'static>>) -> Line<'static> {
    let pad = w.saturating_sub(spans_width(&spans));
    let mut out = Vec::with_capacity(spans.len() + 1);
    if pad > 0 {
        out.push(Span::raw(" ".repeat(pad)));
    }
    out.extend(spans);
    Line::from(out)
}

/// One row with `left` at the box edge and `right` flush to `w`, or —
/// when they can't share it with two cells of gap — the left row, then
/// the right item right-aligned on its own row under it.
fn left_right_rows(
    w: usize,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) -> Vec<Line<'static>> {
    if right.is_empty() {
        return vec![Line::from(left)];
    }
    let lw = spans_width(&left);
    let rw = spans_width(&right);
    if lw + 2 + rw <= w {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(w - lw - rw)));
        spans.extend(right);
        vec![Line::from(spans)]
    } else {
        vec![Line::from(left), right_aligned_line(w, right)]
    }
}

/// devin's home rows under the box, on the screen bg and inside the
/// box's column: row 1 pairs the tip (left edge) with the hints (right
/// edge); row 2 pairs the startup notice's headline with the plugin
/// stats, the notice's command rows continuing under it — or the stats
/// alone on the left when there is no notice. Pairs that can't share a
/// row stack, the right item on its own row under.
fn home_rows(home: HomeRows, w: usize) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    let tip = crate::composer::tip_spans();
    let hints = crate::composer::hint_spans(w.saturating_sub(spans_width(&tip) + 2).max(1));
    rows.extend(left_right_rows(w, tip, hints));
    if home.notice.is_some() || home.stats.is_some() {
        rows.push(Line::default());
    }
    match (home.notice, home.stats) {
        (Some(notice), stats) => {
            let t = crate::theme::theme();
            let mut parts = notice.split(" · ");
            let headline = vec![Span::styled(
                parts.next().unwrap_or_default().to_string(),
                Style::default().fg(t.text_bright),
            )];
            let right = stats.map(crate::composer::stats_spans).unwrap_or_default();
            rows.extend(left_right_rows(w, headline, right));
            let cmds: Vec<(&str, &str)> = parts
                .map(|p| p.split_once(" to ").unwrap_or((p, "")))
                .collect();
            let key_w = cmds
                .iter()
                .map(|(k, _)| display_width(k))
                .max()
                .unwrap_or(0);
            for (key, action) in cmds {
                rows.push(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(
                        format!("{key:<key_w$}   "),
                        Style::default().fg(t.text_body),
                    ),
                    Span::styled(action.to_string(), Style::default().fg(t.text_muted)),
                ]));
            }
        }
        (None, Some(stats)) => rows.push(Line::from(crate::composer::stats_spans(stats))),
        (None, None) => {}
    }
    if let Some(qol) = home.qol {
        let t = crate::theme::theme();
        if home.notice.is_none() && home.stats.is_none() {
            rows.push(Line::default());
        }
        rows.push(Line::from(Span::styled(
            qol.to_string(),
            Style::default().fg(t.text_muted),
        )));
    }
    rows
}

/// The home band's height before its spacer: the empty box, the home
/// rows and the status bar.
pub(crate) fn home_band_h(w: usize) -> usize {
    build_input_box("", 0, w, None, Some(HomeRows::default()), &[])
        .lines
        .len()
        + 1
}

/// True when the last scrollback row is a bare blank (no bg, no glyphs):
/// the transcript already left breathing room above the viewport (a
/// paragraph separator, `ensure_gap`, …). Same predicate `ensure_gap` uses.
pub(crate) fn transcript_ends_blank(transcript: &[Line<'static>]) -> bool {
    transcript.last().is_some_and(|l| {
        l.style.bg.is_none()
            && l.spans
                .iter()
                .all(|s| s.style.bg.is_none() && s.content.trim().is_empty())
    })
}

/// Height the band reserves above its queued, live and input rows:
///   seam    1 row, when the transcript ends in content (see [`needs_seam`])
///   status  1 row, the shimmer text, while a turn runs
///   breath  1 row below the status, so it never melts into the input box
pub(crate) fn status_dock_h(has_status: bool, needs_seam: bool) -> u16 {
    u16::from(needs_seam) + if has_status { 2 } else { 0 }
}

/// Whether the band opens with the seam row: the one gap between the
/// transcript and whatever the band shows first, the status mid-turn or the
/// input box when idle. Gaps are paid as the prefix of the next block, never
/// left as a trailing blank (`transcript::margins`), so the transcript ends
/// in content and the seam is that gap; the only blank tail is the welcome
/// banner's own trailing row. Pure for testability (`Tui::new` needs a TTY).
pub(crate) fn needs_seam(transcript: &[Line<'static>]) -> bool {
    !transcript.is_empty() && !transcript_ends_blank(transcript)
}

/// Rows of margin the uncommitted stream tail owes the status line: one
/// blank while a status runs under a non-empty, non-blank-ended tail — the
/// tail-side analogue of [`needs_seam`], which only sees the committed
/// transcript and so places the one gap *above* the tail. Idle frames owe
/// no row (the input box's own top pad is the margin then), and a tail
/// that already ends blank carries its own. Pure for testability
/// (`Tui::new` needs a TTY).
pub(crate) fn tail_gap_h(has_status: bool, tail: &[Line<'static>]) -> u16 {
    u16::from(has_status && !tail.is_empty() && !transcript_ends_blank(tail))
}

/// The painted height of a tail block (text rows + its [`tail_gap_h`]
/// margin) once the band allowance has kept `kept` rows for it:
/// all-or-nothing — an allowance covering only the margin would paint a
/// lone blank row under the seam, so the tail yields entirely instead.
/// Pure for testability.
pub(crate) fn tail_block_h(kept: u16, gap: u16) -> u16 {
    if kept > gap { kept } else { 0 }
}

/// Queued follow-up inputs held while a turn is in flight (codex
/// `PendingInputPreview` parity, minimal): header + `↳` dim-italic rows,
/// then one blank row so the queue never glues onto the live tool card.
/// One row per queued message, first line only, truncated to `w`.
pub(crate) fn queued_preview_lines(
    queued: &std::collections::VecDeque<(String, Vec<std::path::PathBuf>)>,
    w: usize,
) -> Vec<Line<'static>> {
    if queued.is_empty() {
        return Vec::new();
    }
    let dim = Style::default().fg(crate::theme::theme().text_muted);
    let dim_italic = Style::default()
        .fg(crate::theme::theme().text_muted)
        .add_modifier(Modifier::DIM)
        .add_modifier(Modifier::ITALIC);
    let mut lines = vec![Line::from(vec![
        Span::styled("• ", dim),
        Span::styled(format!("Queued follow-up inputs ({})", queued.len()), dim),
    ])];
    // Viewport is only 10 rows — cap preview so input + footer stay visible.
    let max_show = 3usize;
    for (text, attached) in queued.iter().take(max_show) {
        let first = text.lines().next().unwrap_or("").trim();
        let mut preview: String = first.chars().take(w.saturating_sub(6).max(8)).collect();
        if first.chars().count() > preview.chars().count() {
            preview.push('…');
        }
        if !attached.is_empty() {
            let n = attached.len();
            preview.push_str(&format!(" +{n} image{}", if n == 1 { "" } else { "s" }));
        }
        lines.push(Line::from(vec![
            Span::styled("  ↳ ", dim),
            Span::styled(preview, dim_italic),
        ]));
    }
    if queued.len() > max_show {
        lines.push(Line::from(vec![Span::styled(
            format!("    … +{} more", queued.len() - max_show),
            dim_italic,
        )]));
    }
    // The separator belongs to the queued block's measured height: a command
    // preview stays visually distinct from the running tool below it.
    lines.push(Line::default());
    lines
}

#[path = "widgets_tests.rs"]
#[cfg(test)]
mod tests;
