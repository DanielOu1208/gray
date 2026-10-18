#[test]
fn empty_consent_is_not_grandfathered() {
    let record = super::policy::grant(&[]);
    assert!(record.granted.is_empty());
    assert_eq!(record.hash, gray_plugin::capabilities::consent_hash(&[]));
}

#[test]
fn unknown_capabilities_are_not_granted() {
    let record = super::policy::grant(&[
        gray_plugin::capabilities::HOST_ASK.to_string(),
        "invented.capability".into(),
    ]);
    assert_eq!(
        record.granted,
        vec![gray_plugin::capabilities::HOST_ASK.to_string()]
    );
}

#[test]
fn consent_normalizes_known_ids_and_displays_unknowns() {
    let declared = vec![" host.ask ".into(), "host.ask".into(), "invented".into()];
    let consent = super::policy::grant(&declared);
    assert_eq!(consent.granted, ["host.ask"]);
    assert_eq!(
        consent.hash,
        gray_plugin::capabilities::consent_hash(&["host.ask".into()])
    );
    let lines = super::policy::capability_lines(&declared);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].grantable);
    assert_eq!(
        lines[0].description,
        gray_plugin::capabilities::spec("host.ask")
            .unwrap()
            .description
    );
    assert_eq!(lines[2].id, "invented");
    assert!(!lines[2].grantable);
}

#[test]
fn scan_blocks_only_dangerous_and_retains_caution_findings() {
    use gray_plugin::scan::{Finding, Report, Severity};
    let mut report = Report::default();
    super::policy::require_installable_scan(&report).unwrap();
    report.findings.push(Finding {
        rule: "fixture",
        severity: Severity::Caution,
        path: "plugin.sh".into(),
        line: 1,
        detail: "review this finding".into(),
    });
    super::policy::require_installable_scan(&report).unwrap();
    assert!(report.render().contains("review this finding"));
    report.findings[0].severity = Severity::Critical;
    let error = super::policy::require_installable_scan(&report).unwrap_err();
    assert!(error.to_string().contains("blocked installation"));
}

