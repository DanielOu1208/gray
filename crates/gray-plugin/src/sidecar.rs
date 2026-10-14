use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, oneshot};
use tokio::time::{Duration, timeout};

use gray_core::agent::{CommandOutcome, InputSubmit, Tool, ToolContext, ToolOutput};
use gray_core::credential::CredentialMaterial;
use gray_core::message::{Message, ToolDef};

use crate::{
    CoreEvent, DYNAMIC_PROTOCOL, Manifest, PROVIDER_CREDENTIALS, Plugin, ProviderAuthPoll,
    ProviderAuthStart, ProviderChatRequest, ProviderChatResult, ProviderModelCatalog,
    ProviderModelsRequest, ProviderRefreshRequest, ProviderRevokeRequest, ProviderRevokeResult,
    ProviderRpcError, ToolBefore, manifest_tools,
};

pub type HostHandler = Arc<
    dyn Fn(
            String,
            serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = serde_json::Value> + Send + 'static>,
        > + Send
        + Sync,
>;

pub type NotifyHandler = Arc<dyn Fn(String, serde_json::Value) + Send + Sync>;

pub const HOST_RUN: &str = "host/run";
pub const HOST_SAY: &str = "host/say";

pub const HOST_ASK: &str = "host/ask";

pub const HOST_TTL: Duration = Duration::from_secs(30);

pub const HOST_TOOLS_CHANGED: &str = "host/tools_changed";

pub const TOOLS_CHANGED_DEBOUNCE: Duration = Duration::from_millis(200);

pub const ASK_TTL: Duration = Duration::from_secs(330);

pub const ASK_HANDLER_TTL: Duration = Duration::from_secs(300);

fn session_json(id: &str, cwd: &str) -> Value {
    json!({"id": id, "cwd": cwd})
}

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

enum FrameWrite {
    Ok,
    Failed(String),
    TimedOut,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestSensitivity {
    Normal,
    Sensitive,
}

#[cfg(windows)]
async fn try_write_frame(stdin: &std::sync::Arc<Mutex<ChildStdin>>, frame: &str) -> FrameWrite {
    let stdin = Arc::clone(stdin);
    let frame = frame.to_owned();
    let mut guard = stdin.lock_owned().await;
    let write = tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("sidecar write runtime builds");
        runtime.block_on(async move { guard.write_all(frame.as_bytes()).await })
    });
    match timeout(WRITE_TIMEOUT, write).await {
        Ok(Ok(Ok(()))) => FrameWrite::Ok,
        Ok(Ok(Err(e))) => FrameWrite::Failed(format!("{e}")),
        Ok(Err(e)) => FrameWrite::Failed(format!("sidecar stdin writer failed: {e}")),
        Err(_) => FrameWrite::TimedOut,
    }
}

#[cfg(not(windows))]
async fn try_write_frame(stdin: &std::sync::Arc<Mutex<ChildStdin>>, frame: &str) -> FrameWrite {
    let mut guard = stdin.lock().await;
    match timeout(WRITE_TIMEOUT, guard.write_all(frame.as_bytes())).await {
        Ok(Ok(())) => FrameWrite::Ok,
        Ok(Err(e)) => FrameWrite::Failed(format!("{e}")),
        Err(_) => FrameWrite::TimedOut,
    }
}

struct Pending {
    epoch: u64,
    map: HashMap<u64, oneshot::Sender<Value>>,
}

struct Transport {
    child: Mutex<Child>,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<Pending>>,
    next_id: AtomicU64,
    argv: Vec<String>,

    host_handler: Arc<Mutex<Option<HostHandler>>>,

    notify: Arc<std::sync::Mutex<Option<NotifyHandler>>>,

    host_slots: Arc<tokio::sync::Semaphore>,

    grants: Arc<std::sync::Mutex<BTreeSet<String>>>,
}

pub struct SidecarPlugin {
    manifest: Manifest,

    tools: Arc<std::sync::RwLock<Vec<Arc<dyn Tool>>>>,
    transport: Arc<Transport>,

    cwd: String,

    asks: bool,

    session_id: std::sync::Mutex<String>,

    global: std::sync::atomic::AtomicBool,
}

fn spawn_retrying_etxtbsy(
    mut spawn: impl FnMut() -> std::io::Result<Child>,
) -> std::io::Result<Child> {
    match spawn() {
        Err(e) if e.raw_os_error() == Some(26) => {
            std::thread::sleep(std::time::Duration::from_millis(25));
            spawn()
        }
        other => other,
    }
}

fn spawn_child(argv: &[String]) -> anyhow::Result<(Child, ChildStdin, ChildStdout)> {
    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty argv"))?;

    #[cfg(windows)]
    if prog.to_ascii_lowercase().ends_with(".sh") {
        let script = prog.replace('\\', "/");
        let mut cmd = Command::new(gray_tools::shell::shell_path()?);
        cmd.arg("-c")
            .arg("exec \"$@\"")
            .arg("sh")
            .arg(script)
            .args(args);
        let child = spawn_retrying_etxtbsy(|| {
            cmd.stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
        })?;
        return finish_spawn(child);
    }
    let child = spawn_retrying_etxtbsy(|| {
        Command::new(prog)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(sidecar_stderr(prog))
            .kill_on_drop(true)
            .spawn()
    })?;
    finish_spawn(child)
}

