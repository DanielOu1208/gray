use crate::output::HyperlinkTarget;
use crate::style::test_style;
use crate::{StreamingMarkdownRenderer, render_markdown_ratatui_full};
use pretty_assertions::assert_eq;
use ratatui::text::Line;

fn line_to_string(line: &Line<'static>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn slice_by_cells(rendered: &str, range: std::ops::Range<usize>) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut col = 0usize;
    let mut out = String::new();
    for ch in rendered.chars() {
        let w = ch.width().unwrap_or(0);
        let end = col + w;
        if col >= range.start && end <= range.end {
            out.push(ch);
        } else if end > range.end {
            break;
        }
        col = end;
    }
    out
}

fn parser_link_text<'a>(
    out: &'a crate::output::MarkdownRenderOutput,
    expected_slice: &str,
) -> &'a HyperlinkTarget {
    out.hyperlinks
        .iter()
        .find(|h| {
            let rendered = line_to_string(&out.lines[h.line_index]);
            slice_by_cells(&rendered, h.column_range.clone()) == expected_slice
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a hyperlink whose column range covers {expected_slice:?}; got {:?}",
                out.hyperlinks,
            )
        })
}

#[test]
fn pretty_inline_link_column_range_excludes_brackets() {
    let text = "Here is a [link](https://example.com) in text.\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let h = parser_link_text(&out, "link");
    assert_eq!(h.url, "https://example.com");
    let rendered = line_to_string(&out.lines[h.line_index]);
    let slice: String = rendered
        .chars()
        .skip(h.column_range.start)
        .take(h.column_range.len())
        .collect();
    assert_eq!(
        slice, "link",
        "column_range should cover only the link text glyphs"
    );
}

#[test]
fn non_pretty_inline_link_column_range_covers_text_not_brackets() {
    let text = "[link](https://example.com)\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, false, None);

    let h = parser_link_text(&out, "link");
    let rendered = line_to_string(&out.lines[h.line_index]);
    let slice: String = rendered
        .chars()
        .skip(h.column_range.start)
        .take(h.column_range.len())
        .collect();
    assert_eq!(slice, "link");
}

#[test]
fn duplicated_link_text_on_one_line_produces_distinct_targets() {
    let text = "See [click](https://a.example) and [click](https://b.example) here.\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let click_targets: Vec<&HyperlinkTarget> = out
        .hyperlinks
        .iter()
        .filter(|h| {
            let rendered = line_to_string(&out.lines[h.line_index]);
            let slice: String = rendered
                .chars()
                .skip(h.column_range.start)
                .take(h.column_range.len())
                .collect();
            slice == "click"
        })
        .collect();
    assert_eq!(
        click_targets.len(),
        2,
        "two parser-produced link-text hyperlinks expected",
    );
    let urls: Vec<&str> = click_targets.iter().map(|h| h.url.as_str()).collect();
    assert_eq!(urls, vec!["https://a.example", "https://b.example"]);

    let h0 = click_targets[0];
    let h1 = click_targets[1];
    assert_eq!(
        h0.line_index, h1.line_index,
        "both links should be on the same rendered line"
    );
    assert_ne!(h0.id, h1.id, "ids must differ");
    assert!(
        h0.column_range.end <= h1.column_range.start,
        "column ranges must be disjoint and in order, got {:?} vs {:?}",
        h0.column_range,
        h1.column_range,
    );
}

#[test]
fn cjk_link_uses_display_width_for_column_range() {
    let text = "[日本語](https://example.com)\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let h = parser_link_text(&out, "日本語");
    assert_eq!(
        h.column_range.len(),
        6,
        "三 wide CJK chars -> 6 display cells"
    );
}

