use super::*;

#[test]
fn ctrl_c_first_press_clears_second_within_window_exits() {
    let now = std::time::Instant::now();
    // Draft present → never exit (first press clears).
    assert!(!ctrl_c_should_exit(true, None, now));
    assert!(!ctrl_c_should_exit(true, Some(now), now));
    // Empty, no prior press → arm, don't exit.
    assert!(!ctrl_c_should_exit(false, None, now));
    // Empty, second press inside 5 s → exit.
    let first = now - std::time::Duration::from_secs(2);
    assert!(ctrl_c_should_exit(false, Some(first), now));
    // Empty, prior press expired → don't exit.
    let stale = now - std::time::Duration::from_secs(30);
    assert!(!ctrl_c_should_exit(false, Some(stale), now));
}

#[test]
fn strip_escape_sequences_removes_paste_markers_and_keeps_text() {
    // A paste that arrives still wrapped (gray nested in another terminal, or
    // a child that cleared mode 2004) must not type `^[[200~` into the draft.
    assert_eq!(
        strip_escape_sequences("\u{1b}[200~hello world\u{1b}[201~"),
        "hello world"
    );
    // CSI/OSC debris copied out of a rendered page goes too.
    assert_eq!(
        strip_escape_sequences("\u{1b}]0;title\u{7}a\u{1b}[2Kb"),
        "ab"
    );
    // Escape sequences only: newlines and tabs of a pasted block survive.
    assert_eq!(
        strip_escape_sequences("fn main() {\n\tprintln!(\"hi\");\n}"),
        "fn main() {\n\tprintln!(\"hi\");\n}"
    );
    // A lone ESC is dropped, not turned into text.
    assert_eq!(strip_escape_sequences("a\u{1b}b"), "ab");
    assert_eq!(strip_escape_sequences("\u{1b}[201~"), "");
}
