use globset::{Glob, GlobMatcher};

const MIN_NEEDLE: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Syntax {
    Basic,
    Extended,
    Fixed,
}

#[derive(Default)]
pub struct Walk {

    rules: Vec<(bool, GlobMatcher)>,
    exclude_dirs: Vec<GlobMatcher>,
}

impl Walk {

    pub fn keeps_file(&self, name: &[u8]) -> bool {
        let Ok(name) = std::str::from_utf8(name) else {
            return false;
        };
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

    pub fn skips_dir(&self, name: &[u8]) -> bool {
        std::str::from_utf8(name).is_ok_and(|n| self.exclude_dirs.iter().any(|m| m.is_match(n)))
    }
}

pub struct GrepPlan {

    pub argv: Vec<String>,

    pub operand: String,

    pub needles: Vec<String>,
    pub ignore_case: bool,
    pub walk: Walk,
}

pub fn grep_plan(args: &[String]) -> Result<GrepPlan, &'static str> {
    let mut argv: Vec<String> = Vec::new();
    let mut patterns: Vec<String> = Vec::new();
    let mut positional: Vec<String> = Vec::new();
    let mut syntax: Option<Syntax> = None;
    let (mut ignore_case, mut recursive, mut with_name) = (false, false, None);
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
                "word-regexp" | "line-regexp" | "line-number" | "files-with-matches"
                | "only-matching" | "no-messages" | "quiet" | "silent" => {
                    argv.push(a.to_string());
                }
                "with-filename" => {
                    with_name = Some(true);
                    argv.push(a.to_string());
                }
                "no-filename" => {
                    with_name = Some(false);
                    argv.push(a.to_string());
                }
                "fixed-strings" => set_syntax(Syntax::Fixed)?,
                "extended-regexp" => set_syntax(Syntax::Extended)?,
                "basic-regexp" => set_syntax(Syntax::Basic)?,
                "recursive" => recursive = true,
                "include" | "exclude" | "exclude-dir" => {
                    let g = take(&mut i)?;
                    let m = glob(&g)?;
                    match name {
                        "include" => walk.rules.push((true, m)),
                        "exclude" => walk.rules.push((false, m)),
                        _ => walk.exclude_dirs.push(m),
                    }
                }
                "regexp" => patterns.push(take(&mut i)?),
                "after-context" | "before-context" | "context" | "max-count" => {
                    let v = take(&mut i)?;
                    if v.parse::<usize>().is_err() {
                        return Err("numeric option");
                    }
                    argv.push(format!("--{name}={v}"));
                }
                "color" | "colour" => argv.push(a.to_string()),
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
                            if v.parse::<usize>().is_err() {
                                return Err("numeric option");
                            }
                            argv.push(format!("-{c}{v}"));
                        }
                    }
                    break;
                }
                match c {
                    'i' => {
                        ignore_case = true;
                        argv.push("-i".into());
                    }
                    'n' | 'w' | 'x' | 'l' | 'o' | 's' | 'q' | 'I' => argv.push(format!("-{c}")),
                    'G' => set_syntax(Syntax::Basic)?,
                    'h' => {
                        with_name = Some(false);
                        argv.push("-h".into());
                    }
                    'H' => {
                        with_name = Some(true);
                        argv.push("-H".into());
                    }
                    'E' => set_syntax(Syntax::Extended)?,
                    'F' => set_syntax(Syntax::Fixed)?,
                    'r' => recursive = true,
                    _ => return Err("unsupported option"),
                }
            }
            continue;
        }
        positional.push(a.to_string());
    }
    if !recursive {
        return Err("not recursive");
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
    let [operand] = operands.as_slice() else {
        return Err("needs exactly one directory operand");
    };
    if patterns.iter().any(|p| p.is_empty() || p.contains('\n')) {
        return Err("empty or multi-line pattern");
    }
    let syntax = syntax.unwrap_or(Syntax::Basic);
    let mut needles = Vec::new();
    for p in &patterns {
        needles.extend(needles_for(p, syntax, ignore_case).ok_or("no required literal")?);
    }
    needles.sort();
    needles.dedup();
    if with_name != Some(false) {
        argv.insert(0, "-H".into());
    }
    for p in patterns {
        argv.push("-e".into());
        argv.push(p);
    }
    Ok(GrepPlan {
        argv,
        operand: operand.clone(),
        needles,
        ignore_case,
        walk,
    })
}

fn glob(pat: &str) -> Result<GlobMatcher, &'static str> {
    Ok(Glob::new(pat).map_err(|_| "bad glob")?.compile_matcher())
}

pub fn needles_for(pattern: &str, syntax: Syntax, ignore_case: bool) -> Option<Vec<String>> {
    let branches: Vec<String> = match syntax {
        Syntax::Fixed => vec![pattern.to_string()],
        Syntax::Basic => vec![pattern.to_string()],
        Syntax::Extended => split_alternatives(pattern)?,
    };
    let mut out = Vec::new();
    for b in branches {
        let runs = if syntax == Syntax::Fixed {
            vec![b]
        } else {
            required_runs(&b, syntax == Syntax::Extended)?
        };
        let best = runs
            .into_iter()
            .filter(|r| r.len() >= MIN_NEEDLE && (!ignore_case || folds_safely(r)))
            .max_by_key(|r| r.len())?;
        out.push(best);
    }
    Some(out)
}

fn folds_safely(needle: &str) -> bool {
    needle.is_ascii()
        && !needle
            .chars()
            .any(|c| matches!(c.to_ascii_lowercase(), 'i' | 'k' | 's'))
}

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

            _ => {
                push_literal(c, cs.get(i + 1), ere, &mut cur, &mut runs);
                i += 1;
            }
        }
    }
    runs.push(cur);
    Some(runs.into_iter().filter(|r| !r.is_empty()).collect())
}

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
                let close = (i + 2..cs.len().saturating_sub(1))
                    .find(|&j| cs[j] == delim && cs[j + 1] == ']')?;
                i = close + 2;
            }
            ']' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

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

#[cfg(test)]
#[path = "search_plan_tests.rs"]
mod tests;
