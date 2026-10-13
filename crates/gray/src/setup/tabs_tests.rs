use super::*;

#[test]
fn count_badge_renders_only_when_some() {
    let segs = tab_segments(&[("Installed", None), ("Errors", Some(2))], 0);
    assert_eq!(segs[0].text, "Installed");
    assert_eq!(segs[1].text, "Errors (2)");
    let segs = tab_segments(&[("Installed", None), ("Errors", None)], 1);
    assert_eq!(segs[0].text, "Installed");
    assert_eq!(segs[1].text, "Errors");
}
