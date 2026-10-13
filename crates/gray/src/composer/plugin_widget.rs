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
    if snapshot.version != 1 || max_rows == 0 {
        return Vec::new();
    }
    let dim = ratatui::style::Style::default().fg(crate::theme::theme().text_dim);
    let text = |chars: &[char]| chars.iter().collect::<String>();
    // A band shorter than the widget keeps its top rows and spends its last
    // row on how many lines it cut, so a short screen never hides agents
    // without saying so.
    let total = snapshot.text.lines().count();
    let cut = total > max_rows;
    let keep = if cut { max_rows - 1 } else { total };
    let mut rows: Vec<Line<'static>> = snapshot
        .text
        .lines()
        .take(keep)
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
        .collect();
    if cut {
        rows.push(Line::from(ratatui::text::Span::styled(
            format!(" \u{2193} {} more lines", total - keep),
            dim,
        )));
    }
    rows
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
                let mut gate = SpecGate {
                    stamps: [None; 3],
                    argv: None,
                };
                let mut active = false;
                let mut last_spawn = Instant::now() - IDLE_POLL;
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Ok(home) = crate::plugin_cli::home() {
                        let resolved_fresh = gate.refresh(&home);
                        let interval = if active { TICK } else { IDLE_POLL };
                        if let Some(argv) = gate.argv() {
                            if resolved_fresh || last_spawn.elapsed() >= interval {
                                last_spawn = Instant::now();
                                let snapshot = spawn_widget(argv, &cwd).await.unwrap_or_default();
                                active = !snapshot.text.is_empty();
                                if tx.try_send(snapshot).is_err()
                                    && flag.load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    break;
                                }
                            }
                        } else if resolved_fresh {
                            // The slot went away or was disabled: clear the
                            // rows once instead of every tick.
                            active = false;
                            if tx.try_send(Snapshot::default()).is_err()
                                && flag.load(std::sync::atomic::Ordering::Relaxed)
                            {
                                break;
                            }
                        }
                    }
                    tokio::time::sleep(TICK).await;
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

/// Poll cadence: file stats are per tick, subprocess spawns are gated below.
const TICK: Duration = Duration::from_millis(500);
/// How often an empty widget re-asks its plugin for rows. A widget showing
/// rows keeps the per-tick refresh so live state stays live; an empty band
/// only needs to notice work appearing, so it polls far slower — and the
/// slot spec itself is mtime-gated either way, so idle ticks never touch
/// `widgets.json` or the ~42KB lockfile.
const IDLE_POLL: Duration = Duration::from_secs(5);

/// The mtime gate in front of the widget's file reads and lockfile parses.
/// The inputs that decide who runs the slot are `widgets.json` plus the
/// user and project locks `enabled()` consults; while their mtimes stand,
/// the resolved argv stands and the poll skips straight to the subprocess.
struct SpecGate {
    stamps: [Option<std::time::SystemTime>; 3],
    argv: Option<Vec<String>>,
}

impl SpecGate {
    fn refresh(&mut self, home: &std::path::Path) -> bool {
        let mtime =
            |path: std::path::PathBuf| std::fs::metadata(path).and_then(|m| m.modified()).ok();
        // `enabled()` resolves the project lock against the live cwd, so
        // the stamp follows it rather than the dir the widget started in.
        let project = std::env::current_dir()
            .ok()
            .map(|cwd| gray_plugin::lock::project_lock_path(&cwd));
        let stamps = [
            mtime(home.join("plugins/widgets.json")),
            mtime(gray_plugin::lock::lock_path(home)),
            project.and_then(mtime),
        ];
        if stamps == self.stamps {
            return false;
        }
        self.stamps = stamps;
        self.argv = resolve_widget_argv(home).ok();
        true
    }

    fn argv(&self) -> Option<&[String]> {
        self.argv.as_deref()
    }
}

/// The registered argv of the plugin that owns the widget slot:
/// `widgets.json` selects the owner — never arbitrary argv edited into
/// JSON, the lock entry supplies the command — and the owner must be
/// `enabled`. Sync file reads only; `SpecGate` keeps it off the idle path.
fn resolve_widget_argv(home: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let raw = std::fs::read(home.join("plugins/widgets.json"))?;
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
    Ok(entry.cli_argv.clone().unwrap_or_else(|| entry.argv.clone()))
}

