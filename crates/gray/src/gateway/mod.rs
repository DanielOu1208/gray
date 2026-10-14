pub mod activity;
pub mod brain;
pub mod cli;
pub mod deliver;
pub mod event;
pub mod lifecycle;
pub mod outbox;
pub mod pid;
pub mod service;
pub mod sessions;
pub mod settings;
pub mod socket;
pub mod state;
pub mod turn;

mod run;

pub fn state_dir(home: &Path) -> std::path::PathBuf {
    home.join("gateway")
}

pub fn paused(dir: &Path) -> bool {
    dir.join("PAUSED").exists()
}

use std::path::Path;

fn ensure_gateway_on() -> anyhow::Result<()> {
    ensure_gateway_on_with(crate::setup::gw_auto_enabled())
}

fn ensure_gateway_on_with(enabled: bool) -> anyhow::Result<()> {
    if enabled {
        return Ok(());
    }
    anyhow::bail!("gateway is off — `gray gateway on` to enable")
}

async fn setup_platform(platform: Option<String>) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let home = crate::setup::gray_home()?;
    let name = match platform {
        Some(p) => p,
        None => {
            let apps = crate::plugin_cli::setup_apps(&home);
            match apps.len() {
                0 => "discord".to_string(),
                1 => apps[0].clone(),
                _ => {
                    use std::io::Write;
                    for (i, a) in apps.iter().enumerate() {
                        println!("  {}. {a}", i + 1);
                    }
                    print!("Platform [1]: ");
                    std::io::stdout().flush()?;
                    let mut line = String::new();
                    std::io::stdin().read_line(&mut line)?;
                    let pick = line.trim();
                    let i = if pick.is_empty() {
                        1
                    } else {
                        pick.parse::<usize>()?
                    };
                    apps.get(i.wrapping_sub(1))
                        .context("no such platform")?
                        .clone()
                }
            }
        }
    };
    if !crate::plugin_cli::setup_apps(&home).contains(&name) {
        for line in crate::plugin_cli::install_spec_lines(&home, &name, false).await? {
            println!("{line}");
        }
    }
    crate::plugin_cli::forward(&home, &name, &["setup".to_string()])
}

pub async fn run_cli(cmd: crate::GatewayCmd, config: &crate::config::Config) -> anyhow::Result<()> {
    use crate::GatewayCmd;

    if let GatewayCmd::Lifecycle(cmd) = cmd {
        return lifecycle_cli(cmd);
    }
    if let GatewayCmd::Agent(cmd) = cmd {
        return cli::run(cmd).await;
    }
    if let GatewayCmd::Setup { platform } = cmd {
        return setup_platform(platform).await;
    }

    anyhow::ensure!(
        !cfg!(windows),
        "gateway is not supported on native Windows; use WSL for background scheduling"
    );
    match cmd {
        GatewayCmd::Run => {
            ensure_gateway_on()?;
            run::run_foreground(config).await
        }
        GatewayCmd::Status => {
            let home = crate::setup::gray_home()?;
            let (running, lines) = report(&home);
            for line in lines {
                println!("{line}");
            }
            if !running {
                std::process::exit(1);
            }
            Ok(())
        }
        GatewayCmd::Start => {
            ensure_gateway_on()?;
            println!("{}", service::start()?);
            Ok(())
        }
        GatewayCmd::On => {
            println!(
                "{}",
                crate::repl::handlers::toggle_subsystem(
                    crate::repl::handlers::Subsystem::Gateway,
                    true
                )
            );
            Ok(())
        }
        GatewayCmd::Off => {
            println!(
                "{}",
                crate::repl::handlers::toggle_subsystem(
                    crate::repl::handlers::Subsystem::Gateway,
                    false
                )
            );
            Ok(())
        }
        GatewayCmd::Stop => {
            println!("{}", service::stop()?);
            Ok(())
        }
        GatewayCmd::Restart => {
            println!("{}", service::restart()?);
            Ok(())
        }
        GatewayCmd::Install { no_start, print } => {
            let home = crate::setup::gray_home()?;
            let exe = crate::exe_watch::on_disk_exe()?;
            println!("{}", service::install(&home, &exe, no_start, print)?);
            Ok(())
        }
        GatewayCmd::Uninstall => {
            println!("{}", service::uninstall()?);
            Ok(())
        }
        GatewayCmd::Lifecycle(_) | GatewayCmd::Setup { .. } | GatewayCmd::Agent(_) => {
            unreachable!("handled above")
        }
    }
}

