//! Whole-disk file search for macOS: a fuzzy name index over every entry on
//! disk, kept live from FSEvents, plus a trigram content index for text
//! files. `Engine` runs it all in-process; the `fsearch` binary wraps one in
//! a daemon with a JSON-lines socket.
//!
//! Unix only — paths are raw bytes, the daemon speaks on a Unix socket, and
//! liveness comes from FSEvents/inotify. On Windows the crate is empty and
//! `gray-tools` picks another index backend.

#[cfg(unix)]
pub mod alloc;
#[cfg(unix)]
pub mod content;
#[cfg(unix)]
mod engine;
#[cfg(all(unix, target_os = "macos"))]
mod fsevents;
#[cfg(all(unix, not(target_os = "macos")))]
#[path = "fsevents_linux.rs"]
mod fsevents;
#[cfg(unix)]
pub mod index;
#[cfg(unix)]
pub mod live;
#[cfg(unix)]
pub mod query;
#[cfg(all(unix, target_os = "macos"))]
pub mod walk;
#[cfg(all(unix, not(target_os = "macos")))]
#[path = "walk_linux.rs"]
pub mod walk;

#[cfg(unix)]
pub use content::{Case, FileMatches, Grep, GrepResult, Hit};
#[cfg(unix)]
pub use engine::{Engine, Found, Options, Status, default_dir, gated, has_full_disk_access, no_materialize};
#[cfg(unix)]
pub use query::{GrepMode, Query};
