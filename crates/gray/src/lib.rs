pub mod ask;
pub mod auth;
pub mod auto_title;
pub mod auto_update;
pub mod cache;
pub mod cmd;
pub mod compact;
pub mod composer;
pub mod config;
pub mod cron;
pub mod cron_fire;
pub mod cron_serve;
pub mod cron_status;
pub(crate) mod cron_tool;
pub(crate) mod exe_watch;
pub(crate) mod failed_turn;
pub mod feedback;
pub(crate) mod fusion;
pub mod gateway;
pub mod host;
pub mod keymap;
pub mod logging;
pub(crate) mod mascot;
pub mod plugin_check;
pub mod plugin_cli;
pub(crate) mod plugin_status;
pub mod print;
mod print_meter;
pub mod profile;
pub mod project_trust;
pub mod prompt_templates;
pub mod providers;
pub mod qol;
pub mod repl;
pub mod resume;
mod rotation;
pub mod search;
pub mod session_read;
pub mod session_send;
pub mod session_store;
pub mod setup;
pub mod shell_drain;
pub mod skills;
pub mod skills_tool;
pub mod spill;
pub mod statusline;
pub mod sys_editor;
pub mod system_prompt;
pub mod term_keys;
pub(crate) mod text_width;
pub mod theme;
pub mod tool_fmt;
pub mod tui;
pub mod turn_caps;
pub mod update;

use clap::{ArgGroup, Parser};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use config::Config;
pub use print::run_print_mode;
pub use profile::{build_registry, take_profile_warnings};
pub use repl::{ReplCommand, parse_command, run_repl_mode};
pub use tui::{clear_screen, print_wrapped};

pub const DEFAULT_SYS_PROMPT: &str = r#"<!--
Unreadable note: this HTML comment stays in the file but is stripped before
the prompt reaches the model — only the text after this note reaches it.

This file IS the stored system prompt, sent verbatim every turn with no
runtime context appended. Only what the model can't already know: the
tools describe themselves and the task says the rest.
Per turn gray may append ephemeral context: the <available_skills> list
(fresh skill discovery for the turn's directory) — no skill tool, read
matches with bash — plus <project_context>, the nearest AGENTS.md /
CLAUDE.md above the working directory, and plugin docs. Lean mode — the
default — skips all of that: this file alone is the system prompt. Opt
back in with "lean": false or GRAY_LEAN=0. Edit with `/agentsmd`
(Ctrl-S save & apply, Ctrl-R reset to this default, Ctrl-X cancel).
-->
You are Gray, running on the user's machine.
"#;

pub const BARE_SYS_PROMPT: &str = "You are a helpful assistant that can interact with a computer.";

pub fn sys_prompt_path() -> anyhow::Result<PathBuf> {
    Ok(crate::setup::gray_home()?.join("AGENTS.md"))
}

pub fn load_or_create_system_prompt_at(path: &Path) -> anyhow::Result<String> {
    if let Ok(body) = std::fs::read_to_string(path) {
        return Ok(body);
    }

    if path.file_name().is_some_and(|n| n == "AGENTS.md")
        && let Some(parent) = path.parent()
        && let Ok(body) = std::fs::read_to_string(parent.join("sys.md"))
        && !body.trim().is_empty()
        && std::fs::write(path, &body).is_ok()
    {
        return Ok(body);
    }
    match std::fs::read_to_string(path) {
        Ok(body) => Ok(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT_SYS_PROMPT)?;
            Ok(DEFAULT_SYS_PROMPT.to_string())
        }
        Err(e) => Err(e.into()),
    }
}

pub fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .ok()
        .filter(|&w| w >= 20)
        .unwrap_or(80)
}

pub fn rule(label: &str) -> String {
    let prefix = format!("\u{2500}\u{2500} {label} ");
    let used = prefix.chars().count();
    let fill = term_width().saturating_sub(used);
    format!("{prefix}{}", "\u{2500}".repeat(fill))
}

pub use gray_plugin::builder::{
    PROMPT_CACHE_KEY_MAX_LENGTH, clamp_prompt_cache_key, provider_cache_key,
};

