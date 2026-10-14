use std::sync::Arc;

use async_trait::async_trait;
use gray_core::agent::{ToolContext, ToolOutput};
use gray_core::message::ToolDef;
use serde_json::Value;
use serde_json::json;

use crate::ledger::{FileLedger, LedgerEntry};
use crate::read::notices;
use crate::{Tool, fail, finish, get_opt_bool, get_str, resolve_path};

pub struct WriteTool {
    ledger: Arc<FileLedger>,
}

impl WriteTool {

    pub fn new(ledger: Arc<FileLedger>) -> Self {
        Self { ledger }
    }

    pub(crate) fn decide(
        entry: Option<&LedgerEntry>,
        meta: Option<&std::fs::Metadata>,
        old_bytes: &[u8],
        force: bool,
        display: &str,
    ) -> Option<String> {
        let meta = meta?;
        let Some(entry) = entry else {
            if force {
                return None;
            }
            return Some(notices::write_unread(display));
        };
        if meta.modified().is_ok_and(|t| t != entry.mtime) || meta.len() != entry.size {
            return Some(notices::write_changed(display));
        }

        if let (Some(expected), Some(actual)) =
            (entry.content_hash, FileLedger::hash_bytes(old_bytes))
            && expected != actual
        {
            return Some(notices::write_changed(display));
        }

        if force {
            return None;
        }
        if entry.full_view {
            return None;
        }
        let old_text = String::from_utf8_lossy(old_bytes);
        Some(notices::write_partial(
            display,
            entry.first_line,
            entry.last_line,
            old_text.lines().count(),
        ))
    }
}

impl Default for WriteTool {
    fn default() -> Self {
        Self {
            ledger: Arc::new(FileLedger::new()),
        }
    }
}

pub(crate) fn still_fresh(
    path: &std::path::Path,
    expected_meta: Option<&std::fs::Metadata>,
    expected_hash: Option<u64>,
) -> bool {
    let current = std::fs::metadata(path).ok();
    match (expected_meta, current) {
        (None, None) => {}
        (Some(want), Some(got)) => {
            let same_meta = got.len() == want.len() && got.modified().ok() == want.modified().ok();
            if !same_meta {
                return false;
            }
        }
        _ => return false,
    }
    match expected_hash {
        Some(want) => match std::fs::read(path) {
            Ok(bytes) => FileLedger::hash_bytes(&bytes) == Some(want),
            Err(_) => false,
        },
        None => true,
    }
}

pub(crate) async fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let existing_perms = tokio::fs::metadata(path)
        .await
        .ok()
        .map(|m| m.permissions());
    let tmp_name = format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|s| s.to_string_lossy())
            .as_deref()
            .unwrap_or("file"),
        uuid::Uuid::new_v4()
    );
    let tmp = match path.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join(tmp_name),
        None => std::path::PathBuf::from(tmp_name),
    };
    let result: std::io::Result<()> = async {
        tokio::fs::write(&tmp, bytes).await?;
        if let Some(perms) = existing_perms {
            tokio::fs::set_permissions(&tmp, perms).await?;
        }
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    result
}

#[async_trait]
impl Tool for WriteTool {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "write",
            "Create or overwrite a file with the given content. Parent \
             directories are created automatically.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path (absolute or relative to cwd)"
                    },
                    "content": {
                        "type": "string",
                        "description": "Full file contents (explicit empty string truncates)"
                    },
                    "force": {
                        "type": "boolean",
                        "description": "Overwrite without having read the file (bypasses the read-before-write guard)"
                    }
                },
                "required": ["path"]
            }),
        )
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolOutput {

        let path = match get_str(&args, "path") {
            Ok(p) => p,
            Err(e) => return e,
        };

        let content = match args.get("content") {
            None => {
                return fail(
                    "missing required argument 'content' (provide an explicit empty string to truncate)".to_string(),
                );
            }
            Some(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => {
                    return fail(format!(
                        "invalid argument 'content': expected string, got {}",
                        crate::read::args::render_got(v)
                    ));
                }
            },
        };

        let full = resolve_path(&ctx.cwd, &path);
        let display = full.display().to_string();
        let force = match get_opt_bool(&args, "force") {
            Ok(v) => v.unwrap_or(false),
            Err(e) => return e,
        };

        let disk_meta = match tokio::fs::metadata(&full).await {
            Ok(m) => Some(m),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return fail(format!("write failed for {}: {e}", full.display())),
        };

        if let Some(meta) = disk_meta.as_ref()
            && !meta.file_type().is_file()
        {
            return fail(format!("write refused: {display} is not a regular file"));
        }

        let old_bytes: Vec<u8> = if disk_meta.is_some() {
            match tokio::fs::read(&full).await {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return fail(notices::write_changed(&display));
                }
                Err(e) => return fail(format!("write failed for {}: {e}", full.display())),
            }
        } else {
            Vec::new()
        };
        let old = String::from_utf8_lossy(&old_bytes).into_owned();

        if let Some(msg) = Self::decide(
            self.ledger.get(&full).as_ref(),
            disk_meta.as_ref(),
            &old_bytes,
            force,
            &display,
        ) {
            return fail(msg);
        }
        let existed = disk_meta.is_some();

        let recheck_hash = self.ledger.get(&full).and_then(|e| e.content_hash);
        if !still_fresh(&full, disk_meta.as_ref(), recheck_hash) {
            return fail(notices::write_changed(&display));
        }
        match atomic_write(&full, content.as_bytes()).await {
            Ok(()) => {

                self.ledger.mark_written(&full, content.as_bytes());
                if existed && !old.is_empty() {
                    let patch = crate::edit_diff::generate_unified_patch(&path, &old, &content, 3);

                    let patch = gray_core::redaction::redact_for_disclosure(&patch).into_text();
                    if patch.is_empty() {
                        finish(format!(
                            "wrote {} bytes to {} (no change)",
                            content.len(),
                            full.display()
                        ))
                    } else {
                        finish(patch)
                    }
                } else {
                    finish(format!(
                        "wrote {} bytes to {}",
                        content.len(),
                        full.display()
                    ))
                }
            }
            Err(e) => fail(format!("write failed for {}: {e}", full.display())),
        }
    }
}

#[path = "write_tests.rs"]
#[cfg(test)]
mod tests;
