//! Lazy sidecar plugin: stands in for a `SidecarPlugin` without spawning
//! the process. The cached `<name>-manifest.json` (written at install and
//! refreshed by `gray plugin refresh`) answers `manifest()`/`tools()`/
//! `commands()` at boot; the child spawns on the first call that actually
//! needs the wire — a declared hook firing, one of its own commands, or a
//! `tool/call` — and every later call delegates to the live sidecar.
//!
//! Correctness relies on the same gate `SidecarPlugin` applies internally:
//! every request-based hook checks `manifest.hooks` before touching the
//! wire, so an unclaimed hook can return the trait default without a
//! process. `on_event` is a notification with no reply; dormant plugins
//! (no hooks declared) are never woken to receive it — they could not
//! consume a wire hook anyway. Plugins that declare any hook spawn on the
//! first `pre_tool` event, which precedes that hook's request path.
//!
//! Dynamic (protocol 1.3) sidecars are never wrapped: their tool set only
//! exists on the wire (`plugin/tools`), so they keep eager spawn — see
//! `builder::active_plugins`. `GRAY_LAZY_PLUGINS=0` disables lazy spawn
//! entirely (escape hatch for sidecars with unusual lifecycles).

use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde_json::Value;

use gray_core::agent::{Tool, ToolContext, ToolOutput};
use gray_core::message::{Message, ToolDef};

use crate::sidecar::HostHandler;
use crate::{CommandOutcome, CoreEvent, InputSubmit, Manifest, Plugin, SidecarPlugin, ToolBefore};

/// Read-only env gate: `GRAY_LAZY_PLUGINS` unset/anything-but-false = lazy.
pub fn lazy_enabled() -> bool {
    match std::env::var("GRAY_LAZY_PLUGINS") {
        Ok(v) => !matches!(v.as_str(), "0" | "false" | "off" | "no"),
        Err(_) => true,
    }
}

/// Load the install-time cached manifest for a lock plugin. Returns `None`
/// when absent, unparseable, mismatched, or a dynamic (1.3) manifest —
/// all of which keep the eager spawn path.
pub fn cached_manifest(path: &std::path::Path, lock_name: &str) -> Option<Manifest> {
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let m = Manifest::from_result(&v);
    if m.name != lock_name || m.protocol.as_deref() == Some(crate::DYNAMIC_PROTOCOL) {
        return None;
    }
    Some(m)
}

/// A `Plugin` that serves the cached manifest and spawns its sidecar on
/// first use. All per-plugin state the host may set before the first wire
/// call (capabilities, host handler, session id) is buffered and applied
/// to the child at spawn so nothing is lost to laziness.
pub struct LazySidecarPlugin {
    argv: Vec<String>,
    manifest: Manifest,
    live: tokio::sync::OnceCell<SidecarPlugin>,
    capabilities: Mutex<Vec<String>>,
    host_handler: Mutex<Option<HostHandler>>,
    session_id: Mutex<String>,
    self_ref: Mutex<Weak<LazySidecarPlugin>>,
}

impl LazySidecarPlugin {
    /// `argv` is the resolved spawn command; `manifest` came from the
    /// install cache. Call [`Self::arc`] to get the `Arc` with the
    /// self-weak link the lazy tools need.
    pub fn new(argv: Vec<String>, manifest: Manifest) -> Self {
        Self {
            argv,
            manifest,
            live: tokio::sync::OnceCell::new(),
            capabilities: Mutex::new(Vec::new()),
            host_handler: Mutex::new(None),
            session_id: Mutex::new(String::new()),
            self_ref: Mutex::new(Weak::new()),
        }
    }

