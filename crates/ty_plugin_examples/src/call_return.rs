//! Example: a call-return plugin.
//!
//! It models a field-specifier helper `toy.Field(...)`. At runtime the helper returns a sentinel
//! object, but for type checking the call should be seen as producing the field's value type. This
//! example overrides the call's return type to `str` to stand in for that behavior.

use ty_plugin_sdk::dsl;
use ty_plugin_sdk::protocol::{CallRequest, PluginManifest, PluginResponse, TypeExpr};
use ty_plugin_sdk::{ManifestBuilder, Plugin};

/// The field-specifier function this example claims.
pub const FIELD_FUNCTION: &str = "toy.Field";

/// A plugin that rewrites the return type of `toy.Field(...)` calls.
#[derive(Debug, Default, Clone, Copy)]
pub struct FieldCallReturnPlugin;

impl Plugin for FieldCallReturnPlugin {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("example.field", "Toy field return", "0.1.0")
            .claim_call_return(FIELD_FUNCTION)
            .build()
    }

    fn adjust_call_return(&self, _request: &CallRequest) -> PluginResponse {
        dsl::call_return(TypeExpr::annotation("str"))
    }
}
