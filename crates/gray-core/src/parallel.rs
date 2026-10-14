use std::collections::HashSet;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::task::JoinSet;

use crate::agent::ToolOutput;

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn is_batchable(name: &str) -> bool {
    matches!(
        name,
        "read" | "ls" | "find" | "grep" | "bash" | "write" | "edit"
    )
}

pub fn bash_is_batchable(command: &str) -> bool {
    let scrubbed = command.replace("2>&1", "").replace("2>/dev/null", "");

    if scrubbed.contains('>') {
        return false;
    }

    let toks: Vec<&str> = scrubbed
        .split(|c: char| {
            matches!(
                c,
                ' ' | '\t'
                    | '\n'
                    | '\r'
                    | ';'
                    | '&'
                    | '|'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '`'
                    | '"'
                    | '\''
                    | '$'
                    | ','
                    | '/'
            )
        })
        .filter(|t| !t.is_empty())
        .collect();
    if toks.iter().any(|t| MUTATOR_WORDS.contains(t)) {
        return false;
    }
    if toks.contains(&"sed") && sed_edits_in_place(&toks) {
        return false;
    }

    if toks.contains(&"wget") {
        return false;
    }
    if matches!(toks.first(), Some(s) if *s == "." || *s == "..") {
        return false;
    }
    if inline_code_executes(&toks) || curl_writes_file(&toks) || shell_runs_script(&toks) {
        return false;
    }
    for (i, t) in toks.iter().enumerate() {
        let ok = match *t {
            "git" => first_verb_allowed(&toks, i, GIT_READ_ONLY, &["-c", "--config", "-C"]),
            "cargo" => first_verb_allowed(&toks, i, CARGO_READ_ONLY, &[]),
            "npm" => first_verb_allowed(&toks, i, NPM_READ_ONLY, &[]),
            _ => true,
        };
        if !ok {
            return false;
        }
    }
    true
}

const MUTATOR_WORDS: &[&str] = &[
    "rm", "mv", "cp", "mkdir", "touch", "rmdir", "unlink", "ln", "install", "dd", "shred",
    "truncate", "chmod", "chown", "chgrp", "sudo", "tee", "kill", "pkill", "killall", "shutdown",
    "reboot", "poweroff", "halt", "npx", "scp", "rsync", "sftp", "-delete", "tar", "bsdtar",
    "unzip", "gunzip", "7z", "cpio",
];

fn sed_edits_in_place(toks: &[&str]) -> bool {
    toks.iter().any(|t| {
        if *t == "--in-place" {
            return true;
        }
        match t.strip_prefix('-') {
            Some(flags) if !flags.starts_with('-') => flags.contains('i'),
            _ => false,
        }
    })
}

fn inline_code_executes(toks: &[&str]) -> bool {
    const INTERPS: &[&str] = &["python", "python3", "node", "ruby", "perl"];
    toks.iter().any(|t| INTERPS.contains(t)) && toks.iter().any(|t| *t == "-c" || *t == "-e")
}

fn curl_writes_file(toks: &[&str]) -> bool {
    if !toks.contains(&"curl") {
        return false;
    }
    toks.iter().any(|t| {
        *t == "-o" || *t == "-O" || *t == "--output" || *t == "--remote-name" || {
            t.len() > 2 && t.starts_with('-') && !t.starts_with("--") && t[1..].contains(['o', 'O'])
        }
    })
}

fn shell_runs_script(toks: &[&str]) -> bool {
    const SHELLS: &[&str] = &["sh", "bash", "zsh", "dash"];
    for (i, t) in toks.iter().enumerate() {
        if !SHELLS.contains(t) {
            continue;
        }
        let mut j = i + 1;
        let mut skip_next = false;
        while j < toks.len() {
            let a = toks[j];
            if skip_next {
                skip_next = false;
            } else if a == "-c" || a == "--command" {
                skip_next = true;
            } else if !a.starts_with('-') {
                return true;
            }
            j += 1;
        }
    }
    false
}

const GIT_READ_ONLY: &[&str] = &["status", "diff", "log", "show", "blame"];

const CARGO_READ_ONLY: &[&str] = &["metadata"];

