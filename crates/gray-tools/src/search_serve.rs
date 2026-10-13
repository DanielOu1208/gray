//! The grep/rg shim, server side: answers `search_shim` requests from the
//! session's index for one root. Every answer is either the real tool's argv
//! plus the files it must read, or a decline. The index only prunes; the
//! real tool produces the output.
//!
//! `gray search-serve --root DIR --sock PATH` runs this outside a session
//! (the differential tester's entry point); a session runs the same server.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

/// Serve queries for `root` on `sock` until the process dies.
#[cfg(unix)]
pub fn serve(root: &Path, sock: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(sock);
    let listener = UnixListener::bind(sock)?;
    let root: PathBuf = root.canonicalize()?;
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let root = root.clone();
        std::thread::spawn(move || {
            let _ = answer(&root, stream);
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn serve(_root: &Path, _sock: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("search-serve needs a unix socket"))
}

/// One request line in, one reply line out.
#[cfg(unix)]
fn answer(root: &Path, mut stream: UnixStream) -> std::io::Result<()> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let mut out = decide(root, &line).to_string();
    out.push('\n');
    stream.write_all(out.as_bytes())
}

/// The server's answer to one request. Declines until the index serves.
#[cfg(unix)]
fn decide(_root: &Path, _request: &str) -> serde_json::Value {
    serde_json::json!({ "served": false, "reason": "index not built" })
}
