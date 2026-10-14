use super::*;

#[test]
fn line_format_matches_spec() {
    let s = ToolStats {
        tool: "read",
        class: CLASS_RETRIEVAL,
        path: "long.txt",
        bytes: 51_200,
        lines: 1846,
        truncated_by: CUT_BYTES,
    };
    assert_eq!(
        s.line(),
        "tool=read class=retrieval path=long.txt bytes=51200 lines=1846 \
             est_tokens=12800 truncated_by=bytes"
    );
}

#[test]
fn classify_splits_retrieval_from_action() {
    assert_eq!(classify("read"), CLASS_RETRIEVAL);
    assert_eq!(classify("grep"), CLASS_RETRIEVAL);
    assert_eq!(classify("find"), CLASS_RETRIEVAL);
    assert_eq!(classify("ls"), CLASS_RETRIEVAL);
    assert_eq!(classify("edit"), CLASS_ACTION);
    assert_eq!(classify("write"), CLASS_ACTION);

    assert_eq!(classify("bash"), CLASS_OTHER);
    assert_eq!(classify("some_plugin_tool"), CLASS_OTHER);
}
