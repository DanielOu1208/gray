//! The grep/rg shim, server side: answers `search_shim` requests from the
//! session's index for one root. Every answer is either the real tool's argv
//! plus the files it must read, or a decline. The index only prunes; the
//! real tool produces the output.
//!
//! The candidate files for a query are the union of three sets, each a
//! superset of what the real tool could match in its walk:
//! - files the content index does not hold as they are (not eligible, or
//!   changed since their sync), which every search reads directly;
//! - held files whose content has a required literal of the pattern;
//! - files changed since the server started (inotify), always.
//!
//! Each is then filtered by the walk the real tool would have done, and the
//! survivors are listed in the order that walk reaches them.
//!
//! `gray search-serve --root DIR --sock PATH` runs this outside a session
//! (the differential tester's entry point); a session runs the same server.

use std::collections::{BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Value, json};

use crate::search_plan::{self, GrepPlan};
use crate::search_watch::Watch;

/// Everything one server answers from.
struct Server {
    root: PathBuf,
    engine: fsearch::Engine,
    watch: Arc<Watch>,
    /// Directory listings behind walk-order keys, kept across requests and
    /// dropped when the watch reports a change.
    order: Mutex<HashMap<PathBuf, Option<HashMap<OsString, usize>>>>,
    /// Unheld paths under the root, cached until the watch reports a change.
    unheld: Mutex<Option<Arc<BTreeSet<Vec<u8>>>>>,
    /// Set only for a temporary index dir, whose Drop removes it. A
    /// persistent `--index-dir` is left in place across restarts.
    _index_dir: Option<tempfile::TempDir>,
}

/// Serve queries for `root` on `sock` until the process dies. `index_dir`,
/// when given, holds the index persistently so a restart reloads it instead
/// of rebuilding; otherwise a tempdir is used and removed on exit.
#[cfg(unix)]
pub fn serve(root: &Path, sock: &Path, index_dir: Option<&Path>) -> std::io::Result<()> {
    let root = root.canonicalize()?;
    let home = root.to_str().ok_or_else(|| std::io::Error::other("root is not UTF-8"))?.to_string();
    let (index_dir, _index_dir) = match index_dir {
        Some(d) => {
            std::fs::create_dir_all(d)?;
            if d.canonicalize().unwrap_or_else(|_| d.to_path_buf()).starts_with(&root) {
                return Err(std::io::Error::other("index dir must not be inside the indexed root"));
            }
            (d.to_path_buf(), None)
        }
        None => {
            let t = tempfile::Builder::new().prefix("gray-serve-").tempdir()?;
            (t.path().to_path_buf(), Some(t))
        }
    };
    // The watch goes in before the engine scans: anything written after this
    // point is reported, and anything before it is in the scan.
    let watch = Arc::new(Watch::new(&root)?);
    let drain = watch.clone();
    std::thread::spawn(move || drain.drain_forever());
    let engine = fsearch::Engine::start(fsearch::Options {
        dir: index_dir,
        home,
        root: Some(root.clone()),
        skip: Some(Vec::new()),
    })
    .map_err(std::io::Error::other)?;
    let server = Arc::new(Server {
        root,
        engine,
        watch,
        order: Mutex::new(HashMap::new()),
        unheld: Mutex::new(None),
        _index_dir,
    });
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let server = server.clone();
        std::thread::spawn(move || {
            let _ = answer(&server, stream);
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn serve(_root: &Path, _sock: &Path, _index_dir: Option<&Path>) -> std::io::Result<()> {
    Err(std::io::Error::other("search-serve needs a unix socket"))
}

/// One request line in, one reply line out.
#[cfg(unix)]
fn answer(server: &Server, mut stream: UnixStream) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let mut out = decide(server, &line).to_string();
    out.push('\n');
    stream.write_all(out.as_bytes())
}

fn decide(server: &Server, line: &str) -> Value {
    let started = Instant::now();
    let mut reply = match serve_line(server, line) {
        Ok(Answer::Exec { argv, files, mode }) => json!({ "served": true, "argv": argv, "candidates": files.len(), "files": files, "mode": mode }),
        Ok(Answer::Print { stdout, stderr, code }) => {
            use base64::Engine as _;
            json!({
                "served": true,
                "mode": "answer",
                "candidates": 0,
                "stdout": base64::engine::general_purpose::STANDARD.encode(&stdout),
                "stderr": base64::engine::general_purpose::STANDARD.encode(&stderr),
                "code": code,
            })
        }
        Err(why) => json!({ "served": false, "reason": why }),
    };
    reply["server_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
    reply
}

/// How a servable call is answered: the real tool's argv and the display
/// paths of the files it must read (`Exec` — `"index"` mode when the index
/// pruned the list, `"pass"` for a verbatim pass-through), or the real
/// tool's exact output, exit code included, computed here (`Print` —
/// `"answer"` mode: the index ran the search, no tool runs at all).
pub(crate) enum Answer {
    Exec { argv: Vec<String>, files: Vec<String>, mode: &'static str },
    Print { stdout: Vec<u8>, stderr: Vec<u8>, code: i32 },
}

fn serve_line(server: &Server, line: &str) -> Result<Answer, &'static str> {
    let req: Value = serde_json::from_str(line).map_err(|_| "bad request")?;
    let argv = req["argv"]
        .as_array()
        .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_string)).collect::<Option<Vec<_>>>())
        .ok_or("bad request")?;
    let cwd = Path::new(req["cwd"].as_str().ok_or("bad request")?);
    match req["tool"].as_str() {
        Some("grep") => grep(server, &argv, cwd),
        _ => Err("tool not served"),
    }
}

