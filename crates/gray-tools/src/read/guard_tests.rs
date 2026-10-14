use super::*;

#[test]
fn refusal_message_is_contract_exact() {
    assert_eq!(
        refusal("/dev/zero", "device/FIFO/socket"),
        "read refused: /dev/zero is a device/FIFO/socket; reading it would block. \
             Use bash with a timeout if you really need it."
    );
}

#[test]
fn blocklisted_names_hit() {
    for p in [
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/fd/3",
        "/proc/self/fd",
        "/proc/self/fd/0",
        "/proc/1234/fd",
        "/proc/1234/fd/5",
        "/dev/zero",
        "/dev/urandom",
        "/dev/random",
        "/dev/tty",
        "/dev/tty1",
        "/dev/ttyS0",
        "dev/fd/3",
    ] {
        assert!(is_blocklisted(Path::new(p)), "{p}");
        assert!(check_name(Path::new(p), None, p).is_err(), "{p}");
    }
}

#[test]
fn innocent_names_pass() {
    for p in [
        "zero",
        "/home/u/zero",
        "/tmp/x.txt",
        "/dev",
        "/dev/null",
        "/proc/abc/fd/3",
        "/proc/1234/task",
        "/proc/self/maps",
    ] {
        assert!(!is_blocklisted(Path::new(p)), "{p}");
    }
}

#[test]
fn name_hit_needs_no_io() {
    let missing = Path::new("/dev/fd/999998");
    assert!(!missing.exists());
    assert!(check_name(missing, None, "/dev/fd/999998").is_err());
}

#[test]
fn canonical_hit_when_literal_hides_device() {
    let literal = Path::new("/tmp/link-to-zero");
    assert!(!is_blocklisted(literal));
    assert!(check_name(literal, Some(Path::new("/dev/zero")), "/tmp/link-to-zero").is_err());
}

#[test]
fn metadata_regular_file_and_dir() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.txt");
    std::fs::write(&file, "hi").unwrap();
    assert!(matches!(
        check_metadata(&std::fs::metadata(&file).unwrap(), "a.txt").unwrap(),
        MetadataDecision::RegularFile
    ));
    assert!(matches!(
        check_metadata(&std::fs::metadata(dir.path()).unwrap(), "d").unwrap(),
        MetadataDecision::Directory
    ));
}

#[cfg(unix)]
#[test]
fn metadata_char_device_fifo_socket_refused() {
    if Path::new("/dev/null").exists() {
        let err =
            check_metadata(&std::fs::metadata("/dev/null").unwrap(), "/dev/null").unwrap_err();
        assert!(err.contains("is a device;"), "{err}");
    }
    let dir = tempfile::tempdir().unwrap();

    let fifo = dir.path().join("f.fifo");
    let cstr = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(cstr.as_ptr(), 0o600) }, 0);
    let err = check_metadata(&std::fs::metadata(&fifo).unwrap(), "f.fifo").unwrap_err();
    assert!(err.contains("is a FIFO;"), "{err}");

    let sock = dir.path().join("s.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let err = check_metadata(&std::fs::metadata(&sock).unwrap(), "s.sock").unwrap_err();
    assert!(err.contains("is a socket;"), "{err}");
}
