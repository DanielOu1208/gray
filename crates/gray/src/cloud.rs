//! `gray cloud` — the Grayling seat's hosted machine.
//!
//! Every seat provisions one Maritime micro-VM running gray, billed to the
//! seat's portal credits. Provision is idempotent server-side — calling it on
//! every command returns the same agent — so the module never has to cache an
//! agent id, only the chat thread's conversation_id under $GRAY_HOME.

use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Default, Deserialize)]
struct StatusOut {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    suspended: bool,
}

#[derive(Debug, Default, Deserialize)]
struct ProvisionOut {
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatOut {
    #[serde(default)]
    response: String,
    #[serde(default)]
    conversation_id: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct CloudSession {
    #[serde(default)]
    conversation_id: Option<String>,
}

fn session_path() -> anyhow::Result<PathBuf> {
    Ok(crate::setup::gray_home()?.join("cloud-session.json"))
}

fn load_session_at(path: &Path) -> Option<CloudSession> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

fn save_session(session: &CloudSession) -> anyhow::Result<()> {
    crate::setup::catalog::save_private_json(
        &session_path()?,
        &serde_json::to_value(session).map_err(|e| anyhow::anyhow!("{e}"))?,
    )
}

fn require_token() -> anyhow::Result<String> {
    crate::account::load_token()
        .ok_or_else(|| anyhow::anyhow!("not logged in — run `gray login` first"))
}

async fn post<T: for<'de> Deserialize<'de>>(
    client: &reqwest::Client,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> anyhow::Result<(reqwest::StatusCode, T)> {
    let url = crate::account::endpoint(path)?;
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text)
        && let Some(msg) = parsed.get("error").and_then(|v| v.as_str())
        && !status.is_success()
    {
        anyhow::bail!("{msg}");
    }
    if !status.is_success() {
        anyhow::bail!("registry returned {status}");
    }
    Ok((status, serde_json::from_str(&text)?))
}

async fn provision(client: &reqwest::Client, token: &str) -> anyhow::Result<ProvisionOut> {
    let (_, out): (_, ProvisionOut) =
        post(client, token, "agent/provision", &serde_json::json!({})).await?;
    Ok(out)
}

async fn fetch_status(client: &reqwest::Client, token: &str) -> anyhow::Result<StatusOut> {
    let url = crate::account::endpoint("agent/status")?;
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach {url}: {e}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(crate::account::error_message(resp).await);
    }
    Ok(resp.json().await?)
}

async fn chat(
    client: &reqwest::Client,
    token: &str,
    message: &str,
    conversation_id: Option<&str>,
) -> anyhow::Result<ChatOut> {
    let (_, out): (_, ChatOut) = post(
        client,
        token,
        "agent/chat",
        &serde_json::json!({
            "message": message,
            "conversation_id": conversation_id,
        }),
    )
    .await?;
    Ok(out)
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()?)
}

/// `gray cloud` bare: make sure the seat's machine exists and report it.
/// `gray cloud <words>`: send a message and print the reply, keeping the
/// thread in cloud-session.json so follow-ups land in the same conversation.
pub async fn run_cloud(words: &[String]) -> anyhow::Result<()> {
    let token = require_token()?;
    let client = http_client()?;
    let message = words.join(" ").trim().to_string();

    if message.is_empty() {
        let prov = provision(&client, &token).await?;
        let status = fetch_status(&client, &token).await.unwrap_or(StatusOut {
            status: prov.status.clone(),
            suspended: false,
        });
        if status.suspended {
            println!("your Gray machine is suspended — check your seat at /plans");
            return Ok(());
        }
        match status.status.as_deref() {
            Some("active") => {
                println!("your Gray machine is up — chat with it: gray cloud <message>")
            }
            Some(s) => println!("your Gray machine is {s} — first boot takes a couple of minutes"),
            None => println!("your Gray machine is provisioned"),
        }
        return Ok(());
    }

    // Provision runs before every chat: it is idempotent, so a seat that was
    // refunded-then-repaid recovers without a separate status dance.
    provision(&client, &token).await?;

    let session = session_path()
        .ok()
        .and_then(|p| load_session_at(&p))
        .unwrap_or_default();
    let out = chat(
        &client,
        &token,
        &message,
        session.conversation_id.as_deref(),
    )
    .await?;
    if out.conversation_id.is_some() || session.conversation_id.is_some() {
        save_session(&CloudSession {
            conversation_id: out.conversation_id.or(session.conversation_id),
        })?;
    }
    println!("{}", out.response.trim_end());
    Ok(())
}
