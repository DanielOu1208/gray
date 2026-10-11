//! `gray.ui` v1 documents — the vendored subset channel adapters compile
//! (GRAY-CMD-CONTRACT.md). The wire shape is frozen: `type`-tagged
//! snake_case nodes; the doc body drops into the discord plugin's
//! `component_compile` unchanged.

use serde::{Deserialize, Serialize};

/// The document adapters compile: `protocol`/`version` identify the wire,
/// `surface` is always `"message"` in v1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct UiDoc {
    pub(crate) protocol: String,
    pub(crate) version: u32,
    pub(crate) surface: String,
    pub(crate) document_id: String,
    pub(crate) visibility: Visibility,
    pub(crate) lifecycle: Lifecycle,
    pub(crate) components: Vec<MessageNode>,
}

impl UiDoc {
    /// A message-surface doc with the v1 constants filled in.
    pub(crate) fn message(
        document_id: &str,
        visibility: Visibility,
        lifecycle: Lifecycle,
        components: Vec<MessageNode>,
    ) -> Self {
        Self {
            protocol: "gray.ui".to_string(),
            version: 1,
            surface: "message".to_string(),
            document_id: document_id.to_string(),
            visibility,
            lifecycle,
            components,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Visibility {
    Ephemeral,
    Public,
}

/// Core-enforced lifecycle: `one_shot` consumes the token on first
/// interact, `editable` revises against the pending record until a submit
/// consumes it. (`repeatable`/`static` exist on the wire; unused in v1.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Lifecycle {
    OneShot,
    Editable,
    Repeatable,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ButtonStyle {
    Primary,
    Secondary,
    Success,
    Danger,
    Link,
}

/// One component in a [`UiDoc`] — the contract's v1 subset.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum MessageNode {
    Text {
        content: String,
    },
    Separator,
    ActionRow {
        children: Vec<MessageNode>,
    },
    StringSelect {
        logical_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        options: Vec<SelectOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min_values: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_values: Option<u32>,
    },
    Button {
        logical_id: String,
        label: String,
        style: ButtonStyle,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        disabled: bool,
    },
}

/// A `string_select` option (`default` only appears when set).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SelectOption {
    pub(crate) label: String,
    pub(crate) value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) default: bool,
}
