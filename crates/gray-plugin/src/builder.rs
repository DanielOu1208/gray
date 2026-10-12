//! One profile-aware agent builder for every surface (REPL, `-p`, cron).
//!
//! Lives here (not in `gray`) so every host shares one builder without depending on the binary.
//!
//! Surface policy stays with the callers: the system prompt (skills/context),
//! the executor wrapper (plain vs `DenyExecutor`), the
//! host handler, and abort-vs-warn on sidecar spawn failure all arrive via
//! [`BuilderOptions`]. Cron needs no direct call — the sidecar fires through
//! `host/run` (`gray -p`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use gray_core::agent::{Agent, Tool, ToolExecutor};
use gray_core::credential::CredentialSource;
use gray_provider::{OpenAiProvider, OpenAiProviderProfile};
use gray_tools::Registry;

use crate::profile::{PluginEntry, load_entries};
use crate::{HostHandler, Manifest, Plugin, PluginHookAdapter, SidecarPlugin, merge_manifests};

// ---------------------------------------------------------------------------
// Builtin surface: a single in-process `bash` tool owned by the registry
// itself — not a plugin, so manifests and `plugin list` show real installs
// only. `from_plugins` registers it on the session ledger.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Registry assembly (moved from `gray::profile`; same later-wins semantics)
// ---------------------------------------------------------------------------

/// A plugin whose tool set is live (protocol 1.3): its tools are resolved
/// per call by [`LiveRegistry`] rather than snapshotted into the registry.
fn is_live_plugin(p: &dyn Plugin) -> bool {
    p.manifest().protocol.as_deref() == Some("1.3")
}

/// The plugins `from_plugins` left out of the static registry, in order.
pub fn dynamic_plugins(plugins: &[Arc<dyn Plugin>]) -> Vec<Arc<dyn Plugin>> {
    plugins
        .iter()
        .filter(|p| is_live_plugin(p.as_ref()))
        .cloned()
        .collect()
}

/// Collects tools from plugins in order; on name conflict the owner wins
/// (later manifests win, mirroring `merge_manifests`). Returns the registry
/// plus the manifests in registration order — manifests travel with the
/// registry so `--dump-manifest` can't drift from what's registered.
pub fn from_plugins(plugins: &[Arc<dyn Plugin>]) -> (Registry, Vec<Manifest>) {
    let manifests: Vec<Manifest> = plugins.iter().map(|p| p.manifest()).collect();
    let owners = merge_manifests(manifests.clone());
    // The builtin surface is not a plugin: one `bash` tool built here, on
    // the session ledger the binary's /new + compaction lifecycle acts on
    // via `current_file_ledger`. Fresh per build on purpose: a reused
    // ledger would leak reads across sessions in multi-session hosts.
    let ledger = Arc::new(gray_tools::FileLedger::new());
    let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(
        gray_tools::BashTool::default().with_ledger(ledger.clone()),
    )];
    for p in plugins {
        let owner_name = p.manifest().name;
        // Protocol-1.3 plugins change their tool set while the session
        // runs; `LiveRegistry` resolves those per call instead of
        // snapshotting them here.
        if is_live_plugin(p.as_ref()) {
            continue;
        }
        let may_override = p
            .capabilities()
            .iter()
            .any(|c| c == crate::capabilities::TOOL_OVERRIDE);
        for t in p.tools() {
            // Replacing the built-in `bash` needs the capability, and the
            // capability needs consent: a silently winning sidecar would
            // intercept everything routed through that tool.
            if t.def().name == "bash" && !may_override {
                push_builder_warning(format!(
                    "plugin `{}` claims built-in tool `bash`; grant `{}` to let it (gray plugin capabilities {})",
                    owner_name,
                    crate::capabilities::TOOL_OVERRIDE,
                    owner_name
                ));
                continue;
            }
            if owners
                .get(&t.def().name)
                .map(|o| o == &owner_name)
                .unwrap_or(false)
            {
                if let Some(pos) = tools.iter().position(|e| e.def().name == t.def().name) {
                    tools[pos] = t.clone();
                } else {
                    tools.push(t.clone());
                }
            }
        }
    }
    let registry = Registry::new(tools);
    track_current_ledger(&ledger);
    (registry, manifests)
}

