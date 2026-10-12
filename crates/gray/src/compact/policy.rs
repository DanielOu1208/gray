//! Compaction policy: settings, token estimates, overflow detection (split from `compact`).

use super::*;

pub struct CompactionSettings {
    pub reserve_tokens: usize,
    /// Pre-turn ceiling beyond the window bound: an explicit `--compact-at` /
    /// `GRAY_COMPACT_AT` / `"compact_at_tokens"` choice, or the ceiling
    /// `gray_core::price_cliff_ceiling` derives from the model's
    /// declared price cliff. The window bound always applies on top.
    pub ceiling_tokens: Option<usize>,
    /// The provider-declared cliff `ceiling_tokens` came from, for the "why"
    /// on compaction notices. `None` when the ceiling is an explicit user
    /// value.
    pub price_cliff_tokens: Option<usize>,
}

/// Master switch for *automatic* compaction only (`should_compact` callers and
/// `auto_compact_if_needed`). Manual entry points (`compact_with_keep`,
/// `compact_with_instructions`) always run, so an `enabled=false` session keeps
/// its manual escape hatch.
static AUTO_COMPACT_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// Disables/enables automatic compaction for this session. Manual `/compact` ignores this.
pub fn set_auto_compact_enabled(on: bool) {
    AUTO_COMPACT_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn is_auto_compact_enabled() -> bool {
    AUTO_COMPACT_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Env kill-switch for automatic compaction, following the `GRAY_*` pattern in
/// `config.rs`: `GRAY_NO_AUTO_COMPACT=1` (also `true`/`yes`/`on`) disables it.
/// `0`/`false`/`no`/`off`/unset leave it enabled. Manual `/compact` still runs.
pub fn init_auto_compact_from_env() {
    let disabled = std::env::var("GRAY_NO_AUTO_COMPACT")
        .map(|s| {
            !matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no" | "off"
            )
        })
        .unwrap_or(false);
    set_auto_compact_enabled(!disabled);
}

pub fn should_compact(tokens: usize, window: usize, s: &CompactionSettings) -> bool {
    tokens > window.saturating_sub(s.reserve_tokens)
        || s.ceiling_tokens.is_some_and(|c| tokens >= c)
}

/// Why a firing at `tokens` was the ceiling rather than the window bound —
/// `None` means the window bound (or nothing) fired.
pub fn binding_reason(tokens: usize, s: &CompactionSettings) -> Option<CompactReason> {
    s.ceiling_tokens.filter(|&c| tokens >= c).map(|_| {
        match s.price_cliff_tokens {
            Some(cliff) => CompactReason::PriceCliff { cliff },
            None => CompactReason::UserCeiling,
        }
    })
}

/// Display suffix for a compaction trigger: the empty string for the plain
/// window bound so existing lines read unchanged.
pub fn compact_reason_note(reason: CompactReason) -> String {
    match reason {
        CompactReason::Window => String::new(),
        CompactReason::Overflow => " · overflow".to_string(),
        CompactReason::UserCeiling => " · --compact-at".to_string(),
        CompactReason::PriceCliff { cliff } => {
            format!(" · price cliff {}", crate::setup::format_context_length(cliff))
        }
    }
}

/// Effective (ceiling, price_cliff) for one model: an explicit `compact_at`
/// is the user's choice — it clamps to `window − reserve` and keeps no cliff
/// reason; `Auto`/unset derives the ceiling from the declared cliff.
pub fn ceiling_for(
    compact_at: Option<crate::setup::CompactAt>,
    window: usize,
    reserve: usize,
    price_cliff: Option<usize>,
) -> (Option<usize>, Option<usize>) {
    match compact_at {
        Some(crate::setup::CompactAt::Tokens(n)) => (
            Some(n.min(window.saturating_sub(reserve))).filter(|&c| c > 0),
            None,
        ),
        _ => (
            price_cliff.and_then(|c| gray_core::price_cliff_ceiling(c, reserve)),
            price_cliff,
        ),
    }
}

/// The estimate at which auto-compaction fires for `config`'s model right now
/// (the ceiling when one is declared, else the window bound) — what
/// `run_print_inner`'s registration and `--json` metadata report.
pub fn effective_compact_ceiling(config: &crate::Config) -> usize {
    let model = config.model.as_deref().unwrap_or("");
    let window = crate::setup::resolve_model_context_length(model);
    let reserve = crate::setup::user_reserve_tokens_for(window);
    ceiling_for(
        config.compact_at,
        window,
        reserve,
        crate::setup::model_price_cliff(model).map(|c| c as usize),
    )
    .0
    .unwrap_or_else(|| window.saturating_sub(reserve))
}

/// Window-aware variant — prefer this where the model window is known.
/// `ceiling`/`price_cliff` come from the built agent
/// ([`Agent::compact_ceiling`], [`Agent::price_cliff`]).
pub fn compaction_settings_for(
    window: usize,
    ceiling: Option<usize>,
    price_cliff: Option<usize>,
) -> CompactionSettings {
    CompactionSettings {
        reserve_tokens: crate::setup::user_reserve_tokens_for(window),
        ceiling_tokens: ceiling,
        price_cliff_tokens: price_cliff,
    }
}

pub fn estimate_tokens(msg: &Message) -> usize {
    // Must measure billable context, not displayable prose: a message whose
    // only block is a 50 KiB tool result is ~12.8k tokens, not 0. See
    // `Message::context_text`.
    // Images AND other inline media (PDF/audio/video) take the per-block
    // path: `context_text` includes the base64 payload, so bytes/4 of a
    // large attachment read as millions of tokens the moment it was sent.
    if msg.content.iter().any(|block| {
        matches!(
            block,
            gray_core::message::ContentBlock::Image { .. }
                | gray_core::message::ContentBlock::Media { .. }
        )
    }) {
        gray_core::agent::estimate_message_tokens(msg)
    } else {
        msg.context_text().len().div_ceil(4)
    }
}

pub fn estimate_context_tokens(messages: &[Message], last: Option<Usage>) -> usize {
    if let Some(u) = last
        && u.total() > 0
    {
        return u.total();
    }
    messages.iter().map(estimate_tokens).sum()
}

pub fn is_context_overflow_error(err: &CoreError) -> bool {
    if let CoreError::Provider(msg) = err {
        let lower = msg.to_lowercase();
        lower.contains("context_length")
            || lower.contains("context window")
            || lower.contains("context length")
            || lower.contains("max_tokens")
    } else {
        false
    }
}
