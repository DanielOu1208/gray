use crate::agent::Agent;
use crate::message::{ContentBlock, Message, Role};

pub(crate) fn push_synthetic(agent: &mut Agent, id: &str, reason: &str) {
    agent.messages.push(Message {
        role: Role::User,
        injected: false,
        content: vec![ContentBlock::ToolResult {
            id: id.to_string(),
            content: format!("[{reason}]"),
            is_error: true,
        }],
    });
}

pub(crate) fn answer_pending_tools(
    agent: &mut Agent,
    tool_uses: &[(String, String, serde_json::Value)],
    from_idx: usize,
    reason: &str,
) {
    for (id, _, _) in tool_uses.iter().skip(from_idx) {
        push_synthetic(agent, id, reason);
    }
}

pub(crate) fn answer_pending_range(
    agent: &mut Agent,
    tool_uses: &[(String, String, serde_json::Value)],
    from_idx: usize,
    to_idx: usize,
    reason: &str,
) {
    for (id, _, _) in tool_uses
        .iter()
        .skip(from_idx)
        .take(to_idx.saturating_sub(from_idx))
    {
        push_synthetic(agent, id, reason);
    }
}

#[derive(Default)]
pub(crate) struct PendingToolCall {
    pub(crate) id: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) arguments: String,
    pub(crate) thought_signature: Option<String>,

    pub(crate) started: bool,
}

impl PendingToolCall {
    pub(crate) fn parsed_args(&self) -> serde_json::Value {
        if self.arguments.is_empty() {
            return serde_json::Value::Null;
        }
        let parsed = serde_json::from_str(&self.arguments)
            .or_else(|_| serde_json::from_str(&escape_raw_controls(&self.arguments)));
        match parsed {
            Ok(serde_json::Value::String(inner)) => serde_json::from_str(&inner)
                .ok()
                .filter(serde_json::Value::is_object)
                .unwrap_or(serde_json::Value::String(inner)),
            Ok(v) => v,
            Err(_) => serde_json::Value::String(self.arguments.clone()),
        }
    }
}

fn escape_raw_controls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut escaped) = (false, false);
    for c in s.chars() {
        if in_str && !escaped && (c as u32) < 0x20 {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                _ => out.push_str(&format!("\\u{:04x}", c as u32)),
            }
            continue;
        }
        out.push(c);
        if escaped {
            escaped = false;
        } else if in_str && c == '\\' {
            escaped = true;
        } else if c == '"' {
            in_str = !in_str;
        }
    }
    out
}

#[cfg(test)]
mod parse_tests {
    use super::PendingToolCall;

    fn parse(raw: &str) -> serde_json::Value {
        PendingToolCall {
            arguments: raw.into(),
            ..Default::default()
        }
        .parsed_args()
    }

    #[test]
    fn raw_newlines_in_strings_are_repaired() {
        let v = parse("{\"command\": \"printf 'a\\\\n'\nls\tx\"}");
        assert_eq!(v["command"], "printf 'a\\n'\nls\tx");
    }

    #[test]
    fn double_encoded_object_is_unwrapped() {
        let v = parse(r#""{\"command\":\"ls\"}""#);
        assert_eq!(v["command"], "ls");
    }

    #[test]
    fn garbage_stays_a_string() {
        assert!(parse("{\"command\": ").is_string());
    }
}