const NPM_READ_ONLY: &[&str] = &[
    "ls", "list", "ll", "la", "view", "info", "show", "v", "ping", "whoami", "help",
];

fn first_verb_allowed(toks: &[&str], at: usize, allowed: &[&str], takes_value: &[&str]) -> bool {
    let mut i = at + 1;
    while i < toks.len() {
        let t = toks[i];
        if takes_value.contains(&t) {
            i += 2;
        } else if t.starts_with('-') || t.starts_with('+') {
            i += 1;
        } else {
            return allowed.contains(&t);
        }
    }
    true
}

pub fn parallel_enabled() -> bool {
    match std::env::var("GRAY_PARALLEL_READS") {
        Ok(v) => !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Parallel(Vec<usize>),
    Single(usize),
}

fn is_write_call(name: &str) -> bool {
    matches!(name, "write" | "edit")
}

fn normalize_write_path(raw: &str) -> String {
    let mut segs: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if segs.last().is_some_and(|s| *s != "..") {
                    segs.pop();
                } else if !raw.starts_with('/') {
                    segs.push("..");
                }
            }
            s => segs.push(s),
        }
    }
    let mut out = segs.join("/");
    if raw.starts_with('/') {
        out.insert(0, '/');
    }
    if out.is_empty() {
        out.push_str(if raw.starts_with('/') { "/" } else { "." });
    }
    out
}

fn write_target_path(name: &str, args: &Value) -> Option<String> {
    if !is_write_call(name) {
        return None;
    }
    match args.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => Some(normalize_write_path(p)),
        _ => None,
    }
}

const READ_VERBS: &[&str] = &[
    "cat",
    "head",
    "tail",
    "wc",
    "stat",
    "file",
    "diff",
    "cmp",
    "md5sum",
    "sha1sum",
    "sha256sum",
    "shasum",
    "xxd",
    "od",
    "strings",
    "basename",
    "dirname",
    "realpath",
    "readlink",
    "sort",
    "uniq",
    "cut",
    "tr",
    "tac",
    "nl",
    "ls",
    "grep",
    "rg",
    "find",
    "sed",
    "jq",
    "du",
    "df",
    "test",
];

const NOPATH_VERBS: &[&str] = &[
    "echo", "printf", "true", "false", "pwd", "date", "whoami", "id", "uname", "hostname", "env",
    "printenv", "sleep", "seq", "yes", "expr", "which", "type", "command", "umask", "cal", "clear",
];

const WRITE_VERBS: &[&str] = &[
    "rm", "rmdir", "mv", "cp", "mkdir", "touch", "ln", "truncate", "chmod", "chown", "chgrp",
    "unlink", "tee",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touch {
    Reads(Vec<String>),
    ReadsUnknown,
    Writes {
        reads: Vec<String>,
        writes: Vec<String>,
    },
}

fn path_operands(toks: &[&str]) -> Vec<String> {
    toks.iter()
        .filter(|t| !t.starts_with('-'))
        .filter_map(|t| {
            let t = t.trim();
            if t.is_empty() {
                None
            } else {
                Some(normalize_write_path(t))
            }
        })
        .collect()
}

fn unenumerable_syntax(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\''
            | '"'
            | '`'
            | '$'
            | '*'
            | '?'
            | '~'
            | '{'
            | '}'
            | '('
            | ')'
            | '|'
            | '&'
            | ';'
            | '!'
            | '#'
            | '<'
            | '='
            | '\\'
    )
}

fn classify_call(name: &str, args: &Value) -> Option<Touch> {
    if is_write_call(name) {
        return write_target_path(name, args).map(|p| Touch::Writes {
            reads: Vec::new(),
            writes: vec![p],
        });
    }
    if name == "bash" {
        return bash_touch(args.get("command").and_then(Value::as_str)?);
    }
    match name {
        "read" | "ls" | "find" | "grep" => match args.get("path").and_then(Value::as_str) {
            Some(p) if !p.is_empty() && !p.chars().any(unenumerable_syntax) => {
                Some(Touch::Reads(vec![normalize_write_path(p)]))
            }
            _ => Some(Touch::ReadsUnknown),
        },
        _ => None,
    }
}

