//! Plugin index client: fetch over HTTPS, ETag-cache, lookup.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fetch::redact;

/// Default index URL (overridden by [`INDEX_URL_ENV`]).
pub const DEFAULT_INDEX_URL: &str = "https://gray.alignment.id/plugins/index.json";
/// Env var overriding the index URL (tests point this at loopback).
pub const INDEX_URL_ENV: &str = "GRAY_PLUGIN_INDEX";
/// Default hub index URL: every package kind in one file (overridden by
/// [`HUB_INDEX_URL_ENV`]).
pub const DEFAULT_HUB_INDEX_URL: &str = "https://gray.alignment.id/hub/index.json";
/// Env var overriding the hub index URL (tests point this at loopback).
pub const HUB_INDEX_URL_ENV: &str = "GRAY_HUB_INDEX";
/// Freshness window for the on-disk index cache.
pub const INDEX_TTL_SECS: u64 = 24 * 60 * 60;

pub fn index_url() -> String {
    std::env::var(INDEX_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_INDEX_URL.to_string())
}

pub fn hub_index_url() -> String {
    std::env::var(HUB_INDEX_URL_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HUB_INDEX_URL.to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    #[serde(default)]
    pub plugins: BTreeMap<String, Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub ecosystem: String,
    pub version: String,
    pub source: Source,
    pub hash: HashSpec,
    #[serde(default)]
    pub scope: String,
    /// Package kind: "plugin" (sidecar binary, the default when the index
    /// predates kinds), "skill" (SKILL.md bundle), "misc" (anything else).
    /// Empty string means a legacy index row — treated as a plugin.
    #[serde(default)]
    pub kind: String,
    /// Publisher handle or "official" — informational only.
    #[serde(default)]
    pub author: Option<String>,
    /// `true` for system-owned (first-party) plugins. Absent on community
    /// entries.
    #[serde(default)]
    pub official: bool,
    /// `true` for community plugins a maintainer has blessed. The registry
    /// is open — anyone can publish — so an entry with neither `official`
    /// nor `verified` is arbitrary third-party code; a matching tarball
    /// hash proves integrity, never trustworthiness.
    #[serde(default)]
    pub verified: bool,
    /// SPDX license id when the index carries one; informational only.
    #[serde(default)]
    pub license: String,
}

impl Entry {
    /// Whether the index asserts any trust for this entry.
    pub fn trusted(&self) -> bool {
        self.official || self.verified
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    #[serde(rename = "type")]
    pub type_: String,
    pub url: String,
}

/// Index hash: either `"sha256:<hex>"` or a per-target map.
/// Per-target selection lands later; until then [`HashSpec::primary`] wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HashSpec {
    Single(String),
    PerTarget(BTreeMap<String, String>),
}

impl HashSpec {
    /// Primary hash string: the single value, or the first map entry.
    pub fn primary(&self) -> Option<&str> {
        match self {
            HashSpec::Single(s) => Some(s.as_str()),
            HashSpec::PerTarget(m) => m.values().next().map(|s| s.as_str()),
        }
    }
}

/// Look up a plugin by name. Miss message is part of the CLI contract.
pub fn lookup<'a>(index: &'a Index, name: &str) -> anyhow::Result<&'a Entry> {
    index
        .plugins
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("not in index: {name} (try /plugin install <https-url>)"))
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexCache {
    etag: Option<String>,
    fetched_at: u64,
    index: Index,
}

fn cache_path(file: &str) -> PathBuf {
    crate::plugins_dir().join(file)
}

fn read_cache(file: &str) -> anyhow::Result<IndexCache> {
    let raw = std::fs::read_to_string(cache_path(file))?;
    Ok(serde_json::from_str(&raw)?)
}

fn write_cache(file: &str, cache: &IndexCache) -> anyhow::Result<()> {
    let path = cache_path(file);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string(cache)?)?;
    Ok(())
}

/// Fetch the plugin index (plugin rows only — the legacy surface).
pub async fn fetch_index(client: &reqwest::Client) -> anyhow::Result<Index> {
    fetch_index_url(client, &index_url(), "index-cache.json").await
}

/// Fetch the hub index (every package kind; each row carries `kind`).
pub async fn fetch_hub_index(client: &reqwest::Client) -> anyhow::Result<Index> {
    fetch_index_url(client, &hub_index_url(), "hub-index-cache.json").await
}

/// Fetch an index, using the on-disk cache when fresh (<24h) and
/// revalidating with `If-None-Match` otherwise (304 keeps the cache).
async fn fetch_index_url(
    client: &reqwest::Client,
    url: &str,
    cache_file: &str,
) -> anyhow::Result<Index> {
    let cached = read_cache(cache_file).ok();
    if let Some(c) = &cached
        && crate::now_secs().saturating_sub(c.fetched_at) < INDEX_TTL_SECS
    {
        return Ok(c.index.clone());
    }
    let mut req = client.get(url);
    if let Some(etag) = cached.as_ref().and_then(|c| c.etag.as_deref()) {
        req = req.header(reqwest::header::IF_NONE_MATCH, etag);
    }
    log::debug!("fetching plugin index from {}", redact(url));
    let resp = req.send().await?;
    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        let mut cache =
            cached.ok_or_else(|| anyhow::anyhow!("index revalidated but no cache present"))?;
        cache.fetched_at = crate::now_secs();
        let index = cache.index.clone();
        write_cache(cache_file, &cache)?;
        return Ok(index);
    }
    let resp = resp.error_for_status()?;
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let index: Index = resp.json().await?;
    write_cache(
        cache_file,
        &IndexCache {
            etag,
            fetched_at: crate::now_secs(),
            index: index.clone(),
        },
    )?;
    Ok(index)
}

#[path = "index_tests.rs"]
#[cfg(test)]
mod tests;