fn cache_warm_policy(
    config: &Config,
    model: &str,
    plugin_warm_replay: bool,
    cache_ttl_secs: u64,
) -> Option<gray_core::cache_warm::CacheWarmPolicy> {
    let cacheable = !config.uses_plugin_credentials() || plugin_warm_replay;
    if !cacheable {
        log::info!(target: "gray_agent",
            "cache warm disabled: provider is plugin-credentialed without request.warm_replay");
    }
    if config.bare || !cacheable || std::env::var_os("GRAY_NO_CACHE_WARM").is_some() {
        return None;
    }
    let model = model.to_string();
    Some(gray_core::cache_warm::CacheWarmPolicy {
        ttl: std::time::Duration::from_secs(cache_ttl_secs),
        prices: std::sync::Arc::new(move || {
            let r = crate::setup::get_model_rate(&model)?;
            r.has_cache_prices.then_some(gray_core::cache_warm::Prices {
                input: r.input,
                output: r.output,
                cache_read: r.cache_read,
                cache_write: r.cache_write,
            })
        }),
    })
}

pub async fn build_agent(
    config: &Config,
    cwd: &Path,
    session_id: Option<&str>,
) -> anyhow::Result<gray_core::agent::Agent> {
    build_agent_with(config, cwd, session_id, true).await
}

pub async fn build_model_agent(
    config: &Config,
    cwd: &Path,
    session_id: Option<&str>,
) -> anyhow::Result<gray_core::agent::Agent> {
    build_agent_with(config, cwd, session_id, false).await
}

async fn build_agent_with(
    config: &Config,
    cwd: &Path,
    session_id: Option<&str>,
    spawn_plugins: bool,
) -> anyhow::Result<gray_core::agent::Agent> {
    let Some(model) = &config.model else {
        anyhow::bail!(
            "no model configured yet — run /models to set up your provider & key (or /models provider/id; /help for all commands)"
        );
    };

    let api_key = config.api_key.as_deref().unwrap_or("");

    let body = if config.bare {
        BARE_SYS_PROMPT.to_string()
    } else {
        load_or_create_system_prompt_at(&sys_prompt_path()?)?
    };

    let build_started = std::time::Instant::now();
    let dynamic = if config.uses_plugin_credentials() {
        let home = setup::gray_home()?;
        Some(crate::providers::connect_dynamic_provider(config, &home).await?)
    } else {
        None
    };
    log::debug!(target: "gray_timing", "build_agent provider_connect elapsed_ms={}", build_started.elapsed().as_millis());

    let wire_model = crate::setup::wire_model_for(config).unwrap_or_else(|| model.clone());

    let effort_model = if crate::setup::variants::row_shape(model).is_some() {
        model
    } else {
        &wire_model
    };
    let reasoning_effort = config
        .thinking_effort
        .as_deref()
        .map(|effort| crate::setup::clamp_thinking_level(effort_model, effort).to_string());
    let plugin_warm_replay = dynamic
        .as_ref()
        .is_some_and(|p| p.installed().provider.transport.request.warm_replay);

    let cache_ttl_secs = dynamic
        .as_ref()
        .and_then(|p| p.installed().provider.transport.request.cache_ttl_secs)
        .unwrap_or(300);
    crate::cache::set_cache_ttl(std::time::Duration::from_secs(cache_ttl_secs));
    let cache_warm = cache_warm_policy(config, &wire_model, plugin_warm_replay, cache_ttl_secs);

    let identity = if config.bare {
        String::new()
    } else {
        let provider = dynamic
            .as_ref()
            .map(|p| p.installed().provider.name.clone())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| config.provider_id.clone());
        system_prompt::identity_block(
            crate::setup::selected_model_label(config)
                .as_deref()
                .unwrap_or(""),
            model,
            &provider,
        )
    };
    let prebooted_plugins = if !config.bare && spawn_plugins {
        match crate::plugin_status::take_boot().await {
            Some(Ok(plugins)) => Some(plugins),
            Some(Err(e)) => return Err(anyhow::anyhow!(e)),
            None => None,
        }
    } else {
        None
    };
    let config_bare = config.bare;
    let agent = gray_plugin::builder::build_agent(gray_plugin::builder::BuilderOptions {
        model: wire_model.clone(),
        api_key: api_key.to_string(),
        base_url: config.base_url.clone(),
        reasoning_effort,
        temperature: config.temperature,
        top_p: config.top_p,

        context_window: (!config.bare)
            .then(|| crate::setup::context::resolve_model_context_length(model)),
        session_id: session_id.map(str::to_string),
        cwd: cwd.to_path_buf(),

        system_prompt: gray_plugin::builder::SystemPrompt::Build(Box::new(
            move |_registry: &gray_tools::Registry| {
                let mut prompt = system_prompt::build_system_prompt(Some(body));
                if !identity.is_empty() {
                    if !prompt.is_empty() {
                        prompt.push_str("\n\n");
                    }
                    prompt.push_str(&identity);
                }

                if !config_bare {
                    prompt.push_str("\n\n");
                    prompt.push_str(&system_prompt::harness_facts(Some(
                        std::time::Duration::from_secs(cache_ttl_secs),
                    )));
                }

                if let Some(framing) = crate::fusion::lead_framing() {
                    prompt.push_str("\n\n");
                    prompt.push_str(framing);
                }
                prompt
            },
        )),

        extra_plugins: extra_surface_plugins(config, spawn_plugins),

        bare: config.bare || !spawn_plugins,
        host_handler: Some(host::default_handler(cwd.to_path_buf())),
        profile_path: "gray.yml".to_string(),
        abort_on_spawn_failure: true,
        wrap_executor: None,
        dynamic_provider_profile: dynamic.as_ref().map(|provider| provider.profile().clone()),
        dynamic_credential_source: dynamic
            .as_ref()
            .map(|provider| provider.credential_source()),
        prebooted_plugins,
    })
    .await?;
    log::debug!(target: "gray_timing", "build_agent plugins+agent elapsed_ms={}", build_started.elapsed().as_millis());
    for w in gray_plugin::builder::take_builder_warnings() {
        profile::queue_profile_warning(w);
    }
    if let Some(t) = crate::host::registered_tui()
        && let Ok(mut t) = t.lock()
    {
        t.set_plugin_stats(crate::plugin_status::stats_line());
        if !t.modal_open {
            let _ = t.draw();
        }
    }

    let window = crate::setup::context::resolve_model_context_length(model);
    let (ceiling, price_cliff) = crate::compact::ceiling_for(
        config.compact_at,
        window,
        crate::setup::context::user_reserve_tokens_for(window),
        crate::setup::model_price_cliff(model).map(|c| c as usize),
    );
    let mut agent = agent
        .with_compaction_budget(config.context_reserve, config.context_keep)
        .with_compact_ceiling(ceiling, price_cliff);

    agent.set_prefix_rewrite_ok(!config.uses_plugin_credentials() || plugin_warm_replay);

    agent.set_lean_prompt(config.lean);

    Ok(agent
        .with_tool_timeout(crate::shell_drain::SHELL_TOOL_TIMEOUT)
        .with_cache_warm(cache_warm))
}

