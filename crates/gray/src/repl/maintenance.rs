//! `/update` and `/restart` — the two commands that make a running gray
//! become a *current* gray.
//!
//! Both exist because a self-update is a two-step thing nobody completes:
//! `gray update` installs a new binary and tells you to restart, and the
//! thing that needs restarting is not the shell you typed it in. It is the
//! REPL you are sitting in, and the gateway daemon, which keeps running the
//! build it started with until someone restarts it. So `/update` installs and
//! then says "run /restart", and `/restart` does both halves.
//!
//! Neither codex nor hermes has these: codex shows an update *popup* at
//! startup and hands you a `brew upgrade` line, and hermes-rs's `restart`
//! tears down LSP clients. A CLI that installs its own updates owes the user
//! a way to actually land on them.
//!
//! The re-exec is the delicate part. `/restart` restores the terminal before
//! spawning, exactly like the account commands do, and re-enters the
//! conversation with `resume --last` when it was an interactive session — a
//! restart that silently dropped your conversation would be a worse bug than
//! not shipping it.

use super::TuiOpt;
use crate::gateway::service;

/// How long a version check may take before we call the network a day. Same
/// budget as the startup check: this is a convenience, not a dependency.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// `/update` — check the channel, and install when there is something newer.
///
/// Installs only after the user says so: the installer is a mutable HTTPS
/// script, so `/update` asks the way `gray update` does and reports exactly
/// what happened, including the PATH-shadowing warning that bites right after
/// a self-update.
pub(crate) async fn handle_update(tui: &TuiOpt, arg: Option<String>) {
    let shared = tui.as_ref().map(|(s, _)| s);
    if arg.as_deref().map(str::trim) == Some("everything") {
        crate::qol::update::handle(tui).await;
        return;
    }
    if let Some(rest) = arg.as_deref().map(str::trim)
        && let Some(sub) = rest.strip_prefix("auto")
    {
        let word = sub.trim();
        match word {
            "on" | "off" => {
                let on = word == "on";
                let text = match crate::auto_update::set_enabled(on) {
                    Ok(()) if on => "auto-update on".to_string(),
                    Ok(()) => "auto-update off — `gray update` still installs manually".to_string(),
                    Err(e) => format!("auto-update toggle failed: {e:#}"),
                };
                super::say(shared, &text);
            }
            "status" | "" => super::say(shared, &crate::auto_update::status_text()),
            other => super::say(
                shared,
                &format!("unknown `/update auto {other}` (on|off|status)"),
            ),
        }
        return;
    }
    let current = env!("CARGO_PKG_VERSION");
    super::say(
        shared,
        &format!("checking for a new gray ({})...", crate::update::CHANNEL),
    );

    let check = tokio::time::timeout(CHECK_TIMEOUT, crate::update::latest_version()).await;
    let latest = match check {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            super::say(shared, &format!("update check failed: {e}"));
            return;
        }
        Err(_) => {
            super::say(
                shared,
                "update check timed out — try `gray update` in a shell",
            );
            return;
        }
    };

    if !crate::update::update_available(
        crate::update::CHANNEL,
        &latest,
        current,
        env!("GRAY_BUILD_ID"),
    ) {
        super::say(shared, &format!("gray {current} is current"));
        return;
    }

    super::say(
        shared,
        &format!("gray {latest} available (you have {current}). Install it? [y/N]"),
    );
    if !confirm_yn().await {
        super::say(shared, "not updated");
        return;
    }

    // The installer is a child process writing to the real terminal, so raw
    // mode has to go for the duration and the composer re-anchors after.
    let result = with_terminal(shared, crate::update::install);
    match result {
        Ok(()) => {
            crate::update::warn_on_shadow();
            super::say(
                shared,
                &format!("updated to {latest} — `/restart` to run it (the gateway too)"),
            );
        }
        Err(e) => super::say(shared, &format!("update failed: {e}")),
    }
}

/// `/restart` — put the running gray on the binary that is on disk.
///
/// Both halves, in the order that makes the message true: the gateway first
/// (it is a separate process on the old build), then this REPL re-exec'd on
/// the new one. The conversation survives via `resume --last`.
pub(crate) async fn handle_restart(tui: &TuiOpt) {
    restart_with(tui, true).await;
}

