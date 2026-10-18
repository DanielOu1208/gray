#[test]
fn empty_consent_is_not_grandfathered() {
    let record = super::policy::grant(&[]);
    assert!(record.granted.is_empty());
    assert_eq!(record.hash, gray_plugin::capabilities::consent_hash(&[]));
}

#[test]
fn unknown_capabilities_are_not_granted() {
    let record = super::policy::grant(&[
        gray_plugin::capabilities::HOST_ASK.to_string(),
        "invented.capability".into(),
    ]);
    assert_eq!(
        record.granted,
        vec![gray_plugin::capabilities::HOST_ASK.to_string()]
    );
}

#[test]
fn consent_normalizes_known_ids_and_displays_unknowns() {
    let declared = vec![" host.ask ".into(), "host.ask".into(), "invented".into()];
    let consent = super::policy::grant(&declared);
    assert_eq!(consent.granted, ["host.ask"]);
    assert_eq!(
        consent.hash,
        gray_plugin::capabilities::consent_hash(&["host.ask".into()])
    );
    let lines = super::policy::capability_lines(&declared);
    assert_eq!(lines.len(), 3);
    assert!(lines[0].grantable);
    assert_eq!(
        lines[0].description,
        gray_plugin::capabilities::spec("host.ask")
            .unwrap()
            .description
    );
    assert_eq!(lines[2].id, "invented");
    assert!(!lines[2].grantable);
}

#[test]
fn scan_blocks_only_dangerous_and_retains_caution_findings() {
    use gray_plugin::scan::{Finding, Report, Severity};
    let mut report = Report::default();
    super::policy::require_installable_scan(&report).unwrap();
    report.findings.push(Finding {
        rule: "fixture",
        severity: Severity::Caution,
        path: "plugin.sh".into(),
        line: 1,
        detail: "review this finding".into(),
    });
    super::policy::require_installable_scan(&report).unwrap();
    assert!(report.render().contains("review this finding"));
    report.findings[0].severity = Severity::Critical;
    let error = super::policy::require_installable_scan(&report).unwrap_err();
    assert!(error.to_string().contains("blocked installation"));
}
