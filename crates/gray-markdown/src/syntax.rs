//! Syntax highlighting support using syntect.
//!
//! This module provides the `Syntect` struct which holds the syntax definitions
//! and theme for code block highlighting.

use std::io::Cursor;
use std::path::Path;

use syntect::{
    easy::HighlightLines,
    highlighting::{Theme as SyntectTheme, ThemeSet},
    parsing::{SyntaxReference, SyntaxSet},
};

fn syntax_set_extra_newlines() -> &'static SyntaxSet {
    static SET: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SET.get_or_init(two_face::syntax::extra_newlines)
}

fn syntax_set_defaults() -> &'static SyntaxSet {
    static SET: std::sync::OnceLock<SyntaxSet> = std::sync::OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// Syntax highlighting configuration.
///
/// Holds the theme and syntax definitions for code highlighting.
/// Create one instance and pass it to the markdown renderer.
pub struct Syntect {
    /// The color theme for syntax highlighting.
    pub theme: SyntectTheme,
    /// The syntax definitions (supports 250+ languages via two-face).
    pub syntax_set: &'static SyntaxSet,
}

impl Syntect {
    /// Create a new Syntect instance from theme bytes.
    ///
    /// The theme bytes should be a TextMate `.tmTheme` file.
    /// Uses two-face's extended syntax set with 250+ languages.
    /// A corrupt theme falls back to the default theme instead of panicking.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let syntect = Syntect::new(include_bytes!("assets/tokyo-night.tmTheme"));
    /// ```
    pub fn new(theme_bytes: &[u8]) -> Self {
        let mut cursor = Cursor::new(theme_bytes);
        let theme =
            ThemeSet::load_from_reader(&mut cursor).unwrap_or_else(|_| Self::default().theme);
        // Use two-face's extended syntax set which includes 250+ languages from bat
        let syntax_set = syntax_set_extra_newlines();
        Self { theme, syntax_set }
    }
}

impl Default for Syntect {
    fn default() -> Self {
        let ts = ThemeSet::load_defaults();
        let theme = ts
            .themes
            .get("base16-ocean.dark")
            .cloned()
            .or_else(|| ts.themes.values().next().cloned())
            .unwrap_or_default();
        let syntax_set = syntax_set_defaults();
        Self { theme, syntax_set }
    }
}

/// Get a shared, static Syntect instance.
///
/// With no palette set, the bundled Tokyo Night theme so production rendering
/// matches the theme used across markdown tests. With a palette, a
/// palette-derived theme cached per distinct palette (one leak per palette).
pub fn get_syntect() -> &'static Syntect {
    if let Some(p) = crate::style::palette() {
        return themed_syntect(p);
    }
    static SYNTECT: std::sync::OnceLock<Syntect> = std::sync::OnceLock::new();
    SYNTECT.get_or_init(|| Syntect::new(include_bytes!("../assets/tokyo-night.tmTheme")))
}

fn themed_syntect(p: crate::style::Palette) -> &'static Syntect {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static THEMED: OnceLock<Mutex<HashMap<crate::style::Palette, &'static Syntect>>> =
        OnceLock::new();
    let map = THEMED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut map = map.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(p).or_insert_with(|| {
        Box::leak(Box::new(Syntect {
            theme: palette_theme(&p),
            syntax_set: syntax_set_extra_newlines(),
        }))
    })
}

