use super::*;
use serde_json::json;
use std::collections::HashSet;

fn whole_case() -> Vec<(String, String, serde_json::Value)> {
    vec![
        ("a".into(), "read".into(), json!({"path": "x.rs"})),
        ("b".into(), "grep".into(), json!({"pattern": "y"})),
        ("c".into(), "bash".into(), json!({"command": "ls"})),
        ("d".into(), "read".into(), json!({"path": "z.rs"})),
        ("e".into(), "mystery".into(), json!({})),
        ("f".into(), "read".into(), json!("oops")),
    ]
}
fn known() -> HashSet<String> {
    ["read", "grep", "bash"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}
#[test]
fn planner_groups_only_contiguous_batchable_runs() {
    assert_eq!(
        plan_segments(&whole_case(), &known()),
        vec![
            Segment::Parallel(vec![0, 1, 2, 3]),
            Segment::Single(4),
            Segment::Single(5),
        ]
    );
}
#[test]
fn bash_screen_demotes_mutators() {
    for cmd in [
        "rm -rf /tmp/x",
        "sed -i 's/a/b/' file",
        "cargo install ripgrep",
        "cargo build",
        "echo hi > file",
        "cat f >> g",
        "cat f | tee g",
        "sudo ls",
        "mv a b",
        "cp -r a b",
        "mkdir -p x",
        "touch x",
        "chmod +x run.sh",
        "git commit -m x",
        "git push",
        "npm i lodash",
        "npm install",
        "npm test",
        "kill 1234",
        "find . -delete",
    ] {
        assert!(!bash_is_batchable(cmd), "mutator must demote: {cmd}");
    }
}
#[test]
fn planner_demotes_bash_without_parsable_command() {
    let known: HashSet<String> = ["read", "bash"].iter().map(|s| s.to_string()).collect();
    let u = vec![
        ("a".into(), "read".into(), json!({"path": "a"})),
        ("b".into(), "bash".into(), json!({})),
        ("c".into(), "bash".into(), json!({"command": 42})),
        ("d".into(), "read".into(), json!({"path": "d"})),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![
            Segment::Single(0),
            Segment::Single(1),
            Segment::Single(2),
            Segment::Single(3),
        ]
    );
}
#[test]
fn planner_batches_disjoint_mutating_bash_calls() {
    let known: HashSet<String> = ["bash"].iter().map(|s| s.to_string()).collect();

    let u = vec![
        (
            "a".into(),
            "bash".into(),
            json!({"command": "rm build/a.o"}),
        ),
        (
            "b".into(),
            "bash".into(),
            json!({"command": "rm build/b.o"}),
        ),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![Segment::Parallel(vec![0, 1])]
    );

    let u = vec![
        (
            "a".into(),
            "bash".into(),
            json!({"command": "rm build/a.o"}),
        ),
        (
            "b".into(),
            "bash".into(),
            json!({"command": "mv build/a.o build/c.o"}),
        ),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![Segment::Single(0), Segment::Single(1)]
    );
}

#[test]
fn planner_batches_disjoint_redirect_writes() {
    let known: HashSet<String> = ["bash"].iter().map(|s| s.to_string()).collect();
    let u = vec![
        (
            "a".into(),
            "bash".into(),
            json!({"command": "echo one > out/a.txt"}),
        ),
        (
            "b".into(),
            "bash".into(),
            json!({"command": "echo two > out/b.txt"}),
        ),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![Segment::Parallel(vec![0, 1])]
    );

    let u = vec![
        (
            "a".into(),
            "bash".into(),
            json!({"command": "echo one > out/a.txt"}),
        ),
        (
            "b".into(),
            "bash".into(),
            json!({"command": "cat out/a.txt"}),
        ),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![Segment::Single(0), Segment::Single(1)]
    );
}

#[test]
fn unenumerable_writes_stay_barriers() {
    let known: HashSet<String> = ["bash", "read"].iter().map(|s| s.to_string()).collect();

    let u = vec![
        ("a".into(), "bash".into(), json!({"command": "cargo test"})),
        ("b".into(), "bash".into(), json!({"command": "cargo build"})),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![Segment::Single(0), Segment::Single(1)]
    );

    let u = vec![
        ("a".into(), "bash".into(), json!({"command": "rm $(ls)"})),
        ("b".into(), "read".into(), json!({"path": "a.txt"})),
    ];
    assert_eq!(
        plan_segments(&u, &known),
        vec![Segment::Single(0), Segment::Single(1)]
    );
}

#[test]
fn bash_touch_enumerates_operands_and_redirects() {
    assert_eq!(
        bash_touch("rm a b"),
        Some(Touch::Writes {
            reads: vec![],
            writes: vec!["a".into(), "b".into()]
        })
    );
    assert_eq!(
        bash_touch("echo hi > a.txt"),
        Some(Touch::Writes {
            reads: vec![],
            writes: vec!["a.txt".into()]
        })
    );
    assert_eq!(
        bash_touch("cat f > g"),
        Some(Touch::Writes {
            reads: vec!["f".into()],
            writes: vec!["g".into()]
        })
    );
    assert_eq!(
        bash_touch("sed -i s/a/b/ f"),
        Some(Touch::Writes {
            reads: vec![],
            writes: vec!["s/a/b".into(), "f".into()]
        })
    );
    assert_eq!(bash_touch("ls"), Some(Touch::Reads(vec![])));
    assert_eq!(
        bash_touch("grep -rn foo ."),
        Some(Touch::Reads(vec!["foo".into(), ".".into()]))
    );
    assert_eq!(bash_touch("git status"), Some(Touch::ReadsUnknown));
    assert_eq!(bash_touch("cargo build"), None);
}

#[test]
fn kill_switch_parses() {
    let _g = ENV_LOCK.lock().unwrap();
    let prev = std::env::var("GRAY_PARALLEL_READS").ok();
    for (val, want) in [
        ("0", false),
        ("false", false),
        ("no", false),
        ("off", false),
        ("FALSE", false),
        ("1", true),
        ("yes", true),
    ] {
        unsafe { std::env::set_var("GRAY_PARALLEL_READS", val) };
        assert_eq!(parallel_enabled(), want, "{val}");
    }
    unsafe { std::env::remove_var("GRAY_PARALLEL_READS") };
    assert!(parallel_enabled(), "default is on");
    match prev {
        Some(v) => unsafe { std::env::set_var("GRAY_PARALLEL_READS", v) },
        None => unsafe { std::env::remove_var("GRAY_PARALLEL_READS") },
    }
}
#[tokio::test]
async fn join_runs_concurrently_and_returns_input_order() {
    use crate::agent::ToolOutput;
    use futures::future::BoxFuture;
    let mk = |i: usize| {
        (
            i,
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                ToolOutput::ok(format!("out{i}"))
            }) as BoxFuture<'static, ToolOutput>,
        )
    };
    let futs = vec![mk(0), mk(1), mk(2), mk(3)];
    let cancel = tokio_util::sync::CancellationToken::new();
    let t0 = std::time::Instant::now();
    let got = join_ordered(futs, &cancel).await;
    let dt = t0.elapsed();
    assert_eq!(
        got.into_iter()
            .map(|(i, o)| (i, o.unwrap().content))
            .collect::<Vec<_>>(),
        vec![
            (0, "out0".into()),
            (1, "out1".into()),
            (2, "out2".into()),
            (3, "out3".into())
        ]
    );
    assert!(
        dt < std::time::Duration::from_millis(300),
        "4x80ms overlapped, took {dt:?}"
    );
}
#[tokio::test]
async fn join_task_panic_becomes_error_output() {
    use crate::agent::ToolOutput;
    use futures::future::BoxFuture;
    let futs = vec![(
        0,
        Box::pin(async {
            panic!("boom");
            #[allow(unreachable_code)]
            ToolOutput::error("unreachable")
        }) as BoxFuture<'static, ToolOutput>,
    )];
    let cancel = tokio_util::sync::CancellationToken::new();
    let got = join_ordered(futs, &cancel).await;
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0, 0, "panic keeps its real input index");
    let (_, out) = &got[0];
    assert!(
        out.as_ref().unwrap().is_error,
        "panic must be data, not a crash"
    );
}
#[tokio::test]
async fn join_cancel_marks_unfinished_none() {
    use crate::agent::ToolOutput;
    use futures::future::BoxFuture;
    let mk = |i: usize| {
        (
            i,
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                #[allow(unreachable_code)]
                ToolOutput::ok(format!("out{i}"))
            }) as BoxFuture<'static, ToolOutput>,
        )
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        c2.cancel();
    });
    let got = join_ordered(vec![mk(0), mk(1)], &cancel).await;
    assert_eq!(
        got.len(),
        2,
        "every input yields an entry, even when cancelled"
    );
    assert_eq!(got.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1]);
    assert!(
        got.iter().all(|(_, o)| o.is_none()),
        "cancelled calls yield None"
    );
}

