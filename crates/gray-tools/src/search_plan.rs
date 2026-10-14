//! What a `grep` argv asks for, reduced to what the index can answer exactly:
//! the real tool's argv without its walk flags, the one directory it walks,
//! the walk's file and directory rules, and per pattern a literal every
//! matching line must contain. Anything else is a decline with its reason.
//!
//! The literal is a pruning key only. A file the index says cannot contain
//! it is never handed to the real tool; a file it may contain is, and the
//! real tool decides. So a literal that is too broad costs time, and one
//! that is too narrow costs a wrong answer: when in doubt, decline.

use globset::{Glob, GlobMatcher};

/// The shortest literal worth an index lookup. Shorter ones match nearly
/// every file, so the lookup prunes nothing.
const MIN_NEEDLE: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Syntax {
    Basic,
    Extended,
    Fixed,
}

/// `--include` / `--exclude` / `--exclude-dir` as grep applies them to the
/// files its recursion finds.
#[derive(Default)]
pub struct Walk {
    /// In command-line order: `true` for `--include`, `false` for `--exclude`.
    rules: Vec<(bool, GlobMatcher)>,
    exclude_dirs: Vec<GlobMatcher>,
}

impl Walk {
    /// Does recursion keep a file of this base name? The last matching rule
    /// decides; with none matching, the file is kept unless the first rule
    /// is an include.
    pub fn keeps_file(&self, name: &[u8]) -> bool {
        let Ok(name) = std::str::from_utf8(name) else { return false };
        let mut verdict = None;
        for (include, m) in &self.rules {
            if m.is_match(name) {
                verdict = Some(*include);
            }
        }
        match verdict {
            Some(v) => v,
            None => !self.rules.first().is_some_and(|(include, _)| *include),
        }
    }

    /// Does recursion skip a directory of this base name?
    pub fn skips_dir(&self, name: &[u8]) -> bool {
        std::str::from_utf8(name).is_ok_and(|n| self.exclude_dirs.iter().any(|m| m.is_match(n)))
    }
}

/// A grep invocation the index can serve.
pub struct GrepPlan {
    /// The real tool's argv without the operands or `-r`: output flags,
    /// walk flags, and `-e` patterns. Filename printing (`-H`) is decided
    /// by the server once it knows what each operand resolves to.
    pub argv: Vec<String>,
    /// What the real tool would have read or walked, as given. A directory
    /// under the root is served from the index; anything else (a file, a
    /// missing path, a dir without `-r`) is passed through unchanged.
    pub operands: Vec<String>,
    /// Each matching line of the real tool holds one of these, literally.
    /// Meaningful only when `all_needled`.
    pub needles: Vec<String>,
    /// Every pattern contributed at least one required literal. False when
    /// some pattern is unprunable (`.*`, `x|y.z`, ...) — directory operands
    /// must then decline; file operands do not care.
    pub all_needled: bool,
    pub ignore_case: bool,
    pub walk: Walk,
    /// `-H`/`-h` as the caller gave it; `None` is grep's default.
    pub with_name: Option<bool>,
    /// `-r`/`--recursive` was given.
    pub recursive: bool,
    /// The matcher `-E`/`-F`/`-G` selected (default: basic).
    pub syntax: Syntax,
    /// Every pattern operand (`-e` values, or the positional one).
    pub patterns: Vec<String>,
    pub line_number: bool,
    pub only_matching: bool,
    pub count: bool,
    pub files_only: bool,
    pub word: bool,
    pub line_match: bool,
    pub quiet: bool,
    pub no_messages: bool,
    pub ignore_binary: bool,
    pub before: usize,
    pub after: usize,
    pub max_count: Option<usize>,
    /// `--color=always` (or a bare `--color`): the real tool would emit ANSI
    /// colors even piped. The index cannot serve those bytes yet.
    pub color_always: bool,
}

impl GrepPlan {
    /// Can the index print the real tool's bytes for this plan, given every
    /// operand resolved to an indexed directory? Anything it cannot express
    /// exactly falls back to handing the real tool the candidate files.
    pub fn answerable(&self) -> bool {
        if self.color_always {
            return false;
        }
        self.fsearch_grep().is_some()
    }

    /// The patterns as one `fsearch::Grep`, when the index can run them
    /// exactly: `-F` patterns are literal, a `-G` pattern that is pure
    /// literals (BRE `a\|b` included), and `-E` patterns the regex crate
    /// compiles (GNU-ERE backrefs `\1`..`\9` are refused — they change the
    /// language). `-x` wraps the pattern in line anchors; `-w` stays a
    /// post-filter on the hit ranges.
    pub fn fsearch_grep(&self) -> Option<fsearch::Grep> {
        use fsearch::{Case, Grep, GrepMode};
        let case = if self.ignore_case { Case::Insensitive } else { Case::Sensitive };
        let mut src = match self.syntax {
            Syntax::Fixed => {
                if self.patterns.len() == 1 && !self.line_match {
                    let mut g = Grep::with_case(&self.patterns[0], GrepMode::Literal, case).ok()?;
                    g.max_per_file = usize::MAX;
                    g.budget = None;
                    return Some(g);
                }
                self.patterns.iter().map(|p| regex::escape(p)).collect::<Vec<_>>().join("|")
            }
            Syntax::Basic => {
                let mut lits = Vec::new();
                for p in &self.patterns {
                    lits.extend(bre_literal(p)?);
                }
                lits.iter().map(|p| regex::escape(p)).collect::<Vec<_>>().join("|")
            }
            Syntax::Extended => {
                if self.patterns.iter().any(|p| p.chars().zip(p.chars().skip(1)).any(|(a, b)| a == '\\' && b.is_ascii_digit())) {
                    return None;
                }
                self.patterns.iter().map(|p| format!("(?:{p})")).collect::<Vec<_>>().join("|")
            }
        };
        if self.line_match {
            src = format!("^(?:{src})$");
        }
        let mut g = Grep::with_case(&src, GrepMode::Regex, case).ok()?;
        g.max_per_file = usize::MAX;
        g.budget = None;
        g.before = self.before;
        g.after = self.after;
        if let Some(m) = self.max_count {
            g.max_per_file = m;
        }
        Some(g)
    }
}

