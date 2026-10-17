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

async fn connect_with_code(code: Option<&str>) -> anyhow::Result<String> {
    let code = match code.and_then(crate::account::normalize_code) {
        Some(code) => code,
        None => {
            println!("Connect this terminal to your Gray Cloud machine.\n");
            println!("{}\n", crate::account::login_instructions());
            crate::account::prompt_for_code().ok_or_else(|| {
                anyhow::anyhow!("no enrollment code given — run `gray cloud connect <code>`")
            })?
        }
    };
    let (token, account) = crate::account::login_with_code(&code).await?;
    let previous = crate::account::load_token().filter(|previous| *previous != token);
    crate::account::save_token(&token)?;
    if let Some(previous) = previous {
        crate::account::revoke_token(&previous).await;
    }
    println!("connected as {}", account.label());
    Ok(token)
}

async fn require_token() -> anyhow::Result<String> {
    match crate::account::load_token() {
        Some(token) => Ok(token),
        None => connect_with_code(None).await,
    }
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

pub async fn run_cloud(words: &[String]) -> anyhow::Result<()> {
    let client = http_client()?;
    let message = words.join(" ").trim().to_string();
    let connect = words
        .first()
        .is_some_and(|word| word.eq_ignore_ascii_case("connect"));
    let token = if connect {
        connect_with_code(words.get(1).map(String::as_str)).await?
    } else {
        require_token().await?
    };

    if message.is_empty() || connect {
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