#[test]
fn archive_extractors_never_batch_as_reads() {
    for command in [
        "tar xf a.tar",
        "tar -xzf release.tgz -C /tmp/x",
        "bsdtar xf a.tar",
        "unzip bundle.zip",
        "gunzip data.gz",
        "7z x pack.7z",
        "cpio -id < in.cpio",
    ] {
        assert!(
            !bash_is_batchable(command),
            "extractor batched as read-only: {command}"
        );
    }

    assert!(bash_is_batchable("git diff --stat"));
}

#[test]
fn rewrite_conflicts_demotes_only_the_rewritten_offender() {
    let read = json!({"path": "a.rs"});
    let rewritten_write = json!({"path": "a.rs", "content": "x"});
    let calls: Vec<(&str, &Value)> = vec![("read", &read), ("write", &rewritten_write)];
    assert_eq!(rewrite_conflicts(&calls), vec![1]);

    let r1 = json!({"path": "a.rs"});
    let r2 = json!({"path": "b.rs"});
    let calls: Vec<(&str, &Value)> = vec![("read", &r1), ("read", &r2)];
    assert!(rewrite_conflicts(&calls).is_empty());

    let grep = json!({"pattern": "y"});
    let bash = json!({"command": "rm -rf build"});
    let calls: Vec<(&str, &Value)> = vec![("grep", &grep), ("bash", &bash)];
    assert_eq!(rewrite_conflicts(&calls), vec![1]);
}
