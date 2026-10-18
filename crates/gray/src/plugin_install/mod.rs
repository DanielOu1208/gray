// Phase C removes this allowance when the marketplace connects these APIs.
#![allow(dead_code)]

pub(crate) mod policy;
pub(crate) mod probe;

use std::path::Path;

use gray_pkg::staged::{ApprovedEntry, InstallStage, PreparedPackage};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) struct PreparedInstall {
    pub(crate) package: PreparedPackage,
    pub(crate) manifest: probe::PreparedManifest,
    pub(crate) scan: gray_plugin::scan::Report,
}

pub(crate) async fn prepare(
    home: &Path,
    approved: ApprovedEntry,
    cancel: CancellationToken,
    progress: Option<UnboundedSender<InstallStage>>,
) -> anyhow::Result<PreparedInstall> {
    let client = gray_pkg::fetch::client()?;
    let package =
        gray_pkg::staged::prepare(home, approved, &client, cancel.clone(), progress.clone())
            .await?;
    stage(&progress, InstallStage::Scanning);
    // Keep package ownership in this task and await the bounded scan before cancellation cleanup.
    let root = package.root().to_owned();
    let scan = tokio::task::spawn_blocking(move || gray_plugin::scan::scan_tree(&root)).await??;
    policy::require_installable_scan(&scan)?;
    anyhow::ensure!(!cancel.is_cancelled(), "preparation cancelled");
    stage(&progress, InstallStage::ReadingManifest);
    let manifest = probe::prepared(package.root(), cancel.clone()).await?;
    anyhow::ensure!(!cancel.is_cancelled(), "preparation cancelled");
    Ok(PreparedInstall {
        package,
        manifest,
        scan,
    })
}

fn stage(progress: &Option<UnboundedSender<InstallStage>>, value: InstallStage) {
    if let Some(tx) = progress {
        let _ = tx.send(value);
    }
}

#[cfg(test)]
mod tests;
