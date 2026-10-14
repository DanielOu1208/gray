use super::*;
use gray_core::message::{ContentBlock, Role};
use std::sync::Arc;

#[test]
fn should_compact_ceiling() {

    let (ceiling, _) = ceiling_for(None, 200_000, 12_500, Some(100_000));
    let s = CompactionSettings {
        reserve_tokens: 12_500,
        ceiling_tokens: ceiling,
        price_cliff_tokens: Some(100_000),
    };
    assert!(!should_compact(87_499, 200_000, &s));
    assert!(should_compact(87_500, 200_000, &s));
    assert_eq!(
        binding_reason(87_500, &s),
        Some(CompactReason::PriceCliff { cliff: 100_000 })
    );
    assert_eq!(binding_reason(87_499, &s), None);

    assert!(should_compact(190_000, 200_000, &s));
    assert_eq!(
        binding_reason(190_000, &s),
        Some(CompactReason::PriceCliff { cliff: 100_000 })
    );
}

#[test]
fn ceiling_for_math() {
    use crate::setup::CompactAt;

    assert_eq!(
        ceiling_for(None, 200_000, 12_500, Some(100_000)),
        (Some(87_500), Some(100_000))
    );

    assert_eq!(ceiling_for(None, 200_000, 12_500, None), (None, None));

    assert_eq!(
        ceiling_for(Some(CompactAt::Auto), 1_000_000, 62_500, Some(100_000)),
        (Some(37_500), Some(100_000))
    );

    assert_eq!(
        ceiling_for(
            Some(CompactAt::Tokens(150_000)),
            200_000,
            12_500,
            Some(100_000)
        ),
        (Some(150_000), None)
    );

    assert_eq!(
        ceiling_for(
            Some(CompactAt::Tokens(1_000_000)),
            200_000,
            12_500,
            Some(100_000)
        ),
        (Some(187_500), None)
    );

    assert_eq!(
        ceiling_for(Some(CompactAt::Tokens(50_000)), 10_000, 12_500, None),
        (None, None)
    );
}

#[test]
fn parse_compact_at_grammar() {
    use crate::setup::{CompactAt, parse_compact_at};
    assert_eq!(parse_compact_at("auto"), Some(CompactAt::Auto));
    assert_eq!(parse_compact_at(" AUTO "), Some(CompactAt::Auto));
    assert_eq!(parse_compact_at("90k"), Some(CompactAt::Tokens(90_000)));
    assert_eq!(parse_compact_at("1.5m"), Some(CompactAt::Tokens(1_500_000)));
    assert_eq!(parse_compact_at("90000"), Some(CompactAt::Tokens(90_000)));
    assert_eq!(parse_compact_at("garbage"), None);

    assert_eq!(
        serde_json::from_str::<CompactAt>("90000").unwrap(),
        CompactAt::Tokens(90_000)
    );
    assert_eq!(
        serde_json::from_str::<CompactAt>("\"auto\"").unwrap(),
        CompactAt::Auto
    );
}

#[test]
fn summary_envelope_matches_core_helper_byte_for_byte() {
    let m1 = gray_core::agent::summary_message("  shared summary  ");
    assert!(m1.text_content().contains("shared summary"));

    let m2 = gray_core::agent::summary_message("shared summary");
    assert_eq!(m1.text_content().as_bytes(), m2.text_content().as_bytes());
}

#[test]
fn estimate_counts_tool_results() {

    let big = Message::new(
        Role::User,
        vec![ContentBlock::tool_result("t1", "x".repeat(4096), false)],
    );
    assert_eq!(
        estimate_tokens(&big),
        1024,
        "a 4 KiB tool result is ~1024 tokens; measuring 0 means the \
             estimator is reading text blocks only"
    );
}

#[test]
fn tool_heavy_history_trips_threshold_without_provider_usage() {
    let msgs: Vec<Message> = (0..10)
        .map(|i| {
            Message::new(
                Role::User,
                vec![ContentBlock::tool_result(
                    format!("t{i}"),
                    "y".repeat(8192),
                    false,
                )],
            )
        })
        .collect();

    let tokens = estimate_context_tokens(&msgs, None);
    assert_eq!(tokens, 20_480, "80 KiB of tool output measured as {tokens}");

    let s = CompactionSettings {
        reserve_tokens: 16_384,
        ceiling_tokens: None,
        price_cliff_tokens: None,
    };

    assert!(
        should_compact(tokens, 32_000, &s),
        "tool-heavy history must auto-compact; before the fix it measured \
             zero and the session ran straight into a provider overflow"
    );
}

