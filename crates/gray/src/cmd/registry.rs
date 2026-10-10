//! `gray cmd list` — the slash REGISTRY rendered as the adapter's command
//! table (`{name, description, aliases, args, scope, surface}`).

use serde_json::{Value, json};

use crate::repl::REGISTRY;

/// One free-form `args` option per command (contract v1): adapters expose
/// a single optional string; the hint says what it accepts. `None` = the
/// command takes no arguments.
fn args_hint(name: &str) -> Option<&'static str> {
    match name {
        "model" => Some("provider/model-id [slot=option ...] (empty opens the picker)"),
        "sessions" => Some("[id|--last|--all]"),
        "new" => Some("[first message]"),
        "compact" => Some("[instructions]"),
        "context" => Some("[tokens|auto|status|reserve|keep]"),
        "cron" => Some("[job id or name]"),
        "skills" => Some("[name] [args]"),
        "feedback" => Some("<what happened>"),
        "theme" => Some("[name|new <name>|reload]"),
        "hotkeys" => Some("[reload]"),
        "agentsmd" => Some("[show|reset]"),
        "plugin" => Some("<list|install|remove|update|enable|disable|check>"),
        "gateway" => Some("[on|off]"),
        _ => None,
    }
}

/// The `cmd list` result body: `{"commands": [...]}`.
pub(crate) fn list_json() -> Value {
    let commands: Vec<Value> = REGISTRY
        .iter()
        .map(|d| {
            let args = args_hint(d.name)
                .map(|hint| vec![json!({"name": "args", "required": false, "hint": hint})])
                .unwrap_or_default();
            json!({
                "name": d.name,
                "description": d.desc,
                "aliases": d.aliases,
                "args": args,
                "scope": d.scope,
                "surface": d.surface,
            })
        })
        .collect();
    json!({"commands": commands})
}
