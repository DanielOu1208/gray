# Gray source audit — fixes record

- Audit: `gray_source_audit_candidate_issues.pdf`, 23 candidate findings, baseline `main @ 4f26803`
- Landed on `feat/app-setup` (PR #132), 13 commits, all gates green per commit
- Method: every candidate reproduced against the real code first; 22 fixes, 1 verified false

## Concurrency / ownership

| # | Finding | Disposition |
|---|---------|-------------|
| 1 | Gateway stale-PID replacement race: `read` and `remove_file` were separate fs ops, so one starter's remove could delete a sibling's fresh claim and both would continue | Fixed: the whole read → remove → create decision runs under one flock (`gateway/pid.rs`); wait-then-degrade like the session store |
| 2 | `host/ask` awaited the UI driver *before* entering the `select`, so a blocked stdin read suspended the 300s TTL and the cancel branch | Fixed: the driver runs as its own task, selected alongside the TTL and cancellation |
| 3 | `commands.json` helper kept the lock file but **discarded the `try_lock` error**, so concurrent managers read-modify-wrote without exclusion | Fixed: bounded wait, then fail loudly; unsupported filesystems still degrade to no guard |
| 13 | `lock.json` read-modify-write not serialized | Fixed: `hold_registry_lock()` wraps install/remove/set_enabled |
| 14 | Plugin `remove()` deleted files before the lock-file commit | Fixed: commit first, delete second — a failed write now leaves a re-removable plugin |
| 23 | Two simultaneous setting changes loaded and replaced the whole `config.json` | Fixed: `lock_saved_config_at()` spans every load-modify-save in the REPL and setup modals |

## Protocol / plugin

| # | Finding | Disposition |
|---|---------|-------------|
| 4 | Oversized frames desync the NDJSON protocol | **False**: the reader's per-iteration `take(MAX_FRAME+1)` leaves the remainder of an over-long line in the same `BufReader`, so the next `read_until` consumes exactly that remainder (it cannot parse, and is dropped); the next frame is still the next line. The 64-chunk streak guard covers never-terminating garbage |
| 5 | A TTL firing mid-write left a torn frame in the child's stdin | Fixed: `try_write_frame()` bounds every write; a failed/timed-out write kills that child generation (the stream is unfixable) |
| 6 | A child that stops reading stdin held a host concurrency permit forever | Fixed: the host-reply write is bounded; on timeout the handler task ends and the permit is reclaimed |
| 7 | The `host/run` stdout drain dropped the read half at its cap, handing chatty children SIGPIPE/EPIPE | Fixed: the drain keeps reading to EOF into a sink — the cap holds memory, not the pipe |

## Data loss / state

| # | Finding | Disposition |
|---|---------|-------------|
| 9 | Session delete took only the process-local mutex | Fixed: takes the per-session cross-process lock |
| 10 | `list()` could rename a creator's partial header away as "corrupt" | Fixed: quarantine only a *complete* first line that fails to parse; a torn (unterminated) first line is a creator mid-write |
| 11 | Skill discovery followed symlinked dirs with no cycle detection | Fixed: canonical visited set |
| 12 | An unparseable `auth.json` read as an empty store and could be overwritten with one entry | Fixed: `load_mixed_store_strict()`; write paths refuse on a parse error |

## TOCTOU / races

| # | Finding | Disposition |
|---|---------|-------------|
| 8 | write/edit validated and replaced in separate syscalls | Fixed: verify-at-apply — metadata the check saw plus the current bytes' hash, re-proved right before the rename |
| 19 | Log rotation unsynchronized; a process could keep writing a rotated-away inode | Fixed: the shuffle runs under a `gray.log.lock` flock, and the logger re-anchors on dev/ino drift |
| 20 | `umask(0077)` around the socket bind is process-wide — that window is how cron status got EACCES | Fixed: bind inside a fresh 0700 staging dir and rename into place; the dance remains only as fallback |

## Correctness / platform

| # | Finding | Disposition |
|---|---------|-------------|
| 15 | `pdftotext`/`ffmpeg` ran unbounded | Fixed: mpsc + `recv_timeout` (30s), the clipboard text path's pattern |
| 16 | The clipboard *image* probe was still unbounded | Fixed: same bounded spawn |
| 17 | `Environment=GRAY_HOME` unquoted in the systemd unit | Fixed: quoted with inner-quote escaping |
| 18 | Shared `models.json.tmp` across concurrent refreshes | Fixed: per-process unique temp name |
| 21 | `logs_tail.total_lines_available` counted only the retained tail | Fixed: counts the whole file |
| 22 | `$EDITOR` split on raw whitespace | Fixed: word tokenizer honoring single/double quotes |
| M | Windows archive path-confinement | Fixed: `C:\evil`-style drive names were neither absolute nor `..`, and `dest.join()` with a drive-prefixed path replaces the base on Windows — `unsafe_entry_name()` plus a post-join `confined()` guard in both extractors |

## Notes

- One test flake during the sweep (`cron_fire::script_failure_marks_not_ok`) traced to a disk-full window (`target/debug/incremental` at 20G); three clean reruns, not a code issue.
- Every fix landed with a failing test first; `cargo fmt --check`, `clippy --workspace -- -D warnings`, and `cargo test --workspace` green before each commit.
