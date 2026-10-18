use crossterm::event::KeyCode;

fn snapshot(model: &super::model::MarketModel, width: u16, height: u16) -> String {
    snapshot_with_consent(model, width, height, None)
}

fn snapshot_with_consent(
    model: &super::model::MarketModel,
    width: u16,
    height: u16,
    consent: Option<&super::ConsentView>,
) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| super::draw::render(frame, frame.area(), model, consent))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn loading_snapshot_has_no_install_action() {
    let screen = snapshot(&super::model::MarketModel::default(), 80, 20);
    assert!(screen.contains("Loading marketplace"));
    assert!(!screen.contains("[ Install ]"));
}

fn index(names: &[&str]) -> gray_pkg::index::Index {
    let plugins = names
        .iter()
        .map(|name| {
            (
                name.to_string(),
                serde_json::json!({
                    "ecosystem": "gray-native", "version": "1.0.0",
                    "source": {"type": "tarball", "url": "https://example.invalid/demo.tgz"},
                    "hash": format!("sha256:{}", "0".repeat(64)),
                    "description": format!("{name} helper")
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    serde_json::from_value(serde_json::json!({"plugins": plugins})).unwrap()
}

#[test]
fn search_owns_j_and_k_and_installed_uses_catalog_key() {
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["jack", "ledger"]), ["jack".to_string()].into());
    model.search_focused = true;
    model.input(KeyCode::Char('j'));
    assert_eq!(model.query, "j");
    assert_eq!(model.visible_names(), vec!["jack"]);
    assert!(!model.can_install());
    model.input(KeyCode::Backspace);
    model.input(KeyCode::Char('k'));
    assert_eq!(model.query, "k");
}

#[test]
fn scrolling_keeps_last_selection_visible() {
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["a", "b", "c", "d", "e"]), Default::default());
    for _ in 0..4 {
        model.move_selection(1, 2);
    }
    assert_eq!(model.selected, 4);
    assert_eq!(model.offset, 3);
}

#[test]
fn stale_completion_cannot_advance_a_new_operation() {
    let mut model = super::model::MarketModel::default();
    let old = model.begin(super::model::MarketPhase::Preparing);
    let current = model.begin(super::model::MarketPhase::Loading);
    assert!(!model.accept_operation(old));
    assert!(model.accept_operation(current));
}

#[test]
fn description_search_is_case_insensitive_and_resets_selection() {
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["a", "b"]), Default::default());
    model.entries.get_mut("b").unwrap().description = Some("JOURNAL".into());
    model.move_selection(1, 1);
    model.search_focused = true;
    for c in "journal".chars() {
        model.input(KeyCode::Char(c));
    }
    assert_eq!(model.visible_names(), vec!["b"]);
    assert_eq!((model.selected, model.offset), (0, 0));
    model.input(KeyCode::Char('z'));
    model.move_selection(1, 0);
    assert!(model.visible_names().is_empty());
    assert!(!model.entries.is_empty());
    assert!(!model.can_install());
    assert_eq!((model.selected, model.offset), (0, 0));
}

#[test]
fn installed_identity_ignores_version_runtime_name_and_enabled_state() {
    let mut model = super::model::MarketModel::default();
    // Registry loading passes every key, independent of each row's metadata.
    model.loaded(index(&["demo"]), ["demo".into(), "local".into()].into());
    model.details = true;
    assert!(!model.can_install());
    model.input(KeyCode::Char('i'));
    assert_eq!(model.phase, super::model::MarketPhase::Browsing);
}

#[test]
fn confirmation_snapshot_is_locked_and_consent_requires_explicit_grant() {
    use super::model::{Action, MarketPhase};
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["demo"]), Default::default());
    model.input(KeyCode::Enter);
    assert!(model.details);
    assert_eq!(model.phase, MarketPhase::Browsing);
    model.input(KeyCode::Enter);
    assert_eq!(model.phase, MarketPhase::Confirm);
    model.entries.get_mut("demo").unwrap().version = "2.0.0".into();
    assert_eq!(model.approved.as_ref().unwrap().entry().version, "1.0.0");
    assert_eq!(model.input(KeyCode::Enter), Action::Prepare);
    model.phase = MarketPhase::Consent;
    assert_eq!(model.input(KeyCode::Enter), Action::Cancel);
    assert_eq!(model.input(KeyCode::Char('g')), Action::Grant);
    model.phase = MarketPhase::Committing;
    assert_eq!(model.input(KeyCode::Esc), Action::None);
    assert!(model.blocks_tabs());
}