/// One operand's resolution: `Indexed` is a recursive directory under the
/// root served from the index; `Pass` is anything else — a file, a missing
/// path, a directory without `-r`, one outside the index, one whose name
/// `--exclude-dir` skips, one under an unneedled pattern — passed through
/// for the real tool to read itself. `argv` keeps the walk flags, so a
/// pass-through operand is handled exactly as the real tool would.
enum Op {
    Indexed { scope: PathBuf, prefix: String },
    Pass(String),
}

/// What the answer path draws on besides the parsed plan: an engine plus
/// the server's freshness overlays. The pool-backed caller (`grep_answer`)
/// carries neither a watch nor a root-wide unheld cache — it asks the
/// engine per scope and lets the filesystem walk cover the rest.
struct Src<'a> {
    engine: &'a fsearch::Engine,
    /// The server's root: the scope `unheld_files` is cached at. Unused by
    /// a pool caller (its `unheld` is `None`).
    root: &'a Path,
    watch: Option<&'a Watch>,
    unheld: Option<&'a Mutex<Option<Arc<BTreeSet<Vec<u8>>>>>>,
}

fn grep(server: &Server, argv: &[String], cwd: &Path) -> Result<Answer, &'static str> {
    let plan = search_plan::grep_plan(argv)?;
    let dbg = |m: &str| { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[{m}]"); } };
    dbg("plan");
    let mut ops = Vec::new();
    let mut indexed = false;
    for raw in &plan.operands {
        let joined = cwd.join(raw);
        let scope = (plan.recursive && !raw.is_empty() && joined.is_dir())
            .then(|| joined.canonicalize().ok())
            .flatten();
        let serve = scope.as_ref().is_some_and(|s| {
            plan.all_needled
                && !plan.needles.is_empty()
                && s.starts_with(&server.root)
                && !s.file_name().is_some_and(|n| plan.walk.skips_dir(n.as_bytes()))
        });
        if !serve {
            ops.push(Op::Pass(raw.clone()));
            continue;
        }
        let scope = scope.unwrap();
        let trimmed = raw.trim_end_matches('/');
        let prefix = if raw.starts_with('/') && trimmed.is_empty() { "/".to_string() } else { format!("{trimmed}/") };
        ops.push(Op::Indexed { scope, prefix });
        indexed = true;
    }
    let mut files = Vec::new();
    if indexed {
        dbg("ops");
        let st = server.engine.status();
        if !st.ready || st.root_missing || st.content_pending > 0 || st.content_docs == 0 {
            return Err("index not settled");
        }
        if plan.answerable() && ops.iter().all(|op| matches!(op, Op::Indexed { .. })) {
            let mut order = server.order.lock().unwrap();
            let mut unheld: Option<Arc<BTreeSet<Vec<u8>>>> = None;
            let src = Src {
                engine: &server.engine,
                root: &server.root,
                watch: Some(&server.watch),
                unheld: Some(&server.unheld),
            };
            if let Some(print) = print_answer(&src, &plan, &ops, &mut order, &mut unheld)? {
                return Ok(print);
            }
        }
        let mut order = server.order.lock().unwrap();
        let mut unheld: Option<Arc<BTreeSet<Vec<u8>>>> = None;
        for op in &ops {
            match op {
                Op::Pass(raw) => files.push(raw.clone()),
                Op::Indexed { scope, prefix } => files.extend(dir_files(server, scope, prefix, &plan, &mut order, &mut unheld)?),
            }
        }
    } else {
        files = plan.operands.clone();
    }
    // Filename printing matches grep's rule on the operands the caller gave:
    // on for more than one operand or any directory operand, off for a lone
    // file, and whatever `-H`/`-h` the caller gave regardless.
    let mut out_argv = plan.argv.clone();
    // `-r` was lifted into the plan; a pass-through operand that is a
    // directory still needs the real tool to recurse it.
    if plan.recursive {
        out_argv.push("-r".into());
    }
    if plan.with_name.is_none() && (ops.len() > 1 || indexed) {
        out_argv.insert(0, "-H".into());
    }
    Ok(Answer::Exec { argv: out_argv, files, mode: if indexed { "index" } else { "pass" } })
}

