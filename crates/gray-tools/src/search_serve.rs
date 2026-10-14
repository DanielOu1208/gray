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
        Ok((argv, files)) => json!({ "served": true, "argv": argv, "candidates": files.len(), "files": files }),
        Err(why) => json!({ "served": false, "reason": why }),
    };
    reply["server_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
    reply
}

/// The real tool's argv and the display paths of the files it must read.
type Answer = (Vec<String>, Vec<String>);

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

fn grep(server: &Server, argv: &[String], cwd: &Path) -> Result<Answer, &'static str> {
    let plan = search_plan::grep_plan(argv)?;
    // Each operand is either a directory under the root walked recursively,
    // whose candidates come from the index, or anything else — a file, a
    // missing path, a directory without `-r`, one outside the index, one
    // whose name --exclude-dir skips, one under an unneedled pattern —
    // passed through for the real tool to read itself. `argv` keeps the
    // walk flags, so a pass-through operand is handled exactly as the real
    // tool would.
    enum Op {
        Indexed { scope: PathBuf, prefix: String },
        Pass(String),
    }
    let mut ops = Vec::new();
    let mut indexed = false;
    for raw in &plan.operands {
        let joined = cwd.join(raw);
        let scope = (plan.recursive && joined.is_dir()).then(|| joined.canonicalize().ok()).flatten();
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
        let st = server.engine.status();
        if !st.ready || st.root_missing || st.content_pending > 0 || st.content_docs == 0 {
            return Err("index not settled");
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
    Ok((out_argv, files))
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

/// Names of `dir` in `readdir` order. `None` for a directory past fts's
/// inode-sort threshold, whose order we do not reproduce.
fn readdir_order(dir: &Path) -> Option<HashMap<OsString, usize>> {
    let names: Vec<OsString> = std::fs::read_dir(dir).ok()?.map(|e| e.map(|e| e.file_name())).collect::<Result<_, _>>().ok()?;
    if names.len() > FTS_INODE_SORT_ENTRIES {
        return None;
    }
    Some(names.into_iter().enumerate().map(|(i, n)| (n, i)).collect())
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