#[test]
fn autolink_emits_grouped_targets_for_same_logical_link() {
    let text = "Visit <https://example.com> for info.\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    assert!(
        !out.hyperlinks.is_empty(),
        "expected at least one hyperlink"
    );
    let url = &out.hyperlinks[0].url;
    let id = out.hyperlinks[0].id;
    let line_index = out.hyperlinks[0].line_index;
    for h in &out.hyperlinks {
        assert_eq!(&h.url, url, "all autolink fragments share the same URL");
        assert_eq!(h.id, id, "all autolink fragments share the same id");
        assert_eq!(
            h.line_index, line_index,
            "autolink stays on one rendered line"
        );
    }
    assert_eq!(url, "https://example.com");

    let rendered = line_to_string(&out.lines[line_index]);
    let mut covered: Vec<bool> = vec![false; rendered.chars().count()];
    for h in &out.hyperlinks {
        for col in h.column_range.clone() {
            if col < covered.len() {
                covered[col] = true;
            }
        }
    }
    let covered_text: String = rendered
        .chars()
        .zip(covered.iter())
        .filter_map(|(c, &k)| if k { Some(c) } else { None })
        .collect();
    assert!(
        covered_text.contains("https://example.com"),
        "combined column ranges must cover the rendered URL; got covered={:?} on rendered={:?}",
        covered_text,
        rendered,
    );
}

#[test]
fn two_links_with_prose_between_have_correct_columns() {
    let text = "Pre [a](https://a.example) mid [b](https://b.example) post.\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let h0 = parser_link_text(&out, "a");
    let h1 = parser_link_text(&out, "b");
    assert_eq!(h0.line_index, h1.line_index);
}

#[test]
fn streaming_byte_by_byte_matches_full_render() {
    let text =
        "# Header\n\nSee [docs](https://example.com/docs) and [api](https://example.com/api).\n\n";
    let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
    for byte in text.as_bytes() {

        let buf = [*byte];
        let s = std::str::from_utf8(&buf).expect("ascii test input");
        renderer.push_and_render(s, None);
    }
    renderer.finish(None);
    let view = renderer.view();

    let extract = |hs: &[HyperlinkTarget]| -> Vec<(String, usize, std::ops::Range<usize>)> {
        let mut v: Vec<_> = hs
            .iter()
            .map(|h| (h.url.clone(), h.line_index, h.column_range.clone()))
            .collect();
        v.sort_by_key(|a| (a.1, a.2.start));
        v
    };
    assert_eq!(
        extract(&full.hyperlinks),
        extract(&view.hyperlinks),
        "full-render and streaming+finish must produce the same hyperlinks",
    );
}

#[test]
fn streaming_link_across_chunk_boundaries_resolves_correctly() {
    let part1 = "Para one.\n\nSee [my ";
    let part2 = "link](https://example.com) here.\n\n";
    let full_text = format!("{part1}{part2}");

    let (full, _) = render_markdown_ratatui_full(&full_text, test_style::STYLE, true, None);

    assert_eq!(full.hyperlinks.len(), 1);
    let expected = parser_link_text(&full, "my link");

    let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
    renderer.push_and_render(part1, None);
    renderer.push_and_render(part2, None);
    renderer.finish(None);
    let view = renderer.view();

    assert_eq!(
        view.hyperlinks.len(),
        1,
        "expected 1 hyperlink (parser link text; URL hidden)"
    );
    let got = view
        .hyperlinks
        .iter()
        .find(|h| h.column_range == expected.column_range && h.line_index == expected.line_index)
        .expect("parser-produced hyperlink should be present after finish()");
    assert_eq!(got.url, expected.url);
    assert_eq!(got.line_index, expected.line_index);
    assert_eq!(got.column_range, expected.column_range);
}

#[test]
fn dest_url_containing_bracket_paren_with_streaming_split() {
    let text = "[t](<u](v>) end\n";
    let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
    let line0 = line_to_string(&full.lines[0]);
    assert_eq!(line0.trim(), "t end");
    let link = full
        .hyperlinks
        .iter()
        .find(|h| h.url.contains("u](v"))
        .expect("link");
    let slice: String = line0
        .chars()
        .skip(link.column_range.start)
        .take(link.column_range.len())
        .collect();
    assert_eq!(slice, "t");
    let mut r = StreamingMarkdownRenderer::new(test_style::STYLE, true);
    r.push_and_render("[t](<u]", None);
    r.push_and_render("(v>) end\n", None);
    r.finish(None);
    let view = r.view();
    let view_link = view
        .hyperlinks
        .iter()
        .find(|h| h.url.contains("u](v"))
        .expect("view");
    assert_eq!(view_link.url, link.url);
    assert_eq!(view_link.column_range, link.column_range);
}

