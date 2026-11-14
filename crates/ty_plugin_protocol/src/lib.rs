#![doc = include_str!("../README.md")]

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const CURRENT_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 9 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// The protocol version this build of the host implements.
    pub const CURRENT: ProtocolVersion = CURRENT_PROTOCOL_VERSION;

    /// Negotiate whether a host advertising `self` can serve a plugin built for `plugin`.
    ///
    /// The rule mirrors pre-1.0 semver expectations: the `major` numbers must match, and the
    /// host must be at least as new as the plugin's `minor` (a plugin built against protocol
    /// features the host does not yet implement is rejected). Because the protocol structs do
    /// not set `deny_unknown_fields`, an older host can still *parse* a newer plugin's messages;
    /// negotiation is what prevents it from silently dropping features it cannot honor.
    #[must_use]
    pub fn negotiate(self, plugin: ProtocolVersion) -> ProtocolCompatibility {
        if self.major != plugin.major {
            ProtocolCompatibility::MajorMismatch
        } else if plugin.minor > self.minor {
            ProtocolCompatibility::MinorTooNew
        } else {
            ProtocolCompatibility::Compatible
        }
    }

    /// Convenience wrapper around [`ProtocolVersion::negotiate`] for callers that only need a
    /// boolean answer.
    #[must_use]
    pub fn supports(self, plugin: ProtocolVersion) -> bool {
        self.negotiate(plugin) == ProtocolCompatibility::Compatible
    }
}

