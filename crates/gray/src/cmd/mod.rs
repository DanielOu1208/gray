//! `gray cmd` — the headless slash-command surface for channel adapters
//! (GRAY-CMD-CONTRACT.md). Adapters spawn `GRAY_HOME=<home> gray cmd <verb>`;
//! every answer is one JSON object on stdout — the wire is stdout only.
//!
//! Verbs: `list` (command table), `run <name> [args]` (text or ui result),
//! `interact <token> [action-json]` (pending-doc revisions). Commands the
//! REPL needs a TTY for answer `local_only`; `interactive` ones may answer
//! `kind:"ui"` whose pending draft lives under `<GRAY_HOME>/cmd_ui/`.

mod registry;
mod store;
mod ui;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::CmdVerb;
use crate::config::Config;
use crate::repl::commands::{CmdSurface, resolve};
use crate::repl::usage_panel;
use crate::repl::{SessionTotals, cron, handlers};
use crate::session_store::{JsonlSessionStore, SessionId};
use crate::setup::variants::{Parts, RowShape};

/// A `run`/`interact` answer. `text` is final (`close` says the adapter
/// strips components); `ui` carries a pending document the adapter renders
/// (`fallback_text` is the same content for component-less platforms).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum CommandResult {
    Text {
        text: String,
        close: bool,
    },
    Ui {
        token: String,
        document: ui::UiDoc,
        fallback_text: String,
    },
}

impl CommandResult {
    fn text(text: impl Into<String>) -> Self {
        Self::Text {
            text: text.into(),
            close: true,
        }
    }
}

