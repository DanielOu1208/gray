use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;

pub(crate) struct ToolDef {
    pub id: &'static str,
    pub name: &'static str,
    pub dirs: &'static [&'static str],
    pub files: &'static [&'static str],
    pub bins: &'static [&'static str],
}

pub(crate) const TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "claude",
        name: "Claude Code",
        dirs: &[".claude"],
        files: &[".claude.json"],
        bins: &["claude"],
    },
    ToolDef {
        id: "codex",
        name: "Codex",
        dirs: &[".codex"],
        files: &[],
        bins: &["codex"],
    },
    ToolDef {
        id: "opencode",
        name: "opencode",
        dirs: &[".config/opencode"],
        files: &[],
        bins: &["opencode"],
    },
    ToolDef {
        id: "devin",
        name: "Devin",
        dirs: &[".config/devin", ".local/share/devin"],
        files: &[],
        bins: &["devin"],
    },
    ToolDef {
        id: "cursor",
        name: "Cursor",
        dirs: &[".cursor"],
        files: &[],
        bins: &["cursor-agent"],
    },
    ToolDef {
        id: "windsurf",
        name: "Windsurf",
        dirs: &[".windsurf"],
        files: &[],
        bins: &[],
    },
    ToolDef {
        id: "aider",
        name: "aider",
        dirs: &[],
        files: &[],
        bins: &["aider"],
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