/// The display paths under one indexed operand: the union of unheld,
/// literal-matching, and watch-changed files, walk-filtered and listed in
/// the order the real tool's walk reaches them.
fn dir_files(
    server: &Server,
    scope: &Path,
    prefix: &str,
    plan: &GrepPlan,
    order: &mut HashMap<PathBuf, Option<HashMap<OsString, usize>>>,
    unheld: &mut Option<Arc<BTreeSet<Vec<u8>>>>,
) -> Result<Vec<String>, &'static str> {
    let scope_bytes = scope.as_os_str().as_bytes();
    let changed = server.watch.changed(scope_bytes).ok_or("watch broken")?;
    if !changed.is_empty() {
        order.clear();
        *unheld = None;
        *server.unheld.lock().unwrap() = None;
    }
    if unheld.is_none() {
        let mut cache = server.unheld.lock().unwrap();
        if cache.is_none() {
            let root_bytes = server.root.as_os_str().as_bytes();
            let all: BTreeSet<Vec<u8>> = server
                .engine
                .unheld_files(root_bytes)
                .ok_or("content index not holding scope")?
                .into_iter()
                .collect();
            *cache = Some(Arc::new(all));
        }
        *unheld = Some(cache.as_ref().unwrap().clone());
    }
    let unheld = unheld.as_ref().unwrap().iter().filter(|p| {
        p.starts_with(scope_bytes)
            && (p.len() == scope_bytes.len() || p.get(scope_bytes.len()) == Some(&b'/'))
    }).cloned();
    let matched = server
        .engine
        .held_matches(scope_bytes, &plan.needles, plan.ignore_case)
        .ok_or("match answer partial")?;
    let all: BTreeSet<Vec<u8>> = unheld.chain(matched).chain(changed).collect();
    let mut found = Vec::new();
    for p in all {
        let path = Path::new(OsStr::from_bytes(&p));
        let Ok(rel) = path.strip_prefix(scope) else { continue };
        if !walked(plan, rel) || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let name = rel.to_str().ok_or("path is not UTF-8")?;
        let key = walk_key(order, scope, rel).ok_or("walk order unknown")?;
        found.push((key, format!("{prefix}{name}")));
    }
    // The real tool reads files in the order its walk reaches them; so do we.
    found.sort();
    Ok(found.into_iter().map(|(_, f)| f).collect())
}

/// Where the real tool's walk reaches `rel` under `scope`: the position of
/// each component in its directory's `readdir` order, outermost first. This
/// is the order fts gives with no comparator. `order` caches one directory
/// listing per directory.
fn walk_key(order: &mut HashMap<PathBuf, Option<HashMap<OsString, usize>>>, scope: &Path, rel: &Path) -> Option<Vec<usize>> {
    let mut dir = scope.to_path_buf();
    let mut key = Vec::new();
    for c in rel.components() {
        let name = c.as_os_str();
        let listing = order.entry(dir.clone()).or_insert_with(|| readdir_order(&dir));
        key.push(*listing.as_ref()?.get(name)?);
        dir.push(name);
    }
    Some(key)
}

/// Names of `dir` in `readdir` order — or, past fts's inode-sort threshold,
/// in ascending inode number, the way gnulib fts orders a wide directory.
fn readdir_order(dir: &Path) -> Option<HashMap<OsString, usize>> {
    let names: Vec<OsString> = std::fs::read_dir(dir).ok()?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>().ok()?;
    if names.len() <= FTS_INODE_SORT_ENTRIES {
        return Some(names.into_iter().enumerate().map(|(i, n)| (n, i)).collect());
    }
    use std::os::unix::fs::MetadataExt as _;
    let mut by_ino: Vec<(u64, OsString)> = names
        .into_iter()
        .map(|n| {
            let ino = std::fs::metadata(dir.join(&n)).map(|m| m.ino()).unwrap_or(0);
            (ino, n)
        })
        .collect();
    by_ino.sort();
    Some(by_ino.into_iter().enumerate().map(|(i, (_, n))| (n, i)).collect())
}

/// gnulib fts sorts a directory by inode number past this many entries.
const FTS_INODE_SORT_ENTRIES: usize = 10_000;

/// Would the real tool's recursion under the operand visit this file?
/// `rel` is the path under the operand; every directory on it must pass
/// `--exclude-dir`, and the file its include and exclude rules.
fn walked(plan: &GrepPlan, rel: &Path) -> bool {
    let mut comps = rel.components().peekable();
    while let Some(c) = comps.next() {
        let name = c.as_os_str().as_bytes();
        if comps.peek().is_none() {
            return plan.walk.keeps_file(name);
        }
        if plan.walk.skips_dir(name) {
            return false;
        }
    }
    false
}


// ---------------------------------------------------------------------
// Answer mode: the index prints the real tool's bytes itself.
// ---------------------------------------------------------------------

