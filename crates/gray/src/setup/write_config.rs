//! Writing an app's config the way the app itself would: private (dir 0700,
//! file 0600), atomic (temp + rename), and never echoing a secret.

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Value};

use super::registry::SetupDecl;

/// What the user typed, keyed by the declaration's field names. Debug prints
/// declared secrets as `<secret>` so a panic message, a log line, or a test
/// failure can never leak one.
#[derive(Clone, Default)]
pub struct Supplied {
    values: HashMap<String, String>,
    secret_keys: Vec<String>,
}

impl Supplied {
    pub fn insert(&mut self, key: &str, value: String, secret: bool) {
        if secret && !self.secret_keys.iter().any(|k| k == key) {
            self.secret_keys.push(key.to_string());
        }
        self.values.insert(key.to_string(), value);
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

impl std::fmt::Debug for Supplied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut shown: Vec<(&String, &String)> = self.values.iter().collect();
        shown.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = f.debug_map();
        for (key, value) in shown {
            if self.secret_keys.iter().any(|k| k == key) {
                out.entry(key, &"<secret>");
            } else {
                out.entry(key, value);
            }
        }
        out.finish()
    }
}

/// Merge the supplied answers plus the declaration's derived paths into the
/// app's config and write it privately and atomically. Unknown keys already
/// in the file survive; an unparseable file is replaced (the app's own
/// loader rejects it anyway).
pub fn write_config(
    config_path: &Path,
    decl: &SetupDecl,
    supplied: &Supplied,
    home: &Path,
) -> anyhow::Result<()> {
    let mut data = read_object(config_path);
    for (key, value) in &supplied.values {
        data.insert(key.clone(), Value::String(value.clone()));
    }
    for (key, value) in decl.derived(home) {
        data.insert(key.to_string(), Value::String(value));
    }
    atomic_write(config_path, &Value::Object(data))
}

fn read_object(path: &Path) -> Map<String, Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        },
        Err(_) => Map::new(),
    }
}

fn atomic_write(path: &Path, data: &Value) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|e| anyhow::anyhow!("cannot create the config directory: {e}"))?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(parent)
        .map_err(|e| anyhow::anyhow!("cannot create the config directory: {e}"))?;

    let mut text = serde_json::to_string_pretty(data)?;
    text.push('\n');
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, text.as_bytes())
        .map_err(|e| anyhow::anyhow!("cannot write the config: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("cannot protect the config: {e}"))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| anyhow::anyhow!("cannot replace the config: {e}"))?;
    Ok(())
}

#[path = "write_config_tests.rs"]
#[cfg(test)]
mod tests;
