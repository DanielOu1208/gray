//! Local fusion: a synthesized `fusion` composite row for providers that
//! don't declare one natively, plus the `handoff` tool that runs its
//! sidekick half.
//!
//! Devin's upstream fusion is provider-native: the catalog declares the
//! composite row and the server runs both halves. Every other provider
//! gets the same UX locally — a row synthesized from the cached model
//! lists ([`synthesize_row`]), a lead that streams as its own model id
//! (the variant id IS the lead row id; effort rides as the ordinary
//! request parameter), and a persistent sidekick the lead drives through
//! a `handoff` tool backed by `gray -p … --json` children.
//!
//! Self-describing marker: a local sidekick option id is always a
//! qualified `provider:model` — the ':' a native composite's parts never
//! carry (upstream slot options are bare catalog ids). [`is_local_parts`]
//! is the one check the tool gate keys on, so native fusion (where the
//! provider itself orchestrates) never gains a client-side sidekick.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gray_core::agent::{Tool, ToolContext, ToolOutput};
use gray_core::message::ToolDef;
use gray_plugin::{Manifest, ModelSlot, ModelVariant, Plugin, SlotOption};

use crate::setup::variants::{CachedModel, CachedRow, Parts};

/// The synthesized row's id — provider-scoped (`provider_models.json`
/// keeps one list per provider), so it never clashes with a native
/// composite on another provider.
pub(crate) const ROW_ID: &str = "fusion";
/// Env the sidekick child runs under. Its presence suppresses `handoff`
/// injection so a sidekick cannot chain sidekicks of its own.
pub(crate) const ENV_SIDEKICK: &str = "GRAY_FUSION_SIDEKICK";

/// Slot-option caps bound the cross product (leads × sidekicks = variant
/// count; each variant is two strings and a two-entry map).
const MAX_LEADS: usize = 60;
const MAX_SIDEKICKS: usize = 60;
/// Hard stop on one handoff; the whole process group dies with it.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(600);
/// One NDJSON line, capped like the gateway reader (1 MiB).
const LINE_CAP: usize = 1024 * 1024;
/// stderr is only ever an error hint; the last 2 KiB is plenty.
const STDERR_TAIL: usize = 2 * 1024;
/// A dead child reaps instantly; the bound covers a failed group kill.
const REAP_GRACE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Row synthesis
// ---------------------------------------------------------------------------

/// The `provider` half of a qualified sidekick id for one
/// `provider_models.json` key. A plugin list (`plugin:<plugin>:<pid>`)
/// contributes the bare provider id; an HTTP list contributes the catalog
/// provider id whose base URL it is, else the custom-endpoint slug with
/// ':' flattened to '-' (the qualified id's only ':' stays the
/// provider/model separator — and therefore the local-fusion marker).
fn provider_token(list_key: &str) -> String {
    if let Some(pid) = list_key.strip_prefix("plugin:") {
        return pid.rsplit(':').next().unwrap_or(pid).to_string();
    }
    if let Ok(catalog) = crate::setup::load_catalog()
        && let Some((id, _)) = catalog
            .iter()
            .find(|(_, p)| crate::setup::normalize_custom_base_url(&p.base_url) == list_key)
    {
        return id.clone();
    }
    crate::setup::catalog::custom_key_id(list_key).replace(|c: char| !(c.is_ascii_alphanumeric() || c == '-'), "-")
}

/// A `provider_models.json` key's auth-store id: the catalog provider id
/// or the endpoint's `custom:<slug>` key (unsanitized — auth keys use the
/// raw id; only the qualified sidekick id flattens it).
fn provider_auth_pid(list_key: &str) -> String {
    if let Ok(catalog) = crate::setup::load_catalog()
        && let Some((id, _)) = catalog
            .iter()
            .find(|(_, p)| crate::setup::normalize_custom_base_url(&p.base_url) == list_key)
    {
        return id.clone();
    }
    crate::setup::catalog::custom_key_id(list_key)
}

/// The non-composite rows of `rows` as `(id, name)` pairs.
fn plain_rows(rows: &[CachedRow]) -> Vec<(String, String)> {
    rows.iter()
        .filter(|r| !matches!(r, CachedRow::Full(m) if !m.slots.is_empty()))
        .map(|r| r.pair())
        .collect()
}

