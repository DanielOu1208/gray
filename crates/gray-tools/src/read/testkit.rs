use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use gray_core::agent::{ToolContext, ToolOutput};
use serde_json::Value;
use tempfile::TempDir;

use crate::Tool;
use crate::read::ReadTool;

pub fn big_enabled() -> bool {
    std::env::var("GRAY_ZOO_BIG").as_deref() == Ok("1")
}

pub const SPARSE_BYTES: u64 = 200 * 1024 * 1024;

pub struct Zoo {

    dir: TempDir,
}

impl Zoo {

    pub fn build() -> std::io::Result<Self> {
        let dir = TempDir::new()?;
        write_fixtures(dir.path(), big_enabled())?;
        Ok(Self { dir })
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    pub async fn read(&self, path: &str, offset: Option<u64>, limit: Option<u64>) -> ToolOutput {
        let mut map = serde_json::Map::new();
        map.insert("path".to_string(), Value::String(path.to_string()));
        if let Some(o) = offset {
            map.insert("offset".to_string(), Value::from(o));
        }
        if let Some(l) = limit {
            map.insert("limit".to_string(), Value::from(l));
        }
        let ctx = ToolContext {
            cwd: self.root(),
            ..ToolContext::default()
        };
        ReadTool::default().execute(&ctx, Value::Object(map)).await
    }
}

pub fn write_fixtures(root: &Path, big: bool) -> std::io::Result<()> {

    let long: Vec<String> = (1..=3000).map(|i| format!("line {i:04}")).collect();
    std::fs::write(root.join("long.txt"), long.join("\n") + "\n")?;

    let lock: Vec<String> = (1..=80_000)
        .map(|i| format!("lock entry {i:06} sha=abcdef"))
        .collect();
    std::fs::write(root.join("lockfile.txt"), lock.join("\n") + "\n")?;

    let head = "!function(e){var t={};";
    let first = format!("{head}{}", "x".repeat(3900 - head.len()));
    assert_eq!(first.len(), 3900);
    std::fs::write(
        root.join("minified.js"),
        format!("{first}\n//# sourceMappingURL=app.js.map\nconsole.log(\"ok\");\nconst x = 1;\n"),
    )?;

    let wide: Vec<String> = (1..=500)
        .map(|i| format!("{:<300}", format!("log line {i:04} ")))
        .collect();
    std::fs::write(root.join("wide.log"), wide.join("\n") + "\n")?;

    std::fs::write(root.join("empty.txt"), b"")?;
    std::fs::write(root.join("crlf.txt"), b"alpha\r\nbeta\r\ngamma\r\n")?;
    std::fs::write(
        root.join("bom.txt"),
        "\u{FEFF}fn main() {}\nprintln!(\"hi\");\n",
    )?;

    let emoji: Vec<String> = (1..=200).map(|_| "\u{1F600}".repeat(100)).collect();
    std::fs::write(root.join("emoji.txt"), emoji.join("\n") + "\n")?;

    std::fs::write(
        root.join("fake.png"),
        "this is plain text wearing a .png extension\nsecond line\n",
    )?;

    let mut real = b"\x89PNG\r\n\x1a\n".to_vec();
    real.extend((0..1024).map(|i| (i % 256) as u8));
    std::fs::write(root.join("real.png"), real)?;

    let tiny: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 2,
        0, 0, 0, 144, 119, 83, 222, 0, 0, 0, 12, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 0, 0,
        3, 1, 1, 0, 201, 254, 146, 239, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    std::fs::write(root.join("tiny.png"), tiny)?;

    let nul: Vec<u8> = (0..4096)
        .map(|i| if i % 8 == 7 { 0 } else { b'A' + (i % 26) as u8 })
        .collect();
    std::fs::write(root.join("nul.bin"), nul)?;

    std::fs::write(
        root.join("Screenshot 3.04\u{202F}PM.png"),
        "screenshot bytes stand-in\n",
    )?;

    std::fs::write(root.join("cafe\u{301}.txt"), "nfd spelling\n")?;
    std::fs::write(root.join("caf\u{e9}.txt"), "nfc spelling\n")?;

    std::fs::write(root.join("AGENTS.md"), "# Agents\n\nRead this first.\n")?;

    if big {
        make_sparse(&root.join("sparse.txt"))?;
    }
    Ok(())
}

pub fn make_sparse(path: &Path) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.seek(SeekFrom::Start(SPARSE_BYTES - 1))?;
    f.write_all(b"x")?;
    Ok(())
}

pub fn assert_golden(actual: &str, expected: &str) {
    if actual == expected {
        return;
    }
    panic!("golden mismatch:\n{}", diff_lines(expected, actual));
}

fn diff_lines(expected: &str, actual: &str) -> String {
    let mut out = String::from("--- expected\n+++ actual\n");
    let exp: Vec<&str> = expected.lines().collect();
    let act: Vec<&str> = actual.lines().collect();
    for (i, (e, a)) in exp.iter().zip(act.iter()).enumerate() {
        if e != a {
            out.push_str(&format!("@@ line {} @@\n- {e}\n+ {a}\n", i + 1));
        }
    }
    let n = exp.len().min(act.len());
    for line in &exp[n..] {
        out.push_str(&format!("- {line}\n"));
    }
    for line in &act[n..] {
        out.push_str(&format!("+ {line}\n"));
    }
    if expected.ends_with('\n') != actual.ends_with('\n') {
        out.push_str(&format!(
            "@@ trailing newline differs: expected {}, actual {} @@\n",
            expected.ends_with('\n'),
            actual.ends_with('\n')
        ));
    }
    out
}