fn bash_touch(command: &str) -> Option<Touch> {
    let scrubbed = command.replace("2>&1", "").replace("2>/dev/null", "");
    if scrubbed.chars().any(unenumerable_syntax) {
        return bash_is_batchable(command).then_some(Touch::ReadsUnknown);
    }
    let mut parts = scrubbed.split('>');
    let cmd_part = parts.next().unwrap_or("").trim();
    let mut writes = Vec::new();
    for part in parts {
        let word = part
            .trim_start_matches('>')
            .split_whitespace()
            .next()
            .unwrap_or("");

        if word.is_empty() || word.starts_with('-') {
            return bash_is_batchable(command).then_some(Touch::ReadsUnknown);
        }
        writes.push(normalize_write_path(word));
    }
    let toks: Vec<&str> = cmd_part
        .split_whitespace()
        .filter(|t| !t.is_empty())
        .collect();
    let Some(verb) = toks.first().copied() else {
        return bash_is_batchable(command).then_some(Touch::ReadsUnknown);
    };

    if bash_is_batchable(command) {
        return Some(match verb {
            v if READ_VERBS.contains(&v) => Touch::Reads(path_operands(&toks[1..])),
            v if NOPATH_VERBS.contains(&v) => Touch::Reads(Vec::new()),
            _ => Touch::ReadsUnknown,
        });
    }

    let mut reads = Vec::new();
    if (verb == "sed" && sed_edits_in_place(&toks)) || WRITE_VERBS.contains(&verb) {
        writes.extend(path_operands(&toks[1..]));
    } else if READ_VERBS.contains(&verb) {
        reads = path_operands(&toks[1..]);
    } else if !NOPATH_VERBS.contains(&verb) {
        return None;
    }
    if writes.is_empty() && reads.is_empty() {
        return None;
    }
    Some(Touch::Writes { reads, writes })
}

fn touches_interfere(a: &Touch, b: &Touch) -> bool {
    fn any_overlap(xs: &[String], ys: &[String]) -> bool {
        xs.iter()
            .any(|x| ys.iter().any(|y| touched_paths_overlap(x, y)))
    }
    match (a, b) {
        (Touch::Reads(_), Touch::Reads(_)) | (Touch::Reads(_), Touch::ReadsUnknown) => false,
        (Touch::ReadsUnknown, Touch::Reads(_)) | (Touch::ReadsUnknown, Touch::ReadsUnknown) => {
            false
        }
        (Touch::ReadsUnknown, Touch::Writes { .. })
        | (Touch::Writes { .. }, Touch::ReadsUnknown) => true,
        (Touch::Reads(rs), Touch::Writes { writes: ws, .. }) => any_overlap(rs, ws),
        (Touch::Writes { writes: ws, .. }, Touch::Reads(rs)) => any_overlap(ws, rs),
        (
            Touch::Writes {
                reads: ra,
                writes: wa,
            },
            Touch::Writes {
                reads: rb,
                writes: wb,
            },
        ) => any_overlap(wa, wb) || any_overlap(ra, wb) || any_overlap(wa, rb),
    }
}

fn touched_paths_overlap(a: &str, b: &str) -> bool {
    let (na, nb) = (normalize_write_path(a), normalize_write_path(b));
    if na == nb {
        return true;
    }
    if na == "." || nb == "." || na == "/" || nb == "/" {
        return true;
    }
    if na.starts_with('/') != nb.starts_with('/') {
        return true;
    }
    let sa: Vec<&str> = na.split('/').filter(|p| !p.is_empty()).collect();
    let sb: Vec<&str> = nb.split('/').filter(|p| !p.is_empty()).collect();
    let n = sa.len().min(sb.len());
    sa[..n] == sb[..n]
}