// ---------------------------------------------------------------------------
// Session ledger lifecycle (T3.4)
// ---------------------------------------------------------------------------

// Latest build's FileLedger (Weak: builds own their ledger; the binary
// clears/disarms via this handle for /new, resume, and compaction while
// gray-core stays ignorant of tools).
static CURRENT_LEDGER: std::sync::Mutex<Option<std::sync::Weak<gray_tools::FileLedger>>> =
    std::sync::Mutex::new(None);

/// The ledger adopted by the latest [`from_plugins`] build, if still alive.
pub fn current_file_ledger() -> Option<Arc<gray_tools::FileLedger>> {
    CURRENT_LEDGER.lock().ok()?.as_ref()?.upgrade()
}

fn track_current_ledger(ledger: &Arc<gray_tools::FileLedger>) {
    if let Ok(mut g) = CURRENT_LEDGER.lock() {
        *g = Some(Arc::downgrade(ledger));
    }
}

// ---------------------------------------------------------------------------
// Builder warnings (same queue-then-drain pattern as the REPL's profile queue;
// each surface drains in its own idiom: transcript vs log)
// ---------------------------------------------------------------------------

static BUILD_WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn push_builder_warning(msg: String) {
    BUILD_WARNINGS
        .lock()
        .map(|mut q| {
            if !q.contains(&msg) {
                q.push(msg);
            }
        })
        .ok();
}

/// Drains warnings queued during [`active_plugins`] (unknown names, skipped
/// sidecars, unparseable profile).
pub fn take_builder_warnings() -> Vec<String> {
    BUILD_WARNINGS
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct SpawnOutcome {
    pub name: String,
    pub error: Option<String>,
}

static SPAWN_OUTCOMES: std::sync::Mutex<Option<Vec<SpawnOutcome>>> = std::sync::Mutex::new(None);

pub fn spawn_outcomes() -> Option<Vec<SpawnOutcome>> {
    SPAWN_OUTCOMES.lock().ok().and_then(|s| s.clone())
}

/// Resolve the spawn argv for a plugin dir: the dir itself when
/// executable, else `plugin.sh`, else the single executable inside.
///
/// Single definition shared by the host (`active_plugins` below) and
/// `gray::plugin_check` — do not invent a second resolution rule.
pub fn resolve_argv(dir: &Path) -> anyhow::Result<Vec<String>> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| {
        let Ok(m) = std::fs::metadata(p) else {
            return false;
        };
        if !m.is_file() {
            return false;
        }
        // Executable bit is unix-only; on Windows any file qualifies.
        #[cfg(unix)]
        {
            m.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    };
    if is_exec(dir) {
        return Ok(vec![dir.to_string_lossy().into_owned()]);
    }
    if !dir.is_dir() {
        anyhow::bail!("{} is not a directory (or executable)", dir.display());
    }
    let script = dir.join("plugin.sh");
    if is_exec(&script) {
        return Ok(vec![script.to_string_lossy().into_owned()]);
    }
    let mut execs = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if is_exec(&path) {
            execs.push(path);
        }
    }
    match execs.len() {
        1 => Ok(vec![execs[0].to_string_lossy().into_owned()]),
        0 => anyhow::bail!(
            "no executable in {} (expected plugin.sh or one executable)",
            dir.display()
        ),
        _ => anyhow::bail!(
            "ambiguous plugin dir {}: several executables, add plugin.sh",
            dir.display()
        ),
    }
}

