use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveredFire {
    pub id: String,
    pub name: String,
    pub path: std::path::PathBuf,
    pub excerpt: String,
    pub to_chat: bool,

    pub reminder: bool,

    pub failed: bool,

    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireOutput {
    pub transcript: String,
    pub final_text: String,
}

pub struct TickReport {
    pub fired: usize,
    pub errors: usize,
    pub delivered: Vec<DeliveredFire>,
}

pub(crate) fn refresh_model_from_saved(config: &mut crate::config::Config) {
    let Ok(path) = crate::setup::saved_config_path() else {
        return;
    };
    refresh_model_from_saved_at(config, &path);
}

fn refresh_model_from_saved_at(config: &mut crate::config::Config, path: &std::path::Path) {
    let saved = crate::setup::load_saved_config_at(path);
    if let Some(model) = saved.model.filter(|m| !m.trim().is_empty()) {
        config.model = Some(model);
    }
    if let Some(base) = saved.base_url.filter(|u| !u.trim().is_empty()) {
        config.base_url = base;
    }
}

#[async_trait::async_trait(?Send)]
pub trait AsyncRunner {
    async fn run(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<String>;

    async fn run_full(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<FireOutput> {
        let text = self.run(prompt, cwd).await?;
        Ok(FireOutput {
            transcript: text.clone(),
            final_text: text,
        })
    }
}

pub struct HeadlessRunner {
    pub config: crate::config::Config,

    pub follow_switches: bool,
}

#[async_trait::async_trait(?Send)]
impl AsyncRunner for HeadlessRunner {
    async fn run(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<String> {
        Ok(self.run_full(prompt, cwd).await?.transcript)
    }

    async fn run_full(&self, prompt: String, cwd: PathBuf) -> anyhow::Result<FireOutput> {
        let mut config = self.config.clone();
        if self.follow_switches {
            refresh_model_from_saved(&mut config);
        }

        {
            let rows = crate::setup::canonical_model_rows(&config);
            crate::setup::canonicalize_effort_variant(&mut config, &rows);
        }
        let mut agent = crate::build_agent(&config, &cwd, None).await?;
        let ctx = gray_core::agent::ToolContext {
            cwd,
            cancel: tokio_util::sync::CancellationToken::new(),
            session_id: None,
        };
        let events = agent
            .run(gray_core::message::Message::user(prompt), ctx)
            .await
            .map_err(|e| anyhow::anyhow!(crate::repl::format_core_error(&e, &config.base_url)))?;
        Ok(FireOutput {
            transcript: crate::cron_fire::transcript_text(&events),
            final_text: crate::cron_fire::final_assistant_text(&events),
        })
    }
}

pub const FIRE_TIMEOUT_SECS: u64 = 600;

pub struct SaveLocalDeliver {
    pub home: PathBuf,
}

impl SaveLocalDeliver {
    pub async fn deliver_full(
        &self,
        job: &crate::cron::CronJob,
        now: i64,
        out: &FireOutput,
        elapsed_ms: u64,
    ) -> Result<DeliveredFire, String> {
        let transcript = crate::cron_fire::redact_secrets(&out.transcript, &self.home);
        let path = crate::cron_fire::write_local_output(&self.home, job, now, &transcript)
            .map_err(|e| format!("local write failed: {e:#}"))?;
        let shown = if job.reminder {
            out.final_text.clone()
        } else {
            crate::cron_fire::redact_secrets(&out.final_text, &self.home)
        };

        let excerpt = crate::cron_fire::delivery_excerpt(&shown);
        let saved = |to_chat: bool| DeliveredFire {
            id: job.id.clone(),
            name: job.name.clone(),
            path: path.clone(),
            excerpt: excerpt.clone(),
            to_chat,
            reminder: job.reminder,
            failed: false,
            elapsed_ms,
        };
        match &job.deliver {
            crate::cron::Deliver::Local => Ok(saved(false)),
            crate::cron::Deliver::Target(_) => {
                log::warn!(
                    "cron {}: unknown target {:?}, saved locally",
                    job.id,
                    job.deliver
                );
                Ok(saved(false))
            }
            crate::cron::Deliver::Origin => {
                let Some(origin) = &job.origin else {
                    log::warn!(
                        "cron {}: origin delivery without origin session, saved locally",
                        job.id
                    );
                    return Ok(saved(false));
                };

                if origin.platform == SESSION_PLATFORM {
                    post_to_session_inbox(&self.home, &origin.chat, &saved(true))
                        .map_err(|e| format!("session inbox write failed: {e:#}"))?;
                    return Ok(saved(false));
                }

                let note = crate::cron_fire::mirror_message(&job.name, &excerpt);
                let sessions =
                    crate::session_store::JsonlSessionStore::new(self.home.join("sessions"));
                let sid = crate::session_store::SessionId::new(origin.chat.clone());
                if let Err(e) = sessions
                    .append(&sid, &gray_core::message::Message::user_injected(note))
                    .await
                {
                    log::warn!(
                        "cron {}: no session {:?} to mirror into ({e:#}); delivering anyway",
                        job.id,
                        origin.chat
                    );
                }
                if is_hosted(origin) {
                    post_to_outbox(&self.home, &saved(true), origin)
                        .map_err(|e| format!("outbox write failed: {e:#}"))?;
                }
                Ok(saved(true))
            }
        }
    }
}

pub fn owner_stamp() -> String {
    format!("{}:{}", std::process::id(), uuid::Uuid::new_v4())
}

pub async fn fire_one(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    job: crate::cron::CronJob,
    now: i64,
    deliver: &SaveLocalDeliver,
) -> (crate::cron::RunStatus, Option<DeliveredFire>) {
    use crate::cron::RunStatus;
    let fail = |msg: String| {
        let _ = store.mark_done(
            &job.id,
            job.fire_claim.as_ref(),
            RunStatus::Error,
            Some(&msg),
        );
        RunStatus::Error
    };
    if job.reminder {
        let out = FireOutput {
            transcript: job.prompt.clone(),
            final_text: job.prompt.clone(),
        };
        return match deliver.deliver_full(&job, now, &out, 0).await {
            Ok(saved) => {
                let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
                (RunStatus::Ok, Some(saved))
            }
            Err(msg) => {
                let _ = store.mark_done(
                    &job.id,
                    job.fire_claim.as_ref(),
                    RunStatus::DeliveryFailed,
                    Some(&msg),
                );
                (RunStatus::DeliveryFailed, None)
            }
        };
    }
    let workdir: PathBuf = match &job.workdir {
        Some(w) => w.clone(),
        None => match std::env::current_dir() {
            Ok(c) => c,
            Err(e) => return (fail(format!("cannot resolve workdir: {e:#}")), None),
        },
    };
    if let Some(s) = &job.script
        && (!s.is_absolute() || !s.is_file())
    {
        return (fail(format!("pre-script missing: {}", s.display())), None);
    }
    let mut skill_paths = Vec::new();
    for name in &job.skills {
        match crate::skills_tool::resolve_skill_name(&workdir, name) {
            Some(p) => skill_paths.push(p),
            None => return (fail(format!("skill not found: {name}")), None),
        }
    }
    let mut script_stdout: Option<String> = None;
    if let Some(s) = &job.script {
        let outcome = crate::cron_fire::run_pre_script(s, &workdir).await;
        if !outcome.ok {
            return (
                fail(format!("pre-script failed: {}", outcome.stderr_tail)),
                None,
            );
        }
        if !crate::cron_fire::parse_wake_gate(&outcome.stdout) {
            let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
            return (RunStatus::Ok, None);
        }
        script_stdout = Some(outcome.stdout);
    }
    let prompt =
        crate::cron_fire::assemble_fire_prompt(&job.prompt, &skill_paths, script_stdout.as_deref());
    let started = std::time::Instant::now();
    let run_fut = std::panic::AssertUnwindSafe(runner.run_full(prompt, workdir));
    let out = match tokio::time::timeout(
        std::time::Duration::from_secs(FIRE_TIMEOUT_SECS),
        futures::FutureExt::catch_unwind(run_fut),
    )
    .await
    {
        Err(_) => {
            let msg = "fire exceeded 600s";
            return (
                fail(msg.to_string()),
                failure_delivery(deliver, &job, now, msg),
            );
        }
        Ok(Err(_)) => {
            let msg = "agent run panicked";
            return (
                fail(msg.to_string()),
                failure_delivery(deliver, &job, now, msg),
            );
        }
        Ok(Ok(Err(e))) => {
            let msg = format!("agent run failed: {e:#}");
            return (
                fail(msg.clone()),
                failure_delivery(deliver, &job, now, &msg),
            );
        }
        Ok(Ok(Ok(out))) => out,
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if crate::cron_fire::is_silent_response(&out.final_text) {
        let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
        return (RunStatus::Ok, None);
    }
    match deliver.deliver_full(&job, now, &out, elapsed_ms).await {
        Ok(saved) => {
            let _ = store.mark_done(&job.id, job.fire_claim.as_ref(), RunStatus::Ok, None);
            (RunStatus::Ok, Some(saved))
        }
        Err(msg) => {
            let _ = store.mark_done(
                &job.id,
                job.fire_claim.as_ref(),
                RunStatus::DeliveryFailed,
                Some(&msg),
            );
            (RunStatus::DeliveryFailed, None)
        }
    }
}

fn failure_delivery(
    deliver: &SaveLocalDeliver,
    job: &crate::cron::CronJob,
    now: i64,
    msg: &str,
) -> Option<DeliveredFire> {
    if !matches!(job.deliver, crate::cron::Deliver::Origin) || job.origin.is_none() {
        return None;
    }
    let msg = crate::cron_fire::redact_secrets(msg, &deliver.home);
    let path = crate::cron_fire::write_local_output(&deliver.home, job, now, &msg).ok()?;
    let fired = DeliveredFire {
        id: job.id.clone(),
        name: job.name.clone(),
        path,
        excerpt: msg.lines().next().unwrap_or("").chars().take(300).collect(),
        to_chat: true,
        reminder: job.reminder,
        failed: true,
        elapsed_ms: 0,
    };

    if let Some(origin) = job
        .origin
        .as_ref()
        .filter(|o| o.platform == SESSION_PLATFORM)
    {
        if let Err(e) = post_to_session_inbox(&deliver.home, &origin.chat, &fired) {
            log::warn!("cron {}: session inbox write failed: {e:#}", job.id);
        }
        return None;
    }
    if let Some(origin) = job.origin.as_ref().filter(|o| is_hosted(o))
        && let Err(e) = post_to_outbox(&deliver.home, &fired, origin)
    {
        log::warn!("cron {}: outbox write failed: {e:#}", job.id);
    }
    Some(fired)
}

pub const SESSION_PLATFORM: &str = "repl";

pub fn is_hosted(origin: &crate::cron::store::Origin) -> bool {
    origin.platform != SESSION_PLATFORM && origin.platform != "local"
}

fn post_to_outbox(
    home: &std::path::Path,
    fired: &DeliveredFire,
    origin: &crate::cron::store::Origin,
) -> anyhow::Result<()> {
    write_entry(
        &home.join("cron").join("outbox"),
        &delivery_json(fired, Some(origin)),
    )
}

pub fn drain_outbox(home: &std::path::Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(home.join("cron").join("outbox")) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path);
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("cron outbox: cannot remove {}: {e}", path.display());
            continue;
        }
        match text {
            Ok(t) if !t.trim().is_empty() => out.push(t.trim().to_string()),
            _ => log::warn!("cron outbox: dropped unreadable {}", path.display()),
        }
    }
    out
}

pub(crate) fn session_inbox(home: &std::path::Path, session: &str) -> Option<PathBuf> {
    let safe = !session.is_empty()
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    safe.then(|| home.join("cron").join("inbox").join(session))
}

fn post_to_session_inbox(
    home: &std::path::Path,
    session: &str,
    fired: &DeliveredFire,
) -> anyhow::Result<()> {
    let dir = session_inbox(home, session)
        .ok_or_else(|| anyhow::anyhow!("unsafe session id {session:?}"))?;
    let body = if fired.failed {
        format!("(failed) {}", fired.excerpt)
    } else {
        fired.excerpt.clone()
    };

    let entry = serde_json::json!({
        "card": format!("cron: {}", format_fire_chat(fired)),
        "prompt": crate::cron_fire::mirror_message(&fired.name, &body),
        "name": fired.name,
        "body": fired.excerpt,
        "status": if fired.failed { "failed" } else { "ok" },
        "reminder": fired.reminder,
        "elapsed_ms": fired.elapsed_ms,
    });
    write_entry(&dir, &entry.to_string())
}

pub(crate) fn write_entry(dir: &std::path::Path, body: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let name = format!("{stamp:020}-{}.json", uuid::Uuid::new_v4());
    let tmp = dir.join(format!(".{name}.tmp"));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    std::io::Write::write_all(&mut options.open(&tmp)?, body.as_bytes())?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

pub fn session_inbox_pending(home: &std::path::Path, session: &str) -> bool {
    session_inbox(home, session)
        .and_then(|d| std::fs::read_dir(d).ok())
        .is_some_and(|mut it| {
            it.any(|e| {
                let Ok(e) = e else { return false };
                if e.file_name().to_string_lossy().starts_with('.') {
                    return false;
                }

                !std::fs::read_to_string(e.path()).is_ok_and(|t| crate::session_send::is_held(&t))
            })
        })
}

#[derive(Debug, Clone, PartialEq)]
pub struct CronCard {
    pub name: String,
    pub body: String,
    pub failed: bool,
    pub reminder: bool,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionDelivery {
    pub card: String,
    pub prompt: String,
    pub cron: Option<CronCard>,
}

pub fn drain_session_inbox(home: &std::path::Path, session: &str) -> Vec<SessionDelivery> {
    let Some(dir) = session_inbox(home, session) else {
        return Vec::new();
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = rd
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path);

        if text
            .as_deref()
            .is_ok_and(crate::session_send::is_send_entry)
        {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            log::warn!("cron inbox: cannot remove {}: {e}", path.display());
            continue;
        }
        let parsed = text
            .ok()
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| {
                let field = |k: &str| v.get(k)?.as_str().map(str::to_string);
                let cron = field("name").map(|name| CronCard {
                    name,
                    body: field("body").unwrap_or_default(),
                    failed: field("status").as_deref() == Some("failed"),
                    reminder: v.get("reminder").and_then(|r| r.as_bool()).unwrap_or(false),
                    elapsed_ms: v.get("elapsed_ms").and_then(|e| e.as_u64()).unwrap_or(0),
                });
                Some(SessionDelivery {
                    card: field("card")?,
                    prompt: field("prompt")?,
                    cron,
                })
            });
        match parsed {
            Some(entry) => out.push(entry),
            None => log::warn!("cron inbox: dropped unreadable {}", path.display()),
        }
    }
    out
}

pub const MAX_CONCURRENT_FIRES: usize = 4;

pub async fn tick_once(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    deliver: &SaveLocalDeliver,
    kind: &str,
) -> anyhow::Result<TickReport> {
    tick_once_with(
        store,
        runner,
        deliver,
        kind,
        crate::setup::cron_auto_enabled(),
    )
    .await
}

pub(crate) async fn tick_once_with(
    store: &crate::cron::CronStore,
    runner: &dyn AsyncRunner,
    deliver: &SaveLocalDeliver,
    kind: &str,
    auto: bool,
) -> anyhow::Result<TickReport> {
    if let Err(e) = store.record_tick(kind) {
        log::warn!("cron: cannot record tick heartbeat: {e:#}");
    }

    if !auto {
        return Ok(TickReport {
            fired: 0,
            errors: 0,
            delivered: Vec::new(),
        });
    }
    let owner = owner_stamp();
    let mut fired_ids = Vec::new();
    let mut report = TickReport {
        fired: 0,
        errors: 0,
        delivered: Vec::new(),
    };
    let now = crate::cron::now_secs();
    let due = store.claim_due_limited(now, &owner, MAX_CONCURRENT_FIRES, &fired_ids)?;

    let mut pending = futures::stream::FuturesUnordered::new();
    for (idx, job) in due.into_iter().enumerate() {
        fired_ids.push(job.id.clone());
        pending.push(async move {
            let out = fire_one(store, runner, job, now, deliver).await;
            (idx, out)
        });
    }
    let mut ordered: Vec<Option<(crate::cron::RunStatus, Option<DeliveredFire>)>> = Vec::new();
    ordered.resize_with(pending.len(), || None);
    while let Some((idx, (status, saved))) = futures::StreamExt::next(&mut pending).await {
        ordered[idx] = Some((status, saved));
    }
    for slot in ordered.into_iter().flatten() {
        let (status, saved) = slot;
        report.fired += 1;
        if !matches!(status, crate::cron::RunStatus::Ok) {
            report.errors += 1;
        }
        if let Some(saved) = saved {
            report.delivered.push(saved);
        }
    }
    Ok(report)
}

pub fn format_fire_chat(saved: &DeliveredFire) -> String {
    crate::cron_fire::format_delivery_plain(
        &saved.name,
        &saved.excerpt,
        saved.reminder,
        saved.failed,
    )
}

pub fn delivery_json(saved: &DeliveredFire, origin: Option<&crate::cron::store::Origin>) -> String {
    let (platform, chat, thread, route) = match origin {
        Some(o) => (
            o.platform.clone(),
            o.chat.clone(),
            o.thread.clone(),
            o.route.clone(),
        ),
        None => (String::new(), String::new(), None, None),
    };
    serde_json::json!({
        "type": "cron_delivery",
        "job_id": saved.id,
        "name": saved.name,
        "platform": platform,
        "chat": chat,
        "thread": thread,
        "route": route,
        "kind": if saved.reminder { "reminder" } else { "task" },
        "status": if saved.failed { "failed" } else { "ok" },
        "elapsed_ms": saved.elapsed_ms,
        "final_text": saved.excerpt,
        "text": format_fire_chat(saved),
        "path": saved.path.display().to_string(),
    })
    .to_string()
}

pub async fn serve_loop(
    store: crate::cron::CronStore,
    deliver: SaveLocalDeliver,
    runner: impl AsyncRunner + 'static,
) -> anyhow::Result<()> {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            _ = interval.tick() => {
                match tick_once(&store, &runner, &deliver, "serve").await {
                    Ok(rep) => log::info!("cron tick: fired={} errors={}", rep.fired, rep.errors),
                    Err(e) => log::warn!("cron tick failed: {e:#}"),
                }
            }
        }
    }
    Ok(())
}

#[path = "cron_serve_tests.rs"]
#[cfg(test)]
mod tests;
