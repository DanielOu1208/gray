use super::*;
use crate::Cli;
use clap::Parser;

#[test]
fn exec_prefix_prefers_the_environment_over_the_saved_file() {
    let cli = Cli::parse_from(["gray"]);
    let config = Config::resolve_with(&cli, |k| {
        (k == "GRAY_EXEC_PREFIX").then(|| "sh -s".to_string())
    })
    .expect("config resolves");
    assert_eq!(config.exec_prefix.as_deref(), Some("sh -s"));
}

#[test]
fn a_blank_exec_prefix_is_no_exec_prefix() {
    let cli = Cli::parse_from(["gray"]);
    let config = Config::resolve_with(&cli, |k| {
        (k == "GRAY_EXEC_PREFIX").then(|| "   ".to_string())
    })
    .expect("config resolves");
    assert_eq!(config.exec_prefix, None);
}

#[test]
fn no_exec_prefix_means_local_commands() {
    let cli = Cli::parse_from(["gray"]);
    let config = Config::resolve_with(&cli, |_| None).expect("config resolves");
    assert_eq!(config.exec_prefix, None);
}

#[test]
fn bare_comes_from_the_flag_or_env() {
    let plain = Cli::parse_from(["gray"]);
    assert!(
        !Config::resolve_with(&plain, |_| None)
            .expect("config resolves")
            .bare
    );
    let flag = Cli::parse_from(["gray", "--bare"]);
    assert!(
        Config::resolve_with(&flag, |_| None)
            .expect("config resolves")
            .bare
    );
    let env = Config::resolve_with(&plain, |k| (k == "GRAY_BARE").then(|| "1".to_string()))
        .expect("config resolves");
    assert!(env.bare);
}

#[test]
fn lean_defaults_on_with_flag_and_env_overrides() {
    let plain = Cli::parse_from(["gray"]);
    let flag = Cli::parse_from(["gray", "--lean"]);
    let env0 = |k: &str| (k == "GRAY_LEAN").then(|| "0".to_string());
    let env1 = |k: &str| (k == "GRAY_LEAN").then(|| "1".to_string());
    assert!(
        !Config::resolve_with(&plain, env0)
            .expect("config resolves")
            .lean
    );
    assert!(
        Config::resolve_with(&flag, env0)
            .expect("config resolves")
            .lean
    );
    assert!(
        Config::resolve_with(&plain, env1)
            .expect("config resolves")
            .lean
    );
    assert!(
        Config::resolve_with(&plain, |_| None)
            .expect("config resolves")
            .lean
    );
}

fn key_with(env_key: &str, env_val: &str) -> impl FnMut(&str) -> Option<String> {
    let (k, v) = (env_key.to_string(), env_val.to_string());
    move |name| (name == k).then(|| v.clone())
}

#[test]
fn openai_env_key_is_ignored_on_the_default_openrouter_base() {
    let cli = Cli::parse_from(["gray"]);
    let key = resolve_api_key(
        &cli,
        DEFAULT_BASE_URL,
        &mut key_with("OPENAI_API_KEY", "sk-oai"),
        None,
    );
    assert_eq!(key, None);

    let key = resolve_api_key(
        &cli,
        " https://openrouter.ai/api/v1/ ",
        &mut key_with("OPENAI_API_KEY", "sk-oai"),
        None,
    );
    assert_eq!(key, None);
}

#[test]
fn the_saved_key_beats_openai_env_key() {
    let cli = Cli::parse_from(["gray"]);
    let key = resolve_api_key(
        &cli,
        DEFAULT_BASE_URL,
        &mut key_with("OPENAI_API_KEY", "sk-oai"),
        Some("sk-stored".to_string()),
    );
    assert_eq!(key.as_deref(), Some("sk-stored"));
}

#[test]
fn openai_env_key_applies_to_the_openai_base() {
    let cli = Cli::parse_from(["gray"]);
    let key = resolve_api_key(
        &cli,
        "https://api.openai.com/v1",
        &mut key_with("OPENAI_API_KEY", "sk-oai"),
        None,
    );
    assert_eq!(key.as_deref(), Some("sk-oai"));
}

#[test]
fn gray_api_key_beats_the_saved_key() {
    let cli = Cli::parse_from(["gray"]);
    let key = resolve_api_key(
        &cli,
        DEFAULT_BASE_URL,
        &mut key_with("GRAY_API_KEY", "sk-gray"),
        Some("sk-stored".to_string()),
    );
    assert_eq!(key.as_deref(), Some("sk-gray"));
}

#[test]
fn desktop_bus_replaces_only_a_dead_bus() {
    let home = tempfile::tempdir().unwrap();
    let sock = home.path().join("bus");
    std::fs::write(&sock, "").unwrap();
    let dir = home.path().join(".dbus/session-bus");
    std::fs::create_dir_all(&dir).unwrap();
    let addr = format!("unix:path={},guid=abc", sock.display());
    std::fs::write(
        dir.join("m-0"),
        format!("# comment\nDBUS_SESSION_BUS_ADDRESS='{addr}'\nDBUS_SESSION_BUS_PID=1\n"),
    )
    .unwrap();
    let dead = "unix:path=/nonexistent/gray-bus";
    assert_eq!(desktop_bus(Some(dead), home.path()), Some(addr.clone()));
    assert_eq!(desktop_bus(None, home.path()), Some(addr.clone()));
    assert_eq!(desktop_bus(Some(&addr), home.path()), None);

    std::fs::remove_file(&sock).unwrap();
    assert_eq!(desktop_bus(Some(dead), home.path()), None);
}

#[test]
fn remembered_effort_prefers_the_target_memory_over_the_flat_level() {
    let mut saved = crate::setup::SavedConfig {
        thinking_effort: Some("high".to_string()),
        ..Default::default()
    };
    saved.effort_memory.insert(
        crate::setup::effort_memory_key("devin-sub", "https://127.0.0.1:1/", "swe-2"),
        "max".to_string(),
    );
    assert_eq!(
        remembered_effort(
            &saved.effort_memory,
            "devin-sub",
            "https://127.0.0.1:1/",
            Some("swe-2")
        )
        .as_deref(),
        Some("max")
    );

    assert_eq!(
        remembered_effort(
            &saved.effort_memory,
            "devin-sub",
            "https://127.0.0.1:1/",
            Some("fusion")
        ),
        None
    );
    assert_eq!(
        remembered_effort(
            &saved.effort_memory,
            "devin-sub",
            "https://127.0.0.1:1/",
            None
        ),
        None
    );
    assert_eq!(
        remembered_effort(
            &saved.effort_memory,
            "devin-sub",
            "https://127.0.0.1:1/",
            Some("  ")
        ),
        None
    );

    saved.effort_memory.insert(
        crate::setup::effort_memory_key("", "https://kenari.id/v1", "claude-sonnet-5-5"),
        "medium".to_string(),
    );
    assert_eq!(
        remembered_effort(
            &saved.effort_memory,
            "",
            "https://kenari.id/v1/",
            Some("claude-sonnet-5-5")
        )
        .as_deref(),
        Some("medium")
    );
}