pub(crate) async fn restart_with(tui: &TuiOpt, resume: bool) {
    let shared = tui.as_ref().map(|(s, _)| s);

    match restart_gateway().await {
        GatewayRestart::Restarted(line) => super::say(shared, &line),
        GatewayRestart::NotRunning => {
            super::say(shared, "gateway is not running — nothing to restart there")
        }
        GatewayRestart::Skipped => super::say(shared, "gateway: skipped (scratch GRAY_HOME)"),
        GatewayRestart::Failed(e) => super::say(shared, &format!("gateway restart failed: {e}")),
    }

    // A turn may still be flushing its transcript; a restart that truncates
    // the last exchange is the one thing this command must not do.
    super::drain_in_flight_turn(std::time::Duration::from_millis(1500)).await;

    let exe = match crate::exe_watch::on_disk_exe() {
        Ok(p) => p,
        Err(e) => {
            super::say(
                shared,
                &format!("cannot find the running binary ({e}) — restart gray yourself"),
            );
            return;
        }
    };
    let original: Vec<String> = std::env::args().skip(1).collect();
    let args = if !resume {
        strip_resume_args(&original)
    } else {
        restart_argv(original, crate::host::live_session().as_deref())
    };
    let resumed = args.first().map(|a| a == "resume").unwrap_or(false);

    // Hand the terminal back before the child takes it: raw mode off, cursor
    // visible, the composer's viewport re-anchored — otherwise the child
    // inherits a terminal this process still owns.
    let _ = crossterm::terminal::disable_raw_mode();
    super::restore_viewport(shared);
    let line = if resumed {
        "\r\nrestarting gray (new build, same conversation)…\r\n"
    } else {
        "\r\nrestarting gray (new build)…\r\n"
    };
    print!("{line}");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    #[cfg(unix)]
    let spawned: std::io::Result<()> = {
        use std::os::unix::process::CommandExt as _;
        Err(std::process::Command::new(&exe).args(&args).exec())
    };
    #[cfg(not(unix))]
    let spawned: std::io::Result<()> = match std::process::Command::new(&exe).args(&args).spawn() {
        Ok(_) => std::process::exit(0),
        Err(e) => Err(e),
    };
    if let Err(e) = spawned {
        // The spawn failed, so this process is still the one running and
        // still usable: say so rather than exiting into a dead shell.
        let _ = crossterm::terminal::enable_raw_mode();
        super::say(
            shared,
            &format!("restart failed to spawn ({e}) — still on the old build"),
        );
    }
}

enum GatewayRestart {
    Restarted(String),
    NotRunning,
    /// `$GRAY_HOME` is a scratch home: the system gateway is not ours to touch.
    Skipped,
    Failed(String),
}

/// True when `$GRAY_HOME` is set to a home other than `$HOME/.gray`.
fn scratch_gray_home() -> bool {
    let Some(set) =
        std::env::var_os("GRAY_HOME").filter(|v| !v.to_string_lossy().trim().is_empty())
    else {
        return false;
    };
    let default = std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".gray"));
    is_scratch_home(std::path::Path::new(&set), default.as_deref())
}

/// `set` is scratch unless it is the default home. With no `HOME` to compare
/// against, stay on the safe side and call it scratch.
fn is_scratch_home(set: &std::path::Path, default: Option<&std::path::Path>) -> bool {
    default != Some(set)
}