#[test]
fn is_context_overflow_error_detects() {
    assert!(is_context_overflow_error(&CoreError::Provider(
        "context_length_exceeded: too many tokens".into()
    )));
    assert!(is_context_overflow_error(&CoreError::Provider(
        "This model's maximum context length is 128000 tokens".into()
    )));
    assert!(is_context_overflow_error(&CoreError::Provider(
        "context window exceeded".into()
    )));
    assert!(is_context_overflow_error(&CoreError::Provider(
        "max_tokens exceeded".into()
    )));

    assert!(!is_context_overflow_error(&CoreError::Provider(
        "length truncation: output was cut off".into()
    )));
    assert!(!is_context_overflow_error(&CoreError::Provider(
        "rate limit exceeded".into()
    )));
    assert!(!is_context_overflow_error(&CoreError::Cancelled));
}

#[test]
fn repl_threshold_120k_128k_triggers_compact() {
    let usage = Usage {
        input_tokens: 100_000,
        output_tokens: 20_000,
        ..Default::default()
    };
    assert_eq!(usage.total(), 120_000);
    let window = 128_000;
    let tokens = estimate_context_tokens(&[Message::user("hi")], Some(usage));
    assert_eq!(tokens, 120_000);
    let s = CompactionSettings {
        reserve_tokens: 16_384,
        ceiling_tokens: None,
        price_cliff_tokens: None,
    };
    assert!(should_compact(tokens, window, &s));

    let usage2 = Usage {
        input_tokens: 90_000,
        output_tokens: 10_000,
        ..Default::default()
    };
    let tokens2 = estimate_context_tokens(&[Message::user("hi")], Some(usage2));
    assert!(!should_compact(tokens2, window, &s));
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn auto_compact_triggers_on_threshold() {
    let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
    let _home = TempGrayHome::set();
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gray_core::agent::{Agent, Provider, ToolContext, ToolExecutor};
    use gray_core::event::{StopReason, Usage};
    use gray_core::message::ChatRequest;

    struct FakeProvider {
        summary: String,
    }
    #[async_trait]
    impl Provider for FakeProvider {
        fn stream(
            &self,
            _req: ChatRequest,
        ) -> BoxStream<
            'static,
            Result<gray_core::event::StreamEvent, gray_core::agent::ProviderError>,
        > {
            let summary = self.summary.clone();
            let events = vec![
                gray_core::event::StreamEvent::TextDelta { delta: summary },
                gray_core::event::StreamEvent::MessageComplete {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Some(Usage::new(10, 5)),
                },
            ];
            Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
        }
    }

    struct NoopExecutor;
    #[async_trait]
    impl ToolExecutor for NoopExecutor {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _name: &str,
            _args: serde_json::Value,
        ) -> futures::future::BoxFuture<'static, gray_core::agent::ToolOutput> {
            Box::pin(async { gray_core::agent::ToolOutput::ok("") })
        }
    }

    let provider = FakeProvider {
        summary: "Test summary content".to_string(),
    };
    let executor = NoopExecutor;
    crate::setup::set_user_keep_recent_tokens(Some(0));

    let mut agent = Agent::new(Box::new(provider), Arc::new(executor)).with_messages(vec![
        Message::user("hello ".repeat(500)),
        Message::assistant("hi there ".repeat(500)),
        Message::user("more context ".repeat(500)),
    ]);
    let compacted = auto_compact_if_needed(&mut agent)
        .await
        .expect("compact should succeed");
    crate::setup::set_user_keep_recent_tokens(None);
    assert!(compacted, "should have compacted");
    assert_eq!(agent.messages().len(), 1, "keep=0 leaves only the summary");
    assert!(
        agent.messages()[0]
            .text_content()
            .contains("Test summary content")
    );
}

struct TempGrayHome {
    prev: Option<String>,
    _dir: tempfile::TempDir,
}
impl TempGrayHome {
    fn set() -> Self {
        let prev = std::env::var("GRAY_HOME").ok();
        let dir = tempfile::TempDir::new().expect("temp gray home");
        unsafe { std::env::set_var("GRAY_HOME", dir.path()) };
        Self { prev, _dir: dir }
    }
}
impl Drop for TempGrayHome {
    fn drop(&mut self) {
        match &self.prev {
            Some(v) => unsafe { std::env::set_var("GRAY_HOME", v) },
            None => unsafe { std::env::remove_var("GRAY_HOME") },
        }
    }
}

