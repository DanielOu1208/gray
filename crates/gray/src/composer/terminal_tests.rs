use std::cell::RefCell;
use std::rc::Rc;

use ratatui::backend::CrosstermBackend;
use ratatui::style::Style;

use super::*;

#[derive(Clone, Default)]
struct Tape(Rc<RefCell<Vec<u8>>>);

impl Write for Tape {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Tape {
    fn len(&self) -> usize {
        self.0.borrow().len()
    }

    fn since(&self, mark: usize) -> String {
        String::from_utf8_lossy(&self.0.borrow()[mark..]).into_owned()
    }
}

struct Screen {
    w: usize,
    h: usize,

    rows: Vec<Vec<char>>,
    scrollback: Vec<String>,
    x: usize,
    y: usize,
    top: usize,
    bottom: usize,
}

impl Screen {
    fn replay(tape: &Tape, w: u16, h: u16) -> Self {
        let (w, h) = (usize::from(w), usize::from(h));
        let mut s = Screen {
            w,
            h,
            rows: vec![vec![' '; w]; h],
            scrollback: Vec::new(),
            x: 0,
            y: 0,
            top: 0,
            bottom: h - 1,
        };
        let bytes = tape.0.borrow();
        let text = String::from_utf8_lossy(&bytes);
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\x1b' => match chars.next() {
                    Some('[') => {
                        let mut params = String::new();
                        let mut fin = ' ';
                        for c in chars.by_ref() {
                            if ('\x40'..='\x7e').contains(&c) {
                                fin = c;
                                break;
                            }
                            params.push(c);
                        }
                        s.csi(&params, fin);
                    }

                    Some(']') => {
                        while let Some(c) = chars.next() {
                            if c == '\x07' {
                                break;
                            }
                            if c == '\x1b' {
                                chars.next();
                                break;
                            }
                        }
                    }
                    Some('M') => s.reverse_index(),
                    _ => {}
                },
                '\r' => s.x = 0,
                '\n' => s.line_feed(),
                c if c < ' ' => {}
                c => s.print(c),
            }
        }
        s
    }

    fn csi(&mut self, params: &str, fin: char) {
        if params.starts_with('?') {
            return;
        }
        let nums: Vec<usize> = params.split(';').map(|p| p.parse().unwrap_or(0)).collect();
        let raw = |i: usize| nums.get(i).copied().unwrap_or(0);
        let at_least_1 = |i: usize| raw(i).max(1);
        match fin {
            'H' | 'f' => {
                self.y = (at_least_1(0) - 1).min(self.h - 1);
                self.x = (at_least_1(1) - 1).min(self.w - 1);
            }
            'r' => {
                let (top, bottom) = if params.is_empty() {
                    (1, self.h)
                } else {
                    (at_least_1(0), nums.get(1).copied().unwrap_or(self.h))
                };

                if top < bottom && bottom <= self.h {
                    self.top = top - 1;
                    self.bottom = bottom - 1;
                }
                self.x = 0;
                self.y = 0;
            }
            'J' => match raw(0) {
                0 => {
                    let (x, y) = (self.x.min(self.w), self.y);
                    self.rows[y][x..].fill(' ');
                    for row in &mut self.rows[y + 1..] {
                        row.fill(' ');
                    }
                }
                2 => {
                    for row in &mut self.rows {
                        row.fill(' ');
                    }
                }
                3 => self.scrollback.clear(),
                _ => {}
            },
            'K' => {
                let x = self.x.min(self.w);
                self.rows[self.y][x..].fill(' ');
            }
            'G' => self.x = (at_least_1(0) - 1).min(self.w - 1),
            'S' => self.scroll_up(at_least_1(0)),
            'T' => self.scroll_down(at_least_1(0)),

            _ => {}
        }
    }

    fn line_feed(&mut self) {
        if self.y == self.bottom {
            self.scroll_up(1);
        } else if self.y + 1 < self.h {
            self.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.y == self.top {
            self.scroll_down(1);
        } else if self.y > 0 {
            self.y -= 1;
        }
    }

    fn scroll_up(&mut self, n: usize) {
        for _ in 0..n {
            let row = self.rows.remove(self.top);
            if self.top == 0 {
                self.scrollback.push(text(&row));
            }
            self.rows.insert(self.bottom, vec![' '; self.w]);
        }
    }

    fn scroll_down(&mut self, n: usize) {
        for _ in 0..n {
            self.rows.remove(self.bottom);
            self.rows.insert(self.top, vec![' '; self.w]);
        }
    }

    fn print(&mut self, c: char) {
        let width = crate::text_width::char_width(c);
        if width == 0 {
            return;
        }
        if self.x + width > self.w {
            self.x = 0;
            self.line_feed();
        }
        self.rows[self.y][self.x] = c;
        if width == 2 {
            self.rows[self.y][self.x + 1] = '\0';
        }
        self.x += width;
    }

    fn lines(&self) -> Vec<String> {
        self.rows.iter().map(|r| text(r)).collect()
    }

    fn transcript(&self, band_top: u16) -> Vec<String> {
        let mut all = self.scrollback.clone();
        all.extend(self.lines().into_iter().take(usize::from(band_top)));
        all
    }
}

