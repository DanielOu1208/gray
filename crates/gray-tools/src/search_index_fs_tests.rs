//! Tests for the fsearch backend (`search_index_fs`) and the ignore-chain
//! filter that makes its answers match the fd/rg lanes. Same fixture shape
//! as `search_index_tests.rs` — assertions compare rendered lanes, and
//! `lane_hits` proves which index answered.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gray_core::agent::ToolContext;
use tempfile::TempDir;

use super::FsearchPool;
use crate::search_cmd::SearchArgs;

fn write(root: &std::path::Path, name: &str, content: &str) {
    let p = root.join(name);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(p, content).unwrap();
}

/// Same fixture as the fff tests: nested files, a dotfile dir, a .gitignore
/// hiding `ignored.txt` — the case the index alone cannot answer and the
/// IgnoreChain must drop.
fn tree() -> TempDir {
    let dir = TempDir::new().unwrap();
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status();
    assert!(ok.is_ok_and(|s| s.success()), "git init failed");
    let r = dir.path();
    write(
        r,
        "a.txt",
        "alpha needle one\nnothing here\nbeta needle two\n",
    );
    write(r, "sub/c.txt", "deep needle four\n");
    write(r, "src/lib.rs", "pub fn needle_fn() {}\n");
    write(r, "awkward.txt", "a:b:c needle\nUPPER needle lower\n");
    write(r, ".hidden/h.txt", "hidden needle\n");
    write(r, ".gitignore", "ignored.txt\n");
    write(r, "ignored.txt", "needle ignored\n");
    dir
}

fn sorted_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.sort_unstable();
    lines
}

fn sorted_owned(text: &str) -> Vec<String> {
    sorted_lines(text).into_iter().map(str::to_owned).collect()
}

/// A pool whose engine indexes only the fixture tree (index dir in its own
/// tempdir — `index.bin` inside the fixture would change what find lists).
fn pool_for(dir: &TempDir) -> (FsearchPool, TempDir) {
    let idx = TempDir::new().unwrap();
    let pool = FsearchPool::with_options(fsearch::Options {
        dir: idx.path().join("index"),
        home: dir.path().to_string_lossy().into_owned(),
        root: Some(dir.path().to_path_buf()),
        skip: None,
    });
    (pool, idx)
}

fn ctx_for(dir: &std::path::Path) -> ToolContext {
    ToolContext {
        cwd: dir.to_path_buf(),
        ..Default::default()
    }
}

