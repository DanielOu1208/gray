//! The grep/rg shim, client side. `gray` invoked as `grep` or `rg` (a
//! symlink on the session's PATH) asks the session's index which files the
//! real tool would need to read. If it answers, the real tool runs on exactly
//! those files with the flags that make its output the same; if it declines,
//! or the socket is missing, slow, or confused, the real tool runs on the
//! original argv. The shim never formats output — the real tool does.
//!
//! Wire format: one JSON line each way over `$GRAY_SEARCH_SOCK`. See
//! `search_serve` for the server half.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const SOCK_ENV: &str = "GRAY_SEARCH_SOCK";
pub const INDEX_ENV: &str = "GRAY_SEARCH_INDEX";
pub const TRACE_ENV: &str = "GRAY_SHIM_TRACE";
/// The whole round trip, connect included, before the shim gives up and
/// runs the real tool on the original argv. Sized well under what the real
/// recursive walk costs on a big tree; a wedged server costs this once per
/// call instead of a scan.
pub const BUDGET: Duration = Duration::from_secs(4);
/// Argument bytes per real-tool run. Past this the file list is split and
/// the runs' exit codes merged.
const BATCH_BYTES: usize = 96 * 1024;

/// The server's answer. `served: true` carries the argv for the real tool
/// (its flags and pattern, with the walk flags removed and any output flags
/// it needs added) and the files to pass after `--`.
#[derive(Default)]
pub struct Reply {
    pub served: bool,
    pub reason: String,
    pub argv: Vec<String>,
    pub files: Vec<String>,
    pub candidates: usize,
    pub server_ms: f64,
    /// "index" when the index pruned the file list, "pass" for a verbatim
    /// pass-through (identical output but no index work), "answer" when the
    /// server computed the output itself, "" when declined.
    pub mode: String,
    /// `"answer"` replies carry the real tool's output and exit code,
    /// base64: no process runs at all.
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl Reply {
    /// Parse one reply line. Anything malformed is a decline.
    pub fn parse(line: &str) -> Option<Reply> {
        let v: serde_json::Value = serde_json::from_str(line).ok()?;
        let strings = |k: &str| -> Option<Vec<String>> {
            match v.get(k) {
                None => Some(Vec::new()),
                Some(a) => a
                    .as_array()?
                    .iter()
                    .map(|x| x.as_str().map(str::to_string))
                    .collect(),
            }
        };
        Some(Reply {
            served: v.get("served")?.as_bool()?,
            reason: v
                .get("reason")
                .and_then(|r| r.as_str())
                .unwrap_or_default()
                .to_string(),
            argv: strings("argv")?,
            files: strings("files")?,
            candidates: v.get("candidates").and_then(|c| c.as_u64()).unwrap_or(0) as usize,
            server_ms: v.get("server_ms").and_then(|c| c.as_f64()).unwrap_or(0.0),
            mode: v.get("mode").and_then(|m| m.as_str()).unwrap_or_default().to_string(),
            stdout: v.get("stdout").and_then(|m| m.as_str()).unwrap_or_default().to_string(),
            stderr: v.get("stderr").and_then(|m| m.as_str()).unwrap_or_default().to_string(),
            code: v.get("code").and_then(|c| c.as_i64()).unwrap_or(1) as i32,
        })
    }
}

/// `Some("grep" | "rg")` when this process was started under one of those
/// names. Checked before anything else in `main`.
pub fn invoked_as() -> Option<String> {
    let arg0 = std::env::args_os().next()?;
    let name = Path::new(&arg0).file_name()?.to_str()?;
    matches!(name, "grep" | "rg").then(|| name.to_string())
}

/// Shim entry: answer through the index or run the real tool. Never returns.
pub fn run(tool: &str) -> ! {
    let original: Vec<OsString> = std::env::args_os().collect();
    let start = Instant::now();
    let reply = ask(tool, &original[1..]);
    let (served, reason, candidates, server_ms, mode) = match &reply {
        Some(r) => (r.served, r.reason.as_str(), r.candidates, r.server_ms, r.mode.as_str()),
        None => (false, "no index", 0, 0.0, ""),
    };
    trace(
        tool,
        &original[1..],
        served,
        reason,
        candidates,
        server_ms,
        mode,
        start,
    );
    match reply {
        Some(r) if r.served && r.mode == "answer" => print_answer(r),
        Some(r) if r.served => run_batches(tool, &original[0], r),
        _ => exec_real(tool, &original),
    }
}

/// The server computed the real tool's output itself: write it and exit
/// with its code. Any undecodable payload falls back to the real tool.
fn print_answer(r: Reply) -> ! {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(out), Ok(err)) = (b64.decode(r.stdout), b64.decode(r.stderr)) else {
        exec_real("grep", &std::env::args_os().collect::<Vec<_>>());
    };
    use std::io::Write as _;
    let _ = std::io::stdout().write_all(&out);
    let _ = std::io::stderr().write_all(&err);
    let _ = std::io::stdout().flush();
    std::process::exit(r.code);
}