/// The `fusion` row `list_key`'s catalog gets locally: `Lead` options are
/// its own non-composite rows, `Sidekick` options every provider's
/// non-composite models (its own first) qualified as `provider:model`.
/// Variants are the cross product, each `id` = the lead's row id — the
/// wire id a turn sends — with `effort: None` so the user's level rides
/// as the ordinary request parameter.
///
/// `own` is the provider's own row list (a fresh live list wins over the
/// stale disk copy under the same key); `lists` is every provider's
/// cached rows, `list_key`'s disk entry included — it is ignored in
/// favor of `own`. `None` when the provider already declares a composite
/// (native fusion wins) or has no rows to pair.
pub(crate) fn synthesize_row(
    list_key: &str,
    own: &[CachedRow],
    lists: &BTreeMap<String, Vec<CachedRow>>,
) -> Option<CachedModel> {
    if own
        .iter()
        .any(|r| matches!(r, CachedRow::Full(m) if !m.slots.is_empty()))
    {
        return None;
    }
    let leads = plain_rows(own);
    if leads.is_empty() {
        return None;
    }
    let mut sidekicks: Vec<SlotOption> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut push = |token: &str, row: &(String, String)| {
        let qualified = format!("{token}:{id}", id = row.0);
        if sidekicks.len() < MAX_SIDEKICKS && seen.insert(qualified.clone()) {
            sidekicks.push(SlotOption {
                id: qualified,
                name: row.1.clone(),
            });
        }
    };
    let own_token = provider_token(list_key);
    for row in &leads {
        push(&own_token, row);
    }
    // Plugin providers' rows before custom endpoints' — the native
    // catalogs are the likelier sidekicks when order caps the list.
    for (key, rows) in lists {
        if key == list_key || !key.starts_with("plugin:") {
            continue;
        }
        let token = provider_token(key);
        for row in &plain_rows(rows) {
            push(&token, row);
        }
    }
    for (key, rows) in lists {
        if key == list_key || key.starts_with("plugin:") {
            continue;
        }
        let token = provider_token(key);
        for row in &plain_rows(rows) {
            push(&token, row);
        }
    }
    debug_assert!(!sidekicks.is_empty(), "nonempty leads always qualify");
    let leads: Vec<SlotOption> = leads
        .into_iter()
        .take(MAX_LEADS)
        .map(|(id, name)| SlotOption { id, name })
        .collect();
    let variants: Vec<ModelVariant> = leads
        .iter()
        .flat_map(|lead| {
            sidekicks.iter().map(move |sk| ModelVariant {
                id: lead.id.clone(),
                effort: None,
                fast: false,
                parts: BTreeMap::from([
                    ("lead".to_string(), lead.id.clone()),
                    ("sidekick".to_string(), sk.id.clone()),
                ]),
            })
        })
        .collect();
    Some(CachedModel {
        id: ROW_ID.to_string(),
        name: "Fusion".to_string(),
        reasoning_efforts: Vec::new(),
        variants,
        slots: vec![
            ModelSlot {
                key: "lead".to_string(),
                label: "Lead".to_string(),
                options: leads,
            },
            ModelSlot {
                key: "sidekick".to_string(),
                label: "Sidekick".to_string(),
                options: sidekicks,
            },
        ],
        declared: false,
    })
}

/// Appends the synthesized fusion row to `rows` when `list_key`'s catalog
/// qualifies (the caller registers/shapes it like any other row). Never
/// persisted: it is derived from the cached lists at every read, so a
/// later catalog can't strand stale sidekick options on disk.
pub(crate) fn extend_with_fusion(home: &Path, list_key: &str, rows: &mut Vec<CachedRow>) {
    let mut lists = crate::setup::context::provider_model_lists(home);
    lists.insert(list_key.to_string(), rows.clone());
    if let Some(model) = synthesize_row(list_key, rows, &lists) {
        rows.push(CachedRow::Full(model));
    }
}

// ---------------------------------------------------------------------------
// The `handoff` tool
// ---------------------------------------------------------------------------

/// Whether `parts` picks a local-fusion sidekick — the ':' in a qualified
/// `provider:model` option id, which native composite parts never carry.
pub(crate) fn is_local_parts(parts: &Parts) -> bool {
    parts.get("sidekick").is_some_and(|s| s.contains(':'))
}