/// A BRE pattern reduced to the literals it matches, when every top-level
/// `\|` branch is itself a pure literal (`foo\|bar` -> [foo, bar]).
/// `None` when any branch has an active BRE metacharacter.
fn bre_literal(p: &str) -> Option<Vec<String>> {
    let branches = split_bre_alternatives(p)?;
    let mut out = Vec::new();
    for b in &branches {
        out.push(bre_branch_literal(b)?);
    }
    Some(out)
}

/// One BRE branch as a literal string. BRE metacharacters: an unescaped
/// `.` `*` `[` anywhere, `^` at the start, `$` at the end, and the escapes
/// `\(` `\)` `\{` `\|` `\+` `\?` `\<` `\>` `\b` `\B` `\w` `\W`
/// `\s` `\S` `` \` `` `\'` `\N` (digit). Any other `\x` is the literal
/// `x`. Plain `(` `)` `{` `}` `|` `+` `?` are literal characters in a BRE.
fn bre_branch_literal(b: &str) -> Option<String> {
    let cs: Vec<char> = b.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\\' => {
                let n = *cs.get(i + 1)?;
                match n {
                    '.' | '*' | '^' | '$' | '[' | ']' | '/' | '-' | '&' | '~' | '#' | '@' | '!' | '%' | ':' | ';' | ',' | '=' | '"' | '_' => out.push(n),
                    '(' | ')' | '{' | '}' | '|' | '+' | '?' | '<' | '>' | 'b' | 'B' | 'w' | 'W' | 's' | 'S' | '`' | '\'' => return None,
                    c if c.is_ascii_digit() => return None,
                    '\\' => out.push('\\'),
                    _ => out.push(n),
                }
                i += 2;
            }
            '.' | '*' | '[' => return None,
            '^' if i == 0 => return None,
            '$' if i == cs.len() - 1 => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// Parse a grep argv (without argv[0]) into a plan, or the reason to decline.
