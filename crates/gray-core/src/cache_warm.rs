use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use tokio::time::Instant;

use crate::agent::Provider;
use crate::event::{StreamEvent, Usage};
use crate::message::ChatRequest;

pub const MAX_WARMING_AGE: Duration = Duration::from_secs(60 * 60);

pub const MIN_EXPECTED_SAVINGS: f64 = 0.05;

pub const MIN_WARM_TOKENS: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Prices {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,

    pub cache_write: f64,
}

#[derive(Clone)]
pub struct CacheWarmPolicy {
    pub ttl: Duration,
    pub prices: Arc<dyn Fn() -> Option<Prices> + Send + Sync>,
}

impl std::fmt::Debug for CacheWarmPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheWarmPolicy")
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

pub fn warming_delay(ttl: Duration) -> Option<Duration> {
    (ttl > Duration::from_secs(10)).then(|| (ttl.mul_f64(0.9)).min(ttl - Duration::from_secs(10)))
}

pub fn expected_savings(p: &Prices, prompt_tokens: usize, warm_output: u32) -> f64 {
    let n = prompt_tokens as f64;
    let hit = n * p.cache_read;
    let miss = if p.cache_write > 0.0 {
        n * p.cache_write
    } else {
        n * p.input
    };
    let warm = hit + f64::from(warm_output) * p.output;
    (miss - hit).max(0.0) - warm
}

pub fn worth_refreshing(
    prices: Option<&Prices>,
    prompt_tokens: usize,
    warm_output: u32,
    cache_reported: bool,
) -> bool {
    if prompt_tokens == 0 {
        return false;
    }
    let big = prompt_tokens >= MIN_WARM_TOKENS;
    match prices.filter(|p| p.input > 0.0 || p.cache_read > 0.0 || p.cache_write > 0.0) {
        Some(p) => {
            let savings = expected_savings(p, prompt_tokens, warm_output);
            savings >= MIN_EXPECTED_SAVINGS || (big && savings > 0.0)
        }
        None => big && cache_reported,
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WarmHint {
    pub(crate) prompt_tokens: usize,
    pub(crate) cache_reported: bool,
}

pub(crate) async fn keep_warm(
    provider: Arc<dyn Provider>,
    mut req: ChatRequest,
    policy: CacheWarmPolicy,
    hint: Arc<Mutex<WarmHint>>,
    sent: Instant,
    spent: Arc<Mutex<Usage>>,
) {
    let Some(delay) = warming_delay(policy.ttl) else {
        return;
    };

    let warm_output = provider.warm_output_cap(&req);
    req.max_tokens = Some(warm_output);
    let mut next = sent + delay;
    loop {
        if next > sent + MAX_WARMING_AGE {
            return;
        }

        let deadline = next + (policy.ttl - delay) / 2;
        tokio::time::sleep_until(next).await;
        if Instant::now() > deadline {
            log::debug!(target: "gray_agent", "cache warm: refresh deadline missed");
            return;
        }

        let WarmHint {
            prompt_tokens,
            cache_reported,
        } = hint.lock().map(|h| *h).unwrap_or_default();
        if !worth_refreshing(
            (policy.prices)().as_ref(),
            prompt_tokens,
            warm_output,
            cache_reported,
        ) {
            return;
        }

        let mut stream = provider.stream(req.clone());
        while let Some(event) = stream.next().await {
            match event {
                Ok(StreamEvent::MessageComplete {
                    usage: Some(usage), ..
                }) => {
                    log::debug!(target: "gray_agent", "cache warm: refreshed {} prompt tokens", usage.input_tokens);
                    if let Ok(mut total) = spent.lock() {
                        total.accumulate(&usage);
                    }
                }
                Err(e) => {
                    log::debug!(target: "gray_agent", "cache warm: refresh failed: {e}");
                    break;
                }
                _ => {}
            }
        }
        next = Instant::now() + delay;
    }
}

pub(crate) struct WarmGuard(pub(crate) tokio::task::JoinHandle<()>);

impl Drop for WarmGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[path = "cache_warm_tests.rs"]
#[cfg(test)]
mod tests;
