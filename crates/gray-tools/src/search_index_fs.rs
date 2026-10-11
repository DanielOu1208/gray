//! Resident file index (fsearch) — second backend behind the same
//! Option-fallback contract as `search_index.rs`'s fff pool.
//!
//! One `fsearch::Engine` per process (not per root): it holds a persistent
//! whole-disk index under `gray_home()/fsearch`, mmap'd between launches,
//! so the first indexed search in a fresh process is as warm as the last
//! one in the previous process. `in:` scopes stand in for fff's per-root
//! pickers; a root the index has not covered declines to fd/rg exactly like
//! an unbuilt picker did.
//!
//! The model-visible contract is unchanged: `indexed_*` return `None` for
//! anything the index cannot answer exactly — pattern forms it cannot
//! express, scopes it has not indexed, results it may have truncated — and
//! the fd/rg lanes stay authoritative. Hits that pass are post-filtered
//! through `index_filter::IgnoreChain` so gitignore'd paths never leak in:
//! fd and rg apply ignore rules, the index does not.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use fsearch::{Engine, Grep, GrepMode, Query};
use tokio_util::sync::CancellationToken;

use crate::grep_builtin::Found;
use crate::index_filter::{IgnoreChain, Lane};

/// How far past the caller's limit the index is asked to run. fd takes the
/// first `limit` matches it walks; the index returns at most `limit` by
/// rank, which may truncate *before* the ignore filter drops anything — so
/// the query over-fetches and only a provably complete (untruncated) result
/// is served.
const FETCH_HEADROOM: usize = 4;
const FETCH_MIN: usize = 64;
/// Beyond this an indexed answer is implausible anyway; fd walks the tree.
const FETCH_MAX: usize = 4096;

/// File cap for a scoped content search. `GrepResult.files` beyond it flag
/// `limit_reached`, same as an overshot page on the fff lane.
const GREP_FILE_CAP: usize = 8192;

/// The whole-disk engine: one lock, one index, started on first use.
pub struct FsearchPool {
    opts: fsearch::Options,
    engine: OnceLock<Option<Engine>>,
    hits: AtomicUsize,
}

impl Default for FsearchPool {
    fn default() -> Self {
        Self::new()
    }
}

impl FsearchPool {
    pub fn new() -> Self {
        let home =
            std::env::var_os("HOME").map_or_else(String::new, |h| h.to_string_lossy().into_owned());
        Self::with_options(fsearch::Options {
            dir: gray_core::paths::gray_home()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("fsearch"),
            home,
            // Whole disk: `in:` scopes handle any directory the index
            // has covered, not just repos under $HOME. Pseudo-
            // filesystems are skipped by the engine's own gate.
            root: Some(PathBuf::from("/")),
            skip: None,
        })
    }

    /// A pool over caller-chosen engine options — tests point `root` at a
    /// fixture and `dir` at a scratch index instead of `gray_home()`.
    pub fn with_options(opts: fsearch::Options) -> Self {
        Self {
            opts,
            engine: OnceLock::new(),
            hits: AtomicUsize::new(0),
        }
    }