fn sidecar_stderr(prog: &str) -> std::process::Stdio {
    use std::io::Write;
    let Some(home) = std::env::var_os("HOME") else {
        return std::process::Stdio::inherit();
    };
    let dir = std::path::Path::new(&home).join(".gray").join("logs");
    let _ = std::fs::create_dir_all(&dir);
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("sidecar-stderr.log"))
    {
        Ok(mut f) => {
            let _ = writeln!(f, "--- spawn {prog} (gray pid {})", std::process::id());
            std::process::Stdio::from(f)
        }
        Err(_) => std::process::Stdio::inherit(),
    }
}

fn finish_spawn(mut child: Child) -> anyhow::Result<(Child, ChildStdin, ChildStdout)> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    Ok((child, stdin, stdout))
}

const MAX_FRAME: usize = 256 * 1024;

const MAX_HOST_TASKS: usize = 4;

const MAX_PENDING: usize = 64;

#[allow(clippy::too_many_arguments)]
fn spawn_reader(
    stdout: ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Arc<Mutex<Pending>>,
    host_handler: Arc<Mutex<Option<HostHandler>>>,
    notify: Arc<std::sync::Mutex<Option<NotifyHandler>>>,
    host_slots: Arc<tokio::sync::Semaphore>,
    grants: Arc<std::sync::Mutex<BTreeSet<String>>>,

    plugin_name: String,
    epoch: u64,
) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);

        let mut oversize_streak = 0u32;
        loop {
            let mut buf = Vec::new();
            let n = match (&mut reader)
                .take((MAX_FRAME + 1) as u64)
                .read_until(b'\n', &mut buf)
                .await
            {
                Ok(n) => n,
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            if n > MAX_FRAME {
                oversize_streak += 1;
                if oversize_streak > 64 {
                    break;
                }
                continue;
            }
            oversize_streak = 0;
            let Ok(v) = serde_json::from_slice::<Value>(&buf) else {
                continue;
            };

            if let (Some(id), Some(method)) = (
                v.get("id").cloned(),
                v.get("method").and_then(|m| m.as_str()),
            ) && id.is_string()
                && method.starts_with("host/")
            {
                let params = v.get("params").cloned().unwrap_or(Value::Null);
                let method_owned = method.to_string();
                let stdin = stdin.clone();
                let host_handler = host_handler.clone();
                let host_slots = host_slots.clone();

                let grants = grants.clone();
                let plugin_name = plugin_name.clone();

                let ungranted = {
                    let set = grants.lock().unwrap_or_else(|e| e.into_inner());
                    crate::capabilities::capability_for_host_method(&method_owned)
                        .filter(|cap| !set.contains(*cap))
                        .map(|cap| cap.to_string())
                };
                if let Some(cap) = ungranted {
                    let reply = json!({"id": id, "result": {
                        "error": format!("capability_not_granted: {cap}"),
                        "hint": format!("gray plugin capabilities {plugin_name}"),
                    }});
                    if !matches!(
                        try_write_frame(&stdin, &format!("{reply}\n")).await,
                        FrameWrite::Ok
                    ) {
                        log::warn!("sidecar capability reply could not be written");
                    }
                    continue;
                }

                let Ok(_permit) = host_slots.try_acquire_owned() else {
                    let reply = json!({"id": id, "result": {"error": "host overloaded"}});

                    if !matches!(
                        try_write_frame(&stdin, &format!("{reply}\n")).await,
                        FrameWrite::Ok
                    ) {
                        log::warn!("sidecar overload reply could not be written");
                    }
                    continue;
                };

                let handler_ttl = if method_owned == HOST_ASK {
                    ASK_HANDLER_TTL
                } else {
                    HOST_TTL
                };
                tokio::spawn(async move {
                    let _permit = _permit;
                    let handler = host_handler.lock().await.clone();
                    let result = match handler {
                        Some(h) => timeout(handler_ttl, h(method_owned.clone(), params))
                            .await
                            .unwrap_or_else(
                                |_| json!({"error": format!("{method_owned} timed out")}),
                            ),
                        None => json!({"error": format!("no host handler for {method_owned}")}),
                    };
                    let reply = json!({"id": id, "result": result});
                    match try_write_frame(&stdin, &format!("{reply}\n")).await {
                        FrameWrite::Ok => {}
                        FrameWrite::Failed(e) => {
                            log::warn!("sidecar reply write failed ({e}); dropping the permit");
                        }
                        FrameWrite::TimedOut => {
                            log::warn!("sidecar stopped reading stdin; dropping a host permit");
                        }
                    }
                });
                continue;
            }

            if v.get("id").is_none()
                && let Some(method) = v.get("method").and_then(|m| m.as_str())
                && method.starts_with("host/")
            {
                let handler = notify.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(h) = handler {
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    h(method.to_string(), params);
                }
                continue;
            }
            let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
                continue;
            };
            let tx = pending.lock().await.map.remove(&id);
            if let Some(tx) = tx {
                let _ = tx.send(v.get("result").cloned().unwrap_or(Value::Null));
            }
        }
        let mut p = pending.lock().await;
        if p.epoch == epoch {
            p.map.clear();
        }
    });
}

