use super::*;

#[test]
fn schema_has_single_scalar_types_paths_exclude_and_empty_required() {

    let def = ReadTool::default().def();
    let props = def
        .parameters
        .get("properties")
        .and_then(|p| p.as_object())
        .expect("properties");
    for (name, schema) in props {
        let t = schema
            .get("type")
            .unwrap_or_else(|| panic!("property {name} missing scalar type"));
        assert!(
            t.is_string(),
            "property {name} must have exactly one scalar type (no unions), got {t}"
        );
    }
}
