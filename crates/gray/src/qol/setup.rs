use std::path::{Path, PathBuf};

use super::detect::ToolDef;
use crate::repl::TuiOpt;

struct SetupItem {
    label: String,
    desc: String,
    apply: SetupApply,
}

enum SetupApply {
    Rules { src: PathBuf },
    Skills { src_dir: PathBuf },
    Plugin { name: &'static str },
}

fn def(id: &str) -> Option<&'static ToolDef> {
    super::detect::TOOLS.iter().find(|t| t.id == id)
}

fn say(tui: &TuiOpt, msg: &str) {
    crate::repl::say(tui.as_ref().map(|(s, _)| s), msg);
}

fn agents_md_path() -> anyhow::Result<PathBuf> {
    crate::sys_prompt_path()
}

fn section_present(tool_id: &str) -> bool {
    let Ok(p) = agents_md_path() else {
        return false;
    };
    std::fs::read_to_string(p)
        .map(|b| b.contains(&format!("<!-- gray-setup:{tool_id} BEGIN -->")))
        .unwrap_or(false)
}

fn skill_entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| {
                    e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                        && !e.file_name().to_string_lossy().starts_with('.')
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

fn plugin_installed(name: &str) -> bool {
    gray_pkg::ops::list()
        .map(|l| l.contains_key(name))
        .unwrap_or(false)
}

fn clip(s: String, n: usize) -> String {
    s.chars().take(n).collect()
}

fn items_for(def: &ToolDef) -> Vec<SetupItem> {
    let mut items = Vec::new();
    let home = match gray_core::paths::user_home() {
        Some(h) => h,
        None => return items,
    };
    if let Some(rel) = def.rules {
        let src = home.join(rel);
        if src.is_file() && !section_present(def.id) {
            let body = std::fs::read_to_string(&src).unwrap_or_default();
            let lines = body.lines().count();
            let first = body
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(2)
                .collect::<Vec<_>>()
                .join(" · ");
            items.push(SetupItem {
                label: format!("global rules ({rel})"),
                desc: clip(
                    format!(
                        "{lines} lines / {} B → AGENTS.md · {first}",
                        body.len()
                    ),
                    90,
                ),
                apply: SetupApply::Rules { src },
            });
        }
    }
    if let Some(rel) = def.skills {
        let dir = home.join(rel);
        let entries = skill_entries(&dir);
        if !entries.is_empty() {
            let names: Vec<String> = entries
                .iter()
                .filter_map(|e| e.file_name().map(|n| n.to_string_lossy().to_string()))
                .collect();
            items.push(SetupItem {
                label: format!(
                    "{} skill{}",
                    entries.len(),
                    if entries.len() == 1 { "" } else { "s" }
                ),
                desc: clip(
                    format!(
                        "symlink → ~/.gray/skills · {}{}",
                        names.iter().take(4).cloned().collect::<Vec<_>>().join(", "),
                        if names.len() > 4 { ", …" } else { "" }
                    ),
                    90,
                ),
                apply: SetupApply::Skills { src_dir: dir },
            });
        }
    }
    if let Some(p) = def.plugin
        && !plugin_installed(p)
    {
        items.push(SetupItem {
            label: format!("{p} provider plugin"),
            desc: "gray plugin install + consent at first boot".to_string(),
            apply: SetupApply::Plugin { name: p },
        });
    }
    items
}

fn apply_rules(tool: &ToolDef, src: &Path) -> anyhow::Result<String> {
    let body = std::fs::read_to_string(src)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", src.display()))?;
    let path = agents_md_path()?;
    crate::load_or_create_system_prompt_at(&path)?;
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.contains(&format!("<!-- gray-setup:{} BEGIN -->", tool.id)) {
        return Ok("already present — skipped".into());
    }
    if !existing.ends_with('\n') {
        existing.push('\n');
    }
    let block = format!(
        "\n<!-- gray-setup:{} BEGIN -->\n{}\n<!-- gray-setup:{} END -->\n",
        tool.id,
        body.trim_end(),
        tool.id
    );
    existing.push_str(&block);
    std::fs::write(&path, &existing)?;
    Ok(format!("AGENTS.md +{} B (marked section)", block.len()))
}

fn apply_skills_logged(
    d: &ToolDef,
    src_dir: &Path,
    state: &mut super::QolState,
) -> anyhow::Result<String> {
    let dest_dir = crate::setup::gray_home()?.join("skills");
    std::fs::create_dir_all(&dest_dir)?;
    let mut linked = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    for entry in skill_entries(src_dir) {
        let name = entry
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let link = dest_dir.join(&name);
        if link.symlink_metadata().is_ok() {
            skipped.push(name);
            continue;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&entry, &link)?;
        #[cfg(not(unix))]
        {
            if entry.is_dir() {
                std::os::windows::fs::symlink_dir(&entry, &link)?;
            } else {
                std::os::windows::fs::symlink_file(&entry, &link)?;
            }
        }
        state.imports.push(super::ImportEntry {
            at: now_secs(),
            tool: d.id.to_string(),
            kind: "skill".into(),
            target: link.display().to_string(),
            detail: entry.display().to_string(),
        });
        linked += 1;
    }
    Ok(format!(
        "{linked} linked{}{}",
        if skipped.is_empty() { "" } else { ", skipped existing: " },
        skipped.join(", ")
    ))
}

async fn apply_plugin(name: &str) -> anyhow::Result<String> {
    let home = crate::plugin_cli::home()?;
    let lines = crate::plugin_cli::install_spec_lines(&home, name, false).await?;
    Ok(lines.join(" · "))
}

fn undo_entry(entry: &super::ImportEntry) -> String {
    match entry.kind.as_str() {
        "rules" => {
            let path = Path::new(&entry.target);
            let Ok(body) = std::fs::read_to_string(path) else {
                return "AGENTS.md missing — skipped".into();
            };
            let begin = format!("<!-- gray-setup:{} BEGIN -->", entry.detail);
            let end = format!("<!-- gray-setup:{} END -->", entry.detail);
            match (body.find(&begin), body.find(&end)) {
                (Some(b), Some(e)) if e > b => {
                    let tail = &body[e + end.len()..];
                    let head = body[..b].trim_end().to_string();
                    let merged = format!(
                        "{head}{}",
                        if tail.starts_with('\n') {
                            tail.to_string()
                        } else {
                            format!("\n{tail}")
                        }
                    );
                    match std::fs::write(path, merged) {
                        Ok(()) => "marked section removed".into(),
                        Err(e) => format!("write failed: {e}"),
                    }
                }
                _ => "marked section not found — skipped".into(),
            }
        }
        "skill" => {
            let link = Path::new(&entry.target);
            match link.symlink_metadata() {
                Ok(m) if m.file_type().is_symlink() => match std::fs::remove_file(link) {
                    Ok(()) => format!("symlink {} removed", link.display()),
                    Err(e) => format!("remove failed: {e}"),
                },
                _ => format!("{} not a symlink — left in place", link.display()),
            }
        }
        "plugin" => match gray_pkg::ops::remove(&entry.target) {
            Ok(()) => format!("plugin {} removed", entry.target),
            Err(e) => format!("remove failed: {e}"),
        },
        other => format!("unknown kind {other} — skipped"),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) async fn handle(tui: &TuiOpt, arg: Option<String>) {
    let rest = arg.unwrap_or_default().trim().to_string();
    if rest == "scanned" {
        say(tui, &super::detect::scanned_report());
        return;
    }
    if let Some(sub) = rest.strip_prefix("dismiss") {
        let id = sub.trim();
        match def(id) {
            Some(d) => {
                let mut state = super::load_state();
                state.dismissed.insert(d.id.to_string());
                match super::save_state(&state) {
                    Ok(()) => say(
                        tui,
                        &format!("{} dismissed — no more detection rows", d.name),
                    ),
                    Err(e) => say(tui, &format!("dismiss failed: {e}")),
                }
            }
            None => say(tui, &format!("unknown tool '{id}' — /setup scanned lists ids")),
        }
        return;
    }
    if let Some(sub) = rest.strip_prefix("undo") {
        let n: usize = sub.trim().parse().unwrap_or(1);
        let mut state = super::load_state();
        if state.imports.is_empty() {
            say(tui, "import log empty — nothing to undo");
            return;
        }
        let mut out = String::new();
        for _ in 0..n {
            let Some(entry) = state.imports.pop() else {
                break;
            };
            let res = undo_entry(&entry);
            out.push_str(&format!("undo {} {}: {}\n", entry.kind, entry.target, res));
        }
        let _ = super::save_state(&state);
        say(tui, &out);
        return;
    }

    if !super::enabled() {
        say(tui, "qol disabled (\"qol\": false in config.json)");
        return;
    }
    let detected = super::detect::scan();
    let tools: Vec<&ToolDef> = if rest.is_empty() {
        detected.iter().map(|d| d.def).collect()
    } else {
        match def(&rest) {
            Some(d) => vec![d],
            None => {
                say(
                    tui,
                    &format!(
                        "unknown tool '{rest}' — /setup [{}|undo|dismiss|scanned]",
                        super::detect::TOOLS
                            .iter()
                            .map(|t| t.id)
                            .collect::<Vec<_>>()
                            .join("|")
                    ),
                );
                return;
            }
        }
    };
    let mut plan: Vec<(&ToolDef, Vec<SetupItem>)> = Vec::new();
    for d in tools {
        let items = items_for(d);
        if !items.is_empty() {
            plan.push((d, items));
        }
    }
    if plan.is_empty() {
        say(
            tui,
            "nothing to import — no applicable rules/skills/plugins found (or already imported)",
        );
        return;
    }
    let Some(shared) = tui.as_ref().map(|(s, _)| s) else {
        let mut out = String::from("applicable imports:\n");
        for (d, items) in &plan {
            out.push_str(&format!("  {}:\n", d.name));
            for i in items {
                out.push_str(&format!("    - {} — {}\n", i.label, i.desc));
            }
        }
        say(tui, &out);
        return;
    };

    let questions: Vec<crate::ask::AskQuestion> = plan
        .iter()
        .map(|(d, items)| crate::ask::AskQuestion {
            id: d.id.to_string(),
            header: d.name.to_string(),
            question: format!("import from {}? (check what to apply)", d.name),
            options: items
                .iter()
                .map(|i| crate::ask::AskOption {
                    label: i.label.clone(),
                    description: i.desc.clone(),
                })
                .collect(),
            multi: true,
            default_all: true,
        })
        .collect();
    let answers = crate::ask::ask_questions_modal(shared, questions);
    let mut state = super::load_state();
    let mut out = String::new();
    for (d, items) in &plan {
        let checked: Vec<String> = answers
            .iter()
            .find(|a| a.id == d.id)
            .map(|a| a.answers.clone())
            .unwrap_or_default();
        for item in items {
            if !checked.iter().any(|c| c == &item.label) {
                out.push_str(&format!("{}: {} — skipped by choice\n", d.name, item.label));
                continue;
            }
            let res: Result<String, String> = match &item.apply {
                SetupApply::Rules { src } => match apply_rules(d, src) {
                    Ok(r) => {
                        state.imports.push(super::ImportEntry {
                            at: now_secs(),
                            tool: d.id.to_string(),
                            kind: "rules".into(),
                            target: agents_md_path()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default(),
                            detail: d.id.to_string(),
                        });
                        Ok(r)
                    }
                    Err(e) => Err(format!("{e:#}")),
                },
                SetupApply::Skills { src_dir } => {
                    apply_skills_logged(d, src_dir, &mut state).map_err(|e| format!("{e:#}"))
                }
                SetupApply::Plugin { name } => match apply_plugin(name).await {
                    Ok(r) => {
                        state.imports.push(super::ImportEntry {
                            at: now_secs(),
                            tool: d.id.to_string(),
                            kind: "plugin".into(),
                            target: name.to_string(),
                            detail: String::new(),
                        });
                        Ok(r)
                    }
                    Err(e) => Err(format!("{e:#}")),
                },
            };
            match res {
                Ok(r) => out.push_str(&format!("{}: {} — {}\n", d.name, item.label, r)),
                Err(e) => out.push_str(&format!("{}: {} — failed: {e}\n", d.name, item.label)),
            }
        }
    }
    let _ = super::save_state(&state);
    out.push_str("log: $GRAY_HOME/qol/state.json · /setup undo reverses");
    say(tui, &out);
}