async fn wait_warm(pool: &FsearchPool, dir: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while pool.warm_picker(dir).is_none() {
        assert!(Instant::now() < deadline, "index never became ready");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The trigram lane reports `complete=false` until content syncs; grep tests
/// wait for it so `lane_hits` asserts the index, not the fallback.
async fn wait_content(pool: &FsearchPool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let done = pool
            .status_for_test()
            .is_some_and(|s| s.ready && s.content_pending == 0);
        if done {
            return;
        }
        assert!(Instant::now() < deadline, "content index never settled");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ---------------------------------------------------------------------------
// find lane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn find_serves_from_index_and_drops_gitignored() {
    let dir = tree();
    let (pool, _idx) = pool_for(&dir);
    let pool = Arc::new(pool);
    let ctx = ctx_for(dir.path());
    let args = || SearchArgs {
        pattern: "*.txt".to_string(),
        limit: Some(100),
        ..Default::default()
    };

    // Cold: the tool lane answers while the engine builds.
    let first = crate::search_cmd::find_with_fsearch(&args(), &ctx, pool.clone()).await;
    assert!(first.contains("sub/c.txt"), "{first}");

    wait_warm(&pool, dir.path()).await;
    let before = pool.lane_hits();
    let second = crate::search_cmd::find_with_fsearch(&args(), &ctx, pool.clone()).await;
    assert_eq!(
        sorted_lines(&second),
        sorted_lines(&first),
        "lanes must agree"
    );
    assert!(pool.lane_hits() > before, "warm find must hit the index");
    // The point of the ignore chain: the index scans ignored.txt, fd does not.
    assert!(
        second.contains(".hidden/h.txt"),
        "dotfiles must show: {second}"
    );
    assert!(
        !second.contains("ignored.txt"),
        "gitignored file leaked: {second}"
    );
}

#[tokio::test]
async fn find_returns_matching_dirs_with_slash() {
    let dir = tree();
    let (pool, _idx) = pool_for(&dir);
    let pool = Arc::new(pool);
    let ctx = ctx_for(dir.path());
    wait_warm(&pool, dir.path()).await;
    let text = crate::search_cmd::find_with_fsearch(
        &SearchArgs {
            pattern: "sub".to_string(),
            limit: Some(100),
            ..Default::default()
        },
        &ctx,
        pool.clone(),
    )
    .await;
    assert!(text.contains("sub/"), "dir hit must carry a slash: {text}");
}

#[tokio::test]
async fn find_declines_slash_patterns() {
    let dir = tree();
    let (pool, _idx) = pool_for(&dir);
    let pool = Arc::new(pool);
    let ctx = ctx_for(dir.path());
    wait_warm(&pool, dir.path()).await;
    let before = pool.lane_hits();
    let text = crate::search_cmd::find_with_fsearch(
        &SearchArgs {
            pattern: "sub/*.txt".to_string(),
            limit: Some(100),
            ..Default::default()
        },
        &ctx,
        pool.clone(),
    )
    .await;
    assert!(text.contains("sub/c.txt"), "{text}");
    assert_eq!(pool.lane_hits(), before, "anchored glob must decline to fd");
}

// ---------------------------------------------------------------------------
// grep lane
// ---------------------------------------------------------------------------

#[tokio::test]
async fn grep_served_by_index_with_gitignore_filter() {
    let dir = tree();
    let (pool, _idx) = pool_for(&dir);
    let pool = Arc::new(pool);
    let ctx = ctx_for(dir.path());
    let args = || SearchArgs {
        pattern: "needle".to_string(),
        limit: Some(100),
        ..Default::default()
    };
    let cold = crate::search_cmd::grep_with_fsearch(&args(), &ctx, pool.clone()).await;
    wait_warm(&pool, dir.path()).await;
    wait_content(&pool).await;
    let before = pool.lane_hits();
    let text = crate::search_cmd::grep_with_fsearch(&args(), &ctx, pool.clone()).await;
    assert_eq!(sorted_lines(&text), sorted_lines(&cold), "lanes must agree");
    assert!(pool.lane_hits() > before, "warm grep must hit the index");
    assert!(text.contains(".hidden/h.txt:1: hidden needle"), "{text}");
    assert!(
        !text.contains("ignored.txt"),
        "gitignored file leaked: {text}"
    );
}

#[tokio::test]
async fn grep_glob_and_case() {
    let dir = tree();
    let (pool, _idx) = pool_for(&dir);
    let pool = Arc::new(pool);
    let ctx = ctx_for(dir.path());
    wait_warm(&pool, dir.path()).await;
    wait_content(&pool).await;

    // -g filter: basename glob, only .rs matches.
    let before = pool.lane_hits();
    let text = crate::search_cmd::grep_with_fsearch(
        &SearchArgs {
            pattern: "needle".to_string(),
            glob: Some("*.rs".to_string()),
            limit: Some(100),
            ..Default::default()
        },
        &ctx,
        pool.clone(),
    )
    .await;
    assert!(text.contains("src/lib.rs:1:"), "{text}");
    assert!(!text.contains("a.txt"), "{text}");
    assert!(pool.lane_hits() > before, "glob grep must hit the index");

    // Default is case-sensitive: "NEEDLE" finds nothing, like rg.
    let text = crate::search_cmd::grep_with_fsearch(
        &SearchArgs {
            pattern: "NEEDLE".to_string(),
            limit: Some(100),
            ..Default::default()
        },
        &ctx,
        pool.clone(),
    )
    .await;
    assert!(!text.contains("needle lower"), "smart-case leak: {text}");

    // ignoreCase serves too — the flag is explicit, not smart-case.
    let text = crate::search_cmd::grep_with_fsearch(
        &SearchArgs {
            pattern: "upper".to_string(),
            ignore_case: true,
            limit: Some(100),
            ..Default::default()
        },
        &ctx,
        pool.clone(),
    )
    .await;
    assert!(text.contains("UPPER needle lower"), "{text}");
}

#[tokio::test]
async fn grep_declines_context_and_outside_root() {
    let dir = tree();
    let (pool, _idx) = pool_for(&dir);
    let pool = Arc::new(pool);
    let ctx = ctx_for(dir.path());
    wait_warm(&pool, dir.path()).await;

    // Context lines aren't in the index answer — decline, rg serves them.
    let before = pool.lane_hits();
    let text = crate::search_cmd::grep_with_fsearch(
        &SearchArgs {
            pattern: "beta needle".to_string(),
            limit: Some(100),
            context: Some(1),
            ..Default::default()
        },
        &ctx,
        pool.clone(),
    )
    .await;
    assert!(text.contains("a.txt-2- nothing here"), "{text}");
    assert_eq!(pool.lane_hits(), before, "context grep must decline");

    // A scope the index never covered is not an empty answer — decline.
    let outside = TempDir::new().unwrap();
    write(outside.path(), "x.txt", "needle here\n");
    let before = pool.lane_hits();
    let text = crate::search_cmd::grep_with_fsearch(
        &SearchArgs {
            pattern: "needle".to_string(),
            limit: Some(100),
            path: Some(outside.path().to_path_buf()),
            ..Default::default()
        },
        &ctx_for(outside.path()),
        pool.clone(),
    )
    .await;
    assert!(text.contains("needle here"), "{text}");
    assert_eq!(pool.lane_hits(), before, "uncovered scope must decline");
}

// ---------------------------------------------------------------------------
// End-to-end: the dispatch itself and the index across restarts
// ---------------------------------------------------------------------------

/// Every backend — fff, fsearch, and no index at all — answers the same
/// `find`/`grep` calls with byte-identical output. That is the whole
/// contract: which index ran is invisible.
#[tokio::test]
async fn e2e_all_backends_render_identically() {
    let dir = tree();
    let ctx = ctx_for(dir.path());
    let find_args = || SearchArgs {
        pattern: "*.txt".to_string(),
        limit: Some(100),
        ..Default::default()
    };
    let grep_args = || SearchArgs {
        pattern: "needle".to_string(),
        limit: Some(100),
        ..Default::default()
    };

    let fff = Arc::new(crate::search_index::SearchPool::new(
        dir.path().join("fff-frecency"),
    ));
    let (fs, _idx) = pool_for(&dir);
    let fs = Arc::new(fs);
    let backends = [
        ("fff", crate::search_cmd::IndexBackend::Fff(fff.clone())),
        ("fsearch", crate::search_cmd::IndexBackend::Fs(fs.clone())),
        ("off", crate::search_cmd::IndexBackend::Off),
    ];

    // Cold: every backend runs the tool lanes — identical by construction,
    // and each call kicked its index warm.
    let mut cold_find = Vec::new();
    let mut cold_grep = Vec::new();
    for (_name, b) in &backends {
        cold_find.push(sorted_owned(
            &crate::search_cmd::find_on(&find_args(), &ctx, b).await,
        ));
        cold_grep.push(sorted_owned(
            &crate::search_cmd::grep_on(&grep_args(), &ctx, b).await,
        ));
    }
    assert!(
        cold_find.windows(2).all(|w| w[0] == w[1]),
        "cold find diverged: {cold_find:?}"
    );
    assert!(
        cold_grep.windows(2).all(|w| w[0] == w[1]),
        "cold grep diverged: {cold_grep:?}"
    );

    // Warm both indexes; content sync too — the trigram lane declines until then.
    let deadline = Instant::now() + Duration::from_secs(15);
    while fff.warm_picker(dir.path()).is_none() {
        assert!(Instant::now() < deadline, "fff index never warmed");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    wait_warm(&fs, dir.path()).await;
    wait_content(&fs).await;

    // Warm: different indexes, one answer — and it equals the fallback's.
    for (name, b) in &backends {
        let f = crate::search_cmd::find_on(&find_args(), &ctx, b).await;
        assert_eq!(sorted_owned(&f), cold_find[0], "find diverged on {name}");
        let g = crate::search_cmd::grep_on(&grep_args(), &ctx, b).await;
        assert_eq!(sorted_owned(&g), cold_grep[0], "grep diverged on {name}");
    }
    assert!(fff.lane_hits() > 0, "fff never served");
    assert!(fs.lane_hits() > 0, "fsearch never served");
}

/// The index outlives the process that built it: save, drop, start a new
/// pool on the same files and the scope is warm — the launch-time path a
/// second gray process takes against `~/.gray/fsearch`.
#[tokio::test]
async fn e2e_index_survives_restart() {
    let dir = tree();
    let idx = TempDir::new().unwrap();
    let opts = || fsearch::Options {
        dir: idx.path().join("index"),
        home: dir.path().to_string_lossy().into_owned(),
        root: Some(dir.path().to_path_buf()),
        skip: None,
    };

    let first = {
        let pool = pool_for_opts(opts());
        wait_warm(&pool, dir.path()).await;
        let hits = pool
            .indexed_glob(dir.path(), "*.txt", 100)
            .expect("index should serve");
        assert!(hits.iter().any(|h| h == "a.txt"), "{hits:?}");
        pool.save_for_test();
        // `save()` just flags the apply thread; the file lands a beat later.
        let deadline = Instant::now() + Duration::from_secs(15);
        while !opts().dir.join("index.bin").exists() {
            assert!(Instant::now() < deadline, "index.bin never written");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        hits
    };

    // A new engine on the same dir loads the saved index (follower mode in
    // one process — the mmap path a fresh process takes) and answers the
    // identical query with the identical hits.
    let pool2 = pool_for_opts(opts());
    wait_warm(&pool2, dir.path()).await;
    let hits2 = pool2
        .indexed_glob(dir.path(), "*.txt", 100)
        .expect("reloaded index should serve");
    assert_eq!(first, hits2);
}

fn pool_for_opts(opts: fsearch::Options) -> FsearchPool {
    FsearchPool::with_options(opts)
}

/// `GRAY_SEARCH_INDEX` parsing — the only user-visible switch.
#[test]
fn backend_kind_from_env() {
    use crate::search_cmd::{BackendKind, backend_kind};
    assert_eq!(backend_kind(None), BackendKind::Fs);
    assert_eq!(backend_kind(Some("fsearch")), BackendKind::Fs);
    assert_eq!(backend_kind(Some("fff")), BackendKind::Fff);
    assert_eq!(backend_kind(Some("off")), BackendKind::Off);
    assert_eq!(backend_kind(Some("none")), BackendKind::Off);
    // A typo disables nothing — the default still indexes.
    assert_eq!(backend_kind(Some("offf")), BackendKind::Fs);
}
