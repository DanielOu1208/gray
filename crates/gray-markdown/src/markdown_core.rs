use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::ops::Range;

fn parser_options() -> Options {
    Options::ENABLE_GFM
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_MATH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
}

pub fn offset_events(text: &str) -> impl Iterator<Item = (Event<'_>, Range<usize>)> + '_ {
    DoubleTildeOnlyStrike {
        text,
        events: Parser::new_ext(text, parser_options()).into_offset_iter(),
    }
}

struct DoubleTildeOnlyStrike<'a, I> {
    text: &'a str,
    events: I,
}

fn is_double_tilde_strike(text: &str, range: &Range<usize>) -> bool {
    text.get(range.start..).is_some_and(|s| s.starts_with("~~"))
}

fn strike_delim_text<'a>(
    text: &'a str,
    range: &Range<usize>,
    opening: bool,
) -> (Event<'a>, Range<usize>) {
    let delim = if opening {
        let end = range.start + 1;
        debug_assert!(text.is_char_boundary(end) && end <= text.len());
        (range.start..end, &text[range.start..end])
    } else {
        let start = range.end - 1;
        debug_assert!(text.is_char_boundary(start) && start < text.len());
        (start..range.end, &text[start..range.end])
    };
    (Event::Text(delim.1.into()), delim.0)
}

impl<'a, I> Iterator for DoubleTildeOnlyStrike<'a, I>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    type Item = (Event<'a>, Range<usize>);

    fn next(&mut self) -> Option<Self::Item> {
        let (event, range) = self.events.next()?;
        match &event {
            Event::Start(Tag::Strikethrough) if !is_double_tilde_strike(self.text, &range) => {
                Some(strike_delim_text(self.text, &range, true))
            }
            Event::End(TagEnd::Strikethrough) if !is_double_tilde_strike(self.text, &range) => {
                Some(strike_delim_text(self.text, &range, false))
            }
            _ => Some((event, range)),
        }
    }
}
