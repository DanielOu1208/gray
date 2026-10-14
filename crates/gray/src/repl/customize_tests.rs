use super::*;

fn setup() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let themes = root.path().join("themes");
    let config = root.path().join("config.json");
    (root, themes, config)
}

#[test]
fn list_marks_the_active_theme() {
    let (_root, themes, config) = setup();
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(themes.join("dusk.json"), r#"{"colors":{}}"#).unwrap();
    let out = theme_command(None, Some("dusk"), &themes, &config);
    assert!(out.apply.is_none());
    assert!(out.message.contains("● dusk"), "{}", out.message);
    assert!(out.message.contains("  gray (built-in)"), "{}", out.message);
}

#[test]
fn switching_applies_and_persists() {
    let (_root, themes, config) = setup();
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(themes.join("dusk.json"), r#"{"colors":{"accent":"red"}}"#).unwrap();
    let out = theme_command(Some("dusk"), None, &themes, &config);
    let (t, name) = out.apply.expect("applies");
    assert_eq!(t.accent, ratatui::style::Color::Red);
    assert_eq!(name.as_deref(), Some("dusk"));
    assert_eq!(
        crate::setup::load_saved_config_at(&config).theme.as_deref(),
        Some("dusk")
    );
    let back = theme_command(Some("gray"), Some("dusk"), &themes, &config);
    assert_eq!(back.apply.unwrap().0, theme::GRAY_UI_THEME);
    assert!(crate::setup::load_saved_config_at(&config).theme.is_none());
}

#[test]
fn missing_theme_changes_nothing() {
    let (_root, themes, config) = setup();
    let out = theme_command(Some("nope"), None, &themes, &config);
    assert!(out.apply.is_none());
    assert!(out.message.starts_with("theme not applied"));
    assert!(!config.exists());
}

#[test]
fn new_writes_an_editable_full_palette_once() {
    let (_root, themes, config) = setup();
    let out = theme_command(Some("new mine"), None, &themes, &config);
    assert!(out.apply.is_none());
    let text = std::fs::read_to_string(themes.join("mine.json")).unwrap();
    let (parsed, warnings) = theme::parse_theme(&text).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(parsed, theme::theme());
    let again = theme_command(Some("new mine"), None, &themes, &config);
    assert!(again.message.contains("already exists"));
    assert!(
        theme_command(Some("new gray"), None, &themes, &config)
            .message
            .contains("can't be")
    );
}

fn card_text(line: &ratatui::text::Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.to_string()).collect()
}

#[test]
fn switch_card_names_the_theme_on_its_own_palette() {
    let mut palette = theme::GRAY_UI_THEME;
    palette.surface_bg = ratatui::style::Color::Rgb(9, 9, 9);
    palette.success = ratatui::style::Color::Rgb(1, 2, 3);
    let rows = theme_card_lines(&palette, "dusk", false, 60);
    assert_eq!(rows.len(), 2);
    let head = card_text(&rows[0]);
    assert!(head.starts_with(" ✓ Theme dusk"), "{head}");
    assert!(!head.contains("built-in"), "{head}");
    assert_eq!(rows[0].spans[1].style.fg, Some(palette.success));
    for row in &rows {
        assert_eq!(row.width(), 60, "{}", card_text(row));
        for span in &row.spans {

            if span.content == "▀▀" {
                continue;
            }
            assert_eq!(span.style.bg, Some(palette.surface_bg), "{span:?}");
        }
    }
}

#[test]
fn specimen_row_spells_each_role_in_its_color() {
    let palette = UiTheme {
        accent: ratatui::style::Color::Rgb(1, 0, 0),
        success: ratatui::style::Color::Rgb(0, 1, 0),
        error: ratatui::style::Color::Rgb(0, 0, 1),
        info: ratatui::style::Color::Rgb(1, 1, 0),
        rose: ratatui::style::Color::Rgb(1, 0, 1),
        ..theme::GRAY_UI_THEME
    };
    let rows = theme_card_lines(&palette, "dusk", false, 60);
    let roles = [
        ("accent", palette.accent),
        ("success", palette.success),
        ("error", palette.error),
        ("info", palette.info),
        ("rose", palette.rose),
    ];
    for (word, fg) in roles {
        let span = rows[1]
            .spans
            .iter()
            .find(|s| s.content == word)
            .unwrap_or_else(|| panic!("{word} missing"));
        assert_eq!(span.style.fg, Some(fg), "{word}");
    }
}

#[test]
fn builtin_card_says_built_in_with_the_swatch_at_the_right_edge() {
    let rows = theme_card_lines(&theme::GRAY_UI_THEME, "gray", true, 60);
    let head = card_text(&rows[0]);
    assert!(head.starts_with(" ✓ Theme gray (built-in)"), "{head}");
    let spans = &rows[0].spans;
    assert_eq!(spans.iter().filter(|s| s.content == "▀▀").count(), 8);
    assert_eq!(spans[spans.len() - 2].content, "▀▀");
    assert_eq!(spans[spans.len() - 1].content, " ");
}

#[test]
fn swatch_steps_aside_when_the_width_is_too_small() {
    let rows = theme_card_lines(&theme::GRAY_UI_THEME, "dusk", false, 20);
    assert_eq!(rows[0].width(), 20);
    assert!(rows[0].spans.iter().all(|s| s.content != "▀▀"));
}

#[test]
fn builtin_switch_has_a_card_and_no_notes() {
    let (_root, themes, config) = setup();
    let out = theme_command(Some("gray"), Some("dusk"), &themes, &config);
    let card = out.card.expect("a switch has a card");
    assert!(card.builtin);
    assert_eq!(card.name, "gray");
    assert!(card.notes.is_empty());
    assert_eq!(out.message, "✓ theme gray (built-in)");
}

#[test]
fn switch_card_keeps_palette_warnings_as_plain_notes() {
    let (_root, themes, config) = setup();
    std::fs::create_dir_all(&themes).unwrap();
    std::fs::write(themes.join("dusk.json"), r#"{"colors":{"nope":"red"}}"#).unwrap();
    let out = theme_command(Some("dusk"), None, &themes, &config);
    assert!(out.apply.is_some());
    let card = out.card.expect("a switch has a card");
    assert_eq!(card.name, "dusk");
    assert!(!card.builtin);
    assert_eq!(card.notes, ["warning: unknown role `nope`"]);
}

#[test]
fn failures_and_usage_stay_plain_text() {
    let (_root, themes, config) = setup();
    for arg in ["nope", "dusk extra", "new", "reload"] {
        let out = theme_command(Some(arg), None, &themes, &config);
        assert!(out.card.is_none(), "{arg}: {}", out.message);
        assert!(out.apply.is_none(), "{arg}");
    }
}

#[test]
fn notes_sit_under_the_card_in_one_block() {
    let card = ThemeCard {
        name: "dusk".into(),
        builtin: false,
        notes: vec!["not saved: denied".into()],
    };
    let lines = theme_success_lines(&theme::GRAY_UI_THEME, &card, 60);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2].to_string(), " not saved: denied");
}