fn scripted_agent(summary_text: &str) -> Agent {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gray_core::agent::{Provider, ToolContext, ToolExecutor};
    use gray_core::event::{StopReason, Usage};
    use gray_core::message::ChatRequest;
    struct P {
        text: String,
    }
    #[async_trait]
    impl Provider for P {
        fn stream(
            &self,
            _req: ChatRequest,
        ) -> BoxStream<
            'static,
            Result<gray_core::event::StreamEvent, gray_core::agent::ProviderError>,
        > {
            let text = self.text.clone();
            Box::pin(futures::stream::iter(vec![
                Ok(gray_core::event::StreamEvent::TextDelta { delta: text }),
                Ok(gray_core::event::StreamEvent::MessageComplete {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Some(Usage::new(10, 5)),
                }),
            ]))
        }
    }
    struct E;
    #[async_trait]
    impl ToolExecutor for E {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _name: &str,
            _args: serde_json::Value,
        ) -> futures::future::BoxFuture<'static, gray_core::agent::ToolOutput> {
            Box::pin(async { gray_core::agent::ToolOutput::ok("") })
        }
    }
    Agent::new(
        Box::new(P {
            text: summary_text.to_string(),
        }),
        Arc::new(E),
    )
}

fn sized(tag: &str) -> Message {
    Message::user(format!("{tag}:{}", "x".repeat(400 - tag.len() - 1)))
}

#[tokio::test]
async fn compact_with_keep_rejects_blank_summary() {
    let mut ag = scripted_agent("   \n  ");
    ag.set_messages(vec![Message::user("hello"), Message::assistant("hi")]);
    let before = ag.messages().to_vec();
    let out = compact_with_keep(&mut ag, None, 0)
        .await
        .expect("must not error on blank summary");
    assert!(out.is_none(), "blank summary must refuse, not wipe history");
    assert_eq!(ag.messages(), &before);
}

