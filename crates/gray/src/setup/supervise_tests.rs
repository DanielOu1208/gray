use super::*;

#[test]
fn the_runit_script_execs_the_argv_quoted_for_sh() {
    let script = runit_script(&[
        "/home/vstaln/.cargo/bin/gray-discord".to_string(),
        "run".to_string(),
    ]);
    assert_eq!(
        script,
        "#!/bin/sh\nexec '/home/vstaln/.cargo/bin/gray-discord' 'run'\n"
    );
}

#[test]
fn single_quotes_in_argv_survive_the_sh_quoting() {
    let script = runit_script(&["/bin/it's".to_string()]);
    assert_eq!(script, "#!/bin/sh\nexec '/bin/it'\\''s'\n");
}

#[test]
fn the_pidfile_sits_next_to_the_config() {
    let dir = Path::new("/home/vstaln/.config/gray-discord");
    assert_eq!(pidfile_path(dir, "discord"), dir.join("discord.pid"));
}

#[test]
fn a_missing_pidfile_is_a_clear_error_not_a_panic() {
    let tmp = tempfile::tempdir().unwrap();
    let err = stop_daemon("discord", tmp.path())
        .err()
        .expect("no pid file means nothing to stop");
    assert!(err.to_string().contains("no pid file"), "{err}");
}

#[test]
fn an_unreadable_pidfile_is_reported() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(pidfile_path(tmp.path(), "discord"), "not-a-pid").unwrap();
    let err = stop_daemon("discord", tmp.path())
        .err()
        .expect("a garbage pid must fail cleanly");
    assert!(err.to_string().contains("unreadable pid"), "{err}");
}