/// The outcome of negotiating a plugin's declared [`ProtocolVersion`] against the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolCompatibility {
    /// The host can serve the plugin.
    Compatible,
    /// The `major` versions differ; the wire shape is not guaranteed to match.
    MajorMismatch,
    /// The plugin requires a newer `minor` than the host implements.
    MinorTooNew,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VersionReq {
    pub requirement: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub protocol_version: ProtocolVersion,
    pub ty_compatibility: VersionReq,
    pub runtime: RuntimeSpec,
    #[serde(default)]
    pub capabilities: PluginCapabilities,
    #[serde(default)]
    pub claims: PluginClaims,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_config: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stub_overlays: Vec<StubOverlay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeSpec {
    Mock,
    Wasm(WasmRuntimeSpec),
    Subprocess(SubprocessRuntimeSpec),
    Monty(MontyRuntimeSpec),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WasmRuntimeSpec {
    pub artifact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SubprocessRuntimeSpec {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MontyRuntimeSpec {
    /// Path to the plugin's Python source file, resolved against the plugin package root or the
    /// configured `path`.
    pub artifact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Plugin capabilities are serialized manifest flags."
)]
pub struct PluginCapabilities {
    pub stub_overlays: bool,
    pub class_transform: bool,
    /// Opt in to bounded class batches. Each item contains its own semantic
    /// context and complete class index; ordinary class hooks remain available.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub class_batch: bool,
    pub class_member: bool,
    pub instance_member: bool,
    pub call_signature: bool,
    pub call_return: bool,
    pub call_state: bool,
    pub additional_dependencies: bool,
    pub project_index: bool,
    /// The plugin reads its index through SDK lookup helpers. A host may retain
    /// the index outside the interpreter and supply only requested values.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub project_index_lookup: bool,
    pub cross_symbol_contributions: bool,
    pub settings_data: bool,
    pub virtual_types: bool,
    pub mutation_validation: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct PluginClaims {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub modules: Vec<ModuleClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<ClassClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<SymbolClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<SymbolClaim>,
    /// Class constructors selected by exact class or base class.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub constructors: Vec<ClassClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attributes: Vec<AttributeClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingsClaim>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mutations: Vec<ClassClaim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ModuleClaim {
    pub name: String,
    #[serde(default)]
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassClaim {
    #[serde(flatten)]
    pub kind: ClassClaimKind,
}

impl ClassClaim {
    #[must_use]
    pub fn exact(qualified_name: impl Into<String>) -> Self {
        Self {
            kind: ClassClaimKind::Exact {
                qualified_name: qualified_name.into(),
            },
        }
    }

    #[must_use]
    pub fn subclass_of(base_qualified_name: impl Into<String>) -> Self {
        Self {
            kind: ClassClaimKind::SubclassOf {
                base_qualified_name: base_qualified_name.into(),
            },
        }
    }

    #[must_use]
    pub fn exact_qualified_name(&self) -> Option<&str> {
        match &self.kind {
            ClassClaimKind::Exact { qualified_name } => Some(qualified_name),
            ClassClaimKind::SubclassOf { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum ClassClaimKind {
    Exact { qualified_name: String },
    SubclassOf { base_qualified_name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SymbolClaim {
    pub qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MethodClaim {
    #[serde(flatten)]
    pub kind: MethodClaimKind,
}

impl MethodClaim {
    #[must_use]
    pub fn exact(class_qualified_name: impl Into<String>, method_name: impl Into<String>) -> Self {
        Self {
            kind: MethodClaimKind::Exact {
                class_qualified_name: class_qualified_name.into(),
                method_name: method_name.into(),
            },
        }
    }

    #[must_use]
    pub fn on_subclass_of(
        base_qualified_name: impl Into<String>,
        method_name: impl Into<String>,
    ) -> Self {
        Self {
            kind: MethodClaimKind::OnSubclassOf {
                base_qualified_name: base_qualified_name.into(),
                method_name: method_name.into(),
            },
        }
    }

    /// Claim every method on subclasses of `base_qualified_name` whose name matches
    /// `method_name_pattern`. See [`method_name_pattern_matches`] for the pattern language;
    /// `"*"` claims all methods.
    #[must_use]
    pub fn on_subclass_of_matching(
        base_qualified_name: impl Into<String>,
        method_name_pattern: impl Into<String>,
    ) -> Self {
        Self {
            kind: MethodClaimKind::OnSubclassOfMatching {
                base_qualified_name: base_qualified_name.into(),
                method_name_pattern: method_name_pattern.into(),
            },
        }
    }

    #[must_use]
    pub fn exact_method(&self) -> Option<(&str, &str)> {
        match &self.kind {
            MethodClaimKind::Exact {
                class_qualified_name,
                method_name,
            } => Some((class_qualified_name, method_name)),
            MethodClaimKind::OnSubclassOf { .. } | MethodClaimKind::OnSubclassOfMatching { .. } => {
                None
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum MethodClaimKind {
    Exact {
        class_qualified_name: String,
        method_name: String,
    },
    OnSubclassOf {
        base_qualified_name: String,
        method_name: String,
    },
    /// Claim methods on subclasses of `base_qualified_name` whose names match
    /// `method_name_pattern`, as defined by [`method_name_pattern_matches`].
    OnSubclassOfMatching {
        base_qualified_name: String,
        method_name_pattern: String,
    },
}

/// Match a method name against a claim pattern.
///
/// `*` matches any (possibly empty) run of characters; every other character is literal. There
/// are no character classes or escapes, so `*` is the only metacharacter and `"*"` alone matches
/// every name. An empty pattern matches only an empty name, which no Python method can have.
#[must_use]
pub fn method_name_pattern_matches(pattern: &str, method_name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), method_name.as_bytes());
    let (mut p, mut n) = (0, 0);
    let (mut star_p, mut star_n) = (usize::MAX, 0);
    while n < name.len() {
        if p < pattern.len() && pattern[p] == name[n] {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star_p = p;
            star_n = n;
            p += 1;
        } else if star_p != usize::MAX {
            p = star_p + 1;
            star_n += 1;
            n = star_n;
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AttributeClaim {
    #[serde(flatten)]
    pub kind: AttributeClaimKind,
}

impl AttributeClaim {
    #[must_use]
    pub fn exact(
        owner_qualified_name: impl Into<String>,
        attribute_name: impl Into<String>,
        scope: AttributeScope,
    ) -> Self {
        Self {
            kind: AttributeClaimKind::Exact {
                owner_qualified_name: owner_qualified_name.into(),
                attribute_name: attribute_name.into(),
                scope,
            },
        }
    }

    #[must_use]
    pub fn contribution_target(
        owner_base_qualified_name: impl Into<String>,
        scope: AttributeScope,
    ) -> Self {
        Self {
            kind: AttributeClaimKind::ContributionTarget {
                owner_base_qualified_name: owner_base_qualified_name.into(),
                scope,
            },
        }
    }

    #[must_use]
    pub fn on_subclass_of(
        owner_base_qualified_name: impl Into<String>,
        scope: AttributeScope,
    ) -> Self {
        Self {
            kind: AttributeClaimKind::OnSubclassOf {
                owner_base_qualified_name: owner_base_qualified_name.into(),
                scope,
            },
        }
    }

    #[must_use]
    pub fn exact_attribute(&self) -> Option<(&str, &str, AttributeScope)> {
        match &self.kind {
            AttributeClaimKind::Exact {
                owner_qualified_name,
                attribute_name,
                scope,
            } => Some((owner_qualified_name, attribute_name, *scope)),
            AttributeClaimKind::ContributionTarget { .. }
            | AttributeClaimKind::OnSubclassOf { .. } => None,
        }
    }

    #[must_use]
    pub fn scope(&self) -> AttributeScope {
        match &self.kind {
            AttributeClaimKind::Exact { scope, .. }
            | AttributeClaimKind::ContributionTarget { scope, .. }
            | AttributeClaimKind::OnSubclassOf { scope, .. } => *scope,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum AttributeClaimKind {
    Exact {
        owner_qualified_name: String,
        attribute_name: String,
        scope: AttributeScope,
    },
    ContributionTarget {
        owner_base_qualified_name: String,
        scope: AttributeScope,
    },
    OnSubclassOf {
        owner_base_qualified_name: String,
        scope: AttributeScope,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AttributeScope {
    Class,
    Instance,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SettingsClaim {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub module: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct StubOverlay {
    pub module: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PluginRequest {
    Manifest,
    BuildProjectIndex(BuildProjectIndexRequest),
    AnalyzeClass(AnalyzeClassRequest),
    AnalyzeClasses(AnalyzeClassesRequest),
    ResolveClassMember(ResolveMemberRequest),
    ResolveInstanceMember(ResolveMemberRequest),
    AdjustCallSignature(CallRequest),
    AdjustCallReturn(CallRequest),
    AdjustCallState(CallRequest),
    AdditionalDependencies(DependencyRequest),
    /// Boxed because `MutationRequest` is far larger than the rest, so inlining it
    /// would grow every request. `Box` is transparent to serde, so the wire format is
    /// unaffected.
    ValidateMutation(Box<MutationRequest>),
}

impl PluginRequest {
    /// Index slot shared by semantic requests; project discovery has no index.
    pub fn project_index_mut(&mut self) -> Option<&mut Option<serde_json::Value>> {
        match self {
            Self::AnalyzeClass(request) => Some(&mut request.project_index),
            Self::ResolveClassMember(request) | Self::ResolveInstanceMember(request) => {
                Some(&mut request.project_index)
            }
            Self::AdjustCallSignature(request)
            | Self::AdjustCallReturn(request)
            | Self::AdjustCallState(request) => Some(&mut request.project_index),
            Self::ValidateMutation(request) => Some(&mut request.project_index),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectContext {
    pub root: String,
    pub python_version: String,
    pub platform: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BuildProjectIndexRequest {
    pub context: ProjectContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub classes: Vec<ClassSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub settings: Vec<SettingsModuleSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignments: Vec<AssignmentSummary>,
    /// Module-level function definitions, so plugins can see decorator-based registration
    /// sites that are neither classes nor assignments.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<FunctionSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_index_fingerprint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AnalyzeClassRequest {
    pub context: SemanticContext,
    pub class: ClassSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

/// A bounded collection of independent class requests. The plugin explicitly
/// opts into sharing an interpreter invocation across these items. It must not
/// depend on batch boundaries or mutate state used by another item's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AnalyzeClassesRequest {
    pub requests: Vec<ClassTransformRequest>,
}

/// Keep each item wire-identical to an individual class request while
/// rejecting unrelated hooks and recursively nested batches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClassTransformRequest {
    AnalyzeClass(AnalyzeClassRequest),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ResolveMemberRequest {
    pub context: SemanticContext,
    pub owner: TypeExpr,
    pub member_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_member: Option<MemberSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallRequest {
    pub context: SemanticContext,
    pub callee: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ReceiverSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<ArgumentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_signature: Option<CallableSignature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_return_type: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DependencyRequest {
    pub config: serde_json::Value,
}

/// A syntax-level write or delete operation offered to a semantic plugin for validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MutationRequest {
    pub context: SemanticContext,
    pub operation: MutationOperation,
    /// The inferred receiver, including its lossless structural snapshot when available.
    pub receiver: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<ArgumentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<ArgumentSummary>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_index: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MutationOperation {
    AttributeSet,
    ItemSet,
    AttributeDelete,
    ItemDelete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SemanticContext {
    pub module: String,
    pub file_path: String,
    pub python_version: String,
    pub platform: String,
    /// The plugin's resolved configuration (`default-config` merged with project overrides),
    /// matching what `ProjectContext::config` carries for project-level hooks.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub config: serde_json::Value,
    #[serde(default)]
    pub speculative: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassSummary {
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<TypeExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<CallOrSymbolSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metaclass: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<MethodSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nested_classes: Vec<NestedClassSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_constants: Vec<ConstantSummary>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SymbolRef {
    pub qualified_name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum CallOrSymbolSummary {
    Symbol(SymbolRef),
    Call(CallValueSummary),
    Other {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inferred_type: Option<TypeExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FieldSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotation: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_value: Option<AssignedValueSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_type: Option<TypeExpr>,
    #[serde(default)]
    pub has_default: bool,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MethodSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<CallOrSymbolSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_type: Option<TypeExpr>,
    #[serde(default)]
    pub is_public: bool,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

/// A module-level function definition for [`BuildProjectIndexRequest::functions`].
///
/// `inferred_type` carries the bound type *after* decorators are applied, so a definition
/// whose decorator returns a `Registry[Model, Schema]` instance shows up with that instance
/// type rather than the raw function type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FunctionSummary {
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<CallOrSymbolSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_type: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AssignmentSummary {
    pub name: String,
    pub qualified_name: String,
    pub assigned_value: AssignedValueSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inferred_type: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct NestedClassSummary {
    pub name: String,
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bases: Vec<TypeExpr>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_constants: Vec<ConstantSummary>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConstantSummary {
    pub name: String,
    pub value: LiteralValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum AssignedValueSummary {
    Call(CallValueSummary),
    Literal {
        value: LiteralValue,
    },
    Name(SymbolRef),
    Attribute(SymbolRef),
    Other {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inferred_type: Option<TypeExpr>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallValueSummary {
    pub callee: SymbolRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receiver: Option<ValueSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<ArgumentSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_type: Option<TypeExpr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ValueSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<SymbolRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum LiteralValue {
    Bool {
        value: bool,
    },
    Int {
        value: i64,
    },
    Str {
        value: String,
    },
    None,
    EnumRef(SymbolRef),
    SymbolRef(SymbolRef),
    ClassRef(SymbolRef),
    Tuple {
        items: Vec<LiteralValue>,
    },
    List {
        items: Vec<LiteralValue>,
    },
    Dict {
        entries: Vec<LiteralDictEntry>,
    },
    #[default]
    Unknown,
}

impl LiteralValue {
    #[must_use]
    pub const fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LiteralDictEntry {
    pub key: LiteralValue,
    pub value: LiteralValue,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MemberSummary {
    pub name: String,
    pub access: MemberAccessPatch,
    #[serde(default)]
    pub is_read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ArgumentSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ArgumentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "LiteralValue::is_unknown")]
    pub value: LiteralValue,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<SymbolSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ArgumentKind {
    Positional,
    Keyword,
    StarArgs,
    StarKwargs,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ReceiverSummary {
    pub type_expr: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nominal_class: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub generic_arguments: Vec<TypeExpr>,
    #[serde(default)]
    pub plugin_metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SettingsModuleSummary {
    pub module: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<SettingValueSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SettingValueSummary {
    pub name: String,
    pub value: LiteralValue,
    #[serde(default, skip_serializing_if = "SymbolSource::is_unknown")]
    pub source: SymbolSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum PluginResponse {
    /// Boxed for the same reason as [`PluginRequest::ValidateMutation`]: this variant is
    /// far larger than the rest, so inlining it would grow every response. `Box` is
    /// transparent to serde, so the wire format is unaffected.
    Manifest(Box<PluginManifest>),
    ProjectIndex(ProjectIndexResponse),
    ClassPatch(ClassPatch),
    ClassBatch(ClassBatchResponse),
    MemberPatch(MemberPatch),
    CallSignaturePatch(CallSignaturePatch),
    CallReturnPatch(CallReturnPatch),
    CallStatePatch(CallStatePatch),
    Dependencies(DependenciesResponse),
    MutationDiagnostics(MutationResponse),
    NoChange,
    Error(PluginError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DependenciesResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<PluginDependency>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MutationResponse {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectIndexResponse {
    #[serde(default)]
    pub plugin_index: serde_json::Value,
    /// Optional complete replacement for `project-index` in the named class's
    /// `analyze-class` request. Other hooks and unlisted classes receive the
    /// ordinary index. Values are opaque, plugin-owned JSON, including null.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub class_indexes: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributions: Vec<Contribution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub virtual_types: Vec<VirtualTypeDefinition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<PluginDependency>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassPatch {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldPatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_members: Vec<MemberPatch>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instance_members: Vec<MemberPatch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constructor: Option<CallableSignature>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

/// Results correspond to request items in order. A batch cannot introduce
/// project indexes, virtual definitions, or another nested batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ClassBatchResponse {
    pub responses: Vec<ClassTransformResponse>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ClassTransformResponse {
    ClassPatch(ClassPatch),
    NoChange,
    Error(PluginError),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FieldPatch {
    pub name: String,
    #[serde(default)]
    pub mode: MemberPatchMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor: Option<MemberAccessPatch>,
    pub instance_get_type: TypeExpr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instance_set_type: Option<TypeExpr>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub constructor_parameter: Option<Parameter>,
    #[serde(default)]
    pub has_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MemberPatch {
    pub name: String,
    #[serde(default)]
    pub mode: MemberPatchMode,
    pub access: MemberAccessPatch,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum MemberAccessPatch {
    Value {
        type_expr: TypeExpr,
    },
    Descriptor {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        class_type: Option<TypeExpr>,
        instance_get_type: TypeExpr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_set_type: Option<TypeExpr>,
    },
    Callable {
        signature: CallableSignature,
        /// Protocol-v1 fallback for hosts that do not understand callable member patches.
        fallback_type: TypeExpr,
    },
}

impl MemberAccessPatch {
    #[must_use]
    pub fn value(type_expr: TypeExpr) -> Self {
        Self::Value { type_expr }
    }

    #[must_use]
    pub fn instance_get_type(&self) -> &TypeExpr {
        match self {
            Self::Value { type_expr }
            | Self::Descriptor {
                instance_get_type: type_expr,
                ..
            }
            | Self::Callable {
                fallback_type: type_expr,
                ..
            } => type_expr,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemberPatchMode {
    #[default]
    FillOnMiss,
    ReplaceExisting,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Contribution {
    pub source: SymbolSource,
    pub target: ContributionTarget,
    pub patch: ContributionPatch,
    pub conflict_key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum ContributionTarget {
    Class { qualified_name: String },
    Instance { qualified_name: String },
    Constructor { qualified_name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum ContributionPatch {
    Member(MemberPatch),
    Field(FieldPatch),
    Constructor(CallableSignature),
    Diagnostic(PluginDiagnostic),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallSignaturePatch {
    pub signature: CallableSignature,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallReturnPatch {
    pub return_type: TypeExpr,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<PluginDiagnostic>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_metadata: Option<serde_json::Value>,
}

/// Member facts that hold after a synchronous call completes successfully.
///
/// Missing members retain their ordinary declared types. Unknown calls invalidate all facts.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct CallStatePatch {
    pub receiver_members: BTreeMap<String, TypeExpr>,
    pub result_members: BTreeMap<String, TypeExpr>,
    /// The result is a new object, distinct from all previously existing objects.
    /// Result member facts are ignored unless this guarantee is provided.
    pub fresh_result: bool,
    /// The call cannot mutate any pre-existing object other than its receiver.
    /// It also cannot rebind names in the caller's module or enclosing scopes.
    /// Without this guarantee all existing member facts are discarded first.
    pub preserves_other_objects: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CallableSignature {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<Parameter>,
    pub return_type: TypeExpr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Parameter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kind: ParameterKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_expr: Option<TypeExpr>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterKind {
    PositionalOnly,
    PositionalOrKeyword,
    VarArgs,
    KeywordOnly,
    Kwargs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TypeExpr {
    pub expression: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportBinding>,
    pub mode: TypeExprMode,
    /// A lossless structural representation of this type when the host can provide one.
    ///
    /// The display expression remains the protocol-v1 fallback. Keeping the snapshot optional
    /// lets older plugins ignore it and lets newer plugins return an unchanged host type without
    /// reparsing a lossy display string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Box<TypeSnapshot>>,
}

impl TypeExpr {
    /// A bare type expression (e.g. a runtime value's inferred type), imports left empty.
    pub fn expression(expression: impl Into<String>) -> Self {
        Self::with_mode(expression, TypeExprMode::Expression)
    }

    /// An annotation-position type expression (e.g. a parameter or field annotation).
    pub fn annotation(expression: impl Into<String>) -> Self {
        Self::with_mode(expression, TypeExprMode::Annotation)
    }

    /// A stub-position type expression (e.g. content destined for a generated `.pyi` overlay).
    pub fn stub(expression: impl Into<String>) -> Self {
        Self::with_mode(expression, TypeExprMode::Stub)
    }

    fn with_mode(expression: impl Into<String>, mode: TypeExprMode) -> Self {
        Self {
            expression: expression.into(),
            imports: Vec::new(),
            mode,
            snapshot: None,
        }
    }

    #[must_use]
    pub fn with_snapshot(mut self, snapshot: TypeSnapshot) -> Self {
        self.snapshot = Some(Box::new(snapshot));
        self
    }
}

/// Structural type data that survives a complete host -> plugin -> host round trip.
///
/// This intentionally models Python type shapes rather than checker internals. Unsupported
/// internal types use [`TypeSnapshot::Expression`], which preserves the existing `TypeExpr`
/// behavior while still retaining imports at the exact nested position where they are needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum TypeSnapshot {
    Expression(TypeSnapshotExpression),
    Nominal {
        qualified_name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        arguments: Vec<TypeSnapshot>,
    },
    Tuple {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        prefix: Vec<TypeSnapshot>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        variadic: Option<Box<TypeSnapshot>>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        suffix: Vec<TypeSnapshot>,
    },
    TypedDict {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<TypeSnapshotField>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra_items: Option<Box<TypeSnapshotField>>,
        #[serde(default)]
        closed: bool,
    },
    Union {
        elements: Vec<TypeSnapshot>,
    },
    PluginClass {
        identity: String,
    },
    SelfType {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bound: Option<Box<TypeSnapshot>>,
    },
    Annotated {
        base: Box<TypeSnapshot>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        metadata: Vec<TypeSnapshotMetadata>,
    },
}

impl TypeSnapshot {
    #[must_use]
    pub fn expression(type_expr: &TypeExpr) -> Self {
        Self::Expression(TypeSnapshotExpression {
            expression: type_expr.expression.clone(),
            imports: type_expr.imports.clone(),
            mode: type_expr.mode,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TypeSnapshotExpression {
    pub expression: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<ImportBinding>,
    pub mode: TypeExprMode,
}

impl TypeSnapshotExpression {
    #[must_use]
    pub fn to_type_expr(&self) -> TypeExpr {
        TypeExpr {
            expression: self.expression.clone(),
            imports: self.imports.clone(),
            mode: self.mode,
            snapshot: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TypeSnapshotField {
    pub name: String,
    pub type_snapshot: TypeSnapshot,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TypeSnapshotMetadata {
    pub qualified_name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub arguments: Vec<TypeSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ImportBinding {
    pub module: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TypeExprMode {
    Expression,
    Annotation,
    Stub,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VirtualTypeDefinition {
    pub name: String,
    pub shape: VirtualTypeShape,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "kebab-case"
)]
pub enum VirtualTypeShape {
    Class {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        bases: Vec<TypeExpr>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        members: Vec<MemberPatch>,
    },
    TypedDict {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<VirtualTypeField>,
        #[serde(default = "default_true")]
        total: bool,
    },
    NamedTuple {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fields: Vec<VirtualTypeField>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct VirtualTypeField {
    pub name: String,
    pub type_expr: TypeExpr,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default)]
    pub read_only: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginDependency {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginDiagnostic {
    pub id: String,
    pub message: String,
    pub severity: DiagnosticSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<DiagnosticLocation>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DiagnosticLocation {
    pub file_path: String,
    pub start: TextPosition,
    pub end: TextPosition,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SymbolSource {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualified_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<TextPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<TextPosition>,
}

impl SymbolSource {
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TextPosition {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PluginError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<PluginDiagnostic>,
}
