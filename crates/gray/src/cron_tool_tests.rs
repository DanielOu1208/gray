use super::*;
use serde_json::json;

fn tool() -> (tempfile::TempDir, ScheduleTool) {
    let dir = tempfile::tempdir().unwrap();
    let tool = ScheduleTool {
        home: dir.path().to_path_buf(),
    };
    (dir, tool)
}

fn job(tool: &ScheduleTool, name: &str) -> CronJob {
    CronStore::open(tool.home.join("cron"))
        .unwrap()
        .get(name)
        .unwrap()
        .unwrap()
}

#[test]
fn create_waits_for_approval_and_binds_the_session() {
    let (_d, t) = tool();
    let out = t
        .apply(
            Some("s1"),
            &json!({"name": "bench", "when": "daily 9am", "prompt": "check maid", "times": 3}),
        )
        .unwrap();
    assert!(out.contains("approve it in /cron"), "{out}");
    assert!(
        out.contains("bench · cron 0 9 * * * · awaiting approval · 0/3 runs"),
        "{out}"
    );
    let j = job(&t, "bench");
    assert_eq!(
        (j.state, j.next_run_at, j.repeat.times),
        (JobState::AwaitingApproval, None, Some(3))
    );
    assert_eq!(j.origin.unwrap().chat, "s1");
    assert!(matches!(j.deliver, Deliver::Origin));
}

#[test]
fn editing_an_approved_job_sends_it_back_for_approval() {
    let (_d, t) = tool();
    t.apply(
        Some("s1"),
        &json!({"name": "a", "when": "every 1h", "prompt": "x"}),
    )
    .unwrap();
    let store = CronStore::open(t.home.join("cron")).unwrap();
    store.approve("a").unwrap();
    t.apply(
        Some("s1"),
        &json!({"name": "a", "when": "every 2h", "prompt": "y"}),
    )
    .unwrap();
    let j = job(&t, "a");
    assert_eq!(
        (j.state, j.prompt.as_str()),
        (JobState::AwaitingApproval, "y")
    );
    assert_eq!(j.schedule, Schedule::Interval { secs: 7200 });
}

#[test]
fn list_delete_and_errors() {
    let (_d, t) = tool();
    assert_eq!(t.apply(None, &json!({})).unwrap(), "no jobs");
    assert!(
        t.apply(None, &json!({"name": "a", "when": "daily 9am"}))
            .unwrap_err()
            .to_string()
            .contains("prompt")
    );
    assert!(t.apply(None, &json!({"name": "a"})).is_err());
    assert!(
        t.apply(
            None,
            &json!({"name": "a", "when": "blursday", "prompt": "x"})
        )
        .is_err()
    );
    t.apply(None, &json!({"name": "a", "when": "in 10m", "prompt": "x"}))
        .unwrap();
    let out = t
        .apply(None, &json!({"name": "a", "when": "delete"}))
        .unwrap();
    assert_eq!(out, "deleted a\n\nno jobs");
    assert!(
        t.apply(None, &json!({"name": "a", "when": "delete"}))
            .is_err()
    );
}

#[test]
fn visible_spells_out_what_a_reader_cannot_see() {
    assert_eq!(visible("plain text"), "plain text");
    assert_eq!(visible("a\u{200b}b"), "a\u{27e8}U+200B\u{27e9}b");
    assert_eq!(visible("x\u{202e}y"), "x\u{27e8}U+202E\u{27e9}y");
    assert_eq!(visible("\u{e0041}"), "\u{27e8}U+E0041\u{27e9}");
    assert_eq!(visible("one\ntwo"), "one\u{23ce} two");
    assert_eq!(visible("caf\u{e9} \u{1f600}"), "caf\u{e9} \u{1f600}");
}