fn text(row: &[char]) -> String {
    row.iter()
        .filter(|c| **c != '\0')
        .collect::<String>()
        .trim_end()
        .to_string()
}

type Term = CustomTerminal<CrosstermBackend<Tape>>;

const W: u16 = 20;

fn term(h: u16, y: u16, scrollback: Scrollback) -> (Term, Tape) {
    let tape = Tape::default();
    let term = CustomTerminal::at_row(
        CrosstermBackend::new(tape.clone()),
        Size::new(W, h),
        y,
        scrollback,
    );
    (term, tape)
}

fn insert(term: &mut Term, rows: &[&str]) {
    term.insert_before(rows.len() as u16, |buf| {
        for (i, row) in rows.iter().enumerate() {
            buf.set_string(0, i as u16, *row, Style::default());
        }
    })
    .unwrap();
}

fn band(term: &mut Term, height: u16) {
    let screen = term.last_known_screen_size;
    term.set_viewport_height(height, screen).unwrap();
    term.draw(|frame| {
        let area = frame.area();
        for i in 0..area.height {
            let label = if i + 2 == area.height {
                "❯".to_string()
            } else {
                format!("band{i}")
            };
            frame
                .buffer
                .set_string(area.x, area.y + i, label, Style::default());
        }
    })
    .unwrap();
}

fn band_rows(height: u16) -> Vec<String> {
    (0..height)
        .map(|i| {
            if i + 2 == height {
                "❯".to_string()
            } else {
                format!("band{i}")
            }
        })
        .collect()
}

fn rows(range: std::ops::Range<usize>) -> Vec<String> {
    range.map(|i| format!("r{i}")).collect()
}

#[test]
fn the_band_sits_under_a_short_transcript() {
    let (mut term, tape) = term(16, 0, Scrollback::Regions);
    insert(&mut term, &["banner", "", "/resume"]);
    band(&mut term, 4);

    let screen = Screen::replay(&tape, W, 16);
    let mut want = vec!["banner".to_string(), String::new(), "/resume".to_string()];
    want.extend(band_rows(4));
    assert_eq!(screen.lines()[..7], want[..]);
    assert!(screen.lines()[7..].iter().all(String::is_empty));
    assert_eq!(term.viewport_area, Rect::new(0, 3, W, 4));
}

#[test]
fn a_grounded_band_regrounds_when_it_shrinks() {
    let (mut term, tape) = term(10, 0, Scrollback::Regions);
    let history: Vec<String> = rows(0..20);
    insert(
        &mut term,
        &history.iter().map(String::as_str).collect::<Vec<_>>(),
    );
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));

    // A modal/popup grows the band, then closes — the band must re-pin
    // to the screen bottom instead of floating where the shrink left it.
    band(&mut term, 8);
    assert_eq!(term.viewport_area, Rect::new(0, 2, W, 8));
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));

    let lines = Screen::replay(&tape, W, 10).lines();
    assert_eq!(lines[6..10], band_rows(4)[..]);
}

