use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cron_serve;
use crate::session_store::{JsonlSessionStore, SessionId};

pub const KIND: &str = "send";

pub const STATE_QUEUED: &str = "queued";
pub const STATE_NOT_LIVE: &str = "queued (session not live)";
pub const STATE_STEERED: &str = "steered";
pub const STATE_DELIVERED: &str = "delivered";

const RETRY_MS: u64 = 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct SendMsg {
    pub session: String,
    pub id: String,
    pub from: String,
    pub text: String,
    pub attachments: Vec<PathBuf>,
}

pub fn prompt_for(from: &str, text: &str) -> String {
    format!("↳ from {from}\n{text}")
}

pub fn default_label() -> String {
    std::env::var("GRAY_SESSION_ID").unwrap_or_default()
}

pub fn clean_label(raw: &str) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
    let s = s.trim();
    if s.is_empty() {
        "cli".to_string()
    } else {
        s.to_string()
    }
}

pub fn is_send_entry(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .is_some_and(|v| v.get("kind").and_then(|k| k.as_str()) == Some(KIND))
}

pub fn is_held(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("not_before_ms")?.as_u64())
        .is_some_and(|t| t > now_ms())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn parse_send(text: &str, session: &str) -> Option<SendMsg> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v.get("kind")?.as_str()? != KIND {
        return None;
    }
    let field = |k: &str| v.get(k)?.as_str().map(str::to_string);
    let attachments = v
        .get("attachments")?
        .as_array()?
        .iter()
        .map(|a| a.as_str().map(PathBuf::from))
        .collect::<Option<Vec<_>>>()?;
    Some(SendMsg {
        session: session.to_string(),
        id: field("id")?,
        from: field("from")?,
        text: field("text")?,
        attachments,
    })
}

fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub fn receipt_dir(home: &Path) -> PathBuf {
    home.join("cron").join("send-receipts")
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

fn write_private_file(dir: &Path, name: &str, body: &str) -> anyhow::Result<()> {
    private_dir(dir)?;
    let tmp = dir.join(format!(".{name}.tmp"));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&tmp)?, body.as_bytes())?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

fn entry_json(
    id: &str,
    from: &str,
    text: &str,
    attachments: &[PathBuf],
    not_before_ms: Option<u64>,
) -> String {
    let mut v = serde_json::json!({
        "kind": KIND,
        "id": id,
        "from": from,
        "text": text,
        "attachments": attachments.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
        "created_ms": now_ms(),
    });
    if let Some(t) = not_before_ms {
        v["not_before_ms"] = serde_json::json!(t);
    }
    v.to_string()
}

pub fn enqueue(
    home: &Path,
    session: &str,
    from: &str,
    text: &str,
    attachments: &[PathBuf],
) -> anyhow::Result<String> {
    let dir = cron_serve::session_inbox(home, session)
        .ok_or_else(|| anyhow::anyhow!("unsafe session id {session:?}"))?;
    private_dir(&dir)?;
    let id = uuid::Uuid::new_v4().to_string();
    cron_serve::write_entry(&dir, &entry_json(&id, from, text, attachments, None))?;
    Ok(id)
}

pub fn write_receipt(home: &Path, session: &str, id: &str, state: &str) -> anyhow::Result<()> {
    if !safe_id(id) {
        anyhow::bail!("unsafe send id {id:?}");
    }
    let body = serde_json::json!({ "id": id, "session": session, "state": state });
    write_private_file(&receipt_dir(home), &format!("{id}.json"), &body.to_string())
        .context("write send receipt")
}

fn remove_receipt(home: &Path, id: &str) {
    if safe_id(id) {
        let _ = std::fs::remove_file(receipt_dir(home).join(format!("{id}.json")));
    }
}

