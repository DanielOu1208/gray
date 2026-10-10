//! Shared filesystem roots, without changing process environment. Setting HOME
//! during startup is unsafe once threads exist and makes native and Git tools
//! disagree. Consumers resolve the platform profile directly instead.
use std::path::PathBuf;

pub fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // std uses USERPROFILE, then the Windows profile API. In particular it
        // does not confuse Git Bash's HOME with the native Windows profile.
        std::env::home_dir()
    }
    #[cfg(not(windows))]
    {
        // Preserve the Unix contract: no implicit passwd fallback when a host
        // deliberately removed HOME. Callers already define their error policy.
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

pub fn gray_home() -> Option<PathBuf> {
    std::env::var_os("GRAY_HOME")
        .filter(|v| !v.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| user_home().map(|p| p.join(".gray")))
}

/// `~/.gray/attachments` — where media bytes are materialized so a pasted or
/// replayed attachment outlives both the context copy and a /tmp file.
pub fn attachments_dir() -> Option<PathBuf> {
    gray_home().map(|h| h.join("attachments"))
}

/// Decode base64 `data` into `~/.gray/attachments/<sha16>.<ext>` and return
/// the path. Content-addressed: the same bytes always land on the same file,
/// so repeats are free. `None` on decode/write failure or no home.
///
/// Callers get a durable path they can hand to `read` (image files re-attach
/// as vision) long after the context copy drops the bytes or the session is
/// reopened — the transcript's `↳ attached` chip and the elision citation
/// both point here.
pub fn materialize_media(media_type: &str, data: &str) -> Option<PathBuf> {
    use base64::Engine;
    use sha2::Digest;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()?;
    let digest = sha2::Sha256::digest(&bytes);
    let tag: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    let dir = attachments_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("{tag}.{}", media_extension(media_type)));
    if !path.exists() {
        std::fs::write(&path, &bytes).ok()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
    Some(path)
}

/// Filename extension for a media MIME (data-URL params ignored).
fn media_extension(media_type: &str) -> &'static str {
    match media_type.split(';').next().unwrap_or_default().trim() {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "application/pdf" => "pdf",
        "audio/mpeg" | "audio/mp3" => "mp3",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/ogg" => "ogg",
        "audio/flac" | "audio/x-flac" => "flac",
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/quicktime" => "mov",
        _ => "bin",
    }
}

/// `GRAY_HOME` is process-global; tests that repoint it serialize on this
/// (shared across test files so two suites can't clobber each other).
#[cfg(test)]
pub(crate) static HOME_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
