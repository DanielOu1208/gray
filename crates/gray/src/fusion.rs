use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use gray_core::agent::{Tool, ToolContext, ToolOutput};
use gray_core::message::ToolDef;
use gray_plugin::{Manifest, ModelSlot, ModelVariant, Plugin, SlotOption};

use crate::setup::variants::{CachedModel, CachedRow, Parts};

pub(crate) const ROW_ID: &str = "fusion";

pub(crate) const ENV_SIDEKICK: &str = "GRAY_FUSION_SIDEKICK";

const MAX_LEADS: usize = 60;
const MAX_SIDEKICKS: usize = 60;

const HANDOFF_TIMEOUT: Duration = Duration::from_secs(600);

const LINE_CAP: usize = 1024 * 1024;

const STDERR_TAIL: usize = 2 * 1024;

const REAP_GRACE: Duration = Duration::from_secs(5);

const LEAD_PROMPT: &str = r#"\
You have a `handoff` tool: a persistent sidekick subagent that works alongside you on the same machine (shared filesystem and repos; its shell sessions are separate from yours). You are the lead: you own the outcome and the user-facing and authority actions — talking to the user, planning, and directing the sidekick. The user interacts with one agent: you. Unless they explicitly ask about the sidekick, do not mention it or distinguish its work from yours; own the combined result and present it in the first person.

- **Delegate by default** the hands-on work: implementation, verification (builds, lints, type-checks, test runs), environment setup and repair, and broad fan-out searches where you only need the conclusion. Keep for yourself: planning and design, judgment calls, correctness-critical work where wrong output looks plausible (queries, measurements, eval harnesses, pipeline/model configuration — author and check those yourself), reviewing the sidekick's diffs, and everything user-facing or authority-bound (commits, pushes, PRs, answering the user).
- A `handoff` call blocks until the sidekick reports back — never run the same work yourself in parallel or poll for its result. Write each brief self-contained: the goal, the context and file pointers it needs, your plan, the constraints, and the narrowest verification commands that cover the change — the sidekick treats your list as mandatory, so a "run everything" brief re-gates unchanged work on every handoff.
- It remembers previous handoffs and its runtime state persists (dev servers, background jobs, in-flight commands) — don't re-explain what it already knows. For work involving servers or long-running processes, say what may still be running and whether to leave it up.
- Never make the sidekick redo work you already did: results you derived go into the brief as settled inputs. And review the evidence it reports back (diffs, logs, test output, artifact paths) instead of re-running its verification — check yourself only when the evidence is missing or suspicious."#;

const SIDEKICK_CONTRACT: &str = "<sidekick_contract>
You are the sidekick: a persistent collaborator agent executing task briefs handed off by the lead agent. You do not see the user's messages or the lead's conversation — everything you need is in the brief. Work autonomously with your own tools and verify your work before reporting.

- The lead owns user-facing and authority actions. Do not contact the user, create or update pull requests, commit, or push — for ordinary implementation tasks leave the work tree ready for the lead to review. If the task needs something only the lead or user can do (a secret, information only the user has, a PR action), stop and report what you need instead of working around it.
- Git safety: never run destructive or irreversible git commands unless the brief directs it; stage files explicitly (never `git add .`) and never stage files likely to contain secrets.
- Keep diffs minimal and match the repo's existing conventions; touch only what the task asks for.
- Report back so it stands alone — the user never sees your output: what you did or found, how you verified it (commands run, results, evidence paths), any deviations, and anything left for the lead.
</sidekick_contract>

Brief:
";

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
    crate::setup::catalog::custom_key_id(list_key)
        .replace(|c: char| !(c.is_ascii_alphanumeric() || c == '-'), "-")
}

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

fn plain_rows(rows: &[CachedRow]) -> Vec<(String, String)> {
    rows.iter()
        .filter(|r| !matches!(r, CachedRow::Full(m) if !m.slots.is_empty()))
        .map(|r| r.pair())
        .collect()
}

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
        price_cliff_tokens: None,
    })
}

