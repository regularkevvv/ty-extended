"""Pure rule renames for historical diagnostic comparisons, never product output."""

from __future__ import annotations

RENAMED_RULES = {
    "missing-attribute": "unresolved-attribute",
    "incompatible-attribute-override": "invalid-attribute-override",
    "incompatible-mutable-override": "invalid-mutable-override",
    "incompatible-method-override": "invalid-method-override",
    "incompatible-property-override": "invalid-property-type-override",
    # Explicit returns alone retain the old identity after Ty 0.0.102's split.
    "invalid-explicit-return": "invalid-return-type",
    "invalid-implicit-return-in-function-with-stub-body": "empty-body",
    "unsound-return": "unsound-return-statement",
    "abstract-method-call": "call-abstract-method",
    "not-callable": "call-non-callable",
    "call-to-function-with-unknown-signature": "call-top-callable",
    "invalid-dataclass-field-order": "dataclass-field-order",
    "implicit-string-concatenation-in-annotation": "implicit-concatenated-string-type-annotation",
}


def semantic_id(code):
    return RENAMED_RULES.get(code, code)


def semantic_description(code, description):
    canonical = semantic_id(code)
    prefix = f"{code}: "
    if canonical != code and description.startswith(prefix):
        return f"{canonical}: {description[len(prefix) :]}"
    return description