fn palette_theme(p: &crate::style::Palette) -> SyntectTheme {
    use std::str::FromStr;
    use syntect::highlighting::{
        Color as SynColor, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem,
        ThemeSettings,
    };
    let c = |v: [u8; 3]| SynColor {
        r: v[0],
        g: v[1],
        b: v[2],
        a: 255,
    };
    let item = |scope: &str, fg: Option<SynColor>, font: FontStyle| ThemeItem {
        scope: ScopeSelectors::from_str(scope).unwrap_or_default(),
        style: StyleModifier {
            foreground: fg,
            background: None,
            font_style: (!font.is_empty()).then_some(font),
        },
    };
    Theme {
        name: Some("gray-palette".to_string()),
        settings: ThemeSettings {
            foreground: Some(c(p.text)),
            background: Some(c(p.bg)),
            caret: Some(c(p.text)),
            selection: Some(c(p.faint)),
            ..Default::default()
        },
        scopes: vec![
            item(
                "comment, punctuation.definition.comment",
                Some(c(p.comment)),
                FontStyle::ITALIC,
            ),
            item(
                "keyword, keyword.control, storage.modifier, storage.type.function",
                Some(c(p.keyword)),
                FontStyle::empty(),
            ),
            item("variable.language", Some(c(p.keyword)), FontStyle::ITALIC),
            item(
                "string, punctuation.definition.string",
                Some(c(p.string)),
                FontStyle::empty(),
            ),
            item(
                "string.regexp, constant.character.escape, support.constant, \
                 constant.other, variable.other.constant",
                Some(c(p.constant)),
                FontStyle::empty(),
            ),
            item(
                "constant.numeric, constant.language",
                Some(c(p.number)),
                FontStyle::empty(),
            ),
            item(
                "entity.name.function, support.function, variable.function, \
                 meta.function-call.generic",
                Some(c(p.function)),
                FontStyle::empty(),
            ),
            item(
                "entity.name.type, entity.name.class, entity.name.namespace, \
                 support.type, support.class, storage.type, entity.other.inherited-class",
                Some(c(p.type_)),
                FontStyle::empty(),
            ),
            item(
                "keyword.operator, punctuation",
                Some(c(p.operator)),
                FontStyle::empty(),
            ),
            item("entity.name.tag", Some(c(p.tag)), FontStyle::empty()),
            item(
                "entity.other.attribute-name",
                Some(c(p.attribute)),
                FontStyle::empty(),
            ),
            item("variable.parameter", Some(c(p.text)), FontStyle::ITALIC),
            item("markup.heading", Some(c(p.heading)), FontStyle::BOLD),
            item("markup.bold", None, FontStyle::BOLD),
            item("markup.italic", None, FontStyle::ITALIC),
            item("markup.inserted", Some(c(p.inserted)), FontStyle::empty()),
            item("markup.deleted", Some(c(p.deleted)), FontStyle::empty()),
            item("invalid", Some(c(p.invalid)), FontStyle::empty()),
        ],
        ..Default::default()
    }
}

/// Convert a syntect Style to a Ratatui Style with foreground and font modifiers.
///
/// The foreground goes through [`crate::colors::adapt_color`] so code colors
/// respect the same terminal handling as every other markdown style:
/// truecolor passthrough, 256/16-color downgrade, `NO_COLOR` (no fg), and
/// polarity-safe remapping in minimal mode. Previously this emitted raw
/// `Color::Rgb` unconditionally, so code blocks stayed truecolor while
/// surrounding text was adapted (or vice versa) — the "sometimes wrong
/// colors" inconsistency.
pub fn syntect_to_ratatui_fg(style: syntect::highlighting::Style) -> ratatui::style::Style {
    use ratatui::style::{Modifier, Style};
    let rgb = anstyle::RgbColor(style.foreground.r, style.foreground.g, style.foreground.b);
    let mut s = match crate::colors::adapt_color(anstyle::Color::Rgb(rgb)) {
        Some(adapted) => Style::default().fg(crate::colors::anstyle_to_ratatui_color(adapted)),
        None => Style::default(),
    };
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        s = s.add_modifier(Modifier::BOLD);
    }
    if style
        .font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        s = s.add_modifier(Modifier::ITALIC);
    }
    s
}

impl Syntect {
    /// Find a syntax definition by file path extension.
    pub fn find_syntax_by_file_path(&self, file_path: &Path) -> Option<&SyntaxReference> {
        let ext = file_path.extension()?.to_str()?;
        self.syntax_set.find_syntax_by_extension(ext)
    }

    /// Find a syntax definition by language token (e.g., "rust", "python").
    pub fn find_syntax_by_token(&self, token: &str) -> Option<&SyntaxReference> {
        self.syntax_set.find_syntax_by_token(token)
    }

