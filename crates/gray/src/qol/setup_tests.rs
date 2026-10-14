use std::collections::BTreeMap;
use std::path::Path;

use super::*;

fn tool_with_creds(creds: &'static [CredSource]) -> ToolDef {
    ToolDef {
        id: "t",
        name: "T",
        dirs: &[],
        files: &[],
        bins: &[],
        rules: None,
        skills: None,
        plugin: None,
        mcp: None,
        creds: Some(creds),
        stale_probe: None,
    }
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

#[test]
fn dotenv_pairs_skip_comments_and_strip_quotes() {
    let pairs = dotenv_pairs(
        "# comment\n\nexport OPENROUTER_API_KEY=sk-or\nQUOTED=\"a b\"\nSINGLE='c'\n\
         EMPTY=\nINLINE=v1 # trailing\n",
    );
    let map: BTreeMap<_, _> = pairs.into_iter().collect();
    assert_eq!(map["OPENROUTER_API_KEY"], "sk-or");
    assert_eq!(map["QUOTED"], "a b");
    assert_eq!(map["SINGLE"], "c");
    assert_eq!(map["EMPTY"], "");
    assert_eq!(map["INLINE"], "v1");
}

#[test]
fn env_allowlist_maps_suffixes_only() {
    assert_eq!(
        env_cred_target("OPENROUTER_API_KEY").as_deref(),
        Some("openrouter")
    );
    assert_eq!(env_cred_target("HF_TOKEN").as_deref(), Some("hf"));
    assert_eq!(
        env_cred_target("TEAMS_CLIENT_SECRET").as_deref(),
        Some("teams-client")
    );
    assert_eq!(env_cred_target("BROWSERBASE_PROJECT_ID"), None);
    assert_eq!(env_cred_target("WEB_TOOLS_DEBUG"), None);
    assert_eq!(env_cred_target("_API_KEY"), None);
}

#[test]
fn provider_mapping_rewrites_qwen_aliases() {
    assert_eq!(map_provider("qwen-portal"), "qwen");
    assert_eq!(map_provider("qwen-oauth"), "qwen");
    assert_eq!(map_provider("nous"), "nous");
    assert_eq!(map_provider("WEIRD_NAME"), "weird-name");
}

#[test]
fn hermes_pool_finds_agent_key_and_marks_oauth_skipped() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        ".h/auth.json",
        r#"{"credential_pool":{
             "nous":[{"agent_key":"ak-nous","access_token":"at"}],
             "qwen-oauth":[{"access_token":"qa","expires_at_ms":1}],
             "copilot":[{"secret_fingerprint":"fp"}]},
           "providers":{"shared":{"agent_key":"ak-shared"}}}"#,
    );
    let def = tool_with_creds(&[CredSource::HermesAuth(".h/auth.json")]);
    let (items, skipped) = cred_candidates(&def, dir.path());
    let targets: Vec<_> = items.iter().map(|c| c.target.as_str()).collect();
    assert_eq!(targets, ["nous", "shared"]);
    assert_eq!(skipped, ["qwen-oauth (OAuth)"]);
}

#[test]
fn hermes_extract_reads_pool_then_providers_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("auth.json");
    write(
        dir.path(),
        "auth.json",
        r#"{"credential_pool":{"nous":[{"agent_key":"pool-key"}]},
           "providers":{"other":{"agent_key":"providers-key"}}}"#,
    );
    assert_eq!(
        CredExtract::PoolAgentKey {
            provider: "nous".into()
        }
        .read(&src)
        .unwrap(),
        "pool-key"
    );
    assert_eq!(
        CredExtract::PoolAgentKey {
            provider: "other".into()
        }
        .read(&src)
        .unwrap(),
        "providers-key"
    );
    assert!(
        CredExtract::PoolAgentKey {
            provider: "none".into()
        }
        .read(&src)
        .is_err()
    );
}

#[test]
fn openclaw_models_yields_api_key_providers() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "m/models.json",
        r#"{"providers":{"qwen-portal":{"apiKey":"k1","models":[]},"x":{"api":1}}}"#,
    );
    let def = tool_with_creds(&[CredSource::OpenclawModels("m/models.json")]);
    let (items, skipped) = cred_candidates(&def, dir.path());
    assert!(skipped.is_empty());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].target, "qwen");
    assert_eq!(items[0].extract.read(&items[0].src).unwrap(), "k1");
}