#[tokio::test]
async fn compact_refuses_non_shrinking_replacement() {

    let long = "z".repeat(2000);
    let mut ag = scripted_agent(&long);
    ag.set_messages(vec![Message::user("hi")]);
    let before = ag.messages().to_vec();
    assert!(
        compact_with_keep(&mut ag, None, 0)
            .await
            .expect("must not error")
            .is_none()
    );
    assert_eq!(ag.messages(), &before);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn compact_retains_newest_and_boundary_truncates_oldest() {
    let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
    let _home = TempGrayHome::set();
    let mut ag = scripted_agent("S");
    ag.set_messages(vec![sized("m1"), sized("m2"), sized("m3"), sized("m4")]);

    let out = compact_with_keep(&mut ag, None, 250)
        .await
        .expect("compact must succeed");
    assert!(out.is_some());
    let msgs = ag.messages();

    assert_eq!(
        msgs.len(),
        4,
        "anchor + summary + truncated m3 + m4, got {}",
        msgs.len()
    );
    assert!(
        msgs[0].text_content().starts_with("m1:"),
        "pinned user intent leads"
    );
    assert!(
        msgs[1]
            .text_content()
            .contains("compacted into the following summary"),
        "summary follows the anchor"
    );
    assert!(
        msgs[2].text_content().starts_with("m3:"),
        "boundary group kept truncated: {}",
        msgs[2].text_content().chars().take(20).collect::<String>()
    );
    assert!(
        msgs[3].text_content().contains("m4"),
        "retained newest closes"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn compact_drops_tool_pair_atomically() {
    let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
    let _home = TempGrayHome::set();
    let mut ag = scripted_agent("S");

    let use_msg = Message::new(
        Role::Assistant,
        vec![ContentBlock::tool_use(
            "t1",
            "sh",
            serde_json::json!({"cmd": "x".repeat(2000)}),
        )],
    );
    let res_msg = Message::new(
        Role::User,
        vec![ContentBlock::tool_result("t1", "ok", false)],
    );
    ag.set_messages(vec![
        Message::user("o".repeat(2000)),
        use_msg,
        res_msg,
        Message::user("new"),
    ]);
    let out = compact_with_keep(&mut ag, None, 250)
        .await
        .expect("compact must succeed");
    assert!(
        out.is_some(),
        "dropping the pair still shrinks, must compact"
    );
    let msgs = ag.messages();
    assert!(
        msgs.iter().flat_map(|m| &m.content).all(|b| !matches!(
            b,
            ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. }
        )),
        "no orphaned call/result may survive: {msgs:?}"
    );
    assert_eq!(
        msgs.last().expect("history").text_content(),
        "new",
        "history still ends on the newest user turn"
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn manual_compact_passes_instructions_to_trigger() {
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gray_core::agent::{Provider, ToolContext, ToolExecutor};
    use gray_core::event::{StopReason, Usage};
    use gray_core::message::ChatRequest;
    use std::sync::Mutex;

    struct CapturingProvider {
        seen: Arc<Mutex<Vec<ChatRequest>>>,
    }
    #[async_trait]
    impl Provider for CapturingProvider {
        fn stream(
            &self,
            req: ChatRequest,
        ) -> BoxStream<
            'static,
            Result<gray_core::event::StreamEvent, gray_core::agent::ProviderError>,
        > {
            self.seen.lock().expect("seen lock").push(req);
            let events = vec![
                gray_core::event::StreamEvent::TextDelta {
                    delta: "S".to_string(),
                },
                gray_core::event::StreamEvent::MessageComplete {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Some(Usage::new(10, 5)),
                },
            ];
            Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
        }
    }
    struct NoopExecutor;
    #[async_trait]
    impl ToolExecutor for NoopExecutor {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _name: &str,
            _args: serde_json::Value,
        ) -> futures::future::BoxFuture<'static, gray_core::agent::ToolOutput> {
            Box::pin(async { gray_core::agent::ToolOutput::ok("") })
        }
    }

    let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
    let _home = TempGrayHome::set();
    let seen: Arc<Mutex<Vec<ChatRequest>>> = Arc::default();
    let mut ag = Agent::new(
        Box::new(CapturingProvider { seen: seen.clone() }),
        Arc::new(NoopExecutor),
    );
    ag.set_messages(vec![Message::user("m".repeat(4000))]);

    let out = compact_with_keep(&mut ag, Some("focus on auth"), 0)
        .await
        .expect("must not error");

    assert!(out.is_some());
    let reqs = seen.lock().expect("seen lock");
    assert_eq!(reqs.len(), 1, "one in-band trigger call");
    assert!(
        reqs[0].messages.len() > 1,
        "v2 sends the whole history + trigger, not one serialized prompt"
    );
    let trigger = reqs[0].messages.last().expect("trigger message");
    assert!(
        trigger.text_content().contains("focus on auth"),
        "custom instructions ride the trigger: {}",
        trigger.text_content()
    );
}

#[test]
fn checkpoint_is_private_unpredictable_and_rotated() {
    let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
    let _home = TempGrayHome::set();
    let a = write_continuation_checkpoint("summary A", 10).expect("write");
    let b = write_continuation_checkpoint("summary B", 20).expect("write");
    assert_ne!(a.file_name(), b.file_name(), "names must be unpredictable");
    let home = std::env::var("GRAY_HOME").expect("test sets GRAY_HOME");
    assert!(a.starts_with(&home), "under gray home, got {}", a.display());
    assert_eq!(
        a.parent().expect("parent"),
        std::path::Path::new(&home).join("continuation-checkpoints"),
        "per gray-home dir, never temp_dir(), got {}",
        a.display()
    );
    let body = std::fs::read_to_string(&a).expect("read back");
    assert!(body.contains("summary A"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&a).expect("stat").permissions().mode() & 0o777,
            0o600,
            "owner-only"
        );
    }
    for i in 0..7 {
        write_continuation_checkpoint(&format!("s{i}"), i);
    }
    let count = std::fs::read_dir(a.parent().expect("parent"))
        .expect("read dir")
        .count();
    assert_eq!(count, 5, "rotation keeps newest 5, kept {count}");
}

static COMPACT_SWITCH_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct EnableGuard;
impl Drop for EnableGuard {
    fn drop(&mut self) {
        set_auto_compact_enabled(true);
    }
}

mod switch_tests {
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use gray_core::agent::{Agent, Provider, ToolContext, ToolExecutor};
    use gray_core::event::{StopReason, Usage};
    use gray_core::message::ChatRequest;

    struct FakeProvider;
    #[async_trait]
    impl Provider for FakeProvider {
        fn stream(
            &self,
            _req: ChatRequest,
        ) -> BoxStream<
            'static,
            Result<gray_core::event::StreamEvent, gray_core::agent::ProviderError>,
        > {
            let events = vec![
                gray_core::event::StreamEvent::TextDelta {
                    delta: "summarized".to_string(),
                },
                gray_core::event::StreamEvent::MessageComplete {
                    stop_reason: Some(StopReason::EndTurn),
                    usage: Some(Usage::new(10, 5)),
                },
            ];
            Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
        }
    }

    struct NoopExecutor;
    #[async_trait]
    impl ToolExecutor for NoopExecutor {
        fn execute(
            &self,
            _ctx: &ToolContext,
            _name: &str,
            _args: serde_json::Value,
        ) -> futures::future::BoxFuture<'static, gray_core::agent::ToolOutput> {
            Box::pin(async { gray_core::agent::ToolOutput::ok("") })
        }
    }

    fn agent() -> Agent {

        Agent::new(Box::new(FakeProvider), Arc::new(NoopExecutor)).with_messages(vec![
            Message::user("hello ".repeat(500)),
            Message::assistant("hi there ".repeat(500)),
        ])
    }

    #[tokio::test]
    async fn auto_compact_disabled_is_noop() {
        let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
        let _guard = EnableGuard;
        set_auto_compact_enabled(false);
        let mut ag = agent();
        let out = auto_compact_if_needed(&mut ag)
            .await
            .expect("must not error when disabled");
        assert!(!out);
        assert_eq!(
            ag.messages().len(),
            2,
            "disabled auto-compact must leave history untouched"
        );
    }

    #[tokio::test]
    async fn env_kill_switch_disables_auto_compact() {
        let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
        let _guard = EnableGuard;
        let prev = std::env::var("GRAY_NO_AUTO_COMPACT").ok();
        unsafe { std::env::set_var("GRAY_NO_AUTO_COMPACT", "1") };
        init_auto_compact_from_env();
        let mut ag = agent();
        let out = auto_compact_if_needed(&mut ag)
            .await
            .expect("must not error when disabled");
        assert!(!out);
        assert_eq!(
            ag.messages().len(),
            2,
            "env-disabled auto-compact must leave history untouched"
        );
        match prev {
            Some(v) => unsafe { std::env::set_var("GRAY_NO_AUTO_COMPACT", v) },
            None => unsafe { std::env::remove_var("GRAY_NO_AUTO_COMPACT") },
        }
    }

    #[tokio::test]
    async fn env_unset_leaves_auto_compact_enabled() {
        let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
        let _home = TempGrayHome::set();
        let _guard = EnableGuard;
        let prev = std::env::var("GRAY_NO_AUTO_COMPACT").ok();
        unsafe { std::env::remove_var("GRAY_NO_AUTO_COMPACT") };
        init_auto_compact_from_env();

        crate::setup::set_user_keep_recent_tokens(Some(0));
        let mut ag = agent();
        let out = auto_compact_if_needed(&mut ag)
            .await
            .expect("compact should succeed");
        crate::setup::set_user_keep_recent_tokens(None);
        assert!(out);
        assert!(ag.messages()[0].text_content().contains("summarized"));
        match prev {
            Some(v) => unsafe { std::env::set_var("GRAY_NO_AUTO_COMPACT", v) },
            None => unsafe { std::env::remove_var("GRAY_NO_AUTO_COMPACT") },
        }
    }

    #[tokio::test]
    async fn manual_compact_bypasses_disabled_switch() {
        let _serial = COMPACT_SWITCH_SERIAL.lock().unwrap();
        let _home = TempGrayHome::set();
        let _guard = EnableGuard;
        set_auto_compact_enabled(false);
        let mut ag = agent();
        let out = compact_with_keep(&mut ag, None, 0)
            .await
            .expect("manual compact must run when disabled");
        assert!(out.is_some());
        assert!(ag.messages()[0].text_content().contains("summarized"));
    }
}

