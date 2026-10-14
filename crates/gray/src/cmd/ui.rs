use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SelectOption {
    pub(crate) label: String,
    pub(crate) value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) description: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) default: bool,
}