pub fn read_receipt_state(home: &Path, id: &str) -> Option<String> {
    if !safe_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(receipt_dir(home).join(format!("{id}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("state")?.as_str().map(str::to_string)
}

fn pending_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    paths.sort();
    paths
}

pub fn claim_all(home: &Path, session: &str) -> Vec<SendMsg> {
    let Some(dir) = cron_serve::session_inbox(home, session) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in pending_files(&dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !is_send_entry(&text) || is_held(&text) {
            continue;
        }
        let Some(msg) = parse_send(&text, session) else {
            log::warn!("send inbox: dropped malformed {}", path.display());
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if std::fs::remove_file(&path).is_err() {
            continue;
        }
        out.push(msg);
    }
    out
}

pub fn peek_sends(home: &Path, session: &str) -> Vec<SendMsg> {
    let Some(dir) = cron_serve::session_inbox(home, session) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for path in pending_files(&dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !is_send_entry(&text) || is_held(&text) {
            continue;
        }
        if let Some(msg) = parse_send(&text, session) {
            out.push(msg);
        }
    }
    out
}

pub fn claim_steerable(home: &Path, session: &str) -> Option<SendMsg> {
    let dir = cron_serve::session_inbox(home, session)?;
    for path in pending_files(&dir) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if !is_send_entry(&text) || is_held(&text) {
            continue;
        }
        let Some(msg) = parse_send(&text, session) else {
            log::warn!("send inbox: dropped malformed {}", path.display());
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if !msg.attachments.is_empty() {
            continue;
        }
        if std::fs::remove_file(&path).is_err() {
            continue;
        }
        if let Err(e) = write_receipt(home, session, &msg.id, STATE_STEERED) {
            log::warn!("send receipt for {}: {e:#}", msg.id);
        }
        return Some(msg);
    }
    None
}

pub fn mark_delivered(msgs: &[SendMsg]) {
    let Ok(home) = crate::setup::gray_home() else {
        return;
    };
    for m in msgs {
        if let Err(e) = write_receipt(&home, &m.session, &m.id, STATE_DELIVERED) {
            log::warn!("send receipt for {}: {e:#}", m.id);
        }
    }
}

pub fn hold_back(msgs: &[SendMsg]) {
    let Ok(home) = crate::setup::gray_home() else {
        log::error!("send hold-back: no gray home; {} send(s) lost", msgs.len());
        return;
    };
    for m in msgs {
        if let Err(e) = hold_back_one(&home, m) {
            log::error!("send {} could not be put back: {e:#}", m.id);
        }
    }
}

fn hold_back_one(home: &Path, m: &SendMsg) -> anyhow::Result<()> {
    let dir = cron_serve::session_inbox(home, &m.session)
        .ok_or_else(|| anyhow::anyhow!("unsafe session id {:?}", m.session))?;
    private_dir(&dir)?;
    let body = entry_json(
        &m.id,
        &m.from,
        &m.text,
        &m.attachments,
        Some(now_ms() + RETRY_MS),
    );
    cron_serve::write_entry(&dir, &body)?;
    remove_receipt(home, &m.id);
    Ok(())
}

fn resolve_attachments(paths: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    paths
        .iter()
        .map(|p| {
            let abs =
                std::fs::canonicalize(p).map_err(|e| format!("attach {}: {e}", p.display()))?;
            if abs.is_file() {
                Ok(abs)
            } else {
                Err(format!("attach {}: not a file", p.display()))
            }
        })
        .collect()
}

fn usage(msg: String) -> ! {
    eprintln!("gray send: {msg}");
    std::process::exit(2)
}

pub(crate) fn owner_home() -> anyhow::Result<PathBuf> {
    match std::env::var_os("GRAY_USER_HOME").filter(|v| !v.to_string_lossy().trim().is_empty()) {
        Some(home) => Ok(PathBuf::from(home)),
        None => crate::setup::gray_home(),
    }
}

pub async fn run_cli(
    session: &str,
    text: &str,
    attach: &[PathBuf],
    from: Option<&str>,
    wait: Option<u64>,
) -> anyhow::Result<()> {
    let home = owner_home()?;
    let root = home.join("sessions");
    let store = JsonlSessionStore::new(&root);
    let session = match store.resolve_unique_prefix(session) {
        Ok(id) => id,
        Err(msg) => usage(msg),
    };
    if text.trim().is_empty() && attach.is_empty() {
        usage("nothing to send: give text or --attach".to_string());
    }
    let attachments = resolve_attachments(attach).unwrap_or_else(|e| usage(e));
    let from = clean_label(
        from.map(str::to_string)
            .unwrap_or_else(default_label)
            .as_str(),
    );
    let live = store.open_lock_held(&SessionId::new(session.clone())).await;
    let id = enqueue(&home, &session, &from, text, &attachments)?;
    let mut state = if live { STATE_QUEUED } else { STATE_NOT_LIVE }.to_string();
    if let Some(secs) = wait {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if let Some(s) = read_receipt_state(&home, &id) {
                state = s;
                break;
            }
            if std::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    println!(
        "{}",
        serde_json::json!({ "session": session, "id": id, "state": state })
    );
    Ok(())
}
