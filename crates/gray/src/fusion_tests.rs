//! `synthesize_row` is pure; the tool-side paths run against tempdir homes
//! (`provider_models.json`, `config.json`, `auth.json`) and `#!/bin/sh`
//! fake grays — the NDJSON contract is the real `print.rs` one. Scripts
//! and process groups make this file unix-only.
#![cfg(unix)]

use super::*;
use crate::setup::variants::RowShape;

/// `GRAY_HOME` and [`ENV_SIDEKICK`] are process-global: env-touching tests
/// take a lock rather than repointing each other's home mid-run.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn pair(id: &str, name: &str) -> CachedRow {
    CachedRow::Pair(id.to_string(), name.to_string())
}

/// A declared composite row (any slots makes it one).
fn composite(id: &str) -> CachedRow {
    CachedRow::Full(CachedModel {
        id: id.to_string(),
        name: id.to_string(),
        reasoning_efforts: Vec::new(),
        variants: vec![ModelVariant {
            id: "wire".to_string(),
            effort: None,
            fast: false,
            parts: BTreeMap::from([
                ("lead".to_string(), "a".to_string()),
                ("sidekick".to_string(), "b".to_string()),
            ]),
        }],
        slots: vec![ModelSlot {
            key: "lead".to_string(),
            label: "Lead".to_string(),
            options: vec![SlotOption {
                id: "a".to_string(),
                name: "a".to_string(),
            }],
        }],
        declared: true,
        price_cliff_tokens: None,
    })
}