pub(crate) fn extend_with_fusion(home: &Path, list_key: &str, rows: &mut Vec<CachedRow>) {
    let mut lists = crate::setup::context::provider_model_lists(home);
    lists.insert(list_key.to_string(), rows.clone());
    if let Some(model) = synthesize_row(list_key, rows, &lists) {
        rows.push(CachedRow::Full(model));
    }
}

pub(crate) fn is_local_parts(parts: &Parts) -> bool {
    parts.get("sidekick").is_some_and(|s| s.contains(':'))
}

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

    let _ = crate::setup::context::load_provider_model_list(&list_key);
    let shape = crate::setup::variants::row_shape(model)?;
    if !shape.is_composite() {
        return None;
    }
    let parts = if saved.model_parts.is_empty() {
        saved
            .parts_memory
            .get(&crate::setup::effort_memory_key(
                &saved.provider_id,
                saved.base_url.as_deref().unwrap_or_default(),
                model,
            ))
            .cloned()
            .unwrap_or_default()
    } else {
        saved.model_parts.clone()
    };
    let sidekick = parts.get("sidekick")?;

    Some(if is_local_parts(&parts) {
        sidekick.clone()
    } else {
        format!("{}:{sidekick}", provider_token(&list_key))
    })
}

pub(crate) struct FusionPlugin {

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
            .or_else(|| crate::exe_watch::on_disk_exe().ok())
            .unwrap_or_else(|| PathBuf::from("gray"));
        ["handoff", "native__handoff"]
            .into_iter()
            .map(|name| {
                Arc::new(HandoffTool {
                    name,
                    qualified: qualified.clone(),
                    home: home.clone(),
                    bin: bin.clone(),
                    timeout: HANDOFF_TIMEOUT,
                    gate: self.gate.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }
}

pub(crate) fn lead_framing() -> Option<&'static str> {
    local_sidekick().is_some().then_some(LEAD_PROMPT)
}

struct HandoffTool {

    name: &'static str,

    qualified: String,
    home: PathBuf,

    bin: PathBuf,
    timeout: Duration,
    gate: Arc<tokio::sync::Mutex<()>>,
}

#[async_trait::async_trait]
impl Tool for HandoffTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: self.name.to_string(),
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

            label: None,
            preview: Some("task".to_string()),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, ctx: &ToolContext, args: serde_json::Value) -> ToolOutput {
        let task = ["task", "message"].into_iter().find_map(|key| {
            args.get(key)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
        });
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

fn session_file(home: &Path, qualified: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    qualified.hash(&mut h);
    home.join("fusion")
        .join(format!("sidekick-{:016x}.session", h.finish()))
}

struct SidekickSpec {
    model: String,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
}

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
        spec.env
            .push(("GRAY_CREDENTIAL_SOURCE".into(), "plugin".into()));
        spec.env
            .push(("GRAY_AUTH_REF".into(), installed.auth_ref()));
        spec.env.push((
            "GRAY_BASE_URL".into(),
            installed.provider.transport.base_url.to_string(),
        ));
        spec.env_remove.push("GRAY_API_KEY".into());
    } else {

        for k in [
            "GRAY_PROVIDER_ID",
            "GRAY_CREDENTIAL_SOURCE",
            "GRAY_AUTH_REF",
        ] {
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

            None => spec.env_remove.push("GRAY_API_KEY".into()),
        }
    }
    Some(spec)
}

#[derive(Default)]
struct SidekickOutcome {
    session_id: Option<String>,
    text: String,
    error: Option<String>,

    killed: bool,
}

impl HandoffTool {

    async fn run(
        &self,
        spec: &SidekickSpec,
        task: &str,
        session: Option<&str>,
        ctx: &ToolContext,
    ) -> SidekickOutcome {
        let mut out = SidekickOutcome::default();

        let brief;
        let task = if session.is_none() {
            brief = format!("{SIDEKICK_CONTRACT}\n{task}");
            brief.as_str()
        } else {
            task
        };
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
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
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
