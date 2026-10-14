use super::*;
use crate::setup::{ManagerItem, ManagerSpec, format_plugin_row_parts, run_install_manager};

const GATEWAY_SPEC: ManagerSpec = ManagerSpec {
    title: "Connections",
    empty_hint: "no apps installed — gray plugin install <name>",
    error_verb: "toggle failed",
    supports_toggle: true,
    supports_remove: false,
    errors_tab: false,
    keep_stale_on_relist_error: true,
};

fn declared(home: Option<&Path>, name: &str) -> Vec<String> {
    home.map(|h| crate::plugin_cli::declared_subcommands(h, name))
        .unwrap_or_default()
}

pub(crate) fn app_rows_with(
    rows: &[crate::plugin_cli::ManagedRow],
    home: Option<&Path>,
) -> Vec<ManagerItem> {
    rows.iter()
        .map(|r| {
            let mut row =
                format_plugin_row_parts(&r.name, &r.version, &r.scope, &r.ecosystem, r.on);
            let mut extras = declared(home, &r.name);
            if extras.iter().any(|s| s == "setup") {
                extras.push(format!("gray {} setup", r.name));
            }
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
                needs_setup: false,
            }
        })
        .collect()
}

pub(crate) fn app_rows(rows: &[crate::plugin_cli::ManagedRow]) -> Vec<ManagerItem> {
    app_rows_with(rows, crate::plugin_cli::home().ok().as_deref())
}

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

pub(crate) fn format_text() -> String {
    items()
        .iter()
        .map(|i| i.row.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn run_gateway_modal(
    bg: Option<&crate::setup::BackgroundSnapshot>,
) -> anyhow::Result<bool> {
    run_install_manager(
        bg,
        None,
        &GATEWAY_SPEC,
        || Some(items()),

        |_| anyhow::bail!("remove apps with /plugin remove <name>"),
        crate::plugin_cli::set_managed_enabled,
    )
}

#[path = "gateway_panel_tests.rs"]
#[cfg(test)]
mod tests;
