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
        // Only explicit returns retain the old identity after Ty 0.0.102's split.
        "invalid-explicit-return" => "invalid-return-type",
        "invalid-implicit-return-in-function-with-stub-body" => "empty-body",
        "unsound-return" => "unsound-return-statement",
        "abstract-method-call" => "call-abstract-method",
        "not-callable" => "call-non-callable",
        "call-to-function-with-unknown-signature" => "call-top-callable",
        "invalid-dataclass-field-order" => "dataclass-field-order",
        "implicit-string-concatenation-in-annotation" => {
            "implicit-concatenated-string-type-annotation"
        }
        _ => code,
    }
}

#[cfg(test)]
mod tests {
    use super::semantic_id;

    #[test]
    fn explicit_return_alias_does_not_collapse_split_rules() {
        assert_eq!(
            semantic_id("invalid-explicit-return"),
            "invalid-return-type"
        );
        for code in ["invalid-implicit-return", "invalid-generator-return-type"] {
            assert_eq!(semantic_id(code), code);
        }
    }
}