pub fn grep_plan(args: &[String]) -> Result<GrepPlan, &'static str> {
    let mut argv: Vec<String> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut syntax: Option<Syntax> = None;
    let (mut ignore_case, mut recursive, mut with_name) = (false, false, None);
    let (mut line_number, mut only_matching, mut count, mut files_only) = (false, false, false, false);
    let (mut word, mut line_match, mut quiet, mut no_messages, mut ignore_binary) = (false, false, false, false, false);
    let (mut before, mut after, mut max_count, mut color_always) = (0usize, 0usize, None, false);
    let mut walk = Walk::default();
    let mut set_syntax = |s: Syntax| match syntax {
        Some(old) if old != s => Err("conflicting matchers"),
        _ => {
            syntax = Some(s);
            Ok(())
        }
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        if a == "--" {
            return Err("end of options");
        }
        if let Some(long) = a.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (long, None),
            };
            // Options that take a value: `--name=V` or `--name V`.
            let take = |i: &mut usize| -> Result<String, &'static str> {
                if let Some(v) = value.clone() {
                    return Ok(v);
                }
                let v = args.get(*i).cloned().ok_or("option needs a value")?;
                *i += 1;
                Ok(v)
            };
            match name {
                "ignore-case" => {
                    ignore_case = true;
                    argv.push(a.to_string());
                }
                "word-regexp" => {
                    word = true;
                    argv.push(a.to_string());
                }
                "line-regexp" => {
                    line_match = true;
                    argv.push(a.to_string());
                }
                "line-number" => {
                    line_number = true;
                    argv.push(a.to_string());
                }
                "count" => {
                    count = true;
                    argv.push(a.to_string());
                }
                "files-with-matches" => {
                    files_only = true;
                    argv.push(a.to_string());
                }
                "only-matching" => {
                    only_matching = true;
                    argv.push(a.to_string());
                }
                "no-messages" => {
                    no_messages = true;
                    argv.push(a.to_string());
                }
                "quiet" | "silent" => {
                    quiet = true;
                    argv.push(a.to_string());
                }
                "binary-files" => {
                    match value.as_deref() {
                        Some("without-match") => {
                            ignore_binary = true;
                            argv.push(a.to_string());
                        }
                        Some("binary") => argv.push(a.to_string()),
                        _ => return Err("binary-files mode"),
                    }
                }
                "with-filename" => {
                    with_name = Some(true);
                    argv.push(a.to_string());
                }
                "no-filename" => {
                    with_name = Some(false);
                    argv.push(a.to_string());
                }
                "fixed-strings" => {
                    set_syntax(Syntax::Fixed)?;
                    argv.push(a.to_string());
                }
                "extended-regexp" => {
                    set_syntax(Syntax::Extended)?;
                    argv.push(a.to_string());
                }
                "basic-regexp" => {
                    set_syntax(Syntax::Basic)?;
                    argv.push(a.to_string());
                }
                "recursive" => recursive = true,
                "include" | "exclude" | "exclude-dir" => {
                    let g = take(&mut i)?;
                    let m = glob(&g)?;
                    // The flags stay in the served argv too: they filter
                    // pass-through operands identically and are a no-op on
                    // candidates the walk filter already passed.
                    argv.push(format!("--{name}={g}"));
                    match name {
                        "include" => walk.rules.push((true, m)),
                        "exclude" => walk.rules.push((false, m)),
                        _ => walk.exclude_dirs.push(m),
                    }
                }
                "regexp" => patterns.push(take(&mut i)?),
                "after-context" | "before-context" | "context" | "max-count" => {
                    let v = take(&mut i)?;
                    let n: usize = v.parse().map_err(|_| "numeric option")?;
                    argv.push(format!("--{name}={v}"));
                    match name {
                        "after-context" => after = n,
                        "before-context" => before = n,
                        "context" => {
                            before = n;
                            after = n;
                        }
                        _ => max_count = Some(n),
                    }
                }
                "color" | "colour" => {
                    argv.push(a.to_string());
                    match value.as_deref() {
                        Some("auto") | Some("never") | Some("tty") | Some("if-tty") => {}
                        _ => color_always = true,
                    }
                }
                _ => return Err("unsupported long option"),
            }
            continue;
        }
        if a.len() > 1 && a.starts_with('-') {
            let cluster: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < cluster.len() {
                let c = cluster[k];
                k += 1;
                // Options that take a value: the rest of the word, else the next word.
                if matches!(c, 'A' | 'B' | 'C' | 'm' | 'e') {
                    let rest: String = cluster[k..].iter().collect();
                    let v = if rest.is_empty() {
                        i += 1;
                        args.get(i - 1).cloned().ok_or("option needs a value")?
                    } else {
                        rest
                    };
                    match c {
                        'e' => patterns.push(v),
                        _ => {
                            let n: usize = v.parse().map_err(|_| "numeric option")?;
                            argv.push(format!("-{c}{v}"));
                            match c {
                                'A' => after = n,
                                'B' => before = n,
                                'C' => {
                                    before = n;
                                    after = n;
                                }
                                _ => max_count = Some(n),
                            }
                        }
                    }
                    break;
                }
                match c {
                    'i' => {
                        ignore_case = true;
                        argv.push("-i".into());
                    }
                    'n' => {
                        line_number = true;
                        argv.push("-n".into());
                    }
                    'w' => {
                        word = true;
                        argv.push("-w".into());
                    }
                    'x' => {
                        line_match = true;
                        argv.push("-x".into());
                    }
                    'l' => {
                        files_only = true;
                        argv.push("-l".into());
                    }
                    'c' => {
                        count = true;
                        argv.push("-c".into());
                    }
                    'o' => {
                        only_matching = true;
                        argv.push("-o".into());
                    }
                    's' => {
                        no_messages = true;
                        argv.push("-s".into());
                    }
                    'q' => {
                        quiet = true;
                        argv.push("-q".into());
                    }
                    'I' => {
                        ignore_binary = true;
                        argv.push("-I".into());
                    }
                    'G' => {
                        set_syntax(Syntax::Basic)?;
                        argv.push("-G".into());
                    }
                    'h' => {
                        with_name = Some(false);
                        argv.push("-h".into());
                    }
                    'H' => {
                        with_name = Some(true);
                        argv.push("-H".into());
                    }
                    'E' => {
                        set_syntax(Syntax::Extended)?;
                        argv.push("-E".into());
                    }
                    'F' => {
                        set_syntax(Syntax::Fixed)?;
                        argv.push("-F".into());
                    }
                    'r' => recursive = true,
                    _ => return Err("unsupported option"),
                }
            }
            continue;
        }
        positional.push(a.to_string());
    }
    let operands = if patterns.is_empty() {
        if positional.is_empty() {
            return Err("no pattern");
        }
        patterns.push(positional.remove(0));
        positional
    } else {
        positional
    };
    // `-r` with no operand defaults to `.`; without it a missing operand is
    // stdin, as is an operand `-`.
    let operands = if operands.is_empty() {
        if !recursive {
            return Err("reads stdin");
        }
        vec![".".to_string()]
    } else {
        operands
    };
    if operands.iter().any(|o| o == "-") {
        return Err("reads stdin");
    }
    if patterns.iter().any(|p| p.is_empty() || p.contains('\n')) {
        return Err("empty or multi-line pattern");
    }
    let syntax = syntax.unwrap_or(Syntax::Basic);
    let mut needles = Vec::new();
    let mut all_needled = true;
    for p in &patterns {
        match needles_for(p, syntax, ignore_case) {
            Some(n) => needles.extend(n),
            None => all_needled = false,
        }
    }
    needles.sort();
    needles.dedup();
    for p in &patterns {
        argv.push("-e".into());
        argv.push(p.clone());
    }
    Ok(GrepPlan {
        argv,
        operands,
        needles,
        all_needled,
        ignore_case,
        walk,
        with_name,
        recursive,
        syntax,
        patterns,
        line_number,
        only_matching,
        count,
        files_only,
        word,
        line_match,
        quiet,
        no_messages,
        ignore_binary,
        before,
        after,
        max_count,
        color_always,
    })
}