pub(crate) fn extra_surface_plugins(
    config: &Config,
    spawn_plugins: bool,
) -> Vec<Arc<dyn gray_plugin::Plugin>> {
    let mut extra: Vec<Arc<dyn gray_plugin::Plugin>> = Vec::new();
    if !(config.bare || config.lean || !spawn_plugins) {
        extra.push(Arc::new(crate::skills_tool::SkillsPlugin::default()));
        extra.push(Arc::new(crate::skills_tool::ProjectContextPlugin));
    }
    if !config.bare && spawn_plugins {
        extra.push(Arc::new(crate::fusion::FusionPlugin::default()));
        extra.push(Arc::new(crate::cron_tool::CronPlugin));
    }
    extra
}

#[derive(Parser, Debug, Clone)]
#[command(
    name = "gray",
    version,
    about = "gray — a minimal, modular agent harness in Rust.",
    after_help = "Run with no arguments for the interactive REPL. Use -p for one-shot print mode.",
    group(ArgGroup::new("one_shot_input").args(["print", "input_json"]))
)]
pub struct Cli {
    #[arg(long)]
    pub model: Option<String>,

    #[arg(long)]
    pub base_url: Option<String>,

    #[arg(short = 'p', long = "print")]
    pub print: Option<String>,

    #[arg(long, value_name = "PATH", conflicts_with = "print", requires = "json")]
    pub input_json: Option<PathBuf>,

    #[arg(long, requires = "one_shot_input")]
    pub json: bool,

    #[arg(long, requires = "json", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_requests: Option<u32>,

    #[arg(long, requires = "json")]
    pub input_price: Option<f64>,

    #[arg(long, requires = "json")]
    pub output_price: Option<f64>,

    #[arg(long)]
    pub api_key: Option<String>,