#[test]
fn the_banner_lands_on_the_cursor_row_and_the_band_follows_it() {
    let (mut term, tape) = term(16, 5, Scrollback::Regions);
    insert(&mut term, &["logo", "version"]);
    band(&mut term, 4);
    let lines = Screen::replay(&tape, W, 16).lines();
    assert_eq!(lines[5], "logo");
    assert_eq!(lines[6], "version");
    assert_eq!(lines[7..11], band_rows(4)[..]);
}

#[test]
fn an_insert_shifts_the_band_down_without_repainting_it() {
    let (mut term, tape) = term(16, 0, Scrollback::Regions);
    insert(&mut term, &["banner"]);
    band(&mut term, 4);

    let mark = tape.len();
    insert(&mut term, &["x", "y"]);
    band(&mut term, 4);
    let written = tape.since(mark);
    assert!(
        !written.contains("band"),
        "the band was repainted: {written:?}"
    );
    assert!(
        !written.contains('❯'),
        "the prompt was repainted: {written:?}"
    );

    let lines = Screen::replay(&tape, W, 16).lines();
    assert_eq!(lines[..3], ["banner", "x", "y"]);
    assert_eq!(lines[3..7], band_rows(4)[..]);
}

#[test]
fn on_the_bottom_rows_scroll_into_history_in_order() {
    let (mut term, tape) = term(8, 0, Scrollback::Regions);
    band(&mut term, 3);
    for i in 0..12 {
        insert(&mut term, &[format!("r{i}").as_str()]);
        band(&mut term, 3);
    }
    let screen = Screen::replay(&tape, W, 8);
    assert_eq!(screen.transcript(5), rows(0..12));
    assert_eq!(screen.lines()[5..], band_rows(3)[..]);
    assert_eq!(term.viewport_area, Rect::new(0, 5, W, 3));
}

#[test]
fn a_batch_taller_than_the_screen_keeps_its_order() {
    for scrollback in [Scrollback::Regions, Scrollback::FullScreen] {
        let (mut term, tape) = term(8, 0, scrollback);
        band(&mut term, 3);
        let batch = rows(0..20);
        let refs: Vec<&str> = batch.iter().map(String::as_str).collect();
        insert(&mut term, &refs);
        band(&mut term, 3);

        let screen = Screen::replay(&tape, W, 8);
        assert_eq!(screen.transcript(5), batch, "{scrollback:?}");
        assert_eq!(screen.lines()[5..], band_rows(3)[..], "{scrollback:?}");
    }
}

#[test]
fn growth_and_shrink_keep_the_band_top() {
    let (mut term, tape) = term(16, 0, Scrollback::Regions);
    insert(&mut term, &["logo", "", "version"]);
    for _ in 0..3 {
        band(&mut term, 4);
        assert_eq!(term.viewport_area, Rect::new(0, 3, W, 4));
        band(&mut term, 9);
        assert_eq!(term.viewport_area, Rect::new(0, 3, W, 9));
    }
    band(&mut term, 4);

    let screen = Screen::replay(&tape, W, 16);
    assert_eq!(screen.lines()[..3], ["logo", "", "version"]);
    assert_eq!(screen.lines()[3..7], band_rows(4)[..]);
    assert!(
        screen.lines()[7..].iter().all(String::is_empty),
        "the popup's rows were left behind: {:?}",
        screen.lines()
    );
    assert!(screen.scrollback.is_empty());
}

#[test]
fn growth_past_the_bottom_scrolls_only_the_overflow() {
    let (mut term, tape) = term(10, 0, Scrollback::Regions);
    let transcript = rows(0..6);
    let refs: Vec<&str> = transcript.iter().map(String::as_str).collect();
    insert(&mut term, &refs);
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));

    band(&mut term, 6);
    assert_eq!(term.viewport_area, Rect::new(0, 4, W, 6));
    let screen = Screen::replay(&tape, W, 10);
    assert_eq!(screen.scrollback, rows(0..2));
    assert_eq!(screen.transcript(4), transcript);
    assert_eq!(screen.lines()[4..], band_rows(6)[..]);
}

