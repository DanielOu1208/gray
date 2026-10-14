use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

pub async fn run_prompt_child(cwd: &Path, prompt: &str) -> Value {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return json!({"error": format!("host/run: current_exe: {e}")}),
    };
    let mut child = match tokio::process::Command::new(exe)
        .arg("-p")
        .arg(prompt)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("host/run spawn: {e}")}),
    };

    const MAX_CHILD_OUT: u64 = 256 * 1024;
    let mut piped = child.stdout.take();
    let drain = tokio::spawn(async move {
        let mut v = Vec::new();
        let Some(ref mut o) = piped else {
            return v;
        };
        {
            let mut capped = (&mut *o).take(MAX_CHILD_OUT + 1);
            let _ = capped.read_to_end(&mut v).await;
        }

        let mut sink = tokio::io::sink();
        let _ = tokio::io::copy(o, &mut sink).await;
        v
    });
    let status = match tokio::time::timeout(Duration::from_secs(28), child.wait()).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return json!({"error": format!("host/run wait: {e}")}),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;

            let _ = tokio::time::timeout(Duration::from_secs(2), drain).await;
            return json!({"error": "host/run timed out after 28s"});
        }
    };
    let bytes = match tokio::time::timeout(Duration::from_secs(2), drain).await {
        Ok(Ok(v)) => v,
        _ => Vec::new(),
    };
    if !status.success() {
        return json!({"error": format!("host/run: gray -p exited {status}")});
    }
    let text = String::from_utf8_lossy(&bytes).trim().to_string();
    if text.is_empty() {
        json!({"error": "host/run: empty reply"})
    } else {
        json!({"text": text})
    }
}
