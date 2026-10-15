use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => write!(f, "system"),
            Self::User => write!(f, "user"),
            Self::Assistant => write!(f, "assistant"),
        }
    }
}

/// Opaque Gemini tool-call state, replayable only to the model that produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ThoughtSignature {
    pub signature: String,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },

    Image {
        media_type: String,
        data: String,
    },

    #[serde(alias = "video")]
    Media {
        media_type: String,
        data: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fallback: Vec<ContentBlock>,
    },

    ToolUse {
        id: String,
        name: String,
        args: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thought_signature: Option<ThoughtSignature>,
    },

    ToolResult {
        id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },

    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encrypted_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        item_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },

    StructuredInput {
        protocol: String,
        version: u32,
        kind: String,
        payload: serde_json::Value,
    },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn tool_use(
        id: impl Into<String>,
        name: impl Into<String>,
        args: serde_json::Value,
    ) -> Self {
        Self::ToolUse {
            id: id.into(),
            name: name.into(),
            args,
            thought_signature: None,
        }
    }

    pub fn thinking(text: impl Into<String>) -> Self {
        Self::Thinking {
            text: text.into(),
            encrypted_content: None,
            item_id: None,
            model: None,
        }
    }

    pub fn image(media_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self::Image {
            media_type: media_type.into(),
            data: data.into(),
        }
    }

    pub fn media(
        media_type: impl Into<String>,
        data: impl Into<String>,
        fallback: Vec<ContentBlock>,
    ) -> Self {
        Self::Media {
            media_type: media_type.into(),
            data: data.into(),
            fallback,
        }
    }

    pub fn tool_result(id: impl Into<String>, content: impl Into<String>, is_error: bool) -> Self {
        Self::ToolResult {
            id: id.into(),
            content: content.into(),
            is_error,
        }
    }

    pub fn provider_text(&self) -> Option<String> {
        match self {
            ContentBlock::Text { text } => Some(text.clone()),
            ContentBlock::StructuredInput {
                protocol,
                version,
                kind,
                payload,
            } => {
                let protocol_json =
                    serde_json::to_string(protocol).unwrap_or_else(|_| "\"unknown\"".into());
                let kind_json =
                    serde_json::to_string(kind).unwrap_or_else(|_| "\"unknown\"".into());
                let payload_json = canonical_json(payload);
                let body = format!(
                    "<gray_structured_input protocol={} version={} kind={}>\n{}\n</gray_structured_input>",
                    protocol_json, version, kind_json, payload_json
                );
                Some(body.chars().take(65_536).collect())
            }
            _ => None,
        }
    }
}

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(values) => {
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort();
            let body = keys
                .into_iter()
                .map(|key| {
                    let encoded_key = serde_json::to_string(key).unwrap_or_else(|_| "\"\"".into());
                    let encoded_value = canonical_json(&values[key]);
                    format!("{encoded_key}:{encoded_value}")
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{body}}}")
        }
        serde_json::Value::Array(values) => {
            let body = values
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{body}]")
        }
        other => other.to_string(),
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,

    #[serde(default, skip_serializing_if = "is_false")]
    pub injected: bool,
}

impl Message {
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self {
            role,
            content,
            injected: false,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            injected: false,
        }
    }

    pub fn user_injected(text: impl Into<String>) -> Self {
        Self {
            injected: true,
            ..Self::user(text)
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            injected: false,
        }
    }

    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            injected: false,
        }
    }

    pub fn structured_input(input: crate::input::InputEnvelope) -> Self {
        Self {
            role: Role::User,
            content: vec![input.into_content_block()],
            injected: false,
        }
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn context_text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            let piece = match block {
                ContentBlock::Text { text } => text.clone(),
                ContentBlock::StructuredInput { .. } => block
                    .provider_text()
                    .unwrap_or_else(|| "<gray_structured_input unavailable>".to_string()),
                ContentBlock::ToolResult { content, .. } => content.clone(),
                ContentBlock::ToolUse { name, args, .. } => {
                    format!("{name}{args}")
                }
                ContentBlock::Thinking {
                    text,
                    encrypted_content,
                    ..
                } => match encrypted_content {
                    Some(blob) => format!("{text}{blob}"),
                    None => text.clone(),
                },

                ContentBlock::Image { data, .. } | ContentBlock::Media { data, .. } => data.clone(),
            };
            if !piece.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&piece);
            }
        }
        out
    }
}

pub const MAX_NATIVE_MEDIA_BYTES: usize = 8 * 1024 * 1024;

pub fn resolve_media(
    blocks: Vec<ContentBlock>,
    accepts: impl Fn(&str) -> bool,
) -> Vec<ContentBlock> {
    let max_b64 = MAX_NATIVE_MEDIA_BYTES.div_ceil(3) * 4;
    let mut out = Vec::with_capacity(blocks.len());
    for block in blocks {
        match block {
            ContentBlock::Media {
                media_type,
                data,
                fallback,
            } if !accepts(&media_type) || data.len() > max_b64 => {
                if fallback.is_empty() {
                    out.push(ContentBlock::text(format!(
                        "({media_type} attachment omitted: this model can't take it)"
                    )));
                } else {
                    out.extend(fallback);
                }
            }
            other => out.push(other),
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl ToolDef {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            label: None,
            preview: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}