/// Wire error: the envelope's `error.code`/`error.message`.
#[derive(Debug)]
struct CmdError(&'static str, String);

impl CmdError {
    fn unknown(name: &str) -> Self {
        Self("unknown_command", format!("unknown command '{name}'"))
    }
    fn local(name: &str) -> Self {
        Self(
            "local_only",
            format!("'/{name}' needs the interactive TUI — run it in a gray session"),
        )
    }
    fn args(msg: impl Into<String>) -> Self {
        Self("invalid_args", msg.into())
    }
    fn unavailable(msg: impl Into<String>) -> Self {
        Self("unavailable", msg.into())
    }
    fn expired() -> Self {
        Self("expired", "interaction expired — re-run the command".into())
    }
    fn invalid_action(msg: impl Into<String>) -> Self {
        Self("invalid_action", msg.into())
    }
    fn not_owner() -> Self {
        Self("not_owner", "interaction belongs to another user".into())
    }
}

fn ok(result: Value) -> Value {
    json!({"ok": true, "protocol": 1, "result": result})
}

fn fail(e: CmdError) -> Value {
    json!({"ok": false, "error": {"code": e.0, "message": e.1, "retryable": false}})
}

/// `gray cmd <verb>` entry point: dispatch and print exactly one envelope
/// line. Nothing else may write stdout while this runs.
pub async fn run_cli(verb: CmdVerb, config: &mut Config) -> anyhow::Result<()> {
    let out = match verb {
        CmdVerb::List => ok(registry::list_json()),
        CmdVerb::Run { name, args } => match gray_home() {
            Ok(home) => match run_command(config, &home, &name, &args).await {
                Ok(res) => ok(serde_json::to_value(res)?),
                Err(e) => fail(e),
            },
            Err(e) => fail(CmdError::unavailable(format!("{e:#}"))),
        },
        CmdVerb::Interact { token, action } => match gray_home() {
            Ok(home) => match interact(config, &home, &token, action.as_deref()) {
                Ok(res) => ok(serde_json::to_value(res)?),
                Err(e) => fail(e),
            },
            Err(e) => fail(CmdError::unavailable(format!("{e:#}"))),
        },
    };
    println!("{out}");
    Ok(())
}

fn gray_home() -> anyhow::Result<PathBuf> {
    crate::setup::gray_home()
}

/// `gray cmd run <name> [args]`: canonical-name dispatch. `local` surface
/// is rejected here so a new registry entry can't leak through silently.
async fn run_command(
    config: &mut Config,
    home: &Path,
    name: &str,
    args: &[String],
) -> Result<CommandResult, CmdError> {
    let Some(def) = resolve(name) else {
        return Err(CmdError::unknown(name));
    };
    if def.surface == CmdSurface::Local {
        return Err(CmdError::local(def.name));
    }
    let rest = args.join(" ");
    match def.name {
        "help" => Ok(help_result()),
        "usage" => usage_result(config, home).await,
        "models" => model_result(config, home, &rest),
        "new" => new_result(home).await,
        "sessions" => sessions_result(home, &rest).await,
        "cron" => cron_result(home, &rest),
        "skills" => skills_result(&rest),
        other => Err(CmdError::unavailable(format!(
            "'/{other}' is not implemented over cmd yet"
        ))),
    }
}

/// `/help`: the registry lines plus plugin commands and prompt templates —
/// the same text the headless REPL prints, minus the terminal rule.
fn help_result() -> CommandResult {
    let mut out = String::new();
    for d in crate::repl::REGISTRY {
        out.push_str(&crate::repl::commands::format_help_line(d));
        out.push('\n');
    }
    let mut seen: std::collections::HashSet<String> = crate::repl::REGISTRY
        .iter()
        .map(|d| d.name.to_string())
        .collect();
    for (name, description) in crate::plugin_cli::completions("") {
        if seen.insert(name.clone()) {
            out.push_str(&format!("  /{name:<10} {description}\n"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let templates = crate::prompt_templates::discover(&cwd);
        if !templates.is_empty() {
            out.push_str("prompt templates:\n");
            for t in templates {
                if seen.insert(t.name.clone()) {
                    out.push_str(&format!("  /{:<10} {}\n", t.name, t.description));
                }
            }
        }
    }
    CommandResult::text(out.trim_end())
}

/// `/usage`: session totals rebuilt from the stored entries — the adapter's
/// pointed session first, else the home's most recently active one (the
/// same precedence a resumed turn takes).
async fn usage_result(config: &Config, home: &Path) -> Result<CommandResult, CmdError> {
    let store = JsonlSessionStore::new(home.join("sessions"));
    let summaries = store.list().await;
    let pointed = pointed_session(home).filter(|sid| summaries.iter().any(|s| s.id == *sid));
    let id =
        pointed.or_else(|| crate::resume::latest_summary(&summaries, None).map(|s| s.id.clone()));
    let Some(id) = id else {
        return Ok(CommandResult::text(
            "no sessions yet — usage appears after the first turn",
        ));
    };
    let (meta, entries) = store
        .load(&id)
        .await
        .map_err(|e| CmdError::unavailable(format!("cannot load session {id}: {e:#}")))?;
    let model = if meta.model.is_empty() {
        config.model.clone().unwrap_or_default()
    } else {
        meta.model
    };
    let totals = SessionTotals::from_entries(&entries, &model);
    if totals.turns == 0 {
        return Ok(CommandResult::text(
            "no turns yet this session — usage appears after the first turn",
        ));
    }
    let input = usage_panel::PanelInput {
        totals: &totals,
        model: &model,
        rate: crate::setup::get_model_rate(&model),
        warmth: usage_panel::Warmth::Unknown,
    };
    let mut text = format!("✓ Session usage — {}", usage_panel::usage_header(&input));
    for line in usage_panel::usage_plain(&input) {
        text.push('\n');
        text.push_str(&line);
    }
    Ok(CommandResult::text(text))
}

/// The adapter's session pointer (`<home>/session.json` → `session_id`),
/// `None` when missing or malformed.
fn pointed_session(home: &Path) -> Option<SessionId> {
    std::fs::read(home.join("session.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| {
            v.get("session_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|s| crate::session_store::valid_session_id(s))
        .map(SessionId::new)
}

/// `/models`: bare mints the editable picker doc; `model <id>` is the
/// direct-set path shared with the REPL handler.
fn model_result(config: &mut Config, home: &Path, rest: &str) -> Result<CommandResult, CmdError> {
    let raw = rest.trim();
    if !raw.is_empty() {
        return model_direct(config, home, raw);
    }
    let mut models = model_list_for(config);
    if models.is_empty() {
        return Ok(CommandResult::text(
            "no provider configured — run /models to set up provider & key",
        ));
    }
    let mut draft = store::ModelDraft {
        // No current model: preselect the top row like the TUI picker, so a
        // bare `apply` still does something sensible.
        model: config
            .model
            .clone()
            .or_else(|| models.first().map(|(id, _)| id.clone())),
        effort: config
            .thinking_effort
            .clone()
            .or_else(|| Some("high".to_string())),
        fast: config.fast_mode.unwrap_or(false),
        parts: Default::default(),
    };
    cap_options(&mut models, draft.model.as_deref());
    // Composite rows carry their declared shapes into the pending doc —
    // `interact` re-renders the slot selects and validates parts from the
    // record, never the wire.
    let mut shapes = BTreeMap::new();
    for (id, _) in &models {
        if let Some(shape) = crate::setup::variants::row_shape(id).filter(|s| s.is_composite()) {
            shapes.insert(id.clone(), (*shape).clone());
        }
    }
    // The selected row's slots open on the live selection (when it is this
    // row), else the row's default — the TUI picker's `parts_of` seed.
    if let Some(shape) = draft.model.as_deref().and_then(|id| shapes.get(id)) {
        draft.parts = if draft.model.as_deref() == config.model.as_deref() {
            crate::setup::selection_parts(shape, &config.model_parts)
        } else {
            shape.settle_parts(None)
        };
    }
    let pending = store::PendingDoc {
        kind: "model_picker".to_string(),
        created_at: crate::cron::now_secs(),
        user: None,
        draft: draft.clone(),
        models: models.clone(),
        shapes,
    };
    let token = store::put(home, &pending)
        .map_err(|e| CmdError::unavailable(format!("cannot store pending doc: {e}")))?;
    Ok(CommandResult::Ui {
        token,
        document: model_picker_doc(&draft, &models, &pending.shapes),
        fallback_text: model_picker_fallback(&draft, &models, &pending.shapes),
    })
}

/// `model <id> [slot=option ...]`: the direct-set path shared with the
/// REPL handler. A composite row additionally takes per-slot picks
/// (`model fusion lead=opus sidekick=a`); bare `<id>` on a composite
/// lands the remembered — else default — combination.
fn model_direct(config: &mut Config, home: &Path, raw: &str) -> Result<CommandResult, CmdError> {
    let path = home.join("config.json");
    let mut tokens = raw.split_whitespace();
    let id = tokens.next().unwrap_or_default();
    let mut picks = Parts::new();
    for t in tokens {
        match t.split_once('=') {
            Some((k, v)) if !k.is_empty() && !v.is_empty() => {
                picks.insert(k.to_string(), v.to_string());
            }
            _ => {
                return Err(CmdError::args(format!(
                    "usage: /models <id> [slot=option ...] — bad token '{t}'"
                )));
            }
        }
    }
    let prev = config.model.clone();
    if picks.is_empty() {
        let applied = handlers::apply_model_selection_at(config, id, None, None, Some(&path))
            .map_err(CmdError::args)?;
        // A composite row picked bare lands its settled parts; a plain
        // row sheds a stale composite's (empty clears the live field).
        let shape = crate::setup::variants::row_shape(&applied.model).filter(|s| s.is_composite());
        let parts = shape
            .as_ref()
            .map(|s| opening_parts(prev.as_deref(), config, home, &applied.model, s))
            .unwrap_or_default();
        commit_parts(config, home, &parts);
        let name =
            crate::setup::composite_label_for(config).unwrap_or_else(|| applied.model.clone());
        return Ok(CommandResult::text(model_set_text(&applied, &name)));
    }
    // Slot picks need the row's declared slots. A registered composite
    // row resolves in-process; anything else canonicalizes (names, case)
    // against the known list before its shape lookup.
    let (id, shape) = match crate::setup::variants::row_shape(id).filter(|s| s.is_composite()) {
        Some(shape) => (id.to_string(), shape),
        None => {
            let models = model_list_for(config);
            let canonical =
                crate::setup::validate_direct_model_id(id, &models).map_err(CmdError::args)?;
            match crate::setup::variants::row_shape(&canonical).filter(|s| s.is_composite()) {
                Some(shape) => (canonical, shape),
                None => {
                    return Err(CmdError::args(format!("'{canonical}' has no slots to set")));
                }
            }
        }
    };
    for (k, v) in &picks {
        let Some(slot) = shape.slots.iter().find(|s| &s.key == k) else {
            let keys = shape
                .slots
                .iter()
                .map(|s| s.key.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CmdError::args(format!(
                "unknown slot '{k}' — {id} slots: {keys}"
            )));
        };
        if !slot.options.iter().any(|o| &o.id == v) {
            let opts = slot
                .options
                .iter()
                .map(|o| o.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(CmdError::args(format!(
                "unknown {k} option '{v}' — options: {opts}"
            )));
        }
    }
    // Each pick is a constraint: the winning variant satisfies them all
    // and stays closest to the remembered/default combination, so a lone
    // `sidekick=b` lands `swe` when only `swe × b` exists.
    let baseline = opening_parts(prev.as_deref(), config, home, &id, &shape);
    let satisfies =
        |v: &gray_plugin::ModelVariant| picks.iter().all(|(k, val)| v.parts.get(k) == Some(val));
    let drift = |v: &gray_plugin::ModelVariant| {
        shape
            .slots
            .iter()
            .filter(|s| !picks.contains_key(&s.key) && v.parts.get(&s.key) != baseline.get(&s.key))
            .count()
    };
    let Some(variant) = shape
        .variants
        .iter()
        .filter(|v| satisfies(v))
        .min_by_key(|v| drift(v))
    else {
        let combo = picks
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        return Err(CmdError::args(format!(
            "no {id} variant serves {combo} — check the slot options"
        )));
    };
    let parts = variant.parts.clone();
    let applied = handlers::apply_model_selection_at(config, &id, None, None, Some(&path))
        .map_err(CmdError::args)?;
    commit_parts(config, home, &parts);
    let name = crate::setup::composite_label_for(config).unwrap_or_else(|| applied.model.clone());
    Ok(CommandResult::text(model_set_text(&applied, &name)))
}

/// The combination a composite row opens at for `model <id>`: the live
/// selection when `current` is this row, the remembered pick, else the
/// catalog default — the TUI picker's `parts_of` precedence.
fn opening_parts(
    current: Option<&str>,
    config: &Config,
    home: &Path,
    row: &str,
    shape: &RowShape,
) -> Parts {
    let want = if current == Some(row) {
        Some(config.model_parts.clone())
    } else {
        let saved = crate::setup::load_saved_config_at(&home.join("config.json"));
        saved
            .parts_memory
            .get(&crate::setup::effort_memory_key(
                &config.provider_id,
                &config.base_url,
                row,
            ))
            .cloned()
    };
    shape.settle_parts(want.as_ref())
}

/// The composite half of an apply: live `config.model_parts` plus the
/// saved selection (and the row's remembered pick). Empty clears the
/// live field — a plain row sheds a stale composite's parts.
fn commit_parts(config: &mut Config, home: &Path, parts: &Parts) {
    config.model_parts = parts.clone();
    let path = home.join("config.json");
    let _cfg_lock = crate::setup::lock_saved_config_at(&path).ok();
    let mut saved = crate::setup::load_saved_config_at(&path);
    let key = crate::setup::effort_memory_key(
        &config.provider_id,
        &config.base_url,
        config.model.as_deref().unwrap_or_default(),
    );
    saved.remember_parts(&key, parts);
    let _ = crate::setup::save_saved_config_at(&path, &saved);
}

/// "Row · Lead × Sidekick" for the apply receipt — `None` (the caller
/// falls back to the row id) when the parts don't fill every slot.
fn composite_name(
    row: &str,
    models: &[(String, String)],
    shape: &RowShape,
    parts: &Parts,
    effort: Option<&str>,
) -> Option<String> {
    let name = models
        .iter()
        .find(|(id, _)| id == row)
        .map(|(_, n)| n.as_str())
        .filter(|n| !n.is_empty())
        .unwrap_or(row);
    crate::setup::variants::composite_label_parts(shape, name, parts, effort)
        .map(|segs| segs.into_iter().map(|(t, _)| t).collect())
}

/// The picker's options: the live provider list, then the cached list as
/// the offline fallback, the current model last resort.
fn model_list_for(config: &Config) -> Vec<(String, String)> {
    let (_, rows) = crate::setup::provider_models_for_config(config);
    if !rows.is_empty() {
        return rows;
    }
    let (_, list_key, _) = crate::setup::picker_scope(config);
    let cached = crate::setup::saved_models_for(&list_key, &config.base_url);
    if !cached.is_empty() {
        return cached;
    }
    config
        .model
        .clone()
        .map(|m| vec![(m, String::new())])
        .unwrap_or_default()
}

/// Selects cap at 25 options on the strictest adapter — pin `keep` first,
/// then fill, so the current selection is never clipped out.
const MAX_SELECT_OPTIONS: usize = 25;

fn cap_options(models: &mut Vec<(String, String)>, keep: Option<&str>) {
    if models.len() <= MAX_SELECT_OPTIONS {
        return;
    }
    if let Some(sel) = keep
        && let Some(pos) = models.iter().position(|(id, _)| id == sel)
    {
        let row = models.remove(pos);
        models.insert(0, row);
    }
    models.truncate(MAX_SELECT_OPTIONS);
}

/// `/new`: reset the home's current-session pointer. The adapter's
/// `session.json` loses its `session_id` and advances `generation` — the
/// same semantics as the adapter's own reset, so a still-running turn's
/// write-back is discarded. Session files stay (they remain resumable).
async fn new_result(home: &Path) -> Result<CommandResult, CmdError> {
    let state_path = home.join("session.json");
    if let Ok(bytes) = std::fs::read(&state_path)
        && let Ok(mut state) = serde_json::from_slice::<Value>(&bytes)
        && state.is_object()
    {
        state["session_id"] = Value::Null;
        state["generation"] = Value::String(uuid::Uuid::new_v4().simple().to_string());
        state["last_activity"] = json!(crate::cron::now_secs());
        let _ = crate::cron::store::atomic_write_json(&state_path, &state);
    }
    // gray's own per-cwd remembered pointer — cleared for the process cwd
    // and the `work` dir channel adapters spawn turns in.
    let store = JsonlSessionStore::new(home.join("sessions"));
    for cwd in [std::env::current_dir().ok(), Some(home.join("work"))]
        .into_iter()
        .flatten()
    {
        store.forget(&cwd).await;
    }
    Ok(CommandResult::text(
        "new conversation — the next message opens a fresh session",
    ))
}

/// `/sessions`: resumable sessions in this home, newest activity first.
async fn sessions_result(home: &Path, rest: &str) -> Result<CommandResult, CmdError> {
    if !rest.trim().is_empty() {
        return Err(CmdError::args(
            "usage: /sessions — resuming is the adapter's `gray -p --session <id>`",
        ));
    }
    let store = JsonlSessionStore::new(home.join("sessions"));
    let summaries = crate::resume::recent_summaries(&store, true).await;
    if summaries.is_empty() {
        return Ok(CommandResult::text("no saved sessions"));
    }
    let mut out = String::new();
    for s in &summaries {
        out.push_str(&crate::resume::format_summary_row(s));
        out.push('\n');
    }
    Ok(CommandResult::text(out.trim_end()))
}

/// `/cron`: the read-only dashboard (bare) or one job's row (`<id|name>`),
/// plus the ticker-liveness line — the headless REPL render verbatim.
fn cron_result(home: &Path, rest: &str) -> Result<CommandResult, CmdError> {
    let store = crate::cron::CronStore::open(home.join("cron"))
        .map_err(|e| CmdError::unavailable(format!("{e:#}")))?;
    let jobs = store
        .list()
        .map_err(|e| CmdError::unavailable(format!("{e:#}")))?;
    let now = crate::cron::now_secs();
    let health = store.health(now).ok();
    let id = rest.trim();
    let text = if id.is_empty() {
        cron::format_cron_dashboard(&jobs, health.as_ref(), now)
    } else {
        match jobs.into_iter().find(|j| j.id == id || j.name == id) {
            Some(j) => cron::format_cron_dashboard(&[j], health.as_ref(), now),
            None => format!("unknown cron job {id:?}"),
        }
    };
    Ok(CommandResult::text(text))
}

/// `/skills`: the headless list — running a skill needs a chat turn, so
/// args are rejected rather than silently ignored.
fn skills_result(rest: &str) -> Result<CommandResult, CmdError> {
    if !rest.trim().is_empty() {
        return Err(CmdError::args(
            "skill invocation needs a chat turn — `gray cmd run skills` only lists",
        ));
    }
    let cwd = std::env::current_dir()
        .map_err(|e| CmdError::unavailable(format!("cannot resolve cwd: {e}")))?;
    let discovered = crate::skills::discover_skills(&cwd);
    if discovered.skills.is_empty() {
        return Ok(CommandResult::text("no skills discovered"));
    }
    let auto = crate::setup::skills_auto_enabled();
    let disabled = crate::setup::disabled_skill_names();
    let mut out = String::new();
    if !auto {
        out.push_str("skills auto-load is off (/skills on to re-enable)\n");
    }
    for s in &discovered.skills {
        let mut row = crate::skills::format_discovered_skill_row(s);
        if !auto {
            row.push_str(" [auto-off]");
        } else if disabled.contains(&s.name) {
            row.push_str(" [disabled]");
        }
        out.push_str(&row);
        out.push('\n');
    }
    Ok(CommandResult::text(out.trim_end()))
}

// ---------------------------------------------------------------------------
// interact: pending-doc revisions
// ---------------------------------------------------------------------------

/// The adapter-normalized action (`{user?, values?, submit?}` — all keys
/// optional; a bare `interact` re-renders the current draft).
#[derive(Debug, Default, serde::Deserialize)]
struct Action {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    values: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    submit: Option<String>,
}

/// `gray cmd interact <token> [action-json]`: the next `CommandResult` for
/// a pending `editable` doc — a revision for `values`/`fast`, a final text
/// for `apply`/`cancel` (which consume the token).
fn interact(
    config: &mut Config,
    home: &Path,
    token: &str,
    action_json: Option<&str>,
) -> Result<CommandResult, CmdError> {
    let mut rec = store::get(home, token, crate::cron::now_secs())?;
    if rec.kind != "model_picker" {
        return Err(CmdError::invalid_action(format!(
            "unknown pending doc kind '{}'",
            rec.kind
        )));
    }
    let action: Option<Action> = match action_json {
        None => None,
        Some(raw) => Some(
            serde_json::from_str(raw)
                .map_err(|e| CmdError::invalid_action(format!("bad action json: {e}")))?,
        ),
    };
    if let Some(a) = &action {
        // The first user to interact claims the doc; later interactors must
        // match — adapters on shared channels pass `user` for exactly this.
        match (&rec.user, &a.user) {
            (Some(owner), Some(u)) if u != owner => return Err(CmdError::not_owner()),
            (Some(_), None) => return Err(CmdError::not_owner()),
            (None, Some(u)) => rec.user = Some(u.clone()),
            _ => {}
        }
        for (k, vals) in &a.values {
            let v = vals.last().map(String::as_str).unwrap_or("");
            match k.as_str() {
                "model" => {
                    if !rec.models.iter().any(|(id, _)| id == v) {
                        return Err(CmdError::invalid_action(format!(
                            "unknown model option '{v}'"
                        )));
                    }
                    rec.draft.model = Some(v.to_string());
                    // A new row opens on its own defaults — the slot
                    // selects re-render for it on the revised doc.
                    rec.draft.parts = rec
                        .shapes
                        .get(v)
                        .map(|s| s.settle_parts(None))
                        .unwrap_or_default();
                }
                "effort" => {
                    if !["low", "medium", "high", "max"].contains(&v) {
                        return Err(CmdError::invalid_action(format!("unknown effort '{v}'")));
                    }
                    rec.draft.effort = Some(v.to_string());
                }
                _ if k.starts_with("slot:") => {
                    let key = &k["slot:".len()..];
                    let Some(shape) = rec
                        .draft
                        .model
                        .as_deref()
                        .and_then(|m| rec.shapes.get(m))
                        .filter(|s| s.is_composite())
                    else {
                        return Err(CmdError::invalid_action(format!("unknown control '{k}'")));
                    };
                    // The pick wins, earlier slots hold, later ones take
                    // the serving variant's — the TUI dropdown's coerce.
                    let cur = shape.settle_parts(Some(&rec.draft.parts));
                    match shape.option_parts(&cur, key, v) {
                        Some(parts) => rec.draft.parts = parts,
                        None => {
                            return Err(CmdError::invalid_action(format!(
                                "no variant serves {key}={v} here"
                            )));
                        }
                    }
                }
                other => {
                    return Err(CmdError::invalid_action(format!(
                        "unknown control '{other}'"
                    )));
                }
            }
        }
    }
    match action.and_then(|a| a.submit) {
        None => {
            store::save(home, token, &rec).map_err(save_err)?;
            Ok(revised_doc(token, &rec))
        }
        Some(submit) => match submit.as_str() {
            "fast" => {
                rec.draft.fast = !rec.draft.fast;
                store::save(home, token, &rec).map_err(save_err)?;
                Ok(revised_doc(token, &rec))
            }
            "apply" => {
                let Some(model) = rec.draft.model.clone() else {
                    return Err(CmdError::invalid_action("no model selected"));
                };
                // Exact parts match: a composite's picks must resolve to a
                // concrete variant or the apply bounces back unfinal.
                let shape = rec.shapes.get(&model).filter(|s| s.is_composite());
                let parts = match shape {
                    Some(s) => {
                        if crate::setup::variants::resolve_variant(
                            &s.variants,
                            rec.draft.effort.as_deref(),
                            rec.draft.fast,
                            &rec.draft.parts,
                        )
                        .is_none()
                        {
                            let combo = rec
                                .draft
                                .parts
                                .iter()
                                .map(|(k, v)| format!("{k}={v}"))
                                .collect::<Vec<_>>()
                                .join(" ");
                            return Ok(CommandResult::Text {
                                text: format!(
                                    "no {model} variant serves {combo} — adjust the slot selections"
                                ),
                                close: false,
                            });
                        }
                        rec.draft.parts.clone()
                    }
                    None => Parts::new(),
                };
                match handlers::apply_model_selection_at(
                    config,
                    &model,
                    rec.draft.effort.as_deref(),
                    Some(rec.draft.fast),
                    Some(&home.join("config.json")),
                ) {
                    Ok(applied) => {
                        commit_parts(config, home, &parts);
                        store::remove(home, token);
                        let name = shape
                            .and_then(|s| {
                                composite_name(
                                    &model,
                                    &rec.models,
                                    s,
                                    &parts,
                                    config.thinking_effort.as_deref(),
                                )
                            })
                            .unwrap_or_else(|| applied.model.clone());
                        Ok(CommandResult::text(model_set_text(&applied, &name)))
                    }
                    // A failed apply is not the end of the flow: the doc
                    // stays pending and the adapter keeps its components.
                    Err(msg) => Ok(CommandResult::Text {
                        text: msg,
                        close: false,
                    }),
                }
            }
            "cancel" => {
                store::remove(home, token);
                Ok(CommandResult::text("Cancelled."))
            }
            other => Err(CmdError::invalid_action(format!(
                "unknown submit '{other}'"
            ))),
        },
    }
}

fn save_err(e: std::io::Error) -> CmdError {
    CmdError::unavailable(format!("cannot save pending doc: {e}"))
}

/// The picker doc re-rendered from the current draft (`editable` revisions
/// re-use the token — the record, not the wire, carries the state).
fn revised_doc(token: &str, rec: &store::PendingDoc) -> CommandResult {
    CommandResult::Ui {
        token: token.to_string(),
        document: model_picker_doc(&rec.draft, &rec.models, &rec.shapes),
        fallback_text: model_picker_fallback(&rec.draft, &rec.models, &rec.shapes),
    }
}

fn model_set_text(applied: &handlers::ModelApply, name: &str) -> String {
    let mut text = format!("✓ Model set to {name}");
    if let Some((old, new)) = &applied.clamped {
        text.push_str(&format!(
            "\nThinking effort clamped from {old} to {new} (not supported by this model)"
        ));
    }
    text
}

// ---------------------------------------------------------------------------
// the model picker document (contract reference)
// ---------------------------------------------------------------------------

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn picker_header(draft: &store::ModelDraft) -> String {
    format!(
        "**Model** — `{}` · effort `{}` · fast `{}`",
        draft.model.as_deref().unwrap_or("-"),
        draft.effort.as_deref().unwrap_or("-"),
        if draft.fast { "on" } else { "off" },
    )
}

/// `document_id: "model-picker"`, `visibility: ephemeral`,
/// `lifecycle: editable` — the contract's reference doc rendered from
/// `draft`.
fn model_picker_doc(
    draft: &store::ModelDraft,
    models: &[(String, String)],
    shapes: &BTreeMap<String, RowShape>,
) -> ui::UiDoc {
    let model_options = models
        .iter()
        .map(|(id, name)| ui::SelectOption {
            label: clip(if name.is_empty() { id } else { name }, 100),
            value: id.clone(),
            description: (!name.is_empty() && name != id).then(|| clip(id, 100)),
            default: draft.model.as_deref() == Some(id.as_str()),
        })
        .collect();
    let effort_options = ["low", "medium", "high", "max"]
        .iter()
        .map(|e| ui::SelectOption {
            label: e.to_string(),
            value: e.to_string(),
            description: None,
            default: draft.effort.as_deref() == Some(*e),
        })
        .collect();
    let mut components = vec![
        ui::MessageNode::Text {
            content: picker_header(draft),
        },
        ui::MessageNode::ActionRow {
            children: vec![ui::MessageNode::StringSelect {
                logical_id: "model".to_string(),
                placeholder: Some("model".to_string()),
                options: model_options,
                min_values: None,
                max_values: None,
            }],
        },
    ];
    // A composite row's slots sit between the model and effort selects —
    // one `slot:<key>` select per declared slot.
    if let Some(shape) = draft_shape(draft, shapes) {
        for slot in &shape.slots {
            components.push(slot_select_row(
                slot,
                draft.parts.get(&slot.key).map(String::as_str),
            ));
        }
    }
    components.push(ui::MessageNode::ActionRow {
        children: vec![ui::MessageNode::StringSelect {
            logical_id: "effort".to_string(),
            placeholder: Some("effort".to_string()),
            options: effort_options,
            min_values: None,
            max_values: None,
        }],
    });
    components.push(ui::MessageNode::ActionRow {
        children: vec![
            ui::MessageNode::Button {
                logical_id: "apply".to_string(),
                label: "Apply".to_string(),
                style: ui::ButtonStyle::Primary,
                url: None,
            },
            ui::MessageNode::Button {
                logical_id: "fast".to_string(),
                label: format!("⚡ fast: {}", if draft.fast { "on" } else { "off" }),
                style: ui::ButtonStyle::Secondary,
                url: None,
            },
            ui::MessageNode::Button {
                logical_id: "cancel".to_string(),
                label: "Cancel".to_string(),
                style: ui::ButtonStyle::Secondary,
                url: None,
            },
        ],
    });
    ui::UiDoc::message(
        "model-picker",
        ui::Visibility::Ephemeral,
        ui::Lifecycle::Editable,
        components,
    )
}

/// The selected row's declared shape, when it is composite.
fn draft_shape<'a>(
    draft: &store::ModelDraft,
    shapes: &'a BTreeMap<String, RowShape>,
) -> Option<&'a RowShape> {
    draft
        .model
        .as_deref()
        .and_then(|id| shapes.get(id))
        .filter(|s| s.is_composite())
}

/// One `slot:<key>` select for a composite slot — the draft's pick (or
/// the first option) is the default, pinned inside the 25-option cap so
/// it never clips out.
fn slot_select_row(slot: &gray_plugin::ModelSlot, current: Option<&str>) -> ui::MessageNode {
    let mut options: Vec<(String, String)> = slot
        .options
        .iter()
        .map(|o| (o.id.clone(), o.name.clone()))
        .collect();
    let current = current.filter(|id| slot.options.iter().any(|o| &o.id == *id));
    let keep = current.or_else(|| slot.options.first().map(|o| o.id.as_str()));
    cap_options(&mut options, keep);
    ui::MessageNode::ActionRow {
        children: vec![ui::MessageNode::StringSelect {
            logical_id: format!("slot:{}", slot.key),
            placeholder: Some(slot.label.clone()),
            options: options
                .into_iter()
                .map(|(id, name)| ui::SelectOption {
                    label: clip(if name.is_empty() { &id } else { &name }, 100),
                    value: id.clone(),
                    description: (!name.is_empty() && name != id).then(|| clip(&id, 100)),
                    default: keep == Some(id.as_str()),
                })
                .collect(),
            min_values: None,
            max_values: None,
        }],
    }
}

/// The same content as a numbered/text menu — component-less platforms post
/// this and rely on `run <name> <args>` arg syntax.
fn model_picker_fallback(
    draft: &store::ModelDraft,
    models: &[(String, String)],
    shapes: &BTreeMap<String, RowShape>,
) -> String {
    let mut out = picker_header(draft);
    for (i, (id, name)) in models.iter().enumerate() {
        let label = if name.is_empty() {
            id.clone()
        } else {
            format!("{name} ({id})")
        };
        out.push_str(&format!("\n{}. {}", i + 1, label));
    }
    // A composite row's slots read as numbered option lists; component-less
    // platforms set them through the `slot=option` arg syntax.
    if let Some(shape) = draft_shape(draft, shapes) {
        for slot in &shape.slots {
            out.push_str(&format!("\n{}:", slot.label.to_lowercase()));
            for (i, o) in slot.options.iter().enumerate() {
                let name = if o.name.is_empty() { &o.id } else { &o.name };
                out.push_str(&format!("\n  {}. {name} ({})", i + 1, o.id));
            }
        }
    }
    out.push_str("\neffort: low | medium | high | max");
    let picks = draft_shape(draft, shapes)
        .map(|shape| {
            shape
                .slots
                .iter()
                .map(|s| format!("[{}=<opt>]", s.key))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    out.push_str(&format!(
        "\nreply: model <id> {picks}[effort <level>] [fast on|off]"
    ));
    out
}

#[path = "cmd_tests.rs"]
#[cfg(test)]
mod tests;
