//! Per-session plugin startup results and the welcome stats line.
//!
//! [`probe`] boots the enabled lock plugins through the same
//! `gray_plugin::builder::active_plugins` path the agent build takes
//! (lazy manifest caches included, so most plugins cost a file read, not
//! a process). The welcome row waits on it rather than blocking startup,
//! and `/plugins` reports per-plugin load outcomes from the same store.

use std::sync::Mutex;

/// One enabled lock plugin's startup result: `error` is `Some` when the
/// sidecar did not come up this session.
#[derive(Debug, Clone)]
pub(crate) struct StartResult {
    pub name: String,
    pub error: Option<String>,
}

static RESULTS: Mutex<Option<Vec<StartResult>>> = Mutex::new(None);

/// The probe's outcome, `None` until it lands.
pub(crate) fn results() -> Option<Vec<StartResult>> {
    RESULTS.lock().ok().and_then(|r| r.clone())
}

fn store(results: Vec<StartResult>) {
    if let Ok(mut r) = RESULTS.lock() {
        *r = Some(results);
    }
}

/// Lock entries this session expects to boot: enabled under the project
/// overlay, minus lifecycle-owned `provider_only` sidecars.
fn expected_names() -> Vec<String> {
    let Ok(home) = crate::plugin_cli::home() else {
        return Vec::new();
    };
    crate::plugin_cli::list_managed(&home)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| {
            crate::plugin_cli::enabled(&home, &p.name, &p.entry)
                && p.entry.runtime_role.as_deref() != Some("provider_only")
        })
        .map(|p| p.name)
        .collect()
}

/// `lock plugin "name" failed to spawn (<err>); ignoring` → (name, err).
fn parse_spawn_warning(w: &str) -> Option<(String, String)> {
    let rest = w.strip_prefix("lock plugin ")?;
    let (name, rest) = rest.split_once(" failed to spawn (")?;
    let err = rest.strip_suffix("); ignoring")?.to_string();
    Some((name.trim_matches('"').to_string(), err))
}

/// Boot pass over the lock plugins. Loaded names come back in the plugin
/// list; spawn failures land in builder warnings, which are re-queued to
/// the profile-warning channel so the session still surfaces them.
pub(crate) async fn probe() {
    let names = expected_names();
    if names.is_empty() {
        store(Vec::new());
        return;
    }
    let _ = gray_plugin::builder::take_builder_warnings();
    let loaded: std::collections::HashSet<String> = match gray_plugin::builder::active_plugins(
        crate::profile::gray_defaults(),
        &["tools-minimal", "skills"],
        "gray.yml",
        None,
        false,
    )
    .await
    {
        Ok((plugins, _)) => plugins.iter().map(|p| p.manifest().name.clone()).collect(),
        Err(_) => Default::default(),
    };
    let warnings = gray_plugin::builder::take_builder_warnings();
    let mut spawn_err: std::collections::BTreeMap<String, String> = Default::default();
    for w in &warnings {
        if let Some((name, err)) = parse_spawn_warning(w) {
            spawn_err.insert(name, err);
        }
    }
    for w in warnings {
        crate::profile::queue_profile_warning(w);
    }
    store(
        names
            .into_iter()
            .map(|name| StartResult {
                error: if loaded.contains(&name) {
                    None
                } else {
                    Some(
                        spawn_err
                            .get(&name)
                            .cloned()
                            .unwrap_or_else(|| "did not start".to_string()),
                    )
                },
                name,
            })
            .collect(),
    );
}

/// Plugins updated by the last auto-update run, when it ran inside the
/// last 24 hours.
fn recent_updated() -> Option<u64> {
    let path = crate::setup::gray_home().ok()?.join("auto-update.json");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let at = v.get("at")?.as_u64()?;
    let now = gray_core::spill::now_secs().max(0) as u64;
    if now.saturating_sub(at) > 24 * 3600 {
        return None;
    }
    let p = v.get("plugins")?;
    Some(p.get("updated")?.as_u64().unwrap_or(0) + p.get("rebuilt")?.as_u64().unwrap_or(0))
}

/// The dim welcome row: `64 plugins · 62 loaded · 2 failed · 53 updated`
/// (zero parts omitted; `plugins`/`loaded` always shown). `None` until
/// [`probe`] lands.
pub(crate) fn stats_line() -> Option<String> {
    let results = results()?;
    let plugins = results.len();
    if plugins == 0 {
        return None;
    }
    let loaded = results.iter().filter(|r| r.error.is_none()).count();
    let failed = plugins - loaded;
    let mut parts = vec![format!("{plugins} plugins"), format!("{loaded} loaded")];
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    match recent_updated() {
        Some(u) if u > 0 => parts.push(format!("{u} updated")),
        _ => {}
    }
    if failed > 0 {
        parts.push("/plugins to see why".to_string());
    }
    Some(parts.join(" · "))
}

/// `gray cmd run plugins` / headless `/plugins`: name, version, status.
/// Runs [`probe`] first when no session probe has landed yet.
pub(crate) async fn table() -> String {
    if results().is_none() {
        probe().await;
    }
    let results = results().unwrap_or_default();
    let rows = crate::plugin_cli::list_rows().unwrap_or_default();
    if rows.is_empty() {
        return "no plugins installed".to_string();
    }
    let name_w = rows.iter().map(|r| r.name.len()).max().unwrap_or(0);
    let ver_w = rows.iter().map(|r| r.version.len()).max().unwrap_or(0);
    rows.iter()
        .map(|r| {
            let status = if !r.on {
                "disabled".to_string()
            } else {
                match results.iter().find(|s| s.name == r.name) {
                    Some(s) => match &s.error {
                        Some(e) => format!("failed: {e}"),
                        None => "loaded".to_string(),
                    },
                    None => "not loaded".to_string(),
                }
            };
            format!("{:<name_w$} {:<ver_w$} {status}", r.name, r.version)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
