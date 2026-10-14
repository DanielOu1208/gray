//! `grep_answer` — the in-process answer path the bash tool's bare-`grep`
//! claim uses — against GNU grep byte for byte. Every case must come back
//! byte-identical stdout/stderr/exit-code, or `None` (declined).

#![cfg(unix)]

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use super::{Answer, grep_answer};

fn write_tree(root: &Path) {
    let src = root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("a.py"),
        "def needle():\n    return 'needle'\n\nsecond needle line\ntrailing  \n",
    )
    .unwrap();
    std::fs::write(src.join("b.py"), "# needle again\nno match here\n").unwrap();
    std::fs::write(src.join("bin.dat"), b"ab\x00needle\x00cd").unwrap();
    std::fs::create_dir_all(root.join(".git/hooks")).unwrap();
    std::fs::write(root.join(".git/hooks/x.sample"), "needle in git\n").unwrap();
    std::fs::create_dir_all(root.join("deep/d1/d2")).unwrap();
    std::fs::write(root.join("deep/d1/d2/c.txt"), "deep needle\n").unwrap();
}

fn real_grep(argv: &[String], cwd: &Path) -> (Vec<u8>, Vec<u8>, i32) {
    let out = Command::new("grep")
        .args(argv)
        .current_dir(cwd)
        .output()
        .unwrap();
    (out.stdout, out.stderr, out.status.code().unwrap_or(-1))
}

#[test]
fn grep_answer_matches_gnu() {
    let work = tempfile::TempDir::new().unwrap();
    let idx = tempfile::TempDir::new().unwrap();
    write_tree(work.path());
    let engine = fsearch::Engine::start(fsearch::Options {
        dir: idx.path().to_path_buf(),
        home: work.path().to_string_lossy().into_owned(),
        root: Some(work.path().to_path_buf()),
        skip: Some(Vec::new()),
    })
    .unwrap();

    let deadline = Instant::now() + std::time::Duration::from_secs(90);
    loop {
        let st = engine.status();
        if st.ready && st.content_pending == 0 && st.content_docs > 0 {
            break;
        }
        assert!(Instant::now() < deadline, "index never became ready");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let argv = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let cases: Vec<Vec<String>> = vec![
        argv(&["-rn", "needle", "."]),
        argv(&["-rni", "NEEDLE", "."]),
        argv(&["-rl", "needle", "."]),
        argv(&["-ro", "needle", "."]),
        argv(&["-rc", "needle", "."]),
        argv(&["-rcl", "needle", "."]),
        argv(&["-rnC1", "needle", "."]),
        argv(&["-rEn", "nee[a-z]le", "."]),
        argv(&["-rFn", "needle", "."]),
        argv(&["-rn", "needle", "src", "deep"]),
        argv(&["-rn", "needle", "src/a.py"]), // file operand: must decline
        argv(&["-q", "-r", "needle", "."]),
        argv(&["-rI", "-n", "needle", "."]),
    ];
    let mut served = 0;
    for args in &cases {
        match grep_answer(&engine, args, work.path()) {
            Some(Answer::Print { stdout, stderr, code }) => {
                served += 1;
                let (rout, rerr, rcode) = real_grep(args, work.path());
                assert_eq!(
                    (stdout.as_slice(), stderr.as_slice(), code),
                    (rout.as_slice(), rerr.as_slice(), rcode),
                    "mismatch on {args:?}\n--- ours:\n{}{}\n--- real:\n{}{}",
                    String::from_utf8_lossy(&stdout),
                    String::from_utf8_lossy(&stderr),
                    String::from_utf8_lossy(&rout),
                    String::from_utf8_lossy(&rerr),
                );
            }
            Some(Answer::Exec { .. }) => panic!("grep_answer returned Exec for {args:?}"),
            None => {
                // Legit declines: a file operand, and a *matching* binary
                // under `-o`/context — shapes the renderer cannot reproduce
                // exactly, so the real tool answers.
                let ok = args.last().map(String::as_str) == Some("src/a.py")
                    || args.iter().any(|a| {
                        a.starts_with('-') && a[1..].chars().any(|c| "oCc".contains(c))
                    });
                assert!(ok, "declined {args:?}");
            }
        }
    }
    assert!(served >= 8, "too few cases served: {served}");
}
