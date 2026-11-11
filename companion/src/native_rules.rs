//! Semantic rule identities for internal checks across native Ty releases.
//!
//! This does not change diagnostic identifiers returned to CLI or LSP users.
pub(crate) fn semantic_id(code: &str) -> &str {
    match code {
        "missing-attribute" => "unresolved-attribute",
        "incompatible-attribute-override" => "invalid-attribute-override",
        "incompatible-mutable-override" => "invalid-mutable-override",
        "incompatible-method-override" => "invalid-method-override",
        "incompatible-property-override" => "invalid-property-type-override",
        _ => code,
    }
}
