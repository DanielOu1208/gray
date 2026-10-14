use super::*;
use std::io::Cursor;

#[test]
fn kind_map_covers_media() {
    assert_eq!(attachment_kind(Path::new("a.png")), AttachmentKind::Image);
    assert_eq!(attachment_kind(Path::new("a.JPG")), AttachmentKind::Image);
    assert_eq!(attachment_kind(Path::new("a.pdf")), AttachmentKind::Pdf);
    assert_eq!(attachment_kind(Path::new("a.mp4")), AttachmentKind::Video);
    assert_eq!(attachment_kind(Path::new("a.mkv")), AttachmentKind::Video);
    assert_eq!(attachment_kind(Path::new("a.mp3")), AttachmentKind::Audio);
    assert_eq!(attachment_kind(Path::new("a.wav")), AttachmentKind::Audio);
    assert_eq!(
        attachment_kind(Path::new("a.zip")),
        AttachmentKind::Unsupported
    );
    assert_eq!(
        attachment_kind(Path::new("noext")),
        AttachmentKind::Unsupported
    );
}

#[test]
fn normalize_keeps_long_side() {

    let img = image::RgbaImage::from_pixel(3000, 100, image::Rgba([9, 9, 9, 255]));
    let mut buf = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    let (mime, out) = normalize_image_bytes(&buf).unwrap();
    assert_eq!(mime, "image/png");
    let back = image::load_from_memory(&out).unwrap();
    assert_eq!((back.width(), back.height()), (3000, 100));
}

#[test]
fn normalize_rejects_garbage_loudly() {
    assert!(matches!(
        normalize_image_bytes(b"not an image"),
        Err(MediaError::Decode(_))
    ));
}

#[test]
fn pdf_missing_file_errors() {
    assert!(matches!(
        gray_tools::images::pdf_text(Path::new("/tmp/gray-test-no-such-file-xyz.pdf")),
        Err(MediaError::Extract(_))
    ));
}

#[test]
fn jpeg_exif_passes_through_byte_identical() {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 4, image::Rgb([90, 50, 20])));
    let mut encoded = Cursor::new(Vec::new());
    image
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .unwrap();
    let jpeg = encoded.into_inner();

    let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
    let mut oriented = jpeg[..2].to_vec();
    oriented.extend_from_slice(b"\xff\xe1");
    oriented.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    oriented.extend_from_slice(exif);
    oriented.extend_from_slice(&jpeg[2..]);
    let (mime, bytes) = normalize_image_bytes(&oriented).unwrap();
    assert_eq!(mime, "image/jpeg");
    assert_eq!(bytes, oriented);
}

#[test]
fn inline_links_mixed_with_words_attach() {
    let tmp = tempfile::Builder::new().suffix(".png").tempfile().unwrap();
    let path = tmp.path().to_path_buf();
    let cwd = std::path::Path::new("/tmp");
    let text = format!("look at {} please", path.display());
    let got = extract_inline_image_paths(&text, cwd);
    assert_eq!(got, vec![path]);
}

#[test]
fn inline_file_url_and_punctuation_attach_once() {
    let tmp = tempfile::Builder::new().suffix(".jpg").tempfile().unwrap();
    let path = tmp.path().to_path_buf();
    let url = format!("file://{}", path.display());
    let text = format!("{url}, and again {url}.");
    let got = extract_inline_image_paths(&text, std::path::Path::new("/tmp"));
    assert_eq!(got, vec![path]);
}

#[test]
fn inline_non_image_and_missing_ignored() {
    let cwd = std::path::Path::new("/tmp");
    let text = "see /tmp/gray-test-no-such-file-xyz.png and notes.txt";
    assert!(extract_inline_image_paths(text, cwd).is_empty());
}

#[test]
fn inline_relative_resolves_against_cwd() {
    let dir = tempfile::tempdir().unwrap();
    let rel = dir.path().join("shot.png");
    std::fs::write(&rel, b"fake").unwrap();
    let got = extract_inline_image_paths("check shot.png", dir.path());
    assert_eq!(got, vec![rel]);
}

#[test]
fn a_typed_video_path_is_media_too() {

    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip.mp4");
    std::fs::write(&clip, b"fake mp4").unwrap();
    let got = extract_inline_image_paths(&format!("look at {}", clip.display()), dir.path());
    assert_eq!(got, vec![clip], "a typed video path must be carried");
}

#[test]
fn label_names_every_kind_and_markers_round_trip() {
    assert_eq!(AttachmentKind::Video.label(), "Video");
    assert_eq!(AttachmentKind::Pdf.label(), "PDF");
    assert_eq!(AttachmentKind::Unsupported.label(), "File");

    let clip = Path::new("/tmp/clip.mp4");
    let marker = attachment_marker(clip);
    assert_eq!(marker, "[video: clip.mp4]");
    let found = attachment_markers(&format!("look {marker} now"));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].1, "clip.mp4");
}