/// Restart the gateway, and only ever leave it in a better state than we
/// found it.
///
/// Three cases, in the order that decides who owns the process:
///
/// 1. **Installed under a supervisor** — the supervisor restarts it. That is
///    the right owner and the only one that knows the service contract.
/// 2. **Running with nobody supervising it** — a `gray gateway run` someone
///    launched by hand, which is the common shape on a box without a service
///    installed. Nothing else will bring it back, so we stop it *and* start
///    it again on the new build. Stopping without the relaunch would take a
///    working gateway down and call it a restart.
/// 3. **Not running** — say so and move on; `/restart` is about picking up a
///    new build, not about resurrecting a daemon the user did not start.
async fn restart_gateway() -> GatewayRestart {
    // A scratch home (tests, a second checkout) must never reach the system
    // gateway: `service::detect` finds the installed unit whatever home is set.
    if scratch_gray_home() {
        return GatewayRestart::Skipped;
    }
    let supervised = matches!(
        service::detect(),
        service::Supervisor::Runit { .. } | service::Supervisor::SystemdUser { .. }
    );
    if supervised {
        match service::restart() {
            Ok(line) => return GatewayRestart::Restarted(line),
            // Installed and failing is a real failure worth printing. Not
            // installed is not: fall through and handle the process directly.
            Err(e) if !e.to_string().contains("no service installed") => {
                return GatewayRestart::Failed(e.to_string());
            }
            Err(_) => {}
        }
    }

    let Ok(home) = crate::setup::gray_home() else {
        return GatewayRestart::NotRunning;
    };
    let Some(rec) = crate::gateway::pid::running(&home) else {
        return GatewayRestart::NotRunning;
    };
    // `stop` waits up to 20s for the pid to go and refuses to return while it
    // is still up, so the relaunch below cannot race a dying daemon.
    if let Err(e) = service::stop() {
        return GatewayRestart::Failed(format!("could not stop the running gateway: {e}"));
    }
    match relaunch(&home, rec.pid) {
        Ok(line) => GatewayRestart::Restarted(line),
        Err(e) => GatewayRestart::Failed(format!(
            "stopped the gateway (pid {}) but could not start it again: {e} — `gray gateway run` when you are back",
            rec.pid
        )),
    }
}

