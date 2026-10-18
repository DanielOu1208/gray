use crossterm::event::KeyCode;

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