impl Transport {
    fn name_of(argv: &[String]) -> String {
        argv.first()
            .and_then(|p| Path::new(p).file_stem())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "this plugin".to_string())
    }

    fn new(child: Child, stdin: ChildStdin, stdout: ChildStdout, argv: Vec<String>) -> Arc<Self> {
        let pending = Arc::new(Mutex::new(Pending {
            epoch: 0,
            map: HashMap::new(),
        }));
        let stdin = Arc::new(Mutex::new(stdin));
        let host_handler = Arc::new(Mutex::new(None));
        let notify = Arc::new(std::sync::Mutex::new(None));
        let host_slots = Arc::new(tokio::sync::Semaphore::new(MAX_HOST_TASKS));
        let grants: Arc<std::sync::Mutex<BTreeSet<String>>> =
            Arc::new(std::sync::Mutex::new(BTreeSet::new()));
        spawn_reader(
            stdout,
            stdin.clone(),
            pending.clone(),
            host_handler.clone(),
            notify.clone(),
            host_slots.clone(),
            grants.clone(),
            Self::name_of(&argv),
            0,
        );
        Arc::new(Self {
            child: Mutex::new(child),
            stdin,
            pending,
            next_id: AtomicU64::new(1),
            argv,
            host_handler,
            notify,
            host_slots,
            grants,
        })
    }

    async fn ensure_alive(&self) -> bool {
        let mut child = self.child.lock().await;
        let status = match child.try_wait() {
            Ok(None) => return true,
            Ok(Some(s)) => s.to_string(),
            Err(e) => e.to_string(),
        };
        let name = Self::name_of(&self.argv);
        log::warn!(target: "gray_plugin", "sidecar child exited, respawning");
        match spawn_child(&self.argv) {
            Ok((new_child, stdin, stdout)) => {
                *child = new_child;
                *self.stdin.lock().await = stdin;
                let mut p = self.pending.lock().await;
                p.epoch += 1;
                p.map.clear();
                spawn_reader(
                    stdout,
                    self.stdin.clone(),
                    self.pending.clone(),
                    self.host_handler.clone(),
                    self.notify.clone(),
                    self.host_slots.clone(),
                    self.grants.clone(),
                    Self::name_of(&self.argv),
                    p.epoch,
                );
                self.say(format!(
                    "plugin {name} exited ({status}) — restarted; see ~/.gray/logs/sidecar-stderr.log"
                ))
                .await;
                true
            }
            Err(e) => {
                log::warn!(target: "gray_plugin", "sidecar respawn failed: {e}");
                self.say(format!(
                    "plugin {name} exited ({status}) and could not be restarted: {e}; see ~/.gray/logs/sidecar-stderr.log"
                ))
                .await;
                false
            }
        }
    }

    async fn say(&self, text: String) {
        if let Some(h) = self.host_handler.lock().await.clone() {
            let _ = h(HOST_SAY.to_string(), json!({"text": text})).await;
        }
    }

    async fn terminate_child_generation(&self) {
        let mut child = self.child.lock().await;
        #[cfg(windows)]
        if let Some(pid) = child.id() {
            let pid = pid.to_string();
            let mut taskkill = Command::new("taskkill");
            taskkill.kill_on_drop(true).args(["/PID", &pid, "/T", "/F"]);
            let _ = timeout(Duration::from_secs(2), taskkill.status()).await;
        }
        let _ = child.start_kill();
    }

    async fn send_notification(&self, method: &str, params: Value) -> bool {
        let req = json!({"method": method, "params": params});
        if !self.ensure_alive().await {
            return false;
        }

        matches!(
            try_write_frame(&self.stdin, &format!("{req}\n")).await,
            FrameWrite::Ok
        )
    }

    async fn request(
        &self,
        method: &str,
        params: Option<Value>,
        ttl: Duration,
    ) -> anyhow::Result<Value> {
        self.request_with_sensitivity(method, params, ttl, RequestSensitivity::Normal)
            .await
    }

    async fn request_sensitive(
        &self,
        method: &str,
        params: Option<Value>,
        ttl: Duration,
    ) -> anyhow::Result<Value> {
        self.request_with_sensitivity(method, params, ttl, RequestSensitivity::Sensitive)
            .await
    }

    async fn request_with_sensitivity(
        &self,
        method: &str,
        params: Option<Value>,
        ttl: Duration,
        sensitivity: RequestSensitivity,
    ) -> anyhow::Result<Value> {
        let started = std::time::Instant::now();
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut req = json!({"id": id, "method": method});
        if let Some(p) = params {
            req["params"] = p;
        }
        let frame = format!("{req}\n");
        if frame.len() > MAX_FRAME {
            let error = anyhow::anyhow!("sidecar request frame too large ({method})");
            if sensitivity == RequestSensitivity::Sensitive {
                log::debug!(target: "gray_plugin", "provider rpc method={} frame_bytes={} elapsed_ms={} outcome=error", method, frame.len(), started.elapsed().as_millis());
            }
            return Err(error);
        }

        let outcome = timeout(ttl, async {
            if !self.ensure_alive().await {
                anyhow::bail!("sidecar child dead and respawn failed");
            }
            {
                let pending = self.pending.lock().await;
                if pending.map.len() >= MAX_PENDING {
                    anyhow::bail!("sidecar overloaded: too many in-flight requests");
                }
            }
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.map.insert(id, tx);
            match try_write_frame(&self.stdin, &frame).await {
                FrameWrite::Ok => {}
                FrameWrite::Failed(e) => {
                    self.pending.lock().await.map.remove(&id);

                    self.terminate_child_generation().await;
                    anyhow::bail!("sidecar write failed ({e}); killed this child generation");
                }
                FrameWrite::TimedOut => {
                    self.pending.lock().await.map.remove(&id);

                    self.terminate_child_generation().await;
                    anyhow::bail!(
                        "sidecar stopped reading stdin; killed this child generation ({method})"
                    );
                }
            }
            rx.await
                .map_err(|_| anyhow::anyhow!("sidecar child closed stdout"))
        })
        .await;
        self.pending.lock().await.map.remove(&id);
        let result = match outcome {
            Ok(result) => result,
            Err(_) => Err(anyhow::anyhow!("sidecar request timed out ({method})")),
        };
        if sensitivity == RequestSensitivity::Sensitive {
            log::debug!(target: "gray_plugin", "provider rpc method={} frame_bytes={} elapsed_ms={} outcome={}", method, frame.len(), started.elapsed().as_millis(), if result.is_ok() { "ok" } else { "error" });
        }
        result
    }
}