#[test]
fn screenshot_budget_does_not_count_base64_as_prose() {
    let message = Message::new(
        Role::User,
        vec![ContentBlock::image(
            "image/png",
            "a".repeat(5 * 1024 * 1024),
        )],
    );
    let estimate = estimate_tokens(&message);
    assert!((1000..=4096).contains(&estimate), "estimate: {estimate}");
}

#[tokio::test]
async fn image_then_text_turn_keeps_image_without_spurious_compaction() {
    let mut agent = scripted_agent("answer").with_context_window(Some(128_000));
    let image = Message::new(
        Role::User,
        vec![ContentBlock::image(
            "image/png",
            "a".repeat(5 * 1024 * 1024),
        )],
    );
    agent
        .run(image, gray_core::agent::ToolContext::default())
        .await
        .unwrap();
    let revision = agent.history_revision();
    let tokens = estimate_context_tokens(agent.messages(), None);
    assert!(!should_compact(
        tokens,
        128_000,
        &CompactionSettings {
            reserve_tokens: 16_384,
            ceiling_tokens: None,
            price_cliff_tokens: None
        }
    ));
    agent
        .run(
            Message::user("describe it again"),
            gray_core::agent::ToolContext::default(),
        )
        .await
        .unwrap();
    assert_eq!(agent.history_revision(), revision);
    assert!(agent.messages().iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    }));
}