fn lists(entries: &[(&str, Vec<CachedRow>)]) -> BTreeMap<String, Vec<CachedRow>> {
    entries
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn shape(model: &CachedModel) -> RowShape {
    RowShape {
        variants: model.variants.clone(),
        slots: model.slots.clone(),
    }
}

// --- row synthesis ---------------------------------------------------------

#[test]
fn cross_product_variants_with_qualified_sidekicks() {
    let own = vec![pair("opus", "Opus 5"), pair("sonnet", "Sonnet 4")];
    let all = lists(&[
        ("http://lead.test/v1", own.clone()),
        ("plugin:devsub:dev", vec![pair("swe-2", "SWE-2")]),
        ("http://sk.test/v1", vec![pair("haiku", "Haiku")]),
    ]);
    let m = synthesize_row("http://lead.test/v1", &own, &all).unwrap();
    assert_eq!(m.id, "fusion");
    assert_eq!(m.name, "Fusion");
    // Lead options are the provider's own rows, bare ids.
    let leads: Vec<&str> = m.slots[0].options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(leads, ["opus", "sonnet"]);
    assert_eq!(m.slots[0].key, "lead");
    assert_eq!(m.slots[1].key, "sidekick");
    // Sidekick options are qualified `provider:model`, own provider first:
    // the custom-endpoint slug for HTTP lists, the provider id for plugins.
    let sks: Vec<&str> = m.slots[1].options.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(
        sks,
        [
            "custom-lead-test:opus",
            "custom-lead-test:sonnet",
            "dev:swe-2",
            "custom-sk-test:haiku"
        ]
    );
    // One variant per pairing; the wire id is the lead's row id.
    assert_eq!(m.variants.len(), 2 * sks.len());
    let v = m
        .variants
        .iter()
        .find(|v| v.parts["lead"] == "opus" && v.parts["sidekick"] == "dev:swe-2")
        .unwrap();
    assert_eq!(v.id, "opus");
    assert_eq!(v.effort, None);
    assert!(!v.fast);
}

#[test]
fn a_native_composite_row_blocks_synthesis() {
    let own = vec![pair("a", "A"), composite("fusion")];
    assert!(synthesize_row("http://x.test/v1", &own, &BTreeMap::new()).is_none());
    // Any composite does — not just one named `fusion`.
    let own = vec![composite("trio"), pair("a", "A")];
    assert!(synthesize_row("http://x.test/v1", &own, &BTreeMap::new()).is_none());
}

#[test]
fn no_rows_means_no_fusion() {
    assert!(synthesize_row("http://x.test/v1", &[], &BTreeMap::new()).is_none());
}

#[test]
fn variant_resolution_picks_the_lead_wire_id() {
    let own = vec![pair("opus", "Opus 5"), pair("sonnet", "Sonnet 4")];
    let all = lists(&[("http://lead.test/v1", own.clone())]);
    let m = synthesize_row("http://lead.test/v1", &own, &all).unwrap();
    let shape = shape(&m);
    let parts = Parts::from([
        ("lead".to_string(), "sonnet".to_string()),
        (
            "sidekick".to_string(),
            "custom-lead-test:sonnet".to_string(),
        ),
    ]);
    assert!(shape.serves(&parts));
    // Effort is free-floating: every variant wins regardless of level.
    let v = crate::setup::variants::resolve_variant(&shape.variants, Some("high"), false, &parts)
        .unwrap();
    assert_eq!(v.id, "sonnet");
    // Unserved parts settle on the catalog default (first variant).
    assert_eq!(shape.settle_parts(None), shape.variants[0].parts);
}

#[test]
fn the_footer_label_is_fusion_lead_effort_times_sidekick() {
    let own = vec![pair("opus", "Opus 5"), pair("haiku", "Haiku")];
    let all = lists(&[("http://lead.test/v1", own.clone())]);
    let m = synthesize_row("http://lead.test/v1", &own, &all).unwrap();
    let shape = shape(&m);
    let parts = Parts::from([
        ("lead".to_string(), "opus".to_string()),
        ("sidekick".to_string(), "custom-lead-test:haiku".to_string()),
    ]);
    let segs =
        crate::setup::variants::composite_label_parts(&shape, "Fusion", &parts, Some("high"))
            .unwrap();
    let flat: String = segs.iter().map(|(t, _)| t.clone()).collect();
    assert_eq!(flat, "Fusion · Opus 5 High × Haiku");
}

// --- the marker ------------------------------------------------------------

#[test]
fn the_marker_is_a_colon_in_the_sidekick_part() {
    let local = Parts::from([("sidekick".to_string(), "custom-x:m".to_string())]);
    assert!(is_local_parts(&local));
    // Native composite parts are bare ids — no colon, no tool.
    let native = Parts::from([("sidekick".to_string(), "swe-2".to_string())]);
    assert!(!is_local_parts(&native));
    assert!(!is_local_parts(&Parts::new()));
}

// --- sidekick resolution ----------------------------------------------------

/// Saves and restores mutated env vars on drop.
struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn apply(vars: &[(&'static str, Option<&str>)]) -> Self {
        let saved = vars
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in vars {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
        Self(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

fn write(home: &Path, name: &str, body: &str) {
    std::fs::write(home.join(name), body).unwrap();
}

#[test]
fn an_http_sidekick_resolves_to_base_url_and_stored_key() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    write(
        dir.path(),
        "provider_models.json",
        r#"{"http://sk.test/v1": [["sk-1", "Side"]], "http://lead.test/v1": [["opus", "Opus"]]}"#,
    );
    write(dir.path(), "auth.json", r#"{"custom:sk.test": "KEY42"}"#);
    let spec = resolve_sidekick(dir.path(), "custom-sk-test:sk-1").unwrap();
    assert_eq!(spec.model, "sk-1");
    let env: BTreeMap<&str, &str> = spec
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(env["GRAY_BASE_URL"], "http://sk.test/v1");
    assert_eq!(env["GRAY_API_KEY"], "KEY42");
    // The lead's plugin identity is explicitly cleared.
    assert_eq!(env["GRAY_PROVIDER_ID"], "");
    assert_eq!(env["GRAY_CREDENTIAL_SOURCE"], "");
    assert_eq!(env["GRAY_AUTH_REF"], "");
}

#[test]
fn a_keyless_sidekick_clears_the_leads_key() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[
        ("GRAY_HOME", Some(dir.path().to_str().unwrap())),
        ("GRAY_API_KEY", Some("LEAD-KEY")),
    ]);
    write(
        dir.path(),
        "provider_models.json",
        r#"{"http://sk.test/v1": [["sk-1", "Side"]]}"#,
    );
    let spec = resolve_sidekick(dir.path(), "custom-sk-test:sk-1").unwrap();
    assert!(spec.env_remove.iter().any(|k| k == "GRAY_API_KEY"));
    assert!(!spec.env.iter().any(|(k, _)| k == "GRAY_API_KEY"));
}

#[test]
fn an_unknown_provider_token_resolves_to_nothing() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    write(
        dir.path(),
        "provider_models.json",
        r#"{"http://sk.test/v1": [["sk-1", "Side"]]}"#,
    );
    assert!(resolve_sidekick(dir.path(), "nope:sk-1").is_none());
    // A plugin token without an installed provider also fails cleanly.
    assert!(resolve_sidekick(dir.path(), "dev:sk-1").is_none());
}

// --- injection condition -----------------------------------------------------

#[test]
fn handoff_exists_exactly_while_a_local_fusion_is_selected() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[
        ("GRAY_HOME", Some(dir.path().to_str().unwrap())),
        (ENV_SIDEKICK, None),
    ]);
    write(
        dir.path(),
        "provider_models.json",
        r#"{"http://lead.test/v1": [["opus", "Opus"],["haiku", "Haiku"]],
            "http://sk.test/v1": [["sk-1", "Side"]]}"#,
    );
    write(
        dir.path(),
        "config.json",
        r#"{"model": "fusion", "base_url": "http://lead.test/v1",
            "model_parts": {"lead": "opus", "sidekick": "custom-sk-test:sk-1"}}"#,
    );
    assert_eq!(local_sidekick().as_deref(), Some("custom-sk-test:sk-1"));
    let plugin = FusionPlugin::default();
    let tools = plugin.tools();
    // Both wire aliases resolve to the same tool: `handoff` is the
    // canonical name, `native__handoff` the one Devin-trained leads emit.
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].def().name, "handoff");
    assert_eq!(tools[1].def().name, "native__handoff");

    // A native (bare-id) parts selection resolves against the lead's own
    // provider — the bare option id qualified under its token — and gets
    // the tool too: no provider runs a client-side sidekick for you.
    write(
        dir.path(),
        "config.json",
        r#"{"model": "fusion", "base_url": "http://lead.test/v1",
            "model_parts": {"lead": "opus", "sidekick": "haiku"}}"#,
    );
    assert_eq!(
        local_sidekick().as_deref(),
        Some(format!("{}:haiku", provider_token("http://lead.test/v1")).as_str())
    );
    assert_eq!(plugin.tools().len(), 2);

    // Nor inside a sidekick's own process.
    let _env2 = EnvGuard::apply(&[(ENV_SIDEKICK, Some("1"))]);
    write(
        dir.path(),
        "config.json",
        r#"{"model": "fusion", "base_url": "http://lead.test/v1",
            "model_parts": {"lead": "opus", "sidekick": "custom-sk-test:sk-1"}}"#,
    );
    assert!(local_sidekick().is_none());
    assert!(plugin.tools().is_empty());
}