/// User lock + project overlay (`<cwd>/.gray/plugins.json`, same shape)
/// for lock-entry spawning and the disabled filter. Missing files are empty
/// (silent); corrupt files yield one warning each (paths only, never
/// argv/URLs). `None` home skips the user scope; the project overlay still
/// applies.
fn load_lock_files(cwd: &Path) -> (crate::lock::LockFile, crate::lock::LockFile, Vec<String>) {
    use std::collections::BTreeMap;
    let mut warnings = Vec::new();
    let mut load = |path: PathBuf| -> crate::lock::LockFile {
        match crate::lock::LockFile::load(&path) {
            Ok(lf) => lf,
            Err(e) => {
                warnings.push(format!("cannot load {} ({e:#}); ignoring", path.display()));
                crate::lock::LockFile {
                    schema: 1,
                    plugins: BTreeMap::new(),
                }
            }
        }
    };
    // `$GRAY_HOME` else `$HOME/.gray` — `gray_core::paths` owns the rule;
    // this crate must not depend on `gray-pkg` (networking lives there).
    let user = gray_core::paths::gray_home()
        .map(|h| load(crate::lock::lock_path(&h)))
        .unwrap_or(crate::lock::LockFile {
            schema: 1,
            plugins: BTreeMap::new(),
        });
    let project = load(crate::lock::project_lock_path(cwd));
    (user, project, warnings)
}

// ---------------------------------------------------------------------------
// Profile resolution
// ---------------------------------------------------------------------------

