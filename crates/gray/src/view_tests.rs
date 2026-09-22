use super::*;

use std::io::Cursor;

fn png() -> Vec<u8> {
    let img = image::RgbImage::from_pixel(4, 4, image::Rgb([3, 2, 1]));
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    buf
}

#[test]
fn run_cli_reports_every_image_it_showed() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a.png");
    let b = dir.path().join("b.png");
    std::fs::write(&a, png()).unwrap();
    std::fs::write(&b, png()).unwrap();

    let paths = vec![
        a.to_string_lossy().into_owned(),
        b.to_string_lossy().into_owned(),
    ];
    let (shown, failed) = view_lines(&paths);
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert!(shown[0].starts_with("viewed "), "{shown:?}");
    assert!(failed.is_empty(), "{failed:?}");
}

#[test]
fn run_cli_keeps_going_and_reports_failures() {
    let dir = tempfile::tempdir().unwrap();
    let good = dir.path().join("good.png");
    std::fs::write(&good, png()).unwrap();
    let text = dir.path().join("notes.md");
    std::fs::write(&text, "not an image").unwrap();

    let paths = vec![
        text.to_string_lossy().into_owned(),
        good.to_string_lossy().into_owned(),
    ];
    let (shown, failed) = view_lines(&paths);
    assert_eq!(shown.len(), 1, "the good one still shows: {shown:?}");
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].contains("use cat for text files"), "{failed:?}");
}

#[test]
fn run_cli_reports_a_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let (shown, failed) = view_lines(&[dir.path().join("gone.png").to_string_lossy().into_owned()]);
    assert!(shown.is_empty());
    assert_eq!(failed.len(), 1, "{failed:?}");
    assert!(failed[0].contains("view failed"), "{failed:?}");
}

#[test]
fn run_cli_with_no_paths_is_not_an_error() {
    // The CLI layer requires >=1; the parser is the only guard, so the empty
    // case is a no-op rather than a crash.
    let (shown, failed) = view_lines(&[]);
    assert!(shown.is_empty() && failed.is_empty());
}
