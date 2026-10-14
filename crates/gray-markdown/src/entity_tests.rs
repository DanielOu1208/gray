use crate::render_markdown_ratatui_full;
use crate::style::test_style;

fn lines_to_text(lines: &[ratatui::text::Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

fn pretty_lines(text: &str) -> Vec<String> {
    let (output, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
    lines_to_text(&output.lines)
}

fn raw_lines(text: &str) -> Vec<String> {
    let (output, _) = render_markdown_ratatui_full(text, test_style::STYLE, false, None);
    lines_to_text(&output.lines)
}

#[test]
fn lt_gt_amp_decoded_in_prose() {
    let lines = pretty_lines("Use &lt;tag&gt; with a &amp; b.\n\n");
    assert_eq!(lines[0], "Use <tag> with a & b.", "got: {lines:#?}");
}

#[test]
fn multiple_entities_one_paragraph() {
    let lines = pretty_lines("1 &lt; 2 &amp;&amp; 3 &gt; 2\n\n");
    assert_eq!(lines[0], "1 < 2 && 3 > 2", "got: {lines:#?}");
}

#[test]
fn quote_and_apostrophe_entities() {
    let lines = pretty_lines("&quot;hello&quot; &amp; &#39;world&#39;\n\n");
    assert_eq!(lines[0], "\"hello\" & 'world'", "got: {lines:#?}");
}

#[test]
fn numeric_decimal_and_hex_entities() {

    let lines = pretty_lines("a &#60;b&#x3e; c\n\n");
    assert_eq!(lines[0], "a <b> c", "got: {lines:#?}");
}

#[test]
fn full_html5_named_entities_decoded() {

    let lines = pretty_lines("&mdash; &copy; &hellip; &rarr; &times;\n\n");
    assert_eq!(lines[0], "— © … → ×", "got: {lines:#?}");
}

#[test]
fn nbsp_decodes_to_no_break_space() {
    let lines = pretty_lines("a&nbsp;b\n\n");
    assert_eq!(lines[0], "a\u{a0}b", "got: {lines:#?}");
}

#[test]
fn control_char_entities_are_not_injected() {

    for (src, literal) in [
        ("x &#27; y\n\n", "&#27;"),
        ("x &#x1b; y\n\n", "&#x1b;"),
        ("x &#7; y\n\n", "&#7;"),
        ("x &#0; y\n\n", "&#0;"),
    ] {
        let lines = pretty_lines(src);
        let joined = lines.join("\n");
        assert!(
            joined.contains(literal),
            "control entity must stay literal: src={src:?} got={lines:#?}"
        );
        assert!(
            !joined.chars().any(|c| c.is_control() && c != '\n'),
            "no control char injected: src={src:?} got={lines:#?}"
        );
    }
}

#[test]
fn entity_inside_link_text_decodes_and_keeps_link() {
    let lines = pretty_lines("See [a &lt; b](https://example.com) end.\n\n");
    let joined = lines.join("\n");
    assert!(joined.contains("a < b"), "link text decoded: {lines:#?}");
    assert!(
        !joined.contains("https://example.com"),
        "link url hidden behind the label: {lines:#?}"
    );
    assert!(!joined.contains("&lt;"), "no literal entity: {lines:#?}");
}

#[test]
fn entity_inside_inline_math_does_not_corrupt() {

    let lines = pretty_lines("eq \\(a &lt; b\\) end\n\n");
    let joined = lines.join("\n");
    assert!(joined.contains("end"), "trailing text intact: {lines:#?}");

    assert!(!joined.contains("endend"), "no double emit: {lines:#?}");
}

#[test]
fn raw_mode_preserves_entity_source() {
    let lines = raw_lines("Use &lt;tag&gt; here.\n\n");
    assert!(
        lines[0].contains("&lt;tag&gt;"),
        "raw mode must keep source: {lines:#?}"
    );
}

#[test]
fn entities_decoded_inside_emphasis_and_heading() {
    let bold = pretty_lines("**a &lt; b**\n\n");
    assert_eq!(bold[0], "a < b", "got: {bold:#?}");
    let heading = pretty_lines("## Compare &lt;T&gt;\n\n");
    assert!(
        heading.iter().any(|l| l.contains("Compare <T>")),
        "got: {heading:#?}"
    );
}

#[test]
fn entities_left_literal_in_code() {

    let inline = pretty_lines("call `vec&lt;i32&gt;` now.\n\n");
    assert!(
        inline.iter().any(|l| l.contains("vec&lt;i32&gt;")),
        "inline code stays literal: {inline:#?}"
    );
    let fenced = pretty_lines("```\nGeneric&lt;T&gt;\n```\n\n");
    assert!(
        fenced.iter().any(|l| l.contains("Generic&lt;T&gt;")),
        "code block stays literal: {fenced:#?}"
    );
}

#[test]
fn unknown_or_bare_ampersand_untouched() {

    let lines = pretty_lines("Tom &amp Jerry &unknown; plain & text\n\n");
    assert_eq!(
        lines[0], "Tom &amp Jerry &unknown; plain & text",
        "got: {lines:#?}"
    );
}

#[test]
fn entity_in_table_cell_still_decodes() {

    let lines = pretty_lines("| H |\n|---|\n| a &lt; b |\n\n");
    let joined = lines.join("\n");
    assert!(joined.contains("a < b"), "got: {lines:#?}");
}

#[test]
fn decode_html_entities_text_decodes_and_leaves_unknown_literal() {

    let got = crate::decode_html_entities_text("&lt;a&gt; &amp; it&#x27;s &#27; &bogus; & alone");
    assert_eq!(got, "<a> & it's &#27; &bogus; & alone");
}

#[test]
fn no_panic_on_entity_edge_cases() {
    for text in [
        "&\n\n",
        "&;\n\n",
        "&#;\n\n",
        "&#x;\n\n",
        "&#0;\n\n",
        "&#27;\n\n",
        "&#x1b;\n\n",
        "trailing &lt",
        "&lt;&gt;&amp;",
        "&#xZZ;\n\n",
        "&CounterClockwiseContourIntegral;\n\n",

        "& é &lt; ñ\n\n",
        "café &lt; thé\n\n",
        "🦀 & 🦀\n\n",
        "&amp;🦀&lt;\n\n",

        "&&&&&&&&&&&&\n\n",
        &("&".repeat(200) + "\n\n"),
    ] {
        let _ = pretty_lines(text);
    }
}
