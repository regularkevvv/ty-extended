//! Small constructors for the patch and signature shapes plugins return.
//!
//! These trim the boilerplate of building [`Parameter`], [`FieldPatch`], [`CallableSignature`],
//! and the response variants by hand. Type expressions themselves come from
//! [`TypeExpr::annotation`]/[`TypeExpr::expression`] on the protocol crate.

use ty_plugin_protocol::{
    CallReturnPatch, CallSignaturePatch, CallStatePatch, CallableSignature, ClassPatch, FieldPatch,
    MemberAccessPatch, MemberPatch, MemberPatchMode, Parameter, ParameterKind, PluginDiagnostic,
    PluginResponse, TypeExpr,
};

/// A required positional-or-keyword parameter with an annotated type.
#[must_use]
pub fn positional_or_keyword(name: impl Into<String>, ty: TypeExpr) -> Parameter {
    parameter(
        Some(name.into()),
        ParameterKind::PositionalOrKeyword,
        Some(ty),
        true,
    )
}

/// A required keyword-only parameter with an annotated type.
#[must_use]
pub fn keyword_only(name: impl Into<String>, ty: TypeExpr) -> Parameter {
    parameter(
        Some(name.into()),
        ParameterKind::KeywordOnly,
        Some(ty),
        true,
    )
}

/// Mark a parameter as optional (not required at the call site).
#[must_use]
pub fn optional(mut parameter: Parameter) -> Parameter {
    parameter.required = false;
    parameter
}

fn parameter(
    name: Option<String>,
    kind: ParameterKind,
    type_expr: Option<TypeExpr>,
    required: bool,
) -> Parameter {
    Parameter {
        name,
        kind,
        type_expr,
        required,
    }
}

/// A callable signature from its parameters and return type.
#[must_use]
pub fn signature(
    parameters: impl IntoIterator<Item = Parameter>,
    return_type: TypeExpr,
) -> CallableSignature {
    CallableSignature {
        parameters: parameters.into_iter().collect(),
        return_type,
    }
}

/// A field patch that is both stored on the instance and accepted by the constructor.
#[must_use]
pub fn init_field(name: impl Into<String>, ty: TypeExpr) -> FieldPatch {
    let name = name.into();
    FieldPatch {
        name: name.clone(),
        mode: MemberPatchMode::FillOnMiss,
        descriptor: None,
        instance_get_type: ty.clone(),
        instance_set_type: Some(ty.clone()),
        constructor_parameter: Some(keyword_only(name, ty)),
        has_default: false,
    }
}

/// A field patch with an explicit constructor parameter.
#[must_use]
pub fn field_with_parameter(
    name: impl Into<String>,
    get_type: TypeExpr,
    set_type: Option<TypeExpr>,
    constructor_parameter: Option<Parameter>,
) -> FieldPatch {
    FieldPatch {
        name: name.into(),
        mode: MemberPatchMode::FillOnMiss,
        descriptor: None,
        instance_get_type: get_type,
        instance_set_type: set_type,
        constructor_parameter,
        has_default: false,
    }
}

/// A member patch (writable, no diagnostics).
#[must_use]
pub fn member(name: impl Into<String>, ty: TypeExpr) -> MemberPatch {
    MemberPatch {
        name: name.into(),
        mode: MemberPatchMode::FillOnMiss,
        access: MemberAccessPatch::value(ty),
        read_only: false,
        diagnostics: Vec::new(),
    }
}

/// A descriptor-aware member patch.
#[must_use]
pub fn descriptor_member(
    name: impl Into<String>,
    class_type: Option<TypeExpr>,
    get_type: TypeExpr,
    set_type: Option<TypeExpr>,
) -> MemberPatch {
    MemberPatch {
        name: name.into(),
        mode: MemberPatchMode::FillOnMiss,
        access: MemberAccessPatch::Descriptor {
            class_type,
            instance_get_type: get_type,
            instance_set_type: set_type,
        },
        read_only: false,
        diagnostics: Vec::new(),
    }
}

/// A callable generated instance member with a bound-call signature.
#[must_use]
pub fn callable_member(
    name: impl Into<String>,
    signature: CallableSignature,
    fallback_type: TypeExpr,
) -> MemberPatch {
    MemberPatch {
        name: name.into(),
        mode: MemberPatchMode::FillOnMiss,
        access: MemberAccessPatch::Callable {
            signature,
            fallback_type,
        },
        read_only: true,
        diagnostics: Vec::new(),
    }
}

/// Explicitly allow a field patch to replace an ordinary or inherited member.
#[must_use]
pub fn replace_existing_field(mut field: FieldPatch) -> FieldPatch {
    field.mode = MemberPatchMode::ReplaceExisting;
    field
}

/// Explicitly allow a member patch to replace an ordinary or inherited member.
#[must_use]
pub fn replace_existing_member(mut member: MemberPatch) -> MemberPatch {
    member.mode = MemberPatchMode::ReplaceExisting;
    member
}

/// A [`PluginResponse::CallReturnPatch`] overriding a call's return type (no diagnostics).
#[must_use]
pub fn call_return(return_type: TypeExpr) -> PluginResponse {
    PluginResponse::CallReturnPatch(CallReturnPatch {
        return_type,
        diagnostics: Vec::new(),
        result_metadata: None,
    })
}

/// A response describing object members after successful call completion.
#[must_use]
pub fn call_state(patch: CallStatePatch) -> PluginResponse {
    PluginResponse::CallStatePatch(patch)
}

/// A [`PluginResponse::CallSignaturePatch`] replacing a call's signature (no diagnostics).
#[must_use]
pub fn call_signature(signature: CallableSignature) -> PluginResponse {
    PluginResponse::CallSignaturePatch(CallSignaturePatch {
        signature,
        diagnostics: Vec::new(),
    })
}

/// Builds a [`ClassPatch`] for the `class-transform` hook.
#[derive(Debug, Clone)]
pub struct ClassPatchBuilder {
    patch: ClassPatch,
}

impl Default for ClassPatchBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ClassPatchBuilder {
    /// Start an empty class patch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            patch: ClassPatch {
                fields: Vec::new(),
                class_members: Vec::new(),
                instance_members: Vec::new(),
                constructor: None,
                diagnostics: Vec::new(),
            },
        }
    }

    /// Add a synthesized field.
    #[must_use]
    pub fn field(mut self, field: FieldPatch) -> Self {
        self.patch.fields.push(field);
        self
    }

    /// Add a generated class-scope member.
    #[must_use]
    pub fn class_member(mut self, member: MemberPatch) -> Self {
        self.patch.class_members.push(member);
        self
    }

    /// Add a generated instance-scope member.
    #[must_use]
    pub fn instance_member(mut self, member: MemberPatch) -> Self {
        self.patch.instance_members.push(member);
        self
    }

    /// Set the synthesized constructor signature.
    #[must_use]
    pub fn constructor(mut self, signature: CallableSignature) -> Self {
        self.patch.constructor = Some(signature);
        self
    }

    /// Attach a class-validation diagnostic.
    #[must_use]
    pub fn diagnostic(mut self, diagnostic: PluginDiagnostic) -> Self {
        self.patch.diagnostics.push(diagnostic);
        self
    }

    /// Finish building and return the [`ClassPatch`].
    #[must_use]
    pub fn build(self) -> ClassPatch {
        self.patch
    }

    /// Finish building and wrap the patch in a [`PluginResponse::ClassPatch`].
    #[must_use]
    pub fn response(self) -> PluginResponse {
        PluginResponse::ClassPatch(self.patch)
    }
}
