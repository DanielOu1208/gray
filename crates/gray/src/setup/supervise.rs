//! Starting an app's daemon under whatever init this box actually has, or
//! under gray itself when it has none. A failed start is reported with the
//! init's own words and never fails the setup flow.

use std::path::{Path, PathBuf};

use crate::gateway::service::{Supervisor, detect};

/// The runit `run` script for a daemon: exec the argv, nothing else.
pub fn runit_script(argv: &[String]) -> String {
    let mut script = String::from("#!/bin/sh\nexec");
    for arg in argv {
        script.push(' ');
        script.push_str(&sh_quote(arg));
    }
    script.push('\n');
    script
}

/// Single-quote for `sh`, keeping embedded quotes intact.
fn sh_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Where the "none" supervisor keeps the daemon's pid.
pub fn pidfile_path(config_dir: &Path, app: &str) -> PathBuf {
    config_dir.join(format!("{app}.pid"))
}

/// Start the daemon and return a one-line human report.
pub fn start_daemon(app: &str, argv: &[String], config_dir: &Path) -> anyhow::Result<String> {
    match detect() {
        Supervisor::Runit { dir } => start_runit(app, argv, &dir),
        Supervisor::SystemdUser { unit_dir } => start_systemd(app, argv, &unit_dir),
        Supervisor::None => start_under_gray(app, argv, config_dir),
    }
}

/// Stop a gray-supervised daemon via its pidfile. Other inits own their
/// own stop (`sv down <app>`, `systemctl --user stop <app>`).
pub fn stop_daemon(app: &str, config_dir: &Path) -> anyhow::Result<String> {
    let pidfile = pidfile_path(config_dir, app);
    let text = std::fs::read_to_string(&pidfile).map_err(|_| {
        anyhow::anyhow!(
            "no pid file at {} (not started by gray?)",
            pidfile.display()
        )
    })?;
    let pid: u32 = text
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("unreadable pid file at {}", pidfile.display()))?;
    #[cfg(unix)]
    {
        // SAFETY: kill with a parsed pid has no memory-safety surface.
        let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        if rc != 0 {
            anyhow::bail!("could not signal pid {pid}");
        }
    }
    let _ = std::fs::remove_file(&pidfile);
    Ok(format!("stopped {app} (pid {pid})"))
}

fn start_runit(app: &str, argv: &[String], dir: &Path) -> anyhow::Result<String> {
    let svc = dir.join(app);
    std::fs::create_dir_all(&svc)?;
    let run = svc.join("run");
    std::fs::write(&run, runit_script(argv))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755))?;
    }
    let out = std::process::Command::new("sv")
        .args(["-w", "10", "up"])
        .arg(&svc)
        .output()
        .map_err(|e| anyhow::anyhow!("could not run sv: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "sv up failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(format!("{app} runs under runit at {}", svc.display()))
}

fn start_systemd(app: &str, argv: &[String], unit_dir: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(unit_dir)?;
    let unit = unit_dir.join(format!("{app}.service"));
    let exec: Vec<String> = argv.iter().map(|a| sh_quote(a)).collect();
    std::fs::write(
        &unit,
        format!(
            "[Unit]\nDescription={app} (gray plugin)\nAfter=network.target\n\n[Service]\nExecStart={}\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
            exec.join(" ")
        ),
    )?;
    for args in [vec!["daemon-reload"], vec!["enable", "--now", app]] {
        let out = std::process::Command::new("systemctl")
            .arg("--user")
            .args(&args)
            .output()
            .map_err(|e| anyhow::anyhow!("could not run systemctl: {e}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "systemctl {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(format!(
        "{app} runs under systemd --user ({})",
        unit.display()
    ))
}

/// No init to lean on: gray spawns the daemon detached, logs next to the
/// config, and remembers the pid so `stop_daemon` can end it.
fn start_under_gray(app: &str, argv: &[String], config_dir: &Path) -> anyhow::Result<String> {
    let log = config_dir.join(format!("{app}.log"));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut command = std::process::Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .stdin(std::process::Stdio::null())
        .stdout(log_file)
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid in pre_exec touches no memory Python-style rules
        // would flag; it only creates a session for the child.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not start {}: {e}", argv[0]))?;
    std::fs::write(pidfile_path(config_dir, app), child.id().to_string())?;
    Ok(format!(
        "{app} runs under gray (pid {}, log {})",
        child.id(),
        log.display()
    ))
}

#[path = "supervise_tests.rs"]
#[cfg(test)]
mod tests;
