//! `gray send <session> "text"`: message a running gray session directly.
//!
//! A send is one entry in the session's inbox (`<home>/cron/inbox/<session>`,
//! the spool cron deliveries already use), marked `"kind":"send"`. The
//! session's REPL claims it one of two ways:
//! - mid-turn: [`claim_steerable`] at a tool boundary, through the same steer
//!   hook as the TUI's own queued input (text-only; attachments wait);
//! - idle: [`claim_all`] from `repl::cron::idle_wake`, which starts a turn
//!   with the origin line and the attachments. If no turn runs, [`hold_back`]
//!   puts the sends back as queued.
//!
//! Receipts live in `<home>/cron/send-receipts/<id>.json` and `gray send
//! --wait` polls them. They sit outside the inbox so the REPL's poll never
//! mistakes a receipt for a delivery.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cron_serve;
use crate::session_store::{JsonlSessionStore, SessionId};

/// Inbox entry `kind` for a direct send (cron entries carry no kind).
pub const KIND: &str = "send";
/// Receipt states. A receipt file appears only once a session owns the message.
pub const STATE_QUEUED: &str = "queued";
pub const STATE_NOT_LIVE: &str = "queued (session not live)";
pub const STATE_STEERED: &str = "steered";
pub const STATE_DELIVERED: &str = "delivered";
/// A send put back because no turn ran waits this long before it is offered again.
const RETRY_MS: u64 = 60_000;

/// One claimed direct send.
#[derive(Debug, Clone, PartialEq)]
pub struct SendMsg {
    pub session: String,
    pub id: String,
    pub from: String,
    pub text: String,
    pub attachments: Vec<PathBuf>,
}

/// The line the transcript and the model both see before a sent message, so
/// it is never mistaken for something the user typed.
pub fn prompt_for(from: &str, text: &str) -> String {
    format!("↳ from {from}\n{text}")
}

/// Origin label when `--from` is not given: the sending gray session, else `cli`.
pub fn default_label() -> String {
    std::env::var("GRAY_SESSION_ID").unwrap_or_default()
}

/// Strips control characters and caps the label; an empty label is `cli`.
pub fn clean_label(raw: &str) -> String {
    let s: String = raw.chars().filter(|c| !c.is_control()).take(64).collect();
    let s = s.trim();
    if s.is_empty() {
        "cli".to_string()
    } else {
        s.to_string()
    }
}

/// True for an inbox entry that is a direct send (cron drains skip these).
pub fn is_send_entry(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .is_some_and(|v| v.get("kind").and_then(|k| k.as_str()) == Some(KIND))
}

/// True for a send put back after no turn ran, until its retry time.
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

/// Parses a send entry; `None` when a required field is missing.
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

/// Receipt ids come from inbox files another same-user process can write, so
/// they must not be able to name a path outside the receipts dir.
fn safe_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// `<home>/cron/send-receipts`.
pub fn receipt_dir(home: &Path) -> PathBuf {
    home.join("cron").join("send-receipts")
}

/// Creates a directory that only the owner can read (0700 on unix).
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

/// Atomic 0600 write: temp file, then rename.
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

/// The inbox entry body for one send.
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

/// Queues one send for `session`; returns its id. Errors on an unsafe id.
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

/// Writes the receipt for send `id` addressed to `session`.
pub fn write_receipt(home: &Path, session: &str, id: &str, state: &str) -> anyhow::Result<()> {
    if !safe_id(id) {
        anyhow::bail!("unsafe send id {id:?}");
    }
    let body = serde_json::json!({ "id": id, "session": session, "state": state });
    write_private_file(&receipt_dir(home), &format!("{id}.json"), &body.to_string())
        .context("write send receipt")
}

/// Removes a receipt: the message is back in the inbox, so it is queued again.
fn remove_receipt(home: &Path, id: &str) {
    if safe_id(id) {
        let _ = std::fs::remove_file(receipt_dir(home).join(format!("{id}.json")));
    }
}

/// The receipt state for send `id`, once a session has consumed it.
pub fn read_receipt_state(home: &Path, id: &str) -> Option<String> {
    if !safe_id(id) {
        return None;
    }
    let text = std::fs::read_to_string(receipt_dir(home).join(format!("{id}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("state")?.as_str().map(str::to_string)
}

/// Non-dot files in the session's inbox, oldest first.
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

/// Claims every ready send for `session` (idle path). A file is claimed by
/// removing it, so two readers never both deliver one message. Cron entries
/// and held sends stay where they are. No receipt is written here: the caller
/// marks the sends delivered once a turn owns them ([`mark_delivered`]).
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

/// Claims the oldest ready text-only send for `session` to steer a running
/// turn. Sends with attachments stay queued for the idle turn that can carry them.
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

/// Receipts say `delivered` once a turn owns the messages; called right before
/// the turn starts. [`hold_back`] reverts it if the turn never runs.
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

/// Puts claimed sends back in the inbox as queued because no turn ran for them.
/// Same id, so the sender's receipt follows the message; offered again after
/// [`RETRY_MS`]. A send is never dropped here: a failed put-back is logged loudly.
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

/// Resolves `--attach` paths against the sender's cwd; each must be a file.
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

/// Exit 2 with a message: the caller's mistake, not a runtime failure.
fn usage(msg: String) -> ! {
    eprintln!("gray send: {msg}");
    std::process::exit(2)
}

/// `gray send`: queue the message, print the receipt JSON, and with `wait`
/// block until the session consumes it or the time runs out.
pub async fn run_cli(
    session: &str,
    text: &str,
    attach: &[PathBuf],
    from: Option<&str>,
    wait: Option<u64>,
) -> anyhow::Result<()> {
    let home = crate::setup::gray_home()?;
    let root = crate::session_store::default_root()
        .ok_or_else(|| anyhow::anyhow!("no gray home for sessions"))?;
    if cron_serve::session_inbox(&home, session).is_none() {
        usage(format!("invalid session id {session:?}"));
    }
    if !root.join(format!("{session}.jsonl")).is_file() {
        usage(format!("no session {session:?}"));
    }
    if text.trim().is_empty() && attach.is_empty() {
        usage("nothing to send: give text or --attach".to_string());
    }
    let attachments = resolve_attachments(attach).unwrap_or_else(|e| usage(e));
    let from = clean_label(
        from.map(str::to_string)
            .unwrap_or_else(default_label)
            .as_str(),
    );
    let live = JsonlSessionStore::new(&root)
        .open_lock_held(&SessionId::new(session))
        .await;
    let id = enqueue(&home, session, &from, text, &attachments)?;
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
