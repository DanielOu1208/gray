use super::*;
use std::os::unix::process::ExitStatusExt;

fn code(n: i32) -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(n << 8)
}

fn sig(s: i32) -> std::process::ExitStatus {
    std::process::ExitStatus::from_raw(s)
}

#[test]
fn plain_exit_code_is_honest() {
    let r = exit_report(code(3), "sh -c 'exit 3'", None);
    assert_eq!(r.effective, 3);
    assert_eq!(r.label, "exit 3");
    assert!(r.note.is_none());
}

#[test]
fn sigkill_maps_to_137() {
    let r = exit_report(sig(9), "sh -c 'kill -9 $$'", None);
    assert_eq!(r.effective, 137);
    assert!(r.label.contains("SIGKILL"), "{}", r.label);
    assert!(r.note.is_none());
}

#[test]
fn sigterm_maps_to_143() {
    let r = exit_report(sig(15), "sh -c 'kill -15 $$'", None);
    assert_eq!(r.effective, 143);
    assert!(r.label.contains("SIGTERM"), "{}", r.label);
    assert!(r.note.is_none());
}

#[test]
fn diff_success_is_plain() {
    let r = exit_report(code(0), "diff a a", None);
    assert!(r.note.is_none());
}

#[test]
fn masked_pipeline_note_is_posix_honest() {
    let r = exit_report(code(0), "false | tail -1", Some(&[1, 0]));
    let note = r.note.as_deref().unwrap_or("");
    assert!(!note.contains("pipefail"), "{note}");
    assert!(note.contains("tail"), "{note}");
    assert!(note.contains("false"), "{note}");

    assert!(
        !note.contains('|'),
        "header must not echo the pipeline: {note}"
    );
}

#[test]
fn pipefail_mention_does_not_suppress_masked_note() {
    let r = exit_report(code(0), "echo pipefail | tail -1", Some(&[1, 0]));
    assert!(r.note.is_some());
}

#[test]
fn failing_stage_behind_a_text_filter_is_flagged() {
    for cmd in [
        "pytest -q | head -40",
        "cargo test | tail -5",
        "python3 -c 'raise SystemExit(3)' | head -5",
        "make check | cat",
        "pytest -q | awk '{print $1}'",
        "cargo test 2>&1 | sed -n '1,20p'",
        "make test | cut -d: -f1",
        "npm test | tr a-z A-Z",
        "go test ./... | column -t",
    ] {
        let r = exit_report(code(0), cmd, Some(&[1, 0]));
        let note = r.note.as_deref().unwrap_or("");
        assert!(note.contains("masks"), "{cmd} -> {note}");
        assert!(
            note.contains("rerun"),
            "the note must say how to recover the status: {cmd} -> {note}"
        );

        assert!(!note.contains('|'), "{cmd} -> {note}");
    }
}

#[test]
fn multi_stage_filter_names_stages_generically() {
    let r = exit_report(code(0), "a | b | head -1", Some(&[0, 1, 0]));
    let note = r.note.as_deref().unwrap_or("");
    assert!(note.contains("earlier stages"), "{note}");
}

#[test]
fn text_filter_last_with_no_masking_risk_stays_silent() {
    let r = exit_report(code(0), "cargo test | grep PASS", Some(&[1, 0]));
    assert!(
        r.note.as_deref().unwrap_or("").contains("cargo"),
        "{:?}",
        r.note
    );

    let r = exit_report(code(0), "cat f", None);
    assert!(r.note.is_none(), "{:?}", r.note);

    let r = exit_report(code(0), "head -5 f", None);
    assert!(r.note.is_none(), "{:?}", r.note);
}

#[test]
fn all_green_stages_stay_quiet() {
    for cmd in [
        "false | tail -1",
        "cargo test | grep PASS",
        "a | b | head -1",
    ] {
        let segs = crate::shell::split::split_pipeline(cmd).len();
        let stages = vec![0i32; segs];
        let r = exit_report(code(0), cmd, Some(&stages));
        assert!(r.note.is_none(), "{cmd} -> {:?}", r.note);
    }
}

#[test]
fn unknown_stage_status_stays_quiet() {
    let r = exit_report(code(0), "false | tail -1", None);
    assert!(r.note.is_none(), "{:?}", r.note);
}

#[test]
fn or_or_is_not_a_pipeline() {
    let r = exit_report(code(0), "false || true", None);
    assert!(r.note.is_none());
}

#[test]
fn quoted_pipe_is_not_a_pipeline() {
    let r = exit_report(code(0), "echo \"a|b\"", None);
    assert!(r.note.is_none());
}

#[test]
fn ls_missing_file_stays_an_error() {
    let r = exit_report(code(2), "ls /nonexistent", None);
    assert!(r.note.is_none());
}

#[test]
fn sigint_maps_to_130() {
    let r = exit_report(sig(2), "sh -c 'kill -INT $$'", None);
    assert_eq!(r.effective, 130);
    assert!(r.label.contains("SIGINT"), "{}", r.label);
}

#[test]
fn unknown_signal_stays_numeric() {
    let r = exit_report(sig(10), "sh -c 'kill -USR1 $$'", None);
    assert_eq!(r.effective, 138);
    assert!(r.label.contains("SIG10"), "{}", r.label);
}
