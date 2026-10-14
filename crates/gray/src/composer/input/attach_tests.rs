use super::*;

#[cfg(unix)]
#[test]
fn file_url_percent_decoding() {
    assert_eq!(
        decoded_paste_path("file:///tmp/my%20photo.png"),
        "/tmp/my photo.png"
    );
    assert_eq!(
        decoded_paste_path("file://localhost/tmp/my%20photo.png"),
        "/tmp/my photo.png"
    );
    assert_eq!(
        decoded_paste_path("\"file:///tmp/a%20b.png\""),
        "/tmp/a b.png"
    );
}

#[test]
fn raw_paths_pass_through() {
    assert_eq!(decoded_paste_path("/tmp/100%.png"), "/tmp/100%.png");
    assert_eq!(decoded_paste_path("/tmp/plain.png"), "/tmp/plain.png");
}

#[test]
fn attach_image_leaves_draft_text_untouched() {
    let mut tui = Tui::test_tui(80, 30);
    tui.textarea.set_text("prefix suffix");
    tui.textarea.set_cursor(7);
    let path = PathBuf::from("/tmp/pic one.png");
    attach_image(&mut tui, path.clone());
    assert_eq!(tui.textarea.text(), "prefix suffix");
    assert_eq!(tui.textarea.cursor(), 7);
    assert_eq!(tui.attachments, vec![path]);
}
