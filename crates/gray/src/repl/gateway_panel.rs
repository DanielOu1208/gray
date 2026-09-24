//! `/gateway` connections panel: just the apps gray talks to (toggleable).
//!
//! The app rows come from the merged plugin registry, so a transport shows up
//! the day it is installed (slack, telegram, …). Setup state comes from each
//! app own declaration via `crate::plugin_cli::setup_decl`.
//! Daemon/cron/memory are named nowhere here: `gray gateway status`, `/cron`
//! and `/memory` are the commands that own them, and this panel repeating
//! those names was narration with nothing to do.

use super::*;
use crate::setup::{ManagerItem, ManagerSpec, format_plugin_row_parts, run_install_manager};

const GATEWAY_SPEC: ManagerSpec = ManagerSpec {
    title: "Connections",
    empty_hint: "no apps installed — gray install plugin discord",
    error_verb: "toggle failed",
    supports_toggle: true,
    supports_remove: false,
    errors_tab: false,
    keep_stale_on_relist_error: true,
};

/// A rule row: the visual break between apps and the pointers.
/// Subcommands an app declares for itself. Empty when it registered no
/// manifest (or no home resolves) — nothing is invented on its behalf.
fn declared(home: Option<&Path>, name: &str) -> Vec<String> {
    home.map(|h| crate::plugin_cli::declared_subcommands(h, name))
        .unwrap_or_default()
}

/// One toggleable row per installed app: the `/plugin` row shape plus what
/// the app still needs (setup) and what it says it can do. `home` is the gray
/// home the manifests and app configs are read from.
pub(crate) fn app_rows_with(
    rows: &[crate::plugin_cli::ManagedRow],
    home: Option<&Path>,
) -> Vec<ManagerItem> {
    rows.iter()
        .map(|r| {
            let mut row =
                format_plugin_row_parts(&r.name, &r.version, &r.scope, &r.ecosystem, r.on);
            let mut extras: Vec<String> = Vec::new();
            let mut needs_setup = false;
            if let Some(home) = home
                && let Some(decl) = crate::plugin_cli::setup_decl(&r.name)
            {
                match app_state(&home.join(decl.config_path), decl) {
                    AppState::NeedsSetup => {
                        extras.push("needs setup".to_string());
                        needs_setup = true;
                    }
                    // Stopped but fully configured: the daemon is not
                    // running, so Enter must start it (route to the setup
                    // flow), not merely toggle enable/disable.
                    AppState::Stopped => {
                        extras.push("stopped".to_string());
                        needs_setup = true;
                    }
                    AppState::Connected(Some(bot)) => extras.push(format!("connected as {bot}")),
                    AppState::Connected(None) => extras.push("connected".to_string()),
                }
            }
            extras.extend(declared(home, &r.name));
            if !extras.is_empty() {
                row.push_str(" \u{2014} ");
                row.push_str(&extras.join(" \u{b7} "));
            }
            ManagerItem {
                name: r.name.clone(),
                row,
                lit: r.on,
                enabled: r.on,
                read_only: false,
                needs_setup,
            }
        })
        .collect()
}

/// The app's live state, from files only: the config the app itself writes,
/// the pidfile the daemon keeps next to it, and the identity the daemon
/// writes when its gateway connects. Nothing here opens a socket or reads a
/// token; a daemon whose process died reports stopped even if its state file
/// is still warm on disk.
pub(crate) enum AppState {
    NeedsSetup,
    Stopped,
    /// The daemon is connected; the name it reports, when it wrote one.
    Connected(Option<String>),
}

/// The gateway's portable probe (kill(0) on unix, OpenProcess on Windows).
/// Never re-read /proc here: this row renders on macOS and Windows too.
fn pid_alive(pid: u64) -> bool {
    crate::gateway::pid::pid_alive(pid as u32)
}

fn json_at(path: &std::path::Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub(crate) fn app_state(
    config_path: &std::path::Path,
    decl: &crate::setup::registry::SetupDecl,
) -> AppState {
    if !config_path.exists() {
        return AppState::NeedsSetup;
    }
    // A config missing a required key (a token with no home channel) is a
    // partial setup, not a stopped daemon — the row still routes Enter to
    // the setup flow. Values are never read, only key presence.
    if decl
        .fields
        .iter()
        .filter(|f| f.is_required())
        .any(|f| !crate::setup::registry::key_present(config_path, f.key))
    {
        return AppState::NeedsSetup;
    }
    let Some(dir) = config_path.parent() else {
        return AppState::Stopped;
    };
    let daemon = json_at(&dir.join("daemon.json"));
    let alive = daemon
        .as_ref()
        .and_then(|v| v.get("pid").and_then(serde_json::Value::as_u64))
        .is_some_and(pid_alive);
    if !alive {
        return AppState::Stopped;
    }
    let bot = json_at(&dir.join("state.json")).and_then(|v| {
        v.get("bot")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    });
    AppState::Connected(bot)
}

/// One toggleable row per installed app, read from the live registry.
pub(crate) fn app_rows(rows: &[crate::plugin_cli::ManagedRow]) -> Vec<ManagerItem> {
    // The needs-setup state reads the *user's* home (apps write their
    // configs under ~/.config); the gray home holds only gray's own
    // registries. Unresolvable home -> no probe, never "needs setup".
    app_rows_with(rows, crate::setup::user_home().ok().as_deref())
}

/// The whole panel: apps (or the install hint), the rule, the pointers.
pub(crate) fn items() -> Vec<ManagerItem> {
    let rows = crate::plugin_cli::list_rows().unwrap_or_default();
    let mut out = app_rows(&rows);
    if out.is_empty() {
        out.push(ManagerItem {
            name: String::new(),
            row: GATEWAY_SPEC.empty_hint.to_string(),
            lit: false,
            enabled: false,
            read_only: true,
            needs_setup: false,
        });
    }
    out
}

/// Headless rendering of the same rows (piped stdin, tests).
pub(crate) fn format_text() -> String {
    items()
        .iter()
        .map(|i| i.row.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The connections picker. Returns whether anything was toggled.
pub(crate) fn run_gateway_modal(
    bg: Option<&crate::setup::BackgroundSnapshot>,
) -> anyhow::Result<bool> {
    // Enter on a needs-setup row opens that app's setup flow.
    let setup: crate::setup::SetupAction<'_> =
        &|item: &ManagerItem| crate::setup::app_flow::run_app_setup_modal(&item.name);
    run_install_manager(
        bg,
        Some(setup),
        &GATEWAY_SPEC,
        || Some(items()),
        // Removal is not offered here: /plugin owns it.
        |_| anyhow::bail!("remove apps with /plugin remove <name>"),
        crate::plugin_cli::set_managed_enabled,
    )
}

#[path = "gateway_panel_tests.rs"]
#[cfg(test)]
mod tests;
