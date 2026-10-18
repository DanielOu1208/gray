use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct ApprovedEntry {
    name: String,
    entry: crate::index::Entry,
}

#[derive(Debug)]
pub struct PreparedPackage {
    approved: ApprovedEntry,
    home: PathBuf,
    stage: tempfile::TempDir,
    root: PathBuf,
}

impl ApprovedEntry {
    pub fn new(name: String, entry: crate::index::Entry) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "invalid marketplace plugin name"
        );
        anyhow::ensure!(
            !matches!(name.as_str(), "tmp" | "pi"),
            "reserved plugin name"
        );
        anyhow::ensure!(
            entry.ecosystem == "gray-native"
                && entry.source.type_ == "tarball"
                && matches!(entry.kind.as_str(), "" | "plugin"),
            "unsupported marketplace package"
        );
        anyhow::ensure!(
            entry.hash.primary().is_some_and(|h| !h.is_empty()),
            "marketplace package requires a digest"
        );
        Ok(Self { name, entry })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn entry(&self) -> &crate::index::Entry {
        &self.entry
    }
}

impl PreparedPackage {
    pub fn root(&self) -> &Path {
        debug_assert!(self.root.starts_with(self.stage.path()));
        &self.root
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn approved(&self) -> &ApprovedEntry {
        &self.approved
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallStage {
    Downloading,
    Unpacking,
    Scanning,
    ReadingManifest,
    Committing,
}

pub async fn prepare(
    home: &Path,
    approved: ApprovedEntry,
    client: &reqwest::Client,
    cancel: CancellationToken,
    progress: Option<tokio::sync::mpsc::UnboundedSender<InstallStage>>,
) -> anyhow::Result<PreparedPackage> {
    anyhow::ensure!(!cancel.is_cancelled(), "preparation cancelled");
    let plugins = home.join("plugins");
    std::fs::create_dir_all(&plugins)?;
    if let Some(tx) = &progress {
        let _ = tx.send(InstallStage::Downloading);
    }
    let tmp = plugins.join("tmp");
    let archive = tokio::select! {
        biased;
        _ = cancel.cancelled() => anyhow::bail!("preparation cancelled"),
        result = crate::fetch::download_owned(
            client, &approved.entry.source.url, Some(&approved.entry.hash),
            &tmp
        ) => result?,
    };
    let stage = tempfile::tempdir_in(&plugins)?;
    let home = home.to_path_buf();
    if let Some(tx) = &progress {
        let _ = tx.send(InstallStage::Unpacking);
    }
    // Keep both owned paths in the blocking job and await it even after cancellation.
    let prepared = tokio::task::spawn_blocking(move || {
        let root = stage.path().join("next");
        std::fs::create_dir(&root)?;
        crate::fetch::unpack_tar_gz(&archive, &root)?;
        Ok::<_, anyhow::Error>(PreparedPackage {
            approved,
            home,
            stage,
            root,
        })
    })
    .await??;
    anyhow::ensure!(!cancel.is_cancelled(), "preparation cancelled");
    Ok(prepared)
}

#[path = "staged_tests.rs"]
#[cfg(test)]
mod tests;