    #[arg(short = 'c', long = "continue")]
    pub continue_last: bool,

    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    #[arg(
        short = 'r',
        long = "resume",
        value_name = "ID",
        num_args = 0..=1,
        conflicts_with = "session",
        conflicts_with = "continue_last"
    )]
    pub resume: Option<Option<String>>,

    #[arg(long, value_name = "TOKENS", value_parser = parse_context_window_cli)]
    pub context_window: Option<usize>,

    #[arg(long, value_name = "TOKENS", value_parser = parse_context_window_cli)]
    pub context_reserve: Option<usize>,

    #[arg(long, value_name = "TOKENS", value_parser = parse_context_window_cli)]
    pub context_keep: Option<usize>,

    #[arg(long, value_name = "auto|TOKENS", value_parser = parse_compact_at_cli)]
    pub compact_at: Option<crate::setup::CompactAt>,

    #[arg(long = "dump-manifest")]
    pub dump_manifest: bool,

    #[arg(long = "skill")]
    pub skill: bool,

    #[arg(long)]
    pub bare: bool,

    #[arg(long)]
    pub lean: bool,

    #[arg(long, value_name = "N")]
    pub max_turns: Option<u32>,

    #[arg(long, value_name = "USD")]
    pub max_cost_usd: Option<f64>,

    #[arg(long, value_name = "SECS")]
    pub max_wall_secs: Option<u64>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

fn parse_context_window_cli(s: &str) -> Result<usize, String> {
    crate::setup::parse_context_window(s)
        .ok_or_else(|| format!("invalid context window '{s}' — use e.g. 128000, 128k, 1m"))
}

fn parse_compact_at_cli(s: &str) -> Result<crate::setup::CompactAt, String> {
    crate::setup::parse_compact_at(s).ok_or_else(|| {
        format!("invalid --compact-at '{s}' — use \"auto\" or e.g. 90000, 90k, 1.5m")
    })
}

#[derive(Parser, Debug, Clone)]
pub enum Commands {
    Find {
        #[arg(value_name = "PATTERN")]
        pattern: String,

        #[arg(value_name = "PATH")]
        path: Option<String>,

        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    Grep {
        #[arg(value_name = "PATTERN")]
        pattern: String,

        #[arg(value_name = "PATH")]
        path: Option<String>,

        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        #[arg(long, value_name = "GLOB")]
        glob: Option<String>,

        #[arg(long, short)]
        ignore_case: bool,

        #[arg(long, short = 'F')]
        literal: bool,

        #[arg(long, value_name = "N")]
        context: Option<usize>,
    },

    Resume {
        #[arg(value_name = "SESSION_ID")]
        session_id: Option<String>,

        #[arg(long)]
        last: bool,

        #[arg(long)]
        all: bool,
    },

    #[command(
        about = "Read a session's recent conversation, or list live sessions",
        after_help = "Examples:\n  gray read --live            live sessions: id, pid, cwd, model, last activity\n  gray read <SESSION_ID>      last 20 messages, tool calls summarized\n  gray read <SESSION_ID> --last 50 --json\nReads the owner's session store ($GRAY_USER_HOME, else $GRAY_HOME). A session is live only while its .open lock is held."
    )]
    Read {
        #[arg(value_name = "SESSION_ID")]
        session: Option<String>,

        #[arg(long)]
        live: bool,

        #[arg(long, value_name = "N", default_value_t = 20)]
        last: usize,

        #[arg(long)]
        json: bool,
    },

    Send {
        #[arg(value_name = "SESSION_ID")]
        session: String,

        #[arg(value_name = "TEXT", default_value = "")]
        text: String,

        #[arg(long = "attach", value_name = "PATH")]
        attach: Vec<std::path::PathBuf>,

        #[arg(long, value_name = "LABEL")]
        from: Option<String>,

        #[arg(long, value_name = "SECS")]
        wait: Option<u64>,
    },

    #[command(visible_alias = "plugins")]
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },

    Gateway {
        #[command(subcommand)]
        cmd: GatewayCmd,
    },

    Trust {
        dir: Option<std::path::PathBuf>,

        #[arg(long)]
        revoke: bool,
    },

    #[command(visible_alias = "upgrade")]
    Update {
        #[arg(long, value_name = "on|off|status")]
        auto: Option<String>,

        #[arg(long)]
        everything: bool,
    },

    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },

    Sessions {
        #[command(subcommand)]
        cmd: SessionsCmd,
    },

    Cmd {
        #[command(subcommand)]
        cmd: CmdVerb,
    },

    Spill {
        #[command(subcommand)]
        cmd: SpillCmd,
    },

    #[command(hide = true)]
    SearchServe {
        #[arg(long, value_name = "DIR")]
        root: std::path::PathBuf,

        #[arg(long, value_name = "PATH")]
        sock: std::path::PathBuf,
    },

    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Parser, Debug, Clone)]
