//! `schedule`: the model's own door into the cron store.
//!
//! One upsert-by-name call instead of an action menu: a new name creates,
//! a known name replaces, `when: "off"|"on"|"delete"` pauses, resumes or
//! drops, and every call answers with the current job list (no `list`
//! action). A tool call is the model by construction, so jobs it creates or
//! edits wait for the user's approval in `/cron` — unless the user opted out
//! with `GRAY_CRON_AUTO_APPROVE`.

use std::path::PathBuf;
use std::sync::Arc;

use gray_core::agent::{Tool, ToolContext, ToolOutput};
use gray_core::message::ToolDef;
use gray_plugin::{Manifest, Plugin};
use serde_json::Value;

use crate::cron::schedule::{Schedule, next_run, parse_schedule};
use crate::cron::store::{JobState, Origin};
use crate::cron::{CronJob, CronStore, Deliver, Repeat, now_secs};

/// Live plugin (protocol 1.3): the tool exists only while an interactive
/// TUI is up — headless `--print` runs (bench arms, gateway turns) have
/// nobody to approve or receive a wake.
#[derive(Default)]
pub(crate) struct CronPlugin;

#[async_trait::async_trait]
impl Plugin for CronPlugin {
    fn manifest(&self) -> Manifest {
        Manifest {
            name: "cron".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: Some("1.3".to_string()),
            ..Default::default()
        }
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        match (crate::host::registered_tui(), crate::setup::gray_home()) {
            (Some(_), Ok(home)) => vec![Arc::new(ScheduleTool { home })],
            _ => Vec::new(),
        }
    }
}

pub(crate) struct ScheduleTool {
    pub(crate) home: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ScheduleTool {
    fn def(&self) -> ToolDef {
        ToolDef {
            name: "schedule".to_string(),
            description: concat!(
                "Schedule work for later; it comes back to this session. Create or replace a ",
                "job by `name`. `when` is a schedule (\"in 30m\", \"every 2h\", \"daily 9am\", ",
                "\"weekdays 9:00\", \"every monday 6pm\", 5-field cron, RFC3339) — or ",
                "\"off\"/\"on\" to pause/resume, \"delete\" to remove. At fire time `prompt` runs ",
                "as a fresh turn (`reminder: true` delivers it as literal text instead). New and ",
                "edited jobs may wait for the user's approval. Call with no args to list jobs."
            )
            .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Job name (the upsert key)."},
                    "when": {"type": "string", "description": "Schedule, or off/on/delete."},
                    "prompt": {"type": "string", "description": "What to do at fire time. Required to create."},
                    "reminder": {"type": "boolean", "description": "Deliver `prompt` as literal text, no turn."},
                    "times": {"type": "integer", "minimum": 1, "description": "Stop after this many runs."}
                }
            }),
            label: None,
            preview: Some("name".to_string()),
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn execute(&self, ctx: &ToolContext, args: Value) -> ToolOutput {
        match self.apply(ctx.session_id.as_deref(), &args) {
            Ok(out) => ToolOutput::ok(out),
            Err(e) => ToolOutput::error(format!("{e:#}")),
        }
    }
}

impl ScheduleTool {
    pub(crate) fn apply(&self, sid: Option<&str>, args: &Value) -> anyhow::Result<String> {
        let store = CronStore::open(self.home.join("cron"))?;
        let s = |k: &str| {
            args.get(k)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
        };
        let note = match (s("name"), s("when")) {
            (None, _) => String::new(),
            (Some(_), None) => {
                anyhow::bail!("`when` is required with `name`: a schedule, off, on or delete")
            }
            (Some(name), Some("delete")) => {
                anyhow::ensure!(store.remove(name)?, "no job named {name:?}");
                format!("deleted {name}")
            }
            (Some(name), Some(w @ ("off" | "on"))) => {
                anyhow::ensure!(store.set_paused(name, w == "off")?, "no job named {name:?}");
                format!("{name} {}", if w == "off" { "paused" } else { "resumed" })
            }
            (Some(name), Some(when)) => upsert(&store, sid, name, when, s("prompt"), args)?,
        };
        let jobs = store.list()?;
        let list = if jobs.is_empty() {
            "no jobs".to_string()
        } else {
            jobs.iter().map(job_line).collect::<Vec<_>>().join("\n")
        };
        Ok(if note.is_empty() {
            list
        } else {
            format!("{note}\n\n{list}")
        })
    }
}

