use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const BLOCK: char = '\u{2580}';

type MascotRun = ((u8, u8, u8), (u8, u8, u8), usize);

const MASCOT_PNG: &[u8] = include_bytes!("../assets/graychan.png");

static MASCOT_SHOWN: AtomicBool = AtomicBool::new(false);

pub(crate) fn mascot_shown() -> bool {
    MASCOT_SHOWN.load(Ordering::Relaxed)
}

pub(crate) fn set_mascot_shown(shown: bool) {
    MASCOT_SHOWN.store(shown, Ordering::Relaxed);
}

pub(crate) struct MascotGrid {
    cols: usize,
    rows: usize,
    px: Vec<u8>,
}

impl MascotGrid {
    fn px(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * self.cols + x) * 3;
        (self.px[i], self.px[i + 1], self.px[i + 2])
    }

    fn runs(&self, row: usize) -> Vec<MascotRun> {
        let mut out = Vec::new();
        let mut x = 0;
        while x < self.cols {
            let top = self.px(x, row * 2);
            let bot = self.px(x, row * 2 + 1);
            let mut n = 1;
            while x + n < self.cols
                && self.px(x + n, row * 2) == top
                && self.px(x + n, row * 2 + 1) == bot
            {
                n += 1;
            }
            out.push((top, bot, n));
            x += n;
        }
        out
    }

    fn lines(&self, pad: usize) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(self.rows);
        for row in 0..self.rows {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if pad > 0 {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            for (top, bot, n) in self.runs(row) {
                spans.push(Span::styled(
                    BLOCK.to_string().repeat(n),
                    Style::default()
                        .fg(Color::Rgb(top.0, top.1, top.2))
                        .bg(Color::Rgb(bot.0, bot.1, bot.2)),
                ));
            }
            out.push(Line::from(spans));
        }
        out
    }
}

pub(crate) fn decode_grid(term_cols: u16, term_rows: u16) -> Option<MascotGrid> {
    let cols = term_cols as usize;
    let rows = term_rows as usize;
    if cols < 20 || rows < 10 {
        return None;
    }
    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(MASCOT_PNG), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(2048);
    limits.max_image_height = Some(2048);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().ok()?.to_rgb8();
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    if iw == 0 || ih == 0 {
        return None;
    }
    let rows_cap = rows.saturating_sub(9).max(8);
    let cols_fit = (rows_cap as f32 * 2.0 * iw as f32 / ih as f32).round() as usize;
    let grid_cols = cols.min(cols_fit).max(8);
    let grid_rows = ((grid_cols as f32 * ih as f32 / iw as f32) / 2.0)
        .round()
        .max(2.0) as usize;
    let scaled = image::imageops::resize(
        &img,
        grid_cols as u32,
        (grid_rows * 2) as u32,
        image::imageops::FilterType::Lanczos3,
    );
    Some(MascotGrid {
        cols: grid_cols,
        rows: grid_rows,
        px: scaled.into_raw(),
    })
}

pub(crate) fn mascot_lines(
    term_cols: u16,
    term_rows: u16,
    center_in: Option<usize>,
) -> Option<Vec<Line<'static>>> {
    if std::env::var_os("NO_COLOR").is_some() {
        return None;
    }
    mascot_lines_unchecked(term_cols, term_rows, center_in)
}

fn mascot_lines_unchecked(
    term_cols: u16,
    term_rows: u16,
    center_in: Option<usize>,
) -> Option<Vec<Line<'static>>> {
    let grid = decode_grid(term_cols, term_rows)?;
    let pad = center_in
        .unwrap_or(term_cols as usize)
        .saturating_sub(grid.cols)
        / 2;
    Some(grid.lines(pad))
}

pub(crate) fn print_mascot() -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }
    let (cols, rows) = crossterm::terminal::size().unwrap_or((0, 0));
    let Some(grid) = decode_grid(cols, rows) else {
        return false;
    };
    let pad = (cols as usize).saturating_sub(grid.cols) / 2;
    let mut out = std::io::stdout().lock();
    for row in 0..grid.rows {
        let mut line = String::new();
        if pad > 0 {
            line.push_str(&" ".repeat(pad));
        }
        for (top, bot, n) in grid.runs(row) {
            line.push_str(&format!(
                "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m{}",
                top.0,
                top.1,
                top.2,
                bot.0,
                bot.1,
                bot.2,
                BLOCK.to_string().repeat(n)
            ));
        }
        line.push_str("\x1b[0m\r\n");
        let _ = out.write_all(line.as_bytes());
    }
    let _ = out.flush();
    true
}

pub(crate) fn print_banner() -> bool {
    if mascot_shown() {
        set_mascot_shown(false);
        crate::tui::print_logo();
        return true;
    }
    let shown = print_mascot();
    set_mascot_shown(shown);
    shown
}

#[path = "mascot_tests.rs"]
#[cfg(test)]
mod tests;