/// The qualified sidekick id of the live selection, when it is a local
/// fusion. Reads the saved config so a mid-session `/model` switch is
/// seen on the next turn without a rebuild (the plugin's `tools()` is
/// resolved per turn — see below). The env guard runs first: a sidekick's
/// own `gray` spawns with [`ENV_SIDEKICK`], so it can never offer
/// `handoff` — sidekicks cannot chain sidekicks.
fn local_sidekick() -> Option<String> {
    if std::env::var_os(ENV_SIDEKICK).is_some() {
        return None;
    }
    let path = crate::setup::saved_config_path().ok()?;
    let saved = crate::setup::load_saved_config_at(&path);
    let model = saved.model.as_deref()?;
    let list_key = if saved.credential_source == "plugin"
        && !saved.provider_id.is_empty()
        && !saved.auth_ref.is_empty()
    {
        crate::setup::catalog::plugin_models_key(&saved.provider_id)
    } else {
        crate::setup::normalize_custom_base_url(saved.base_url.as_deref()?)
    };
    // Loads (and registers) the row shapes the parts resolve against.
    let _ = crate::setup::context::load_provider_model_list(&list_key);
    let shape = crate::setup::variants::row_shape(model)?;
    if !shape.is_composite() {
        return None;
    }
    if !is_local_parts(&saved.model_parts) {
        return None;
    }
    saved.model_parts.get("sidekick").cloned()
}

/// Carries `handoff` as a protocol-1.3 plugin: registered on every
/// full-surface build so `LiveRegistry` resolves [`Self::tools`] per
/// turn — the tool exists exactly while the live selection is a local
/// fusion, appearing and dropping with `/model` and needing no rebuild.
pub(crate) struct FusionPlugin {
    /// Serializes calls to one sidekick: the session it continues is a
    /// single history, so concurrent handoffs would interleave it.
    gate: Arc<tokio::sync::Mutex<()>>,
}

impl Default for FusionPlugin {
    fn default() -> Self {
        Self {
            gate: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for FusionPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "fusion".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: Some("1.3".to_string()),
            ..Default::default()
        }
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let (Some(qualified), Ok(home)) = (local_sidekick(), crate::setup::gray_home()) else {
            return Vec::new();
        };
        let bin = std::env::var_os("GRAY_BIN")
            .map(PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .unwrap_or_else(|| PathBuf::from("gray"));
        vec![Arc::new(HandoffTool {
            qualified,
            home,
            bin,
            timeout: HANDOFF_TIMEOUT,
            gate: self.gate.clone(),
        })]
    }
}

/// `handoff {task, label?}`: runs the sidekick as a `gray -p --json`
/// child in the same `GRAY_HOME`, pooling the session id it reports so
/// the agent — and its context — persists across handoffs.
struct HandoffTool {
    /// `provider:model` from `model_parts.sidekick`.
    qualified: String,
    home: PathBuf,
    /// The gray executable: `GRAY_BIN`, else the running binary; a stub
    /// script in tests.
    bin: PathBuf,
    timeout: Duration,
    gate: Arc<tokio::sync::Mutex<()>>,
}

#[async_trait::async_trait]
impl Tool for HandoffTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "handoff".to_string(),
            description: concat!(
                "Delegate a task to your sidekick: a persistent collaborator agent that ",
                "keeps its context, history and runtime state across calls. Give it a ",
                "self-contained brief — it works autonomously with its own tools and ",
                "returns its final report. Use it for implementation, verification and ",
                "broad searches while you keep planning, review and user communication."
            )
            .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "Self-contained task brief for the sidekick."
                    },
                    "label": {
                        "type": "string",
                        "description": "Optional short label for the task."
                    }
                },
                "required": ["task"]
            }),
            // Transcripts show the brief as the arg preview.
            label: None,
            preview: Some("task".to_string()),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, ctx: &ToolContext, args: serde_json::Value) -> ToolOutput {
        let task = args
            .get("task")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(task) = task else {
            return ToolOutput::error("handoff: 'task' is required and must not be empty");
        };
        if let Some(label) = args
            .get("label")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            log::info!(target: "gray_tools", "handoff [{label}] → {}", self.qualified);
        } else {
            log::info!(target: "gray_tools", "handoff → {}", self.qualified);
        }
        let Some(spec) = resolve_sidekick(&self.home, &self.qualified) else {
            return ToolOutput::error(format!(
                "handoff: cannot resolve sidekick provider in '{}'",
                self.qualified
            ));
        };
        let file = session_file(&self.home, &self.qualified);
        let stored = std::fs::read_to_string(&file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let _permit = self.gate.lock().await;
        let mut out = self.run(&spec, task, stored.as_deref(), ctx).await;
        // A dead stored session turns the whole call into an error once —
        // the retry below runs without it rather than failing forever.
        if out.error.is_some() && stored.is_some() && !out.killed {
            let _ = std::fs::remove_file(&file);
            out = self.run(&spec, task, None, ctx).await;
        }
        match out.error {
            Some(e) => ToolOutput::error(format!("handoff failed: {e}")),
            None => {
                if let Some(sid) = out.session_id {
                    if let Some(parent) = file.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = std::fs::write(&file, format!("{sid}\n")) {
                        log::warn!("fusion: session file write failed: {e}");
                    }
                }
                if out.text.is_empty() {
                    ToolOutput::ok("handoff completed with no report")
                } else {
                    ToolOutput::ok(out.text)
                }
            }
        }
    }
}