/// One file's contribution to the output, in walk order.
struct Block {
    /// Path under the scope: dedupe key and display source.
    rel: PathBuf,
    /// Position in the real tool's walk of its operand.
    key: Vec<usize>,
    /// The path as the real tool prints it (operand prefix + relative).
    display: String,
    /// The absolute path, for a long-line re-read.
    abs: PathBuf,
    /// Matches from the index (or from reading an unheld file).
    fm: Option<fsearch::FileMatches>,
    /// A binary file: matched once, announced instead of line-matched.
    binary: bool,
    /// The file matched (for `-l` / the exit code), whatever is printed.
    matched: bool,
    /// A read error the real tool would report (unless `-s`).
    err: Option<String>,
}

/// The last printed match's position, for context-group adjacency:
/// (operand index, absolute path bytes, last printed line number).
type LastPrinted = Option<(usize, Vec<u8>, usize)>;

/// Print a grep the index can answer exactly, or `None` to fall back to
/// handing the real tool the candidate files. `Err` declines the call.
fn print_answer(
    src: &Src,
    plan: &GrepPlan,
    ops: &[Op],
    order: &mut HashMap<PathBuf, Option<HashMap<OsString, usize>>>,
    unheld: &mut Option<Arc<BTreeSet<Vec<u8>>>>,
) -> Result<Option<Answer>, &'static str> {
    let Some(mut g) = plan.fsearch_grep() else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: fsearch_grep"); } return Ok(None) };
    let mut blocks: Vec<(usize, Block)> = Vec::new();
    let mut walked_files: Vec<(usize, PathBuf)> = Vec::new();
    for (oi, op) in ops.iter().enumerate() {
        let Op::Indexed { scope, prefix } = op else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: op not indexed"); } return Ok(None) };
        let scope_bytes = scope.as_os_str().as_bytes();
        // Index-held matches, paged to completion. The engine reads dirty
        // files itself, so these are already fresh.
        g.skip = 0;
        loop {
            let q = fsearch::Query {
                scope: Some(scope_bytes.to_vec()),
                limit: usize::MAX,
                size: (0, u64::MAX),
                mtime: (0, u32::MAX),
                ..Default::default()
            };
            if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[engine.grep {scope:?}]", scope = scope); }
            let (res, _idx) = src.engine.grep(&q, &g).map_err(|_| "index grep failed")?;
            if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[grepped: {} files]", res.files.len()); }
            if !res.complete {
                return Ok(None);
            }
            let next = res.next;
            for fm in res.files {
                let path = Path::new(OsStr::from_bytes(&fm.path));
                let Ok(rel) = path.strip_prefix(scope) else { continue };
                if !walked(plan, rel) {
                    continue;
                }
                let Some(key) = walk_key(order, scope, rel) else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: no walk_key {rel:?}"); } return Ok(None) };
                let Some(name) = rel.to_str() else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: nonutf8 {rel:?}"); } return Ok(None) };
                blocks.push((oi, Block {
                    key,
                    rel: rel.to_path_buf(),
                    display: format!("{prefix}{name}"),
                    abs: path.to_path_buf(),
                    fm: Some(fm),
                    binary: false,
                    matched: true,
                    err: None,
                }));
            }
            match next {
                Some(n) => g.skip = n,
                None => break,
            }
        }
        // Every regular file under the scope the engine did not already
        // answer is read here, the way the real tool would — unheld,
        // watch-changed and ignored files alike. Only a walk can name them
        // all; the index alone never saw some (`.git` internals).
        // Files the index does not already answer for are read here, the
        // way the real tool reads them: its unheld set, the watch's changed
        // set, and — reachable only by walking — files under directories the
        // name index ignores entirely (`.git` internals).
        if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[changed]"); }
        let mut extra: BTreeSet<Vec<u8>> = match src.watch {
            Some(w) => w.changed(scope_bytes).ok_or("watch broken")?.into_iter().collect(),
            None => BTreeSet::new(),
        };
        match src.unheld {
            Some(cache) => {
                if unheld.is_none() {
                    let mut c = cache.lock().unwrap();
                    if c.is_none() {
                        if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[unheld_files]"); }
                        let all: BTreeSet<Vec<u8>> = src
                            .engine
                            .unheld_files(src.root.as_os_str().as_bytes())
                            .ok_or("content index not holding scope")?
                            .into_iter()
                            .collect();
                        *c = Some(Arc::new(all));
                    }
                    *unheld = Some(c.as_ref().unwrap().clone());
                }
                extra.extend(
                    unheld
                        .as_ref()
                        .unwrap()
                        .iter()
                        .filter(|p| {
                            p.starts_with(scope_bytes)
                                && (p.len() == scope_bytes.len() || p.get(scope_bytes.len()) == Some(&b'/'))
                        })
                        .cloned(),
                );
            }
            None => {
                if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[unheld_files]"); }
                extra.extend(
                    src.engine
                        .unheld_files(scope_bytes)
                        .ok_or("content index not holding scope")?,
                );
            }
        }
        if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[names]"); }
        let known = src
            .engine
            .search(&fsearch::Query {
                scope: Some(scope_bytes.to_vec()),
                name_re: Some(regex::bytes::Regex::new(".*").map_err(|_| "re")?),
                size: (0, u64::MAX),
                mtime: (0, u32::MAX),
                limit: 1 << 24, // the searcher's 2*limit buffer would overflow usize::MAX
                ..Default::default()
            })
            .map_err(|_| "index list failed")?
            .into_iter()
            .map(|f| f.path.as_os_str().as_bytes().to_vec())
            .collect::<std::collections::HashSet<Vec<u8>>>();
        if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[walk]"); }
        let mut stack = vec![scope.to_path_buf()];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: readdir {d:?}"); } return Ok(None) };
            for e in rd.flatten() {
                let p = e.path();
                let bytes = p.as_os_str().as_bytes();
                let Ok(ft) = e.file_type() else { continue };
                if ft.is_dir() {
                    // Under -c the walk enumerates files the name index
                    // already knows, so it descends into known dirs too;
                    // otherwise the index answers for the whole subtree.
                    if plan.count || !known.contains(bytes) {
                        stack.push(p);
                    }
                    continue;
                }
                // `-r` follows symlinks only on the command line; a link
                // found by the walk is skipped entirely.
                if !ft.is_file() {
                    continue;
                }
                if plan.count
                    && let Ok(rel) = p.strip_prefix(scope)
                    && walked(plan, rel)
                {
                    walked_files.push((oi, rel.to_path_buf()));
                }
                if !known.contains(bytes) {
                    extra.insert(bytes.to_vec());
                }
            }
        }
        if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[extra {} files]", extra.len()); }
        // Fresh-read paths may reach us in a different byte form than the
        // engine's answer: compare the path relative to the scope, which
        // normalizes both.
        for p in extra {
            let path = Path::new(OsStr::from_bytes(&p));
            let Ok(rel) = path.strip_prefix(scope) else { continue };
            if !walked(plan, rel) || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
                continue;
            }
            if blocks
                .iter()
                .any(|(bo, b)| *bo == oi && b.rel.as_os_str().as_bytes() == rel.as_os_str().as_bytes())
            {
                continue; // the engine's fresh-read answer already covers it
            }
            let Some(key) = walk_key(order, scope, rel) else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: walk_key2 {rel:?}"); } return Ok(None) };
            let Some(name) = rel.to_str() else { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: nonutf8 {rel:?}"); } return Ok(None) };
            let display = format!("{prefix}{name}");
            let block = match read_block(&g, path, plan) {
                ReadBlock::File { fm, binary, matched } => Block {
                    key,
                    rel: rel.to_path_buf(),
                    display,
                    abs: path.to_path_buf(),
                    fm,
                    binary,
                    matched,
                    err: None,
                },
                ReadBlock::Err(e) => Block {
                    key,
                    rel: rel.to_path_buf(),
                    display,
                    abs: path.to_path_buf(),
                    fm: None,
                    binary: false,
                    matched: false,
                    err: Some(e),
                },
                ReadBlock::Bail => { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: read_block {path:?}"); } return Ok(None) },
            };
            blocks.push((oi, block));
        }
    }
    // A file prints nothing unless it matched, errored, or is a binary that
    // matched (announced or listed) — drop the rest.
    blocks.retain(|(_, b)| b.fm.is_some() || (b.binary && b.matched) || b.err.is_some());
    blocks.sort_by(|(ao, a), (bo, b)| (ao, &a.key).cmp(&(bo, &b.key)));
    // A match that came through a symlink is one `-r` never made.
    blocks.retain(|(_, b)| {
        !std::fs::symlink_metadata(&b.abs).is_ok_and(|m| m.file_type().is_symlink())
    });
    // `-l` fully overrides `-c`: only matching names, no counts.
    if plan.count && !plan.files_only {
        return emit_count(plan, ops, blocks, walked_files, order);
    }
    if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[emit]"); }
    emit(plan, blocks)
}

