use std::path::Path;

use crate::cron::store::Origin as Route;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub owners: Vec<Route>,

    pub max_turns: usize,

    pub turn_timeout_secs: u64,

    pub workdir: Option<std::path::PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            owners: Vec::new(),
            max_turns: 2,
            turn_timeout_secs: 1800,
            workdir: None,
        }
    }
}

impl Settings {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("config.json");
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                log::warn!(
                    "gateway: {} unreadable ({e}); using defaults",
                    path.display()
                );
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: u32,
    pub end: u32,
}

impl Window {
    pub fn parse(raw: &str) -> Option<Self> {
        let (a, b) = raw.trim().split_once('-')?;
        Some(Self {
            start: minutes(a)?,
            end: minutes(b)?,
        })
    }

    pub fn contains(&self, minute: u32) -> bool {
        if self.start == self.end {
            return true;
        }
        if self.start < self.end {
            (self.start..self.end).contains(&minute)
        } else {
            minute >= self.start || minute < self.end
        }
    }
}

fn minutes(raw: &str) -> Option<u32> {
    let (h, m) = raw.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 24 && m < 60 && h * 60 + m <= 1440).then_some((h * 60 + m) % 1440)
}

#[path = "settings_tests.rs"]
#[cfg(test)]
mod tests;
