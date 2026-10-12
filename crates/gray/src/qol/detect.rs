use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub(crate) struct ToolDef {
    pub id: &'static str,
    pub name: &'static str,
    pub dirs: &'static [&'static str],
    pub files: &'static [&'static str],
    pub bins: &'static [&'static str],
    pub rules: Option<&'static str>,
    pub skills: Option<&'static str>,
    pub plugin: Option<&'static str>,
}

pub(crate) const TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "claude",
        name: "Claude Code",
        dirs: &[".claude"],
        files: &[".claude.json"],
        bins: &["claude"],
        rules: Some(".claude/CLAUDE.md"),
        skills: Some(".claude/skills"),
        plugin: Some("claude-sub"),
    },
    ToolDef {
        id: "codex",
        name: "Codex",
        dirs: &[".codex"],
        files: &[],
        bins: &["codex"],
        rules: Some(".codex/AGENTS.md"),
        skills: Some(".codex/skills"),
        plugin: Some("codex-sub"),
    },
    ToolDef {
        id: "opencode",
        name: "opencode",
        dirs: &[".config/opencode"],
        files: &[],
        bins: &["opencode"],
        rules: Some(".config/opencode/AGENTS.md"),
        skills: Some(".config/opencode/skills"),
        plugin: Some("opencode-sub"),
    },
    ToolDef {
        id: "devin",
        name: "Devin",
        dirs: &[".config/devin", ".local/share/devin"],
        files: &[],
        bins: &["devin"],
        rules: None,
        skills: Some(".config/devin/skills"),
        plugin: Some("devin-sub"),
    },
    ToolDef {
        id: "cursor",
        name: "Cursor",
        dirs: &[".cursor"],
        files: &[],
        bins: &["cursor-agent"],
        rules: None,
        skills: Some(".cursor/skills"),
        plugin: None,
    },
    ToolDef {
        id: "windsurf",
        name: "Windsurf",
        dirs: &[".windsurf"],
        files: &[],
        bins: &[],
        rules: None,
        skills: None,
        plugin: None,
    },
    ToolDef {
        id: "aider",
        name: "aider",
        dirs: &[],
        files: &[],
        bins: &["aider"],
        rules: None,
        skills: None,
        plugin: None,
    },
];

pub(crate) struct Detection {
    pub def: &'static ToolDef,
    pub fp: u64,
}

fn home() -> Option<PathBuf> {
    gray_core::paths::user_home()
}

fn on_path(bin: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    for dir in std::env::split_paths(&paths) {
        if dir.join(bin).is_file() {
            return true;
        }
    }
    false
}

pub(crate) fn scan() -> Vec<Detection> {
    let mut out = Vec::new();
    let home = home();
    for def in TOOLS {
        let mut paths = Vec::new();
        if let Some(h) = &home {
            for rel in def.dirs.iter().chain(def.files.iter()) {
                let p = h.join(rel);
                if p.symlink_metadata().is_ok() {
                    paths.push(p);
                }
            }
        }
        let bins: Vec<String> = def
            .bins
            .iter()
            .filter(|b| on_path(b))
            .map(|b| b.to_string())
            .collect();
        if paths.is_empty() && bins.is_empty() {
            continue;
        }
        let mut h = DefaultHasher::new();
        for p in &paths {
            p.hash(&mut h);
            if let Ok(m) = p.symlink_metadata() {
                m.len().hash(&mut h);
            }
        }
        bins.hash(&mut h);
        out.push(Detection {
            def,
            fp: h.finish(),
        });
    }
    out
}

fn notice_line(names: &[&str]) -> String {
    if names.len() == 1 {
        let id = TOOLS
            .iter()
            .find(|t| t.name == names[0])
            .map(|t| t.id)
            .unwrap_or("setup");
        return format!("{} detected · /setup {id}", names[0]);
    }
    format!("{} detected · /setup", names.join(", "))
}

pub(crate) async fn run_for_home(tui: crate::composer::SharedTui) {
    if !super::enabled() {
        return;
    }
    let detected = scan();
    if detected.is_empty() {
        return;
    }
    let mut state = super::load_state();
    let fresh: Vec<&Detection> = detected
        .iter()
        .filter(|d| !state.tools.contains_key(d.def.id) && !state.dismissed.contains(d.def.id))
        .collect();
    if fresh.is_empty() {
        return;
    }
    let names: Vec<&str> = fresh.iter().map(|d| d.def.name).collect();
    let line = notice_line(&names);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    for d in &fresh {
        state.tools.insert(
            d.def.id.to_string(),
            super::ToolStamp {
                fp: d.fp,
                seen_at: now,
            },
        );
    }
    let _ = super::save_state(&state);
    if let Ok(mut t) = tui.lock() {
        if t.is_home() {
            t.set_home_qol(Some(line));
        } else {
            t.push_dim(line);
            t.ensure_gap();
        }
    }
}


pub(crate) fn scanned_report() -> String {
    let mut out = String::from("declared scan set:\n");
    let home = home().unwrap_or_else(|| PathBuf::from("~"));
    for def in TOOLS {
        out.push_str(&format!("  {} ({})\n", def.name, def.id));
        for rel in def.dirs.iter().chain(def.files.iter()) {
            let p = home.join(rel);
            let found = p.symlink_metadata().is_ok();
            let suffix = if *rel == ".claude.json" && found {
                let keys = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|b| serde_json::from_str::<serde_json::Value>(&b).ok())
                    .and_then(|v| {
                        v.as_object()
                            .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    })
                    .unwrap_or_default();
                format!(
                    "  (keys: {})",
                    if keys.is_empty() { "?".to_string() } else { keys.join(", ") }
                )
            } else {
                String::new()
            };
            out.push_str(&format!(
                "    {} {}{}\n",
                if found { "+" } else { "-" },
                rel,
                suffix
            ));
        }
        for b in def.bins {
            out.push_str(&format!(
                "    {} {} on PATH\n",
                if on_path(b) { "+" } else { "-" },
                b
            ));
        }
    }
    let state = super::load_state();
    if !state.dismissed.is_empty() {
        out.push_str(&format!(
            "dismissed: {}\n",
            state.dismissed.iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }
    out
}