    /// Create a highlighter for the given file path.
    pub fn highlight_lines_by_file_path(&self, file_path: &Path) -> Option<HighlightLines<'_>> {
        Some(HighlightLines::new(
            self.find_syntax_by_file_path(file_path)?,
            &self.theme,
        ))
    }

    /// Create a highlighter for the given language token.
    pub fn highlight_lines_for_token(&self, token: &str) -> Option<HighlightLines<'_>> {
        Some(HighlightLines::new(
            self.find_syntax_by_token(token)?,
            &self.theme,
        ))
    }

    /// Highlighter for a fenced code block *info* string: a normal language token
    /// (e.g. `rust`, `python`), or a **line-range citation** of the form
    /// `lineStart:lineEnd:path/to/file.ext` where the syntax is resolved the same
    /// way as [`Syntect::highlight_lines_by_file_path`] (see
    /// [`Syntect::find_syntax_by_file_path`]).
    ///
    /// If the string matches the citation form but no syntax is found for the
    /// path, this falls back to [`Syntect::find_syntax_by_token`] with the full
    /// `fence_info` string, so plain ` ```lang` blocks keep working and odd
    /// citations degrade like the pre-citation code path.
    pub fn highlight_lines_for_fence_info(&self, fence_info: &str) -> Option<HighlightLines<'_>> {
        Some(HighlightLines::new(
            self.find_syntax_for_fence_info(fence_info)?,
            &self.theme,
        ))
    }

    /// Resolve the [`SyntaxReference`] for a fenced code block *info* string,
    /// using the SAME rules as [`Syntect::highlight_lines_for_fence_info`]:
    /// a `lineStart:lineEnd:path` citation resolves by file path, otherwise
    /// (or if the path has no known syntax) it falls back to a language token.
    ///
    /// Exposed so the incremental open-code highlighter can build its own
    /// resumable `ParseState`/`HighlightState` against exactly the syntax the
    /// batch `HighlightLines` path would have used — keeping the two
    /// byte-identical.
    pub(crate) fn find_syntax_for_fence_info(&self, fence_info: &str) -> Option<&SyntaxReference> {
        // Fence info strings often carry extra params after the language
        // (e.g. ```rust ignore, ```python linenums). Only the first
        // whitespace-separated token is the language. Previously the full
        // info string was passed to `find_syntax_by_token`, so any extra
        // word — or a capitalised token like `Rust` — silently missed and
        // the block fell back to unhighlighted (no colors), while clean
        // fences highlighted fine: the "sometimes no colors" inconsistency.
        let token = fence_info.split_whitespace().next().unwrap_or("").trim();
        if token.is_empty() {
            return None;
        }
        if let Some((_, _, path)) = parse_line_citation_fence_info(token)
            && let Some(s) = self.find_syntax_by_file_path(Path::new(path))
        {
            return Some(s);
        }
        self.find_syntax_by_token(token)
            .or_else(|| self.find_syntax_by_token(&token.to_ascii_lowercase()))
            // Common LLM shorthand (` ```js `, ` ```sh `, ` ```yml `) is an
            // extension, not a syntect token — resolve via extension lookup
            // before giving up and rendering unhighlighted.
            .or_else(|| self.syntax_set.find_syntax_by_extension(token))
            .or_else(|| {
                self.syntax_set
                    .find_syntax_by_extension(&token.to_ascii_lowercase())
            })
    }
}

/// ```text
/// lineStart:lineEnd:path/to/file.ext
/// ```
///
/// The path is the segment after the **second** colon; it is then parsed with
/// [`Path::new`]. Paths with extra colons in the first two segments (e.g. some
/// Windows `C:...` forms) are not supported; use a repo-relative or
/// forward-slash form.
fn parse_line_citation_fence_info(info: &str) -> Option<(&str, &str, &str)> {
    let mut it = info.splitn(3, ':');
    let start = it.next()?;
    let end = it.next()?;
    let path = it.next()?;
    if start.is_empty() || !start.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if end.is_empty() || !end.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    if path.is_empty() {
        return None;
    }
    Some((start, end, path))
}

/// Syntax highlight code, returning raw styled segments per line.
///
/// `fence_info` is the fenced code block *info* string (language tag or
/// `lineStart:lineEnd:path` citation form); see
/// [`Syntect::highlight_lines_for_fence_info`]. Lives here (not in `parse`)
/// so both the parser and the streaming highlighter caches depend one-way on
/// `syntax`.
pub(crate) fn syntax_highlight_raw(
    syntect: Option<&Syntect>,
    fence_info: &str,
    text: &str,
) -> Option<Vec<Vec<(syntect::highlighting::Style, String)>>> {
    use syntect::util::LinesWithEndings;

    let syn = syntect?;
    let mut hl = syn.highlight_lines_for_fence_info(fence_info)?;
    let mut lines = Vec::new();
    for line in LinesWithEndings::from(text) {
        let highlighted = hl.highlight_line(line, syn.syntax_set).ok()?;
        lines.push(
            highlighted
                .into_iter()
                .map(|(s, t)| (s, t.to_string()))
                .collect(),
        );
    }
    Some(lines)
}