/// The answer path with no socket and no server: `grep` argv answered in
/// process off a caller's engine (the process-wide pool). Every operand
/// must be a recursive directory the index covers, the pattern must carry a
/// required literal, and the plan must be exactly renderable — anything
/// else is `None` and the real tool answers.
pub(crate) fn grep_answer(engine: &fsearch::Engine, argv: &[String], cwd: &Path) -> Option<Answer> {
    let plan = search_plan::grep_plan(argv).ok()?;
    if !plan.recursive
        || !plan.answerable()
        || !plan.all_needled
        || plan.needles.is_empty()
        || plan.operands.is_empty()
    {
        return None;
    }
    let st = engine.status();
    if !st.ready || st.content_pending > 0 || st.content_docs == 0 {
        return None;
    }
    let mut ops = Vec::with_capacity(plan.operands.len());
    for raw in &plan.operands {
        let joined = cwd.join(raw);
        let scope = (!raw.is_empty() && joined.is_dir())
            .then(|| joined.canonicalize().ok())
            .flatten()
            .filter(|s| {
                engine.covers(s.as_os_str().as_bytes())
                    && !s.file_name().is_some_and(|n| plan.walk.skips_dir(n.as_bytes()))
            })?;
        let trimmed = raw.trim_end_matches('/');
        let prefix = if raw.starts_with('/') && trimmed.is_empty() {
            "/".to_string()
        } else {
            format!("{trimmed}/")
        };
        ops.push(Op::Indexed { scope, prefix });
    }
    let src = Src { engine, root: Path::new("/"), watch: None, unheld: None };
    let mut order = HashMap::new();
    let mut unheld = None;
    match print_answer(&src, &plan, &ops, &mut order, &mut unheld) {
        Ok(Some(a @ Answer::Print { .. })) => Some(a),
        _ => None,
    }
}

