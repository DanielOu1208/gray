use serde_json::{Value, json};

use super::provider::ProviderDecl;
use crate::Manifest;

fn valid_provider_value() -> Value {
    json!({
        "id": "good",
        "name": "Good provider",
        "transport": {
            "kind": "openai-responses",
            "base_url": "https://example.test/v1",
            "authorization": {
                "kind": "bearer",
                "secret_name": "access_token"
            },
            "request": {
                "prompt_cache_key": false,
                "store": false,
                "include_reasoning_encrypted": true,
                "previous_response_id": false,
                "tool_choice": "auto",
                "parallel_tool_calls": true,
                "text_verbosity": "low"
            },
            "headers": [
                {"name": "X-Static", "value": "ok", "required": false},
                {
                    "name": "X-Account",
                    "source": {"kind": "metadata", "name": "account_id"},
                    "required": true
                },
                {
                    "name": "X-Session",
                    "source": {"kind": "session_id"},
                    "required": false
                }
            ]
        },
        "auth_methods": [{
            "id": "oauth",
            "name": "OAuth",
            "kind": "oauth",
            "operations": ["login", "refresh", "revoke", "models"]
        }]
    })
}

fn valid_provider_decl() -> ProviderDecl {
    ProviderDecl::from_value(&valid_provider_value()).unwrap()
}

#[test]
fn protocol_1_2_manifest_exposes_valid_provider() {
    let manifest = Manifest::from_result(&json!({
        "name": "provider-fixture",
        "version": "0.1.0",
        "protocol": "1.2",
        "capabilities": ["provider.credentials"],
        "providers": [valid_provider_value()]
    }));
    assert_eq!(manifest.providers.len(), 1);
    assert_eq!(manifest.providers[0].id, "good");
    assert!(manifest.provider_errors.is_empty());
}

#[test]
fn invalid_provider_does_not_hide_a_valid_peer() {
    let mut invalid = valid_provider_value();
    invalid["transport"]["headers"][0]["value"] = json!("bad\r\nInjected");
    let manifest = Manifest::from_result(&json!({
        "name": "provider-fixture",
        "version": "0.1.0",
        "protocol": "1.2",
        "providers": [invalid, valid_provider_value()]
    }));
    assert_eq!(manifest.providers.len(), 1);
    assert_eq!(manifest.providers[0].id, "good");
    assert_eq!(manifest.provider_errors.len(), 1);
}

#[test]
fn protocol_1_1_manifest_has_no_provider_surface() {
    let manifest = Manifest::from_result(&json!({
        "name": "legacy-fixture",
        "version": "0.1.0",
        "protocol": "1.1",
        "providers": [valid_provider_value()]
    }));
    assert!(manifest.providers.is_empty());
    assert_eq!(manifest.provider_errors.len(), 1);
}

#[test]
fn profile_binding_changes_when_request_policy_changes() {
    let a = valid_provider_decl();
    let mut b = a.clone();
    b.transport.request.parallel_tool_calls = Some(false);
    assert_ne!(
        a.profile_binding("oauth").unwrap(),
        b.profile_binding("oauth").unwrap()
    );
}

#[test]
fn duplicate_provider_ids_keep_one_provider_and_report_one_error() {
    let manifest = Manifest::from_result(&json!({
        "name": "provider-fixture",
        "version": "0.1.0",
        "protocol": "1.2",
        "providers": [valid_provider_value(), valid_provider_value()]
    }));
    assert_eq!(manifest.providers.len(), 1);
    assert_eq!(manifest.provider_errors.len(), 1);
}

#[test]
fn malformed_provider_list_is_reported_without_breaking_legacy_fields() {
    let manifest = Manifest::from_result(&json!({
        "name": "provider-fixture",
        "version": "0.1.0",
        "protocol": "1.2",
        "commands": ["/still-live"],
        "providers": {"not": "an array"}
    }));
    assert!(manifest.providers.is_empty());
    assert_eq!(manifest.provider_errors.len(), 1);
    assert_eq!(manifest.commands, vec!["/still-live"]);
}

#[test]
fn profile_binding_normalizes_default_ports_and_trailing_slashes() {
    let mut with_port = valid_provider_decl();
    with_port.transport.base_url = "https://EXAMPLE.test:443/v1/".parse().unwrap();
    let mut without_port = valid_provider_decl();
    without_port.transport.base_url = "https://example.test/v1".parse().unwrap();
    assert_eq!(
        with_port.profile_binding("oauth").unwrap(),
        without_port.profile_binding("oauth").unwrap()
    );
}

#[test]
fn provider_declaration_limit_is_checked_before_acceptance() {
    let mut value = valid_provider_value();
    value["name"] = json!("x".repeat(70 * 1024));
    assert!(ProviderDecl::from_value(&value).is_err());
}

#[test]
fn invalid_url_and_request_policy_are_rejected() {
    let mut bad_url = valid_provider_value();
    bad_url["transport"]["base_url"] = json!("http://example.test/v1");
    assert!(ProviderDecl::from_value(&bad_url).is_err());

    let mut bad_policy = valid_provider_value();
    bad_policy["transport"]["request"]["tool_choice"] = json!("bogus");
    assert!(ProviderDecl::from_value(&bad_policy).is_err());
}