#[test]
fn nested_image_in_link_finds_outer_closer() {
    let text = "[![badge](https://img.shields.io/v1.svg)](https://github.com/repo) end\n";
    let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let link = full
        .hyperlinks
        .iter()
        .find(|h| h.url.contains("github.com/repo"))
        .expect("outer link should target repo URL");

    assert!(
        !link.url.contains("shields.io"),
        "should not pick the inner image URL"
    );
}

#[test]
fn link_inside_table_cell_emits_hyperlink_and_styling() {
    let text = "\
| Name | Link |
|------|------|
| Foo  | [click](https:
";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);

    let link = out
        .hyperlinks
        .iter()
        .find(|h| h.url == "https://example.com")
        .expect("table cell link should produce a HyperlinkTarget");

    let rendered = line_to_string(&out.lines[link.line_index]);
    let slice: String = rendered
        .chars()
        .skip(link.column_range.start)
        .take(link.column_range.len())
        .collect();
    assert_eq!(
        slice, "click",
        "column_range should cover only the link text glyphs in the cell, \
             got slice={slice:?} from rendered={rendered:?}"
    );

    let cell_line = &out.lines[link.line_index];
    let click_span = cell_line
        .spans
        .iter()
        .find(|s| s.content.as_ref() == "click")
        .expect("expected a span containing exactly the link text");
    assert!(
        click_span
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD),
        "link text span inside the cell should carry link_text styling \
             (bold in test_style), got style={:?}",
        click_span.style,
    );
}

#[test]
fn paragraph_link_keeps_link_text_fg_over_default_text_fg() {
    use crate::MarkdownStyle;
    use anstyle::{AnsiColor, Color, Style as AStyle};

    let style = MarkdownStyle {
        text: AStyle::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
        link_text: AStyle::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Blue)))
            .underline(),
        ..test_style::STYLE
    };

    let text = "Hello [click](https://x.com) world.\n";
    let (out, _) = render_markdown_ratatui_full(text, style, true, None);

    let line = &out.lines[0];
    let click_span = line
        .spans
        .iter()
        .find(|s| s.content == "click")
        .expect("expected a span containing the link text");
    let ok_fg = click_span.style.fg == Some(ratatui::style::Color::Blue)
        || click_span.style.fg == Some(ratatui::style::Color::Rgb(125, 207, 255));
    assert!(
        ok_fg,
        "link text fg must be link_text's blue (or gray Rgb), not ms.text's red; got {:?}",
        click_span.style,
    );
    assert!(
        click_span
            .style
            .add_modifier
            .contains(ratatui::style::Modifier::UNDERLINED),
        "link text must remain underlined; got {:?}",
        click_span.style,
    );
}

#[test]
fn formatted_link_keeps_link_fg_and_gains_effects() {
    use crate::MarkdownStyle;
    use anstyle::{AnsiColor, Color, Style as AStyle};
    use ratatui::style::Modifier;

    let style = MarkdownStyle {
        text: AStyle::new().fg_color(Some(Color::Ansi(AnsiColor::Red))),
        strong_inner: AStyle::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Red)))
            .bold(),
        emphasis_inner: AStyle::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Red)))
            .italic(),
        strikethrough_inner: AStyle::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Red)))
            .strikethrough(),
        link_text: AStyle::new()
            .fg_color(Some(Color::Ansi(AnsiColor::Blue)))
            .underline(),
        ..test_style::STYLE
    };

    for (md, effect) in [
        ("**[click](https://x.com)** end.\n", Modifier::BOLD),
        ("[**click**](https://x.com) end.\n", Modifier::BOLD),
        ("*[click](https://x.com)* end.\n", Modifier::ITALIC),
        ("~~[click](https://x.com)~~ end.\n", Modifier::CROSSED_OUT),
    ] {
        let (out, _) = render_markdown_ratatui_full(md, style, true, None);
        let line = &out.lines[0];
        let click_span = line
            .spans
            .iter()
            .find(|s| s.content == "click")
            .unwrap_or_else(|| panic!("expected a span for the link text in {md:?}"));
        let ok_fg = click_span.style.fg == Some(ratatui::style::Color::Blue)
            || click_span.style.fg == Some(ratatui::style::Color::Rgb(125, 207, 255));
        assert!(
            ok_fg,
            "in {md:?} the link text fg must stay link_text's blue (or gray Rgb), \
                 not strong/emphasis_inner's red; got {:?}",
            click_span.style,
        );
        assert!(
            click_span.style.add_modifier.contains(effect),
            "in {md:?} the link text must gain {effect:?}; got {:?}",
            click_span.style,
        );
        assert!(
            click_span.style.add_modifier.contains(Modifier::UNDERLINED),
            "in {md:?} the link text must remain underlined; got {:?}",
            click_span.style,
        );
    }
}

