use gray_markdown::decode_html_entities_text;

pub(super) fn web_body_text(tool_name: &str, trimmed: &str) -> Option<String> {
    match tool_name {
        "web_search" => search_body_text(trimmed),
        "web_fetch" => Some(fetch_body_text(trimmed)),
        _ => None,
    }
}

fn search_body_text(trimmed: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let results = v.get("results")?.as_array()?;
    if results.is_empty() {
        return Some("no results".to_string());
    }

    let mut lines = Vec::new();
    for result in results {
        for (key, indent) in [("title", ""), ("url", "  "), ("snippet", "  ")] {
            let Some(field) = result.get(key).and_then(|f| f.as_str()) else {
                continue;
            };
            let field = decode_html_entities_text(field);
            let field = field.trim();
            if field.is_empty() {
                continue;
            }
            lines.push(format!("{indent}{field}"));
        }
    }
    Some(lines.join("\n"))
}

fn fetch_body_text(trimmed: &str) -> String {

    const MAX_CHARS: usize = 2000;
    let decoded = decode_html_entities_text(trimmed);
    if decoded.chars().count() <= MAX_CHARS {
        return decoded;
    }
    let mut cut = decoded
        .char_indices()
        .nth(MAX_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(decoded.len());

    if let Some((ws, _)) = decoded[..cut]
        .char_indices()
        .rev()
        .take(200)
        .find(|(_, c)| c.is_whitespace())
    {
        cut = ws;
    }
    let remaining = decoded[cut..].chars().count();
    format!("{}\n… +{remaining} chars", decoded[..cut].trim_end())
}