fn glob(pat: &str) -> Result<GlobMatcher, &'static str> {
    Ok(Glob::new(pat).map_err(|_| "bad glob")?.compile_matcher())
}

/// One literal per alternative of `pattern` that every match of that
/// alternative must contain. `None` when some alternative has none.
pub fn needles_for(pattern: &str, syntax: Syntax, ignore_case: bool) -> Option<Vec<String>> {
    let branches: Vec<String> = match syntax {
        Syntax::Fixed => vec![pattern.to_string()],
        Syntax::Basic => split_bre_alternatives(pattern)?,
        Syntax::Extended => split_alternatives(pattern)?,
    };
    let mut out = Vec::new();
    for b in branches {
        let runs = if syntax == Syntax::Fixed { vec![b] } else { required_runs(&b, syntax == Syntax::Extended)? };
        let best = runs
            .into_iter()
            .filter(|r| r.len() >= MIN_NEEDLE && (!ignore_case || folds_safely(r)))
            .max_by_key(|r| r.len())?;
        out.push(best);
    }
    Some(out)
}

/// Under `-i`, a needle must fold the same way in the index as in the tool:
/// ASCII only, and free of the letters whose folds reach beyond ASCII (or
/// whose upper case a locale folds to them).
fn folds_safely(needle: &str) -> bool {
    needle.is_ascii() && !needle.chars().any(|c| matches!(c.to_ascii_lowercase(), 'i' | 'k' | 's'))
}

/// Split an ERE at its top-level `|`. `None` if the parentheses do not
/// balance (the real tool reports that).
fn split_alternatives(p: &str) -> Option<Vec<String>> {
    let cs: Vec<char> = p.chars().collect();
    let mut out = vec![String::new()];
    let mut depth = 0usize;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\\' => {
                out.last_mut()?.push(c);
                out.last_mut()?.push(*cs.get(i + 1)?);
                i += 2;
                continue;
            }
            '[' => {
                let end = bracket_end(&cs, i)?;
                out.last_mut()?.extend(&cs[i..=end]);
                i = end + 1;
                continue;
            }
            '(' => depth += 1,
            ')' => depth = depth.checked_sub(1)?,
            '|' if depth == 0 => {
                out.push(String::new());
                i += 1;
                continue;
            }
            _ => {}
        }
        out.last_mut()?.push(c);
        i += 1;
    }
    (depth == 0).then_some(out)
}

/// Split a BRE at its top-level `\\|` (GNU alternation). `\\(`/`\\)`
/// groups count toward depth, so an alternation inside one stays in the
/// branch and `required_runs` declines it there. `None` when escapes or
/// groups do not balance (the real tool reports that).
fn split_bre_alternatives(p: &str) -> Option<Vec<String>> {
    let cs: Vec<char> = p.chars().collect();
    let mut out = vec![String::new()];
    let mut depth = 0usize;
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '[' => {
                let end = bracket_end(&cs, i)?;
                out.last_mut()?.extend(&cs[i..=end]);
                i = end + 1;
                continue;
            }
            '\\' => {
                let n = *cs.get(i + 1)?;
                match n {
                    '(' => depth += 1,
                    ')' => depth = depth.checked_sub(1)?,
                    '|' if depth == 0 => {
                        out.push(String::new());
                        i += 2;
                        continue;
                    }
                    _ => {}
                }
                out.last_mut()?.push('\\');
                out.last_mut()?.push(n);
                i += 2;
                continue;
            }
            _ => {}
        }
        out.last_mut()?.push(cs[i]);
        i += 1;
    }
    (depth == 0).then_some(out)
}

/// The literal runs every match must contain, at the top level of one
/// alternative. Anything that can match other text (classes, groups, `.`,
/// anchors, optional atoms) ends a run; a group's content is not required.
/// `None` for syntax it does not model, so the caller declines.
fn required_runs(p: &str, ere: bool) -> Option<Vec<String>> {
    let cs: Vec<char> = p.chars().collect();
    let mut runs: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            '\\' => {
                let n = *cs.get(i + 1)?;
                // Escaped punctuation is literal, except the BRE operators.
                if !n.is_ascii_punctuation() || (!ere && "(){}|+?".contains(n)) {
                    return None;
                }
                push_literal(n, cs.get(i + 2), ere, &mut cur, &mut runs);
                i += 2;
            }
            '[' => {
                runs.push(std::mem::take(&mut cur));
                i = bracket_end(&cs, i)? + 1;
            }
            '(' if ere => {
                runs.push(std::mem::take(&mut cur));
                i = group_end(&cs, i)? + 1;
            }
            '{' | '}' | ')' if ere => return None,
            '.' | '^' | '$' | '*' => {
                runs.push(std::mem::take(&mut cur));
                i += 1;
            }
            '+' | '?' | '|' if ere => {
                runs.push(std::mem::take(&mut cur));
                i += 1;
            }
            // In a BRE, `( ) { } | + ?` are plain characters.
            _ => {
                push_literal(c, cs.get(i + 1), ere, &mut cur, &mut runs);
                i += 1;
            }
        }
    }
    runs.push(cur);
    Some(runs.into_iter().filter(|r| !r.is_empty()).collect())
}

/// Add a literal char to the run, unless the quantifier after it makes it
/// optional (then the run ends before it). An ERE `+` keeps it required but
/// ends the run after it.
fn push_literal(c: char, next: Option<&char>, ere: bool, cur: &mut String, runs: &mut Vec<String>) {
    let optional = next == Some(&'*') || (ere && matches!(next, Some('?' | '{')));
    if optional {
        runs.push(std::mem::take(cur));
        return;
    }
    cur.push(c);
    if ere && next == Some(&'+') {
        runs.push(std::mem::take(cur));
    }
}