    pub fn arc(self) -> Arc<Self> {
        let arc = Arc::new(self);
        *arc.self_ref.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&arc);
        arc
    }

    /// Spawn-once accessor: concurrent first-touches share one spawn.
    /// On success the buffered state is applied before any call delegates.
    async fn live(&self) -> Result<&SidecarPlugin, String> {
        self.live
            .get_or_try_init(|| async {
                let plugin = SidecarPlugin::spawn(self.argv.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                // Snapshot buffered state into lets: a MutexGuard
                // temporary in an `if let` scrutinee outlives the block
                // and makes the future !Send.
                let caps = self
                    .capabilities
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                plugin.set_capabilities(caps);
                let h = self
                    .host_handler
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if let Some(h) = h {
                    plugin.set_host_handler(h).await;
                }
                let sid = self
                    .session_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if !sid.is_empty() {
                    plugin.set_session_id(&sid);
                }
                Ok(plugin)
            })
            .await
    }

    /// Mirror of `SidecarPlugin::claims`: does the manifest declare `hook`?
    fn claims(&self, hook: &str) -> bool {
        self.manifest.hooks.iter().any(|h| h == hook)
    }

    /// Set-granted-capabilities: buffered, applied to the child at spawn.
    pub fn set_capabilities(&self, granted: Vec<String>) {
        *self.capabilities.lock().unwrap_or_else(|e| e.into_inner()) = granted;
    }

    /// Host handler is buffered pre-spawn and installed on the child.
    pub async fn set_host_handler(&self, handler: HostHandler) {
        if let Some(p) = self.live.get() {
            p.set_host_handler(handler).await;
        } else {
            *self.host_handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
        }
    }
}

#[async_trait]
impl Plugin for LazySidecarPlugin {
    fn manifest(&self) -> Manifest {
        self.manifest.clone()
    }

    fn capabilities(&self) -> Vec<String> {
        self.capabilities
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Lazy tools: definition from the cached manifest, execution
    /// delegates into the (spawned-on-first-call) live sidecar.
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let me = self
            .self_ref
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        self.manifest
            .tools
            .iter()
            .map(|def| {
                Arc::new(LazyTool {
                    def: def.clone(),
                    owner: me.clone(),
                }) as Arc<dyn Tool>
            })
            .collect()
    }

    fn set_session_id(&self, id: &str) {
        *self.session_id.lock().unwrap_or_else(|e| e.into_inner()) = id.to_string();
        if let Some(p) = self.live.get() {
            p.set_session_id(id);
        }
    }

    async fn prompt_context(&self, cwd: &str) -> Option<String> {
        if !self.claims("prompt/context") {
            return None;
        }
        self.live().await.ok()?.prompt_context(cwd).await
    }

    async fn tool_before(&self, name: &str, args: &Value) -> ToolBefore {
        if !self.claims("tool/before") {
            return ToolBefore::Allow;
        }
        match self.live().await {
            Ok(p) => p.tool_before(name, args).await,
            // A policy hook that can't spawn must fail closed, same as the
            // live transport's transport-error path.
            Err(e) => {
                log::warn!(target: "gray_plugin", "lazy sidecar {} spawn for tool/before failed: {e}", self.manifest.name);
                ToolBefore::Deny("plugin policy unavailable; tool not executed".to_string())
            }
        }
    }

    async fn input_submit(&self, text: &str) -> InputSubmit {
        if !self.claims("input/submit") {
            return InputSubmit::Pass;
        }
        match self.live().await {
            Ok(p) => p.input_submit(text).await,
            Err(_) => InputSubmit::Pass, // never eat input on spawn failure
        }
    }

    async fn agent_before_start(&self) -> Option<String> {
        if !self.claims("agent/before_start") {
            return None;
        }
        self.live().await.ok()?.agent_before_start().await
    }

    async fn context_build(&self, messages: &[Message]) -> Option<Vec<Message>> {
        if !self.claims("context/build") {
            return None;
        }
        self.live().await.ok()?.context_build(messages).await
    }

    async fn tool_after(&self, name: &str, output: &ToolOutput) -> Option<ToolOutput> {
        if !self.claims("tool/after") {
            return None;
        }
        self.live().await.ok()?.tool_after(name, output).await
    }

    async fn run_command(&self, name: &str, argv: Vec<String>) -> Option<CommandOutcome> {
        // Same gate as the live impl: only spawn for commands this plugin
        // actually declares — an unclaimed `run_command` costs a process.
        if !self.manifest.commands.iter().any(|c| c == name)
            && !self.manifest.subcommands.iter().any(|c| c == name)
        {
            return None;
        }
        match self.live().await {
            Ok(p) => p.run_command(name, argv).await,
            Err(e) => Some(CommandOutcome::Say(format!(
                "plugin {} failed to start: {e}",
                self.manifest.name
            ))),
        }
    }