fn upsert(
    store: &CronStore,
    sid: Option<&str>,
    name: &str,
    when: &str,
    prompt: Option<&str>,
    args: &Value,
) -> anyhow::Result<String> {
    let schedule = parse_schedule(when)?;
    let times = match args.get("times").and_then(Value::as_u64) {
        Some(t) => Some(u32::try_from(t.max(1))?),
        None => None,
    };
    let reminder = args.get("reminder").and_then(Value::as_bool);
    let auto = crate::cron::auto_approve();
    let verb = if store.get(name)?.is_some() {
        // An edit is a new request: an approved job the model rewrites goes
        // back to the user, or approval would only ever gate the first draft.
        store.update(name, |j| {
            j.schedule = schedule;
            if let Some(p) = prompt {
                j.prompt = p.to_string();
            }
            if let Some(r) = reminder {
                j.reminder = r;
            }
            if times.is_some() {
                j.repeat = Repeat {
                    times,
                    completed: 0,
                };
            }
            (j.state, j.next_run_at) = if auto {
                (JobState::Active, next_run(now_secs(), &j.schedule))
            } else {
                (JobState::AwaitingApproval, None)
            };
            Ok(())
        })?;
        "updated"
    } else {
        let prompt =
            prompt.ok_or_else(|| anyhow::anyhow!("`prompt` is required to create {name:?}"))?;
        let origin = sid.map(|sid| Origin {
            platform: crate::cron_serve::SESSION_PLATFORM.to_string(),
            chat: sid.to_string(),
            thread: None,
            route: None,
        });
        let deliver = if origin.is_some() {
            Deliver::Origin
        } else {
            Deliver::Local
        };
        let reminder = reminder.unwrap_or(false);
        let id = if auto {
            store.add_full(
                name,
                when,
                prompt,
                deliver,
                origin,
                None,
                vec![],
                None,
                reminder,
            )?
        } else {
            store.add_awaiting_approval(
                name,
                when,
                prompt,
                deliver,
                origin,
                None,
                vec![],
                None,
                reminder,
            )?
        };
        if times.is_some() {
            store.update(&id, |j| {
                j.repeat.times = times;
                Ok(())
            })?;
        }
        "created"
    };
    Ok(if auto {
        format!("{verb} {name}")
    } else {
        format!("{verb} {name} — waiting for the user to approve it in /cron")
    })
}

fn job_line(j: &CronJob) -> String {
    let state = match j.state {
        JobState::Active if j.enabled => "active",
        JobState::Active => "disabled",
        JobState::Paused => "paused",
        JobState::Done => "done",
        JobState::AwaitingApproval => "awaiting approval",
    };
    let next = j
        .next_run_at
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|t| {
            format!(
                " · next {}",
                t.with_timezone(&chrono::Local).format("%a %b %-d %H:%M")
            )
        })
        .unwrap_or_default();
    let runs = match j.repeat.times {
        Some(t) => format!(" · {}/{t} runs", j.repeat.completed),
        None => String::new(),
    };
    format!(
        "{} · {} · {state}{next}{runs}",
        j.name,
        schedule_text(&j.schedule)
    )
}

pub(crate) fn schedule_text(s: &Schedule) -> String {
    match s {
        Schedule::Cron { expr } => format!("cron {expr}"),
        Schedule::Interval { secs } => match secs {
            s if s % 86_400 == 0 => format!("every {}d", s / 86_400),
            s if s % 3600 == 0 => format!("every {}h", s / 3600),
            s => format!("every {}m", s / 60),
        },
        Schedule::Once { .. } => "once".to_string(),
    }
}

/// `text` with every character a reader can't see spelled out: zero-width,
/// bidi overrides, tag characters and controls become `⟨U+XXXX⟩`, newlines
/// `⏎`. What the user approves is then exactly what will run — showing beats
/// guessing which characters a scanner should strip.
pub(crate) fn visible(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\n' => "\u{23ce} ".to_string(),
            '\u{200b}'..='\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{00ad}'
            | '\u{e0000}'..='\u{e007f}' => format!("\u{27e8}U+{:04X}\u{27e9}", c as u32),
            c if c.is_control() => format!("\u{27e8}U+{:04X}\u{27e9}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

#[path = "cron_tool_tests.rs"]
#[cfg(test)]
mod tests;
