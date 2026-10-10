//! Shared source helpers: the shallow git clone used by `github:` skill
//! installs and plugin `git` sources.

use std::path::PathBuf;

/// Shallow-clone `url` into `<tmp>/repo` (`extra` holds `--branch` /
/// `--filter` / `--sparse` flags) and return the keeper tempdir + path.
pub fn clone_into_tmp(
    url: &str,
    extra: &[&str],
    under_plugins_tmp: bool,
) -> anyhow::Result<(tempfile::TempDir, PathBuf)> {
    let tmp = if under_plugins_tmp {
        let root = crate::plugins_dir().join("tmp");
        std::fs::create_dir_all(&root)?;
        tempfile::tempdir_in(&root)?
    } else {
        tempfile::tempdir()?
    };
    let dst = tmp.path().join("repo");
    // Installs are hash-verified content copies, not working trees: line
    // endings must reach the disk exactly as committed. Windows runners
    // default core.autocrlf=true, which rewrites every text file.
    let mut cmd = std::process::Command::new("git");
    cmd.args(["-c", "core.autocrlf=false", "clone", "--depth", "1"])
        .args(extra)
        .arg("--")
        .arg(url)
        .arg(&dst)
        .env("GIT_TERMINAL_PROMPT", "0");
    let out = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("git failed to run ({e})"))?;
    if !out.status.success() {
        anyhow::bail!(
            "cloning {} failed: {}",
            crate::fetch::redact(url),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok((tmp, dst))
}

