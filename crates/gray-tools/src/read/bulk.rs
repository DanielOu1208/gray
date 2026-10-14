use std::path::Path;

pub const MAX_MATCHES: usize = 200;

pub const AGGREGATE_BYTES: u64 = 100 * 1024;

pub const DEFAULT_DIR_EXCLUDES: &[&str] = &["node_modules", "target", ".git", "dist"];

pub fn header(rel: &str) -> String {
    format!("==> {rel} <==")
}

pub fn is_glob(s: &str) -> bool {
    s.chars().any(|c| c == '*' || c == '?')
}

fn mentions_dir(pattern: &str, dir: &str) -> bool {
    pattern.split('/').any(|seg| seg == dir)
}

pub fn is_excluded(rel: &str, inputs: &[String]) -> bool {
    let norm = rel.strip_prefix("./").unwrap_or(rel);
    if inputs
        .iter()
        .any(|p| p.strip_prefix("./").unwrap_or(p) == norm)
    {
        return false;
    }
    if let Some(hit) = DEFAULT_DIR_EXCLUDES
        .iter()
        .find(|d| norm.split('/').any(|seg| seg == **d))
        && !inputs.iter().any(|p| mentions_dir(p, hit))
    {
        return true;
    }
    norm.rsplit('/')
        .next()
        .is_some_and(|base| base.ends_with(".lock"))
}

struct Globs {
    full: globset::GlobSet,
    base: globset::GlobSet,
}

impl Globs {
    fn compile<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Self {
        let mut full = globset::GlobSetBuilder::new();
        let mut base = globset::GlobSetBuilder::new();
        for p in patterns {
            let pat = p.strip_prefix("./").unwrap_or(p);
            let (builder, pattern) = if let Some(dir) = pat.strip_suffix('/') {
                (&mut full, format!("{dir}/**"))
            } else if pat.contains('/') {
                (&mut full, pat.to_string())
            } else {
                (&mut base, pat.to_string())
            };

            if let Ok(glob) = globset::GlobBuilder::new(&pattern)
                .literal_separator(true)
                .build()
            {
                builder.add(glob);
            }
        }
        let build =
            |b: globset::GlobSetBuilder| b.build().unwrap_or_else(|_| globset::GlobSet::empty());
        Self {
            full: build(full),
            base: build(base),
        }
    }

    fn is_match(&self, rel: &str) -> bool {
        if self.full.is_match(rel) {
            return true;
        }
        let base = rel.rsplit('/').next().unwrap_or(rel);
        self.base.is_match_candidate(&globset::Candidate::new(base))
    }
}

pub fn expand(cwd: &Path, inputs: &[String], extra_excludes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let excludes = Globs::compile(extra_excludes.iter().map(String::as_str));
    let globs: Vec<&str> = inputs
        .iter()
        .filter(|s| is_glob(s))
        .map(String::as_str)
        .collect();
    for lit in inputs.iter().filter(|s| !is_glob(s)) {
        let rel = lit.strip_prefix("./").unwrap_or(lit).replace('\\', "/");
        if out.contains(&rel) {
            continue;
        }
        if excludes.is_match(&rel) {
            continue;
        }
        if std::fs::metadata(cwd.join(&rel)).is_ok_and(|m| m.is_file()) {
            out.push(rel);
        }
    }
    if !globs.is_empty() {
        let matcher = Globs::compile(globs.iter().copied());
        let walker = ignore::WalkBuilder::new(cwd)
            .hidden(false)
            .require_git(false)
            .build();
        for entry in walker.flatten() {
            let Ok(rel) = entry.path().strip_prefix(cwd) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue;
            }
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = rel.to_string_lossy().replace('\\', "/");
            if out.contains(&rel) {
                continue;
            }
            if excludes.is_match(&rel) {
                continue;
            }
            if is_excluded(&rel, inputs) {
                continue;
            }
            if matcher.is_match(&rel) {
                out.push(rel);
            }
        }
    }
    out.sort();
    out.truncate(MAX_MATCHES);
    out
}

pub fn fit_within_cap(files: &[(String, u64)]) -> (Vec<String>, Vec<String>) {
    let mut shown = Vec::new();
    let mut total: u64 = 0;
    for (i, (name, size)) in files.iter().enumerate() {
        if i > 0 && total.saturating_add(*size) > AGGREGATE_BYTES {
            return (shown, files[i..].iter().map(|(n, _)| n.clone()).collect());
        }
        total = total.saturating_add(*size);
        shown.push(name.clone());
    }
    (shown, Vec::new())
}

#[path = "bulk_tests.rs"]
#[cfg(test)]
mod tests;