fn build_tools(transport: &Arc<Transport>, tools: &Value, asks: bool) -> Vec<Arc<dyn Tool>> {
    manifest_tools(&json!({"tools": tools}))
        .into_iter()
        .map(|entry| Arc::new(SidecarTool::new(entry, transport.clone(), asks)) as Arc<dyn Tool>)
        .collect()
}

async fn refresh_tools_into(
    transport: &Arc<Transport>,
    tools: &std::sync::RwLock<Vec<Arc<dyn Tool>>>,
    asks: bool,
) -> anyhow::Result<usize> {
    let r = transport
        .request("plugin/tools", Some(json!({})), HOST_TTL)
        .await?;
    let new = build_tools(transport, r.get("tools").unwrap_or(&Value::Null), asks);
    let n = new.len();
    *tools.write().unwrap_or_else(|e| e.into_inner()) = new;
    Ok(n)
}

impl SidecarPlugin {
    pub async fn spawn(argv: Vec<String>) -> anyhow::Result<Self> {
        let started = std::time::Instant::now();
        let spawned = Self::spawn_inner(argv).await;
        if let Ok(p) = &spawned {
            log::debug!(target: "gray_timing", "sidecar spawn name={} elapsed_ms={}", p.manifest.name, started.elapsed().as_millis());
        }
        spawned
    }

    async fn spawn_inner(argv: Vec<String>) -> anyhow::Result<Self> {
        let (child, stdin, stdout) = spawn_child(&argv)?;
        let transport = Transport::new(child, stdin, stdout, argv.clone());
        let result = transport
            .request("plugin/manifest", None, Duration::from_secs(30))
            .await?;
        let mut manifest = Manifest::from_result(&result);
        let name = manifest.name.trim().to_string();
        if name.is_empty() {
            anyhow::bail!(
                "sidecar manifest has missing/empty name (argv: {})",
                argv.join(" ")
            );
        }
        manifest.name = name;
        let dynamic = manifest.protocol.as_deref() == Some(DYNAMIC_PROTOCOL);
        let asks = manifest.protocol.as_deref() == Some("1.1") || dynamic;
        let tools = Arc::new(std::sync::RwLock::new(build_tools(
            &transport,
            result.get("tools").unwrap_or(&Value::Null),
            asks,
        )));
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let plugin = Self {
            manifest,
            tools,
            transport,
            cwd,
            asks,
            session_id: std::sync::Mutex::new(String::new()),
            global: std::sync::atomic::AtomicBool::new(false),
        };
        if dynamic {
            plugin.install_tools_changed_handler();

            let transport = Arc::downgrade(&plugin.transport);
            let tools = Arc::downgrade(&plugin.tools);
            let asks = plugin.asks;
            let name = plugin.manifest.name.clone();
            tokio::spawn(async move {
                let (Some(transport), Some(tools)) = (transport.upgrade(), tools.upgrade()) else {
                    return;
                };
                if let Err(e) = refresh_tools_into(&transport, &tools, asks).await {
                    log::warn!(target: "gray_plugin", "{name}: plugin/tools refresh after spawn failed: {e}");
                }
            });
        }
        Ok(plugin)
    }

    pub fn is_dynamic(&self) -> bool {
        self.manifest.protocol.as_deref() == Some(DYNAMIC_PROTOCOL)
    }

    pub async fn refresh_tools(&self) -> anyhow::Result<usize> {
        if !self.is_dynamic() {
            return Ok(self.tools.read().unwrap_or_else(|e| e.into_inner()).len());
        }
        refresh_tools_into(&self.transport, &self.tools, self.asks).await
    }

