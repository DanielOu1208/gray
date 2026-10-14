use std::path::{Path, PathBuf};
use std::time::SystemTime;

use gray_core::message::{ContentBlock, Role};
use serde_json::{Value, json};

use crate::session_store::{JsonlSessionStore, SessionId};

const PREVIEW_CHARS: usize = 160;

fn usage(msg: String) -> ! {
    eprintln!("gray read: {msg}");
    std::process::exit(2)
}

pub async fn run_cli(
    session: Option<&str>,
    live: bool,
    last: usize,
    json: bool,
) -> anyhow::Result<()> {
    let root = crate::session_send::owner_home()?.join("sessions");
    match (session, live) {
        (Some(id), false) => read_session(&root, id, last, json).await,
        (None, _) | (_, true) => list_live(&root, json).await,
    }
}

struct Live {
    id: String,
    pid: Option<u32>,
    cwd: String,
    model: String,
    last_activity: Option<SystemTime>,
}

pub async fn live_sessions(root: &Path) -> std::io::Result<Vec<(String, Option<u32>)>> {
    let store = JsonlSessionStore::new(root);
    let read = match std::fs::read_dir(root) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut ids = Vec::new();
    for entry in read {
        let name = entry?.file_name().into_string().ok();
        if let Some(id) = name.and_then(|n| n.strip_suffix(".open").map(str::to_owned)) {
            ids.push(SessionId::new(id));
        }
    }
    let held = store.held_open_locks(ids).await;
    let mut out: Vec<(String, Option<u32>)> = held
        .into_iter()
        .map(|id| {
            let pid = store.open_holder_pid(&SessionId::new(id.clone()));
            (id, pid)
        })
        .collect();
    out.sort();
    Ok(out)
}

async fn list_live(root: &Path, json: bool) -> anyhow::Result<()> {
    let mut rows: Vec<Live> = live_sessions(root)
        .await?
        .into_iter()
        .map(|(id, pid)| {
            let transcript = root.join(format!("{id}.jsonl"));
            let header = read_header(&transcript);
            Live {
                cwd: header["cwd"].as_str().unwrap_or("").to_string(),
                model: header["model"].as_str().unwrap_or("").to_string(),
                last_activity: std::fs::metadata(&transcript)
                    .and_then(|m| m.modified())
                    .ok(),
                id,
                pid,
            }
        })
        .collect();
    rows.sort_by_key(|a| std::cmp::Reverse(a.last_activity));
    if json {
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "pid": r.pid,
                    "cwd": r.cwd,
                    "model": r.model,
                    "last_activity": r.last_activity.map(unix_secs),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if rows.is_empty() {
        println!("no live sessions in {}", root.display());
        return Ok(());
    }
    let width = rows.iter().map(|r| r.id.len()).max().unwrap_or(2).max(2);
    println!(
        "{:<width$}  {:>7}  {:<20}  {:<16}  CWD",
        "ID", "PID", "MODEL", "LAST ACTIVITY"
    );
    for r in &rows {
        println!(
            "{:<width$}  {:>7}  {:<20}  {:<16}  {}",
            r.id,
            r.pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string()),
            r.model,
            r.last_activity.map(ago).unwrap_or_else(|| "-".to_string()),
            r.cwd,
        );
    }
    Ok(())
}

async fn read_session(root: &Path, id: &str, last: usize, json: bool) -> anyhow::Result<()> {
    let store = JsonlSessionStore::new(root);
    let sid = SessionId::new(id.to_string());
    let path: PathBuf = root.join(format!("{id}.jsonl"));
    if id.is_empty()
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        usage(format!("invalid session id {id:?}"));
    }
    if !path.is_file() {
        usage(format!("no session {id:?} in {}", root.display()));
    }
    let live = store.open_lock_held(&sid).await;
    let pid = live.then(|| store.open_holder_pid(&sid)).flatten();
    let (meta, entries) = store.load(&sid).await?;
    let messages: Vec<(u64, Role, Vec<String>)> = entries
        .iter()
        .filter(|e| !e.message.injected && e.message.role != Role::System)
        .filter_map(|e| {
            let lines = render_blocks(&e.message.content);
            (!lines.is_empty()).then_some((e.timestamp, e.message.role, lines))
        })
        .collect();
    let start = messages.len().saturating_sub(last.max(1));
    let shown = &messages[start..];
    if json {
        let out = json!({
            "id": id,
            "live": live,
            "pid": pid,
            "cwd": meta.cwd,
            "model": meta.model,
            "messages": shown.iter().map(|(ts, role, lines)| json!({
                "at": ts / 1000,
                "role": role.to_string(),
                "lines": lines,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    let state = match (live, pid) {
        (true, Some(pid)) => format!("live (pid {pid})"),
        (true, None) => "live".to_string(),
        (false, _) => "not live".to_string(),
    };
    println!(
        "session {id} · {state} · {} · {}",
        meta.model,
        meta.cwd.display()
    );
    if start > 0 {
        println!("({start} earlier message(s) not shown; --last N for more)");
    }
    for (ts, role, lines) in shown {
        println!();
        println!("[{}] {role}:", clock(*ts));
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

fn render_blocks(blocks: &[ContentBlock]) -> Vec<String> {
    let mut out = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text } if !text.trim().is_empty() => {
                out.push(text.trim_end().to_string());
            }
            ContentBlock::ToolUse { name, args, .. } => {
                let detail = tool_detail(args);
                if detail.is_empty() {
                    out.push(format!("→ {name}"));
                } else {
                    out.push(format!("→ {name}: {detail}"));
                }
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                let first = content
                    .lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty())
                    .unwrap_or("");
                let mark = if *is_error { "✗" } else { "⤷" };
                out.push(format!("  {mark} {}", preview(first)));
            }
            ContentBlock::Image { .. } => out.push("[image]".to_string()),
            ContentBlock::Media { media_type, .. } => out.push(format!("[{media_type}]")),
            _ => {}
        }
    }
    out
}

fn tool_detail(args: &Value) -> String {
    let pick = [
        "command",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
        "prompt",
    ]
    .iter()
    .find_map(|k| args.get(*k).and_then(Value::as_str))
    .map(str::to_string)
    .or_else(|| {
        args.as_object()
            .and_then(|m| m.values().find_map(Value::as_str))
            .map(str::to_string)
    })
    .unwrap_or_default();
    preview(&pick)
}

fn preview(text: &str) -> String {
    let one = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > PREVIEW_CHARS {
        format!("{}…", one.chars().take(PREVIEW_CHARS).collect::<String>())
    } else {
        one
    }
}

fn read_header(path: &Path) -> Value {
    use std::io::BufRead;
    std::fs::File::open(path)
        .ok()
        .and_then(|f| std::io::BufReader::new(f).lines().next()?.ok())
        .and_then(|line| serde_json::from_str(&line).ok())
        .unwrap_or(Value::Null)
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn clock(ms: u64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms as i64)
        .single()
        .map(|t| t.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "?".to_string())
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match secs {
        0..=59 => format!("{secs}s ago"),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86_399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}
