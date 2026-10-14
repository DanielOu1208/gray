use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn json_turn_exits_while_stdin_stays_open() {
    let root = tempfile::tempdir().unwrap();
    let (url, server) = serve_one_reply().await;
    let (status, stdout) =
        run_with_stdin_held_open(root.path(), &url, &["-p", "reply ok", "--json"]).await;
    server.abort();
    assert!(status.success(), "exit {status:?}, stdout: {stdout}");
    assert!(stdout.contains("\"type\":\"result\""), "{stdout}");
}

#[tokio::test]
async fn json_failure_exits_one_while_stdin_stays_open() {
    let root = tempfile::tempdir().unwrap();

    let (status, stdout) = run_with_stdin_held_open(
        root.path(),
        "http://127.0.0.1:9/v1",
        &["-p", "reply ok", "--json", "--session", "no-such-session"],
    )
    .await;
    assert_eq!(status.code(), Some(1), "stdout: {stdout}");
    assert!(stdout.contains("\"type\":\"error\""), "{stdout}");
}

async fn run_with_stdin_held_open(
    home: &Path,
    base_url: &str,
    args: &[&str],
) -> (ExitStatus, String) {
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_gray"))
        .current_dir(home)
        .env("GRAY_HOME", home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("GRAY_NO_UPDATE_CHECK", "1")
        .env("GRAY_STDIN_STEER", "1")
        .args([
            "--model",
            "cwd-test",
            "--api-key",
            "test-only",
            "--base-url",
            base_url,
        ])
        .args(["--context-window", "128000"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let _held_stdin = child.stdin.take();
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("gray must exit once its turn is done, with stdin still open")
        .unwrap();
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

async fn serve_one_reply() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let body_start = loop {
            let mut chunk = [0; 4096];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "request ended before headers");
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(i) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
        };
        let headers = String::from_utf8_lossy(&bytes[..body_start]).to_lowercase();
        let length = headers
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .unwrap()
            .trim()
            .parse::<usize>()
            .unwrap();
        while bytes.len() < body_start + length {
            let mut chunk = [0; 4096];
            let n = stream.read(&mut chunk).await.unwrap();
            assert!(n > 0, "request ended before body");
            bytes.extend_from_slice(&chunk[..n]);
        }
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    (url, server)
}
