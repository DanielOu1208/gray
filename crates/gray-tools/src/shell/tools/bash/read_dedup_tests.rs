use super::*;

use crate::shell::contract::INLINE_BUDGET_BYTES;

fn fixture() -> (tempfile::TempDir, FileLedger) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ledger = FileLedger::new();
    (dir, ledger)
}

fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write");
    path
}

#[cfg(unix)]
#[test]
fn only_a_bare_single_file_read_is_a_plain_read() {
    let (dir, _) = fixture();
    let path = write(&dir, "a.rs", "one\ntwo\n");
    let cwd = dir.path();
    let whole = |c: &str| plain_read(c, cwd).map(|r| (r.window, r.whole_file));

    assert_eq!(whole("cat a.rs"), Some(((1, None), true)));
    assert_eq!(whole("  cat   a.rs  "), Some(((1, None), true)));
    assert_eq!(whole("head -n 20 a.rs"), Some(((1, Some(20)), false)));
    assert_eq!(whole("tail -n 20 a.rs"), Some(((0, Some(20)), false)));
    assert_eq!(whole("sed -n '1,40p' a.rs"), Some(((1, Some(40)), false)));
    assert_eq!(whole("sed -n '5,$p' a.rs"), None);
    assert_eq!(whole("sed -n '1,$p' a.rs"), Some(((1, None), true)));

    for c in [
        "cat a.rs | head",
        "cat a.rs b.rs",
        "cat -n a.rs",
        "cat *.rs",
        "cat ~/a.rs",
        "cat $HOME/a.rs",
        "cat a.rs > copy",
        "wc -l a.rs",
        "cat",
        "cat a.rs; rm a.rs",
        "cat missing.rs",
        "cat .",
        "head -c 100 a.rs",
        "sed -n '5,1p' a.rs",
        "sed -e 1p a.rs",
        "",
    ] {
        assert_eq!(whole(c), None, "{c:?} must not be a plain read");
    }

    let arg = path.display().to_string().replace('\\', "/");
    let abs = plain_read(&format!("cat {arg}"), cwd).expect("abs");

    assert_eq!(
        std::fs::canonicalize(&abs.path).expect("abs resolves"),
        std::fs::canonicalize(&path).expect("path resolves")
    );

    std::fs::create_dir(cwd.join("~")).expect("mkdir");
    write(&dir, "~/a.rs", "tilde\n");
    assert!(plain_read("cat ~/a.rs", cwd).is_none());
    assert!(
        plain_read("cat -a.rs", cwd).is_none(),
        "a flag is not a path"
    );
    assert!(
        plain_read("head -n 0 a.rs", cwd).is_none(),
        "no lines, no window"
    );
}

#[test]
fn a_repeat_of_an_unchanged_file_is_stubbed_once() {
    let (dir, ledger) = fixture();
    let path = write(&dir, "a.rs", "one\ntwo\nthree\n");
    let read = plain_read("cat a.rs", dir.path()).expect("plain read");
    let size = std::fs::metadata(&path).unwrap().len();

    assert!(check(&ledger, &read, true).is_none());
    record(&ledger, &read, size);

    let hit = check(&ledger, &read, true).expect("stubbed");
    assert!(hit.content.contains("unchanged since your previous read"));
    assert!(hit.content.contains("cat a.rs"));
    assert!(!hit.content.contains("three"));

    assert!(check(&ledger, &read, true).is_none());
    record(&ledger, &read, size);
    assert!(check(&ledger, &read, true).is_some());
}

#[test]
fn a_changed_file_never_stubs() {
    let (dir, ledger) = fixture();
    let path = write(&dir, "a.rs", "one\n");
    let read = plain_read("cat a.rs", dir.path()).expect("plain read");
    let size = std::fs::metadata(&path).unwrap().len();
    record(&ledger, &read, size);

    std::fs::write(&path, "two\n").expect("rewrite");
    let later =
        std::fs::metadata(&path).unwrap().modified().unwrap() + std::time::Duration::from_secs(2);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(later))
        .expect("bump mtime");
    let changed = plain_read("cat a.rs", dir.path()).expect("plain read");
    assert!(check(&ledger, &changed, true).is_none());
    record(&ledger, &changed, size);

    assert!(check(&ledger, &changed, true).is_some());
}

#[test]
fn a_different_window_is_a_different_read() {
    let (dir, ledger) = fixture();
    write(&dir, "a.rs", "one\ntwo\nthree\n");
    let whole = plain_read("cat a.rs", dir.path()).expect("plain read");
    let head = plain_read("head -n 2 a.rs", dir.path()).expect("plain read");
    let tail = plain_read("tail -n 2 a.rs", dir.path()).expect("plain read");

    record(&ledger, &whole, 14);
    assert!(
        check(&ledger, &head, true).is_none(),
        "a window nobody read is never stubbed"
    );
    assert!(check(&ledger, &tail, true).is_none());
    assert!(check(&ledger, &whole, true).is_some());

    record(&ledger, &head, 8);
    assert!(check(&ledger, &head, true).is_some());
    assert!(check(&ledger, &whole, true).is_none());
}

#[test]
fn an_elided_run_is_never_recorded() {
    let (dir, ledger) = fixture();
    let path = write(&dir, "big.txt", &"x".repeat(INLINE_BUDGET_BYTES + 100));
    let size = std::fs::metadata(&path).unwrap().len();
    let read = plain_read("cat big.txt", dir.path()).expect("plain read");

    record(&ledger, &read, size);
    assert!(check(&ledger, &read, true).is_none());
}

#[test]
fn a_whole_file_read_needs_a_whole_view_to_stub() {
    let (dir, ledger) = fixture();
    write(&dir, "a.rs", "one\ntwo\n");
    let read = plain_read("cat a.rs", dir.path()).expect("plain read");

    let head = plain_read("head -n 1 a.rs", dir.path()).expect("plain read");
    record(&ledger, &head, 4);
    assert!(check(&ledger, &read, true).is_none());
}

#[test]
fn the_kill_switch_and_a_lifecycle_reset_both_land() {
    let (dir, ledger) = fixture();
    let path = write(&dir, "a.rs", "one\n");
    let size = std::fs::metadata(&path).unwrap().len();
    let read = plain_read("cat a.rs", dir.path()).expect("plain read");
    record(&ledger, &read, size);
    assert!(check(&ledger, &read, false).is_none(), "GRAY_READ_DEDUP=0");
    assert!(check(&ledger, &read, true).is_some());

    ledger.clear();
    assert!(check(&ledger, &read, true).is_none());

    record(&ledger, &read, size);
    ledger.disarm_all_dedup();
    assert!(check(&ledger, &read, true).is_none());
}