/// `-c`: the real tool prints one `file:N` (or `N`) per file its walk
/// visits, zero for files it found nothing in. `walked` is every file the
/// operand's recursion reached; `blocks` the ones that produced anything.
fn emit_count(
    plan: &GrepPlan,
    ops: &[Op],
    blocks: Vec<(usize, Block)>,
    walked: Vec<(usize, PathBuf)>,
    order: &mut HashMap<PathBuf, Option<HashMap<OsString, usize>>>,
) -> Result<Option<Answer>, &'static str> {
    let show_name = plan.with_name.unwrap_or(true);
    let mut by_rel: HashMap<(usize, Vec<u8>), &Block> = HashMap::new();
    for (oi, b) in &blocks {
        by_rel.insert((*oi, b.rel.as_os_str().as_bytes().to_vec()), b);
    }
    let mut rows: Vec<(usize, Vec<usize>, PathBuf, String)> = Vec::new();
    for (oi, rel) in walked {
        let Op::Indexed { scope, prefix } = &ops[oi] else { return Ok(None) };
        let Some(key) = walk_key(order, scope, &rel) else { return Ok(None) };
        let Some(name) = rel.to_str() else { return Ok(None) };
        rows.push((oi, key, rel.clone(), format!("{prefix}{name}")));
    }
    rows.sort();
    let mut out = Vec::<u8>::new();
    let mut err = String::new();
    let mut errored = false;
    let mut any = false;
    for (oi, _, rel, display) in &rows {
        let n = match by_rel.get(&(*oi, rel.as_os_str().as_bytes().to_vec())) {
            None => 0,
            Some(b) => {
                if let Some(e) = &b.err {
                    if !plan.no_messages {
                        use std::fmt::Write as _;
                        let _ = writeln!(err, "grep: {}: {}", display, e);
                    }
                    errored = true;
                    continue;
                }
                match &b.fm {
                    None => 0,
                    Some(fm) => count_lines(fm, plan)?,
                }
            }
        };
        any |= n > 0;
        if show_name {
            out.extend_from_slice(display.as_bytes());
            out.push(b':');
        }
        out.extend_from_slice(n.to_string().as_bytes());
        out.push(b'\n');
    }
    if plan.quiet {
        out.clear();
    }
    let code = if errored { 2 } else if any { 0 } else { 1 };
    Ok(Some(Answer::Print { stdout: out, stderr: err.into_bytes(), code }))
}

/// Lines in `fm` the flags actually select: every matching line, or under
/// `-w` just the ones with a word-boundary range.
fn count_lines(fm: &fsearch::FileMatches, plan: &GrepPlan) -> Result<usize, &'static str> {
    if !plan.word {
        return Ok(fm.lines.len());
    }
    let mut n = 0;
    for (ln, hit) in fm.lines.iter().zip(fm.hits.iter()) {
        if ln.1.contains('\u{FFFD}') {
            return Err("lossy text");
        }
        n += hit.ranges.iter().any(|r| word_keep(&ln.1, *r)) as usize;
    }
    Ok(n)
}

/// What reading one unheld/changed file found.
enum ReadBlock {
    File { fm: Option<fsearch::FileMatches>, binary: bool, matched: bool },
    Err(String),
    /// The file's match cannot be printed exactly (a matching binary under
    /// `-o`/context/`-x`/`-w`/`-c`): the whole call falls back to exec.
    Bail,
}

/// Read one file the index does not hold as-it-is, the way the real tool
/// would see it: a file with a NUL byte is binary — the real tool announces
/// it, it does not print matching lines.
fn read_block(g: &fsearch::Grep, path: &Path, plan: &GrepPlan) -> ReadBlock {
    let buf = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return ReadBlock::Err(e.to_string()),
    };
    if buf.contains(&0) {
        if std::env::var_os("SERVE_DBG").is_some() { eprintln!("serve[binary {:?} match={}]", path, g.matches_buf(&buf)); }
        if plan.ignore_binary {
            return ReadBlock::File { fm: None, binary: true, matched: false };
        }
        if g.matches_buf(&buf) {
            // A matching binary is announced exactly only without `-o`,
            // context, `-x`, `-w` or `-c` shaping.
            if plan.only_matching || plan.before > 0 || plan.after > 0 || plan.line_match || plan.word || plan.count {
                return ReadBlock::Bail;
            }
            return ReadBlock::File { fm: None, binary: true, matched: true };
        }
        return ReadBlock::File { fm: None, binary: true, matched: false };
    }
    let res = fsearch::grep_file(g, path.as_os_str().as_bytes());
    let fm = res.files.into_iter().next();
    let matched = fm.is_some();
    ReadBlock::File { fm, binary: false, matched }
}