#[test]
fn hotkeys_lists_actions_and_marks_overrides() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        crate::keymap::path_in(dir.path()),
        r#"{"tui.input.newLine":"ctrl+o","/compact":"f2"}"#,
    )
    .unwrap();
    let km = crate::keymap::load_in(dir.path());
    let (msg, apply) = keys_command(None, dir.path(), &km);
    assert!(apply.is_none());
    assert!(msg.contains("tui.editor.cursorWordLeft"));
    assert!(
        msg.lines()
            .any(|l| l.contains("tui.input.newLine") && l.contains('*'))
    );
    assert!(msg.contains("f2") && msg.contains("/compact"));
}

#[test]
fn hotkeys_reload_returns_new_keymap_with_warnings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(crate::keymap::path_in(dir.path()), r#"{"nope.x":"f3"}"#).unwrap();
    let (msg, apply) = keys_command(
        Some("reload"),
        dir.path(),
        &crate::keymap::Keymap::defaults(),
    );
    assert!(apply.is_some());
    assert!(msg.contains("reloaded") && msg.contains("unknown action `nope.x`"));
    let (usage, none) = keys_command(
        Some("bogus"),
        dir.path(),
        &crate::keymap::Keymap::defaults(),
    );
    assert!(none.is_none() && usage.starts_with("usage: /hotkeys"));
}