#[test]
fn escape_leaves_search_then_details_before_closing() {
    use super::model::Action;
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["demo"]), Default::default());
    model.details = true;
    model.search_focused = true;
    assert_eq!(model.input(KeyCode::Esc), Action::None);
    assert!(model.details);
    assert_eq!(model.input(KeyCode::Esc), Action::None);
    assert!(!model.details);
    assert_eq!(model.input(KeyCode::Esc), Action::Close);
}

fn compact(screen: &str) -> Vec<&str> {
    screen.lines().map(str::trim_end).collect()
}

#[test]
fn stable_status_buffer_snapshots() {
    use super::model::{MarketModel, MarketPhase};
    let mut model = MarketModel::default();
    assert_eq!(
        compact(&snapshot(&model, 40, 5)),
        ["Loading marketplace…", "", "", "", ""]
    );
    model.phase = MarketPhase::LoadError;
    model.message = Some("HTTP 503".into());
    assert_eq!(
        compact(&snapshot(&model, 40, 5)),
        [
            "Marketplace unavailable",
            "HTTP 503",
            "",
            "",
            "[ Retry: r ]  [ Back: Esc ]"
        ]
    );
    model.loaded(index(&[]), Default::default());
    assert_eq!(
        compact(&snapshot(&model, 40, 5)),
        [
            "No plugins available",
            "",
            "",
            "",
            "/ search · ↑↓/jk select · Enter details"
        ]
    );
    model.loaded(index(&["demo"]), Default::default());
    model.query = "absent".into();
    assert_eq!(compact(&snapshot(&model, 40, 5))[0], "No matching plugins");
}

#[test]
fn browse_and_details_show_trust_installed_badges_and_metadata() {
    let mut model = super::model::MarketModel::default();
    let mut catalog = index(&["demo"]);
    let entry = catalog.plugins.get_mut("demo").unwrap();
    entry.verified = true;
    entry.author = Some("Builder".into());
    entry.kind = "plugin".into();
    entry.commands = vec!["/demo".into()];
    entry.license = "MIT".into();
    entry.homepage = Some("https://example.invalid/home".into());
    entry.repo = Some("https://example.invalid/repo".into());
    model.loaded(catalog, ["demo".into()].into());
    assert!(snapshot(&model, 80, 20).contains("demo · verified community [installed]"));
    model.details = true;
    let screen = snapshot(&model, 80, 20);
    for field in [
        "demo [installed]",
        "demo helper",
        "Author: Builder",
        "Version: 1.0.0",
        "Trust: verified community",
        "Kind: plugin",
        "Commands: /demo",
        "License: MIT",
        "Homepage: https://example.invalid/home",
        "Repo: https://example.invalid/repo",
        "[ Install disabled ]",
    ] {
        assert!(screen.contains(field), "{field} missing:\n{screen}");
    }
}

fn consent_fixture() -> super::ConsentView {
    super::ConsentView {
        capabilities: crate::plugin_install::policy::capability_lines(&[
            "host.ask".into(),
            "unknown.capability".into(),
        ]),
        commands: vec!["/demo".into()],
        scan_text: "Caution: shell invocation\nreview plugin.sh:7".into(),
    }
}

#[test]
fn confirmation_and_consent_render_the_approved_information() {
    use super::model::MarketPhase;
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["demo"]), Default::default());
    model.confirm();
    let screen = snapshot(&model, 100, 20);
    for expected in [
        "Confirm preparation: demo",
        "Version: 1.0.0",
        "Source: https://example.invalid/demo.tgz",
        "Hash: sha256:",
        "Trust: unverified community",
        "A matching hash proves integrity, not safety.",
        "Preparing will run this plugin's manifest command as you.",
        "[ Approve: Enter ]",
        "[ Back: Esc ]",
    ] {
        assert!(screen.contains(expected), "{expected} missing:\n{screen}");
    }
    model.phase = MarketPhase::Consent;
    let consent = consent_fixture();
    let screen = snapshot_with_consent(&model, 100, 20, Some(&consent));
    for expected in [
        "Capability consent",
        "host.ask:",
        "unknown.capability [not grantable]",
        "Commands: /demo",
        "Caution: shell invocation",
        "review plugin.sh:7",
        "[ Grant: g ]",
        "[ Cancel: Enter/Esc ]",
    ] {
        assert!(screen.contains(expected), "{expected} missing:\n{screen}");
    }
}