/// Emit the real tool's bytes for `blocks` and its exit code.
fn emit(plan: &GrepPlan, mut blocks: Vec<(usize, Block)>) -> Result<Option<Answer>, &'static str> {
    let _ = &mut blocks;
    if plan.ignore_binary {
        blocks.retain(|(_, b)| !b.binary);
    }
    let show_name = plan.with_name.unwrap_or(true);
    let mut out = Vec::<u8>::new();
    let mut err = String::new();
    let mut errored = false;
    let mut matched = false;
    // Context groups are separated by `--` whenever printed lines are not
    // adjacent — a file change breaks adjacency the way a line gap does.
    let mut printed = false;
    let mut last: LastPrinted = None; // (operand, path, last line)
    for (oi, b) in &blocks {
        if let Some(e) = &b.err {
            if !plan.no_messages {
                use std::fmt::Write as _;
                let _ = writeln!(err, "grep: {}: {}", b.display, e);
            }
            errored = true;
            continue;
        }
        if b.binary && !b.matched {
            continue;
        }
        match emit_block(&mut out, &mut err, plan, b, *oi, show_name, &mut printed, &mut last)? {
            Some(m) => {
                matched |= m;
                // `-q` exits at the first match: later errors and binary
                // notices are never printed by the real tool.
                if plan.quiet && m {
                    break;
                }
            }
            None => { if std::env::var_os("SERVE_DBG").is_some() { eprintln!("bail: emit_block {b:?}", b = b.display); } return Ok(None) },
        }
    }
    let code = if errored {
        2
    } else if matched {
        0
    } else {
        1
    };
    if plan.quiet {
        out.clear();
    }
    Ok(Some(Answer::Print { stdout: out, stderr: err.into_bytes(), code }))
}

/// One file's output, appended. `Ok(Some(matched))`: whether the file truly
/// matched after `-w` filtering (drives the exit code). `Ok(None)`: the
/// answer cannot be printed exactly — the caller falls back to exec.
fn emit_block(
    out: &mut Vec<u8>,
    err: &mut String,
    plan: &GrepPlan,
    b: &Block,
    oi: usize,
    show_name: bool,
    printed: &mut bool,
    last: &mut LastPrinted,
) -> Result<Option<bool>, &'static str> {
    if !b.matched {
        return Ok(Some(false));
    }
    if b.binary {
        if plan.files_only {
            out.extend_from_slice(b.display.as_bytes());
            out.push(b'\n');
        } else if !plan.quiet {
            err.push_str(&format!("grep: {}: binary file matches\n", b.display));
        }
        *printed = true;
        return Ok(Some(true));
    }
    let Some(fm) = &b.fm else { return Ok(Some(b.matched)) };
    // `-l` beats `-c` in the real tool; count output lives in emit_count.
    if plan.files_only {
        if plan.word && !word_match(fm, b)? {
            return Ok(Some(false));
        }
        out.extend_from_slice(b.display.as_bytes());
        out.push(b'\n');
        *printed = true;
        return Ok(Some(true));
    }
    // The real tool prints selected lines plus the context windows around
    // them, merged by line number: a line inside two windows (or that is a
    // match itself) prints once, and `--` separates printed groups that are
    // not line-adjacent — within a file and across files alike. The index's
    // stored text is lossy and trimmed; every printed line is re-read raw.
    let ctx = plan.before.max(plan.after);
    let raw = std::fs::read(&b.abs).map_err(|_| "file vanished mid-search")?;
    let lines: Vec<&[u8]> = raw.split(|&c| c == b'\n').collect();
    let line = |ln: usize| -> Result<&[u8], &'static str> {
        lines.get(ln.wrapping_sub(1)).copied().ok_or("line moved")
    };
    let mut rows: std::collections::BTreeMap<usize, Row> = std::collections::BTreeMap::new();
    let mut pending: Vec<usize> = Vec::new(); // word-miss lines: context if inside a window
    let mut windows: Vec<(usize, usize)> = Vec::new();
    let mut any_match = false;
    for ((ln, _text), hit) in fm.lines.iter().zip(fm.hits.iter()) {
        let ranges: Vec<(usize, usize)> = if plan.word || plan.only_matching {
            let raw_l = line(*ln)?;
            let sh = String::from_utf8_lossy(raw_l);
            let sh = sh.trim_end();
            if sh.contains('\u{FFFD}') {
                return Ok(None);
            }
            if plan.word {
                hit.ranges.iter().cloned().filter(|r| word_keep(sh, *r)).collect()
            } else {
                hit.ranges.clone()
            }
        } else {
            hit.ranges.clone()
        };
        if plan.word && ranges.is_empty() {
            pending.push(*ln);
            continue;
        }
        any_match = true;
        rows.insert(*ln, Row { is_match: true, ranges });
        if ctx > 0 && !plan.only_matching {
            windows.push((ln.saturating_sub(plan.before), ln + plan.after));
            for (k, _c) in hit.before.iter().enumerate() {
                let n = ln.saturating_sub(hit.before.len() - k);
                rows.entry(n).or_insert(Row { is_match: false, ranges: Vec::new() });
            }
            for (k, _c) in hit.after.iter().enumerate() {
                let n = ln + 1 + k;
                rows.entry(n).or_insert(Row { is_match: false, ranges: Vec::new() });
            }
        }
    }
    if !any_match {
        return Ok(Some(false));
    }
    for ln in pending {
        if windows.iter().any(|(lo, hi)| ln >= *lo && ln <= *hi) {
            rows.insert(ln, Row { is_match: false, ranges: Vec::new() });
        }
    }
    let mut emitted_any = false;
    for (ln, row) in &rows {
        // Group separation: `--` between printed groups that are not
        // adjacent — across files, or past the context span.
        let adjacent = last
            .as_ref()
            .is_some_and(|(loi, lp, ll)| *loi == oi && lp == b.abs.as_os_str().as_bytes() && *ln <= ll + 1);
        if ctx > 0 && !plan.only_matching && *printed && !adjacent {
            out.extend_from_slice(b"--\n");
        }
        let raw_l = line(*ln)?;
        if row.is_match {
            if plan.only_matching {
                let sh = String::from_utf8_lossy(raw_l);
                let sh = sh.trim_end();
                if sh.contains('\u{FFFD}') {
                    return Ok(None);
                }
                for r in &row.ranges {
                    let Some(seg) = sh.get(r.0..r.1) else { return Ok(None) };
                    print_line(out, b, plan, show_name, plan.line_number, *ln, seg.as_bytes());
                }
            } else {
                print_line(out, b, plan, show_name, plan.line_number, *ln, raw_l);
            }
        } else {
            print_ctx(out, b, plan, show_name, *ln, raw_l);
        }
        *printed = true;
        emitted_any = true;
        *last = Some((oi, b.abs.as_os_str().as_bytes().to_vec(), *ln));
    }
    Ok(Some(emitted_any))
}

