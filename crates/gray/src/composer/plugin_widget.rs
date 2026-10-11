//! Generic plugin-owned above-editor rows. No subagent dependency or layout here.
use ratatui::text::Line;
use std::time::{Duration, Instant};

#[derive(Debug, Default, PartialEq, serde::Deserialize)]
pub(crate) struct Snapshot {
    pub version: u32,
    pub text: String,
    #[serde(default)]
    pub shimmer_lines: Vec<usize>,
    /// `(line, start, end)` char ranges to shimmer; wins over
    /// `shimmer_lines` (whole-line) for the lines it names.
    #[serde(default)]
    pub shimmer_ranges: Vec<(usize, usize, usize)>,
}

pub(crate) fn lines(
    snapshot: &Snapshot,
    elapsed: Duration,
    max_rows: usize,
    width: usize,
) -> Vec<Line<'static>> {
    if snapshot.version != 1 {
        return Vec::new();
    }
    let dim = ratatui::style::Style::default().fg(crate::theme::theme().text_dim);
    let text = |chars: &[char]| chars.iter().collect::<String>();
    snapshot
        .text
        .lines()
        .take(max_rows.min(12))
        .enumerate()
        .map(|(index, line)| {
            let mut chars: Vec<char> = line
                .chars()
                .filter(|ch| !ch.is_control())
                .take(4096)
                .collect();
            // One column in, like the host's own status rows and prompt; a
            // row wider than the band ends in `…` instead of being clipped.
            let room = width.saturating_sub(1);
            if width > 0 && crate::text_width::chars_width(&chars) > room {
                chars.truncate(crate::text_width::fit_char_count(
                    &chars,
                    room.saturating_sub(1),
                ));
                chars.push('\u{2026}');
            }
            let mut ranges: Vec<(usize, usize)> = snapshot
                .shimmer_ranges
                .iter()
                .filter(|r| r.0 == index)
                .map(|r| (r.1.min(chars.len()), r.2.min(chars.len())))
                .filter(|(start, end)| start < end)
                .collect();
            if ranges.is_empty() && snapshot.shimmer_lines.contains(&index) {
                ranges.push((0, chars.len()));
            }
            let mut spans = vec![ratatui::text::Span::styled(" ", dim)];
            let mut at = 0;
            for (start, end) in ranges {
                if start < at {
                    continue;
                }
                if start > at {
                    spans.push(ratatui::text::Span::styled(text(&chars[at..start]), dim));
                }
                spans.extend(super::draw::shimmer_spans(
                    &text(&chars[start..end]),
                    elapsed,
                ));
                at = end;
            }
            if at < chars.len() {
                spans.push(ratatui::text::Span::styled(text(&chars[at..]), dim));
            }
            Line::from(spans)
        })
        .collect()
}

