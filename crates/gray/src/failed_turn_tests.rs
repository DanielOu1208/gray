use super::*;
use crate::session_store::SessionMeta;
use tempfile::tempdir;

async fn make_session(store: &JsonlSessionStore, id: &str, cwd: &Path) -> SessionId {
    let id = SessionId::new(id);
    store
        .create(SessionMeta::new(id.clone(), 1, cwd, "t"))
        .await
        .unwrap();
    store
        .append(&id, &Message::user("work so far"))
        .await
        .unwrap();
    id
}

fn checkpoint_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(FILE_PREFIX)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn record_failure_writes_checkpoint() {
    let dir = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let id = SessionId::new("sess-a");
    record_failure_in(
        dir.path(),
        &id,
        cwd.path(),
        "do the task",
        "hash1",
        "rate_limited",
        3,
    );
    let files = checkpoint_files(dir.path());
    assert_eq!(files.len(), 1);
    let cp: FailedTurn =
        serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap();
    assert_eq!(cp.session, "sess-a");
    assert_eq!(cp.cwd, canonical(cwd.path()));
    assert_eq!(cp.prompt_hash, "hash1");
    assert_eq!(cp.prompt, "do the task");
    assert_eq!(cp.code, "rate_limited");
    assert_eq!(cp.exit, 3);
    assert!(now_millis().saturating_sub(cp.failed_at_ms) < 60_000);
}

#[tokio::test]
async fn fresh_checkpoint_resumes_same_session() {
    let dir = tempdir().unwrap();
    let sessions = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let store = JsonlSessionStore::new(sessions.path());
    let id = make_session(&store, "sess-a", cwd.path()).await;
    let hash = prompt_hash("do the task");
    record_failure_in(
        dir.path(),
        &id,
        cwd.path(),
        "do the task",
        &hash,
        "rate_limited",
        3,
    );
    let got = resume_session_in(dir.path(), &store, cwd.path(), &hash).await;
    assert_eq!(got.as_ref().map(|s| s.as_str()), Some("sess-a"));
}

#[tokio::test]
async fn expired_checkpoint_starts_fresh() {
    let dir = tempdir().unwrap();
    let sessions = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let store = JsonlSessionStore::new(sessions.path());
    let id = make_session(&store, "sess-a", cwd.path()).await;
    let hash = prompt_hash("do the task");
    let stale = FailedTurn {
        session: id.as_str().to_string(),
        cwd: canonical(cwd.path()),
        prompt_hash: hash.clone(),
        prompt: "do the task".to_string(),
        failed_at_ms: now_millis().saturating_sub(FRESH_MS + 60_000),
        exit: 3,
        code: "rate_limited".to_string(),
    };
    let path = checkpoint_path(dir.path(), cwd.path(), &hash);
    std::fs::write(&path, serde_json::to_string(&stale).unwrap()).unwrap();
    let got = resume_session_in(dir.path(), &store, cwd.path(), &hash).await;
    assert!(got.is_none());
    assert!(!path.exists());
}

#[tokio::test]
async fn prompt_mismatch_starts_fresh() {
    let dir = tempdir().unwrap();
    let sessions = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let store = JsonlSessionStore::new(sessions.path());
    let id = make_session(&store, "sess-a", cwd.path()).await;
    let hash_a = prompt_hash("task A");
    let hash_b = prompt_hash("task B");
    record_failure_in(
        dir.path(),
        &id,
        cwd.path(),
        "task A",
        &hash_a,
        "turn_failed",
        1,
    );
    let got = resume_session_in(dir.path(), &store, cwd.path(), &hash_b).await;
    assert!(got.is_none());
}

#[tokio::test]
async fn checkpoint_with_mismatched_inner_hash_starts_fresh() {
    let dir = tempdir().unwrap();
    let sessions = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let store = JsonlSessionStore::new(sessions.path());
    let id = make_session(&store, "sess-a", cwd.path()).await;
    let hash_b = prompt_hash("task B");
    let cp = FailedTurn {
        session: id.as_str().to_string(),
        cwd: canonical(cwd.path()),
        prompt_hash: prompt_hash("task A"),
        prompt: "task A".to_string(),
        failed_at_ms: now_millis(),
        exit: 1,
        code: "turn_failed".to_string(),
    };
    let path = checkpoint_path(dir.path(), cwd.path(), &hash_b);
    std::fs::write(&path, serde_json::to_string(&cp).unwrap()).unwrap();
    let got = resume_session_in(dir.path(), &store, cwd.path(), &hash_b).await;
    assert!(got.is_none());
}

#[tokio::test]
async fn checkpoint_for_missing_session_starts_fresh() {
    let dir = tempdir().unwrap();
    let sessions = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let store = JsonlSessionStore::new(sessions.path());
    let hash = prompt_hash("do the task");
    record_failure_in(
        dir.path(),
        &SessionId::new("gone-session"),
        cwd.path(),
        "do the task",
        &hash,
        "turn_failed",
        1,
    );
    let got = resume_session_in(dir.path(), &store, cwd.path(), &hash).await;
    assert!(got.is_none());
}

#[tokio::test]
async fn env_opt_out_disables_resume() {
    let dir = tempdir().unwrap();
    let sessions = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let store = JsonlSessionStore::new(sessions.path());
    let id = make_session(&store, "sess-a", cwd.path()).await;
    let hash = prompt_hash("do the task");
    record_failure_in(
        dir.path(),
        &id,
        cwd.path(),
        "do the task",
        &hash,
        "turn_failed",
        1,
    );
    unsafe { std::env::set_var("GRAY_NO_RESUME", "1") };
    assert!(disabled());
    let got = resume_session(&store, cwd.path(), &hash).await;
    unsafe { std::env::remove_var("GRAY_NO_RESUME") };
    assert!(got.is_none());
    assert!(!disabled());
}

#[test]
fn clear_removes_only_matching_checkpoint() {
    let dir = tempdir().unwrap();
    let cwd = tempdir().unwrap();
    let hash = prompt_hash("do the task");
    let id_a = SessionId::new("sess-a");
    let id_b = SessionId::new("sess-b");
    record_failure_in(
        dir.path(),
        &id_a,
        cwd.path(),
        "do the task",
        &hash,
        "turn_failed",
        1,
    );
    clear_checkpoint_in(dir.path(), &id_b, cwd.path(), &hash);
    assert_eq!(checkpoint_files(dir.path()).len(), 1);
    clear_checkpoint_in(dir.path(), &id_a, cwd.path(), &hash);
    assert!(checkpoint_files(dir.path()).is_empty());
}

#[test]
fn prompt_keying_is_stable_and_distinct() {
    let cwd = tempdir().unwrap();
    let a = prompt_hash("task A");
    let b = prompt_hash("task B");
    assert_ne!(a, b);
    assert_eq!(a, prompt_hash("task A"));
    assert_ne!(
        checkpoint_path(Path::new("/x"), cwd.path(), &a),
        checkpoint_path(Path::new("/x"), cwd.path(), &b)
    );
}
