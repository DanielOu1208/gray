pub const EST_TOKENS_DIVISOR: u64 = 4;

pub const CUT_LINES: &str = "lines";
pub const CUT_BYTES: &str = "bytes";
pub const CUT_NONE: &str = "none";

pub fn est_tokens(bytes: u64) -> u64 {
    bytes / EST_TOKENS_DIVISOR
}

pub const CLASS_RETRIEVAL: &str = "retrieval";
pub const CLASS_ACTION: &str = "action";
pub const CLASS_OTHER: &str = "other";

pub const SELF_REPORTING: &[&str] = &["read"];

pub fn classify(tool: &str) -> &'static str {
    match tool {
        "read" | "grep" | "find" | "ls" => CLASS_RETRIEVAL,
        "edit" | "write" => CLASS_ACTION,
        _ => CLASS_OTHER,
    }
}

pub fn enabled() -> bool {
    matches!(std::env::var("GRAY_TOOL_STATS").as_deref(), Ok("1"))
}

pub struct ToolStats<'a> {
    pub tool: &'a str,

    pub class: &'a str,
    pub path: &'a str,
    pub bytes: u64,
    pub lines: u64,
    pub truncated_by: &'a str,
}

impl ToolStats<'_> {
    pub fn line(&self) -> String {
        format!(
            "tool={} class={} path={} bytes={} lines={} est_tokens={} truncated_by={}",
            self.tool,
            self.class,
            self.path,
            self.bytes,
            self.lines,
            est_tokens(self.bytes),
            self.truncated_by,
        )
    }

    pub fn report(&self) {
        if !enabled() {
            return;
        }
        log::info!(target: "gray_tools", "{}", self.line());
        let Ok(home) = std::env::var("GRAY_HOME") else {
            return;
        };
        let dir = std::path::Path::new(&home).join("logs");
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let rec = serde_json::json!({
            "tool": self.tool, "class": self.class, "path": self.path,
            "bytes": self.bytes, "lines": self.lines,
            "est_tokens": est_tokens(self.bytes),
            "truncated_by": self.truncated_by,
        });
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("tool-stats.jsonl"))
            && let Err(e) = writeln!(f, "{rec}")
        {
            log::debug!(target: "gray_tools", "tool-stats append failed: {e}");
        }
    }
}

#[path = "stats_tests.rs"]
#[cfg(test)]
mod tests;
