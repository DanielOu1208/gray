use std::io::Write;
use std::path::{Path, PathBuf};

pub const ISSUES_NEW_URL: &str = "https://github.com/vstaln/gray/issues/new";
const MAX_URL_BODY_CHARS: usize = 3000;
const MAX_TITLE_CHARS: usize = 80;

pub fn build_title(text: &str) -> String {
    for line in text.lines() {
        let clean = line.trim();
        if clean.is_empty() {
            continue;
        }
        if clean.chars().count() <= MAX_TITLE_CHARS {
            return clean.to_string();
        }
        let prefix: String = clean.chars().take(MAX_TITLE_CHARS).collect();
        let cut = prefix
            .rfind(' ')
            .map(|i| prefix[..i].to_string())
            .unwrap_or(prefix);
        return format!("{}…", cut.trim_end());
    }
    "Feedback".to_string()
}

pub fn terminal_label(term_program: Option<&str>, term: Option<&str>) -> String {
    for s in [term_program, term].into_iter().flatten() {
        let s = s.trim();
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "unknown".to_string()
}

pub fn shell_label(shell: Option<&str>) -> String {
    match shell.map(str::trim) {
        Some(s) if !s.is_empty() => s.rsplit('/').next().unwrap_or(s).to_string(),
        _ => "unknown".to_string(),
    }
}

pub fn build_body(
    text: &str,
    version: &str,
    os: &str,
    model: &str,
    session: &str,
    terminal: &str,
    shell: &str,
) -> String {
    let title = build_title(text);
    format!(
        "## Summary\n{title}\n\n## Expected Behavior\n—\n\n## Actual Behavior\n{}\n\n## Steps to Reproduce\n—\n\nGray Version: {version}\nOperating System: {os}\nTerminal: {terminal}\nShell: {shell}\nModel: {model}\nSession: {session}\n\n## Fix prompt\n—\n\n## Additional context\n—\n",
        text.trim()
    )
}

pub fn issue_url(title: &str, body: &str) -> String {

    let home = crate::setup::gray_home().unwrap_or_default();
    let body = crate::cron_fire::redact_secrets(body, &home);
    let short = if body.chars().count() > MAX_URL_BODY_CHARS {
        let cut: String = body.chars().take(MAX_URL_BODY_CHARS).collect();
        format!("{cut}\n… [truncated for URL — full text in local file]")
    } else {
        body.to_string()
    };
    let enc = |s: &str| url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
    format!(
        "{ISSUES_NEW_URL}?template=bug_report.yml&title={}&body={}",
        enc(title),
        enc(&short)
    )
}

pub fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

pub fn save_feedback(dir: &Path, title: &str, body: &str, stamp: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let mut reserved: Option<(PathBuf, std::fs::File)> = None;
    for n in 1..100 {
        let path = if n == 1 {
            dir.join(format!("feedback-{stamp}.md"))
        } else {
            dir.join(format!("feedback-{stamp}-{n}.md"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => {
                reserved = Some((path, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    let Some((path, mut f)) = reserved else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "feedback filename space exhausted",
        ));
    };
    writeln!(f, "# {title}")?;
    writeln!(f)?;
    writeln!(f, "{body}")?;
    Ok(path)
}

pub fn open_in_browser(url: &str) {
    use std::process::{Command, Stdio};
    let res = match std::env::consts::OS {
        "macos" => Command::new("open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
        "windows" => Command::new("cmd")
            .args(["/c", "start", "", url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
        _ => Command::new("xdg-open")
            .arg(url)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn(),
    };
    let _ = res;
}

#[path = "feedback_tests.rs"]
#[cfg(test)]
mod tests;
