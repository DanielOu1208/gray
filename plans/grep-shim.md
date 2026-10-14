# grep/rg shim over the fsearch index — design contract

Owner: lead (this doc). Implementer and tester work against it independently.

## Goal
The model types plain `grep`/`rg` — in pipes, after `cd X &&`, in loops, anywhere —
and gets byte-for-byte what the real tool prints, faster on big repos. The model is
never told; no prompt text changes.

## Shape
1. **Shim on PATH.** Session start creates `$GRAY_HOME/shim/{grep,rg}` → symlinks to
   the gray binary and prepends that dir to the bash child's PATH. `gray` main checks
   `argv[0]` basename first: `grep`/`rg` → shim mode, before clap, before any config.
2. **Shim mode** asks the session's index over `$GRAY_SEARCH_SOCK` (unix socket, JSON
   line) "which files under these roots could match", gets a candidate list or a
   decline, then `execv`s the REAL binary (first match on PATH with the shim dir
   removed) on that list. Declined, socket missing, timeout (>50 ms), any doubt →
   `execv` the real binary with the original argv, untouched. The shim never formats
   output; the real tool does.
3. **The index only prunes.** Candidates = (indexed files whose content may contain
   the pattern's required literals) ∪ (every file the real tool would read that the
   content index does not hold as-is: `.git/`, binary, >1 MB, lockfiles, minified,
   gitignored-but-grep-reads-it, created/changed since sync). Pruning must be a
   superset of true matches — prove it per pattern class, decline otherwise.
4. **Selection emulation.** Because explicit file args bypass a tool's own walk
   filters, the server computes the walk the tool would have done (grep: -r
   recursion, --include/--exclude/--exclude-dir, no ignore files, follows nothing;
   rg: .gitignore/.ignore/.rgignore, hidden, -g, -t/-T, --hidden, --no-ignore*,
   -u…, max-depth) and the shim strips those flags and passes files instead, adding
   `-H` (grep) / `--with-filename` (rg) unless `-h`/`-I`-style overrides say not to.
   Reuse `index_filter::IgnoreChain` for rg rules. Batch past ARG_MAX; merge exit
   codes (2 if any batch 2, else 0 if any 0, else 1).
5. **Pattern → required literals.** Parse BRE (grep default), ERE (-E), fixed (-F),
   multiple -e, Rust-regex (rg) into an OR-of-AND literal set conservatively. No
   extractable literal of ≥3 bytes in every branch → decline. `-i` → case-folded
   lookup that is Unicode-safe (Kelvin sign, long s) or decline when the literal
   holds k/s or non-ASCII. -P → decline unless trivially literal.
6. **Freshness.** On Linux fsearch only relists folders whose mtime moved, on a 60 s
   tick, so in-place edits can stay invisible. Fix: the session watches its project
   root with inotify (recursive, one watch per dir; IN_MODIFY, CLOSE_WRITE, CREATE,
   DELETE, MOVED_*, ATTRIB). Every query first drains the inotify fd — writes made
   earlier in the *same* command are already queued — and every touched path joins
   the pass-through set until the content index re-syncs it. IN_Q_OVERFLOW, watch
   limit, or an unwatched new dir race → decline the whole root until a full resync.
7. **Index root.** Today content indexing is `$HOME`-only, so `/testbed` and `/app`
   in benchmark containers are never served. Add: the session's project root (git
   worktree of the launch cwd, else cwd if it is a repo-like tree) is indexed
   regardless of `$HOME`. Build starts at session start in the background at low IO
   priority; until it is ready every call declines (real tool runs).
8. **Remove the old bare claim.** `bare_search_args` (`grep PAT PATH`, `find -name`)
   answers in gray's GrepTool/fd format, which is NOT what grep/find print. Delete
   the bare-command claim; keep the explicit `gray grep`/`gray find` verbs as-is.
9. **Servable:** grep with -n -i -w -x -l -o -h -H -s -q -I -E -F -G -e
   -A/-B/-C -m --include --exclude --exclude-dir --color*, plus -r on
   directory operands. Operands resolve per operand: a recursive directory
   under the root is answered from the index (needs a >=3-byte required
   literal per pattern); every other operand — file, missing path,
   non-recursive dir, dir outside the root, --exclude-dir-named operand,
   unneedled pattern — is passed through to the real tool verbatim
   (identical output, walk flags stay in argv). Trace tags each served call
   `mode: index|pass`. Still declines: -v -L -c -R -z -P -f --files --pre
   -U --json, stdin input (`grep pat` with no -r, operand `-`), rg.
10. **Persistence.** `search-serve --index-dir DIR` keeps the index across
    restarts; on Linux a restart relists folders changed since the save and
    revalidates doc (size,mtime), so a cold boot after edits is a relist,
    not a rebuild. Without --index-dir the index is a tempdir as before.
11. **Observability.** `GRAY_SHIM_TRACE=<file>` appends one JSON line per call:
    `{tool, argv, cwd, served, reason, candidates, scope_files, server_ms, total_ms}`.
    The benchmark and the video read this. `GRAY_SEARCH_INDEX=off` disables
    everything (no shim on PATH) — that is the control arm.
12. **Test entry point.** Hidden subcommand `gray search-serve --root DIR --sock PATH`
    runs the same server outside a session, so the differential tester can drive
    `PATH=$shimdir:$PATH GRAY_SEARCH_SOCK=$sock grep ...` directly.

## Lead amendments (2026-10-14, override anything above)
- **File order is part of the contract for grep.** Models pipe to `| head` constantly;
  a different file order changes what they see. GNU `grep -r` visits files in
  readdir order (fts, unsorted). The shim must hand candidate files to the real grep
  in exactly the order `grep -r` would have visited them (server reproduces the walk
  order: readdir order per directory, depth-first as fts does). rg's order is
  thread-dependent, so rg stays order-free.
- **Must work under `gray --bare`.** The benchmark runs `gray --bare --json`; the shim
  is core bash behaviour, not a plugin, and must be active there unless
  `GRAY_SEARCH_INDEX=off`.
- **Benchmark containers:** repo at /app (not under $HOME, HOME=/root), 2 CPUs, 8 GB,
  overlayfs. Index build time and peak RSS there must be measured and reported.

## Equivalence contract (what the tester checks)
For every command: identical exit status; identical stderr as a multiset of lines;
grep: identical stdout byte-for-byte (file order included, see amendments).
rg: identical stdout after grouping into per-file blocks (file order free).
A pipeline (`| head`) that differs is a FAILURE, not a pass. `-l` / `rg -c` compared as sets. `-q` exit only.

## Non-goals (v1)
`find` shim, `git grep`, `ag`, Windows, macOS behaviour changes (FSEvents path keeps
working; inotify is Linux-only), any prompt or tool-surface change.
