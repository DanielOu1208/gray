//! `gray cmd list` — the merged command table rendered for adapters:
//! builtins from the REGISTRY, then enabled plugins' manifest commands,
//! then skills; a name claimed by an earlier tier wins.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::repl::REGISTRY;

/// One free-form `args` option per command (contract v1): adapters expose
/// a single optional string; the hint says what it accepts. `None` = the
/// command takes no arguments.
fn args_hint(name: &str) -> Option<&'static str> {
    match name {
        "models" => Some("provider/model-id [slot=option ...] (empty opens the picker)"),
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
        "plugin" => Some("[list|update|enable|disable|check]"),
        "gateway" => Some("[status]"),
        "jobs" => Some("[stop <job|wake id>]"),
        "update" => Some("[status|install|auto on|off|status]"),
        _ => None,
    }
}

const SLOW: &[&str] = &["compact", "update", "plugin"];

fn hint_arg(hint: impl Into<String>) -> Vec<Value> {
    vec![json!({"name": "args", "required": false, "hint": hint.into()})]
}

/// The `cmd list` result body: `{"commands": [...]}` — builtins in
/// registry order, plugins by lock name, skills in discovery order.
pub(crate) fn list_json() -> Value {
    let mut commands: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for d in REGISTRY {
        let args = args_hint(d.name).map(hint_arg).unwrap_or_default();
        let mut row = json!({
            "name": d.name,
            "description": d.desc,
            "aliases": d.aliases,
            "args": args,
            "scope": d.scope,
            "surface": d.surface,
            "source": "builtin",
        });
        if SLOW.contains(&d.name) {
            row["slow"] = json!(true);
        }
        seen.insert(d.name.to_string());
        seen.extend(d.aliases.iter().map(|a| a.to_string()));
        commands.push(row);
    }
    let mut homes = Vec::new();
    if let Ok(h) = crate::setup::gray_home() {
        homes.push(h);
    }
    if let Some(main) = gray_core::paths::user_home().map(|h| h.join(".gray"))
        && !homes.contains(&main)
    {
        homes.push(main);
    }
    for home in homes {
        for (plugin, row) in plugin_commands(&home) {
            if !seen.insert(row.name.clone()) {
                log::warn!(target: "gray_cmd", "command '/{}' from plugin '{plugin}' shadows an earlier command — dropped", row.name);
                continue;
            }
            commands.push(row.json);
        }
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for s in crate::skills::discover_skills(&cwd).skills {
        if !seen.insert(s.name.clone()) {
            log::warn!(target: "gray_cmd", "skill '{}' collides with an earlier command — dropped", s.name);
            continue;
        }
        commands.push(json!({
            "name": s.name,
            "description": s.description,
            "aliases": Vec::<String>::new(),
            "args": hint_arg("[args]"),
            "scope": "conversation",
            "surface": "text",
            "source": "skill",
        }));
    }
    json!({"commands": commands})
}

struct PluginRow {
    name: String,
    json: Value,
}

fn plugin_commands(home: &Path) -> Vec<(String, PluginRow)> {
    let Ok(lock) = gray_plugin::lock::LockFile::load(&gray_plugin::lock::lock_path(home)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (plugin, entry) in &lock.plugins {
        if !crate::plugin_cli::enabled(home, plugin, entry) {
            continue;
        }
        let subs = crate::plugin_cli::declared_subcommands(home, plugin);
        let hint = if subs.is_empty() {
            "[args]".to_string()
        } else {
            format!("[{}]", subs.join("|"))
        };
        for command in crate::plugin_cli::declared_commands(home, plugin) {
            let name = command.trim().trim_start_matches('/').to_string();
            if name.is_empty() {
                continue;
            }
            out.push((
                plugin.clone(),
                PluginRow {
                    name: name.clone(),
                    json: json!({
                        "name": name,
                        "description": format!("{plugin} plugin"),
                        "aliases": Vec::<String>::new(),
                        "args": hint_arg(hint.clone()),
                        "scope": "conversation",
                        "surface": "text",
                        "source": format!("plugin:{plugin}"),
                        "slow": true,
                    }),
                },
            ));
        }
    }
    out
}
