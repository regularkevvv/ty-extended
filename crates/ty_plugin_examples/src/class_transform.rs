//! Example: a class-transform plugin.
//!
//! It models a toy base class `toy.Model`: any subclass gets its annotated class-body fields
//! promoted into instance attributes and a matching keyword-only constructor, in the style of a
//! dataclass/`pydantic`-shaped library.

use ty_plugin_sdk::dsl::{self, ClassPatchBuilder};
use ty_plugin_sdk::protocol::{
    AnalyzeClassRequest, Parameter, PluginManifest, PluginResponse, TypeExpr,
};
use ty_plugin_sdk::{ManifestBuilder, Plugin};

/// The base class whose subclasses this example transforms.
pub const MODEL_BASE: &str = "toy.Model";

/// A plugin that synthesizes fields and a keyword constructor for `toy.Model` subclasses.
#[derive(Debug, Default, Clone, Copy)]
pub struct ModelClassTransformPlugin;

impl Plugin for ModelClassTransformPlugin {
    fn manifest(&self) -> PluginManifest {
        ManifestBuilder::new("example.model", "Toy model transform", "0.1.0")
            .claim_class_transform(MODEL_BASE)
            .build()
    }

    fn analyze_class(&self, request: &AnalyzeClassRequest) -> PluginResponse {
        // Only transform classes that actually derive from the claimed base. The host routes by
        // claim, but a plugin should still verify the shape it was handed.
        if !derives_from_model(request) {
            return PluginResponse::NoChange;
        }

        let mut patch = ClassPatchBuilder::new();
        let mut parameters: Vec<Parameter> = Vec::new();

        for field in &request.class.fields {
            let Some(annotation) = field.annotation.clone() else {
                continue;
            };

            let parameter = dsl::keyword_only(&field.name, annotation.clone());
            let parameter = if field.has_default {
                dsl::optional(parameter)
            } else {
                parameter
            };

            let mut field_patch = dsl::field_with_parameter(
                &field.name,
                annotation.clone(),
                Some(annotation),
                Some(parameter.clone()),
            );
            field_patch.has_default = field.has_default;
            patch = patch.field(field_patch);
            parameters.push(parameter);
        }

        patch
            .constructor(dsl::signature(parameters, TypeExpr::expression("Self")))
            .response()
    }
}

fn derives_from_model(request: &AnalyzeClassRequest) -> bool {
    request
        .class
        .bases
        .iter()
        .any(|base| base.expression == MODEL_BASE)
}