#[test]
fn progress_success_and_failure_buffers() {
    use super::model::MarketPhase;
    use gray_pkg::staged::InstallStage;
    let mut model = super::model::MarketModel {
        phase: MarketPhase::Preparing,
        stage: Some(InstallStage::Scanning),
        ..Default::default()
    };
    assert_eq!(
        compact(&snapshot(&model, 60, 5)),
        [
            "⠋ Scanning",
            "",
            "",
            "",
            "Esc cancel · cleanup finishes before leaving"
        ]
    );
    model.phase = MarketPhase::Committing;
    model.stage = Some(InstallStage::Committing);
    assert!(snapshot(&model, 80, 8).contains("cancellation disabled"));
    model.phase = MarketPhase::Installed;
    model.message = Some("demo installed\nWarning: provider refresh failed".into());
    let screen = snapshot(&model, 80, 8);
    assert!(screen.contains("Installed"));
    assert!(screen.contains("Warning: provider refresh failed"));
    assert!(!screen.contains("Install failed"));
    model.phase = MarketPhase::InstallFailed;
    model.message = Some("missing required plugins: questions, ledger".into());
    let screen = snapshot(&model, 80, 8);
    assert!(screen.contains("missing required plugins: questions, ledger"));
    assert!(screen.contains("[ Retry: r ]"));
}

#[test]
fn all_phases_render_on_narrow_and_tiny_terminals() {
    use super::model::MarketPhase::*;
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["demo"]), Default::default());
    model.confirm();
    let consent = consent_fixture();
    for phase in [
        Loading,
        LoadError,
        Browsing,
        Confirm,
        Preparing,
        Consent,
        Committing,
        Installed,
        InstallFailed,
    ] {
        model.phase = phase;
        model.message = Some("example message".into());
        for (width, height) in [(20, 6), (80, 20), (1, 1), (0, 0)] {
            snapshot_with_consent(&model, width, height, Some(&consent));
        }
    }
}

#[test]
fn final_row_stays_visible_and_long_text_wraps_and_scrolls() {
    let names = (0..150)
        .map(|i| format!("plugin-{i:03}"))
        .collect::<Vec<_>>();
    let mut model = super::model::MarketModel::default();
    model.loaded(
        index(&names.iter().map(String::as_str).collect::<Vec<_>>()),
        Default::default(),
    );
    model.move_selection(149, 15);
    assert!(snapshot(&model, 80, 20).contains("> plugin-149"));
    model.details = true;
    model.entries.get_mut("plugin-149").unwrap().description = Some("a".repeat(200));
    let screen = snapshot(&model, 20, 10);
    assert!(screen.contains("aaaaaaaaaaaaaaaaaaaa"));
    model.scroll = u16::MAX;
    let screen = snapshot(&model, 20, 10);
    assert!(
        compact(&screen)
            .join("")
            .contains("https://example.invalid/demo.tgz")
    );
    assert!(!screen.contains("plugin-149"));
}

#[test]
fn catalog_manifest_and_errors_cannot_inject_controls_or_url_credentials() {
    let mut model = super::model::MarketModel::default();
    model.loaded(index(&["demo"]), Default::default());
    let entry = model.entries.get_mut("demo").unwrap();
    entry.description = Some("\x1b[31mhello\x1b[0m\x07".into());
    entry.source.url = "https://user:secret@example.invalid/demo?token=private#secret".into();
    model.details = true;
    let screen = snapshot(&model, 100, 20);
    assert!(screen.contains("hello"));
    for absent in ["\x1b", "\x07", "secret", "private", "user:"] {
        assert!(!screen.contains(absent));
    }
    model.phase = super::model::MarketPhase::InstallFailed;
    model.message = Some("failed https://u:secret@example.invalid/a?token=private\x1b[0m".into());
    let screen = snapshot(&model, 100, 20);
    assert!(!screen.contains("secret"));
    assert!(!screen.contains("private"));
}
