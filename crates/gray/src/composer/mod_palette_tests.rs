use super::palette_repaint_due;
use crate::theme::{GRAY_UI_THEME, UiTheme};
use ratatui::style::Color;

fn cream() -> UiTheme {
    UiTheme {
        window_bg: Color::Rgb(0xf0, 0xef, 0xe6),
        ..GRAY_UI_THEME
    }
}

#[test]
fn an_unchanged_palette_is_never_repainted() {
    assert!(!palette_repaint_due(GRAY_UI_THEME, GRAY_UI_THEME, false));
    assert!(!palette_repaint_due(cream(), cream(), false));
}

#[test]
fn whitegold_then_gray_repaints_each_switch_once() {
    let (gray, white) = (GRAY_UI_THEME, cream());

    let mut painted = gray;

    assert!(palette_repaint_due(painted, white, false));
    painted = white;
    assert!(!palette_repaint_due(painted, white, false));

    assert!(palette_repaint_due(painted, gray, false));
    painted = gray;
    assert!(!palette_repaint_due(painted, gray, false));
}

#[test]
fn a_modal_preview_waits_for_the_modal_to_close() {
    assert!(!palette_repaint_due(GRAY_UI_THEME, cream(), true));
    assert!(palette_repaint_due(GRAY_UI_THEME, cream(), false));
}
