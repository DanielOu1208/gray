use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};

use crate::message::ChatRequest;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fingerprint {
    provider: u64,
    system: u64,
    tools: u64,
    messages: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Provider,
    System,
    Tools,

    HistoryAt(usize),

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

pub fn fingerprint(model_id: &str, req: &ChatRequest) -> Fingerprint {
    Fingerprint {
        provider: digest(model_id),
        system: digest(req.system.as_deref().unwrap_or_default()),
        tools: digest(&req.tools),
        messages: req.messages.iter().map(digest).collect(),
    }
}

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
    (next.messages.len() < prev.messages.len()).then_some(Change::HistoryTruncated {
        from: prev.messages.len(),
        to: next.messages.len(),
    })
}

#[derive(Debug, Default)]
pub struct PrefixGuard {
    last: Option<Fingerprint>,
    expected: Option<&'static str>,
}

impl PrefixGuard {

    pub fn expect_change(&mut self, reason: &'static str) {
        self.expected = Some(reason);
    }

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