#[test]
fn openclaw_profiles_are_oauth_only_so_skipped() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "p/auth-profiles.json",
        r#"{"version":1,"profiles":{"qwen-portal:qwen-cli":{"type":"oauth","access":"a","refresh":"r"}}}"#,
    );
    let def = tool_with_creds(&[CredSource::OpenclawProfiles("p/auth-profiles.json")]);
    let (items, skipped) = cred_candidates(&def, dir.path());
    assert!(items.is_empty());
    assert_eq!(skipped, ["qwen-portal:qwen-cli (OAuth)"]);
}

#[test]
fn creds_roundtrip_store_then_undo_on_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "OPENROUTER_API_KEY=sk-test\n");
    let store = CredentialStore::new(dir.path().join("auth.json"));
    let mut state = crate::qol::QolState::default();
    let entries = vec![CredItem {
        target: "openrouter".into(),
        src: dir.path().join(".env"),
        extract: CredExtract::DotEnv {
            name: "OPENROUTER_API_KEY".into(),
        },
    }];
    let def = tool_with_creds(&[]);
    let msg = apply_creds_at(&store, &def, &entries, &mut state, "r1").unwrap();
    assert!(msg.contains("1 stored"));
    assert_eq!(
        store.load().unwrap()["openrouter"],
        AuthEntry::Key("sk-test".into())
    );
    let entry = &state.imports[0];
    assert_eq!(entry.kind, "creds");
    assert_eq!(entry.target, "openrouter");
    assert_eq!(entry.detail, cred_fp("sk-test"));

    let (done, msg) = undo_cred("openrouter", &entry.detail, &store);
    assert!(done, "{msg}");
    assert!(!store.load().unwrap().contains_key("openrouter"));

    let mut map = store.load().unwrap();
    map.insert("openrouter".into(), AuthEntry::Key("other".into()));
    store.replace(&map).unwrap();
    let (done, msg) = undo_cred("openrouter", &entry.detail, &store);
    assert!(done);
    assert!(msg.contains("left in place"), "{msg}");
    assert_eq!(
        store.load().unwrap()["openrouter"],
        AuthEntry::Key("other".into())
    );
}

#[test]
fn creds_apply_never_overwrites_existing_keys() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "K_API_KEY=new-value\n");
    let store = CredentialStore::new(dir.path().join("auth.json"));
    let mut map = BTreeMap::new();
    map.insert("k".into(), AuthEntry::Key("old-value".into()));
    store.replace(&map).unwrap();
    let mut state = crate::qol::QolState::default();
    let entries = vec![CredItem {
        target: "k".into(),
        src: dir.path().join(".env"),
        extract: CredExtract::DotEnv {
            name: "K_API_KEY".into(),
        },
    }];
    let def = tool_with_creds(&[]);
    let msg = apply_creds_at(&store, &def, &entries, &mut state, "r1").unwrap();
    assert!(msg.contains("0 stored"), "{msg}");
    assert!(msg.contains("k"), "{msg}");
    assert_eq!(
        store.load().unwrap()["k"],
        AuthEntry::Key("old-value".into())
    );
    assert!(state.imports.is_empty());
}

#[test]
fn age_desc_formats_stale_probe_ages() {
    assert_eq!(
        age_desc(std::time::Duration::from_secs(255 * 86_400)),
        Some("last used ~8mo ago".to_string())
    );
    assert_eq!(
        age_desc(std::time::Duration::from_secs(20 * 86_400)),
        Some("last used ~2w ago".to_string())
    );
    assert_eq!(age_desc(std::time::Duration::from_secs(3 * 86_400)), None);
}

#[test]
fn cred_source_ids_list_names_not_values() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "OPENROUTER_API_KEY=sk\nDEBUG=1\n");
    let ids = cred_source_ids(&CredSource::DotEnv(".env"), &dir.path().join(".env"));
    assert_eq!(ids, ["OPENROUTER_API_KEY"]);
}

#[cfg(unix)]
#[test]
fn skill_entries_skips_symlinked_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let real = dir.path().join("skills/real");
    let outside = dir.path().join("elsewhere/target");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(dir.path().join("skills/.hidden")).unwrap();
    std::os::unix::fs::symlink(&outside, dir.path().join("skills/linked")).unwrap();
    let entries = skill_entries(&dir.path().join("skills"));
    assert_eq!(entries, vec![real]);
}