fn lifecycle_cli(cmd: crate::LifecycleCmd) -> anyhow::Result<()> {
    use crate::LifecycleCmd;
    let answer = match cmd {
        LifecycleCmd::Boot { dir, interrupted } => lifecycle::boot_json(&dir, interrupted),
        LifecycleCmd::Stop { dir } => lifecycle::stop_json(&dir),
        LifecycleCmd::Restart { dir } => {
            lifecycle::request_restart(&dir)?;
            serde_json::json!({"restart": true})
        }
        LifecycleCmd::ClearRestart { dir } => {
            lifecycle::clear_restart(&dir);
            serde_json::json!({"restart": false})
        }
    };
    println!("{answer}");
    Ok(())
}

fn report(home: &Path) -> (bool, Vec<String>) {
    let now = crate::cron::now_secs();
    let sup = service::detect();
    let mut lines = Vec::new();
    let mut running = false;

    if let Some(result) = socket::query(home, "status") {
        running = true;
        let pid = result.get("pid").and_then(|v| v.as_u64()).unwrap_or(0);
        let uptime = result
            .get("uptime_secs")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        lines.push(format!(
            "gateway: running (pid {pid}, up {})",
            crate::cron_status::ago_short(uptime)
        ));
        lines.push(format!(
            "  socket: {} — answered status (protocol {})",
            socket::sock_path(home).display(),
            socket::PROTOCOL
        ));
        if let Some(kind) = result.get("supervisor").and_then(|v| v.as_str()) {
            lines.push(format!("  supervisor: {kind}"));
        }
    } else if let Some(rec) = pid::running(home) {
        running = true;
        lines.push(format!(
            "gateway: running (pid {}, up {}) — pid file only, no socket answer",
            rec.pid,
            crate::cron_status::ago_short(now.saturating_sub(rec.started_at))
        ));
    } else {
        lines.push("gateway: not running".to_string());
        let sock = socket::sock_path(home);
        if sock.exists() {
            lines.push(format!("  socket: stale file at {}", sock.display()));
        }
        if let Some(st) = state::read(home) {
            lines.push(format!(
                "  last state: {} ({}) — {} ago",
                st.gateway_state,
                st.exit_reason.as_deref().unwrap_or("no reason recorded"),
                crate::cron_status::ago_short(now.saturating_sub(st.updated_at))
            ));
        }
    }

    match crate::cron::CronStore::open(home.join("cron")) {
        Ok(store) => {
            let health = store.health(now).unwrap_or_default();
            let jobs = store.list().map(|v| v.len()).unwrap_or(0);
            lines.push(format!("  jobs: {jobs}"));
            for l in crate::cron_status::ticker_line(&health, now).lines() {
                lines.push(format!("  cron: {l}"));
            }
        }
        Err(e) => lines.push(format!("  cron: store unavailable: {e:#}")),
    }
    lines.push(format!("  service: {}", service::describe(&sup)));
    (running, lines)
}

#[cfg(test)]
mod toggle_tests {
    use super::ensure_gateway_on_with;

    #[test]
    fn gateway_switch_off_refuses_run_and_start() {
        assert!(ensure_gateway_on_with(true).is_ok());
        let err = ensure_gateway_on_with(false).unwrap_err().to_string();
        assert!(err.contains("gateway is off"), "{err}");
        assert!(err.contains("gray gateway on"), "{err}");
    }
}