pub enum SpillCmd {
    Head {
        #[arg(value_name = "HANDLE")]
        handle: String,

        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },

    Tail {
        #[arg(value_name = "HANDLE")]
        handle: String,

        #[arg(long, short = 'n', default_value_t = 50)]
        lines: usize,
    },

    Grep {
        #[arg(value_name = "HANDLE")]
        handle: String,

        #[arg(value_name = "PATTERN")]
        pattern: String,

        #[arg(long, short = 'n', default_value_t = 0)]
        context: usize,

        #[arg(long, short)]
        ignore_case: bool,

        #[arg(long, short = 'F')]
        literal: bool,

        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    Stats,
}

#[derive(Parser, Debug, Clone)]
pub enum SessionsCmd {
    Prune {
        #[arg(long, default_value_t = 90)]
        older_than_days: u64,
    },
}

#[derive(Parser, Debug, Clone)]
pub enum CronCmd {
    List,

    #[command(
        after_help = "Reminder (one command, no model turn):\n  gray cron add \"in 2m\" \"clean my roo\" --reminder\nThe text is stored and delivered verbatim: never reword it, never fix typos, never pass --name."
    )]
    Add {
        schedule: String,

        prompt: String,

        #[arg(long)]
        deliver: Option<String>,

        #[arg(long = "origin-session")]
        origin_session: Option<String>,

        #[arg(long)]
        name: Option<String>,

        #[arg(long = "in", value_name = "DIR")]
        workdir: Option<PathBuf>,

        #[arg(long)]
        skills: Option<String>,

        #[arg(long)]
        script: Option<PathBuf>,

        #[arg(long)]
        reminder: bool,
    },

    Tick {
        #[arg(long)]
        json: bool,
    },

    Serve,

    Pause {
        id: String,
    },

    Resume {
        id: String,
    },

    Run {
        id: String,
    },

    Show {
        id: String,
    },

    Remove {
        id: String,
    },
}

#[derive(Parser, Debug, Clone)]
pub enum GatewayCmd {
    Setup {
        platform: Option<String>,
    },

    Run,

    Status,

    Start,

    Stop,

    Restart,

    Install {
        #[arg(long)]
        no_start: bool,

        #[arg(long)]
        print: bool,
    },

    Uninstall,

    On,

    Off,

    #[command(subcommand)]
    Lifecycle(LifecycleCmd),

    #[command(flatten)]
    Agent(crate::gateway::cli::AgentCmd),
}

#[derive(Parser, Debug, Clone)]
pub enum LifecycleCmd {
    Boot {
        #[arg(long)]
        dir: std::path::PathBuf,

        #[arg(long, default_value_t = 0)]
        interrupted: usize,
    },

    Stop {
        #[arg(long)]
        dir: std::path::PathBuf,
    },

    Restart {
        #[arg(long)]
        dir: std::path::PathBuf,
    },

    ClearRestart {
        #[arg(long)]
        dir: std::path::PathBuf,
    },
}

#[derive(Parser, Debug, Clone)]
pub enum CmdVerb {
    List,

    Run {
        name: String,

        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    Interact {
        token: String,

        action: Option<String>,
    },
}

#[derive(Parser, Debug, Clone)]
pub enum PluginCmd {
    List,

    Install {
        spec: String,

        #[arg(short, long)]
        force: bool,

        #[arg(short = 'y', long)]
        yes: bool,
    },

    Remove {
        name: String,
    },

    Update {
        #[arg(default_value = "all")]
        target: String,
        #[arg(long)]
        check: bool,
    },

    Enable {
        name: String,

        #[arg(long)]
        global: bool,
    },

    Disable {
        name: String,
    },

    Check {
        dir: String,
    },

    Capabilities {
        name: Option<String>,

        #[arg(long)]
        all: bool,
    },
}

#[path = "lib_tests.rs"]
#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "input_json_tests.rs"]
mod input_json_tests;
