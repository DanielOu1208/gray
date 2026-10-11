//! One guard at the request choke point.
//!
//! Prompt caching rests on a single rule: a session's outbound prefix
//! (provider, system prompt, tool inventory, history) only grows. Every
//! request is fingerprinted here and compared with the previous one of the
//! same agent; the first differing component is reported. A change the
//! agent announced beforehand (compaction, soft elision, the stale-output
//! mask — all funnelled through `expect_change`) is normal and logged at
//! debug; anything else is a self-inflicted cache miss and logged at warn,
//! so an accident (a tempdir in the system prompt, a reordered tool, a
//! timestamp) becomes a loud line instead of a silent re-bill.

use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::message::ChatRequest;

/// Hashes of the prefix components, in prefix order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    provider: u64,
    system: u64,
    tools: u64,
    messages: Vec<u64>,
}

/// The first prefix component that differs between two consecutive requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Provider,
    System,
    Tools,
    /// Message `k` of the earlier request was rewritten.
    HistoryAt(usize),
    /// The history shrank without any earlier message changing.
    HistoryTruncated {
        from: usize,
        to: usize,
    },
}

impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Change::Provider => write!(f, "provider"),
            Change::System => write!(f, "system"),
            Change::Tools => write!(f, "tools"),
            Change::HistoryAt(k) => write!(f, "history@{k}"),
            Change::HistoryTruncated { from, to } => write!(f, "history truncated {from}->{to}"),
        }
    }
}

fn digest<T: serde::Serialize + ?Sized>(value: &T) -> u64 {
    let mut h = DefaultHasher::new();
    serde_json::to_string(value)
        .unwrap_or_default()
        .hash(&mut h);
    h.finish()
}

/// Fingerprint of what a provider would see for `req` (the model id stands
/// in for the provider: a relay that bakes the tier into it renames the
/// cache key with the tier).
pub fn fingerprint(model_id: &str, req: &ChatRequest) -> Fingerprint {
    Fingerprint {
        provider: digest(model_id),
        system: digest(req.system.as_deref().unwrap_or_default()),
        tools: digest(&req.tools),
        messages: req.messages.iter().map(digest).collect(),
    }
}

/// First component of `prev` that `next` no longer shares, in prefix order;
/// `None` when `next` merely appends.
pub fn diff(prev: &Fingerprint, next: &Fingerprint) -> Option<Change> {
    if prev.provider != next.provider {
        return Some(Change::Provider);
    }
    if prev.system != next.system {
        return Some(Change::System);
    }
    if prev.tools != next.tools {
        return Some(Change::Tools);
    }
    if let Some(k) = prev
        .messages
        .iter()
        .zip(&next.messages)
        .position(|(a, b)| a != b)
    {
        return Some(Change::HistoryAt(k));
    }
    (next.messages.len() < prev.messages.len()).then(|| Change::HistoryTruncated {
        from: prev.messages.len(),
        to: next.messages.len(),
    })
}

/// Per-agent state: the previous request's fingerprint and whether the
/// next change was announced.
#[derive(Debug, Default)]
pub struct PrefixGuard {
    last: Option<Fingerprint>,
    expected: Option<&'static str>,
}

impl PrefixGuard {
    /// Announce that the next request legitimately rewrites the prefix.
    pub fn expect_change(&mut self, reason: &'static str) {
        self.expected = Some(reason);
    }

    /// Record `req`; returns the change against the previous request (if
    /// any) with the announced reason, if one was given.
    pub fn observe(
        &mut self,
        model_id: &str,
        req: &ChatRequest,
    ) -> Option<(Change, Option<&'static str>)> {
        let next = fingerprint(model_id, req);
        let change = self.last.as_ref().and_then(|prev| diff(prev, &next));
        self.last = Some(next);
        let expected = self.expected.take();
        change.map(|c| (c, expected))
    }
}
