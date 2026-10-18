fn approved_demo() -> super::ApprovedEntry {
    super::ApprovedEntry::new(
        "demo".into(),
        serde_json::from_value(serde_json::json!({
            "ecosystem": "gray-native", "kind": "plugin", "version": "1.0.0",
            "source": {"type": "tarball", "url": "https://example.invalid/demo.tgz"},
            "hash": format!("sha256:{}", "0".repeat(64))
        }))
        .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn cancelled_prepare_has_no_install_side_effects() {
    let home = tempfile::tempdir().unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let result = super::prepare(
        home.path(),
        approved_demo(),
        &gray_pkg_client(),
        cancel,
        None,
    )
    .await;
    assert!(result.is_err());
    assert!(!home.path().join("plugins/demo").exists());
    assert!(!home.path().join("plugins/lock.json").exists());
}

fn gray_pkg_client() -> reqwest::Client {
    crate::fetch::client().unwrap()
}

fn archive_bytes(path: &str) -> Vec<u8> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    let body = b"#!/bin/sh\nprintf '{}\\n'\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    // Set raw bytes so the traversal fixture reaches the unpacker, not tar's builder guard.
    header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
    header.set_cksum();
    archive.append(&header, &body[..]).unwrap();
    archive.into_inner().unwrap().finish().unwrap()
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{address}/demo.tgz")
}

async fn archive_entry(bytes: Vec<u8>) -> super::ApprovedEntry {
    use sha2::Digest;
    let hash = format!("sha256:{:x}", sha2::Sha256::digest(&bytes));
    let url = serve(axum::Router::new().route(
        "/demo.tgz",
        axum::routing::get(move || {
            let bytes = bytes.clone();
            async move { bytes }
        }),
    ))
    .await;
    let mut entry = approved_demo().entry().clone();
    entry.source.url = url;
    entry.hash = crate::index::HashSpec::Single(hash);
    super::ApprovedEntry::new("demo".into(), entry).unwrap()
}

async fn prepared_demo(home: &std::path::Path) -> super::PreparedPackage {
    let entry = archive_entry(archive_bytes("plugin.sh")).await;
    super::prepare(
        home,
        entry,
        &gray_pkg_client(),
        tokio_util::sync::CancellationToken::new(),
        None,
    )
    .await
    .unwrap()
}

fn assert_clean(home: &std::path::Path) {
    let plugins = home.join("plugins");
    if !plugins.exists() {
        return;
    }
    for path in std::fs::read_dir(plugins).unwrap() {
        let path = path.unwrap().path();
        assert_eq!(
            path.file_name().unwrap(),
            "tmp",
            "unexpected leftover: {}",
            path.display()
        );
        assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn verified_preparation_is_owned_and_not_installed() {
    let home = tempfile::tempdir().unwrap();
    let prepared = prepared_demo(home.path()).await;
    assert_eq!(prepared.home(), home.path());
    assert_eq!(prepared.approved().name(), "demo");
    assert!(prepared.root().join("plugin.sh").is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(prepared.root().join("plugin.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
    assert!(!home.path().join("plugins/demo").exists());
    assert!(!home.path().join("plugins/lock.json").exists());
    let stage = prepared.root().parent().unwrap().to_path_buf();
    drop(prepared);
    assert!(!stage.exists());
    assert_clean(home.path());
}

#[tokio::test]
async fn corrupt_digest_and_traversal_cleanup_staging() {
    for corrupt_hash in [true, false] {
        let home = tempfile::tempdir().unwrap();
        let approved = archive_entry(archive_bytes(if corrupt_hash {
            "plugin.sh"
        } else {
            "../escape"
        }))
        .await;
        let approved = if corrupt_hash {
            let mut entry = approved.entry().clone();
            entry.hash = approved_demo().entry().hash.clone();
            super::ApprovedEntry::new("demo".into(), entry).unwrap()
        } else {
            approved
        };
        let error = super::prepare(
            home.path(),
            approved,
            &gray_pkg_client(),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains(if corrupt_hash {
            "hash mismatch"
        } else {
            "unsafe archive"
        }));
        assert!(!home.path().join("escape").exists());
        assert_clean(home.path());
    }
}

#[tokio::test]
async fn cancellation_during_download_cleans_owned_archive() {
    let home = tempfile::tempdir().unwrap();
    let (started, received) = tokio::sync::oneshot::channel();
    let started = std::sync::Arc::new(std::sync::Mutex::new(Some(started)));
    let url = serve(axum::Router::new().route(
        "/demo.tgz",
        axum::routing::get(move || {
            let started = started.clone();
            async move {
                let _ = started.lock().unwrap().take().unwrap().send(());
                std::future::pending::<Vec<u8>>().await
            }
        }),
    ))
    .await;
    let mut entry = approved_demo().entry().clone();
    entry.source.url = url;
    let approved = super::ApprovedEntry::new("demo".into(), entry).unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let client = gray_pkg_client();
    let future = super::prepare(home.path(), approved, &client, cancel.clone(), None);
    tokio::pin!(future);
    tokio::select! {
        result = &mut future => panic!("preparation completed unexpectedly: {result:?}"),
        _ = received => {}
    }
    cancel.cancel();
    assert!(future.await.unwrap_err().to_string().contains("cancelled"));
    assert_clean(home.path());
}

#[test]
fn approved_identity_rejects_unsafe_or_unsupported_entries() {
    for name in [
        "",
        "../escape",
        "Demo",
        "demo.plugin",
        "tmp",
        "pi",
        "a/b",
        "a\\b",
    ] {
        assert!(super::ApprovedEntry::new(name.into(), approved_demo().entry().clone()).is_err());
    }
    for (ecosystem, kind, source) in [
        ("other", "plugin", "tarball"),
        ("gray-native", "skill", "tarball"),
        ("gray-native", "plugin", "git"),
    ] {
        let mut entry = approved_demo().entry().clone();
        entry.ecosystem = ecosystem.into();
        entry.kind = kind.into();
        entry.source.type_ = source.into();
        assert!(super::ApprovedEntry::new("demo".into(), entry).is_err());
    }
    let mut entry = approved_demo().entry().clone();
    entry.hash = crate::index::HashSpec::Single(String::new());
    assert!(super::ApprovedEntry::new("demo".into(), entry).is_err());
}
