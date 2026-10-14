pub fn prefix_line(n: usize, line: &str) -> String {
    format!("{n:>6}\t{line}")
}

pub const MAX_LINE_CHARS: usize = 2000;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Cut {
    Lines,
    Bytes,
}

pub struct WindowLine {
    pub text: String,
    pub overflow_chars: u64,
}

pub struct Window {
    pub shown: Vec<String>,

    pub cut: Option<Cut>,

    pub clamped: usize,

    pub next_offset: Option<usize>,
}

pub fn clamp_counted(line: &str, overflow_chars: u64, max: usize) -> (String, bool) {
    let total = line.chars().count() as u64 + overflow_chars;
    if total <= max as u64 {
        return (line.to_string(), false);
    }
    let kept: String = line.chars().take(max).collect();
    (format!("{kept} …[+{} chars]", total - max as u64), true)
}

pub fn window(
    first_n: usize,
    lines: &[WindowLine],
    max_lines: usize,
    max_bytes: usize,
    max_chars: usize,
    has_more: bool,
) -> Window {
    let mut shown: Vec<String> = Vec::new();
    let mut bytes_used: usize = 0;
    let mut clamped = 0;
    for (i, l) in lines.iter().enumerate() {
        let n = first_n + i;
        if shown.len() >= max_lines {
            return Window {
                shown,
                cut: Some(Cut::Lines),
                clamped,
                next_offset: Some(n),
            };
        }
        let (c, hit) = clamp_counted(&l.text, l.overflow_chars, max_chars);
        let p = prefix_line(n, &c);
        let need = p.len() + if shown.is_empty() { 0 } else { 1 };
        if bytes_used + need > max_bytes {
            return Window {
                shown,
                cut: Some(Cut::Bytes),
                clamped,
                next_offset: Some(n),
            };
        }
        if hit {
            clamped += 1;
        }
        shown.push(p);
        bytes_used += need;
    }

    if shown.len() >= max_lines && max_lines > 0 && has_more {
        let next = first_n + lines.len();
        return Window {
            shown,
            cut: Some(Cut::Lines),
            clamped,
            next_offset: Some(next),
        };
    }
    Window {
        shown,
        cut: None,
        clamped,
        next_offset: None,
    }
}

#[path = "window_tests.rs"]
#[cfg(test)]
mod tests;
