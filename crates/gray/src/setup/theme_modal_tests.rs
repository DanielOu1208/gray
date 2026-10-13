use super::*;
use ratatui::backend::TestBackend;

fn row(name: &str) -> Row {
    Row {
        name: name.to_string(),
        theme: theme::GRAY_UI_THEME,
    }
}

fn twelve() -> Vec<Row> {
    (0..12).map(|i| row(&format!("theme{i:02}"))).collect()
}

/// Draws the picker over `rows` at `w`x`h` with `sel` selected and returns
/// the screen as one string per row.
fn screen(w: u16, h: u16, rows: &[Row], sel: usize) -> Vec<String> {
    let refs: Vec<&Row> = rows.iter().collect();
    let list_h = list_rows_for(h, refs.len());
    let bg = BackgroundSnapshot::default_initial();
    let mut terminal = ratatui::Terminal::new(TestBackend::new(w, h)).expect("test terminal");
    terminal
        .draw(|frame| draw_picker(frame, &bg, &refs, sel, "", list_h))
        .expect("draw picker");
    let buf = terminal.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect())
        .collect()
}

#[test]
fn specimen_rows_show_on_an_80x20_terminal() {
    let lines = screen(80, 20, &twelve(), 0);
    assert!(lines.iter().any(|l| l.contains("Aa bright")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("✓ success")), "{lines:#?}");
}

#[test]
fn specimen_rows_show_on_an_80x24_terminal() {
    let lines = screen(80, 24, &twelve(), 0);
    assert!(lines.iter().any(|l| l.contains("Aa bright")), "{lines:#?}");
    assert!(lines.iter().any(|l| l.contains("✓ success")), "{lines:#?}");
}

#[test]
fn list_scrolls_to_keep_the_selection_on_screen() {
    let lines = screen(80, 20, &twelve(), 11);
    assert!(lines.iter().any(|l| l.contains("theme11")), "{lines:#?}");
    assert!(!lines.iter().any(|l| l.contains("theme00")), "{lines:#?}");
}

#[test]
fn list_rows_leave_room_for_the_specimen() {
    assert_eq!(list_rows_for(20, 12), 9);
    assert_eq!(list_rows_for(24, 12), 12);
    assert_eq!(list_rows_for(20, 0), 1);
    assert_eq!(list_rows_for(8, 12), 1);
}