#[test]
fn a_shrink_leaves_its_rows_under_the_footer_for_the_next_insert() {
    let (mut term, tape) = term(10, 0, Scrollback::Regions);
    insert(&mut term, &["r0", "r1", "r2", "r3"]);
    band(&mut term, 6);
    band(&mut term, 4);
    // the band re-grounds, and the transcript slides down with it
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));
    let lines = Screen::replay(&tape, W, 10).lines();
    assert_eq!(lines[5], "r3", "no blank row between transcript and band");
    assert_eq!(lines[6..], band_rows(4)[..]);
    assert!(lines[..2].iter().all(String::is_empty));

    insert(&mut term, &["r4"]);
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));

    insert(&mut term, &["r5", "r6"]);
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));
    let screen = Screen::replay(&tape, W, 10);
    assert_eq!(screen.scrollback, ["", "", "r0"]);
    let mut transcript = vec![String::new(), String::new()];
    transcript.extend(rows(0..7));
    assert_eq!(screen.transcript(6), transcript);
}

#[test]
fn a_modal_round_trip_repaints_the_band_in_place() {
    let (mut term, mut tape) = term(16, 0, Scrollback::Regions);
    insert(&mut term, &["banner", "/resume"]);
    band(&mut term, 4);
    let before = Screen::replay(&tape, W, 16).lines();

    write!(tape, "\x1b[3;1H❯ stale\x1b[4;1H❯").unwrap();
    term.clear().unwrap();
    band(&mut term, 4);

    let after = Screen::replay(&tape, W, 16).lines();
    assert_eq!(after, before);
    assert_eq!(after.iter().filter(|l| l.contains('❯')).count(), 1);
}

#[test]
fn full_screen_inserts_keep_the_band_out_of_scrollback() {
    let (mut term, tape) = term(8, 0, Scrollback::FullScreen);
    band(&mut term, 3);
    for i in 0..10 {
        insert(&mut term, &[format!("r{i}").as_str()]);
        band(&mut term, 3);
    }
    let screen = Screen::replay(&tape, W, 8);
    assert_eq!(screen.transcript(5), rows(0..10));
    assert_eq!(screen.lines()[5..], band_rows(3)[..]);
    assert_eq!(term.viewport_area, Rect::new(0, 5, W, 3));
}

#[test]
fn a_one_row_history_region_falls_back_to_whole_screen_scrolling() {
    let (mut term, tape) = term(5, 0, Scrollback::Regions);
    insert(&mut term, &["r0"]);
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 1, W, 4));
    insert(&mut term, &["r1", "r2"]);
    band(&mut term, 4);
    let screen = Screen::replay(&tape, W, 5);
    assert_eq!(screen.transcript(1), rows(0..3));
    assert_eq!(screen.lines()[1..], band_rows(4)[..]);
}

#[test]
fn hyperlinks_and_wide_glyphs_keep_the_rest_of_the_row_in_place() {
    let (mut term, tape) = term(8, 0, Scrollback::Regions);
    term.insert_before(2, |buf| {
        buf.set_string(0, 0, "abc", Style::default());
        buf[(0, 0)].set_symbol("\x1b]8;;https://example.com\x07a\x1b]8;;\x07");
        buf.set_string(0, 1, "中文x", Style::default());
    })
    .unwrap();
    let lines = Screen::replay(&tape, W, 8).lines();
    assert_eq!(lines[0], "abc");
    assert_eq!(lines[1], "中文x");
}

#[test]
fn a_frame_on_a_shrunken_screen_does_not_scroll_history() {
    let (mut term, tape) = term(10, 0, Scrollback::Regions);
    let transcript = rows(0..6);
    let refs: Vec<&str> = transcript.iter().map(String::as_str).collect();
    insert(&mut term, &refs);
    band(&mut term, 4);
    let mark = tape.len();
    term.set_viewport_height(4, Size::new(W, 8)).unwrap();
    assert_eq!(term.viewport_area, Rect::new(0, 4, W, 4));
    assert!(!tape.since(mark).contains("\r\n"), "history was scrolled");
}

