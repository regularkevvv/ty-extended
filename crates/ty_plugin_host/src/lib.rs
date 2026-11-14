//! Host-side support for loading and routing `ty` semantic plugins.
//!
//! This crate owns plugin manifests, routing, runtime error normalization, and
//! runner abstraction. Checker integration belongs in ty-extended.

use std::collections::BTreeMap;

use thiserror::Error;
use ty_plugin_protocol::{
    AttributeClaimKind, AttributeScope, CURRENT_PROTOCOL_VERSION, ClassClaimKind, MethodClaimKind,
    PluginManifest, PluginRequest, PluginResponse, ProtocolCompatibility, RuntimeSpec,
    method_name_pattern_matches,
};

#[cfg(all(feature = "plugins-monty", not(target_arch = "wasm32")))]
mod monty;
#[cfg(all(feature = "plugins-monty", not(target_arch = "wasm32")))]
pub use monty::{MontyLimits, MontyRunner};

#[derive(Debug, Error)]
pub enum HostError {
    #[error("duplicate plugin id `{0}`")]
    DuplicatePluginId(String),
    #[error(
        "plugin `{plugin_id}` uses unsupported protocol version {major}.{minor}; host supports {supported_major}.{supported_minor}"
    )]
    UnsupportedProtocolVersion {
        plugin_id: String,
        major: u16,
        minor: u16,
        supported_major: u16,
        supported_minor: u16,
    },
    #[error("plugin `{plugin_id}` declares stub overlays without the stub-overlays capability")]
    StubOverlayCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares class-transform claims without the class-transform capability"
    )]
    ClassTransformCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares class-member claims without the class-member capability"
    )]
    ClassMemberCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares instance-member claims without the instance-member capability"
    )]
    InstanceMemberCapabilityMissing { plugin_id: String },
    #[error("plugin `{plugin_id}` declares call claims without a call hook capability")]
    CallCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares settings summaries without the settings-data capability"
    )]
    SettingsDataCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares contribution-target claims without the cross-symbol-contributions capability"
    )]
    CrossSymbolContributionsCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares cross-symbol contributions without the project-index capability"
    )]
    ProjectIndexCapabilityMissing { plugin_id: String },
    #[error(
        "plugin `{plugin_id}` declares mutation claims without the mutation-validation capability"
    )]
    MutationValidationCapabilityMissing { plugin_id: String },
    #[error("unknown plugin id `{0}`")]
    UnknownPlugin(String),
    #[error("plugin `{plugin_id}` runtime failed: {source}")]
    Runtime {
        plugin_id: String,
        #[source]
        source: RuntimeError,
    },
}

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("unsupported runtime `{0}`")]
    UnsupportedRuntime(&'static str),
    #[error("plugin trapped: {0}")]
    Trap(String),
    #[error("plugin timed out")]
    Timeout,
    #[error("plugin response exceeded the configured size limit")]
    ResponseTooLarge,
    #[error("plugin returned an invalid response: {0}")]
    InvalidResponse(String),
}

