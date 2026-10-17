use crate::cron::schedule::Schedule;

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
