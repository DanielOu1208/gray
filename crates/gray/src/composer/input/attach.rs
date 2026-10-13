//! Attachment helpers (split from `input`; re-exported there).

use super::*;

// ---------------------------------------------------------------------------
// Attachment helpers — verbatim from mod.rs 759-885
// ---------------------------------------------------------------------------

/// Attaches a file as a chip above the draft. The chip is the only thing
/// that marks it, so the draft text stays exactly what was typed.
pub(crate) fn attach_image(tui: &mut Tui, path: PathBuf) {
    tui.attachments.push(path);
    let _ = tui.draw();
}

/// Max attachable file size. `repl::attachments` caps only oversized
/// inputs, so this local sane cap refuses multi-GB drops before
/// ffmpeg/image decode OOMs.
pub(crate) const MAX_ATTACH_FILE_BYTES: u64 = 100 * 1024 * 1024;

/// Strip `file://` (+ optional `localhost`) and percent-decode (%20 etc.).
/// Raw (non-`file://`) paths pass through untouched so literal `%` in real
/// filenames is never corrupted. Shared by paste + clipboard-text paths.
fn decoded_paste_path(raw: &str) -> String {
    let s = raw
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    if let Some(stripped) = s.strip_prefix("file://") {
        url::Url::parse(s)
            .ok()
            .and_then(|u| u.to_file_path().ok())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| {
                stripped
                    .strip_prefix("localhost")
                    .unwrap_or(stripped)
                    .to_string()
            })
    } else {
        s.to_string()
    }
}

fn file_size_exceeds(path: &Path, limit: u64) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > limit)
        .unwrap_or(false)
}

/// Any file the media pipeline accepts (images, pdf, video, audio) —
/// opencode parity: MIME-driven, not image-only.
pub(crate) fn is_attachable_path(path: &str) -> bool {
    use crate::repl::attachments::{AttachmentKind, attachment_kind};
    let decoded = decoded_paste_path(path);
    let p = Path::new(&decoded);
    if !p.exists() || !p.is_file() {
        return false;
    }
    if file_size_exceeds(p, MAX_ATTACH_FILE_BYTES) {
        return false;
    }
    !matches!(attachment_kind(p), AttachmentKind::Unsupported)
}

pub(crate) fn try_attach_image_paste(tui: &mut Tui, pasted: &str) -> bool {
    let trimmed = pasted
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`');
    if trimmed.contains('\n') || trimmed.is_empty() || trimmed.len() > 512 {
        return false;
    }
    let path_str = decoded_paste_path(trimmed);
    let p = Path::new(&path_str);
    if p.exists() && p.is_file() && file_size_exceeds(p, MAX_ATTACH_FILE_BYTES) {
        tui.push_dim(format!(
            "attachment too big (>{}MB, skipped): {}",
            MAX_ATTACH_FILE_BYTES / 1024 / 1024,
            p.display()
        ));
        let _ = tui.draw();
        return true;
    }
    if is_attachable_path(&path_str) {
        let path = PathBuf::from(path_str);
        attach_image(tui, path);
        return true;
    }
    false
}

/// Paste an image from the OS clipboard via native helpers
/// (wl-paste/xclip). Image first, then clipboard text via the caller.
// arboard removed, native helpers only. If Wayland session
// quirks ever bite, the text-path fallback below still catches pasted paths.
pub(crate) fn try_attach_clipboard_image(tui: &mut Tui) -> bool {
    for (cmd, args) in [
        ("wl-paste", vec!["--type", "image/png"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ] {
        // The image probe gets the same bounded spawn the text path uses:
        // a wedged wl-paste/xclip must not freeze Ctrl+V (audit #16).
        if let Some(out) = super::clipboard::output_with_timeout(
            std::path::Path::new(cmd),
            &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        ) && !out.stdout.is_empty()
            && out.status.success()
            && let Ok(mut tmp) = tempfile::Builder::new()
                .prefix("clipboard-")
                .suffix(".png")
                .tempfile()
            && std::io::Write::write_all(&mut tmp, &out.stdout).is_ok()
            && let Ok((_file, path)) = tmp.keep()
        {
            if image::open(&path).is_ok() {
                attach_image(tui, path);
                return true;
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    if let Some(text) = clipboard::read_system_clipboard_text()
        && try_attach_image_paste(tui, &text)
    {
        return true;
    }
    false
}

#[path = "attach_tests.rs"]
#[cfg(test)]
mod tests;