    fn install_tools_changed_handler(&self) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let transport = Arc::downgrade(&self.transport);
        let tools = Arc::downgrade(&self.tools);
        let asks = self.asks;
        let name = self.manifest.name.clone();
        let pending = Arc::new(AtomicBool::new(false));
        self.set_notify_handler(Arc::new(move |method, _params| {
            if method != HOST_TOOLS_CHANGED {
                return;
            }
            if transport.upgrade().is_none() || tools.upgrade().is_none() {
                return;
            }
            if pending.swap(true, Ordering::SeqCst) {
                return;
            }
            let (transport, tools, pending, name) = (
                transport.clone(),
                tools.clone(),
                pending.clone(),
                name.clone(),
            );
            rt.spawn(async move {
                tokio::time::sleep(TOOLS_CHANGED_DEBOUNCE).await;
                pending.store(false, Ordering::SeqCst);
                let (Some(transport), Some(tools)) = (transport.upgrade(), tools.upgrade()) else {
                    return;
                };
                if let Err(e) = refresh_tools_into(&transport, &tools, asks).await {
                    log::warn!(target: "gray_plugin", "{name}: plugin/tools refresh failed: {e}");
                }
            });
        }));
    }

    fn pinned_sid(&self) -> String {
        self.session_id.lock().expect("session_id").clone()
    }

    pub async fn shutdown(&self, grace: Duration) {
        if self.manifest.protocol.is_some() {
            let _ = self
                .transport
                .send_notification("plugin/shutdown", json!({"reason": "session_end"}))
                .await;
        }
        let mut child = self.transport.child.lock().await;
        let _ = timeout(grace, child.wait()).await;
        let _ = child.kill().await;
    }

    pub fn set_capabilities(&self, granted: Vec<String>) {
        let mut set = self
            .transport
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        *set = granted
            .into_iter()
            .filter(|c| !c.trim().is_empty())
            .collect();
    }

    pub fn capabilities(&self) -> Vec<String> {
        self.transport
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    fn require_provider_credentials(&self) -> Result<(), ProviderRpcError> {
        if self
            .capabilities()
            .iter()
            .any(|capability| capability == PROVIDER_CREDENTIALS)
        {
            Ok(())
        } else {
            Err(ProviderRpcError::CapabilityMissing(
                PROVIDER_CREDENTIALS.to_owned(),
            ))
        }
    }

    async fn provider_rpc(
        &self,
        method: &str,
        params: Value,
        ttl: Duration,
    ) -> Result<Value, ProviderRpcError> {
        self.require_provider_credentials()?;
        let value = self
            .transport
            .request_sensitive(method, Some(params), ttl)
            .await
            .map_err(|error| ProviderRpcError::Unavailable(error.to_string()))?;
        if let Some(error) = value.get("error") {
            let failure = serde_json::from_value(error.clone()).map_err(|_| {
                ProviderRpcError::Protocol("provider RPC returned an invalid error".into())
            })?;
            return Err(ProviderRpcError::Rpc(failure));
        }
        Ok(value)
    }

    pub async fn provider_auth_start(
        &self,
        provider: &str,
        auth_method: &str,
    ) -> Result<ProviderAuthStart, ProviderRpcError> {
        let value = self
            .provider_rpc(
                "provider/auth/start",
                json!({"provider": provider, "auth_method": auth_method}),
                Duration::from_secs(10),
            )
            .await?;
        serde_json::from_value(value)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider auth start".into()))
    }

    pub async fn provider_auth_poll(
        &self,
        operation_id: &str,
    ) -> Result<ProviderAuthPoll, ProviderRpcError> {
        let value = self
            .provider_rpc(
                "provider/auth/poll",
                json!({"operation_id": operation_id}),
                Duration::from_secs(10),
            )
            .await?;
        parse_auth_poll(value)
    }

    pub async fn provider_auth_cancel(&self, operation_id: &str) -> Result<(), ProviderRpcError> {
        self.provider_rpc(
            "provider/auth/cancel",
            json!({"operation_id": operation_id}),
            Duration::from_secs(10),
        )
        .await?;
        Ok(())
    }

    pub async fn provider_auth_refresh(
        &self,
        request: &ProviderRefreshRequest,
    ) -> Result<CredentialMaterial, ProviderRpcError> {
        let params = serde_json::to_value(request)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider refresh request".into()))?;
        let value = self
            .provider_rpc("provider/auth/refresh", params, Duration::from_secs(30))
            .await?;
        parse_credential_material(value)
    }

    pub async fn provider_auth_revoke(
        &self,
        request: &ProviderRevokeRequest,
    ) -> Result<ProviderRevokeResult, ProviderRpcError> {
        let params = serde_json::to_value(request)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider revoke request".into()))?;
        let value = self
            .provider_rpc("provider/auth/revoke", params, Duration::from_secs(10))
            .await?;
        match value.get("status").and_then(Value::as_str) {
            Some("revoked") => Ok(ProviderRevokeResult::Revoked),
            Some("unsupported") => Ok(ProviderRevokeResult::Unsupported),
            _ => match serde_json::from_value(value) {
                Ok(result) => Ok(result),
                Err(_) => Err(ProviderRpcError::Protocol(
                    "invalid provider revoke result".into(),
                )),
            },
        }
    }

    pub async fn provider_models(
        &self,
        request: &ProviderModelsRequest,
    ) -> Result<ProviderModelCatalog, ProviderRpcError> {
        let params = serde_json::to_value(request)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider models request".into()))?;
        let value = self
            .provider_rpc("provider/models", params, Duration::from_secs(30))
            .await?;
        serde_json::from_value(value)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider model catalog".into()))
    }

    pub async fn provider_usage(
        &self,
        request: &crate::ProviderUsageRequest,
    ) -> Result<crate::ProviderUsageLimits, ProviderRpcError> {
        let params = serde_json::to_value(request)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider usage request".into()))?;
        let value = self
            .provider_rpc("provider/usage", params, Duration::from_secs(60))
            .await?;
        serde_json::from_value(value)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider usage result".into()))
    }

    pub async fn provider_chat(
        &self,
        request: &ProviderChatRequest,
    ) -> Result<ProviderChatResult, ProviderRpcError> {
        let params = serde_json::to_value(request)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider chat request".into()))?;
        let value = self
            .provider_rpc("provider/chat", params, Duration::from_secs(30))
            .await?;
        serde_json::from_value(value)
            .map_err(|_| ProviderRpcError::Protocol("invalid provider chat result".into()))
    }

    pub async fn set_host_handler(&self, handler: HostHandler) {
        *self.transport.host_handler.lock().await = Some(handler);
    }

    pub fn set_notify_handler(&self, handler: NotifyHandler) {
        *self
            .transport
            .notify
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(handler);
    }

    fn claims(&self, hook: &str) -> bool {
        self.manifest.hooks.iter().any(|h| h == hook)
    }
}

