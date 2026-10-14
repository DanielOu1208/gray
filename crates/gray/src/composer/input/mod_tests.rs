use super::*;

#[test]
fn strip_escape_sequences_removes_paste_markers_and_keeps_text() {
    assert_eq!(
        strip_escape_sequences("\u{1b}[200~hello world\u{1b}[201~"),
        "hello world"
    );

    assert_eq!(
        strip_escape_sequences("\u{1b}]0;title\u{7}a\u{1b}[2Kb"),
        "ab"
    );

    assert_eq!(
        strip_escape_sequences("fn main() {\n\tprintln!(\"hi\");\n}"),
        "fn main() {\n\tprintln!(\"hi\");\n}"
    );

    assert_eq!(strip_escape_sequences("a\u{1b}b"), "ab");
    assert_eq!(strip_escape_sequences("\u{1b}[201~"), "");
}
