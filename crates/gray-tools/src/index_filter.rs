//! Ignore-file parity for index-served answers.
//!
//! `fd --hidden` and `rg --hidden` still filter results through ignore
//! files — `.gitignore`, `.ignore`, `.fdignore`/`.rgignore`,
//! `.git/info/exclude`, and the global gitignore — while the fsearch index
//! happily lists everything it scanned. For the index lanes to answer
//! exactly like the tools, returned hits are filtered through the same
//! rules here.
//!
//! Matchers are built lazily per call: only ancestor directories of actual
//! hits get their ignore files read, cached for the duration of the call,
//! so a query never walks the tree. A hit under a directory ignored at a
//! shallower level is dropped — git never descends into an ignored dir, so
//! no deeper rule may un-ignore it.
//!
//! Lane rules differ, matching the real tools:
//! - find (fd 10.x, `--no-require-git` outside repos): `.gitignore` applies
//!   at every level, `.ignore` and `.fdignore` on top, `.git/info/exclude`
//!   and the global file inside a worktree.
//! - grep (rg): `.gitignore` applies only inside a worktree, `.ignore` and
//!   `.rgignore` anywhere, `.git/info/exclude` and the global file inside a
//!   worktree.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    Find,
    Grep,
}

/// Per-call ignore evaluator. `new` is cheap — nothing is read until hits
/// are checked.
pub struct IgnoreChain {
    lane: Lane,
    /// Nearest ancestor holding a worktree marker (`.git` or `.jj`): the
    /// boundary git-family rules apply under for rg, and where
    /// `.git/info/exclude` lives. `None` = outside any worktree.
    git_root: Option<PathBuf>,
    /// Global gitignore (git config `core.excludesFile`, XDG fallback),
    /// rooted like a top-level .gitignore of the worktree/search root.
    global: Gitignore,
    /// dir -> its ignore matchers, ordered low→high precedence.
    cache: HashMap<PathBuf, Rc<Vec<Gitignore>>>,
}

impl IgnoreChain {
    pub fn new(root: &Path, lane: Lane) -> Self {
        let git_root = root
            .ancestors()
            .find(|p| p.join(".git").exists() || p.join(".jj").exists())
            .map(|p| p.to_path_buf());
        // Global rules behave like a .gitignore at the worktree root (or
        // the search root outside one): that's what both tools apply.
        let global_at = git_root.clone().unwrap_or_else(|| root.to_path_buf());
        let global = GitignoreBuilder::new(&global_at).build_global().0;
        Self { lane, git_root, global, cache: HashMap::new() }
    }

    /// `path`'s ignore verdict under this lane's rules: true = the real
    /// tool would suppress it from results.
    pub fn is_ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        // Ancestor dirs of `path`, top-down. The chain above the search
        // root matters too — parent .ignore files apply to results, and a
        // root under an ignored dir reports nothing.
        let mut dirs: Vec<&Path> = path.ancestors().skip(1).collect();
        dirs.reverse();
        // Matchers collected so far, in precedence order (later = deeper).
        let mut seen: Vec<Rc<Vec<Gitignore>>> = Vec::with_capacity(dirs.len() + 1);
        if !self.global.is_empty() {
            seen.push(Rc::new(vec![self.global.clone()]));
        }
        for dir in dirs {
            // An ignored ancestor prunes everything below it — git never
            // descends, so no deeper ignore file may contradict.
            if seen.iter().flat_map(|v| v.iter()).any(|m| m.matched(dir, true).is_ignore()) {
                return true;
            }
            seen.push(self.level(dir));
        }
        // Deepest rule wins; within a level the ordering built by `level`
        // (exclude < gitignore < ignore < custom) applies.
        seen.iter()
            .flat_map(|v| v.iter())
            .rev()
            .find_map(|m| match m.matched(path, is_dir) {
                x if x.is_none() => None,
                x => Some(x.is_ignore()),
            })
            .unwrap_or(false)
    }

    /// `dir`'s ignore matchers for this lane, precedence low→high.
    fn level(&mut self, dir: &Path) -> Rc<Vec<Gitignore>> {
        if let Some(ms) = self.cache.get(dir) {
            return ms.clone();
        }
        let in_worktree = self.git_root.as_ref().is_some_and(|r| dir.starts_with(r));
        let mut files: Vec<PathBuf> = Vec::with_capacity(4);
        if self.git_root.as_deref() == Some(dir) {
            files.push(dir.join(".git/info/exclude"));
        }
        // fd's --no-require-git keeps .gitignore live anywhere; rg needs a
        // worktree. .ignore is lane-agnostic, custom names sit on top.
        if self.lane == Lane::Find || in_worktree {
            files.push(dir.join(".gitignore"));
        }
        files.push(dir.join(".ignore"));
        files.push(dir.join(match self.lane {
            Lane::Find => ".fdignore",
            Lane::Grep => ".rgignore",
        }));
        // Each precedence class is its own matcher — later entries override
        // earlier ones in `is_ignored`.
        let mut ms: Vec<Gitignore> = Vec::new();
        for file in files {
            let mut b = GitignoreBuilder::new(dir);
            // add() errors on a missing/unreadable file: skip it like the
            // tools do instead of fabricating rules.
            if b.add(&file).is_none()
                && let Ok(g) = b.build()
                && !g.is_empty()
            {
                ms.push(g);
            }
        }
        let ms = Rc::new(ms);
        self.cache.insert(dir.to_path_buf(), ms.clone());
        ms
    }
}