pub fn plan_segments(
    tool_uses: &[(String, String, Value)],
    known: &HashSet<String>,
) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    let mut run_touches: Vec<Touch> = Vec::new();
    let flush = |run: &mut Vec<usize>, out: &mut Vec<Segment>| {
        if run.len() >= 2 {
            out.push(Segment::Parallel(std::mem::take(run)));
        } else {
            out.extend(run.drain(..).map(Segment::Single));
        }
    };
    for (idx, (_, name, args)) in tool_uses.iter().enumerate() {
        let touch = if is_batchable(name) && known.contains(name) && args.is_object() {
            classify_call(name, args)
        } else {
            None
        };
        match touch {
            Some(t) if run_touches.iter().all(|u| !touches_interfere(u, &t)) => {
                run.push(idx);
                run_touches.push(t);
            }
            _ => {
                flush(&mut run, &mut out);
                run_touches.clear();
                out.push(Segment::Single(idx));
            }
        }
    }
    flush(&mut run, &mut out);
    out
}

pub fn rewrite_conflicts(calls: &[(&str, &Value)]) -> Vec<usize> {
    let mut kept: Vec<Touch> = Vec::new();
    let mut out = Vec::new();
    for (pos, (name, args)) in calls.iter().enumerate() {
        let touch = if is_batchable(name) && args.is_object() {
            classify_call(name, args)
        } else {
            None
        };
        match touch {
            Some(t) if kept.iter().all(|u| !touches_interfere(u, &t)) => kept.push(t),
            _ => out.push(pos),
        }
    }
    out
}

pub const CANCEL_REPORT_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

pub fn parallel_max() -> usize {
    parse_parallel_max(std::env::var("GRAY_PARALLEL_MAX").ok().as_deref())
}

pub(crate) fn parse_parallel_max(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(tokio::sync::Semaphore::MAX_PERMITS)
        .min(tokio::sync::Semaphore::MAX_PERMITS)
}

pub async fn join_ordered(
    futs: Vec<(usize, BoxFuture<'static, ToolOutput>)>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Vec<(usize, Option<ToolOutput>)> {
    use futures::FutureExt as _;
    use std::panic::AssertUnwindSafe;
    let inputs: Vec<usize> = futs.iter().map(|(idx, _)| *idx).collect();
    let mut set: JoinSet<(usize, ToolOutput)> = JoinSet::new();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(parallel_max()));
    for (idx, fut) in futs {
        let sem = sem.clone();
        set.spawn(async move {
            let Ok(_permit) = sem.acquire_owned().await else {
                return (idx, ToolOutput::error("parallel semaphore closed"));
            };

            match AssertUnwindSafe(fut).catch_unwind().await {
                Ok(out) => (idx, out),
                Err(id) => {
                    let msg = id
                        .downcast_ref::<&str>()
                        .copied()
                        .or_else(|| id.downcast_ref::<String>().map(String::as_str))
                        .unwrap_or("unknown panic");
                    (idx, ToolOutput::error(format!("tool task panicked: {msg}")))
                }
            }
        });
    }
    let mut done: Vec<(usize, Option<ToolOutput>)> = Vec::new();
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {

                let deadline = tokio::time::sleep(CANCEL_REPORT_GRACE);
                tokio::pin!(deadline);
                loop {
                    tokio::select! {
                        biased;
                        _ = &mut deadline => {
                            set.abort_all();
                            break;
                        }
                        res = set.join_next() => match res {
                            Some(Ok((idx, out))) => done.push((idx, Some(out))),
                            Some(Err(_)) => {}
                            None => break,
                        },
                    }
                }
                while let Some(res) = set.join_next().await {
                    if let Ok((idx, out)) = res {
                        done.push((idx, Some(out)));
                    }
                }

                let completed: HashSet<usize> = done.iter().map(|(i, _)| *i).collect();
                for idx in inputs {
                    if !completed.contains(&idx) {
                        done.push((idx, None));
                    }
                }
                break;
            }
            res = set.join_next() => {
                match res {
                    None => break,
                    Some(Ok((idx, out))) => done.push((idx, Some(out))),

                    Some(Err(_)) => {}
                }
            }
        }
    }
    done.sort_by_key(|(i, _)| *i);
    done
}

#[path = "parallel_tests.rs"]
#[cfg(test)]
mod tests;

#[path = "parallel_bound_tests.rs"]
#[cfg(test)]
mod bound_tests;

#[path = "parallel_dag_tests.rs"]
#[cfg(test)]
mod dag_tests;

#[path = "parallel_screen_tests.rs"]
#[cfg(test)]
mod screen_tests;
