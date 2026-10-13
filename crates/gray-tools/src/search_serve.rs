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
use std::sync::Arc;
use std::time::Instant;

use serde_json::{Value, json};

use crate::search_plan::{self, GrepPlan};
use crate::search_watch::Watch;

/// Everything one server answers from. The index directory is removed when
/// the process exits.
struct Server {
    root: PathBuf,
    engine: fsearch::Engine,
    watch: Arc<Watch>,
    _index_dir: tempfile::TempDir,
}

/// Serve queries for `root` on `sock` until the process dies.
#[cfg(unix)]
pub fn serve(root: &Path, sock: &Path) -> std::io::Result<()> {
    let root = root.canonicalize()?;
    let home = root.to_str().ok_or_else(|| std::io::Error::other("root is not UTF-8"))?.to_string();
    let index_dir = tempfile::Builder::new().prefix("gray-serve-").tempdir()?;
    // The watch goes in before the engine scans: anything written after this
    // point is reported, and anything before it is in the scan.
    let watch = Arc::new(Watch::new(&root)?);
    let drain = watch.clone();
    std::thread::spawn(move || drain.drain_forever());
    let engine = fsearch::Engine::start(fsearch::Options {
        dir: index_dir.path().to_path_buf(),
        home,
        root: Some(root.clone()),
        skip: Some(Vec::new()),
    })
    .map_err(std::io::Error::other)?;
    let server = Arc::new(Server { root, engine, watch, _index_dir: index_dir });
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
pub fn serve(_root: &Path, _sock: &Path) -> std::io::Result<()> {
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
    let st = server.engine.status();
    if !st.ready || st.root_missing || st.content_pending > 0 || st.content_docs == 0 {
        return Err("index not settled");
    }
    let (scope, prefix) = operand(&server.root, cwd, &plan)?;
    let scope_bytes = scope.as_os_str().as_bytes();
    let changed = server.watch.changed(scope_bytes).ok_or("watch broken")?;
    let unheld = server.engine.unheld_files(scope_bytes).ok_or("content index not holding scope")?;
    let matched = server
        .engine
        .held_matches(scope_bytes, &plan.needles, plan.ignore_case)
        .ok_or("match answer partial")?;
    let all: BTreeSet<Vec<u8>> = unheld.into_iter().chain(matched).chain(changed).collect();
    let mut order = HashMap::new();
    let mut found = Vec::new();
    for p in all {
        let path = Path::new(OsStr::from_bytes(&p));
        let Ok(rel) = path.strip_prefix(&scope) else { continue };
        if !walked(&plan, rel) || !std::fs::symlink_metadata(path).is_ok_and(|m| m.is_file()) {
            continue;
        }
        let name = rel.to_str().ok_or("path is not UTF-8")?;
        let key = walk_key(&mut order, &scope, rel).ok_or("walk order unknown")?;
        found.push((key, format!("{prefix}{name}")));
    }
    // The real tool reads files in the order its walk reaches them; so do we.
    found.sort();
    Ok((plan.argv, found.into_iter().map(|(_, f)| f).collect()))
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

/// The canonical directory the operand names under the root, and the prefix
/// the real tool prints before each path under it (`operand/`, as given with
/// any trailing slash removed).
fn operand(root: &Path, cwd: &Path, plan: &GrepPlan) -> Result<(PathBuf, String), &'static str> {
    let raw = cwd.join(&plan.operand);
    if !raw.is_dir() {
        return Err("operand is not a directory");
    }
    let scope = raw.canonicalize().map_err(|_| "operand does not resolve")?;
    if !scope.starts_with(root) {
        return Err("operand outside the index root");
    }
    if scope.file_name().is_some_and(|n| plan.walk.skips_dir(n.as_bytes())) {
        return Err("operand excluded by --exclude-dir");
    }
    let trimmed = plan.operand.trim_end_matches('/');
    let prefix = if plan.operand.starts_with('/') && trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("{trimmed}/")
    };
    Ok((scope, prefix))
}

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
