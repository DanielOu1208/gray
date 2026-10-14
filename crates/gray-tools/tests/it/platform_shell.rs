use std::time::Duration;

use gray_core::agent::{Tool, ToolContext};
use gray_tools::BashTool;
use serde_json::json;

#[tokio::test]
async fn shell_preserves_command_cwd_and_nonzero_exit() {

    let dir = tempfile::Builder::new()
        .prefix("gray shell café ")
        .tempdir()
        .unwrap();
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        ..ToolContext::default()
    };
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        BashTool::default().execute(
            &ctx,
            json!({"command": "printf 'exact bytes' > result.txt; printf 'hello'; exit 7"}),
        ),
    )
    .await
    .expect("shell execution must be bounded");
    assert!(!out.is_error, "{}", out.content);
    assert!(out.content.starts_with("exit 7"), "{}", out.content);
    assert!(out.content.contains("hello"), "{}", out.content);
    assert_eq!(
        std::fs::read(dir.path().join("result.txt")).unwrap(),
        b"exact bytes"
    );
}

async fn stopped_tree_cannot_write_later(cancel: bool) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext {
        cwd: dir.path().to_path_buf(),
        ..ToolContext::default()
    };
    let token = ctx.cancel.clone();
    let cancellation = tokio::spawn(async move {
        if cancel {
            tokio::time::sleep(Duration::from_secs(1)).await;
            token.cancel();
        }
    });
    let tool = BashTool::default();
    let out = tokio::time::timeout(
        Duration::from_secs(12),
        tool.execute(
            &ctx,
            json!({

                "command": "printf 'ready\\n'; (sleep 4; printf escaped > escaped.txt) & wait",
                "timeout": if cancel { 10 } else { 1 }
            }),
        ),
    )
    .await
    .expect("timeout/cancellation must not hang waiting for surviving children");
    cancellation.await.unwrap();
    assert!(!out.is_error, "{}", out.content);
    assert!(
        out.content.contains("ready"),
        "partial output lost: {}",
        out.content
    );
    assert!(
        out.content
            .starts_with(if cancel { "cancelled" } else { "still running" }),
        "{}",
        out.content
    );
    if !cancel {

        let stop = out
            .content
            .split("Stop: `")
            .nth(1)
            .and_then(|s| s.split('`').next())
            .expect("the notice advertises a stop command")
            .to_string();
        let stopped = tool.execute(&ctx, json!({"command": stop})).await;
        assert!(
            stopped.content.starts_with("stopped job "),
            "{}",
            stopped.content
        );
    }

    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !dir.path().join("escaped.txt").exists(),
        "descendant survived {} and wrote after termination: {}",
        if cancel {
            "cancellation"
        } else {
            "the advertised stop"
        },
        out.content
    );
}

#[tokio::test]
async fn timeout_stops_descendants() {
    stopped_tree_cannot_write_later(false).await;
}

#[tokio::test]
async fn cancellation_stops_descendants() {
    stopped_tree_cannot_write_later(true).await;
}