    async fn on_event(&self, e: CoreEvent) {
        // Never spawn a dormant plugin just to deliver a notification it
        // declared no interest in (hooks-empty manifests claim no wire
        // surface that events feed). Hook-declaring plugins spawn on the
        // pre_tool event that precedes their hook anyway.
        if self.live.get().is_none() && self.manifest.hooks.is_empty() {
            return;
        }
        if let Ok(p) = self.live().await {
            p.on_event(e).await;
        }
    }

    async fn shutdown(&self) {
        if let Some(p) = self.live.get() {
            p.shutdown(std::time::Duration::from_secs(2)).await;
        }
    }
}

/// A tool whose definition is cached but whose execution spawns the
/// owning sidecar on first call, then delegates to the live tool object
/// (looked up by name so a refreshed tool set is honored).
struct LazyTool {
    def: ToolDef,
    owner: Weak<LazySidecarPlugin>,
}

#[async_trait]
impl Tool for LazyTool {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn def(&self) -> ToolDef {
        self.def.clone()
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolOutput {
        let Some(owner) = self.owner.upgrade() else {
            return ToolOutput::error(format!("plugin tool {} unloaded", self.def.name));
        };
        let name = self.def.name.clone();
        let plugin = match owner.live().await {
            Ok(p) => p,
            Err(e) => {
                return ToolOutput::error(format!(
                    "plugin {} failed to start for {name}: {e}",
                    owner.manifest.name
                ));
            }
        };
        let Some(tool) = plugin.tools().into_iter().find(|t| t.def().name == name) else {
            return ToolOutput::error(format!("plugin tool {name} not found after spawn"));
        };
        tool.execute(ctx, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(tools: usize, hooks: &[&str], commands: &[&str]) -> Manifest {
        Manifest::from_result(&json!({
            "name": "demo",
            "version": "0.1",
            "protocol": "1.1",
            "tools": (0..tools).map(|i| json!({"name": format!("t{i}"), "description": "d", "parameters": {}})).collect::<Vec<_>>(),
            "hooks": hooks,
            "commands": commands,
        }))
    }

    #[test]
    fn cached_manifest_rejects_dynamic_and_mismatch() {
        let dir = std::env::temp_dir().join(format!("lazy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("demo-manifest.json");
        std::fs::write(
            &p,
            r#"{"name":"demo","version":"0.1","protocol":"1.3","tools":[]}"#,
        )
        .unwrap();
        assert!(cached_manifest(&p, "demo").is_none(), "1.3 must stay eager");
        std::fs::write(
            &p,
            r#"{"name":"other","version":"0.1","protocol":"1.1","tools":[]}"#,
        )
        .unwrap();
        assert!(
            cached_manifest(&p, "demo").is_none(),
            "name mismatch = stale cache"
        );
        std::fs::write(&p, "not json").unwrap();
        assert!(cached_manifest(&p, "demo").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn dormant_calls_never_spawn() {
        // A command-only sidecar (no hooks, no tools): every trait call
        // resolves from the manifest without a process.
        let p = LazySidecarPlugin::new(
            vec!["/nonexistent/binary".into()],
            manifest(0, &[], &["/demo"]),
        )
        .arc();
        assert!(p.run_command("/other", vec![]).await.is_none());
        assert!(p.prompt_context("/tmp").await.is_none());
        assert_eq!(p.input_submit("hi").await, crate::InputSubmit::Pass);
        p.on_event(CoreEvent::TurnEnd {
            usage: gray_core::event::Usage::default(),
        })
        .await;
        assert!(p.live.get().is_none(), "dormant plugin must not spawn");
        assert_eq!(p.manifest().name, "demo");
        assert_eq!(p.tools().len(), 0);
    }

    #[tokio::test]
    async fn unclaimed_command_and_events_skip_spawn() {
        // Hook-declaring plugin: claimed commands spawn; the rest stay cold.
        let p = LazySidecarPlugin::new(
            vec!["/nonexistent/binary".into()],
            manifest(2, &["prompt/context"], &[]),
        )
        .arc();
        assert!(p.run_command("/nope", vec![]).await.is_none());
        assert!(p.live.get().is_none());
        // A claimed hook DOES attempt the spawn (which fails — bad argv —
        // but proves the gate opened).
        assert!(p.prompt_context("/tmp").await.is_none());
        assert!(p.live.get().is_none(), "failed spawn leaves cell empty");
        assert_eq!(p.tools().len(), 2);
    }
}