#[test]
fn soft_break_inside_link_text_preserves_column_range() {
    let md = "foo [link\ntext](https://example.com) bar";
    let (out, _) = render_markdown_ratatui_full(md, test_style::STYLE, true, None);

    assert_eq!(out.lines.len(), 1, "{:?}", out.lines);

    let parser_id = out
        .hyperlinks
        .iter()
        .find(|h| h.url == "https://example.com")
        .expect("at least one hyperlink target")
        .id;
    let mut fragments: Vec<&HyperlinkTarget> = out
        .hyperlinks
        .iter()
        .filter(|h| h.id == parser_id && h.url == "https://example.com")
        .collect();
    fragments.sort_by_key(|h| h.column_range.start);

    for f in &fragments {
        assert_eq!(f.line_index, 0, "{f:?}");
    }
    for w in fragments.windows(2) {
        assert_eq!(
            w[0].column_range.end, w[1].column_range.start,
            "gap: {:?} -> {:?}",
            w[0].column_range, w[1].column_range,
        );
    }

    let union_start = fragments.first().unwrap().column_range.start;
    let union_end = fragments.last().unwrap().column_range.end;
    let rendered = line_to_string(&out.lines[0]);
    assert_eq!(
        slice_by_cells(&rendered, union_start..union_end),
        "link text",
        "rendered={rendered:?} union={union_start}..{union_end}",
    );
    assert_eq!(
        union_end - union_start,
        crate::buffers::unicode_display_width("link text"),
    );
}

#[test]
fn file_link_displays_only_underlined_label_with_original_target() {
    let text = "Open [the file](file:///tmp/project%20space/report.md) please.\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
    let visible = out
        .lines
        .iter()
        .map(line_to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(visible.trim(), "Open the file please.");
    let link = parser_link_text(&out, "the file");
    assert_eq!(link.url, "file:///tmp/project%20space/report.md");
    assert!(out.lines[link.line_index].spans.iter().any(|s| {
        s.content.contains("the file")
            && s.style
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED)
    }));
}

#[test]
fn file_links_stream_like_full_render_and_leave_code_literal() {
    let text = "See [first](file:///tmp/first.md) and [second](file:///tmp/second.md).\n\n`file:///tmp/literal`\n\n";
    let (full, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
    let mut renderer = StreamingMarkdownRenderer::new(test_style::STYLE, true);
    for ch in text.chars() {
        renderer.push_and_render(&ch.to_string(), None);
    }
    renderer.finish(None);
    let visible = |lines: &[Line<'static>]| lines.iter().map(line_to_string).collect::<Vec<_>>();
    assert_eq!(visible(&renderer.view().lines), visible(&full.lines));
    assert!(
        visible(&full.lines)
            .join("\n")
            .contains("See first and second.")
    );
    assert!(
        visible(&full.lines)
            .join("\n")
            .contains("file:///tmp/literal")
    );
    for out in [&full, renderer.view()] {
        assert_eq!(parser_link_text(out, "first").url, "file:///tmp/first.md");
        assert_eq!(parser_link_text(out, "second").url, "file:///tmp/second.md");
    }
}

#[test]
fn link_destinations_and_titles_are_hidden() {
    let text =
        "[report](file:///tmp/report.md \"private title\") and [web](https://example.com).\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
    let visible = out
        .lines
        .iter()
        .map(line_to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(visible.trim(), "report and web.");
    assert_eq!(
        parser_link_text(&out, "report").url,
        "file:///tmp/report.md"
    );
    assert_eq!(parser_link_text(&out, "web").url, "https://example.com");
}

#[test]
fn image_keeps_visible_destination() {
    let text = "see ![logo](https://example.com/logo.png) here\n";
    let (out, _) = render_markdown_ratatui_full(text, test_style::STYLE, true, None);
    let visible = line_to_string(&out.lines[0]);
    assert!(
        visible.contains("https://example.com/logo.png"),
        "image URL stays visible: {visible:?}"
    );
}