/// Ordered active plugins: the profile file order followed by enabled
/// lock-file installs, or `defaults` when both are empty. Sidecars spawn
/// once per build with `handler` installed (`host/run`/`host/say`).
/// A profile-sidecar spawn failure aborts when `abort_on_spawn_failure`
/// (interactive boot) else warns + skips (the daemon must stay up).
/// Lock-entry spawn failures always warn + skip (names only, never argv).
/// Unknown builtin names always warn + skip. Sidecars disabled in the
/// plugin lock (`enabled: false` in the user lock, `<cwd>/.gray/plugins.json`
/// overlay winning per name) warn + skip before spawning — the flag never
/// aborts, even when `abort_on_spawn_failure`.
///
/// This is the single production host: profile entries first, then enabled
/// lock entries (real installs resolve their executable from
/// `<home>/plugins/<name>` via [`resolve_argv`]; legacy entries
/// with an explicit `argv` spawn it directly). See [`crate::boot`] (kept
/// as a test-only harness) for the legacy split.
/// `defaults` is the catalog of builtin plugins a `gray.yml` may name;
/// `default_names` selects which of them is the no-profile fallback (the
/// default surface). Decoupled so a catalog can offer opt-in builtins while
/// booting only a subset.
pub async fn active_plugins(
    defaults: Vec<Arc<dyn Plugin>>,
    default_names: &[&str],
    profile_path: &str,
    handler: Option<HostHandler>,
    abort_on_spawn_failure: bool,
) -> anyhow::Result<(Vec<Arc<dyn Plugin>>, bool)> {
    // Profile entries: missing file is the default state (silent, empty);
    // anything else (parse error) warns once via the caller's drain.
    let entries: Vec<PluginEntry> = match load_entries(profile_path) {
        Ok(entries) => entries,
        Err(e) => {
            let missing = e
                .downcast_ref::<std::io::Error>()
                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
            if !missing {
                push_builder_warning(format!(
                    "cannot load {profile_path} profile ({e}); using builtin plugins"
                ));
            }
            Vec::new()
        }
    };

    // Lock state once per build (file reads, not per sidecar).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (user_lock, project_lock, lock_warnings) = load_lock_files(&cwd);
    for w in lock_warnings {
        push_builder_warning(w);
    }
    // Legacy exact-argv disabled set (non-empty-argv entries only).
    let disabled = crate::lock::disabled_sidecar_argvs(&user_lock, &project_lock);
    // Path correlation for real installs (empty-argv locks): resolved
    // executable paths + install dirs of disabled entries, so a profile
    // sidecar pointing at the same install matches even though the lock
    // argv is empty.
    // Lock entries install into `<home>/plugins`, mirroring `gray-pkg`
    // (which owns the lockfile writes this crate must not depend on).
    let pdir = gray_core::paths::gray_home().map(|h| h.join("plugins"));
    let mut disabled_paths: Vec<String> = Vec::new();
    let mut disabled_dirs: Vec<String> = Vec::new();
    for name in user_lock.plugins.keys() {
        if crate::lock::effective_enabled(&user_lock, &project_lock, name) {
            continue;
        }
        if let Some(pd) = &pdir {
            let dir = pd.join(name);
            disabled_dirs.push(dir.to_string_lossy().into_owned());
            if let Ok(argv) = resolve_argv(&dir) {
                disabled_paths.extend(argv);
            }
        }
    }
    let profile_disabled = |spec: &[String]| -> bool {
        if !spec.is_empty() && disabled.contains(&spec.to_vec()) {
            return true;
        }
        // Single-path specs (dir or executable) match a disabled install
        // by resolved executable or install dir.
        if spec.len() == 1 {
            if disabled_paths.contains(&spec[0]) || disabled_dirs.contains(&spec[0]) {
                return true;
            }
            if let Ok(argv) = resolve_argv(Path::new(&spec[0]))
                && argv.iter().any(|a| disabled_paths.contains(a))
            {
                return true;
            }
        }
        false
    };

    // User-installed sidecars extend the default surface when no profile exists;
    // their presence must not suppress the default bash tool.
    let mut plugins = if entries.is_empty() {
        defaults
            .iter()
            .filter(|p| default_names.contains(&p.manifest().name.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    for (i, e) in entries.iter().enumerate() {
        match e {
            PluginEntry::Builtin(n) => {
                match defaults.iter().find(|p| p.manifest().name == *n).cloned() {
                    Some(p) => plugins.push(p),
                    None => push_builder_warning(format!(
                        "unknown plugin {n:?} in {profile_path} — ignoring"
                    )),
                }
            }
            PluginEntry::Sidecar(spec) => {
                // Auto-fetch installers (`npx -y`, `uvx`, `pip install`) pull
                // mutable code at spawn: only install sidecars you trust.
                if spec.0.iter().any(|a| a == "-y" || a == "--yes")
                    || spec
                        .0
                        .first()
                        .is_some_and(|p| p.ends_with("npx") || p.ends_with("uvx"))
                {
                    push_builder_warning(format!(
                        "sidecar[{i}] auto-downloads its package ({}) — only install sidecars you trust",
                        spec.0.join(" ")
                    ));
                }
                if profile_disabled(&spec.0) {
                    push_builder_warning(format!(
                        "sidecar[{i}] disabled in plugin lock — skipping"
                    ));
                    continue;
                }
                let label = spec.0.join(" ");
                match SidecarPlugin::spawn(spec.0.clone()).await {
                    Ok(p) => {
                        if let Some(h) = &handler {
                            p.set_host_handler(h.clone()).await;
                        }
                        // A profile sidecar is argv the operator wrote into
                        // gray.yml: placement is the consent, so it keeps
                        // everything it declares.
                        p.set_capabilities(p.manifest().capabilities.clone());
                        plugins.push(Arc::new(p) as Arc<dyn Plugin>);
                    }
                    Err(e) if abort_on_spawn_failure => {
                        return Err(e)
                            .with_context(|| format!("sidecar[{i}] ({label}) failed to spawn"));
                    }
                    Err(e) => push_builder_warning(format!(
                        "sidecar[{i}] ({label}) failed to spawn, skipping: {e:#}"
                    )),
                }
            }
        }
    }

    // Enabled lock entries (the project overlay wins per name on the flag;
    // the overlay toggles, it never discovers — project-only names have no
    // known argv/dir, so only user names spawn). Real installs (empty argv)
    // resolve their executable from the install dir; legacy entries spawn
    // their recorded argv. Failures warn (names only) and never abort.
    let mut outcomes: Vec<SpawnOutcome> = Vec::new();
    for (name, entry) in user_lock.plugins.iter() {
        if !crate::lock::effective_enabled(&user_lock, &project_lock, name) {
            continue;
        }
        // Provider-only sidecars are lifecycle-owned by the provider runtime.
        // They stay in the lock for provider discovery, but never boot as a
        // second sidecar process.
        if entry.runtime_role.as_deref() == Some("provider_only") {
            continue;
        }
        if let Some(p) =
            spawn_lock_plugin(name, entry, pdir.as_deref(), &handler, &mut outcomes).await
        {
            plugins.push(p);
        }
    }

    // `global` lock entries live in the real user home's lock and boot in
    // every session: a gateway/subagent GRAY_HOME's own lock doesn't know
    // them. When session and user home are the same the loop above already
    // booted them; a name the session lock has always wins.
    let global_home = gray_core::paths::gray_user_home();
    let same_home = match (&gray_core::paths::gray_home(), &global_home) {
        (Some(a), Some(b)) => {
            a == b || std::fs::canonicalize(a).ok() == std::fs::canonicalize(b).ok()
        }
        (None, None) => true,
        _ => false,
    };
    if !same_home && let Some(gh) = global_home {
        let gpath = crate::lock::lock_path(&gh);
        let global_lock = match crate::lock::LockFile::load(&gpath) {
            Ok(lf) => lf,
            Err(e) => {
                push_builder_warning(format!("cannot load {} ({e:#}); ignoring", gpath.display()));
                crate::lock::LockFile {
                    schema: 1,
                    plugins: std::collections::BTreeMap::new(),
                }
            }
        };
        let gpdir = gh.join("plugins");
        let booted: std::collections::HashSet<String> =
            plugins.iter().map(|p| p.manifest().name.clone()).collect();
        for (name, entry) in global_lock.plugins.iter() {
            if !entry.global
                || !crate::lock::effective_enabled(&global_lock, &project_lock, name)
                || entry.runtime_role.as_deref() == Some("provider_only")
                || booted.contains(name)
            {
                continue;
            }
            if let Some(p) =
                spawn_lock_plugin(name, entry, Some(&gpdir), &handler, &mut outcomes).await
            {
                p.set_global(true);
                plugins.push(p);
            }
        }
    }
    if let Ok(mut s) = SPAWN_OUTCOMES.lock() {
        *s = Some(outcomes);
    }

    if plugins.is_empty() {
        let fallback: Vec<Arc<dyn Plugin>> = defaults
            .into_iter()
            .filter(|p| default_names.contains(&p.manifest().name.as_str()))
            .collect();
        return Ok((fallback, true));
    }
    // Later manifests win on name conflict (same rule as `merge_manifests`
    // and the legacy boot harness).
    let mut deduped: Vec<Arc<dyn Plugin>> = Vec::new();
    for p in plugins {
        let name = p.manifest().name.clone();
        if let Some(pos) = deduped.iter().position(|e| e.manifest().name == name) {
            deduped.remove(pos);
        }
        deduped.push(p);
    }
    let fallback = entries.is_empty() && user_lock.plugins.values().all(|entry| !entry.enabled);
    Ok((deduped, fallback))
}

/// Spawn one enabled lock entry: `pdir` is the owning home's plugins dir
/// (manifest cache + install root). Records the outcome, warns on spawn
/// failure, and returns the plugin on success.
async fn spawn_lock_plugin(
    name: &str,
    entry: &crate::lock::LockEntry,
    pdir: Option<&Path>,
    handler: &Option<HostHandler>,
    outcomes: &mut Vec<SpawnOutcome>,
) -> Option<Arc<dyn Plugin>> {
    let argv: Vec<String> = if !entry.argv.is_empty() {
        entry.argv.clone()
    } else {
        let pd = pdir?;
        match resolve_argv(&pd.join(name)) {
            Ok(a) => a,
            Err(e) => {
                outcomes.push(SpawnOutcome {
                    name: name.to_string(),
                    error: Some(format!("{e:#}")),
                });
                return None;
            }
        }
    };
    // Lazy spawn: the install-time `<name>-manifest.json` cache answers
    // manifest/tools/commands at boot and the child spawns on the first
    // real wire use (see [`crate::lazy`]). Dynamic (1.3) sidecars,
    // missing/stale caches, and `GRAY_LAZY_PLUGINS=0` keep eager spawn.
    let lazy_manifest = if crate::lazy::lazy_enabled() {
        pdir.and_then(|pd| {
            crate::lazy::cached_manifest(&pd.join(format!("{name}-manifest.json")), name)
        })
    } else {
        None
    };
    if let Some(m) = lazy_manifest {
        let p = crate::lazy::LazySidecarPlugin::new(argv.clone(), m);
        if let Some(h) = handler {
            p.set_host_handler(h.clone()).await;
        }
        // Consent decides what this sidecar may ask of the host —
        // declared capabilities read from the cached manifest, same
        // list the live spawn would report.
        let declared = p.manifest().capabilities.clone();
        p.set_capabilities(
            crate::capabilities::granted_for(entry, &declared)
                .into_iter()
                .collect(),
        );
        let pending = crate::capabilities::pending_consent(entry, &declared);
        if !pending.is_empty() {
            push_builder_warning(format!(
                "plugin {name:?} declares ungranted {} ({}); grant with gray plugin capabilities {name} --all",
                if pending.len() == 1 {
                    "capability".to_string()
                } else {
                    "capabilities".to_string()
                },
                pending.join(", ")
            ));
        }
        outcomes.push(SpawnOutcome {
            name: name.to_string(),
            error: None,
        });
        return Some(p.arc() as Arc<dyn Plugin>);
    }
    match SidecarPlugin::spawn(argv).await {
        Ok(p) => {
            if let Some(h) = handler {
                p.set_host_handler(h.clone()).await;
            }
            // Consent decides what this sidecar may ask of the host.
            // A pre-consent entry keeps what it declares (it ran with
            // those powers before consent existed); a consented one is
            // cut down to the intersection.
            let declared = p.manifest().capabilities.clone();
            p.set_capabilities(
                crate::capabilities::granted_for(entry, &declared)
                    .into_iter()
                    .collect(),
            );
            let pending = crate::capabilities::pending_consent(entry, &declared);
            if !pending.is_empty() {
                push_builder_warning(format!(
                    "plugin {name:?} declares ungranted {} ({}); grant with gray plugin capabilities {name} --all",
                    if pending.len() == 1 {
                        "capability".to_string()
                    } else {
                        "capabilities".to_string()
                    },
                    pending.join(", ")
                ));
            }
            outcomes.push(SpawnOutcome {
                name: name.to_string(),
                error: None,
            });
            Some(Arc::new(p) as Arc<dyn Plugin>)
        }
        Err(e) => {
            push_builder_warning(format!(
                "lock plugin {name:?} failed to spawn ({e:#}); ignoring"
            ));
            outcomes.push(SpawnOutcome {
                name: name.to_string(),
                error: Some(format!("{e:#}")),
            });
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Provider cache key (moved from `gray`; same truncate-don't-hash rule)
// ---------------------------------------------------------------------------

/// Max `prompt_cache_key` length (matches the Responses API cache-key limit).
pub const PROMPT_CACHE_KEY_MAX_LENGTH: usize = 64;

/// Clamp a cache key to the max length (truncate, don't hash —
/// the prefix stays human-grepable in logs).
pub fn clamp_prompt_cache_key(key: &str) -> &str {
    if key.len() <= PROMPT_CACHE_KEY_MAX_LENGTH {
        return key;
    }
    // UUIDs are ASCII so byte cut == char cut; walk back over a boundary just in case.
    let mut end = PROMPT_CACHE_KEY_MAX_LENGTH;
    while !key.is_char_boundary(end) {
        end -= 1;
    }
    &key[..end]
}

/// Resolves the Responses `prompt_cache_key`: the session id when known
/// (stable across resumes, so a resumed session keeps its cache shard), else
/// a per-process stable id (rebuilds mid-session must not bust the shard).
/// A fresh random key per build guaranteed 0% cache; never do that.
pub fn provider_cache_key(session_id: Option<&str>) -> String {
    if let Some(s) = session_id.filter(|s| !s.is_empty()) {
        return clamp_prompt_cache_key(s).to_string();
    }
    static FALLBACK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FALLBACK
        .get_or_init(|| uuid::Uuid::new_v4().to_string())
        .clone()
}

// ---------------------------------------------------------------------------
// The single builder
// ---------------------------------------------------------------------------

/// Builds the `system` prompt: either a ready string or a
/// registry-aware closure (REPL/`-p` tool snippets + guidelines need the
/// resolved registry, which only exists after profile resolution).
pub enum SystemPrompt {
    Literal(String),
    Build(PromptBuilder),
}

/// `FnOnce(&Registry) -> String`: caller builds its prompt from the resolved
/// registry (snippets, names, guidelines).
pub type PromptBuilder = Box<dyn FnOnce(&Registry) -> String + Send>;

/// Wraps the profile-built registry executor (a `DenyExecutor` wrap;
/// `None` = plain registry).
pub type ExecutorWrap = Box<dyn FnOnce(Arc<dyn ToolExecutor>) -> Arc<dyn ToolExecutor> + Send>;

pub struct BuilderOptions {
    pub model: String,
    pub api_key: String,
    pub base_url: String,
    pub reasoning_effort: Option<String>,
    /// Sampling passthroughs for providers that accept them (None = server default).
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    /// Known model context window in tokens (`None` = unknown: only
    /// overflow-recovery compaction runs).
    pub context_window: Option<usize>,
    /// Pins the Responses cache shard; callers thread their session id so
    /// sessions don't all collide on the per-process fallback key.
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub system_prompt: SystemPrompt,
    /// Surface plugins that are always active regardless of the profile
    /// (gray: none — bash-only, empty by default).
    /// Appended after profile + lock plugins; on tool-name conflict the
    /// later manifest wins, so these always win their own tool names.
    pub extra_plugins: Vec<Arc<dyn Plugin>>,
    pub host_handler: Option<HostHandler>,
    pub profile_path: String,
    /// Bare run: the implicit `bash` surface only; no gray.yml profile, no lock sidecars.
    pub bare: bool,
    pub abort_on_spawn_failure: bool,
    pub wrap_executor: Option<ExecutorWrap>,
    /// Declared plugin-backed provider profile; `None` preserves the
    /// built-in static API-key provider.
    pub dynamic_provider_profile: Option<OpenAiProviderProfile>,
    /// Agent-lifetime credential lease source for `dynamic_provider_profile`.
    pub dynamic_credential_source: Option<Arc<dyn CredentialSource>>,
    pub prebooted_plugins: Option<Vec<Arc<dyn Plugin>>>,
}

/// Profile-aware [`Agent`] builder used by all surfaces: resolves
/// `profile_plugins → Registry::from_plugins else builtin`, wires sidecar
/// hooks, and constructs the provider. Surface warnings (unknown plugins,
/// skipped sidecars) queue in [`take_builder_warnings`] for the caller to
/// drain; spawn aborts only when `abort_on_spawn_failure`.
pub async fn build_agent(opts: BuilderOptions) -> anyhow::Result<Agent> {
    let BuilderOptions {
        model,
        api_key,
        base_url,
        reasoning_effort,
        temperature,
        top_p,
        context_window,
        session_id,
        cwd,
        system_prompt,
        extra_plugins,
        host_handler,
        profile_path,
        bare,
        abort_on_spawn_failure,
        wrap_executor,
        dynamic_provider_profile,
        dynamic_credential_source,
        prebooted_plugins,
    } = opts;
    // No builtin plugins in this crate's catalog: the `bash` surface is not
    // a plugin — `from_plugins` registers it directly — so a `gray.yml`
    // `builtin:` entry (a retired tier name, or anything else unknown)
    // just warns and skips.
    let mut plugins = if let Some(prebooted) = prebooted_plugins {
        prebooted
    } else if bare {
        Vec::new()
    } else {
        active_plugins(
            Vec::new(),
            &[],
            &profile_path,
            host_handler,
            abort_on_spawn_failure,
        )
        .await?
        .0
    };
    // Always-on surface plugins (see `extra_plugins`): appended after the
    // profile + lock set so they survive a profile that names nothing.
    // Same-name dedupe as `active_plugins` (later wins) keeps a profile
    // entry with the same name from doubling up.
    for p in extra_plugins {
        let name = p.manifest().name.clone();
        if let Some(pos) = plugins.iter().position(|e| e.manifest().name == name) {
            plugins.remove(pos);
        }
        plugins.push(p);
    }
    // The agent's session id reaches sidecars so `prompt/context` can pin a
    // session-scoped answer (a plugin serving per-session state, like a
    // memory snapshot, replays identical bytes on rebuild). Empty = the
    // anonymous case.
    for p in &plugins {
        p.set_session_id(session_id.as_deref().unwrap_or(""));
    }
    let (registry, manifests) = from_plugins(&plugins);
    let dynamic = dynamic_plugins(&plugins);
    let ledger = current_file_ledger();
    let system = match system_prompt {
        SystemPrompt::Literal(s) => s,
        SystemPrompt::Build(f) => f(&registry),
    };
    let provider: Box<dyn gray_core::agent::Provider> =
        match (dynamic_provider_profile, dynamic_credential_source) {
            (Some(profile), Some(source)) => Box::new(
                OpenAiProvider::new_with_profile(
                    model,
                    reasoning_effort,
                    Some(provider_cache_key(session_id.as_deref())),
                    profile,
                    source,
                )
                .map_err(|e| anyhow::anyhow!("failed to initialize plugin provider: {e}"))?
                .with_sampling(temperature, top_p),
            ),
            // Anthropic's own host speaks the native Messages wire: its
            // OpenAI-compatible endpoint has no prompt caching.
            (None, None) if gray_provider::anthropic::is_anthropic_base_url(&base_url) => Box::new(
                gray_provider::AnthropicProvider::new(api_key, model, base_url, reasoning_effort)
                    .map_err(|e| anyhow::anyhow!("failed to initialize Anthropic provider: {e}"))?
                    .with_sampling(temperature, top_p),
            ),
            (None, None) => Box::new(
                OpenAiProvider::new(
                    api_key,
                    model,
                    base_url,
                    reasoning_effort,
                    Some(provider_cache_key(session_id.as_deref())),
                )
                .map_err(|e| anyhow::anyhow!("failed to initialize OpenAI provider: {e}"))?
                .with_sampling(temperature, top_p),
            ),
            _ => anyhow::bail!("a dynamic provider needs both a profile and a credential source"),
        };

    let tool_defs = registry.defs();
    // Display-only headlines (`label` in a manifest `tools` entry):
    // renderer-injected, never model-visible (providers project
    // name/description/parameters only).
    let tool_labels: Vec<(String, String)> = manifests
        .iter()
        .flat_map(|m| m.tools.iter())
        .filter_map(|t| {
            t.label
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| (t.name.clone(), s.to_string()))
        })
        .collect();
    let tool_previews: Vec<(String, String)> = manifests
        .iter()
        .flat_map(|m| m.tools.iter())
        .filter_map(|t| {
            t.preview
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| (t.name.clone(), s.to_string()))
        })
        .collect();
    // Live (protocol-1.3) plugin tools are resolved per call and merged
    // into the advertised defs by the agent's per-turn refresh.
    let registry = Arc::new(registry);
    let base: Arc<dyn ToolExecutor> = if dynamic.is_empty() {
        registry
    } else {
        Arc::new(crate::LiveRegistry::new(registry, dynamic))
    };
    let executor: Arc<dyn ToolExecutor> = match wrap_executor {
        Some(wrap) => wrap(base),
        None => base,
    };
    let hooks = PluginHookAdapter::for_plugins(&plugins, &cwd.to_string_lossy());
    Ok(Agent::new(provider, executor)
        .with_system(system)
        .with_tools(tool_defs)
        .with_tool_labels(tool_labels)
        .with_tool_previews(tool_previews)
        .with_context_window(context_window)
        .with_history_rewrite_hook(Arc::new(move || {
            if let Some(ledger) = &ledger {
                ledger.disarm_all_dedup();
            }
        }))
        .with_hooks(hooks))
}

#[path = "builder_tests.rs"]
#[cfg(test)]
mod tests;