/// Ask the session index. `None` when there is no index to ask or it did
/// not answer within [`BUDGET`]. Non-UTF-8 argv is declined.
fn ask(tool: &str, args: &[OsString]) -> Option<Reply> {
    if std::env::var_os(INDEX_ENV).is_some_and(|v| v == "off") {
        return None;
    }
    let sock = std::env::var_os(SOCK_ENV)?;
    let argv: Vec<String> = args
        .iter()
        .map(|a| a.to_str().map(str::to_string))
        .collect::<Option<_>>()?;
    let cwd = std::env::current_dir().ok()?;
    let cwd = cwd.to_str()?;
    let deadline = Instant::now() + BUDGET;
    let mut stream = UnixStream::connect(OsStr::from_bytes(sock.as_bytes())).ok()?;
    let left = deadline.checked_duration_since(Instant::now())?;
    stream.set_read_timeout(Some(left)).ok()?;
    stream.set_write_timeout(Some(left)).ok()?;
    let mut line = serde_json::json!({ "tool": tool, "argv": argv, "cwd": cwd }).to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).ok()?;
    Reply::parse(&reply)
}

/// Run the real tool on the served file list. One batch replaces this
/// process; more run in turn and their exit codes merge (2 if any is 2, else
/// 0 if any is 0, else 1).
fn run_batches(tool: &str, argv0: &OsString, reply: Reply) -> ! {
    let mut files: Vec<&str> = reply.files.iter().map(String::as_str).collect();
    // No candidate: the real tool still reads something, so it sees an empty
    // file. That keeps exit status and error messages (a bad pattern) intact.
    if files.is_empty() {
        files.push("/dev/null");
    }
    let base: usize = reply.argv.iter().map(|a| a.len() + 1).sum::<usize>() + 3;
    let mut batches: Vec<Vec<&str>> = vec![Vec::new()];
    let mut size = base;
    for f in files {
        if size + f.len() + 1 > BATCH_BYTES && !batches.last().is_some_and(Vec::is_empty) {
            batches.push(Vec::new());
            size = base;
        }
        size += f.len() + 1;
        batches.last_mut().expect("nonempty").push(f);
    }
    let real = match real_tool(tool) {
        Some(p) => p,
        None => fail(&format!("{tool}: not found")),
    };
    let build = |chunk: &[&str]| {
        let mut c = Command::new(&real);
        c.arg0(argv0).args(&reply.argv).arg("--").args(chunk);
        c
    };
    if let [only] = batches.as_slice() {
        let err = build(only).exec();
        fail(&format!("{tool}: {err}"));
    }
    let mut code = 1;
    for chunk in &batches {
        let status = build(chunk).stdin(Stdio::inherit()).status();
        match status.ok().and_then(|s| s.code()) {
            Some(2) => code = 2,
            Some(0) if code != 2 => code = 0,
            Some(_) => {}
            None => code = 2,
        }
    }
    std::process::exit(code)
}

/// Run the real tool on the original argv. Never returns.
fn exec_real(tool: &str, original: &[OsString]) -> ! {
    let Some(real) = real_tool(tool) else {
        fail(&format!("{tool}: not found"));
    };
    let err = Command::new(&real)
        .arg0(&original[0])
        .args(&original[1..])
        .exec();
    fail(&format!("{tool}: {err}"));
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(127)
}

/// The first `tool` on PATH that is not this binary (the shim's own dirs
/// hold symlinks to it).
pub fn real_tool(tool: &str) -> Option<PathBuf> {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let cand = dir.join(tool);
        if !cand.is_file() {
            continue;
        }
        if let (Some(me), Ok(real)) = (&me, cand.canonicalize())
            && &real == me
        {
            continue;
        }
        return Some(cand);
    }
    None
}

/// `GRAY_SHIM_TRACE=<file>`: one JSON line per shim call, for the benchmark.
fn trace(
    tool: &str,
    argv: &[OsString],
    served: bool,
    reason: &str,
    candidates: usize,
    server_ms: f64,
    mode: &str,
    start: Instant,
) {
    let Some(path) = std::env::var_os(TRACE_ENV) else {
        return;
    };
    let argv: Vec<String> = argv
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let line = serde_json::json!({
        "tool": tool,
        "argv": argv,
        "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "served": served,
        "reason": reason,
        "candidates": candidates,
        "server_ms": server_ms,
        "mode": mode,
        "total_ms": start.elapsed().as_secs_f64() * 1000.0,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        // One write syscall: concurrent shims must not interleave fragments.
        let _ = f.write_all(format!("{line}\n").as_bytes());
    }
}