pub(crate) struct Widget {
    latest: std::sync::mpsc::Receiver<Snapshot>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    snapshot: Snapshot,
    started: Instant,
}
impl Widget {
    pub(crate) fn new(cwd: std::path::PathBuf) -> Self {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let snapshot = read_widget(&cwd).await.unwrap_or_default();
                    if tx.try_send(snapshot).is_err()
                        && flag.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            });
        });
        Self {
            latest: rx,
            stop,
            snapshot: Snapshot::default(),
            started: Instant::now(),
        }
    }
    pub(crate) fn refresh(&mut self) -> bool {
        if let Ok(snapshot) = self.latest.try_recv() {
            let changed = self.snapshot != snapshot;
            self.snapshot = snapshot;
            return changed;
        }
        false
    }
    pub(crate) fn active(&self) -> bool {
        !self.snapshot.text.is_empty()
    }
    pub(crate) fn rows(&self, max_rows: usize, width: usize) -> Vec<Line<'static>> {
        lines(&self.snapshot, self.started.elapsed(), max_rows, width)
    }
}
impl Drop for Widget {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn read_widget(cwd: &std::path::Path) -> anyhow::Result<Snapshot> {
    let home = crate::plugin_cli::home()?;
    read_widget_at(&home, cwd).await
}

async fn read_widget_at(home: &std::path::Path, cwd: &std::path::Path) -> anyhow::Result<Snapshot> {
    let raw = tokio::fs::read(home.join("plugins/widgets.json")).await?;
    let spec: serde_json::Value = serde_json::from_slice(&raw)?;
    let name = spec["name"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing widget owner"))?;
    let registry = gray_plugin::lock::LockFile::load(&gray_plugin::lock::lock_path(home))?;
    let entry = registry
        .plugins
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("widget owner is not registered"))?;
    anyhow::ensure!(
        crate::plugin_cli::enabled(home, name, entry),
        "widget owner is disabled"
    );
    // The slot can select the owner only, never arbitrary argv edited into JSON.
    let argv = entry.cli_argv.as_ref().unwrap_or(&entry.argv);
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("empty widget argv"))?;
    let mut command = tokio::process::Command::new(program);
    command.args(args).arg("widget").current_dir(cwd);
    let bytes = crate::plugin_cli::capture(command, Duration::from_secs(2)).await?;
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(snapshot.version == 1, "unsupported widget snapshot version");
    Ok(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        widgets::{Paragraph, Widget as _},
    };
    #[test]
    fn snapshot_paints_rows_above_input_with_existing_shimmer() {
        let snapshot=Snapshot {version:1, text:"⬢ Agents\n├─ ⬡ Scout  Inspect\n│    ⎿  working…\n└─ ⬡ Reviewer  Review\n     ⎿  reading…".into(),shimmer_lines:vec![1,3],shimmer_ranges:vec![(3,3,13)]};
        let rows = lines(&snapshot, Duration::from_millis(900), 12, 100);
        assert_eq!(rows.len(), 5);
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 10));
        Paragraph::new(rows.clone()).render(Rect::new(0, 0, 100, 5), &mut buf);
        Paragraph::new("› draft survives").render(Rect::new(0, 6, 100, 1), &mut buf);
        let text = |y| (0..100).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert!(text(0).starts_with(" ⬢ Agents"));
        assert!(text(1).contains("⬡ Scout"));
        assert!(text(4).contains("⎿  reading…"));
        assert!(text(6).starts_with("› draft survives"));
        let mut expected = vec![ratatui::text::Span::styled(
            " ",
            ratatui::style::Style::default().fg(crate::theme::theme().text_dim),
        )];
        expected.extend(super::super::draw::shimmer_spans(
            snapshot.text.lines().nth(1).unwrap(),
            Duration::from_millis(900),
        ));
        assert_eq!(rows[1].spans, expected);
        // A range shimmers only its slice: `⬡ Reviewer`, not the task.
        assert_eq!(rows[3].spans[1].content, "└─ ");
        assert_eq!(rows[3].spans.last().unwrap().content, "  Review");
        // Too wide for the band: cut with an ellipsis, never clipped.
        let narrow = lines(&snapshot, Duration::ZERO, 12, 12);
        assert_eq!(narrow[1].to_string(), " ├─ ⬡ Scout…");
    }
    #[test]
    fn invalid_versions_and_control_characters_cannot_escape_widget() {
        assert!(
            lines(
                &Snapshot {
                    version: 2,
                    text: "x".into(),
                    ..Default::default()
                },
                Duration::ZERO,
                12,
                80
            )
            .is_empty()
        );
        let rows = lines(
            &Snapshot {
                version: 1,
                text: "\x1b[2Jhello\nnext".into(),
                ..Default::default()
            },
            Duration::ZERO,
            1,
            80,
        );
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].to_string().contains('\x1b'));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn widget_uses_registered_owner_and_honors_disable() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let exe = home.path().join("sample");
        let program = r#"#!/bin/sh
if [ "$1" = manifest ]; then
 printf '%s\n' '{"name":"sample","version":"1","widget":true,"commands":["/sample"]}'
else
 printf '%s\n' '{"version":1,"text":"Plugin ready","shimmer_lines":[]}'
fi
"#;
        std::fs::write(&exe, program).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::plugin_cli::register_native(home.path(), Some("sample"), &exe, false)
            .await
            .unwrap();
        // Injected slot argv must never be executed; only the registered owner is.
        std::fs::write(
            home.path().join("plugins/widgets.json"),
            r#"{"name":"sample","argv":["/not/an/executable"]}"#,
        )
        .unwrap();
        let snapshot = read_widget_at(home.path(), home.path()).await.unwrap();
        assert_eq!(snapshot.text, "Plugin ready");
        let lock_path = gray_plugin::lock::lock_path(home.path());
        let mut lock = gray_plugin::lock::LockFile::load(&lock_path).unwrap();
        lock.plugins.get_mut("sample").unwrap().enabled = false;
        lock.save(&lock_path).unwrap();
        assert!(read_widget_at(home.path(), home.path()).await.is_err());
        assert!(lines(&snapshot, Duration::ZERO, 0, 80).is_empty());
    }
}