#[test]
fn a_popup_on_a_full_screen_regrounds_the_band() {
    let (mut term, tape) = term(10, 0, Scrollback::Regions);
    let transcript = rows(0..6);
    let refs: Vec<&str> = transcript.iter().map(String::as_str).collect();
    insert(&mut term, &refs);
    band(&mut term, 4);
    band(&mut term, 6);
    band(&mut term, 4);
    assert_eq!(term.viewport_area, Rect::new(0, 6, W, 4));
    let screen = Screen::replay(&tape, W, 10);
    assert_eq!(screen.scrollback, rows(0..2));
    assert!(screen.lines()[..2].iter().all(String::is_empty));
    assert_eq!(screen.lines()[2..6], rows(2..6)[..]);
    assert_eq!(screen.lines()[6..], band_rows(4)[..]);
}

#[test]
fn a_busy_turn_does_not_tear_the_transcript() {
    use ratatui::text::Line;

    fn band_fill(term: &mut Term, height: u16, transcript: &[String]) {
        let screen = term.last_known_screen_size;
        let slid = term.set_viewport_height(height, screen).unwrap();
        if slid > 0 {
            let top = usize::from(term.viewport_area.y);
            let old_top = top - slid as usize;
            let len = transcript.len();
            if len > old_top {
                let fill: Vec<Line> = transcript[len.saturating_sub(top)..len - old_top]
                    .iter()
                    .map(|s| Line::from(s.clone()))
                    .collect();
                let y = slid.saturating_sub(fill.len() as u16);
                term.paint_lines_at(y, &fill).unwrap();
            }
        }
        term.draw(|frame| {
            let area = frame.area();
            for i in 0..area.height {
                let label = if i + 2 == area.height {
                    "\u{276f}".to_string()
                } else {
                    format!("band{i}")
                };
                frame
                    .buffer
                    .set_string(area.x, area.y + i, label, Style::default());
            }
        })
        .unwrap();
    }

    let (mut term, tape) = term(24, 0, Scrollback::Regions);

    let seed = rows(0..30);
    let refs: Vec<&str> = seed.iter().map(String::as_str).collect();
    insert(&mut term, &refs);
    let mut expected: Vec<String> = seed.clone();
    let mut n = 30usize;
    band_fill(&mut term, 4, &expected);
    assert_eq!(term.viewport_area, Rect::new(0, 20, W, 4));

    let step = |term: &mut Term, expected: &mut Vec<String>, n: &mut usize| {
        let chunk = rows(*n..*n + 2);
        let refs: Vec<&str> = chunk.iter().map(String::as_str).collect();
        insert(term, &refs);
        expected.extend(chunk);
        *n += 2;
        band_fill(term, 7, expected);
        band_fill(term, 4, expected);
    };

    for _ in 0..6 {
        step(&mut term, &mut expected, &mut n);
    }

    let img: Vec<String> = (0..27).map(|i| format!("img{i:02}")).collect();
    let refs: Vec<&str> = img.iter().map(String::as_str).collect();
    insert(&mut term, &refs);
    expected.extend(img.clone());
    band_fill(&mut term, 4, &expected);

    for _ in 0..6 {
        step(&mut term, &mut expected, &mut n);
    }

    let screen = Screen::replay(&tape, W, 24);
    let got = screen.transcript(20);
    for (i, row) in got.iter().enumerate() {
        assert!(!row.is_empty(), "torn transcript: blank row at index {i}");
    }
    let mut it = got.iter().peekable();
    for want in &expected {
        while it.peek().is_some_and(|g| *g != want) {
            it.next();
        }
        assert!(
            it.next().is_some_and(|g| g == want),
            "expected row {want:?} missing from transcript"
        );
    }
    assert_eq!(screen.lines()[20..], band_rows(4)[..]);
}
