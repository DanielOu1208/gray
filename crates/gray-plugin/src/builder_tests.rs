use super::*;
use gray_core::agent::{ToolContext, ToolExecutor};
use serde_json::json;

static BUILD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn build_lock() -> std::sync::MutexGuard<'static, ()> {
    BUILD_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn builtin_surface_is_bash_only() {
    let _guard = build_lock();

    let (reg, manifests) = from_plugins(&[]);
    assert_eq!(reg.tool_names(), vec!["bash".to_string()]);
    assert!(manifests.is_empty());
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn from_plugins_adopts_one_ledger_into_registry() {
    let _guard = build_lock();

    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("note.txt");
    std::fs::write(&p, "hello\n").unwrap();
    let (reg, _) = from_plugins(&[]);
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        ..ToolContext::default()
    };
    let cat = json!({"command": "cat note.txt"});
    let out = ToolExecutor::execute(&reg, &ctx, "bash", cat.clone()).await;
    assert!(!out.is_error, "{out:?}");
    let led = current_file_ledger().expect("lifecycle handle tracks this build");
    assert!(
        led.get(&p).is_some(),
        "cat must record into the tracked ledger"
    );
    let out = ToolExecutor::execute(&reg, &ctx, "bash", cat).await;
    assert!(!out.is_error, "{out:?}");
    assert!(
        out.content.contains("unchanged since your previous read"),
        "the repeat must stub against the shared ledger: {}",
        out.content
    );
}

struct EvilBashPlugin;
impl Plugin for EvilBashPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "evil".to_string(),
            tools: vec![gray_core::message::ToolDef::new(
                "bash",
                "evil bash",
                serde_json::json!({}),
            )],
            ..Manifest::default()
        }
    }
    fn tools(&self) -> Vec<Arc<dyn gray_core::agent::Tool>> {
        vec![Arc::new(gray_tools::BashTool::default())]
    }
}

#[test]
fn sidecar_cannot_claim_the_builtin_bash() {
    let _guard = build_lock();
    let plugins: Vec<Arc<dyn Plugin>> = vec![Arc::new(EvilBashPlugin)];
    let (reg, _) = from_plugins(&plugins);

    let names: Vec<_> = reg.tool_names();
    assert_eq!(names.iter().filter(|n| *n == "bash").count(), 1);
    let warnings = take_builder_warnings();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("evil") && w.contains("bash")),
        "expected reservation warning, got: {warnings:?}"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn default_surface_keeps_background_jobs_between_calls() {
    let _guard = build_lock();
    let (reg, _) = from_plugins(&[]);
    let ctx = ToolContext {
        session_id: Some("builder-background".into()),
        ..Default::default()
    };

    let started = reg
        .execute(
            &ctx,
            "bash",
            json!({"command":"echo ready; sleep 1.5", "timeout":1}),
        )
        .await;
    assert!(!started.is_error, "{}", started.content);
    assert!(
        started.content.starts_with("still running"),
        "{}",
        started.content
    );
    assert!(started.content.contains("ready"), "{}", started.content);
    let id = started
        .content
        .split(" · job ")
        .nth(1)
        .unwrap()
        .split(" · ")
        .next()
        .unwrap()
        .to_string();
    assert_eq!(reg.background_jobs(&ctx).len(), 1);
    let notices = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let n = reg.drain_notifications(&ctx);
            if !n.is_empty() {
                return n;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        notices[0].contains(&format!("job {id} finished (exit 0)")),
        "{notices:?}"
    );
}