impl RuntimeError {
    /// A short, actionable remediation hint suitable for a plugin diagnostic's sub-message.
    ///
    /// A runtime failure disables that hook for the call (the checker falls back to no change), so
    /// the hint tells the user how to recover rather than describing an internal error.
    #[must_use]
    pub fn hint(&self) -> &'static str {
        match self {
            RuntimeError::UnsupportedRuntime(_) => {
                "This build of `ty` cannot execute this plugin runtime; rebuild with the runtime enabled or remove the plugin."
            }
            RuntimeError::Trap(_) => {
                "The plugin crashed while handling a request. Report this to the plugin author; update or disable the plugin to continue."
            }
            RuntimeError::Timeout => {
                "The plugin exceeded its execution budget. Update or disable the plugin, or raise its limit if you trust it."
            }
            RuntimeError::ResponseTooLarge => {
                "The plugin returned more data than the host accepts. Update or disable the plugin."
            }
            RuntimeError::InvalidResponse(_) => {
                "The plugin returned a response `ty` could not understand. Ensure the plugin targets a compatible protocol version."
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginEnvironment {
    plugins: BTreeMap<String, LoadedPlugin>,
    routes: RouteTable,
}

impl PluginEnvironment {
    pub fn from_manifests(manifests: Vec<PluginManifest>) -> Result<Self, HostError> {
        let mut plugins = BTreeMap::new();

        for manifest in manifests {
            validate_manifest(&manifest)?;

            let plugin_id = manifest.id.clone();
            if plugins
                .insert(plugin_id.clone(), LoadedPlugin { manifest })
                .is_some()
            {
                return Err(HostError::DuplicatePluginId(plugin_id));
            }
        }

        let routes = RouteTable::from_plugins(plugins.values());

        Ok(Self { plugins, routes })
    }

    pub fn plugin(&self, plugin_id: &str) -> Option<&LoadedPlugin> {
        self.plugins.get(plugin_id)
    }

    pub fn plugins(&self) -> impl Iterator<Item = &LoadedPlugin> {
        self.plugins.values()
    }

    pub fn routes(&self) -> &RouteTable {
        &self.routes
    }
}

#[derive(Debug, Clone)]
pub struct LoadedPlugin {
    manifest: PluginManifest,
}

impl LoadedPlugin {
    pub fn manifest(&self) -> &PluginManifest {
        &self.manifest
    }

    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    pub fn runtime(&self) -> &RuntimeSpec {
        &self.manifest.runtime
    }
}

#[derive(Debug, Default, Clone)]
pub struct RouteTable {
    class_transforms: BTreeMap<String, Vec<String>>,
    subclass_transforms: BTreeMap<String, Vec<String>>,
    class_members: BTreeMap<MemberRouteKey, Vec<String>>,
    instance_members: BTreeMap<MemberRouteKey, Vec<String>>,
    class_contribution_targets: BTreeMap<String, Vec<String>>,
    instance_contribution_targets: BTreeMap<String, Vec<String>>,
    call_signatures: BTreeMap<String, Vec<String>>,
    call_returns: BTreeMap<String, Vec<String>>,
    call_states: BTreeMap<String, Vec<String>>,
    call_state_constructor_subclasses: BTreeMap<String, Vec<String>>,
    call_signature_methods_on_subclass: BTreeMap<MethodRouteKey, Vec<String>>,
    call_return_methods_on_subclass: BTreeMap<MethodRouteKey, Vec<String>>,
    call_state_methods_on_subclass: BTreeMap<MethodRouteKey, Vec<String>>,
    /// Pattern method claims can't be keyed by an exact method name, so they're stored per
    /// owner base as `(method_name_pattern, plugin_id)` pairs and filtered at lookup.
    call_signature_method_patterns_on_subclass: BTreeMap<String, Vec<MethodPatternRoute>>,
    call_return_method_patterns_on_subclass: BTreeMap<String, Vec<MethodPatternRoute>>,
    call_state_method_patterns_on_subclass: BTreeMap<String, Vec<MethodPatternRoute>>,
    dependency_plugins: Vec<String>,
    project_index_plugins: Vec<String>,
    settings_plugins: BTreeMap<String, Vec<String>>,
    stub_overlay_plugins: BTreeMap<String, Vec<String>>,
    mutation_classes: BTreeMap<String, Vec<String>>,
    mutation_subclasses: BTreeMap<String, Vec<String>>,
}

impl RouteTable {
    fn from_plugins<'a>(plugins: impl IntoIterator<Item = &'a LoadedPlugin>) -> Self {
        let mut routes = Self::default();

        for plugin in plugins {
            let manifest = plugin.manifest();
            let plugin_id = plugin.id().to_string();
            let capabilities = &manifest.capabilities;
            let claims = &manifest.claims;

            if capabilities.class_transform {
                for claim in &claims.classes {
                    match &claim.kind {
                        ClassClaimKind::Exact { qualified_name } => {
                            routes
                                .class_transforms
                                .entry(qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        ClassClaimKind::SubclassOf {
                            base_qualified_name,
                        } => {
                            routes
                                .subclass_transforms
                                .entry(base_qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                    }
                }
            }

            if capabilities.class_member {
                for claim in claims
                    .attributes
                    .iter()
                    .filter(|claim| claim.scope() == AttributeScope::Class)
                {
                    if let Some((owner_qualified_name, attribute_name, _)) = claim.exact_attribute()
                    {
                        routes
                            .class_members
                            .entry(MemberRouteKey::new(owner_qualified_name, attribute_name))
                            .or_default()
                            .push(plugin_id.clone());
                    }
                }
            }

            if capabilities.instance_member {
                for claim in claims
                    .attributes
                    .iter()
                    .filter(|claim| claim.scope() == AttributeScope::Instance)
                {
                    if let Some((owner_qualified_name, attribute_name, _)) = claim.exact_attribute()
                    {
                        routes
                            .instance_members
                            .entry(MemberRouteKey::new(owner_qualified_name, attribute_name))
                            .or_default()
                            .push(plugin_id.clone());
                    }
                }
            }

            if capabilities.cross_symbol_contributions {
                for claim in &claims.attributes {
                    match &claim.kind {
                        AttributeClaimKind::ContributionTarget {
                            owner_base_qualified_name,
                            scope: AttributeScope::Class,
                        } => {
                            routes
                                .class_contribution_targets
                                .entry(owner_base_qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        AttributeClaimKind::ContributionTarget {
                            owner_base_qualified_name,
                            scope: AttributeScope::Instance,
                        } => {
                            routes
                                .instance_contribution_targets
                                .entry(owner_base_qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        AttributeClaimKind::Exact { .. }
                        | AttributeClaimKind::OnSubclassOf { .. } => {}
                    }
                }
            }

            if capabilities.call_signature {
                for symbol in &claims.functions {
                    routes
                        .call_signatures
                        .entry(symbol.qualified_name.clone())
                        .or_default()
                        .push(plugin_id.clone());
                }
                for method in &claims.methods {
                    match &method.kind {
                        MethodClaimKind::Exact {
                            class_qualified_name,
                            method_name,
                        } => {
                            routes
                                .call_signatures
                                .entry(method_qualified_name(class_qualified_name, method_name))
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        MethodClaimKind::OnSubclassOf {
                            base_qualified_name,
                            method_name,
                        } => {
                            routes
                                .call_signature_methods_on_subclass
                                .entry(MethodRouteKey::new(base_qualified_name, method_name))
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        MethodClaimKind::OnSubclassOfMatching {
                            base_qualified_name,
                            method_name_pattern,
                        } => {
                            routes
                                .call_signature_method_patterns_on_subclass
                                .entry(base_qualified_name.clone())
                                .or_default()
                                .push(MethodPatternRoute::new(
                                    method_name_pattern,
                                    plugin_id.clone(),
                                ));
                        }
                    }
                }
            }

            if capabilities.call_return {
                for symbol in &claims.functions {
                    routes
                        .call_returns
                        .entry(symbol.qualified_name.clone())
                        .or_default()
                        .push(plugin_id.clone());
                }
                for method in &claims.methods {
                    match &method.kind {
                        MethodClaimKind::Exact {
                            class_qualified_name,
                            method_name,
                        } => {
                            routes
                                .call_returns
                                .entry(method_qualified_name(class_qualified_name, method_name))
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        MethodClaimKind::OnSubclassOf {
                            base_qualified_name,
                            method_name,
                        } => {
                            routes
                                .call_return_methods_on_subclass
                                .entry(MethodRouteKey::new(base_qualified_name, method_name))
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        MethodClaimKind::OnSubclassOfMatching {
                            base_qualified_name,
                            method_name_pattern,
                        } => {
                            routes
                                .call_return_method_patterns_on_subclass
                                .entry(base_qualified_name.clone())
                                .or_default()
                                .push(MethodPatternRoute::new(
                                    method_name_pattern,
                                    plugin_id.clone(),
                                ));
                        }
                    }
                }
            }

            if capabilities.call_state {
                for claim in &claims.constructors {
                    match &claim.kind {
                        ClassClaimKind::Exact { qualified_name } => {
                            routes
                                .call_states
                                .entry(qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        ClassClaimKind::SubclassOf {
                            base_qualified_name,
                        } => {
                            routes
                                .call_state_constructor_subclasses
                                .entry(base_qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                    }
                }
                for symbol in &claims.functions {
                    routes
                        .call_states
                        .entry(symbol.qualified_name.clone())
                        .or_default()
                        .push(plugin_id.clone());
                }
                for method in &claims.methods {
                    match &method.kind {
                        MethodClaimKind::Exact {
                            class_qualified_name,
                            method_name,
                        } => {
                            routes
                                .call_states
                                .entry(method_qualified_name(class_qualified_name, method_name))
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        MethodClaimKind::OnSubclassOf {
                            base_qualified_name,
                            method_name,
                        } => {
                            routes
                                .call_state_methods_on_subclass
                                .entry(MethodRouteKey::new(base_qualified_name, method_name))
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        MethodClaimKind::OnSubclassOfMatching {
                            base_qualified_name,
                            method_name_pattern,
                        } => {
                            routes
                                .call_state_method_patterns_on_subclass
                                .entry(base_qualified_name.clone())
                                .or_default()
                                .push(MethodPatternRoute::new(
                                    method_name_pattern,
                                    plugin_id.clone(),
                                ));
                        }
                    }
                }
            }

            if capabilities.additional_dependencies {
                routes.dependency_plugins.push(plugin_id.clone());
            }

            if capabilities.project_index {
                routes.project_index_plugins.push(plugin_id.clone());
            }

            if capabilities.settings_data {
                for settings in &claims.settings {
                    routes
                        .settings_plugins
                        .entry(settings.module.clone())
                        .or_default()
                        .push(plugin_id.clone());
                }
            }

            if capabilities.stub_overlays {
                for overlay in &manifest.stub_overlays {
                    routes
                        .stub_overlay_plugins
                        .entry(overlay.module.clone())
                        .or_default()
                        .push(plugin_id.clone());
                }
            }

            if capabilities.mutation_validation {
                for claim in &claims.mutations {
                    match &claim.kind {
                        ClassClaimKind::Exact { qualified_name } => {
                            routes
                                .mutation_classes
                                .entry(qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                        ClassClaimKind::SubclassOf {
                            base_qualified_name,
                        } => {
                            routes
                                .mutation_subclasses
                                .entry(base_qualified_name.clone())
                                .or_default()
                                .push(plugin_id.clone());
                        }
                    }
                }
            }
        }

        routes
    }

    pub fn class_transform_plugins(&self, qualified_name: &str) -> &[String] {
        self.class_transforms
            .get(qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn subclass_transform_plugins(&self, base_qualified_name: &str) -> &[String] {
        self.subclass_transforms
            .get(base_qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn class_member_plugins(&self, owner_qualified_name: &str, member_name: &str) -> &[String] {
        self.class_members
            .get(&MemberRouteKey::new(owner_qualified_name, member_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn instance_member_plugins(
        &self,
        owner_qualified_name: &str,
        member_name: &str,
    ) -> &[String] {
        self.instance_members
            .get(&MemberRouteKey::new(owner_qualified_name, member_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn class_contribution_target_plugins(&self, owner_base_qualified_name: &str) -> &[String] {
        self.class_contribution_targets
            .get(owner_base_qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn instance_contribution_target_plugins(
        &self,
        owner_base_qualified_name: &str,
    ) -> &[String] {
        self.instance_contribution_targets
            .get(owner_base_qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_signature_plugins(&self, qualified_name: &str) -> &[String] {
        self.call_signatures
            .get(qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_return_plugins(&self, qualified_name: &str) -> &[String] {
        self.call_returns
            .get(qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_state_plugins(&self, qualified_name: &str) -> &[String] {
        self.call_states
            .get(qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_state_constructor_on_subclass_plugins(
        &self,
        base_qualified_name: &str,
    ) -> &[String] {
        self.call_state_constructor_subclasses
            .get(base_qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_signature_method_on_subclass_plugins(
        &self,
        base_qualified_name: &str,
        method_name: &str,
    ) -> &[String] {
        self.call_signature_methods_on_subclass
            .get(&MethodRouteKey::new(base_qualified_name, method_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_return_method_on_subclass_plugins(
        &self,
        base_qualified_name: &str,
        method_name: &str,
    ) -> &[String] {
        self.call_return_methods_on_subclass
            .get(&MethodRouteKey::new(base_qualified_name, method_name))
            .map_or(&[], Vec::as_slice)
    }

    pub fn call_state_method_on_subclass_plugins(
        &self,
        base_qualified_name: &str,
        method_name: &str,
    ) -> &[String] {
        self.call_state_methods_on_subclass
            .get(&MethodRouteKey::new(base_qualified_name, method_name))
            .map_or(&[], Vec::as_slice)
    }

    /// Plugins whose `on-subclass-of-matching` claims select `method_name` on subclasses of
    /// `base_qualified_name`. Pattern routes filter at lookup, so this allocates.
    pub fn call_signature_method_on_subclass_pattern_plugins(
        &self,
        base_qualified_name: &str,
        method_name: &str,
    ) -> Vec<&str> {
        matching_method_pattern_plugins(
            &self.call_signature_method_patterns_on_subclass,
            base_qualified_name,
            method_name,
        )
    }

    pub fn call_return_method_on_subclass_pattern_plugins(
        &self,
        base_qualified_name: &str,
        method_name: &str,
    ) -> Vec<&str> {
        matching_method_pattern_plugins(
            &self.call_return_method_patterns_on_subclass,
            base_qualified_name,
            method_name,
        )
    }

    pub fn dependency_plugins(&self) -> &[String] {
        &self.dependency_plugins
    }

    pub fn project_index_plugins(&self) -> &[String] {
        &self.project_index_plugins
    }

    pub fn settings_plugins(&self, module: &str) -> &[String] {
        self.settings_plugins.get(module).map_or(&[], Vec::as_slice)
    }

    pub fn stub_overlay_plugins(&self, module: &str) -> &[String] {
        self.stub_overlay_plugins
            .get(module)
            .map_or(&[], Vec::as_slice)
    }

    pub fn mutation_class_plugins(&self, qualified_name: &str) -> &[String] {
        self.mutation_classes
            .get(qualified_name)
            .map_or(&[], Vec::as_slice)
    }

    pub fn mutation_subclass_plugins(&self, base_qualified_name: &str) -> &[String] {
        self.mutation_subclasses
            .get(base_qualified_name)
            .map_or(&[], Vec::as_slice)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MemberRouteKey {
    owner_qualified_name: String,
    member_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct MethodRouteKey {
    owner_base_qualified_name: String,
    method_name: String,
}

impl MethodRouteKey {
    fn new(owner_base_qualified_name: &str, method_name: &str) -> Self {
        Self {
            owner_base_qualified_name: owner_base_qualified_name.to_string(),
            method_name: method_name.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct MethodPatternRoute {
    method_name_pattern: String,
    plugin_id: String,
}

impl MethodPatternRoute {
    fn new(method_name_pattern: &str, plugin_id: String) -> Self {
        Self {
            method_name_pattern: method_name_pattern.to_string(),
            plugin_id,
        }
    }
}

fn matching_method_pattern_plugins<'a>(
    routes: &'a BTreeMap<String, Vec<MethodPatternRoute>>,
    base_qualified_name: &str,
    method_name: &str,
) -> Vec<&'a str> {
    routes
        .get(base_qualified_name)
        .map_or_else(Vec::new, |routes| {
            routes
                .iter()
                .filter(|route| {
                    method_name_pattern_matches(&route.method_name_pattern, method_name)
                })
                .map(|route| route.plugin_id.as_str())
                .collect()
        })
}

impl MemberRouteKey {
    fn new(owner_qualified_name: &str, member_name: &str) -> Self {
        Self {
            owner_qualified_name: owner_qualified_name.to_string(),
            member_name: member_name.to_string(),
        }
    }
}

pub trait PluginRunner {
    fn execute(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
    ) -> Result<PluginResponse, RuntimeError>;

    /// Execute independent handlers whose author guarantees no observable
    /// mutable module state between requests. The default keeps fresh calls.
    /// One batch retains the ordinary resource and response-size ceilings.
    fn execute_stateless_batch_with_project_index(
        &self,
        plugin: &LoadedPlugin,
        requests: &[&PluginRequest],
        index: &serde_json::Value,
    ) -> Result<Vec<PluginResponse>, RuntimeError> {
        if requests.len() > 64 {
            return Err(RuntimeError::InvalidResponse(
                "stateless batch exceeds 64 requests".into(),
            ));
        }
        requests
            .iter()
            .map(|request| self.execute_with_project_index(plugin, request, index))
            .collect()
    }

    /// Borrow a plugin's index for one stateless call. Runtimes without lazy
    /// lookup support retain the ordinary inline wire representation.
    fn execute_with_project_index(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
        index: &serde_json::Value,
    ) -> Result<PluginResponse, RuntimeError> {
        let mut request = request.clone();
        let slot = request
            .project_index_mut()
            .ok_or_else(|| RuntimeError::InvalidResponse("request has no project index".into()))?;
        *slot = Some(index.clone());
        self.execute(plugin, &request)
    }
}

#[derive(Debug, Clone)]
pub struct PluginHost<R> {
    environment: PluginEnvironment,
    runner: R,
}

impl<R> PluginHost<R> {
    pub fn new(environment: PluginEnvironment, runner: R) -> Self {
        Self {
            environment,
            runner,
        }
    }

    pub fn environment(&self) -> &PluginEnvironment {
        &self.environment
    }
}

impl<R: PluginRunner> PluginHost<R> {
    pub fn execute_stateless_batch_with_project_index(
        &self,
        plugin_id: &str,
        requests: &[&PluginRequest],
        index: &serde_json::Value,
    ) -> Result<Vec<PluginResponse>, HostError> {
        let plugin = self
            .environment
            .plugin(plugin_id)
            .ok_or_else(|| HostError::UnknownPlugin(plugin_id.to_owned()))?;
        self.runner
            .execute_stateless_batch_with_project_index(plugin, requests, index)
            .map_err(|source| HostError::Runtime {
                plugin_id: plugin_id.to_owned(),
                source,
            })
    }

    pub fn execute_with_project_index(
        &self,
        plugin_id: &str,
        request: &PluginRequest,
        index: &serde_json::Value,
    ) -> Result<PluginResponse, HostError> {
        let plugin = self
            .environment
            .plugin(plugin_id)
            .ok_or_else(|| HostError::UnknownPlugin(plugin_id.to_string()))?;
        self.runner
            .execute_with_project_index(plugin, request, index)
            .map_err(|source| HostError::Runtime {
                plugin_id: plugin_id.to_owned(),
                source,
            })
    }

    pub fn execute(
        &self,
        plugin_id: &str,
        request: &PluginRequest,
    ) -> Result<PluginResponse, HostError> {
        let plugin = self
            .environment
            .plugin(plugin_id)
            .ok_or_else(|| HostError::UnknownPlugin(plugin_id.to_string()))?;

        self.runner
            .execute(plugin, request)
            .map_err(|source| HostError::Runtime {
                plugin_id: plugin_id.to_string(),
                source,
            })
    }
}

#[cfg(feature = "mock")]
#[derive(Debug, Default, Clone)]
pub struct MockRunner {
    responses: BTreeMap<(String, HookKind), PluginResponse>,
}

#[cfg(feature = "mock")]
impl MockRunner {
    #[must_use]
    pub fn with_response(
        mut self,
        plugin_id: impl Into<String>,
        hook: HookKind,
        response: PluginResponse,
    ) -> Self {
        self.responses.insert((plugin_id.into(), hook), response);
        self
    }
}

#[cfg(feature = "mock")]
impl PluginRunner for MockRunner {
    fn execute(
        &self,
        plugin: &LoadedPlugin,
        request: &PluginRequest,
    ) -> Result<PluginResponse, RuntimeError> {
        Ok(self
            .responses
            .get(&(plugin.id().to_string(), HookKind::from(request)))
            .cloned()
            .unwrap_or(PluginResponse::NoChange))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HookKind {
    Manifest,
    AnalyzeClass,
    AnalyzeClasses,
    ResolveClassMember,
    ResolveInstanceMember,
    AdjustCallSignature,
    AdjustCallReturn,
    AdjustCallState,
    AdditionalDependencies,
    BuildProjectIndex,
    ValidateMutation,
}

impl From<&PluginRequest> for HookKind {
    fn from(request: &PluginRequest) -> Self {
        match request {
            PluginRequest::Manifest => Self::Manifest,
            PluginRequest::BuildProjectIndex(_) => Self::BuildProjectIndex,
            PluginRequest::AnalyzeClass(_) => Self::AnalyzeClass,
            PluginRequest::AnalyzeClasses(_) => Self::AnalyzeClasses,
            PluginRequest::ResolveClassMember(_) => Self::ResolveClassMember,
            PluginRequest::ResolveInstanceMember(_) => Self::ResolveInstanceMember,
            PluginRequest::AdjustCallSignature(_) => Self::AdjustCallSignature,
            PluginRequest::AdjustCallReturn(_) => Self::AdjustCallReturn,
            PluginRequest::AdjustCallState(_) => Self::AdjustCallState,
            PluginRequest::AdditionalDependencies(_) => Self::AdditionalDependencies,
            PluginRequest::ValidateMutation(_) => Self::ValidateMutation,
        }
    }
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), HostError> {
    if CURRENT_PROTOCOL_VERSION.negotiate(manifest.protocol_version)
        != ProtocolCompatibility::Compatible
    {
        return Err(HostError::UnsupportedProtocolVersion {
            plugin_id: manifest.id.clone(),
            major: manifest.protocol_version.major,
            minor: manifest.protocol_version.minor,
            supported_major: CURRENT_PROTOCOL_VERSION.major,
            supported_minor: CURRENT_PROTOCOL_VERSION.minor,
        });
    }

    if !manifest.stub_overlays.is_empty() && !manifest.capabilities.stub_overlays {
        return Err(HostError::StubOverlayCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if !manifest.claims.classes.is_empty() && !manifest.capabilities.class_transform {
        return Err(HostError::ClassTransformCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if manifest.claims.attributes.iter().any(|claim| {
        matches!(
            claim.kind,
            AttributeClaimKind::Exact {
                scope: AttributeScope::Class,
                ..
            }
        )
    }) && !manifest.capabilities.class_member
    {
        return Err(HostError::ClassMemberCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if manifest.claims.attributes.iter().any(|claim| {
        matches!(
            claim.kind,
            AttributeClaimKind::Exact {
                scope: AttributeScope::Instance,
                ..
            }
        )
    }) && !manifest.capabilities.instance_member
    {
        return Err(HostError::InstanceMemberCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if (!manifest.claims.functions.is_empty() || !manifest.claims.methods.is_empty())
        && !manifest.capabilities.call_signature
        && !manifest.capabilities.call_return
        && !manifest.capabilities.call_state
    {
        return Err(HostError::CallCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if !manifest.claims.constructors.is_empty() && !manifest.capabilities.call_state {
        return Err(HostError::CallCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if !manifest.claims.settings.is_empty() && !manifest.capabilities.settings_data {
        return Err(HostError::SettingsDataCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    let has_contribution_target_claims = manifest
        .claims
        .attributes
        .iter()
        .any(|claim| matches!(claim.kind, AttributeClaimKind::ContributionTarget { .. }));

    if has_contribution_target_claims && !manifest.capabilities.cross_symbol_contributions {
        return Err(HostError::CrossSymbolContributionsCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if (manifest.capabilities.cross_symbol_contributions || has_contribution_target_claims)
        && !manifest.capabilities.project_index
    {
        return Err(HostError::ProjectIndexCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    if !manifest.claims.mutations.is_empty() && !manifest.capabilities.mutation_validation {
        return Err(HostError::MutationValidationCapabilityMissing {
            plugin_id: manifest.id.clone(),
        });
    }

    Ok(())
}

fn method_qualified_name(class_qualified_name: &str, method_name: &str) -> String {
    format!("{class_qualified_name}.{method_name}")
}
