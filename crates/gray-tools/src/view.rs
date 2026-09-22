//! Showing an image file to whoever asked — the `gray view` command and the
//! bash tool's `gray view` / `cat` fast path both decode here.
//!
//! bash output is text only, so an agent that renders a chart, screenshot or
//! diagram has nothing to check its own work against. Both surfaces land
//! here: decode, downscale-before-send (the caps pasted attachments and the
//! `read` tool already use), hand back base64 for a vision block. Text files
//! stay bash's job (`cat`) — the extension gate refuses them before any
//! decoding, so a mislabeled file fails loudly instead of sending an
//! undecodable part.

use std::path::{Path, PathBuf};

/// One image, ready to ride along as a vision block.
#[derive(Debug)]
pub struct Shown {
    pub path: PathBuf,
    pub media_type: String,
    /// base64 of the encoded (downscaled) bytes.
    pub data: String,
}

/// Why an image could not be shown, worded for whoever prints it.
#[derive(Debug)]
pub enum ViewError {
    NotAnImage(PathBuf),
    Read(PathBuf, std::io::Error),
    Media(PathBuf, crate::images::MediaError),
}

impl std::fmt::Display for ViewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnImage(p) => write!(
                f,
                "not an image file: {} — view accepts png/jpg/jpeg/gif/webp/bmp/heic/heif; use cat for text files",
                p.display()
            ),
            Self::Read(p, e) => write!(f, "view failed for {}: {e}", p.display()),
            Self::Media(p, e) => write!(f, "view failed for {}: {e}", p.display()),
        }
    }
}

/// Downscale-before-send one image file: longest side capped at 2000px and
/// base64 under 5MB — [`crate::images::normalize_image_bytes`], the same
/// normalization pasted attachments and the `read` tool take. `cat` is the
/// full-resolution exception; this is the everyday path.
pub fn load(path: &Path) -> Result<Shown, ViewError> {
    // Extension gate first: cheap, and it turns `view notes.md` into a clear
    // refusal naming bash's role instead of a decode failure.
    if !crate::images::is_image_extension(path) {
        return Err(ViewError::NotAnImage(path.to_path_buf()));
    }
    let bytes = std::fs::read(path).map_err(|e| ViewError::Read(path.to_path_buf(), e))?;
    let (media_type, out) = crate::images::normalize_image_bytes(&bytes)
        .map_err(|e| ViewError::Media(path.to_path_buf(), e))?;
    use base64::Engine as _;
    Ok(Shown {
        path: path.to_path_buf(),
        media_type,
        data: base64::engine::general_purpose::STANDARD.encode(&out),
    })
}

#[path = "view_tests.rs"]
#[cfg(test)]
mod tests;
