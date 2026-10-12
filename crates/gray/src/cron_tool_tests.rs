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
fn create_goes_live_and_binds_the_session() {
    let (_d, t) = tool();
    let out = t
        .apply(
            Some("s1"),
            &json!({"name": "bench", "when": "daily 9am", "prompt": "check maid", "times": 3}),
        )
        .unwrap();
    assert!(
        out.contains("bench · cron 0 9 * * * · active · next"),
        "{out}"
    );
    let j = job(&t, "bench");
    assert_eq!(j.state, JobState::Active);
    assert!(j.next_run_at.is_some());
    assert_eq!(j.repeat.times, Some(3));
    assert_eq!(j.origin.unwrap().chat, "s1");
    assert!(matches!(j.deliver, Deliver::Origin));
}

#[test]
fn editing_rearms_the_job_at_once() {
    let (_d, t) = tool();
    t.apply(
        Some("s1"),
        &json!({"name": "a", "when": "every 1h", "prompt": "x"}),
    )
    .unwrap();
    t.apply(
        Some("s1"),
        &json!({"name": "a", "when": "every 2h", "prompt": "y"}),
    )
    .unwrap();
    let j = job(&t, "a");
    assert_eq!((j.state, j.prompt.as_str()), (JobState::Active, "y"));
    assert!(j.next_run_at.is_some());
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
