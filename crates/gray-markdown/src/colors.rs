use std::sync::OnceLock;

use anstyle::{Color, Effects};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum ColorLevel {
    None,

    Basic,

    Ansi256,

    #[default]
    TrueColor,
}

impl ColorLevel {
    pub fn has_256(self) -> bool {
        self >= Self::Ansi256
    }
}

static COLOR_LEVEL: OnceLock<ColorLevel> = OnceLock::new();

pub fn detect_color_level() -> ColorLevel {
    *COLOR_LEVEL.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return ColorLevel::None;
        }

        let level = match supports_color::on(supports_color::Stream::Stdout) {
            None => ColorLevel::TrueColor,
            Some(level) => {
                if level.has_16m {
                    ColorLevel::TrueColor
                } else if level.has_256 {
                    ColorLevel::Ansi256
                } else if level.has_basic {
                    ColorLevel::Basic
                } else {
                    ColorLevel::None
                }
            }
        };

        if level < ColorLevel::TrueColor && terminal_supports_truecolor() {
            return ColorLevel::TrueColor;
        }

        level
    })
}

fn terminal_supports_truecolor() -> bool {
    use std::env;

    if let Ok(prog) = env::var("TERM_PROGRAM") {
        let norm: String = prog
            .trim()
            .chars()
            .filter(|c| !matches!(c, ' ' | '-' | '_' | '.'))
            .map(|c| c.to_ascii_lowercase())
            .collect();

        if matches!(
            norm.as_str(),
            "iterm"
                | "iterm2"
                | "itermapp"
                | "ghostty"
                | "kitty"
                | "wezterm"
                | "alacritty"
                | "warp"
                | "warpterminal"
                | "vscode"
        ) {
            return true;
        }
    }

    env::var("ITERM_SESSION_ID").is_ok()
        || env::var("ITERM_PROFILE").is_ok()
        || env::var("WEZTERM_VERSION").is_ok()
        || env::var("KITTY_WINDOW_ID").is_ok()
        || env::var("ALACRITTY_SOCKET").is_ok()
}

pub fn adapt_color(color: Color) -> Option<Color> {
    let level = detect_color_level();

    match level {
        ColorLevel::None => None,
        ColorLevel::TrueColor => Some(color),
        ColorLevel::Ansi256 => Some(match color {
            Color::Rgb(rgb) => Color::Ansi256(anstyle_lossy::rgb_to_xterm(rgb)),
            other => other,
        }),
        ColorLevel::Basic => Some(match color {
            Color::Rgb(rgb) => {
                Color::Ansi(anstyle_lossy::rgb_to_ansi(rgb, anstyle_lossy::palette::VGA))
            }
            Color::Ansi256(idx) => Color::Ansi(anstyle_lossy::xterm_to_ansi(
                idx,
                anstyle_lossy::palette::VGA,
            )),
            Color::Ansi(ansi) => Color::Ansi(ansi),
        }),
    }
}

pub fn adapt_style(style: anstyle::Style) -> anstyle::Style {
    let fg = style.get_fg_color().and_then(adapt_color);
    let bg = style.get_bg_color().and_then(adapt_color);
    let effects = style.get_effects();

    let mut new_style = anstyle::Style::new();
    if let Some(fg) = fg {
        new_style = new_style.fg_color(Some(fg));
    }
    if let Some(bg) = bg {
        new_style = new_style.bg_color(Some(bg));
    }
    new_style | effects
}

pub(crate) fn anstyle_to_ratatui_style(style: anstyle::Style) -> ratatui::style::Style {
    use ratatui::style::{Modifier, Style as RStyle};

    let mut out = RStyle::default();

    if let Some(fg) = style.get_fg_color() {
        out = out.fg(anstyle_to_ratatui_color(fg));
    }
    if let Some(bg) = style.get_bg_color() {
        out = out.bg(anstyle_to_ratatui_color(bg));
    }

    let effects = style.get_effects();
    let mut modifiers = Modifier::empty();
    if effects.contains(Effects::BOLD) {
        modifiers |= Modifier::BOLD;
    }
    if effects.contains(Effects::DIMMED) {
        modifiers |= Modifier::DIM;
    }
    if effects.contains(Effects::ITALIC) {
        modifiers |= Modifier::ITALIC;
    }
    if effects.contains(Effects::UNDERLINE) {
        modifiers |= Modifier::UNDERLINED;
    }
    if effects.contains(Effects::STRIKETHROUGH) {
        modifiers |= Modifier::CROSSED_OUT;
    }
    if effects.contains(Effects::HIDDEN) {
        modifiers |= Modifier::HIDDEN;
    }

    out.add_modifier(modifiers)
}

pub(crate) fn anstyle_to_ratatui_color(color: anstyle::Color) -> ratatui::style::Color {
    use ratatui::style::Color;
    match color {
        anstyle::Color::Ansi(ansi) => match ansi {
            anstyle::AnsiColor::Black => Color::Black,
            anstyle::AnsiColor::Red => Color::Red,
            anstyle::AnsiColor::Green => Color::Green,
            anstyle::AnsiColor::Yellow => Color::Yellow,
            anstyle::AnsiColor::Blue => Color::Blue,
            anstyle::AnsiColor::Magenta => Color::Magenta,
            anstyle::AnsiColor::Cyan => Color::Cyan,
            anstyle::AnsiColor::White => Color::Gray,
            anstyle::AnsiColor::BrightBlack => Color::DarkGray,
            anstyle::AnsiColor::BrightRed => Color::LightRed,
            anstyle::AnsiColor::BrightGreen => Color::LightGreen,
            anstyle::AnsiColor::BrightYellow => Color::LightYellow,
            anstyle::AnsiColor::BrightBlue => Color::LightBlue,
            anstyle::AnsiColor::BrightMagenta => Color::LightMagenta,
            anstyle::AnsiColor::BrightCyan => Color::LightCyan,
            anstyle::AnsiColor::BrightWhite => Color::White,
        },
        anstyle::Color::Ansi256(idx) => Color::Indexed(idx.index()),
        anstyle::Color::Rgb(rgb) => Color::Rgb(rgb.0, rgb.1, rgb.2),
    }
}
