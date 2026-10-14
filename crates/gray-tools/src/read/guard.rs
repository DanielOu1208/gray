use std::fs;
use std::path::{Component, Path};

pub fn refusal(display: &str, kind: &str) -> String {
    format!(
        "read refused: {display} is a {kind}; \
         reading it would block. Use bash with a timeout if you really need it."
    )
}

fn components(path: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in path.components() {
        match c {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s.to_string_lossy().into_owned()),
        }
    }
    out
}

pub fn is_blocklisted(path: &Path) -> bool {
    let c = components(path);
    if c.is_empty() {
        return false;
    }
    if c[0] == "dev" {
        return match c.get(1).map(String::as_str) {
            Some("stdin") | Some("stdout") | Some("stderr") | Some("zero") | Some("urandom")
            | Some("random") => true,

            Some("fd") => true,
            Some(tty) if tty.starts_with("tty") => true,
            _ => false,
        };
    }
    if c[0] == "proc" {
        return match (c.get(1).map(String::as_str), c.get(2).map(String::as_str)) {
            (Some("self"), Some("fd")) => true,
            (Some(pid), Some("fd"))
                if !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) =>
            {
                true
            }
            _ => false,
        };
    }
    false
}

pub fn check_name(literal: &Path, canonical: Option<&Path>, display: &str) -> Result<(), String> {
    if is_blocklisted(literal) || canonical.is_some_and(|c| c != literal && is_blocklisted(c)) {
        return Err(refusal(display, "device/FIFO/socket"));
    }
    Ok(())
}

#[derive(Debug)]
pub enum MetadataDecision {
    RegularFile,
    Directory,
}

pub fn check_metadata(meta: &fs::Metadata, display: &str) -> Result<MetadataDecision, String> {
    let ft = meta.file_type();
    if ft.is_dir() {
        return Ok(MetadataDecision::Directory);
    }
    if ft.is_file() {
        return Ok(MetadataDecision::RegularFile);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if ft.is_char_device() || ft.is_block_device() {
            return Err(refusal(display, "device"));
        }
        if ft.is_fifo() {
            return Err(refusal(display, "FIFO"));
        }
        if ft.is_socket() {
            return Err(refusal(display, "socket"));
        }
        Ok(MetadataDecision::RegularFile)
    }
    #[cfg(not(unix))]
    {
        Err(refusal(display, "special file"))
    }
}

#[path = "guard_tests.rs"]
#[cfg(test)]
mod tests;