/// Where a sidekick's pooled session id lives; the qualified id's hash
/// keeps the name unique per pairing and filesystem-safe.
fn session_file(home: &Path, qualified: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    qualified.hash(&mut h);
    home.join("fusion")
        .join(format!("sidekick-{:016x}.session", h.finish()))
}

/// The child's resolved identity: the bare model id for `--model` plus
/// the env that points `Config::resolve` at its provider connection —
/// plugin credentials ride `GRAY_PROVIDER_*`, HTTP ones
/// `GRAY_BASE_URL`/`GRAY_API_KEY`.
struct SidekickSpec {
    model: String,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
}

/// `provider:model` → the child's model and env. The provider token maps
/// back to a `provider_models.json` key ([`provider_token`]); from there
/// a plugin key resolves through the installed-provider cache and an
/// HTTP key through the catalog id / custom slug its auth key is stored
/// under. `None` when the token matches no known provider — a stale
/// part from a provider whose list has since gone.
fn resolve_sidekick(home: &Path, qualified: &str) -> Option<SidekickSpec> {
    let (token, model) = qualified.split_once(':')?;
    if token.is_empty() || model.is_empty() {
        return None;
    }
    let lists = crate::setup::context::provider_model_lists(home);
    let key = lists.keys().find(|k| provider_token(k) == token)?.clone();
    let mut spec = SidekickSpec {
        model: model.to_string(),
        env: Vec::new(),
        env_remove: Vec::new(),
    };
    if let Some(pid) = key.strip_prefix("plugin:") {
        let installed = crate::providers::ProviderRegistry::load_cached(home)
            .installed()
            .into_iter()
            .find(|p| p.provider_id() == pid)?;
        spec.env.push(("GRAY_PROVIDER_ID".into(), pid.to_string()));
        spec.env.push(("GRAY_CREDENTIAL_SOURCE".into(), "plugin".into()));
        spec.env.push(("GRAY_AUTH_REF".into(), installed.auth_ref()));
        spec.env.push((
            "GRAY_BASE_URL".into(),
            installed.provider.transport.base_url.to_string(),
        ));
        spec.env_remove.push("GRAY_API_KEY".into());
    } else {
        // Empty strings are the override: they clear whatever plugin
        // identity the saved config (the lead's provider) holds, so the
        // child resolves as a plain API-key connection.
        for k in ["GRAY_PROVIDER_ID", "GRAY_CREDENTIAL_SOURCE", "GRAY_AUTH_REF"] {
            spec.env.push((k.into(), String::new()));
        }
        spec.env.push(("GRAY_BASE_URL".into(), key.clone()));
        let pid = provider_auth_pid(&key);
        let key_value = crate::setup::load_auth_keys()
            .get(&pid)
            .cloned()
            .or_else(|| {
                crate::setup::catalog::detected_env_key(&pid, |v| std::env::var(v).ok())
                    .map(|(_, k)| k)
            });
        match key_value {
            Some(k) => spec.env.push(("GRAY_API_KEY".into(), k)),
            // Keyless endpoint (local server): nothing to send — and the
            // lead's own key must not leak to a different host.
            None => spec.env_remove.push("GRAY_API_KEY".into()),
        }
    }
    Some(spec)
}

/// One child run's parsed outcome (`error` set ⇒ the rest is noise).
#[derive(Default)]
struct SidekickOutcome {
    session_id: Option<String>,
    text: String,
    error: Option<String>,
    /// The timeout or a cancel killed it — never retried.
    killed: bool,
}