#[cfg(unix)]
mod staged {
    use super::super as install;
    use gray_pkg::staged::{ApprovedEntry, InstallStage};
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    fn executable(root: &Path, script: &str) -> PathBuf {
        let path = root.join("plugin.sh");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    const COUNTER: &str = r#"#!/bin/sh
printf x >> "$0.count"
printf '%s\n' '{"name":"runtime-demo","version":"1.0.0","protocol":"1.1","capabilities":["host.ask"],"commands":["/demo"]}'
"#;

    #[tokio::test]
    async fn manifest_probe_runs_once() {
        let root = tempfile::tempdir().unwrap();
        let exe = executable(root.path(), COUNTER);
        let raw = super::super::probe::manifest(&exe, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(raw["name"], "runtime-demo");
        assert_eq!(
            std::fs::read_to_string(exe.with_extension("sh.count")).unwrap(),
            "x"
        );
    }

    async fn wait_for_file(path: &Path) {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !std::fs::read_to_string(path)
                .ok()
                .is_some_and(|s| s.trim().parse::<i32>().is_ok())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn assert_reaped(pid_file: &Path) {
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        // The owned probe must wait, not merely signal the child.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn cancelling_manifest_probe_kills_and_reaps_child() {
        let root = tempfile::tempdir().unwrap();
        let exe = executable(
            root.path(),
            "#!/bin/sh\necho $$ > \"$0.pid\"\nexec sleep 30\n",
        );
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let task = tokio::spawn(async move { super::super::probe::manifest(&exe, token).await });
        let pid = root.path().join("plugin.sh.pid");
        wait_for_file(&pid).await;
        cancel.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_reaped(&pid);
    }

    const WIRE: &str = r#"#!/bin/sh
if [ $# -gt 0 ]; then exit 1; fi
while IFS= read -r line; do
  case "$line" in
    *plugin/shutdown*) exit 0 ;;
    *plugin/manifest*)
      printf '%s\n' '{"id":1,"result":{"name":"runtime-demo","version":"1.0.0","protocol":"1.1","tools":[],"capabilities":["host.ask","invented"],"commands":["/demo"]}}'
      ;;
  esac
done
"#;

    #[tokio::test]
    async fn wire_fallback_matches_existing_sidecar_manifest() {
        use gray_plugin::Plugin as _;
        let root = tempfile::tempdir().unwrap();
        let exe = executable(root.path(), WIRE);
        let prepared = install::probe::prepared(root.path(), CancellationToken::new())
            .await
            .unwrap();
        assert!(!prepared.cli);
        assert_eq!(prepared.declared, ["host.ask", "invented"]);
        let old = gray_plugin::SidecarPlugin::spawn(vec![exe.to_string_lossy().into_owned()])
            .await
            .unwrap();
        let expected = serde_json::to_value(old.manifest()).unwrap();
        old.shutdown(std::time::Duration::from_secs(2)).await;
        assert_eq!(
            serde_json::to_value(gray_plugin::Manifest::from_result(&prepared.raw)).unwrap(),
            expected
        );
    }

    #[tokio::test]
    async fn cancelling_wire_handshake_kills_and_reaps_child() {
        let root = tempfile::tempdir().unwrap();
        executable(
            root.path(),
            "#!/bin/sh\nif [ $# -gt 0 ]; then exit 1; fi\necho $$ > \"$0.pid\"\nexec sleep 30\n",
        );
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let path = root.path().to_owned();
        let task = tokio::spawn(async move { install::probe::prepared(&path, token).await });
        let pid = root.path().join("plugin.sh.pid");
        wait_for_file(&pid).await;
        cancel.cancel();
        assert!(format!("{:#}", task.await.unwrap().unwrap_err()).contains("cancelled"));
        assert_reaped(&pid);
    }

    #[tokio::test]
    async fn cancelled_cli_probe_never_starts_wire_fallback() {
        let root = tempfile::tempdir().unwrap();
        executable(
            root.path(),
            "#!/bin/sh\nif [ $# -eq 0 ]; then touch \"$0.wire\"; exit 1; fi\necho $$ > \"$0.pid\"\nexec sleep 30\n",
        );
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let path = root.path().to_owned();
        let task = tokio::spawn(async move { install::probe::prepared(&path, token).await });
        let pid = root.path().join("plugin.sh.pid");
        wait_for_file(&pid).await;
        cancel.cancel();
        assert!(task.await.unwrap().is_err());
        assert!(!root.path().join("plugin.sh.wire").exists());
        assert_reaped(&pid);
    }

    #[tokio::test]
    async fn manifest_timeout_and_oversized_output_reap_children() {
        let root = tempfile::tempdir().unwrap();
        let exe = executable(
            root.path(),
            "#!/bin/sh\necho $$ > \"$0.pid\"\nexec sleep 30\n",
        );
        let error = install::probe::manifest(&exe, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert_reaped(&root.path().join("plugin.sh.pid"));
        executable(
            root.path(),
            "#!/bin/sh\necho $$ > \"$0.pid\"\nwhile :; do printf '01234567890123456789012345678901'; done\n",
        );
        let error = install::probe::manifest(&exe, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("64 KiB"));
        assert_reaped(&root.path().join("plugin.sh.pid"));
    }

    #[tokio::test]
    async fn wire_oversized_output_reaps_child() {
        let root = tempfile::tempdir().unwrap();
        let exe = executable(
            root.path(),
            "#!/bin/sh\necho $$ > \"$0.pid\"\nwhile :; do printf '01234567890123456789012345678901'; done\n",
        );
        let error = install::probe::wire_manifest(&exe, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("256 KiB"));
        assert_reaped(&root.path().join("plugin.sh.pid"));
    }

    struct Fixture {
        home: tempfile::TempDir,
        approved: ApprovedEntry,
        server: tokio::task::JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    async fn fixture(script: &str, extra: Option<&str>) -> Fixture {
        let source = tempfile::tempdir().unwrap();
        executable(source.path(), script);
        let archive = source.path().join("fixture.tgz");
        let mut tar = std::process::Command::new("tar");
        tar.env("COPYFILE_DISABLE", "1")
            .arg("--format=ustar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(source.path())
            .arg("plugin.sh");
        if let Some(extra) = extra {
            std::fs::write(source.path().join("payload.sh"), extra).unwrap();
            tar.arg("payload.sh");
        }
        assert!(tar.status().unwrap().success());
        let bytes = std::fs::read(archive).unwrap();
        let hash = format!("sha256:{:x}", Sha256::digest(&bytes));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/fixture.tgz", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            socket.read(&mut request).await.unwrap();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&bytes).await.unwrap();
        });
        let approved = ApprovedEntry::new(
            "demo".into(),
            serde_json::from_value(serde_json::json!({
                "ecosystem": "gray-native", "kind": "plugin", "version": "catalog-1",
                "source": {"type": "tarball", "url": url}, "hash": hash
            }))
            .unwrap(),
        )
        .unwrap();
        Fixture {
            home: tempfile::tempdir().unwrap(),
            approved,
            server,
        }
    }

    fn assert_clean(home: &Path) {
        assert!(!home.join("plugins/lock.json").exists());
        assert!(!home.join("plugins/demo").exists());
        for item in std::fs::read_dir(home.join("plugins")).unwrap() {
            let path = item.unwrap().path();
            assert_eq!(path.file_name().unwrap(), "tmp");
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn preparation_keeps_exact_manifest_and_cleans_up_when_dropped() {
        let fixture = fixture(COUNTER, None).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let prepared = install::prepare(
            fixture.home.path(),
            fixture.approved.clone(),
            CancellationToken::new(),
            Some(tx),
        )
        .await
        .unwrap();
        assert_eq!(prepared.package.approved().entry().version, "catalog-1");
        assert_eq!(prepared.manifest.raw["name"], "runtime-demo");
        assert_eq!(
            prepared.manifest.executable_relative,
            Path::new("plugin.sh")
        );
        assert_eq!(prepared.manifest.commands, ["/demo"]);
        assert_eq!(prepared.manifest.declared, ["host.ask"]);
        assert!(prepared.manifest.cli);
        assert_eq!(
            std::fs::read_to_string(prepared.package.root().join("plugin.sh.count")).unwrap(),
            "x"
        );
        let mut stages = Vec::new();
        while let Some(stage) = rx.recv().await {
            stages.push(stage);
        }
        assert_eq!(
            stages,
            [
                InstallStage::Downloading,
                InstallStage::Unpacking,
                InstallStage::Scanning,
                InstallStage::ReadingManifest
            ]
        );
        drop(prepared);
        assert_clean(fixture.home.path());
    }

    #[tokio::test]
    async fn dangerous_scan_blocks_before_any_executable_probe() {
        let marker = tempfile::tempdir().unwrap();
        let path = marker.path().join("probe-count");
        let script = format!("#!/bin/sh\nprintf x >> '{}'\n", path.display());
        let fixture = fixture(&script, Some("#!/bin/sh\nrm -rf /\n")).await;
        let error = install::prepare(
            fixture.home.path(),
            fixture.approved.clone(),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("scan blocked"));
        assert!(!path.exists());
        assert_clean(fixture.home.path());
    }

    #[tokio::test]
    async fn invalid_manifest_drops_staging_and_does_not_install() {
        let fixture = fixture("#!/bin/sh\nprintf 'not-json\\n'\n", None).await;
        let error = install::prepare(
            fixture.home.path(),
            fixture.approved.clone(),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("invalid plugin manifest JSON"));
        assert_clean(fixture.home.path());
    }

    #[tokio::test]
    async fn caution_findings_survive_preparation_for_consent() {
        let fixture = fixture(COUNTER, Some("# review .gray/auth.json\n")).await;
        let prepared = install::prepare(
            fixture.home.path(),
            fixture.approved.clone(),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(prepared.scan.verdict(), gray_plugin::scan::Verdict::Caution);
        assert!(prepared.scan.render().contains("credential_store_read"));
        drop(prepared);
        assert_clean(fixture.home.path());
    }

    #[tokio::test]
    async fn cancellation_during_staged_probe_cleans_up_before_return() {
        let marker = tempfile::tempdir().unwrap();
        let pid = marker.path().join("pid");
        let script = format!("#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n", pid.display());
        let fixture = fixture(&script, None).await;
        let home = fixture.home.path().to_owned();
        let approved = fixture.approved.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let task =
            tokio::spawn(async move { install::prepare(&home, approved, token, None).await });
        wait_for_file(&pid).await;
        cancel.cancel();
        assert!(task.await.unwrap().is_err());
        assert_reaped(&pid);
        assert_clean(fixture.home.path());
    }
}
