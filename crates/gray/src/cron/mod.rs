//! cron: recurring/one-shot job scheduling for the gray daemon.
//!
//! Task 1: schedule expressions (pure math). Job store + ticker land in
//! Tasks 2-3. Time is UTC everywhere, comparisons in epoch seconds.

pub mod schedule;
pub mod store;

pub use schedule::Schedule;
pub use store::TICKER_STALE_SECS;
pub use store::{
    CronHealth, CronJob, CronStore, Deliver, OverdueJob, Repeat, RunStatus, TickStamp, now_secs,
};

/// `GRAY_CRON_AUTO_APPROVE=1/true/yes/on` opts out of the approval gate for
/// session-requested jobs: the user sets it in their own env (it is exported
/// to bash-tool children like `GRAY_SESSION_ID`), so its presence is still a
/// user decision — just a standing one instead of per-job.
pub fn auto_approve() -> bool {
    std::env::var("GRAY_CRON_AUTO_APPROVE").is_ok_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}