/// Index of the `]` closing the bracket expression opening at `open`.
fn bracket_end(cs: &[char], open: usize) -> Option<usize> {
    let mut i = open + 1;
    if cs.get(i) == Some(&'^') {
        i += 1;
    }
    if cs.get(i) == Some(&']') {
        i += 1;
    }
    while i < cs.len() {
        match cs[i] {
            '[' if matches!(cs.get(i + 1), Some(':' | '.' | '=')) => {
                let delim = cs[i + 1];
                let close = (i + 2..cs.len().saturating_sub(1)).find(|&j| cs[j] == delim && cs[j + 1] == ']')?;
                i = close + 2;
            }
            ']' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Index of the `)` closing the group opening at `open` (ERE).
fn group_end(cs: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < cs.len() {
        match cs[i] {
            '\\' => i += 1,
            '[' => i = bracket_end(cs, i)?,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------
// `rg` plans: ripgrep is a different tool. The same serving modes apply,
// but the walk, the output shapes, the flag grammar and the error forms
// are ripgrep's own — none of it is interchangeable with GNU grep.
// ---------------------------------------------------------------------

/// Case handling: the last of `-i`/`-s`/`-S` wins. `Smart` resolves when
/// the matcher is built: insensitive iff no pattern has an uppercase
/// character.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CaseMode {
    Sensitive,
    Insensitive,
    Smart,
}

/// Which kind of output the call produces: the last of `-c`/`--count`,
/// `--count-matches`, `-l`/`--files-with-matches` decides (`-lc` counts,
/// `-cl` lists). `-o` is not a mode; under `Count` it counts matches, not
/// lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RgMode {
    Lines,
    Count,
    Files,
}

/// An `rg` invocation the index can serve, in the spirit of [`GrepPlan`]:
/// the argv a pass-through exec needs, the operands, a pruning literal
/// per pattern, and every flag the walker or the emitter honors. `rg`
/// recurses into any directory operand by default; there is no `-r`.
pub struct RgPlan {
    /// Output flags and the patterns as `-e` values — everything the real
    /// tool needs minus the operands. `-u`, `-g` and friends stay too:
    /// they re-filter exec-mode operands harmlessly.
    pub argv: Vec<String>,
    /// The operands, in argv order. Directory operands under the root are
    /// served; files and missing paths pass through.
    pub operands: Vec<String>,
    /// Every pattern (`-e` values, or the positional one).
    pub patterns: Vec<String>,
    /// `-F`/`--fixed-strings` selected.
    pub fixed: bool,
    /// Last of `-i`/`-s`/`-S`.
    pub case: CaseMode,
    /// Each matching line of the real tool holds one of these, literally.
    /// Meaningful only when `all_needled`.
    pub needles: Vec<String>,
    /// Every pattern contributed at least one required literal.
    pub all_needled: bool,
    /// `-H`/`-I` as given; `None` is rg's default.
    pub with_name: Option<bool>,
    /// `-n`/`-N` as given. Piped output defaults to no numbers;
    /// `--column`/`--vimgrep` imply `-n`.
    pub line_number: Option<bool>,
    /// `--column`/`--vimgrep`: a byte column after the number.
    pub column: bool,
    /// `--vimgrep`: one `path:line:col:text` row per match.
    pub vimgrep: bool,
    /// `-o`.
    pub only_matching: bool,
    /// Last of `-c`/`--count`/`--count-matches`/`-l`.
    pub mode: RgMode,
    /// `-w`.
    pub word: bool,
    /// `-x`.
    pub line_match: bool,
    /// `-q`/`--quiet`.
    pub quiet: bool,
    /// `--no-messages`.
    pub no_messages: bool,
    /// `--heading`/`--no-heading` as given.
    pub heading: Option<bool>,
    /// `-0`/`--null`: a NUL between the filename and the fields.
    pub null_sep: bool,
    pub before: usize,
    pub after: usize,
    pub max_count: Option<usize>,
    /// `--hidden`/`--no-hidden`, or the implicit `--hidden` of `-uu`.
    pub hidden: Option<bool>,
    /// `--no-ignore`, or the first `-u`: no ignore files apply at all.
    pub no_ignore: bool,
    pub no_ignore_dot: bool,
    pub no_ignore_vcs: bool,
    pub no_ignore_parent: bool,
    pub no_ignore_global: bool,
    pub no_ignore_exclude: bool,
    /// `-g`/`--glob`/`--iglob` values in command order, `!` negations kept.
    pub globs: Vec<String>,
    /// `--ignore-file` paths.
    pub ignore_files: Vec<String>,
    /// `--max-depth`.
    pub max_depth: Option<usize>,
    /// `--max-filesize`, bytes resolved (`10K` → 10240).
    pub max_filesize: Option<u64>,
    /// `--no-require-git` flips this off; gitignores apply everywhere.
    pub require_git: bool,
    /// `--path-separator` replaces `/` in display paths.
    pub path_separator: Option<String>,
    /// `--color=always` (or a bare `--color`): ANSI the answer path
    /// cannot emit.
    pub color_always: bool,
    /// A flag was accepted that stays correct in exec mode but cannot be
    /// rendered exactly: `-r`/`--replace`, `-b`/`--byte-offset`,
    /// `-M`/`--max-columns`, `-U`/`--multiline`, `-P`/`--pcre2`,
    /// `-a`/`--text`, `--binary`, `--sort`/`--sortr`, or the third `-u`.
    pub exec_only: bool,
}

impl RgPlan {
    /// The index can print the real tool's bytes for this plan: the
    /// pattern set compiles to one rust-regex (rg's own engine), no ANSI
    /// colors, and no exec-only shaping.
    pub fn answerable(&self) -> bool {
        !self.color_always && !self.exec_only && self.fsearch_grep().is_some()
    }

    /// Whether the filename column shows, given the operand shape: `-H`
    /// wins, `-I` loses, and otherwise it is on when more than one path
    /// was given or any operand is a directory.
    pub fn show_name(&self, operand_count: usize, any_dir: bool) -> bool {
        self.with_name.unwrap_or(operand_count > 1 || any_dir)
    }

    /// Whether matching folds case (`-i`, or `-S` with an all-lowercase
    /// pattern set — the same resolution `fsearch_grep` makes).
    pub fn insensitive(&self) -> bool {
        match self.case {
            CaseMode::Insensitive => true,
            CaseMode::Sensitive => false,
            CaseMode::Smart => self
                .patterns
                .iter()
                .all(|p| !p.chars().any(char::is_uppercase)),
        }
    }

    /// Line numbers: `--column`/`--vimgrep` imply them; otherwise `-n`
    /// when given (piped rg defaults them off).
    pub fn numbered(&self) -> bool {
        self.line_number.unwrap_or(self.column || self.vimgrep)
    }

    /// The patterns as one `fsearch::Grep`, when the index can run them
    /// exactly: `-F` literals escaped, else the union rg itself builds
    /// (`(?:p1)|(?:p2)`), `-x` line-anchored, `-w` half-word-boundary
    /// wrapped — the same wraps rg applies to its patterns.
    pub fn fsearch_grep(&self) -> Option<fsearch::Grep> {
        use fsearch::{Case, Grep, GrepMode};
        let case = match self.case {
            CaseMode::Insensitive => Case::Insensitive,
            CaseMode::Sensitive => Case::Sensitive,
            CaseMode::Smart => {
                if self.patterns.iter().all(|p| !p.chars().any(char::is_uppercase)) {
                    Case::Insensitive
                } else {
                    Case::Sensitive
                }
            }
        };
        let mut src = if self.fixed {
            self.patterns
                .iter()
                .map(|p| regex::escape(p))
                .collect::<Vec<_>>()
                .join("|")
        } else {
            self.patterns
                .iter()
                .map(|p| format!("(?:{p})"))
                .collect::<Vec<_>>()
                .join("|")
        };
        if self.line_match {
            src = format!("^(?:{src})$");
        }
        if self.word {
            src = format!("\\b{{start-half}}(?:{src})\\b{{end-half}}");
        }
        let mut g = Grep::with_case(&src, GrepMode::Regex, case).ok()?;
        g.max_per_file = usize::MAX;
        g.budget = None;
        g.before = self.before;
        g.after = self.after;
        if let Some(m) = self.max_count {
            g.max_per_file = m;
        }
        Some(g)
    }
}

/// `--max-filesize 10K` and friends: a number with an optional K/M/G/T
/// suffix, or `None` when it does not parse.
fn parse_size(v: &str) -> Option<u64> {
    let (num, mult) = match v.as_bytes().last() {
        Some(b'K') | Some(b'k') => (&v[..v.len() - 1], 1u64 << 10),
        Some(b'M') | Some(b'm') => (&v[..v.len() - 1], 1u64 << 20),
        Some(b'G') | Some(b'g') => (&v[..v.len() - 1], 1u64 << 30),
        Some(b'T') | Some(b't') => (&v[..v.len() - 1], 1u64 << 40),
        _ => (v, 1),
    };
    num.parse::<u64>().ok()?.checked_mul(mult)
}

/// Parse an rg argv (without argv[0]) into a plan, or the reason to
/// decline. Flags that change the file set the index cannot reproduce
/// (`-v`, `-L`, `-z`, `-t`, `-f`, `--files`) or output the emitter does
/// not shape (`--json`, `--stats`, custom separators, `--replace` in
/// answer mode only) decline the whole call; the real tool then runs on
/// the original argv.
pub fn rg_plan(args: &[String]) -> Result<RgPlan, &'static str> {
    let mut argv: Vec<String> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut e_seen = false;
    let mut case = CaseMode::Sensitive;
    let mut fixed = false;
    let mut with_name: Option<bool> = None;
    let mut line_number: Option<bool> = None;
    let (mut column, mut vimgrep, mut only_matching) = (false, false, false);
    let mut mode = RgMode::Lines;
    let (mut word, mut line_match, mut quiet, mut no_messages) = (false, false, false, false);
    let (mut heading, mut null_sep) = (None, false);
    let (mut before, mut after, mut max_count) = (0usize, 0usize, None);
    let (mut unrestricted, mut hidden) = (0u8, None);
    let mut no_ignore = false;
    let (mut no_dot, mut no_vcs, mut no_parent, mut no_global, mut no_exclude) =
        (false, false, false, false, false);
    let (mut globs, mut ignore_files) = (Vec::new(), Vec::new());
    let (mut max_depth, mut max_filesize) = (None, None);
    let (mut require_git, mut path_separator) = (true, None);
    let (mut color_always, mut exec_only) = (false, false);
    // The `-u` counter: ignore files off at one, hidden shown at two,
    // `--binary` at three.
    macro_rules! bump_u {
        () => {{
            unrestricted = (unrestricted + 1).min(3);
            no_ignore = true;
            no_dot = false;
            no_vcs = false;
            no_parent = false;
            no_global = false;
            no_exclude = false;
            if unrestricted >= 2 {
                hidden = Some(true);
            }
            if unrestricted >= 3 {
                exec_only = true;
            }
        }};
    }
    let mut i = 0;
    let mut opts_done = false;
    while i < args.len() {
        let a = args[i].as_str();
        i += 1;
        if !opts_done && a == "--" {
            opts_done = true;
            continue;
        }
        if !opts_done && a.len() > 1 && a.starts_with('-') {
            if let Some(long) = a.strip_prefix("--") {
                let (name, value) = match long.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (long, None),
                };
                let take = |i: &mut usize| -> Result<String, &'static str> {
                    if let Some(v) = value.clone() {
                        return Ok(v);
                    }
                    let v = args.get(*i).cloned().ok_or("option needs a value")?;
                    *i += 1;
                    Ok(v)
                };
                // Every accepted flag stays in the argv an exec-mode
                // pass or batch runs — the real tool applies it to the
                // operand it is handed the same way. Value flags push a
                // normalized `--name=value`; `--regexp` patterns ride at
                // the end as `-e` args.
                let flag_only = !matches!(name,
                    "max-depth" | "max-filesize" | "ignore-file" | "glob" | "iglob"
                    | "path-separator" | "regexp" | "after-context" | "before-context"
                    | "context" | "max-count" | "threads" | "dfa-size-limit"
                    | "regex-size-limit" | "engine" | "replace" | "max-columns"
                    | "sort" | "sortr");
                if flag_only {
                    argv.push(a.to_string());
                }
                match name {
                    "ignore-case" => case = CaseMode::Insensitive,
                    "case-sensitive" => case = CaseMode::Sensitive,
                    "smart-case" => case = CaseMode::Smart,
                    "fixed-strings" => fixed = true,
                    "no-fixed-strings" => fixed = false,
                    "word-regexp" => word = true,
                    "no-word-regexp" => word = false,
                    "line-regexp" => line_match = true,
                    "no-line-regexp" => line_match = false,
                    "line-number" => line_number = Some(true),
                    "no-line-number" => line_number = Some(false),
                    "column" => column = true,
                    "no-column" => column = false,
                    "vimgrep" => vimgrep = true,
                    "only-matching" => only_matching = true,
                    "count" => mode = RgMode::Count,
                    "count-matches" => {
                        mode = RgMode::Count;
                        only_matching = true;
                    }
                    "files-with-matches" => mode = RgMode::Files,
                    "quiet" => quiet = true,
                    "no-messages" => no_messages = true,
                    "messages" => no_messages = false,
                    "heading" => heading = Some(true),
                    "no-heading" => heading = Some(false),
                    "null" => null_sep = true,
                    "with-filename" => with_name = Some(true),
                    "no-filename" => with_name = Some(false),
                    "unrestricted" => bump_u!(),
                    "hidden" => hidden = Some(true),
                    "no-hidden" => hidden = Some(false),
                    "no-ignore" => {
                        no_ignore = true;
                        no_dot = false;
                        no_vcs = false;
                        no_parent = false;
                        no_global = false;
                        no_exclude = false;
                    }
                    "ignore" => no_ignore = false,
                    "no-ignore-dot" => no_dot = true,
                    "no-ignore-vcs" => {
                        no_vcs = true;
                        no_exclude = true;
                    }
                    "no-ignore-parent" => no_parent = true,
                    "no-ignore-global" => no_global = true,
                    "no-ignore-exclude" => no_exclude = true,
                    "no-require-git" => require_git = false,
                    "require-git" => require_git = true,
                    "max-depth" => {
                        let v = take(&mut i)?;
                        max_depth = Some(v.parse().map_err(|_| "numeric option")?);
                        argv.push(format!("--max-depth={v}"));
                    }
                    "max-filesize" => {
                        let v = take(&mut i)?;
                        max_filesize = Some(parse_size(&v).ok_or("bad filesize")?);
                        argv.push(format!("--max-filesize={v}"));
                    }
                    "ignore-file" => {
                        let v = take(&mut i)?;
                        ignore_files.push(v.clone());
                        argv.push(format!("--ignore-file={v}"));
                    }
                    "glob" | "iglob" => {
                        let v = take(&mut i)?;
                        globs.push(v.clone());
                        argv.push(format!("--{name}={v}"));
                    }
                    "path-separator" => {
                        let v = take(&mut i)?;
                        path_separator = Some(v.clone());
                        argv.push(format!("--path-separator={v}"));
                    }
                    "regexp" => {
                        e_seen = true;
                        patterns.push(take(&mut i)?);
                    }
                    "after-context" | "before-context" | "context" | "max-count" => {
                        let v = take(&mut i)?;
                        let n: usize = v.parse().map_err(|_| "numeric option")?;
                        argv.push(format!("--{name}={v}"));
                        match name {
                            "after-context" => after = n,
                            "before-context" => before = n,
                            "context" => {
                                before = n;
                                after = n;
                            }
                            _ => max_count = Some(n),
                        }
                    }
                    // Accepted without effect on what is printed; they
                    // only tune how the real tool works.
                    "threads" | "dfa-size-limit" | "regex-size-limit" => {
                        argv.push(format!("--{name}={}", take(&mut i)?));
                    }
                    "mmap" | "no-mmap" | "line-buffered" | "block-buffered" | "no-follow"
                    | "no-encoding" | "auto-hybrid-regex" | "no-auto-hybrid-regex" => {}
                    "engine" => {
                        let v = take(&mut i)?;
                        match v.as_str() {
                            "default" | "auto" => argv.push(format!("--engine={v}")),
                            _ => return Err("pcre2 engine"),
                        }
                    }
                    // Real-tool work the index cannot print: correct to
                    // serve the candidate files, never the bytes.
                    "replace" | "max-columns" => {
                        let v = take(&mut i)?;
                        exec_only = true;
                        argv.push(format!("--{name}={v}"));
                    }
                    "byte-offset" | "multiline" | "multiline-dotall" | "pcre2" | "text"
                    | "binary" | "no-binary" | "no-text" | "no-unicode" => exec_only = true,
                    "sort" | "sortr" => {
                        let v = take(&mut i)?;
                        exec_only = true;
                        argv.push(format!("--{name}={v}"));
                    }
                    "color" | "colour" => {
                        if !matches!(value.as_deref(), Some("auto") | Some("never")) {
                            color_always = true;
                        }
                    }
                    _ => return Err("unsupported long option"),
                }
                continue;
            }
            let cluster: Vec<char> = a[1..].chars().collect();
            let mut k = 0;
            while k < cluster.len() {
                let c = cluster[k];
                k += 1;
                // Options that take a value: the rest of the word, else the
                // next word.
                if matches!(c, 'A' | 'B' | 'C' | 'm' | 'e' | 'g' | 'j' | 'r' | 'M' | 'f' | 't' | 'T' | 'E') {
                    let rest: String = cluster[k..].iter().collect();
                    let v = if rest.is_empty() {
                        i += 1;
                        args.get(i - 1).cloned().ok_or("option needs a value")?
                    } else {
                        rest
                    };
                    match c {
                        'e' => {
                            e_seen = true;
                            patterns.push(v);
                        }
                        'g' => {
                            globs.push(v.clone());
                            argv.push(format!("-g{v}"));
                        }
                        'A' | 'B' | 'C' | 'm' => {
                            let n: usize = v.parse().map_err(|_| "numeric option")?;
                            argv.push(format!("-{c}{v}"));
                            match c {
                                'A' => after = n,
                                'B' => before = n,
                                'C' => {
                                    before = n;
                                    after = n;
                                }
                                _ => max_count = Some(n),
                            }
                        }
                        'j' => argv.push(format!("-j{v}")),
                        'r' | 'M' => {
                            exec_only = true;
                            argv.push(format!("-{c}{v}"));
                        }
                        _ => return Err("pattern file / type / encoding"),
                    }
                    break;
                }
                match c {
                    'i' => case = CaseMode::Insensitive,
                    's' => case = CaseMode::Sensitive,
                    'S' => case = CaseMode::Smart,
                    'F' => fixed = true,
                    'w' => word = true,
                    'x' => line_match = true,
                    'n' => line_number = Some(true),
                    'N' => line_number = Some(false),
                    'o' => only_matching = true,
                    'c' => mode = RgMode::Count,
                    'l' => mode = RgMode::Files,
                    'q' => quiet = true,
                    'u' => bump_u!(),
                    '0' => null_sep = true,
                    'H' => with_name = Some(true),
                    'I' => with_name = Some(false),
                    'a' | 'b' | 'P' | 'U' => exec_only = true,
                    _ => return Err("unsupported short flag"),
                }
                argv.push(format!("-{c}"));
            }
            continue;
        }
        positional.push(a.to_string());
    }
    if patterns.is_empty() {
        if e_seen {
            return Err("no pattern");
        }
        let Some(first) = positional.first().cloned() else {
            return Err("stdin search");
        };
        patterns.push(first);
        positional.remove(0);
    }
    let insensitive = match case {
        CaseMode::Insensitive => true,
        CaseMode::Sensitive => false,
        CaseMode::Smart => patterns.iter().all(|p| !p.chars().any(char::is_uppercase)),
    };
    let syntax = if fixed { Syntax::Fixed } else { Syntax::Extended };
    let mut needles = Vec::new();
    let mut all_needled = true;
    for p in &patterns {
        match needles_for(p, syntax, insensitive) {
            Some(ns) if !ns.is_empty() => needles.extend(ns),
            _ => all_needled = false,
        }
    }
    for p in &patterns {
        argv.push("-e".into());
        argv.push(p.clone());
    }
    Ok(RgPlan {
        argv,
        operands: positional,
        patterns,
        fixed,
        case,
        needles,
        all_needled,
        with_name,
        line_number,
        column,
        vimgrep,
        only_matching,
        mode,
        word,
        line_match,
        quiet,
        no_messages,
        heading,
        null_sep,
        before,
        after,
        max_count,
        hidden,
        no_ignore,
        no_ignore_dot: no_dot,
        no_ignore_vcs: no_vcs,
        no_ignore_parent: no_parent,
        no_ignore_global: no_global,
        no_ignore_exclude: no_exclude,
        globs,
        ignore_files,
        max_depth,
        max_filesize,
        require_git,
        path_separator,
        color_always,
        exec_only,
    })
}

#[cfg(test)]
#[path = "search_plan_tests.rs"]
mod tests;

