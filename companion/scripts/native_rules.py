"""Pure rule renames for historical diagnostic comparisons, never product output."""

from __future__ import annotations

RENAMED_RULES = {
    "missing-attribute": "unresolved-attribute",
    "incompatible-attribute-override": "invalid-attribute-override",
    "incompatible-mutable-override": "invalid-mutable-override",
    "incompatible-method-override": "invalid-method-override",
    "incompatible-property-override": "invalid-property-type-override",
}


def semantic_id(code):
    return RENAMED_RULES.get(code, code)


def semantic_description(code, description):
    canonical = semantic_id(code)
    prefix = f"{code}: "
    if canonical != code and description.startswith(prefix):
        return f"{canonical}: {description[len(prefix) :]}"
    return description