    /// Tool calls served by the index rather than a fallback lane.
    pub fn lane_hits(&self) -> usize {
        self.hits.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn status_for_test(&self) -> Option<fsearch::Status> {
        self.engine().map(fsearch::Engine::status)
    }

    /// Ask the engine to persist `index.bin` — the write itself lands on
    /// the apply thread a wake-up later.
    #[cfg(test)]
    pub(crate) fn save_for_test(&self) {
        if let Some(e) = self.engine() {
            e.save();
        }
    }

    /// The engine, started lazily. `Engine::start` only opens the index dir
    /// and spawns its threads — the whole-disk scan happens on its own.
    /// A failed start sticks at `None` and every caller falls back.
    fn engine(&self) -> Option<&Engine> {
        self.engine
            .get_or_init(|| Engine::start(self.opts.clone()).ok())
            .as_ref()
    }

    /// Canonical `dir` if the index can currently serve it: built and the
    /// scope is an indexed directory. `None` = the fallback lane answers —
    /// the same contract `SearchPool::warm_picker` has.
    pub fn warm_picker(&self, dir: &Path) -> Option<PathBuf> {
        let e = self.engine()?;
        if !e.status().ready {
            return None;
        }
        let cdir = dir.canonicalize().ok()?;
        if !cdir.is_dir() || !e.covers(cdir.as_os_str().as_bytes()) {
            return None;
        }
        Some(cdir)
    }

    /// Kick the engine build so the *next* search finds it warm. Cheap:
    /// `Engine::start` returns as soon as its threads spawn.
    pub fn warm_in_background(self: &Arc<Self>, _dir: &Path) {
        let _ = self.engine();
    }

    /// `find` lane: basename glob over the index → `fd --glob`-shaped
    /// relative paths. `None` = caller falls back to fd/walk.
    pub fn indexed_glob(&self, dir: &Path, pattern: &str, limit: usize) -> Option<Vec<String>> {
        let cdir = self.warm_picker(dir)?;
        let e = self.engine()?;
        // Absolute and depth-anchored globs are matched on full paths by fd
        // --full-path; the name regex here only sees basenames — decline.
        if pattern.starts_with('/') || pattern.contains('/') {
            return None;
        }
        // globset rejects what fd rejects — let fd surface its parse error.
        globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .ok()?;
        // fd --glob is a case-sensitive basename match.
        let name_re =
            regex::bytes::Regex::new(&format!("^(?:{})", glob_to_regex(pattern)?)).ok()?;
        let fetch = limit
            .saturating_mul(FETCH_HEADROOM)
            .saturating_add(FETCH_MIN)
            .min(FETCH_MAX);
        let q = Query {
            scope: Some(cdir.as_os_str().as_bytes().to_vec()),
            name_re: Some(name_re),
            limit: fetch,
            ..parse_defaults()
        };
        let hits = e.search(&q).ok()?;
        // A full page may be a truncated ranking — unprovable completeness.
        if hits.len() >= fetch {
            return None;
        }
        let mut chain = IgnoreChain::new(&cdir, Lane::Find);
        let mut out = Vec::with_capacity(hits.len());
        for h in &hits {
            let is_dir = h.kind & 3 == fsearch::walk::KIND_DIR;
            if chain.is_ignored(&h.path, is_dir) {
                continue;
            }
            let Ok(rel) = h.path.strip_prefix(&cdir) else {
                continue;
            };
            let mut s = rel.to_string_lossy().into_owned();
            if is_dir {
                s.push('/');
            }
            out.push(s);
        }
        out.truncate(limit);
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(out)
    }

    /// `grep` lane: content search scoped to the index → `Found` shaped
    /// like `grep_builtin::search`, so `render_matches` cannot tell lanes
    /// apart. `None` = caller falls back to rg/builtin.
    #[allow(clippy::too_many_arguments)]
    pub fn indexed_grep(
        &self,
        dir: &Path,
        pattern: &str,
        glob: Option<&str>,
        ignore_case: bool,
        literal: bool,
        limit: usize,
        context: usize,
        cancel: &CancellationToken,
    ) -> Option<Found> {
        if cancel.is_cancelled() || context > 0 {
            return None;
        }
        let cdir = self.warm_picker(dir)?;
        let e = self.engine()?;
        // `GrepResult.complete` only says the scan finished — it does not
        // say the scope's files are in the trigram index. Coverage is
        // `content_pending`, but that counter is 0 both while the first
        // sync is still queued and once it has drained; `content_docs`
        // separates them. Anything short of "synced at least once, nothing
        // pending" is an unprovable answer — decline.
        let st = e.status();
        if st.content_pending > 0 || st.content_docs == 0 {
            return None;
        }
        // rg -g exclusions (`!*.rs`) and depth-anchored globs aren't
        // expressible on one basename regex — decline like the fff lane.
        if glob.is_some_and(|g| g.starts_with('!') || g.contains('/') || g.contains('{')) {
            return None;
        }
        let mut g = Grep::with_case(
            pattern,
            if literal {
                GrepMode::Literal
            } else {
                GrepMode::Regex
            },
            ignore_case,
        )
        .ok()?; // invalid regex → rg reports the canonical error
        // rg shows every matching line of a file; the index caps at five.
        // `limit` shown hits bounds what one file can contribute anyway.
        g.max_per_file = limit.max(1);
        let mut q = Query {
            scope: Some(cdir.as_os_str().as_bytes().to_vec()),
            limit: GREP_FILE_CAP,
            ..parse_defaults()
        };
        if let Some(glob) = glob {
            globset::GlobBuilder::new(glob)
                .literal_separator(true)
                .build()
                .ok()?;
            q.name_re =
                Some(regex::bytes::Regex::new(&format!("^(?:{})", glob_to_regex(glob)?)).ok()?);
        }
        let (res, _indexed) = e.grep(&q, &g).ok()?;
        // The time budget ran out mid-scan: a partial candidate set is
        // unanswerable — rg runs to completion on the fallback lane.
        if !res.complete {
            return None;
        }
        let mut chain = IgnoreChain::new(&cdir, Lane::Grep);
        let mut hits = Vec::new();
        let mut limit_reached = res.files.len() >= GREP_FILE_CAP;
        'files: for f in &res.files {
            let p = Path::new(std::ffi::OsStr::from_bytes(&f.path));
            if chain.is_ignored(p, false) {
                continue;
            }
            let Ok(rel) = p.strip_prefix(&cdir) else {
                continue;
            };
            // dir.join(rel) keeps the raw (uncanonicalized) root in the
            // path — format_matches relativizes against it.
            let path = dir.join(rel).to_string_lossy().into_owned();
            for (ln, text) in &f.lines {
                if hits.len() >= limit {
                    limit_reached = true;
                    break 'files;
                }
                hits.push((path.clone(), *ln, Some(text.clone()), true));
            }
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        Some(Found {
            hits,
            limit_reached,
        })
    }
}

/// `Query::default()` derives zeroed ranges — `(0, 0)` matches only empty
/// files and instant mtimes. The CLI parser seeds unconstrained fields with
/// the full span; build queries on that same base.
fn parse_defaults() -> Query {
    Query {
        size: (0, u64::MAX),
        mtime: (0, u32::MAX),
        ..Default::default()
    }
}

/// Basename glob → anchored-ready regex body (`*.rs` → `.*\.rs`), or `None`
/// for syntax the translator does not cover — the caller declines and the
/// real tool answers. Handles `*`, `?`, `[...]` classes (incl. `[:posix:]`
/// and `!` negation), `{a,b}` alternates, and `\` escapes.
fn glob_to_regex(pat: &str) -> Option<String> {
    fn inner(
        chars: &mut std::iter::Peekable<std::str::Chars>,
        out: &mut String,
        in_alt: bool,
    ) -> Option<()> {
        while let Some(c) = chars.next() {
            match c {
                '*' => out.push_str(".*"),
                '?' => out.push('.'),
                '[' => {
                    out.push('[');
                    if chars.next_if_eq(&'!').is_some() {
                        out.push('^');
                    }
                    if chars.next_if_eq(&']').is_some() {
                        out.push_str("\\]");
                    }
                    let mut closed = false;
                    while let Some(c) = chars.next() {
                        match c {
                            ']' => {
                                out.push(']');
                                closed = true;
                                break;
                            }
                            '[' if chars.peek() == Some(&':') => {
                                // POSIX class [:name:] — copy through `:]`.
                                out.push('[');
                                for c in chars.by_ref() {
                                    out.push(c);
                                    if c == ']' {
                                        break;
                                    }
                                }
                            }
                            '\\' => {
                                out.push('\\');
                                out.push(chars.next()?);
                            }
                            c => out.push(c),
                        }
                    }
                    if !closed {
                        return None;
                    }
                }
                '{' => {
                    out.push_str("(?:");
                    inner(chars, out, true)?;
                    out.push(')');
                }
                ',' if in_alt => out.push('|'),
                '}' if in_alt => return Some(()),
                '\\' => {
                    out.push_str("\\\\");
                    let n = chars.next()?;
                    if n.is_ascii_punctuation() {
                        out.push('\\');
                    }
                    out.push(n);
                }
                c if c.is_ascii_punctuation() => {
                    out.push('\\');
                    out.push(c);
                }
                c => out.push(c),
            }
        }
        (!in_alt).then_some(())
    }
    let mut out = String::new();
    inner(&mut pat.chars().peekable(), &mut out, false)?;
    Some(out)
}

/// Process-wide backend; index files under `gray_home()/fsearch`.
pub fn global_fs_pool() -> &'static Arc<FsearchPool> {
    static POOL: std::sync::OnceLock<Arc<FsearchPool>> = std::sync::OnceLock::new();
    POOL.get_or_init(|| Arc::new(FsearchPool::new()))
}

#[path = "search_index_fs_tests.rs"]
#[cfg(test)]
mod tests;
