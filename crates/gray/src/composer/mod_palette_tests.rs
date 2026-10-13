use super::palette_repaint_due;
use crate::theme::{GRAY_UI_THEME, UiTheme};
use ratatui::style::Color;

/// The gray palette with a cream window, as a light theme paints it.
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
    // Boot paints the history under gray.
    let mut painted = gray;
    // `/theme whitegold`: the command's retract re-emits under gray first,
    // then the switch lands. The switch must repaint once.
    assert!(palette_repaint_due(painted, white, false));
    painted = white;
    assert!(!palette_repaint_due(painted, white, false));
    // `/theme gray`: the retract re-emits under white first, baking cream
    // into every row. The switch back must repaint those rows too.
    assert!(palette_repaint_due(painted, gray, false));
    painted = gray;
    assert!(!palette_repaint_due(painted, gray, false));
}

#[test]
fn a_modal_preview_waits_for_the_modal_to_close() {
    // The picker previews palettes while it owns the screen: nothing may
    // repaint under it. The same switch is due once the modal closes.
    assert!(!palette_repaint_due(GRAY_UI_THEME, cream(), true));
    assert!(palette_repaint_due(GRAY_UI_THEME, cream(), false));
}
