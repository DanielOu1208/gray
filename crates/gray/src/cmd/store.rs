//! Pending `gray.ui` documents: `<GRAY_HOME>/cmd_ui/<token>.json`, 1h TTL.
//! The draft an `editable` doc revises against lives here, never on the
//! wire — adapters only carry the rendered document.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::CmdError;
use crate::setup::variants::RowShape;

/// `interact` dies an hour after `run` minted the token; the adapter's
/// answer to `expired` is re-running the command fresh.
pub(crate) const PENDING_TTL_SECS: i64 = 3600;

/// A pending ui document — currently only the model picker mints one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PendingDoc {
    /// Which flow interprets `draft` (`"model_picker"`).
    #[serde(default)]
    pub(crate) kind: String,
    pub(crate) created_at: i64,
    /// The platform user the doc belongs to, once one has interacted.
    #[serde(default)]
    pub(crate) user: Option<String>,
    /// Picker state as of the last emitted revision.
    #[serde(default)]
    pub(crate) draft: ModelDraft,
    /// The model options the doc was rendered from — validation and
    /// re-render both need them without refetching the provider.
    #[serde(default)]
    pub(crate) models: Vec<(String, String)>,
    /// Composite rows' declared shapes by row id — `interact` re-renders
    /// the slot selects and validates parts without refetching.
    #[serde(default)]
    pub(crate) shapes: BTreeMap<String, RowShape>,
}

/// The model picker's server-side selection (`values`/`submit` revise it).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct ModelDraft {
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    #[serde(default)]
    pub(crate) fast: bool,
    /// Composite slot picks (slot key → option id); empty for plain rows.
    #[serde(default)]
    pub(crate) parts: BTreeMap<String, String>,
}

fn dir(home: &Path) -> PathBuf {
    home.join("cmd_ui")
}

fn doc_path(home: &Path, token: &str) -> PathBuf {
    dir(home).join(format!("{token}.json"))
}

fn write_doc(path: &Path, doc: &PendingDoc) -> std::io::Result<()> {
    let body = serde_json::to_vec(doc).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Mint a token and persist the pending doc. Returns the token.
pub(crate) fn put(home: &Path, doc: &PendingDoc) -> std::io::Result<String> {
    let dir = dir(home);
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(&dir)?;
    let token = uuid::Uuid::new_v4().simple().to_string();
    write_doc(&doc_path(home, &token), doc)?;
    Ok(token)
}

/// Read a pending doc: absent, malformed or TTL'd out is `expired` to the
/// caller (an expired file is removed so the state dir doesn't grow).
pub(crate) fn get(home: &Path, token: &str, now: i64) -> Result<PendingDoc, CmdError> {
    if token.is_empty()
        || token
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && b != b'-' && b != b'_')
    {
        return Err(CmdError::expired());
    }
    let path = doc_path(home, token);
    let doc: PendingDoc = std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(CmdError::expired)?;
    if now.saturating_sub(doc.created_at) > PENDING_TTL_SECS {
        let _ = std::fs::remove_file(&path);
        return Err(CmdError::expired());
    }
    Ok(doc)
}

/// Persist a revision (interact on an `editable` doc).
pub(crate) fn save(home: &Path, token: &str, doc: &PendingDoc) -> std::io::Result<()> {
    write_doc(&doc_path(home, token), doc)
}

/// Consume the token — `apply`/`cancel`, or a `one_shot` doc's first touch.
pub(crate) fn remove(home: &Path, token: &str) {
    let _ = std::fs::remove_file(doc_path(home, token));
}