/// A printable line: the file's raw line at that number, a match or context.
struct Row {
    is_match: bool,
    ranges: Vec<(usize, usize)>,
}

/// Under `-w`, did the file produce a word-boundary match? (`-l` and the
/// exit code depend on it, not just the printed lines.)
fn word_match(fm: &fsearch::FileMatches, b: &Block) -> Result<bool, &'static str> {
    for ((ln, text), hit) in fm.lines.iter().zip(fm.hits.iter()) {
        if text.contains('\u{FFFD}') {
            return Ok(false);
        }
        let full = ensure_full(b, *ln, text)?;
        let text = full.as_deref().unwrap_or(text.as_str());
        if hit.ranges.iter().any(|r| word_keep(text, *r)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `text` truncated at 400 bytes? Read the true line off disk. `None` when
/// the stored text is already whole.
fn ensure_full(b: &Block, ln: usize, text: &str) -> Result<Option<String>, &'static str> {
    if text.len() < 400 {
        return Ok(None);
    }
    let buf = std::fs::read(&b.abs).map_err(|_| "file vanished mid-search")?;
    let line = buf.split(|&c| c == b'\n').nth(ln - 1).ok_or("line moved")?;
    Ok(Some(String::from_utf8_lossy(line).trim_end().to_string()))
}

/// Does match range `r` sit on a word boundary in `text`? GNU `-w`: the
/// match must not touch a word constituent `[A-Za-z0-9_]` on either side.
fn word_keep(text: &str, r: (usize, usize)) -> bool {
    let bs = text.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let left = r.0 == 0 || !word(bs[r.0 - 1]);
    let right = r.1 >= bs.len() || !word(bs[r.1]);
    left && right
}

/// A matching line as the real tool prints it.
fn print_line(out: &mut Vec<u8>, b: &Block, _plan: &GrepPlan, show_name: bool, numbered: bool, ln: usize, text: &[u8]) {
    if show_name {
        out.extend_from_slice(b.display.as_bytes());
        out.push(b':');
    }
    if numbered {
        out.extend_from_slice(ln.to_string().as_bytes());
        out.push(b':');
    }
    out.extend_from_slice(text);
    out.push(b'\n');
}

/// A context line: `file-line- text` instead of `file:line: text`.
fn print_ctx(out: &mut Vec<u8>, b: &Block, plan: &GrepPlan, show_name: bool, ln: usize, text: &[u8]) {
    if show_name {
        out.extend_from_slice(b.display.as_bytes());
        out.push(b'-');
    }
    if plan.line_number {
        out.extend_from_slice(ln.to_string().as_bytes());
        out.push(b'-');
    }
    out.extend_from_slice(text);
    out.push(b'\n');
}

#[path = "search_serve_tests.rs"]
#[cfg(test)]
mod tests;
