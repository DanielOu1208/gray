use std::collections::BTreeMap;

use super::credential::{
    CredentialEnvelope, CredentialLease, CredentialMaterial, CredentialSource, SecretMap,
    StaticCredentialSource,
};

#[test]
fn credential_material_debug_redacts_every_value() {
    let value = CredentialMaterial {
        secrets: SecretMap::from_iter([("access_token", "test-access")]),
        metadata: BTreeMap::from([("account_id".into(), "acct_test".into())]),
        expires_at: Some(123),
    };
    let debug = format!("{value:?}");
    assert!(!debug.contains("test-access"));
    assert!(!debug.contains("acct_test"));
    assert!(debug.contains("redacted"));
}

#[test]
fn credential_material_round_trips_secret_values_without_debug_exposure() {
    let value = CredentialMaterial {
        secrets: SecretMap::from_iter([
            ("access_token", "test-access"),
            ("refresh_token", "test-refresh"),
        ]),
        metadata: BTreeMap::from([("account_id".into(), "acct_test".into())]),
        expires_at: Some(456),
    };
    let encoded = serde_json::to_string(&value).unwrap();
    let decoded: CredentialMaterial = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.secrets.get("access_token"), Some("test-access"));
    assert_eq!(decoded.secrets.get("refresh_token"), Some("test-refresh"));
    assert!(!format!("{decoded:?}").contains("test-access"));
}

#[test]
fn envelope_binds_owner_and_profile_without_becoming_a_credential_log() {
    let envelope = CredentialEnvelope::new(
        "codex-auth",
        "codex",
        "chatgpt-subscription",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        CredentialMaterial {
            secrets: SecretMap::from_iter([("access_token", "test-access")]),
            metadata: BTreeMap::new(),
            expires_at: Some(789),
        },
    )
    .unwrap();
    let debug = format!("{envelope:?}");
    assert!(!debug.contains("test-access"));
    assert!(debug.contains("redacted"));
}

#[tokio::test]
async fn static_credential_source_returns_a_lease() {
    let source = StaticCredentialSource::new("access_token", "test-access");
    let lease = source.acquire().await.unwrap();
    assert_eq!(lease.secrets.get("access_token"), Some("test-access"));
    assert_eq!(lease.metadata.len(), 0);
    let _: CredentialLease = lease;
}

#[test]
fn envelope_rejects_oversized_credential_payload() {
    let material = CredentialMaterial {
        secrets: super::credential::SecretMap::from_iter([(
            "access_token",
            "x".repeat(super::credential::MAX_CREDENTIAL_BYTES),
        )]),
        ..CredentialMaterial::default()
    };
    assert!(CredentialEnvelope::new("p", "d", "a", "b", material).is_err());
}