fn parse_auth_poll(value: Value) -> Result<ProviderAuthPoll, ProviderRpcError> {
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderRpcError::Protocol("invalid provider auth state".into()))?;
    match state {
        "pending" => Ok(ProviderAuthPoll::Pending {
            retry_after_ms: value.get("retry_after_ms").and_then(Value::as_u64),
        }),
        "completed" => {
            let credential = value
                .get("credential")
                .cloned()
                .ok_or_else(|| ProviderRpcError::Protocol("invalid provider auth state".into()))?;
            Ok(ProviderAuthPoll::Completed(parse_credential_material(
                credential,
            )?))
        }
        "failed" => {
            let failure = value
                .get("error")
                .cloned()
                .ok_or_else(|| ProviderRpcError::Protocol("invalid provider auth state".into()))?;
            serde_json::from_value(failure)
                .map(ProviderAuthPoll::Failed)
                .map_err(|_| ProviderRpcError::Protocol("invalid provider auth state".into()))
        }
        "cancelled" => Ok(ProviderAuthPoll::Cancelled),
        "operation_lost" => Ok(ProviderAuthPoll::OperationLost),
        _ => Err(ProviderRpcError::Protocol(
            "invalid provider auth state".into(),
        )),
    }
}

fn parse_credential_material(value: Value) -> Result<CredentialMaterial, ProviderRpcError> {
    let value = value.get("credential").cloned().unwrap_or(value);
    serde_json::from_value(value)
        .map_err(|_| ProviderRpcError::Protocol("invalid provider credential".into()))
}

#[async_trait]
impl Plugin for SidecarPlugin {
    fn capabilities(&self) -> Vec<String> {
        SidecarPlugin::capabilities(self)
    }