/// `<argv> widget` emits one snapshot object and exits — the only source of
/// widget content, so spawning stays the model.
async fn spawn_widget(argv: &[String], cwd: &std::path::Path) -> anyhow::Result<Snapshot> {
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
async fn read_widget_at(home: &std::path::Path, cwd: &std::path::Path) -> anyhow::Result<Snapshot> {
    let argv = resolve_widget_argv(home)?;
    spawn_widget(&argv, cwd).await
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
    /// The subagents plugin's text for eight running runs, as `lines()` reads
    /// it: a blank lead row, the count header, five agents at two rows each,
    /// the overflow line, a blank and the affordance (15 rows).
    fn eight_running() -> Snapshot {
        let mut rows = vec![String::new(), "8 subagents \u{b7} 8 running".to_string()];
        for n in 1..=5 {
            rows.push(format!("  \u{25cf} agent-{n}  Haiku 5.5  1m  Task {n}"));
            rows.push(format!("      step {n}"));
        }
        rows.push("  +3 more".into());
        rows.push(String::new());
        rows.push("   /subagents \u{2014} open \u{b7} chat \u{b7} stop".into());
        Snapshot {
            version: 1,
            text: rows.join("\n") + "\n",
            ..Default::default()
        }
    }

    #[test]
    fn a_band_shorter_than_the_widget_says_how_much_it_cut() {
        let rows = lines(&eight_running(), Duration::ZERO, 9, 80);
        assert_eq!(rows.len(), 9);
        assert_eq!(rows[1].to_string(), " 8 subagents \u{b7} 8 running");
        // Eight rows of the widget are shown; the ninth names the other seven.
        assert_eq!(rows[8].to_string(), " \u{2193} 7 more lines");
        assert!(lines(&eight_running(), Duration::ZERO, 0, 80).is_empty());
    }

    #[test]
    fn a_band_tall_enough_shows_the_whole_widget_without_an_indicator() {
        let rows = lines(&eight_running(), Duration::ZERO, 16, 80);
        assert_eq!(rows.len(), 15);
        assert!(rows[12].to_string().contains("+3 more"));
        assert!(rows[14].to_string().contains("/subagents"));
        assert!(rows.iter().all(|r| !r.to_string().contains('\u{2193}')));
    }

    #[test]
    fn rows_use_the_real_width_and_cut_with_an_ellipsis() {
        let snapshot = Snapshot {
            version: 1,
            text: format!("{}\n", "word ".repeat(30).trim_end()),
            ..Default::default()
        };
        // 149 cells fit a 200-column band (one cell of inset): no cut.
        let wide = lines(&snapshot, Duration::ZERO, 4, 200)[0].to_string();
        assert!(wide.ends_with("word"), "{wide}");
        assert!(!wide.contains('\u{2026}'), "{wide}");
        // At 80 columns the same row is cut to exactly the width.
        let narrow = lines(&snapshot, Duration::ZERO, 4, 80)[0].to_string();
        assert!(narrow.ends_with('\u{2026}'), "{narrow}");
        assert_eq!(crate::text_width::display_width(&narrow), 80);
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

    #[cfg(unix)]
    #[tokio::test]
    async fn spec_gate_holds_resolution_until_inputs_move() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let exe = home.path().join("sample");
        let program = r#"#!/bin/sh
if [ "$1" = manifest ]; then
 printf '%s\n' '{"name":"sample","version":"1","widget":true,"commands":["/sample"]}'
else
 printf '%s\n' '{"version":1,"text":"hi","shimmer_lines":[]}'
fi
"#;
        std::fs::write(&exe, program).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::plugin_cli::register_native(home.path(), Some("sample"), &exe, false)
            .await
            .unwrap();
        let mut gate = SpecGate {
            stamps: [None; 3],
            argv: None,
        };
        // First look resolves the slot; nothing moving means no re-resolve
        // and no re-read of widgets.json or lock.json.
        assert!(gate.refresh(home.path()));
        assert!(gate.argv().is_some());
        assert!(!gate.refresh(home.path()));
        // Disable bumps the lockfile: the argv is dropped on the next look.
        let lock_path = gray_plugin::lock::lock_path(home.path());
        let mut lock = gray_plugin::lock::LockFile::load(&lock_path).unwrap();
        lock.plugins.get_mut("sample").unwrap().enabled = false;
        lock.save(&lock_path).unwrap();
        assert!(gate.refresh(home.path()));
        assert!(gate.argv().is_none());
    }
}