impl HandoffTool {
    /// `gray -p <task> --json --model <m> [--session <sid>]` in `ctx.cwd`,
    /// same `GRAY_HOME`, marked a sidekick. Own process group so a
    /// timeout takes grandchildren down too (gateway's `ChildRunner`
    /// pattern).
    async fn run(
        &self,
        spec: &SidekickSpec,
        task: &str,
        session: Option<&str>,
        ctx: &ToolContext,
    ) -> SidekickOutcome {
        let mut out = SidekickOutcome::default();
        let mut cmd = tokio::process::Command::new(&self.bin);
        cmd.arg("-p")
            .arg(task)
            .arg("--json")
            .arg("--model")
            .arg(&spec.model)
            .arg("--max-requests")
            .arg("200");
        if let Some(sid) = session {
            cmd.arg("--session").arg(sid);
        }
        cmd.current_dir(&ctx.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                Ok(())
            });
        }
        cmd.env("GRAY_HOME", &self.home)
            .env(ENV_SIDEKICK, "1")
            .env("GRAY_SHOW_REASONING", "0")
            .env(
                "GRAY_MAX_WALL_SECS",
                self.timeout.as_secs().max(1).to_string(),
            );
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                out.error = Some(format!("sidekick spawn failed: {e}"));
                return out;
            }
        };
        let pid = child.id().unwrap_or(0);
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take())
        else {
            out.error = Some("sidekick spawn failed: no pipes".into());
            return out;
        };
        let mut stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut tail: Vec<u8> = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        tail.extend_from_slice(&chunk[..n]);
                        if tail.len() > STDERR_TAIL {
                            tail.drain(..tail.len() - STDERR_TAIL);
                        }
                    }
                }
            }
            tail
        });
        let consume = async {
            use tokio::io::AsyncBufReadExt;
            let rd = tokio::io::BufReader::new(stdout);
            let mut lines = rd.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = if line.len() > LINE_CAP {
                    &line[..LINE_CAP]
                } else {
                    &line
                };
                handle_row(line, &mut out);
            }
        };
        enum Done {
            Child,
            Killed(&'static str),
        }
        let done = tokio::select! {
            _ = consume => Done::Child,
            _ = tokio::time::sleep(self.timeout) => {
                kill_tree(&mut child, pid).await;
                Done::Killed("handoff timed out")
            },
            _ = ctx.cancel.cancelled() => {
                kill_tree(&mut child, pid).await;
                Done::Killed("handoff cancelled")
            },
        };
        if let Done::Killed(msg) = done {
            out.killed = true;
            stderr_task.abort();
            out.error = Some(msg.into());
            return out;
        }
        let status = match tokio::time::timeout(REAP_GRACE, child.wait()).await {
            Ok(Ok(s)) => Some(s),
            _ => {
                kill_tree(&mut child, pid).await;
                None
            }
        };
        let stderr_tail = match tokio::time::timeout(REAP_GRACE, &mut stderr_task).await {
            Ok(Ok(tail)) => tail,
            _ => {
                stderr_task.abort();
                Vec::new()
            }
        };
        match status {
            Some(s) if !s.success() && out.error.is_none() => {
                let tail = String::from_utf8_lossy(&stderr_tail);
                let tail = tail.trim();
                let code = s
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| s.to_string());
                out.error = Some(if tail.is_empty() {
                    format!("sidekick exited with status {code}")
                } else {
                    format!("sidekick exited with status {code}: {tail}")
                });
            }
            None if out.error.is_none() => {
                out.error = Some("sidekick did not exit".into());
            }
            _ => {}
        }
        out
    }
}

/// One stdout row: `session_id` rides any row, `result`/`error` close the
/// outcome. Non-JSON lines are child noise and dropped.
fn handle_row(line: &str, out: &mut SidekickOutcome) {
    let Ok(row) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
        return;
    };
    if let Some(sid) = row.get("session_id").and_then(|v| v.as_str())
        && crate::session_store::valid_session_id(sid)
    {
        out.session_id = Some(sid.to_string());
    }
    match row.get("type").and_then(|v| v.as_str()) {
        Some("result") => {
            if let Some(t) = row.get("text").and_then(|v| v.as_str()) {
                out.text = t.to_string();
            }
        }
        Some("error") => {
            let msg = row
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown sidekick error");
            out.error = Some(match row.get("code").and_then(|v| v.as_str()) {
                Some(code) => format!("{code}: {msg}"),
                None => msg.to_string(),
            });
        }
        _ => {}
    }
}

/// SIGKILL the child's whole process group; the `pre_exec` setpgid made
/// it the leader. Off unix there is no group — the child alone dies.
#[cfg(unix)]
async fn kill_tree(child: &mut tokio::process::Child, pid: u32) {
    if pid != 0 {
        unsafe {
            libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
        }
    }
    if tokio::time::timeout(REAP_GRACE, child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

#[cfg(not(unix))]
async fn kill_tree(child: &mut tokio::process::Child, _pid: u32) {
    let _ = child.kill().await;
}

#[path = "fusion_tests.rs"]
#[cfg(test)]
mod tests;
