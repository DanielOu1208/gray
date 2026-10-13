//! Plugin profile: gray-surface policy over the shared builder.
//!
//! Profile→registry→hooks assembly lives once in
//! [`gray_plugin::builder`] (lowest common crate — keeps the shared builder
//! out of any single binary). This module keeps gray's
//! surface policy: the implicit `bash` surface (not a plugin) plus the
//! always-on context-only [`crate::skills_tool::SkillsPlugin`]
//! (per-turn `<available_skills>` list, no tools), the transcript-safe
//! warning queue, and the `--dump-manifest` registry view.

use std::sync::Arc;

use gray_plugin::Plugin;

pub use gray_plugin::builder::from_plugins;

use crate::skills_tool::SkillsPlugin;

/// Builtin plugin catalog: what a `gray.yml` `builtin:` entry may name.
/// The `bash` surface is not a plugin (the builder registers it directly),
/// so `skills` is the only builtin a profile can pick; a `builtin:` naming
/// a retired tools tier — or anything unknown — warns and skips, and the
/// bash surface is always there regardless.
pub(crate) fn gray_defaults() -> Vec<Arc<dyn Plugin>> {
    vec![
        // Always on: per-turn `<available_skills>` context, no tools.
        // (The live agent path in `lib::build_agent` appends the same via
        // `extra_plugins`; this covers `--dump-manifest`/`builtin_registry`.)
        Arc::new(SkillsPlugin::default()) as Arc<dyn Plugin>,
    ]
}

/// Profile warnings queued for transcript display. Raw `eprintln!` while the
/// composer viewport is live collides with the next draw (ghost/overlapped
/// rows), so lib code never prints — it queues here and the UI drains.
/// One lock, one Vec: each distinct message is queued once per drain cycle
/// (N is tiny; Vec scan is fine). A rebuild re-queues a still-broken profile
/// warning — correct, like a compiler re-emitting warnings.
static PROFILE_WARNINGS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

pub(crate) fn queue_profile_warning(msg: String) {
    PROFILE_WARNINGS
        .lock()
        .map(|mut q| {
            if !q.contains(&msg) {
                q.push(msg);
            }
        })
        .ok();
}

/// Drains queued profile warnings (transcript/non-TUI display owns rendering).
pub fn take_profile_warnings() -> Vec<String> {
    PROFILE_WARNINGS
        .lock()
        .map(|mut q| std::mem::take(&mut *q))
        .unwrap_or_default()
}

fn drain_builder_warnings() {
    for w in gray_plugin::builder::take_builder_warnings() {
        queue_profile_warning(w);
    }
}

/// Ordered active plugins: the `gray.yml` profile order, or builtins when
/// the profile is missing/unparseable/empty. Manifest-only boot (no host
/// handler); a sidecar spawn failure aborts boot with entry index + argv.
pub(crate) async fn active_plugins(
    handler: Option<gray_plugin::HostHandler>,
) -> anyhow::Result<(Vec<Arc<dyn Plugin>>, bool)> {
    let (mut plugins, fallback) = gray_plugin::builder::active_plugins(
        gray_defaults(),
        // Fallback when no profile resolves: the implicit `bash` surface +
        // skills context.
        &["skills"],
        "gray.yml",
        handler,
        true,
    )
    .await?;
    drain_builder_warnings();
    // Always-on context-only `skills` plugin: appended after the profile +
    // lock set so it survives a sidecar-only profile too
    // (same later-wins dedupe as the builder's `extra_plugins`;
    // mirrors `lib::build_agent`). Carries no tools — tools stay bash-only.
    if !fallback {
        let name = "skills";
        if let Some(pos) = plugins.iter().position(|e| e.manifest().name == name) {
            plugins.remove(pos);
        }
        plugins.push(Arc::new(SkillsPlugin::default()) as Arc<dyn Plugin>);
    }
    Ok((plugins, fallback))
}

/// The default builtin registry (no profile file): the implicit `bash`
/// surface plus the context-only `skills` plugin (so `/context` tool
/// estimates match the live agent).
pub fn builtin_registry() -> gray_tools::Registry {
    from_plugins(&[Arc::new(SkillsPlugin::default()) as Arc<dyn Plugin>]).0
}

/// Builds the tool registry from the `gray.yml` profile plugin order,
/// falling back to builtins when no profile file is present.
/// Returns `(registry, manifests, used_fallback)` — the flag feeds
/// `--dump-manifest`'s note.
/// A sidecar spawn failure is a hard `Err` naming the entry (caller aborts boot).
pub async fn build_registry()
-> anyhow::Result<(gray_tools::Registry, Vec<gray_plugin::Manifest>, bool)> {
    let (plugins, fallback) = active_plugins(None).await?;
    let (registry, manifests) = from_plugins(&plugins);
    Ok((registry, manifests, fallback))
}