/// Start the gateway again, detached, on the binary that is on disk.
///
/// Its own process group, so a closed terminal or a SIGHUP aimed at this
/// REPL's group does not take the daemon with it — the same shape as the
/// `gateway run` the user had launched by hand.
fn relaunch(home: &std::path::Path, old_pid: u32) -> anyhow::Result<String> {
    use std::process::Stdio;
    let exe = crate::exe_watch::on_disk_exe()?;
    let log = home.join("logs").join("gateway.log");
    std::fs::create_dir_all(
        log.parent()
            .ok_or_else(|| anyhow::anyhow!("{} has no parent", log.display()))?,
    )?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["gateway", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(file.try_clone()?))
        .stderr(Stdio::from(file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    cmd.spawn()?;

    // Report what is true, not what we hope: a spawn is not a running daemon.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if let Some(now) = crate::gateway::pid::running(home) {
            return Ok(format!(
                "gateway restarted on the new build (pid {} was {old_pid}, log: {})",
                now.pid,
                log.display()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(format!(
        "gateway stopped (pid {old_pid}) and relaunched; it has not reported in yet — log: {}",
        log.display()
    ))
}

/// What the child process gets. An interactive session (a bare `gray`, no
/// subcommand) resumes the conversation it was having; anything else — a
/// `-p` prompt, `resume --last`, a subcommand — is passed through verbatim,
/// because re-running a one-shot prompt or a subcommand is the caller's
/// business, not ours.
/// Rebuild argv for the post-restart process. An argv that already names a
/// session (`resume`, `-r`, `-c`, `--session`) keeps it. Anything else lands
/// back on THIS session — plain `--last` resumes whatever was touched most
/// recently, and a sibling window's newer session wins that race and holds
/// a live lock, so the restart would refuse itself into a fresh session.
pub(crate) fn restart_argv(original: Vec<String>, current: Option<&str>) -> Vec<String> {
    let Some(sid) = current else {
        return if original.is_empty() {
            vec!["resume".into(), "--last".into()]
        } else {
            original
        };
    };
    if has_session_selector(&original) {
        return original;
    }
    vec!["resume".into(), sid.to_string()]
}

/// Does the argv already name a session to reopen?
fn has_session_selector(args: &[String]) -> bool {
    args.iter().any(|a| {
        matches!(
            a.as_str(),
            "resume" | "-c" | "--continue" | "-r" | "--resume" | "--session"
        ) || a.starts_with("--session=")
            || a.starts_with("--resume=")
            || (a.starts_with("-r") && a.len() > 2)
    })
}

fn strip_resume_args(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter().peekable();
    if it.peek().map(|a| a.as_str()) == Some("resume") {
        it.next();
        while let Some(a) = it.peek() {
            match a.as_str() {
                "--last" | "--all" => {
                    it.next();
                }
                s if !s.starts_with('-') => {
                    it.next();
                    break;
                }
                _ => break,
            }
        }
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "-c" | "--continue" | "--session" | "-r" | "--resume" => {
                if matches!(a.as_str(), "--session" | "-r" | "--resume")
                    && let Some(v) = it.peek()
                    && !v.starts_with('-')
                {
                    it.next();
                }
            }
            s if s.starts_with("--session=")
                || s.starts_with("--resume=")
                || s.starts_with("-r") && s.len() > 2 => {}
            _ => out.push(a.clone()),
        }
    }
    out
}

/// One keypress, y/N, read the way the TUI reads keys.
///
/// Not `update::confirm()`: that helper switches raw mode off when it is
/// done, which is right for a shell and wrong here — the composer owns raw
/// mode and would be left reading a cooked terminal. Dispatch runs between
/// input events, so nobody else is reading stdin at this moment.
async fn confirm_yn() -> bool {
    use crossterm::event::{self, Event, KeyCode, KeyEvent};
    // A dead tty spins crossterm's read on EIO inside block_in_place.
    if crate::composer::input::stdin_gone() {
        return false;
    }
    // Callers run under the multi-thread runtime `main` installs, which
    // `block_in_place` requires.
    let got = tokio::task::block_in_place(event::read);
    matches!(
        got,
        Ok(Event::Key(KeyEvent {
            code: KeyCode::Char('y' | 'Y'),
            ..
        }))
    )
}

/// Run a blocking child-process flow with the terminal in cooked mode, then
/// put the composer back. Same contract as the account commands.
pub(crate) fn with_terminal<T>(
    shared: Option<&crate::composer::SharedTui>,
    f: impl FnOnce() -> T,
) -> T {
    let was_raw = crossterm::terminal::is_raw_mode_enabled().unwrap_or(false);
    let _ = crossterm::terminal::disable_raw_mode();
    let out = f();
    if was_raw {
        let _ = crossterm::terminal::enable_raw_mode();
    }
    super::restore_viewport(shared);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_gray_home_never_reaches_the_system_gateway() {
        let default = std::path::Path::new("/home/me/.gray");
        // A GRAY_HOME elsewhere, including a trailing-slash spelling of the default.
        assert!(is_scratch_home(
            std::path::Path::new("/tmp/vp_home"),
            Some(default)
        ));
        assert!(is_scratch_home(std::path::Path::new("/tmp/vp_home"), None));
        assert!(!is_scratch_home(
            std::path::Path::new("/home/me/.gray/"),
            Some(default)
        ));
    }

    #[test]
    fn a_bare_session_resumes_its_own_conversation() {
        // The conversation is the session: restart must land back on THIS
        // one — `--last` can pick a sibling window's newer session and hit
        // its live lock (the "already open in another process" refusal).
        assert_eq!(
            restart_argv(vec![], Some("viscous-lithium-meridian")),
            vec!["resume", "viscous-lithium-meridian"]
        );
        // No live session yet (first launch before any prompt): `--last`
        // is the best pointer available.
        assert_eq!(restart_argv(vec![], None), vec!["resume", "--last"]);
    }

    #[test]
    fn anything_the_user_typed_is_passed_through() {
        // An argv that already names a session is kept verbatim.
        for args in [
            vec!["resume".to_string(), "--last".to_string()],
            vec!["-r".to_string(), "other-session".to_string()],
            vec!["--session=x".to_string()],
            vec!["-c".to_string()],
        ] {
            assert_eq!(
                restart_argv(args.clone(), Some("viscous-lithium-meridian")),
                args
            );
        }
        // No session to return to: selector-less argv still passes through.
        assert_eq!(
            restart_argv(vec!["-p".to_string(), "hello".to_string()], None),
            vec!["-p", "hello"]
        );
    }
}