// --- the child runner --------------------------------------------------------

/// An executable `#!/bin/sh` script in `dir` named `name`.
fn fake_gray(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn tool(bin: PathBuf, home: &Path, qualified: &str) -> HandoffTool {
    HandoffTool {
        name: "handoff",
        qualified: qualified.to_string(),
        home: home.to_path_buf(),
        bin,
        timeout: Duration::from_secs(10),
        gate: Arc::new(tokio::sync::Mutex::new(())),
    }
}

fn ctx(home: &Path) -> ToolContext {
    ToolContext {
        cwd: home.to_path_buf(),
        ..Default::default()
    }
}

fn provider_file(home: &Path) {
    write(
        home,
        "provider_models.json",
        r#"{"http://sk.test/v1": [["sk-1", "Side"]]}"#,
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn first_call_pools_the_session_and_later_calls_resume_it() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    provider_file(dir.path());
    // The fake can't echo `$*` back in JSON (the contract carries raw
    // newlines), so it logs args to a file the test inspects instead.
    let bin = fake_gray(
        dir.path(),
        "gray-sess",
        r#"printf '%s' "$*" >> "$(dirname "$0")/args.txt"
printf '%s\n' '{"type":"result","text":"ok","session_id":"sid-9"}'
"#,
    );
    let t = tool(bin, dir.path(), "custom-sk-test:haiku");
    let args = serde_json::json!({"task": "do the thing"});
    let out = t.execute(&ctx(dir.path()), args.clone()).await;
    assert!(!out.is_error, "{}", out.content);
    let args_log = || std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
    let logged = args_log();
    assert!(logged.contains("--model haiku"), "{logged}");
    assert!(!logged.contains("--session"), "{logged}");
    // A fresh session opens with the sidekick contract once.
    assert_eq!(logged.matches("<sidekick_contract>").count(), 1);
    // The reported session id is pooled for next time.
    let files: Vec<_> = std::fs::read_dir(dir.path().join("fusion"))
        .unwrap()
        .collect();
    assert_eq!(files.len(), 1);
    assert_eq!(
        std::fs::read_to_string(files[0].as_ref().unwrap().path()).unwrap(),
        "sid-9\n"
    );
    // Second call resumes it — and does not re-brief the contract.
    let out = t.execute(&ctx(dir.path()), args).await;
    assert!(!out.is_error, "{}", out.content);
    let logged = args_log();
    assert!(logged.contains("--session sid-9"), "{logged}");
    assert_eq!(logged.matches("<sidekick_contract>").count(), 1);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_dead_pooled_session_retries_fresh_once() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    provider_file(dir.path());
    // Fails only when a session flag is passed — the stored id is dead.
    let bin = fake_gray(
        dir.path(),
        "gray-dead",
        r#"case "$*" in
  *"--session"*) printf '%s\n' '{"type":"error","message":"no such session"}' ;;
  *) printf '%s\n' '{"type":"result","text":"fresh ok","session_id":"sid-2"}' ;;
esac
"#,
    );
    let t = tool(bin, dir.path(), "custom-sk-test:sk-1");
    std::fs::create_dir_all(dir.path().join("fusion")).unwrap();
    let stale = session_file(dir.path(), "custom-sk-test:sk-1");
    std::fs::write(&stale, "sid-dead\n").unwrap();
    let out = t
        .execute(&ctx(dir.path()), serde_json::json!({"task": "hi"}))
        .await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "fresh ok");
    assert_eq!(std::fs::read_to_string(&stale).unwrap(), "sid-2\n");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_timeout_is_an_error_and_kills_the_group() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    provider_file(dir.path());
    let bin = fake_gray(dir.path(), "gray-hang", "exec sleep 30");
    let mut t = tool(bin, dir.path(), "custom-sk-test:sk-1");
    t.timeout = Duration::from_millis(300);
    let start = std::time::Instant::now();
    let out = t
        .execute(&ctx(dir.path()), serde_json::json!({"task": "hi"}))
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("timed out"), "{}", out.content);
    assert!(start.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_child_error_row_is_a_tool_error() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    provider_file(dir.path());
    let bin = fake_gray(
        dir.path(),
        "gray-err",
        r#"printf '%s\n' '{"type":"error","code":"no_key","message":"missing key"}'
"#,
    );
    let t = tool(bin, dir.path(), "custom-sk-test:sk-1");
    let out = t
        .execute(&ctx(dir.path()), serde_json::json!({"task": "hi"}))
        .await;
    assert!(out.is_error);
    assert!(out.content.contains("missing key"), "{}", out.content);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_empty_task_is_rejected() {
    let _lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let _env = EnvGuard::apply(&[("GRAY_HOME", Some(dir.path().to_str().unwrap()))]);
    provider_file(dir.path());
    let t = tool(dir.path().join("gray"), dir.path(), "custom-sk-test:sk-1");
    let out = t.execute(&ctx(dir.path()), serde_json::json!({})).await;
    assert!(out.is_error);
    assert!(out.content.contains("task"), "{}", out.content);
}