    fn manifest(&self) -> Manifest {
        self.manifest.clone()
    }
    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_session_id(&self, id: &str) {
        *self.session_id.lock().expect("session_id") = id.to_string();
    }
    fn is_global(&self) -> bool {
        self.global.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn set_global(&self, global: bool) {
        self.global
            .store(global, std::sync::atomic::Ordering::Relaxed);
    }
    async fn prompt_context(&self, cwd: &str) -> Option<String> {
        if !self.claims("prompt/context") {
            return None;
        }
        let params = json!({"cwd": cwd, "session": session_json(&self.pinned_sid(), cwd)});
        let v = self
            .transport
            .request("prompt/context", Some(params), Duration::from_secs(30))
            .await
            .ok()?;
        v.get("text")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
    }
    async fn tool_before(&self, name: &str, args: &Value) -> ToolBefore {
        if !self.claims("tool/before") {
            return ToolBefore::Allow;
        }
        let params = json!({"name": name, "args": args, "session": session_json(&self.pinned_sid(), &self.cwd)});
        let ttl = if self.asks { ASK_TTL } else { HOST_TTL };
        let mut res = self
            .transport
            .request("tool/before", Some(params.clone()), ttl)
            .await;

        if matches!(&res, Err(e) if e.to_string().contains("closed stdout")) {
            res = self
                .transport
                .request("tool/before", Some(params), ttl)
                .await;
        }
        match res {
            Ok(v) => ToolBefore::from_result(&v),
            Err(e) => {
                log::warn!(target: "gray_plugin", "sidecar {} tool/before failed: {e}", self.manifest.name);
                ToolBefore::Deny(format!(
                    "plugin '{}' policy check failed ({:.160}); tool not executed — retry may succeed",
                    self.manifest.name,
                    e.to_string()
                ))
            }
        }
    }
    async fn input_submit(&self, text: &str) -> InputSubmit {
        if !self.claims("input/submit") {
            return InputSubmit::Pass;
        }
        let params = json!({"text": text, "session": session_json(&self.pinned_sid(), &self.cwd)});
        let ttl = if self.asks { ASK_TTL } else { HOST_TTL };
        match self
            .transport
            .request("input/submit", Some(params), ttl)
            .await
        {
            Ok(v) => InputSubmit::from_result(&v),

            Err(e) => {
                log::warn!(target: "gray_plugin", "sidecar input/submit failed: {e}");
                InputSubmit::Pass
            }
        }
    }
    async fn agent_before_start(&self) -> Option<String> {
        if !self.claims("agent/before_start") {
            return None;
        }
        let params = json!({"session": session_json(&self.pinned_sid(), &self.cwd)});
        let ttl = if self.asks { ASK_TTL } else { HOST_TTL };
        let v = self
            .transport
            .request("agent/before_start", Some(params), ttl)
            .await
            .ok()?;
        v.get("text")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(|t| t.to_string())
    }
    async fn context_build(&self, messages: &[Message]) -> Option<Vec<Message>> {
        if !self.claims("context/build") {
            return None;
        }

        let (mut wire, media) = scrub_media_for_frame(messages);
        let session = session_json(&self.pinned_sid(), &self.cwd);
        let text = scrub_text_for_frame(&mut wire, &session);
        let params = json!({"messages": wire, "session": session});
        let ttl = if self.asks { ASK_TTL } else { HOST_TTL };
        match self
            .transport
            .request("context/build", Some(params), ttl)
            .await
        {
            Ok(v) => match v.get("messages") {
                Some(m) => match serde_json::from_value::<Vec<Message>>(m.clone()) {
                    Ok(msgs) => Some(restore_media(restore_text(msgs, &text), &media)),
                    Err(e) => {
                        log::warn!(target: "gray_plugin", "sidecar context/build reply unparseable: {e}");
                        None
                    }
                },

                None => None,
            },
            Err(e) => {
                log::warn!(target: "gray_plugin", "sidecar context/build failed: {e}");
                None
            }
        }
    }

    async fn tool_after(&self, name: &str, output: &ToolOutput) -> Option<ToolOutput> {
        if !self.claims("tool/after") {
            return None;
        }
        let params = json!({"name": name, "content": output.content, "is_error": output.is_error, "session": session_json(&self.pinned_sid(), &self.cwd)});
        let ttl = if self.asks { ASK_TTL } else { HOST_TTL };
        let v = self
            .transport
            .request("tool/after", Some(params), ttl)
            .await
            .ok()?;
        let content = v.get("content").and_then(|c| c.as_str());
        let is_error = v.get("is_error").and_then(|e| e.as_bool());
        if content.is_none() && is_error.is_none() {
            return None;
        }
        let mut out = output.clone();
        if let Some(c) = content {
            out.content = c.to_string();
        }
        if let Some(e) = is_error {
            out.is_error = e;
        }
        Some(out)
    }
    async fn run_command(&self, name: &str, argv: Vec<String>) -> Option<CommandOutcome> {
        if !self.manifest.commands.iter().any(|c| c == name)
            && !self.manifest.subcommands.iter().any(|c| c == name)
        {
            return None;
        }
        let params = json!({"name": name, "argv": argv, "session": session_json(&self.pinned_sid(), &self.cwd)});
        let v = self
            .transport
            .request("command/run", Some(params), Duration::from_secs(30))
            .await
            .ok()?;
        command_outcome(&v)
    }
    async fn on_event(&self, e: CoreEvent) {
        let session = session_json(&self.pinned_sid(), &self.cwd);
        let params = match &e {
            CoreEvent::PreTool { name, args } => {
                json!({"type": "pre_tool", "name": name, "args": args, "session": session})
            }
            CoreEvent::PostTool { name, output } => {
                json!({"type": "post_tool", "name": name, "content": output.content, "is_error": output.is_error, "session": session})
            }
            CoreEvent::TurnEnd { usage } => {
                json!({"type": "turn_end", "usage": usage, "session": session})
            }
        };
        let name = self.manifest.name.clone();

        if !self
            .transport
            .send_notification("event/notify", params)
            .await
        {
            log::warn!(target: "gray_plugin", "sidecar {name} hook failed, skipping");
        }
    }
    async fn shutdown(&self) {
        self.shutdown(Duration::from_secs(2)).await;
    }
}

fn scrub_media_for_frame(
    messages: &[Message],
) -> (
    Vec<Message>,
    Vec<Vec<(usize, gray_core::message::ContentBlock)>>,
) {
    let mut pulled_all = Vec::with_capacity(messages.len());
    let wire = messages
        .iter()
        .map(|m| {
            let mut m = m.clone();
            let mut pulled = Vec::new();
            for (i, b) in m.content.iter_mut().enumerate() {
                if matches!(
                    b,
                    gray_core::message::ContentBlock::Image { .. }
                        | gray_core::message::ContentBlock::Media { .. }
                ) {
                    pulled.push((i, b.clone()));
                    *b = gray_core::message::ContentBlock::Text {
                        text: "[media omitted from context/build payload]".to_string(),
                    };
                }
            }
            pulled_all.push(pulled);
            m
        })
        .collect();
    (wire, pulled_all)
}

fn restore_media(
    mut msgs: Vec<Message>,
    media: &[Vec<(usize, gray_core::message::ContentBlock)>],
) -> Vec<Message> {
    if msgs.len() != media.len() {
        return msgs;
    }
    for (m, pulled) in msgs.iter_mut().zip(media.iter()) {
        for (idx, block) in pulled {
            let pos = (*idx).min(m.content.len());
            if matches!(
                m.content.get(pos),
                Some(gray_core::message::ContentBlock::Text { .. })
            ) {
                m.content[pos] = block.clone();
            } else {
                m.content.insert(pos, block.clone());
            }
        }
    }
    msgs
}

const MIN_STUB_BYTES: usize = 256;

fn serialized_len<T: serde::Serialize + ?Sized>(v: &T) -> usize {
    serde_json::to_string(v).map(|s| s.len()).unwrap_or(0)
}

fn largest_scrubbable(wire: &[Message]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize, usize)> = None;
    for (mi, m) in wire.iter().enumerate() {
        for (bi, b) in m.content.iter().enumerate() {
            let size = match b {
                gray_core::message::ContentBlock::Text { text } => text.len(),
                gray_core::message::ContentBlock::ToolResult { content, .. } => content.len(),
                gray_core::message::ContentBlock::ToolUse { args, .. } => serialized_len(args),
                _ => 0,
            };
            if size >= MIN_STUB_BYTES && best.is_none_or(|(.., n)| size > n) {
                best = Some((mi, bi, size));
            }
        }
    }
    best.map(|(mi, bi, _)| (mi, bi))
}

fn scrub_text_for_frame(
    wire: &mut Vec<Message>,
    session: &Value,
) -> Vec<Vec<(usize, gray_core::message::ContentBlock, String)>> {
    let mut pulled: Vec<Vec<(usize, gray_core::message::ContentBlock, String)>> =
        wire.iter().map(|_| Vec::new()).collect();
    let mut frame_len = serialized_len(&json!({"messages": &*wire, "session": session}));

    while frame_len + 128 > MAX_FRAME {
        let Some((mi, bi)) = largest_scrubbable(wire) else {
            break;
        };
        let original = wire[mi].content[bi].clone();
        let stub = format!(
            "[{} bytes elided — context/build frame cap]",
            serialized_len(&original)
        );
        let stub_block = gray_core::message::ContentBlock::Text { text: stub.clone() };
        let saved = serialized_len(&original).saturating_sub(serialized_len(&stub_block));
        wire[mi].content[bi] = stub_block;
        pulled[mi].push((bi, original, stub));
        frame_len = frame_len.saturating_sub(saved);
    }
    pulled
}

fn restore_text(
    mut msgs: Vec<Message>,
    pulled: &[Vec<(usize, gray_core::message::ContentBlock, String)>],
) -> Vec<Message> {
    if msgs.len() != pulled.len() {
        return msgs;
    }
    for (m, stubs) in msgs.iter_mut().zip(pulled.iter()) {
        for (idx, original, stub) in stubs {
            if let Some(gray_core::message::ContentBlock::Text { text }) = m.content.get(*idx)
                && text == stub
            {
                m.content[*idx] = original.clone();
            }
        }
    }
    msgs
}

struct SidecarTool {
    def: ToolDef,
    transport: Arc<Transport>,

    asks: bool,
}

impl SidecarTool {
    fn new(def: ToolDef, transport: Arc<Transport>, asks: bool) -> Self {
        Self {
            def,
            transport,
            asks,
        }
    }

    fn transport_asks(&self) -> bool {
        self.asks
    }
}

#[async_trait]
impl Tool for SidecarTool {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn def(&self) -> ToolDef {
        self.def.clone()
    }
    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolOutput {
        let name = self.def.name.clone();
        let cwd = ctx.cwd.to_string_lossy();
        let sid = ctx.session_id.as_deref().unwrap_or("");
        log::debug!(target: "gray_plugin", "plugin tool call: {name} (plugin-provided, session {sid})");
        let params = json!({"name": name, "args": args, "session": session_json(sid, &cwd)});
        let ttl = if self.transport_asks() {
            ASK_TTL
        } else {
            HOST_TTL
        };
        match self.transport.request("tool/call", Some(params), ttl).await {
            Ok(v) => {
                let is_error = v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
                let content = v.get("content").and_then(|c| c.as_str());
                match (is_error, content) {
                    (true, c) => gray_core::tool_out::fail(c.unwrap_or_default().to_string()),
                    (false, Some(c)) => {
                        let mut out = gray_core::tool_out::finish(c.to_string());
                        attach_media(&name, &mut out, &v);
                        out
                    }

                    (false, None) => ToolOutput::error(format!(
                        "plugin protocol error: {name} reply missing content"
                    )),
                }
            }
            Err(e) => {
                log::warn!(target: "gray_plugin", "sidecar {name} tool call failed, skipping: {e}");
                let msg = e.to_string();
                let kind = if msg.contains("timed out") || msg.contains("elapsed") {
                    "timeout"
                } else if msg.contains("closed stdout") || msg.contains("respawn failed") {
                    "crashed"
                } else {
                    "protocol error"
                };
                ToolOutput::error(format!("plugin {kind}: {name}"))
            }
        }
    }
}

fn attach_media(name: &str, out: &mut ToolOutput, reply: &Value) {
    let entries = |key: &str| {
        reply
            .get(key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    };
    let field = |e: &Value, k: &str| e.get(k).and_then(|s| s.as_str()).map(str::to_string);
    for e in entries("images") {
        match (field(&e, "mime"), field(&e, "data_base64")) {
            (Some(media_type), Some(data)) => out
                .images
                .push(gray_core::agent::AttachedImage { media_type, data }),
            _ => log::warn!(target: "gray_plugin", "{name}: dropped malformed image entry"),
        }
    }
    for e in entries("media") {
        match (field(&e, "mime"), field(&e, "data_base64")) {
            (Some(media_type), Some(data)) => {
                let fallback = field(&e, "fallback")
                    .filter(|f| !f.is_empty())
                    .map(|f| vec![gray_core::message::ContentBlock::Text { text: f }])
                    .unwrap_or_default();
                out.media.push(gray_core::agent::AttachedMedia {
                    media_type,
                    data,
                    fallback,
                })
            }
            _ => log::warn!(target: "gray_plugin", "{name}: dropped malformed media entry"),
        }
    }
}

pub(crate) fn command_outcome(v: &Value) -> Option<CommandOutcome> {
    let field = |k: &str| v.get(k).and_then(|t| t.as_str()).filter(|t| !t.is_empty());
    if let Some(plugin) = field("subagent_settings") {
        return Some(CommandOutcome::SubagentSettings(plugin.to_string()));
    }
    if let Some(plugin) = field("agent_picker") {
        return Some(CommandOutcome::AgentPicker(plugin.to_string()));
    }
    if let Some(row) = field("model_picker") {
        return Some(CommandOutcome::ModelPicker(row.to_string()));
    }
    if let Some(p) = field("prompt") {
        return Some(CommandOutcome::Prompt(p.to_string()));
    }
    field("text").map(|t| CommandOutcome::Say(t.to_string()))
}

#[path = "sidecar_tests.rs"]
#[cfg(test)]
mod tests;
