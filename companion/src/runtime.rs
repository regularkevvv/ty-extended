use crate::model::{CallSite, Oracle, Source, typ};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use ty_plugin_host::{HostError, MontyLimits, MontyRunner, PluginEnvironment, PluginHost};
use ty_plugin_protocol::{PluginManifest, PluginRequest, method_name_pattern_matches};

pub struct Runtime {
    host: PluginHost<MontyRunner>,
    load_failures: BTreeMap<String, (String, String)>,
    plugins: Vec<Registration>,
    pub manifest: Value,
    pub project: Option<Arc<Project>>,
    pub project_index: Option<Value>,
    pub config: Value,
    pub python_version: String,
    pub platform: String,
    pub project_patches: BTreeMap<String, Value>,
    pub project_contributions: Vec<Value>,
    pub projected_receivers: BTreeMap<String, String>,
    protocol_types: BTreeMap<(String, String), BTreeMap<String, Value>>,
    virtual_shapes: BTreeMap<String, Value>,
    pub selected_virtual_scope: Option<String>,
    pub nominal_candidates: BTreeSet<String>,
    pub annotation_candidates: BTreeSet<String>,
    cache: RequestCache,
    type_cache: RefCell<RequestCache>,
    projection_generation: u64,
    pub executions: usize,
    pub hits: usize,
    pub execution_time: Duration,
}

#[derive(Clone)]
struct Registration {
    id: String,
    manifest: Value,
    config: Value,
    index: Option<Value>,
    index_generation: u64,
    virtual_names: BTreeMap<String, String>,
    inverse_virtual_names: BTreeMap<String, String>,
}

type RequestKey = [u8; 32];

fn type_key(
    qualified: bool,
    expression: &str,
    module: Option<&str>,
    virtual_scope: Option<&str>,
) -> RequestKey {
    let mut digest = Sha256::new();
    digest.update([u8::from(qualified)]);
    for part in [Some(expression), module, virtual_scope] {
        // None and an empty module name are different protocol contexts.
        digest.update([u8::from(part.is_some())]);
        let part = part.unwrap_or_default();
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part.as_bytes());
    }
    digest.finalize().into()
}

/// Serializes directly into the digest, so project summaries need no second
/// megabyte-sized allocation just to look up a cached result.
fn request_key(value: &impl serde::Serialize) -> Result<RequestKey> {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0.finalize().into())
}

/// Decode the known payload directly instead of buffering its entire tree in
/// serde's internally tagged enum representation first. These protocol structs
/// accept unknown fields, so the existing `kind` field is ignored by them.
/// Retain the input for the original decoder on failure: its content buffering
/// accepts some unusual nested enum representations that Value does not, and
/// also determines the exact diagnostics for invalid requests.
fn decode_request(mut payload: Value) -> serde_json::Result<PluginRequest> {
    let indexed = matches!(
        payload.get("kind").and_then(Value::as_str),
        Some(
            "analyze-class"
                | "resolve-class-member"
                | "resolve-instance-member"
                | "adjust-call-signature"
                | "adjust-call-return"
                | "adjust-call-state"
                | "validate-mutation"
        )
    );
    // The index is opaque JSON, often much larger than the semantic request.
    // Move it directly into the result instead of cloning it through borrowed
    // deserialization. A null placeholder decodes as Option::None and retains
    // the exact original map slot for restoration if the fallback is needed.
    let index = if indexed {
        payload.get_mut("project-index").map(Value::take)
    } else {
        None
    };
    let direct = match payload.get("kind").and_then(Value::as_str) {
        Some("build-project-index") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::BuildProjectIndex)
        }
        Some("analyze-class") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::AnalyzeClass)
        }
        Some("resolve-class-member") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::ResolveClassMember)
        }
        Some("resolve-instance-member") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::ResolveInstanceMember)
        }
        Some("adjust-call-signature") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::AdjustCallSignature)
        }
        Some("adjust-call-return") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::AdjustCallReturn)
        }
        Some("adjust-call-state") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::AdjustCallState)
        }
        Some("additional-dependencies") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::AdditionalDependencies)
        }
        Some("validate-mutation") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::ValidateMutation)
        }
        // Preserve the protocol enum's validation and diagnostics for malformed
        // discriminants, future variants, and the fieldless manifest request.
        _ => return serde_json::from_value(payload),
    };
    match direct {
        Ok(mut request) => {
            let index = index.filter(|value| !value.is_null());
            match &mut request {
                PluginRequest::AnalyzeClass(request) => request.project_index = index,
                PluginRequest::ResolveClassMember(request)
                | PluginRequest::ResolveInstanceMember(request) => request.project_index = index,
                PluginRequest::AdjustCallSignature(request)
                | PluginRequest::AdjustCallReturn(request)
                | PluginRequest::AdjustCallState(request) => request.project_index = index,
                PluginRequest::ValidateMutation(request) => request.project_index = index,
                _ => {}
            }
            Ok(request)
        }
        Err(_) => {
            if let Some(index) = index {
                payload["project-index"] = index;
            }
            serde_json::from_value(payload)
        }
    }
}

#[derive(Default)]
struct RequestCache {
    entries: BTreeMap<RequestKey, CachedResponse>,
    oldest: BTreeSet<(u64, RequestKey)>,
    clock: u64,
    bytes: usize,
}

struct CachedResponse {
    value: Value,
    touched: u64,
    bytes: usize,
}

impl RequestCache {
    const MAX_ENTRIES: usize = 8192;
    const MAX_BYTES: usize = 64 * 1024 * 1024;

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn get(&mut self, key: &RequestKey) -> Option<&Value> {
        let entry = self.entries.get_mut(key)?;
        self.oldest.remove(&(entry.touched, *key));
        self.clock += 1;
        entry.touched = self.clock;
        self.oldest.insert((entry.touched, *key));
        Some(&entry.value)
    }

    fn insert(&mut self, key: RequestKey, value: Value) {
        // Include allocation/container overhead as well as string data. Bounds
        // apply to retained response storage, independent of project lifetime.
        fn weight(value: &Value) -> usize {
            std::mem::size_of::<Value>()
                + match value {
                    Value::String(value) => value.capacity(),
                    Value::Array(values) => values.iter().map(weight).sum(),
                    Value::Object(values) => values
                        .iter()
                        .map(|(name, value)| name.capacity() + 64 + weight(value))
                        .sum(),
                    _ => 0,
                }
        }
        let bytes = weight(&value) + std::mem::size_of::<CachedResponse>() + 128;
        if bytes > Self::MAX_BYTES {
            return;
        }
        if let Some(previous) = self.entries.remove(&key) {
            self.oldest.remove(&(previous.touched, key));
            self.bytes -= previous.bytes;
        }
        while self.entries.len() >= Self::MAX_ENTRIES || self.bytes + bytes > Self::MAX_BYTES {
            let Some((_, key)) = self.oldest.pop_first() else {
                break;
            };
            self.bytes -= self.entries.remove(&key).unwrap().bytes;
        }
        self.clock += 1;
        self.entries.insert(
            key,
            CachedResponse {
                value,
                touched: self.clock,
                bytes,
            },
        );
        self.oldest.insert((self.clock, key));
        self.bytes += bytes;
    }
}

impl Runtime {
    pub fn new(manifest_path: &Path, plugin_path: &Path) -> Result<Self> {
        Self::from_plugins(&[(
            manifest_path.to_path_buf(),
            plugin_path.to_path_buf(),
            json!({}),
        )])
    }

    pub fn from_plugins(paths: &[(PathBuf, PathBuf, Value)]) -> Result<Self> {
        let mut plugins = Vec::new();
        let mut manifests = Vec::new();
        let mut load_failures = BTreeMap::new();
        let mut runner = MontyRunner::new(MontyLimits::default())?;
        for (manifest_path, plugin_path, overrides) in paths {
            let data = std::fs::read(manifest_path)?;
            let manifest: PluginManifest = serde_json::from_slice(&data)?;
            let manifest_value: Value = serde_json::from_slice(&data)?;
            let mut config = manifest_value
                .get("default-config")
                .cloned()
                .unwrap_or(json!({}));
            merge_config(&mut config, overrides);
            match std::fs::read(plugin_path) {
                Ok(bytes) => {
                    if let Err(error) = runner.add_plugin(&manifest.id, bytes) {
                        load_failures.insert(
                            manifest.id.clone(),
                            (error.to_string(), error.hint().to_owned()),
                        );
                    }
                }
                Err(error) => {
                    load_failures.insert(
                        manifest.id.clone(),
                        (
                            format!(
                                "failed to read artifact for plugin `{}`: {error}",
                                manifest.id
                            ),
                            "Fix the plugin artifact path or remove the plugin.".to_owned(),
                        ),
                    );
                }
            }
            plugins.push(Registration {
                id: manifest.id.clone(),
                manifest: manifest_value,
                config,
                index: None,
                index_generation: 0,
                virtual_names: BTreeMap::new(),
                inverse_virtual_names: BTreeMap::new(),
            });
            manifests.push(manifest);
        }
        // The fork creates its semantic environment from the host's BTreeMap,
        // so first-claim and first-field precedence are lexical plugin-id order.
        plugins.sort_by(|left, right| left.id.cmp(&right.id));
        let environment = PluginEnvironment::from_manifests(manifests)?;
        let mut manifest_value = plugins
            .first()
            .map(|plugin| plugin.manifest.clone())
            .unwrap_or_else(|| json!({"id":"companion.empty","capabilities":{},"claims":{}}));
        for plugin in plugins.iter().skip(1) {
            for (capability, enabled) in plugin.manifest["capabilities"]
                .as_object()
                .into_iter()
                .flatten()
            {
                if enabled == true {
                    manifest_value["capabilities"][capability] = json!(true);
                }
            }
            for (key, values) in plugin.manifest["claims"].as_object().into_iter().flatten() {
                if let Some(values) = values.as_array() {
                    let aggregate = manifest_value["claims"]
                        .as_object_mut()
                        .unwrap()
                        .entry(key.clone())
                        .or_insert(json!([]))
                        .as_array_mut()
                        .unwrap();
                    for value in values {
                        if !aggregate.contains(value) {
                            aggregate.push(value.clone());
                        }
                    }
                }
            }
        }
        let mut settings = Vec::new();
        for plugin in &plugins {
            for claim in plugin.manifest["claims"]["settings"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let resolved = claim["config-key"]
                    .as_str()
                    .and_then(|key| plugin.config[key].as_str());
                let claim =
                    resolved.map_or_else(|| claim.clone(), |module| json!({"module":module}));
                if !settings.contains(&claim) {
                    settings.push(claim);
                }
            }
        }
        if !settings.is_empty() {
            manifest_value["claims"]["settings"] = json!(settings);
        }
        Ok(Self {
            host: PluginHost::new(environment, runner),
            load_failures,
            config: plugins
                .first()
                .map(|plugin| plugin.config.clone())
                .unwrap_or(json!({})),
            plugins,
            python_version: "3.13".into(),
            platform: match std::env::consts::OS {
                "macos" => "darwin",
                "windows" => "win32",
                platform => platform,
            }
            .into(),
            manifest: manifest_value,
            project: None,
            project_index: None,
            project_patches: BTreeMap::new(),
            project_contributions: Vec::new(),
            projected_receivers: BTreeMap::new(),
            protocol_types: BTreeMap::new(),
            virtual_shapes: BTreeMap::new(),
            selected_virtual_scope: None,
            nominal_candidates: BTreeSet::new(),
            annotation_candidates: BTreeSet::new(),
            cache: RequestCache::default(),
            type_cache: RefCell::default(),
            projection_generation: 0,
            executions: 0,
            hits: 0,
            execution_time: Duration::ZERO,
        })
    }

    pub fn attach_project(&mut self, project: Arc<Project>, config: Value) {
        // A type snapshot can depend on imports, ancestry and structural record
        // declarations anywhere in the project. Retain memoized snapshots only
        // within this immutable project revision; never key them by an address.
        self.type_cache.get_mut().clear();
        self.advance_projection_generation();
        self.protocol_types
            .retain(|(module, _), _| project.modules.contains_key(module));
        self.project = Some(project);
        merge_config(&mut self.config, &config);
        // --config is the original single-plugin CLI override. Discovered
        // plugins retain their independently resolved configuration.
        if self.plugins.len() == 1 {
            merge_config(&mut self.plugins[0].config, &config);
        }
        // Monty evaluates each request from fresh module state with a fixed
        // clock/random seed and no filesystem access. Every observable input is
        // in the request; changing a source snapshot does not invalidate equal
        // requests. Configuration is included in each request's cache key.
    }

    /// Semantic results can depend on each plugin's opaque project index even
    /// when the merged contributions and virtual types have not changed.
    pub fn index_generations(&self) -> Vec<(String, u64)> {
        self.plugins
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.index_generation))
            .collect()
    }

    /// Changes observable by another file during one fixed-point loop. The
    /// surrounding check fixes plugin registrations/configuration, the opaque
    /// project indexes, contributions and the virtual display scope before
    /// entering that loop. This token is not a cross-revision cache key.
    pub fn projection_generation(&self) -> u64 {
        self.projection_generation
    }

    fn advance_projection_generation(&mut self) {
        self.projection_generation = self
            .projection_generation
            .checked_add(1)
            .expect("projection generation exhausted");
    }

    pub fn set_project_patch(&mut self, qualified: String, patch: Value) {
        if self.project_patches.get(&qualified) != Some(&patch) {
            self.project_patches.insert(qualified, patch);
            self.advance_projection_generation();
        }
    }

    pub fn set_projected_receiver(&mut self, qualified: String, receiver: String) {
        if self.projected_receivers.get(&qualified) != Some(&receiver) {
            self.projected_receivers.insert(qualified, receiver);
            self.type_cache.get_mut().clear();
            self.advance_projection_generation();
        }
    }

    pub fn virtual_display_names(&self) -> BTreeMap<String, String> {
        self.plugins
            .iter()
            .flat_map(|plugin| {
                plugin
                    .inverse_virtual_names
                    .iter()
                    .map(|(generated, original)| (generated.clone(), original.clone()))
            })
            .collect()
    }

    pub fn canonical_name(&self, source: &Source, name: &str) -> String {
        if let Some(owner) = self
            .projected_receivers
            .get(&format!("{}.{}", source.module, name))
            .or_else(|| self.projected_receivers.get(name))
        {
            return owner.clone();
        }
        self.project.as_ref().map_or_else(
            || name.to_string(),
            |project| {
                let qualified = project.canonical_name(&source.module, name);
                let (nominal, _) = generic_parts(&qualified);
                // Display forms such as Literal[1] are checker/typing syntax,
                // not evidence for an unrelated user class called Literal.
                if matches!(
                    nominal,
                    "Literal"
                        | "Self"
                        | "Any"
                        | "Unknown"
                        | "Never"
                        | "NoReturn"
                        | "Callable"
                        | "ClassVar"
                        | "Final"
                        | "Optional"
                        | "Union"
                        | "Type"
                        | "Tuple"
                        | "List"
                        | "Dict"
                        | "Set"
                        | "FrozenSet"
                        | "Annotated"
                        | "TypeGuard"
                        | "TypeIs"
                        | "Unpack"
                        | "Required"
                        | "NotRequired"
                        | "ReadOnly"
                        | "Concatenate"
                        | "TypeVar"
                        | "ParamSpec"
                        | "TypeVarTuple"
                        | "Generic"
                        | "Protocol"
                        | "TypeAlias"
                        | "TypeAliasType"
                        | "LiteralString"
                        | "object"
                        | "type"
                        | "str"
                        | "int"
                        | "bool"
                        | "float"
                        | "complex"
                        | "bytes"
                        | "bytearray"
                        | "tuple"
                        | "list"
                        | "dict"
                        | "set"
                        | "frozenset"
                        | "None"
                ) {
                    return qualified;
                }
                if !nominal.contains('.')
                    && nominal
                        .chars()
                        .all(|character| character.is_alphanumeric() || character == '_')
                {
                    let mut matches = project
                        .classes
                        .keys()
                        .filter(|candidate| candidate.rsplit('.').next() == Some(nominal));
                    if let Some(candidate) = matches.next()
                        && matches.next().is_none()
                    {
                        return format!("{candidate}{}", &qualified[nominal.len()..]);
                    }
                }
                qualified
            },
        )
    }

    pub fn protocol_name(&self, source: &Source, name: &str) -> String {
        let Some(module) = &source.protocol_module else {
            return name.to_owned();
        };
        name.strip_prefix(&format!("{}.", source.module))
            .map_or_else(
                || name.to_owned(),
                |name| {
                    if module.is_empty() {
                        name.to_owned()
                    } else {
                        format!("{module}.{name}")
                    }
                },
            )
    }

    fn internal_protocol_name(&self, source: &Source, name: &str) -> String {
        if source.protocol_module.is_some() {
            self.canonical_name(source, name)
        } else {
            name.to_owned()
        }
    }

    pub fn member_type(
        &self,
        source: &Source,
        owner: &str,
        member: &str,
        class_access: bool,
    ) -> Option<Value> {
        fn access_type(access: &Value, class_access: bool) -> Option<Value> {
            match access["kind"].as_str().unwrap_or("value") {
                "value" => access.get("type-expr").cloned(),
                "descriptor" => access
                    .get(if class_access {
                        "class-type"
                    } else {
                        "instance-get-type"
                    })
                    .cloned(),
                _ => None,
            }
        }
        fn patch_type(patch: &Value, class_access: bool) -> Option<Value> {
            if patch.get("instance-get-type").is_some() {
                if class_access {
                    patch
                        .get("descriptor")
                        .and_then(|descriptor| access_type(descriptor, true))
                } else {
                    patch.get("instance-get-type").cloned()
                }
            } else {
                access_type(&patch["access"], class_access)
            }
        }
        let _ = source;
        let (nominal, arguments) = generic_parts(owner);
        let nominal = if matches!(nominal, "type" | "builtins.type") {
            arguments.first().copied().unwrap_or(nominal)
        } else {
            nominal
        };
        let (nominal, _) = generic_parts(nominal);
        let mro = self
            .project
            .as_ref()
            .and_then(|project| project.mro(nominal))
            .unwrap_or_else(|| vec![nominal.to_string()]);
        for owner in mro {
            if let Some(patch) = self.project_patches.get(&owner) {
                for field in patch["fields"].as_array().into_iter().flatten() {
                    if field["name"] == member
                        && let Some(value) = patch_type(field, class_access)
                    {
                        return Some(value);
                    }
                }
                for field in patch[if class_access {
                    "class-members"
                } else {
                    "instance-members"
                }]
                .as_array()
                .into_iter()
                .flatten()
                {
                    if field["name"] == member
                        && let Some(value) = access_type(&field["access"], class_access)
                    {
                        return Some(value);
                    }
                }
            }
            for contribution in self.project_contributions.iter().rev() {
                if contribution["target"]["qualified-name"] != owner
                    || contribution["patch"]["name"] != member
                {
                    continue;
                }
                if contribution["target"]["kind"] == "class" && !class_access {
                    continue;
                }
                if let Some(value) = patch_type(&contribution["patch"], class_access) {
                    return Some(value);
                }
            }
            if let Some(mut declared) = self
                .project
                .as_ref()
                .and_then(|project| project.declared_member_type(&owner, member))
                && let Some(expression) = declared["expression"].as_str()
            {
                let (outer, arguments) = generic_parts(expression);
                let expression = if matches!(
                    outer,
                    "ClassVar" | "typing.ClassVar" | "typing_extensions.ClassVar"
                ) {
                    arguments.first().copied().unwrap_or(expression)
                } else {
                    expression
                };
                declared["expression"] =
                    json!(crate::project::substitute_self(expression, nominal));
                return Some(declared);
            }
        }
        None
    }

    /// Recover a protocol type from a known class/member patch before consulting
    /// the checker's deliberately abbreviated display. This is generic patch
    /// provenance, not an inference rule for a particular plugin or framework.
    pub fn expression_type(
        &self,
        source: &Source,
        expression: &ruff_python_ast::Expr,
    ) -> Option<Value> {
        use crate::model::span;
        use ruff_python_ast::Expr;
        match expression {
            Expr::Name(_) | Expr::Attribute(_) => {
                let symbol = self.canonical_name(source, source.slice(span(expression)));
                if self
                    .project
                    .as_ref()
                    .is_some_and(|project| project.class_summary(&symbol).is_some())
                {
                    return Some(typ(format!("type[{symbol}]")));
                }
                if let Expr::Attribute(attribute) = expression {
                    let parent = self.expression_type(source, &attribute.value)?;
                    let parent = parent["expression"].as_str()?;
                    let (nominal, _) = generic_parts(parent);
                    return self.member_type(
                        source,
                        parent,
                        attribute.attr.as_str(),
                        matches!(nominal, "type" | "builtins.type"),
                    );
                }
                None
            }
            Expr::Call(call) => {
                let symbol = self.canonical_name(source, source.slice(span(call.func.as_ref())));
                self.project
                    .as_ref()
                    .and_then(|project| project.default_constructor_result(&symbol))
                    .map(typ)
            }
            _ => None,
        }
    }

    pub fn type_expr(&self, source: &Source, expression: &str) -> Value {
        if let Some(owner) = self
            .projected_receivers
            .get(&format!("{}.{}", source.module, expression))
        {
            return self.qualified_type_expr(owner);
        }
        // Display spelling and nominal identity are independent SDK fields.
        // Plugins may inspect either: keep the checker spelling while resolving
        // imported classes for the structural snapshot.
        if expression.starts_with("Self@") || expression == "Self" {
            return json!({"expression":"Self","mode":"annotation","snapshot":{"kind":"self-type"}});
        }
        let key = type_key(false, expression, Some(&source.module), None);
        if let Some(value) = self.type_cache.borrow_mut().get(&key).cloned() {
            return value;
        }
        let display = self.project.as_ref().map_or_else(
            || expression.to_owned(),
            |project| project.rewrite_virtual_identities(expression),
        );
        let qualified = self.canonical_name(source, expression);
        let value = self.type_expr_with_identity(display, &qualified, Some(&source.module));
        self.type_cache.borrow_mut().insert(key, value.clone());
        value
    }

    /// Types obtained from protocol metadata and established flow identities
    /// are already qualified. Source aliases must not be applied a second time.
    pub fn qualified_type_expr(&self, expression: &str) -> Value {
        self.qualified_type_expr_in_module(expression, None)
    }

    fn qualified_type_expr_in_module(&self, expression: &str, module: Option<&str>) -> Value {
        let key = type_key(
            true,
            expression,
            module,
            self.selected_virtual_scope.as_deref(),
        );
        if let Some(value) = self.type_cache.borrow_mut().get(&key).cloned() {
            return value;
        }
        let mut value = self.type_expr_with_identity(expression.to_owned(), expression, module);
        // Native virtual named tuples carry their protocol identity in the
        // snapshot, but their displayed nominal name belongs to the first
        // checked module. Keep that lossy display separate from the snapshot.
        let mut displayed = value["snapshot"].clone();
        self.qualify_virtual_display(&mut displayed);
        value["expression"] = json!(crate::type_snapshot::qualified_expression(
            &displayed,
            value["expression"].as_str().unwrap_or(expression),
            module
        ));
        self.type_cache.borrow_mut().insert(key, value.clone());
        value
    }

    fn qualify_virtual_display(&self, value: &mut Value) {
        if value["kind"] == "plugin-class"
            && let Some(identity) = value["identity"].as_str()
            && self
                .virtual_shapes
                .get(identity)
                .is_some_and(|shape| shape["kind"] == "named-tuple")
            && let Some(module) = &self.selected_virtual_scope
        {
            let short = identity.rsplit('.').next().unwrap_or(identity);
            value["identity"] = json!(if module.is_empty() {
                short.to_owned()
            } else {
                format!("{module}.{short}")
            });
            return;
        }
        match value {
            Value::Array(values) => values
                .iter_mut()
                .for_each(|value| self.qualify_virtual_display(value)),
            Value::Object(values) => values
                .values_mut()
                .for_each(|value| self.qualify_virtual_display(value)),
            _ => {}
        }
    }

    fn virtual_snapshot(&self, name: &str) -> Option<Value> {
        let shape = self.virtual_shapes.get(name)?;
        if shape["kind"] != "typed-dict" {
            return Some(json!({"kind":"plugin-class","identity":name}));
        }
        // Native virtual TypedDict definitions decode field annotations without
        // virtual-class context, and always produce a closed structural record.
        let fields: Vec<_> = shape["fields"].as_array().into_iter().flatten().map(|field| {
            let annotation = &field["type-expr"];
            let snapshot = crate::type_snapshot::decoded_annotation(annotation);
            json!({"name":field["name"],"type-snapshot":snapshot,
                "required":shape["total"].as_bool().unwrap_or(true) && field["required"].as_bool().unwrap_or(true),
                "read-only":field["read-only"].as_bool().unwrap_or(false)})
        }).collect();
        Some(json!({"kind":"typed-dict","fields":fields,"closed":true}))
    }

    pub fn register_protocol_types(
        &mut self,
        module: &str,
        prefix: &str,
        types: BTreeMap<String, Value>,
    ) {
        let key = (module.to_owned(), prefix.to_owned());
        // An absent and an empty alias registry have identical lookups. Avoid
        // invalidating every cached type when a file contributes no aliases.
        if types.is_empty() && !self.protocol_types.contains_key(&key) {
            return;
        }
        if self.protocol_types.get(&key) != Some(&types) {
            if types.is_empty() {
                self.protocol_types.remove(&key);
            } else {
                self.protocol_types.insert(key, types);
            }
            self.type_cache.get_mut().clear();
            self.advance_projection_generation();
        }
    }

    pub fn set_virtual_shapes(&mut self, shapes: BTreeMap<String, Value>) {
        if self.virtual_shapes != shapes {
            self.virtual_shapes = shapes;
            self.type_cache.get_mut().clear();
            self.advance_projection_generation();
        }
    }

    fn protocol_type(&self, module: Option<&str>, name: &str) -> Option<&Value> {
        self.protocol_types.iter().find_map(|((owner, _), types)| {
            if module == Some(owner.as_str()) {
                types.get(name)
            } else {
                None
            }
            .or_else(|| {
                name.strip_prefix(&format!("{owner}."))
                    .and_then(|name| types.get(name))
            })
        })
    }

    fn type_expr_with_identity(
        &self,
        display: String,
        qualified: &str,
        module: Option<&str>,
    ) -> Value {
        let display = crate::project::rewrite_names(&display, |name| {
            self.protocol_type(module, name)
                .and_then(|value| value["expression"].as_str())
                .map_or_else(|| name.to_owned(), str::to_owned)
        });
        let mut value = typ(display);
        value["snapshot"] = crate::type_snapshot::from_annotation(qualified, &|name| {
            if let Some(value) = self.protocol_type(module, name) {
                return value.get("snapshot").cloned();
            }
            if let Some(snapshot) = self.virtual_snapshot(name) {
                return Some(snapshot);
            }
            if self.project.as_ref().is_some_and(|project| {
                project
                    .virtual_identities
                    .values()
                    .any(|identity| identity == name)
            }) {
                return Some(json!({"kind":"plugin-class","identity":name}));
            }
            if let Some(project) = &self.project {
                return crate::type_snapshot::project_record(project, name, &mut BTreeSet::new());
            }
            None
        });
        value
    }

    /// A Self result must retain its bound when it becomes the next receiver.
    /// The display expression is still Self and must not be used as a class key.
    pub fn receiver_identity(value: &Value) -> Option<String> {
        if let Some(identity) = crate::type_snapshot::receiver_annotation(&value["snapshot"]) {
            return Some(identity);
        }
        crate::project::type_expression(value)
    }

    fn nominal_name(&self, source: &Source, expression: &str) -> String {
        let (nominal, arguments) = generic_parts(expression);
        let nominal = if matches!(nominal, "type" | "builtins.type") {
            arguments.first().map_or_else(
                || nominal.to_owned(),
                |inner| generic_parts(inner).0.to_owned(),
            )
        } else {
            nominal.to_owned()
        };
        if self.project.is_none() && !nominal.contains('.') {
            format!("{}.{}", source.module, nominal)
        } else {
            nominal
        }
    }

    fn subclass(&self, class: &str, base: &str) -> bool {
        class == base
            || self
                .project
                .as_ref()
                .is_some_and(|p| p.is_subclass(class, base))
    }

    fn class_claim(&self, source: &Source, claim: &Value, qualified: &str) -> bool {
        match claim["kind"].as_str().unwrap_or("exact") {
            "exact" => claim["qualified-name"] == self.protocol_name(source, qualified),
            "subclass-of" => claim["base-qualified-name"].as_str().is_some_and(|base| {
                self.subclass(qualified, &self.internal_protocol_name(source, base))
            }),
            _ => false,
        }
    }

    /// Resolve the function actually defined in the receiver's MRO. State
    /// hooks must not refine an overriding method merely because its receiver
    /// is a subclass of a claimed class.
    pub fn method_callee(&self, owner: &str, method: &str) -> String {
        let (owner, _) = generic_parts(owner);
        self.project
            .as_ref()
            .and_then(|project| project.method_owner(owner, method))
            .map(|owner| format!("{owner}.{method}"))
            .unwrap_or_else(|| format!("{owner}.{method}"))
    }

    #[cfg(test)]
    pub fn receiver_summary(&self, source: &Source, expression: &str) -> Value {
        let qualified = self.canonical_name(source, expression);
        self.qualified_receiver_summary(source, &qualified)
    }

    fn qualified_receiver_summary(&self, source: &Source, qualified: &str) -> Value {
        let (outer, outer_arguments) = generic_parts(qualified);
        let summary_type = if matches!(outer, "type" | "builtins.type") {
            outer_arguments.first().copied().unwrap_or(qualified)
        } else {
            qualified
        };
        let (nominal, arguments) = generic_parts(summary_type);
        // A nongeneric subclass can inherit a concrete generic specialization.
        // This mirrors the SDK receiver-summary contract; the original receiver
        // expression remains available independently of the summary class.
        if arguments.is_empty()
            && let Some(project) = &self.project
            && let Some((base, args)) = generic_base(project, nominal, &mut BTreeSet::new())
        {
            return json!({"type-expr":self.qualified_type_expr_in_module(qualified,Some(&source.module)),"nominal-class":base,
                        "generic-arguments":args.into_iter().map(|arg|self.qualified_type_expr_in_module(&arg,Some(&source.module))).collect::<Vec<_>>(),"plugin-metadata":null});
        }
        let nominal = if self.project.is_none() && !nominal.contains('.') {
            format!("{}.{}", source.module, nominal)
        } else {
            nominal.to_string()
        };
        json!({"type-expr":self.qualified_type_expr_in_module(qualified,Some(&source.module)),"nominal-class":nominal,
            "generic-arguments":arguments.iter().map(|arg| self.qualified_type_expr_in_module(arg,Some(&source.module))).collect::<Vec<_>>(),"plugin-metadata":null})
    }

    pub fn nominal_receiver(&self, source: &Source, expression: &str) -> bool {
        let _ = source;
        let (nominal, arguments) = generic_parts(expression);
        if matches!(nominal, "type" | "builtins.type") {
            return arguments
                .first()
                .is_some_and(|inner| self.nominal_receiver(source, inner));
        }
        if matches!(
            nominal,
            "Unknown"
                | "Any"
                | "typing.Any"
                | "@Todo"
                | "None"
                | "Never"
                | "Module"
                | "ModuleType"
                | "types.ModuleType"
                | "Literal"
                | "typing.Literal"
        ) {
            return false;
        }
        if !nominal
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
        {
            return false;
        }
        true
    }

    pub fn default_member_expr(&self, owner: &str, member: &str) -> Option<Value> {
        let (owner, _) = generic_parts(owner);
        if let Some(patch) = self.project_patches.get(owner) {
            for field in patch["fields"].as_array().into_iter().flatten() {
                if field["name"] == member {
                    return field["instance-get-type"]["expression"]
                        .as_str()
                        .map(|_| field["instance-get-type"].clone());
                }
            }
            for field in patch["instance-members"].as_array().into_iter().flatten() {
                if field["name"] == member {
                    return field["access"]["type-expr"]["expression"]
                        .as_str()
                        .map(|_| field["access"]["type-expr"].clone());
                }
            }
        }
        for contribution in self.project_contributions.iter().rev() {
            if contribution["target"]["qualified-name"] == owner
                && contribution["patch"]["name"] == member
            {
                let patch = &contribution["patch"];
                if let Some(value) = patch
                    .get("instance-get-type")
                    .or_else(|| patch["access"].get("type-expr"))
                {
                    return Some(value.clone());
                }
            }
        }
        self.project
            .as_ref()
            .and_then(|project| project.class_summary(owner))
            .and_then(|class| class["fields"].as_array())
            .and_then(|fields| fields.iter().find(|field| field["name"] == member))
            .and_then(|field| field.get("annotation"))
            .cloned()
    }

    pub fn resolve_member(
        &mut self,
        source: &Source,
        owner: &str,
        member: &str,
        class_access: bool,
        existing: Option<&Value>,
    ) -> Result<Value> {
        let nominal = self.nominal_name(source, owner);
        let mro = self
            .project
            .as_ref()
            .and_then(|project| project.mro(&nominal))
            .unwrap_or_else(|| vec![nominal]);
        for candidate in mro {
            let candidate = if class_access {
                format!("type[{candidate}]")
            } else {
                candidate
            };
            let mut payload =
                json!({"owner":self.qualified_type_expr(&candidate), "member-name":member});
            if let Some(existing) = existing {
                payload["existing-member"] = json!({"name":member,"access":{"kind":"value","type-expr":existing},"is-read-only":false});
            }
            let response = self.execute(
                source,
                if class_access {
                    "resolve-class-member"
                } else {
                    "resolve-instance-member"
                },
                payload,
            )?;
            if response["kind"] == "member-patch" && response["name"] == member {
                return Ok(response);
            }
        }
        Ok(json!({"kind":"no-change"}))
    }

    fn allowed(&self, source: &Source, kind: &str, payload: &Value, manifest: &Value) -> bool {
        let capability = match kind {
            "build-project-index" => "project-index",
            "analyze-class" => "class-transform",
            "resolve-class-member" => "class-member",
            "resolve-instance-member" => "instance-member",
            "adjust-call-signature" => "call-signature",
            "adjust-call-return" => "call-return",
            "adjust-call-state" => "call-state",
            "validate-mutation" => "mutation-validation",
            "additional-dependencies" => "additional-dependencies",
            _ => return false,
        };
        if manifest["capabilities"][capability] != true {
            return false;
        }
        let claims = &manifest["claims"];
        let list = |key: &str| claims[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
        match kind {
            "build-project-index" | "additional-dependencies" => true,
            "analyze-class" => payload["class"]["qualified-name"]
                .as_str()
                .is_some_and(|name| {
                    // The fork's semantic environment treats both class claim
                    // variants as class-transform ancestry candidates. Exact
                    // constructor/member claims keep their separate semantics.
                    list("classes").iter().any(|claim| {
                        claim["qualified-name"]
                            .as_str()
                            .or_else(|| claim["base-qualified-name"].as_str())
                            .is_some_and(|base| {
                                self.subclass(name, &self.internal_protocol_name(source, base))
                            })
                    })
                }),
            "adjust-call-signature" | "adjust-call-return" | "adjust-call-state" => {
                let callee = payload["callee"]["expression"].as_str().unwrap_or("");
                if list("functions")
                    .iter()
                    .any(|claim| claim["qualified-name"] == self.protocol_name(source, callee))
                {
                    return true;
                }
                // Constructors are state claims in the existing SDK. Ordinary
                // constructor signatures are returned by class transforms.
                if kind == "adjust-call-state"
                    && payload.get("receiver").is_none()
                    && list("constructors")
                        .iter()
                        .any(|claim| self.class_claim(source, claim, callee))
                {
                    return true;
                }
                let method = callee.rsplit('.').next().unwrap_or(callee);
                let receiver = payload["receiver"]["nominal-class"].as_str();
                list("methods").iter().any(|claim| {
                    match claim["kind"].as_str().unwrap_or("exact") {
                        "exact" => {
                            format!(
                                "{}.{}",
                                claim["class-qualified-name"].as_str().unwrap_or(""),
                                claim["method-name"].as_str().unwrap_or("")
                            ) == self.protocol_name(source, callee)
                        }
                        "on-subclass-of" | "on-subclass-of-matching" => {
                            let Some(base) = claim["base-qualified-name"].as_str() else {
                                return false;
                            };
                            let base = self.internal_protocol_name(source, base);
                            let matches = if claim["kind"] == "on-subclass-of-matching" {
                                claim["method-name-pattern"]
                                    .as_str()
                                    .is_some_and(|pattern| {
                                        method_name_pattern_matches(pattern, method)
                                    })
                            } else {
                                claim["method-name"] == method
                            };
                            matches
                                && receiver.is_some_and(|owner| self.subclass(owner, &base))
                                && (kind != "adjust-call-state"
                                    || callee == format!("{base}.{method}"))
                        }
                        _ => false,
                    }
                })
            }
            "resolve-instance-member" | "resolve-class-member" => {
                let expression = payload["owner"]["expression"].as_str().unwrap_or("");
                let owner = self.nominal_name(source, expression);
                let scope = if kind == "resolve-class-member" {
                    "class"
                } else {
                    "instance"
                };
                list("attributes").iter().any(|claim| {
                    claim["scope"] == scope
                        && match claim["kind"].as_str().unwrap_or("exact") {
                            "exact" => {
                                claim["owner-qualified-name"] == self.protocol_name(source, &owner)
                                    && claim["attribute-name"] == payload["member-name"]
                            }
                            "on-subclass-of" if scope == "instance" => {
                                claim["owner-base-qualified-name"]
                                    .as_str()
                                    .is_some_and(|base| {
                                        self.subclass(
                                            &owner,
                                            &self.internal_protocol_name(source, base),
                                        )
                                    })
                            }
                            // Contribution targets authorize project-index patches,
                            // not a direct member hook invocation.
                            _ => false,
                        }
                })
            }
            "validate-mutation" => {
                let expression = payload["receiver"]["expression"].as_str().unwrap_or("");
                let owner = self.nominal_name(source, expression);
                list("mutations")
                    .iter()
                    .any(|claim| self.class_claim(source, claim, &owner))
            }
            _ => false,
        }
    }

    fn dispatch(&mut self, id: &str, payload: Value) -> Result<Value> {
        let key = request_key(&("project", id, &payload))?;
        self.dispatch_with_key(id, payload, key)
    }

    fn dispatch_with_key(&mut self, id: &str, payload: Value, key: RequestKey) -> Result<Value> {
        if let Some(value) = self.cache.get(&key) {
            self.hits += 1;
            return Ok(value.clone());
        }
        let kind = payload["kind"].as_str().unwrap_or("").to_owned();
        let request = decode_request(payload).context("invalid plugin request")?;
        let start = Instant::now();
        if let Some((message, hint)) = self.load_failures.get(id) {
            let response = hook_failure_message(id, &kind, message, hint);
            self.cache.insert(key, response.clone());
            return Ok(response);
        }
        let result = self.host.execute(id, &request);
        self.execution_time += start.elapsed();
        self.executions += 1;
        let response = match result {
            // The fork consumes only the response variant for the requested
            // hook. An explicit protocol Error is an abstention, unlike a host
            // failure (trap, execution limit or undecodable response).
            Ok(ty_plugin_protocol::PluginResponse::Error(_)) => json!({"kind":"no-change"}),
            Ok(response) => serde_json::to_value(response)?,
            Err(error) => hook_failure_response(id, &kind, error),
        };
        // Like the checker's Salsa memo, cache failures for the exact request.
        // Diagnostics remain in the response and are replayed at each current
        // source location, never accumulated globally across editor revisions.
        self.cache.insert(key, response.clone());
        Ok(response)
    }

    fn dispatch_semantic(&mut self, index: usize, mut payload: Value) -> Result<Value> {
        let id = self.plugins[index].id.clone();
        // A generation identifies the exact opaque index supplied to this
        // plugin. Equal indexes preserve cached results across source revisions
        // without cloning or hashing a megabyte-sized index for every hook.
        let key = request_key(&(
            "semantic",
            &id,
            self.plugins[index].index_generation,
            &payload,
        ))?;
        if let Some(response) = self.cache.get(&key) {
            self.hits += 1;
            return Ok(response.clone());
        }
        if let Some(project_index) = self.plugins[index].index.as_ref() {
            payload["project-index"] = project_index.clone();
        }
        self.dispatch_with_key(&id, payload, key)
    }

    pub fn execute_project(&mut self, kind: &str, payload: Value) -> Result<Value> {
        let capability = match kind {
            "build-project-index" => "project-index",
            "additional-dependencies" => "additional-dependencies",
            _ => bail!("not a project hook: {kind}"),
        };
        let mut responses = Vec::new();
        for index in 0..self.plugins.len() {
            let plugin = &self.plugins[index];
            if plugin.manifest["capabilities"][capability] != true {
                continue;
            }
            let mut request = payload.clone();
            request["kind"] = json!(kind);
            if kind == "build-project-index" {
                request["context"]["config"] = plugin.config.clone();
                {
                    let modules: BTreeSet<_> = plugin.manifest["claims"]["settings"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|claim| {
                            claim["config-key"]
                                .as_str()
                                .and_then(|key| plugin.config[key].as_str())
                                .or_else(|| claim["module"].as_str())
                        })
                        .collect();
                    if let Some(settings) =
                        request.get_mut("settings").and_then(Value::as_array_mut)
                    {
                        settings.retain(|setting| {
                            setting["module"]
                                .as_str()
                                .is_some_and(|module| modules.contains(module))
                        });
                        let severity = if plugin
                            .config
                            .get("strict-settings")
                            .or_else(|| plugin.config.get("strict_settings"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                        {
                            "error"
                        } else {
                            "warning"
                        };
                        for setting in settings {
                            for diagnostic in
                                setting["diagnostics"].as_array_mut().into_iter().flatten()
                            {
                                diagnostic["severity"] = json!(severity);
                            }
                        }
                    }
                }
            } else {
                request["config"] = plugin.config.clone();
            }
            let id = plugin.id.clone();
            let settings_diagnostics: Vec<_> = request["settings"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|setting| {
                    setting["diagnostics"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned()
                })
                .collect();
            let mut response = self.dispatch(&id, request)?;
            if response["kind"] == "project-index" && !settings_diagnostics.is_empty() {
                // Native project-index diagnostics belong to the successful
                // plugin response, with that plugin's own strictness policy.
                let mut diagnostics = settings_diagnostics;
                diagnostics.extend(
                    response["diagnostics"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
                response["diagnostics"] = json!(diagnostics);
            }
            if kind == "build-project-index" {
                let next_index = response.get("plugin-index").cloned();
                if self.plugins[index].index != next_index {
                    self.plugins[index].index_generation = self.plugins[index]
                        .index_generation
                        .checked_add(1)
                        .context("plugin index generation exhausted")?;
                    self.plugins[index].index = next_index;
                }
            }
            if self.plugins.len() > 1 && kind == "build-project-index" {
                let namespace = id
                    .bytes()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>();
                let names: BTreeMap<_, _> = response["virtual-types"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|definition| {
                        let name = definition["name"].as_str()?;
                        Some((name.to_owned(), format!("_ty_plugin_{namespace}.{name}")))
                    })
                    .collect();
                self.plugins[index].virtual_names = names;
                self.plugins[index].inverse_virtual_names = self.plugins[index]
                    .virtual_names
                    .iter()
                    .map(|(original, generated)| (generated.clone(), original.clone()))
                    .collect();
                rewrite_protocol_names(&mut response, &self.plugins[index].virtual_names);
                if let Some(definitions) = response
                    .get_mut("virtual-types")
                    .and_then(Value::as_array_mut)
                {
                    for definition in definitions {
                        if let Some(name) = definition["name"]
                            .as_str()
                            .and_then(|name| self.plugins[index].virtual_names.get(name))
                        {
                            definition["name"] = json!(name);
                        }
                    }
                }
            }
            responses.push(response);
        }
        if responses.len() == 1 {
            return Ok(responses.pop().unwrap());
        }
        if responses.is_empty() {
            return Ok(json!({"kind":"no-change"}));
        }
        let mut merged = json!({"kind":if kind == "build-project-index" {"project-index"} else {"dependencies"}});
        for key in [
            "contributions",
            "virtual-types",
            "dependencies",
            "diagnostics",
        ] {
            let values: Vec<_> = responses
                .iter()
                .flat_map(|response| response[key].as_array().into_iter().flatten().cloned())
                .collect();
            if !values.is_empty() {
                merged[key] = json!(values);
            }
        }
        Ok(merged)
    }

    pub fn execute(&mut self, source: &Source, kind: &str, payload: Value) -> Result<Value> {
        let mut responses = Vec::new();
        for index in 0..self.plugins.len() {
            let plugin = &self.plugins[index];
            if plugin.virtual_names.is_empty()
                && !self.allowed(source, kind, &payload, &plugin.manifest)
            {
                continue;
            }
            let mut request = payload.clone();
            rewrite_protocol_names(&mut request, &plugin.inverse_virtual_names);
            if !plugin.virtual_names.is_empty()
                && !self.allowed(source, kind, &request, &plugin.manifest)
            {
                continue;
            }
            let object = request
                .as_object_mut()
                .context("hook payload must be object")?;
            object.insert("kind".into(), json!(kind));
            object.insert("context".into(), json!({"module":source.protocol_module.as_ref().unwrap_or(&source.module),"file-path":source.path,
                "python-version":self.python_version,"platform":self.platform,"config":plugin.config,"speculative":false}));
            if source.protocol_module.is_some() {
                rewrite_notebook_request(&mut request, source, |name| {
                    self.protocol_name(source, name)
                });
            }
            let mut response = self.dispatch_semantic(index, request)?;
            rewrite_protocol_names(&mut response, &self.plugins[index].virtual_names);
            if source.protocol_module.is_some() {
                rewrite_notebook_request(&mut response, source, |name| {
                    self.internal_protocol_name(source, name)
                });
            }
            match kind {
                "analyze-class" => responses.push(response),
                "resolve-instance-member" | "resolve-class-member" => {
                    if response["kind"] == "member-patch"
                        && response["name"] == payload["member-name"]
                    {
                        return Ok(response);
                    }
                }
                // Calls and mutations choose a claiming plugin before running
                // it. A no-change response must not fall through to another.
                _ => return Ok(response),
            }
        }
        if kind != "analyze-class" || responses.is_empty() {
            return Ok(json!({"kind":"no-change"}));
        }
        if responses.len() == 1 {
            return Ok(responses.pop().unwrap());
        }
        let mut merged = json!({"kind":"class-patch"});
        for key in ["fields", "class-members", "instance-members"] {
            let mut names = BTreeSet::new();
            let members: Vec<_> = responses
                .iter()
                .flat_map(|response| response[key].as_array().into_iter().flatten())
                .filter(|member| names.insert(member["name"].clone().to_string()))
                .cloned()
                .collect();
            if !members.is_empty() {
                merged[key] = json!(members);
            }
        }
        if let Some(constructor) = responses
            .iter()
            .find_map(|response| response.get("constructor"))
        {
            merged["constructor"] = constructor.clone();
        }
        let diagnostics: Vec<_> = responses
            .iter()
            .flat_map(|response| {
                response["diagnostics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .cloned()
            })
            .collect();
        if !diagnostics.is_empty() {
            merged["diagnostics"] = json!(diagnostics);
        }
        Ok(merged)
    }
}

fn hook_failure_response(id: &str, kind: &str, error: HostError) -> Value {
    let (message, hint) = match error {
        HostError::Runtime { source, .. } => (source.to_string(), source.hint()),
        error => (
            error.to_string(),
            "Fix the plugin manifest or remove the plugin.",
        ),
    };
    hook_failure_message(id, kind, &message, hint)
}

fn hook_failure_message(id: &str, kind: &str, message: &str, hint: &str) -> Value {
    if matches!(
        kind,
        "adjust-call-signature" | "adjust-call-return" | "adjust-call-state" | "validate-mutation"
    ) {
        // infer/builder.rs reports these failures at the original expression;
        // returning no state patch also follows object_state.rs's invalidation
        // path, discarding facts for receivers, aliases and other objects.
        json!({"kind":"no-change","diagnostics":[{
            "id":message,
            "message":format!("Plugin `{id}` failed while handling a semantic hook"),
            "severity":"warning",
            "runtime-error":{"plugin-id":id,"message":message,"hint":hint}
        }]})
    } else {
        // static_literal.rs's class/member/project-index hooks log failures and
        // abstain without adding a source diagnostic or stopping other plugins.
        eprintln!("warning: Plugin `{id}` {kind} hook failed: {message}. {hint}");
        json!({"kind":"no-change"})
    }
}

fn merge_config(config: &mut Value, overrides: &Value) {
    if let (Some(defaults), Some(overrides)) = (config.as_object_mut(), overrides.as_object()) {
        defaults.extend(overrides.clone());
    } else if !overrides.is_null() {
        *config = overrides.clone();
    }
}

/// Notebook namespace translation is restricted to protocol type/identity
/// nodes. Opaque indexes, metadata and literal values remain byte-for-byte data.
fn rewrite_notebook_request(
    value: &mut Value,
    source: &Source,
    rename: impl Fn(&str) -> String + Copy,
) {
    match value {
        Value::Object(object) => {
            if let Some(expression) = object.get("expression").and_then(Value::as_str)
                && object.get("mode").is_some_and(Value::is_string)
            {
                object.insert(
                    "expression".into(),
                    json!(crate::project::rewrite_names(expression, rename)),
                );
            }
            for key in ["qualified-name", "nominal-class", "identity"] {
                if let Some(name) = object.get(key).and_then(Value::as_str) {
                    object.insert(key.into(), json!(rename(name)));
                }
            }
            if object.get("module").and_then(Value::as_str) == Some(source.module.as_str()) {
                object.insert(
                    "module".into(),
                    json!(source.protocol_module.as_ref().unwrap_or(&source.module)),
                );
            }
            for (key, child) in object.iter_mut() {
                if !matches!(
                    key.as_str(),
                    "config"
                        | "plugin-index"
                        | "plugin-metadata"
                        | "result-metadata"
                        | "metadata"
                        | "value"
                ) {
                    rewrite_notebook_request(child, source, rename);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_notebook_request(value, source, rename);
            }
        }
        _ => {}
    }
}

/// Virtual declarations are private to their owning plugin. Rewrite protocol
/// type nodes at the boundary; opaque plugin indexes/config/metadata and quoted
/// Literal values must never be inspected or changed.
fn rewrite_protocol_names(value: &mut Value, names: &BTreeMap<String, String>) {
    if names.is_empty() {
        return;
    }
    match value {
        Value::Object(object) => {
            if object.get("expression").is_some_and(Value::is_string)
                && object.get("mode").is_some_and(Value::is_string)
            {
                let expression = object["expression"].as_str().unwrap();
                object.insert(
                    "expression".into(),
                    json!(crate::project::rewrite_names(expression, |name| names
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| name.to_owned()))),
                );
                if let Some(imports) = object.get_mut("imports").and_then(Value::as_array_mut) {
                    for import in imports {
                        if let (Some(module), Some(name)) =
                            (import["module"].as_str(), import["name"].as_str())
                            && let Some(renamed) = names.get(&format!("{module}.{name}"))
                            && let Some((module, name)) = renamed.rsplit_once('.')
                        {
                            import["module"] = json!(module);
                            import["name"] = json!(name);
                        }
                    }
                }
            }
            for key in ["qualified-name", "nominal-class", "identity"] {
                if let Some(original) = object.get(key).and_then(Value::as_str)
                    && let Some(renamed) = names.get(original)
                {
                    object.insert(key.into(), json!(renamed));
                }
            }
            for (key, child) in object.iter_mut() {
                if !matches!(
                    key.as_str(),
                    "config"
                        | "plugin-index"
                        | "plugin-metadata"
                        | "result-metadata"
                        | "metadata"
                        | "source"
                        | "value"
                ) {
                    rewrite_protocol_names(child, names);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_protocol_names(value, names);
            }
        }
        _ => {}
    }
}

fn generic_base(
    project: &Project,
    owner: &str,
    seen: &mut BTreeSet<String>,
) -> Option<(String, Vec<String>)> {
    if !seen.insert(owner.to_owned()) {
        return None;
    }
    let class = project.class_summary(owner)?;
    for base in class["bases"].as_array().into_iter().flatten() {
        let expression = base["expression"].as_str()?;
        let (nominal, args) = generic_parts(expression);
        if !args.is_empty()
            && !matches!(
                nominal,
                "typing.Generic" | "typing.Protocol" | "Generic" | "Protocol"
            )
        {
            let module = class["source"]["module"]
                .as_str()
                .unwrap_or_else(|| owner.rsplit_once('.').map_or("", |(module, _)| module));
            return Some((
                nominal.to_string(),
                args.iter()
                    .map(|arg| {
                        if let Ok(parsed) = ruff_python_parser::parse_expression(arg)
                            && let ruff_python_ast::Expr::StringLiteral(literal) =
                                parsed.syntax().body.as_ref()
                        {
                            return project.canonical_name(module, literal.value.to_str());
                        }
                        (*arg).to_owned()
                    })
                    .collect(),
            ));
        }
        if let Some(found) = generic_base(project, nominal, seen) {
            return Some(found);
        }
    }
    None
}

/// Split only outer generic arguments, preserving nested tuples, unions, and
/// quoted Literal values. A display expression that is not nominal stays whole.
pub fn generic_parts(expression: &str) -> (&str, Vec<&str>) {
    let expression = expression.trim();
    let Some(start) = expression.find('[') else {
        return (expression, Vec::new());
    };
    if !expression.ends_with(']') {
        return (expression, Vec::new());
    }
    let owner = expression[..start].trim();
    if !owner
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    {
        return (expression, Vec::new());
    }
    let body = &expression[start + 1..expression.len() - 1];
    let mut args = Vec::new();
    let mut depth = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut from = 0;
    for (index, character) in body.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(body[from..index].trim());
                from = index + 1;
            }
            _ => {}
        }
    }
    if from < body.len() {
        args.push(body[from..].trim());
    }
    (owner, args)
}

pub fn call_request(
    runtime: &mut Runtime,
    source: &Source,
    kind: &str,
    call: &CallSite,
    oracle: Option<&Oracle>,
    receiver: Option<&str>,
) -> Result<Value> {
    let mut callee = call.callee.clone();
    let receiver = receiver
        .or(call.flow_receiver_type.as_deref())
        .or(call.receiver_type.as_deref())
        .map(str::to_owned)
        .or_else(|| {
            oracle
                .and_then(|oracle| oracle.get(&call.receiver_key()))
                .map(|expression| runtime.canonical_name(source, expression))
        });
    let is_constructor = runtime
        .project
        .as_ref()
        .is_some_and(|project| project.class_summary(&callee).is_some());
    let mut summary = receiver
        .as_deref()
        .filter(|_| !is_constructor)
        .filter(|owner| runtime.nominal_receiver(source, owner))
        .map(|owner| runtime.qualified_receiver_summary(source, owner));
    let oracle_receiver = oracle.and_then(|oracle| {
        oracle.get(&call.receiver_key()).or_else(|| {
            call.receiver_span
                .and_then(|span| oracle.get(&crate::model::expression_key(span)))
        })
    });
    if oracle_receiver
        .is_some_and(|expression| expression.starts_with("Self@") || expression == "Self")
        && let Some(summary) = &mut summary
    {
        let bound = summary["type-expr"].clone();
        summary["type-expr"] = json!({"expression":"Self","mode":"annotation","snapshot":{"kind":"self-type","bound":{"kind":"expression","expression":bound["expression"],"mode":"annotation"}}});
    }
    if let Some(summary) = &summary
        && runtime.project.is_some()
    {
        let method = callee.rsplit('.').next().unwrap_or(&callee);
        // The summary may intentionally describe a generic base. Routing
        // still needs the actual receiver class so overridden methods are
        // never mistaken for an inherited base implementation.
        let actual = summary["type-expr"]["snapshot"]["bound"]["expression"]
            .as_str()
            .or_else(|| summary["type-expr"]["expression"].as_str())
            .unwrap_or("");
        let (outer, arguments) = generic_parts(actual);
        let owner = if matches!(outer, "type" | "builtins.type") {
            arguments.first().copied().unwrap_or(outer)
        } else {
            outer
        };
        if !owner.is_empty() {
            callee = runtime.method_callee(owner, method);
        }
    }
    // Claims inspect only the callee and nominal receiver. Avoid constructing
    // argument/return snapshots and source locations for calls no plugin can
    // observe. Use the same namespace translation and claim implementation as
    // execute(), including notebooks and inherited state-hook restrictions.
    let mut routing = json!({"callee":{"expression":callee,"mode":"expression"}});
    if let Some(summary) = &summary {
        routing["receiver"] = json!({"nominal-class":summary["nominal-class"]});
    }
    let claimed = runtime.plugins.iter().any(|plugin| {
        if plugin.inverse_virtual_names.is_empty() {
            runtime.allowed(source, kind, &routing, &plugin.manifest)
        } else {
            let mut translated = routing.clone();
            rewrite_protocol_names(&mut translated, &plugin.inverse_virtual_names);
            runtime.allowed(source, kind, &translated, &plugin.manifest)
        }
    });
    if !claimed {
        return Ok(json!({"kind":"no-change"}));
    }
    let arguments: Vec<_> = call
        .arguments
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            let mut value =
                json!({"kind":arg.kind,"value":arg.literal,"source":source.location(arg.span)});
            if let Some(name) = &arg.name {
                value["name"] = json!(name);
            }
            if let Some(expression) = oracle.and_then(|o| o.get(&call.argument_key(index))) {
                value["type-expr"] = runtime.type_expr(source, expression);
            }
            if let Some(inferred) = &arg.inferred_type {
                if value.get("type-expr").is_none() {
                    value["type-expr"] = inferred.clone();
                } else if let Some(snapshot) = inferred.get("snapshot") {
                    value["type-expr"]["snapshot"] = snapshot.clone();
                }
            }
            value
        })
        .collect();
    let mut value =
        json!({"callee":{"expression":callee,"mode":"expression"},"arguments":arguments});
    if let Some(expression) = oracle.and_then(|o| o.get(&call.return_key())) {
        value["default-return-type"] = runtime.type_expr(source, expression);
    }
    if let Some(summary) = summary {
        value["receiver"] = summary;
    }
    runtime.execute(source, kind, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concrete_request_decoding_preserves_protocol_values_and_errors() {
        fn same_decode(payload: &Value) {
            let original = serde_json::from_value::<PluginRequest>(payload.clone())
                .map_err(|error| error.to_string());
            let direct = decode_request(payload.clone()).map_err(|error| error.to_string());
            assert_eq!(direct, original, "request: {payload}");
        }

        fn paths(value: &Value, path: String, output: &mut Vec<String>) {
            output.push(path.clone());
            match value {
                Value::Object(fields) => {
                    for (name, value) in fields {
                        let name = name.replace('~', "~0").replace('/', "~1");
                        paths(value, format!("{path}/{name}"), output);
                    }
                }
                Value::Array(values) => {
                    for (index, value) in values.iter().enumerate() {
                        paths(value, format!("{path}/{index}"), output);
                    }
                }
                _ => {}
            }
        }

        let context = json!({
            "module":"models","file-path":"models.py","python-version":"3.13",
            "platform":"darwin","config":{"enabled":true,"values":[null,1,-2,1.5]},
            "speculative":false
        });
        let type_expr = json!({
            "expression":"Alias","mode":"annotation",
            "imports":[{"module":"builtins","name":"str","alias":"Alias"}],
            "snapshot":{"kind":"nominal","qualified-name":"builtins.str","arguments":[]}
        });
        let class = json!({
            "qualified-name":"models.Row","bases":[type_expr],
            "fields":[{"name":"value","annotation":type_expr,"has-default":false}],
            "methods":[],"decorators":[],"nested-classes":[],"class-constants":[],
            "metaclass":type_expr,"source":{}
        });
        let argument = json!({
            "name":"value","kind":"keyword","type-expr":type_expr,
            "value":{"kind":"str","value":"text"},"source":{}
        });
        let index = json!({"nested":{"a/b~c":[null,false,0,1.25,"value"]}});
        let mut fixtures = vec![
            json!({"kind":"manifest"}),
            json!({"kind":"build-project-index","context":{
                "root":"/project","python-version":"3.13","platform":"darwin","config":index
            },"classes":[class],"settings":[],"assignments":[],"functions":[],
                "previous-index-fingerprint":"previous"}),
            json!({"kind":"analyze-class","context":context,"class":class,"project-index":index}),
            json!({"kind":"additional-dependencies","config":index}),
            json!({"kind":"validate-mutation","context":context,"operation":"attribute-set",
                "receiver":type_expr,"key":argument,"value":argument,"source":{},"project-index":index}),
        ];
        for kind in ["resolve-class-member", "resolve-instance-member"] {
            fixtures.push(json!({"kind":kind,"context":context,"owner":type_expr,"member-name":"value",
                "existing-member":{"name":"value","access":{"kind":"value","type-expr":type_expr},"is-read-only":false},
                "project-index":index}));
        }
        for kind in [
            "adjust-call-signature",
            "adjust-call-return",
            "adjust-call-state",
        ] {
            fixtures.push(json!({"kind":kind,"context":context,"callee":type_expr,
                "receiver":{"type-expr":type_expr,"nominal-class":"models.Row","generic-arguments":[type_expr],"plugin-metadata":index},
                "arguments":[argument],"existing-signature":{"parameters":[{
                    "name":"value","kind":"positional-or-keyword","type-expr":type_expr,"required":true
                }],"return-type":type_expr},"default-return-type":type_expr,"project-index":index}));
        }
        let replacements = [
            Value::Null,
            json!(true),
            json!(0),
            json!(1),
            json!(2),
            json!(-1),
            json!(u64::MAX),
            json!(1.25),
            json!("unknown"),
            json!([]),
            json!({}),
            json!({"unknown": null}),
        ];
        for fixture in fixtures {
            // This assertion keeps the test meaningful if protocol schemas
            // change: every base fixture must continue to be a valid request.
            let original: PluginRequest = serde_json::from_value(fixture.clone()).unwrap();
            same_decode(&fixture);
            // Omitted default fields must retain the enum's exact behavior.
            same_decode(&serde_json::to_value(original).unwrap());
            for index in [
                None,
                Some(Value::Null),
                Some(json!(true)),
                Some(json!(42)),
                Some(json!("opaque")),
                Some(json!([1, null, "value"])),
                Some(index.clone()),
            ] {
                let mut indexed = fixture.clone();
                if let Some(index) = index {
                    indexed["project-index"] = index;
                } else {
                    indexed.as_object_mut().unwrap().remove("project-index");
                }
                same_decode(&indexed);
                for path in [
                    "/class/bases/0/snapshot/kind",
                    "/owner/snapshot/kind",
                    "/callee/snapshot/kind",
                    "/receiver/snapshot/kind",
                ] {
                    let mut fallback = indexed.clone();
                    if let Some(kind) = fallback.pointer_mut(path) {
                        // Numeric nested discriminants require the old enum
                        // decoder. Its successful result must retain the index.
                        *kind = json!(1);
                        same_decode(&fallback);
                    }
                }
                if indexed["context"].is_object() {
                    indexed["context"]["platform"] = Value::Null;
                    same_decode(&indexed);
                }
            }
            let mut locations = Vec::new();
            paths(&fixture, String::new(), &mut locations);
            for path in locations {
                for replacement in &replacements {
                    let mut changed = fixture.clone();
                    *changed.pointer_mut(&path).unwrap() = replacement.clone();
                    same_decode(&changed);
                }
                if let Some(fields) = fixture.pointer(&path).and_then(Value::as_object) {
                    for name in fields.keys() {
                        let mut changed = fixture.clone();
                        changed
                            .pointer_mut(&path)
                            .unwrap()
                            .as_object_mut()
                            .unwrap()
                            .remove(name);
                        same_decode(&changed);
                    }
                    let mut changed = fixture.clone();
                    changed.pointer_mut(&path).unwrap()["unknown-extra-field"] = index.clone();
                    same_decode(&changed);
                }
            }
        }
    }

    #[test]
    fn virtual_record_wire_preserves_shape_and_named_tuple_identity() -> Result<()> {
        let mut runtime = Runtime::from_plugins(&[])?;
        runtime.selected_virtual_scope = Some("accounts".into());
        runtime.set_virtual_shapes(BTreeMap::from([("virtual.Row".into(), json!({"kind":"typed-dict","total":false,"fields":[
            {"name":"title","type-expr":{"expression":"Alias","imports":[{"module":"builtins","name":"str","alias":"Alias"}]},"required":true,"read-only":true},
            {"name":"pages","type-expr":{"expression":"int | None"},"required":true}
        ]})), (
            "virtual.NamedRow".into(),
            json!({"kind":"named-tuple","fields":[]}),
        )]));
        let record = runtime.qualified_type_expr("query.Rows[virtual.Row]");
        assert_eq!(
            record["expression"],
            "query.Rows[<TypedDict with items 'pages', 'title'>]"
        );
        let snapshot = &record["snapshot"]["arguments"][0];
        assert_eq!(snapshot["kind"], "typed-dict");
        assert_eq!(snapshot["closed"], true);
        assert_eq!(
            snapshot["fields"][0]["type-snapshot"]["qualified-name"],
            "builtins.str"
        );
        assert_eq!(snapshot["fields"][0]["required"], false);
        assert_eq!(snapshot["fields"][0]["read-only"], true);
        let named = runtime.qualified_type_expr("query.Rows[virtual.NamedRow]");
        assert_eq!(named["expression"], "query.Rows[accounts.NamedRow]");
        assert_eq!(
            named["snapshot"]["arguments"][0],
            json!({"kind":"plugin-class","identity":"virtual.NamedRow"})
        );
        runtime.selected_virtual_scope = Some("other".into());
        assert_eq!(
            runtime.qualified_type_expr("query.Rows[virtual.NamedRow]")["expression"],
            "query.Rows[other.NamedRow]"
        );
        runtime.set_virtual_shapes(BTreeMap::new());
        assert_eq!(
            runtime.qualified_type_expr("query.Rows[virtual.Row]")["snapshot"]["arguments"][0]["kind"],
            "nominal"
        );
        Ok(())
    }

    #[test]
    fn structural_aliases_are_scoped_and_replaced_on_rerender() -> Result<()> {
        let mut runtime = Runtime::from_plugins(&[])?;
        let source = |module: &str| Source::new("".into(), format!("{module}.py"), module.into());
        for (module, field) in [("left", "a"), ("right", "b")] {
            runtime.register_protocol_types(module,"_generated_",BTreeMap::from([("_record".into(),json!({"expression":"Record","mode":"annotation","snapshot":{"kind":"typed-dict","fields":[{"name":field,"type-snapshot":{"kind":"nominal","qualified-name":"builtins.int"}}]}}))]));
        }
        assert_eq!(
            runtime.type_expr(&source("left"), "list[_record]")["snapshot"]["arguments"][0]["fields"]
                [0]["name"],
            "a"
        );
        assert_eq!(
            runtime.type_expr(&source("right"), "_record")["snapshot"]["fields"][0]["name"],
            "b"
        );
        runtime.register_protocol_types("left", "_generated_", BTreeMap::new());
        assert_eq!(
            runtime.type_expr(&source("left"), "_record")["snapshot"]["kind"],
            "expression"
        );
        assert_eq!(
            runtime.type_expr(&source("right"), "_record")["snapshot"]["kind"],
            "typed-dict"
        );
        Ok(())
    }

    #[test]
    fn type_snapshot_cache_tracks_project_revisions_and_projected_receivers() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-type-cache-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("models.py");
        let mut runtime = Runtime::from_plugins(&[])?;
        let source = Source::new(String::new(), "models.py".into(), "models".into());
        for annotation in ["int", "str"] {
            std::fs::write(
                &path,
                format!(
                    "from typing import TypedDict\nclass Row(TypedDict):\n    item: {annotation}\n"
                ),
            )?;
            let project = Project::load(
                &directory,
                std::slice::from_ref(&path),
                &runtime.manifest,
                json!({}),
            )?;
            runtime.attach_project(Arc::new(project), json!({}));
            assert!(runtime.type_cache.borrow().entries.is_empty());
            let value = runtime.type_expr(&source, "Row");
            assert_eq!(value["snapshot"]["kind"], "typed-dict");
            assert_eq!(
                value["snapshot"]["fields"][0]["type-snapshot"]["qualified-name"],
                format!("builtins.{annotation}")
            );
            let cache_clock = runtime.type_cache.borrow().clock;
            assert_eq!(runtime.type_expr(&source, "Row"), value);
            assert_eq!(runtime.type_cache.borrow().clock, cache_clock + 1);
        }
        for annotation in ["int", "str"] {
            runtime.set_projected_receiver("models.projected".into(), annotation.into());
            assert_eq!(
                runtime.type_expr(&source, "projected")["snapshot"]["qualified-name"],
                format!("builtins.{annotation}")
            );
            // An unqualified projected key is resolved during canonicalization
            // after the complete source-expression cache lookup.
            runtime.set_projected_receiver("bare".into(), annotation.into());
            let value = runtime.type_expr(&source, "bare");
            assert_eq!(value["expression"], "bare");
            assert_eq!(
                value["snapshot"]["qualified-name"],
                format!("builtins.{annotation}")
            );
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn projection_generation_retains_equal_cross_file_inputs() -> Result<()> {
        let mut runtime = Runtime::from_plugins(&[])?;
        let initial = runtime.projection_generation();
        let value = runtime.qualified_type_expr("str");
        runtime.register_protocol_types("unused", "static_", BTreeMap::new());
        assert_eq!(runtime.projection_generation(), initial);
        assert_eq!(runtime.type_cache.borrow().entries.len(), 1);
        assert_eq!(runtime.qualified_type_expr("str"), value);

        let record = BTreeMap::from([("Alias".into(), runtime.qualified_type_expr("str"))]);
        runtime.register_protocol_types("models", "static_", record.clone());
        runtime.set_project_patch("models.Model".into(), json!({"kind":"no-change"}));
        runtime.set_projected_receiver("models.receiver".into(), "str".into());
        let established = runtime.projection_generation();
        assert!(established > initial);

        runtime.register_protocol_types("models", "static_", record);
        runtime.set_project_patch("models.Model".into(), json!({"kind":"no-change"}));
        runtime.set_projected_receiver("models.receiver".into(), "str".into());
        assert_eq!(runtime.projection_generation(), established);

        runtime.register_protocol_types("models", "static_", BTreeMap::new());
        assert!(runtime.projection_generation() > established);
        let removed = runtime.projection_generation();
        runtime.register_protocol_types("models", "static_", BTreeMap::new());
        assert_eq!(runtime.projection_generation(), removed);
        Ok(())
    }

    #[test]
    fn cached_source_and_qualified_types_keep_distinct_display_contracts() -> Result<()> {
        let mut runtime = Runtime::from_plugins(&[])?;
        let source = Source::new(String::new(), "models.py".into(), "models".into());
        runtime.register_protocol_types(
            "models",
            "static_",
            BTreeMap::from([(
                "Alias".into(),
                json!({"expression":"StringDisplay","mode":"annotation","snapshot":{
                    "kind":"nominal","qualified-name":"builtins.str"
                }}),
            )]),
        );
        for _ in 0..2 {
            assert_eq!(
                runtime.type_expr(&source, "Alias")["expression"],
                "StringDisplay"
            );
            assert_eq!(runtime.qualified_type_expr("Alias")["expression"], "Alias");
            assert_eq!(
                runtime.qualified_type_expr_in_module("Alias", Some("models"))["expression"],
                "builtins.str"
            );
        }
        Ok(())
    }

    #[test]
    fn unclaimed_calls_skip_argument_and_return_type_snapshots() -> Result<()> {
        let mut runtime = Runtime::from_plugins(&[])?;
        let source = Source::new("ordinary(1)".into(), "models.py".into(), "models".into());
        let call = CallSite {
            span: crate::model::Span { start: 0, end: 11 },
            function: crate::model::Span { start: 0, end: 8 },
            callee: "ordinary".into(),
            arguments: vec![crate::model::Argument {
                span: crate::model::Span { start: 9, end: 10 },
                name: None,
                literal: json!({"kind":"int","value":1}),
                kind: "positional",
                inferred_type: None,
            }],
            receiver_span: None,
            receiver_type: None,
            flow_receiver_type: None,
        };
        let oracle = BTreeMap::from([
            (call.argument_key(0), "Literal[1]".into()),
            (call.return_key(), "int".into()),
        ]);
        assert_eq!(
            call_request(
                &mut runtime,
                &source,
                "adjust-call-return",
                &call,
                Some(&oracle),
                None,
            )?,
            json!({"kind":"no-change"})
        );
        assert!(runtime.type_cache.borrow().entries.is_empty());
        assert_eq!(runtime.executions, 0);
        Ok(())
    }

    #[test]
    fn host_hook_failures_warn_only_for_expression_hooks() {
        for kind in [
            "adjust-call-signature",
            "adjust-call-return",
            "adjust-call-state",
            "validate-mutation",
            "analyze-class",
            "resolve-instance-member",
            "resolve-class-member",
            "build-project-index",
        ] {
            let response = hook_failure_response(
                "failure",
                kind,
                HostError::Runtime {
                    plugin_id: "failure".into(),
                    source: ty_plugin_host::RuntimeError::Timeout,
                },
            );
            assert_eq!(response["kind"], "no-change");
            let warning = kind.starts_with("adjust-call-") || kind == "validate-mutation";
            assert_eq!(response.get("diagnostics").is_some(), warning, "{kind}");
            if warning {
                assert_eq!(response["diagnostics"][0]["id"], "plugin timed out");
                assert_eq!(
                    response["diagnostics"][0]["runtime-error"]["hint"],
                    ty_plugin_host::RuntimeError::Timeout.hint()
                );
            }
        }
    }

    #[test]
    fn malformed_monty_responses_are_cached_recoverable_warnings() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-failure-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let manifest = directory.join("plugin.json");
        let plugin = directory.join("plugin.py");
        std::fs::write(
            &manifest,
            serde_json::to_vec(&json!({
                "id":"failure","name":"Failure","version":"0.0.0","protocol-version":{"major":0,"minor":7},
                "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},
                "capabilities":{"call-return":true},"claims":{"functions":[{"qualified-name":"demo.load"}]}
            }))?,
        )?;
        let source = Source::new("load()".into(), "demo.py".into(), "demo".into());
        let request =
            json!({"callee":{"expression":"demo.load","mode":"expression"},"arguments":[]});
        std::fs::write(
            &plugin,
            "@on_call_return\ndef returns(request):\n    return {'kind':'invalid-response-kind'}\n",
        )?;
        let mut runtime = Runtime::new(&manifest, &plugin)?;
        let first = runtime.execute(&source, "adjust-call-return", request.clone())?;
        assert_eq!(first["kind"], "no-change");
        assert!(
            first["diagnostics"][0]["id"]
                .as_str()
                .unwrap()
                .starts_with("plugin returned an invalid response:")
        );
        assert_eq!(runtime.executions, 1);
        assert_eq!(
            runtime.execute(&source, "adjust-call-return", request.clone())?,
            first
        );
        assert_eq!(runtime.executions, 1);
        assert_eq!(runtime.hits, 1);
        // Plugin/config reload creates a new Runtime, so cached failures cannot
        // survive an edited artifact. Ordinary SDK exceptions are protocol
        // Error responses and retain the fork's silent abstention behavior.
        std::fs::write(
            &plugin,
            "@on_call_return\ndef returns(request):\n    raise ValueError('explicit failure')\n",
        )?;
        let mut runtime = Runtime::new(&manifest, &plugin)?;
        assert_eq!(
            runtime.execute(&source, "adjust-call-return", request.clone())?,
            json!({"kind":"no-change"})
        );
        std::fs::write(
            &plugin,
            "@on_call_return\ndef returns(request):\n    return call_return_patch(type_annotation('int'))\n",
        )?;
        let mut runtime = Runtime::new(&manifest, &plugin)?;
        assert_eq!(
            runtime.execute(&source, "adjust-call-return", request)?["return-type"]["expression"],
            "int"
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn notebook_protocol_uses_local_names_without_sharing_internal_classes() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-notebooks-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let manifest = json!({"id":"notebook","name":"Notebook test","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true,"call-return":true},"claims":{"classes":[{"kind":"exact","qualified-name":"Resource"}],"functions":[{"qualified-name":"load"}]}});
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        std::fs::write(
            directory.join("plugin.py"),
            "@on_class_transform\ndef transform(request):\n    assert request['context']['module'] == ''\n    assert request['class']['qualified-name'] == 'Resource'\n    assert request['class']['source']['module'] == ''\n    annotation = 'int' if request['context']['file-path'].endswith('one.py') else 'str'\n    return class_patch(fields=[field('value', type_annotation(annotation)), field('parent', type_annotation('Resource'))])\n@on_call_return\ndef returns(request):\n    assert request['callee']['expression'] == 'load'\n    return call_return_patch(type_annotation('Resource'))\n",
        )?;
        let files = [directory.join("one.py"), directory.join("two.py")];
        for path in &files {
            std::fs::write(
                path,
                "class Resource: pass\ndef load() -> Resource: return Resource()\n",
            )?;
        }
        let mut project = Project::load(&directory, &files, &manifest, json!({}))?;
        for path in &files {
            project.register_notebook_source(path)?;
        }
        assert!(
            project.build_request()["classes"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let project = Arc::new(project);
        let mut runtime =
            Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py"))?;
        runtime.attach_project(project.clone(), json!({}));
        for (module, expected) in [("one", "int"), ("two", "str")] {
            let source = &project.modules[module].source;
            let response = runtime.execute(
                source,
                "analyze-class",
                json!({"class":project.class_summary(&format!("{module}.Resource")).unwrap()}),
            )?;
            assert_eq!(
                response["fields"][0]["instance-get-type"]["expression"],
                expected
            );
            assert_eq!(
                response["fields"][1]["instance-get-type"]["expression"],
                format!("{module}.Resource")
            );
            let mut types = crate::type_render::Types::with_project("_test_", module, &project);
            assert_eq!(
                types.render(&response["fields"][1]["instance-get-type"])?,
                "Resource"
            );
            assert!(!types.prelude().contains(&format!("import {module}")));
            let returns = runtime.execute(
                source,
                "adjust-call-return",
                json!({"callee":{"expression":format!("{module}.load"),"mode":"expression"}}),
            )?;
            assert_eq!(
                returns["return-type"]["expression"],
                format!("{module}.Resource")
            );
        }
        assert_eq!(runtime.executions, 4);
        let mut opaque = json!({"expression":"one.Resource | Literal['one.Resource']","mode":"annotation","plugin-index":{"qualified-name":"one.Resource"},"metadata":{"expression":"one.Resource","mode":"annotation"}});
        let source = &project.modules["one"].source;
        rewrite_notebook_request(&mut opaque, source, |name| {
            runtime.protocol_name(source, name)
        });
        assert_eq!(opaque["expression"], "Resource | Literal['one.Resource']");
        assert_eq!(opaque["plugin-index"]["qualified-name"], "one.Resource");
        assert_eq!(opaque["metadata"]["expression"], "one.Resource");
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn deep_acyclic_call_hooks_converge_beyond_twelve_rounds() -> Result<()> {
        let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-deep-call-inference-{}", std::process::id()));
        let root = directory.join("project");
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let manifest_path = directory.join("plugin.json");
        let plugin = directory.join("plugin.py");
        let manifest = json!({"id":"test.deep","name":"Deep calls","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},"claims":{"functions":[{"qualified-name":"app.seed"},{"qualified-name":"app.identity"}]}});
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
        std::fs::write(
            &plugin,
            "@on_call_return\ndef returns(request):\n    if request['callee']['expression']=='app.seed':\n        return call_return_patch(type_annotation('int'))\n    return call_return_patch(request['arguments'][0]['type-expr'])\n",
        )?;
        let depth = 20;
        std::fs::write(
            root.join("app.py"),
            format!(
                "def seed() -> object: return object()\ndef identity(value: object) -> object: return value\nvalue: int = {}seed(){}\n",
                "identity(".repeat(depth),
                ")".repeat(depth)
            ),
        )?;
        let options = BTreeMap::from([
            ("ty".into(), official),
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("checker-mode".into(), "push".into()),
            (
                "manifest".into(),
                manifest_path.to_string_lossy().into_owned(),
            ),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("python-version".into(), "3.13".into()),
        ]);
        let result = crate::project_check::Session::new(&root, &options)?.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert!(
            result["projection_rounds"].as_u64().unwrap() > 12,
            "fixture must exercise the old arbitrary bound: {result:#}"
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn failed_source_refresh_cannot_reuse_partially_updated_snapshot() -> Result<()> {
        let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-failed-refresh-{}", std::process::id()));
        let root = directory.join("project");
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        std::fs::write(
            root.join("ty.toml"),
            "[environment]\npython-version='3.13'\n",
        )?;
        std::fs::write(root.join("a.py"), "class Resource:\n    marker: int = 1\n")?;
        std::fs::write(
            root.join("b.py"),
            "from a import Resource\nvalue: int = Resource().marker\n",
        )?;
        let notebook = serde_json::to_string(
            &json!({"cells":[{"cell_type":"code","execution_count":null,"metadata":{},"outputs":[],"source":["value = 1\n"]}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}),
        )?;
        std::fs::write(root.join("z.ipynb"), &notebook)?;
        let mut session = crate::project_check::Session::new(
            &root,
            &BTreeMap::from([
                ("ty".into(), official),
                (
                    "out".into(),
                    directory.join("output").to_string_lossy().into_owned(),
                ),
                ("checker-mode".into(), "push".into()),
            ]),
        )?;
        assert_eq!(session.check(&BTreeMap::new())?["diagnostics"], json!([]));
        std::fs::write(
            root.join("a.py"),
            "class Resource:\n    marker: str = 'bad'\n",
        )?;
        std::fs::write(root.join("z.ipynb"), "{")?;
        assert!(session.check(&BTreeMap::new()).is_err());
        std::fs::write(root.join("z.ipynb"), notebook)?;
        let final_result = session.check(&BTreeMap::new())?;
        let diagnostics = final_result["diagnostics"].as_array().unwrap();
        assert_eq!(
            diagnostics.len(),
            1,
            "failed refresh returned stale results: {final_result:#}"
        );
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn changed_opaque_index_invalidates_unchanged_modules_with_multiple_plugins() -> Result<()> {
        let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory = std::env::temp_dir().join(format!(
            "ty-multi-index-invalidation-{}",
            std::process::id()
        ));
        let root = directory.join("project");
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        std::fs::write(
            root.join("app.py"),
            "def load() -> object: return object()\nvalue: int = load()\n",
        )?;
        std::fs::write(root.join("settings.py"), "TYPE = 'int'\n")?;
        let mut entries = Vec::new();
        for id in ["a", "z"] {
            let manifest_path = directory.join(format!("{id}.json"));
            let plugin = directory.join(format!("{id}.py"));
            let manifest = json!({"id":id,"name":id,"version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"project-index":true,"call-return":id=="a"},"claims":{"modules":[{"name":"settings"}],"functions":if id=="a" {json!([{"qualified-name":"app.load"}])} else {json!([])}}});
            std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
            let code = if id == "a" {
                "@on_project_index\ndef index(request):\n    annotation='int'\n    for assignment in request['assignments']:\n        if assignment['qualified-name']=='settings.TYPE':\n            annotation=assignment['assigned-value']['value']['value']\n    return project_index(plugin_index={'annotation':annotation})\n@on_call_return\ndef returns(request):\n    return call_return_patch(type_annotation(request['project-index']['annotation']))\n"
            } else {
                "@on_project_index\ndef index(request):\n    return project_index(plugin_index={'fixed':True})\n"
            };
            std::fs::write(&plugin, code)?;
            entries.push(format!(
                "{{id={id:?},runtime='monty',trusted=true,path={},manifest-path={}}}",
                serde_json::to_string(&plugin)?,
                serde_json::to_string(&manifest_path)?
            ));
        }
        std::fs::write(
            root.join("ty.toml"),
            format!(
                "[environment]\npython-version='3.13'\n[plugins]\nenabled=true\nauto-discover=false\nplugin=[{}]\n",
                entries.join(",")
            ),
        )?;
        let mut session = crate::project_check::Session::new(
            &root,
            &BTreeMap::from([
                ("ty".into(), official),
                (
                    "out".into(),
                    directory.join("output").to_string_lossy().into_owned(),
                ),
                ("checker-mode".into(), "push".into()),
            ]),
        )?;
        let first = session.check(&BTreeMap::new())?;
        assert_eq!(first["diagnostics"], json!([]), "{first:#}");
        std::fs::write(root.join("settings.py"), "TYPE = 'str'\n")?;
        let second = session.check(&BTreeMap::new())?;
        let diagnostics = second["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{second:#}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        assert!(
            diagnostics[0]["location"]["path"]
                .as_str()
                .unwrap()
                .ends_with("app.py")
        );
        std::fs::write(root.join("settings.py"), "TYPE = 'int'\n")?;
        assert_eq!(session.check(&BTreeMap::new())?["diagnostics"], json!([]));
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn settings_diagnostics_use_each_plugins_policy_and_successful_index() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-settings-policy-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let mut paths = Vec::new();
        for (id, strict, module) in [("strict", true, "one"), ("loose", false, "two")] {
            let manifest = json!({"id":id,"name":id,"version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"project-index":true,"settings-data":true},"claims":{"settings":[{"module":module}]}});
            let path = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
            std::fs::write(
                &artifact,
                "@on_project_index\ndef index(request):\n    return project_index(plugin_index={'settings':request['settings']})\n",
            )?;
            paths.push((path, artifact, json!({"strict_settings":strict})));
        }
        let mut runtime = Runtime::from_plugins(&paths)?;
        let request = json!({"context":{"root":"","python-version":"3.13","platform":"darwin"},"settings":[{"module":"one","diagnostics":[{"id":"one","message":"unsupported","severity":"warning"}]},{"module":"two","diagnostics":[{"id":"two","message":"unsupported","severity":"error"}]}]});
        let result = runtime.execute_project("build-project-index", request.clone())?;
        assert_eq!(result["diagnostics"].as_array().unwrap().len(), 2);
        for plugin in &runtime.plugins {
            let settings = &plugin.index.as_ref().unwrap()["settings"];
            assert_eq!(settings.as_array().unwrap().len(), 1);
            let expected = if plugin.id == "strict" {
                "error"
            } else {
                "warning"
            };
            assert_eq!(settings[0]["diagnostics"][0]["severity"], expected);
            assert!(
                result["diagnostics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(
                        |diagnostic| diagnostic["id"] == settings[0]["diagnostics"][0]["id"]
                            && diagnostic["severity"] == expected
                    )
            );
        }
        std::fs::write(
            &paths[0].1,
            "@on_project_index\ndef index(request):\n    return None\n",
        )?;
        let mut runtime = Runtime::from_plugins(&paths[..1])?;
        let result = runtime.execute_project("build-project-index", request)?;
        assert_eq!(result["kind"], "no-change");
        assert!(result.get("diagnostics").is_none());
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn request_cache_bounds_storage_and_retains_recent_results() {
        let mut cache = RequestCache::default();
        let first = request_key(&0).unwrap();
        let second = request_key(&1).unwrap();
        for index in 0..RequestCache::MAX_ENTRIES {
            cache.insert(request_key(&index).unwrap(), json!(index));
        }
        assert_eq!(cache.get(&first), Some(&json!(0)));
        cache.insert(
            request_key(&RequestCache::MAX_ENTRIES).unwrap(),
            json!("new"),
        );
        assert!(cache.get(&second).is_none());
        assert!(cache.get(&first).is_some());
        assert_eq!(cache.entries.len(), RequestCache::MAX_ENTRIES);
        assert_eq!(cache.entries.len(), cache.oldest.len());
        assert!(cache.bytes <= RequestCache::MAX_BYTES);
    }

    #[test]
    fn multiple_plugins_preserve_precedence_and_isolate_config_index_and_cache() {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-multiple-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let mut paths = Vec::new();
        for (id, annotation, abstain) in [("z-last", "str", false), ("a-first", "int", true)] {
            let manifest = json!({"id":id,"name":id,"version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"class-transform":true,"project-index":true,"instance-member":true,"call-return":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Item"}],"functions":[{"qualified-name":"demo.load"}],"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"demo.Item","attribute-name":"dynamic"}]}});
            let manifest_path = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            std::fs::write(&artifact, "@on_project_index\ndef index(request):\n    annotation = request['context']['config']['annotation']\n    if not request['context']['config']['abstain']:\n        annotation = request['context']['root'] or annotation\n    return project_index(plugin_index={'annotation':annotation}, virtual_types=[virtual_type('virtual.Record', virtual_class(members=[member('value',type_annotation(request['context']['config']['annotation']))]))])\n@on_class_transform\ndef transform(request):\n    return class_patch(fields=[field('shared',type_annotation(request['context']['config']['annotation'])),field(request['context']['config']['unique'],type_annotation('bool'))])\n@on_call_return\ndef result(request):\n    if request['context']['config']['abstain']: return None\n    return call_return_patch(type_annotation('str'))\n@on_instance_member\ndef member_hook(request):\n    if request['context']['config']['abstain']: return None\n    return member_response(member('dynamic',type_annotation(request['project-index']['annotation'])))\n").unwrap();
            paths.push((
                manifest_path,
                artifact,
                json!({"annotation":annotation,"abstain":abstain,"unique":id.replace('-',"_")}),
            ));
        }
        let mut runtime = Runtime::from_plugins(&paths).unwrap();
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        let request = json!({"context":{"root":"","python-version":"3.13","platform":"darwin"}});
        let index = runtime
            .execute_project("build-project-index", request.clone())
            .unwrap();
        let virtuals = index["virtual-types"].as_array().unwrap();
        assert_eq!(virtuals.len(), 2);
        assert_ne!(virtuals[0]["name"], virtuals[1]["name"]);
        assert_eq!(runtime.virtual_display_names().len(), 2);
        let class = runtime
            .execute(
                &source,
                "analyze-class",
                json!({"class":{"qualified-name":"demo.Item"}}),
            )
            .unwrap();
        assert_eq!(class["fields"][0]["instance-get-type"]["expression"], "int");
        assert_eq!(class["fields"].as_array().unwrap().len(), 3);
        let result = runtime
            .execute(
                &source,
                "adjust-call-return",
                json!({"callee":{"expression":"demo.load","mode":"expression"}}),
            )
            .unwrap();
        assert_eq!(
            result["kind"], "no-change",
            "first claiming plugin does not fall through"
        );
        let member_request = json!({"owner":typ("demo.Item"),"member-name":"dynamic"});
        let member = runtime
            .execute(&source, "resolve-instance-member", member_request.clone())
            .unwrap();
        assert_eq!(member["access"]["type-expr"]["expression"], "str");
        let executions = runtime.executions;
        assert_eq!(
            runtime
                .execute(&source, "resolve-instance-member", member_request.clone())
                .unwrap(),
            member
        );
        assert_eq!(runtime.executions, executions);
        assert!(runtime.hits >= 2);

        std::fs::write(directory.join("demo.py"), "# changed source snapshot\n").unwrap();
        let project = Project::load(
            &directory,
            &[directory.join("demo.py")],
            &runtime.manifest,
            json!({}),
        )
        .unwrap();
        runtime.attach_project(Arc::new(project), json!({}));
        runtime
            .execute_project("build-project-index", request.clone())
            .unwrap();
        assert_eq!(
            runtime
                .execute(&source, "resolve-instance-member", member_request.clone())
                .unwrap(),
            member
        );
        assert_eq!(
            runtime.executions, executions,
            "equal requests survive revisions"
        );

        let first_generation = runtime.plugins[0].index_generation;
        let mut changed = request.clone();
        changed["context"]["root"] = json!("bytes");
        runtime
            .execute_project("build-project-index", changed)
            .unwrap();
        assert_eq!(runtime.plugins[0].index_generation, first_generation);
        let changed_member = runtime
            .execute(&source, "resolve-instance-member", member_request.clone())
            .unwrap();
        assert_eq!(changed_member["access"]["type-expr"]["expression"], "bytes");
        assert_eq!(
            runtime.executions,
            executions + 3,
            "only changed index invalidates semantic results"
        );

        runtime.plugins[1].config["annotation"] = json!("bool");
        runtime
            .execute_project("build-project-index", request)
            .unwrap();
        let configured_member = runtime
            .execute(&source, "resolve-instance-member", member_request)
            .unwrap();
        assert_eq!(
            configured_member["access"]["type-expr"]["expression"],
            "bool"
        );
        assert_eq!(
            runtime.executions,
            executions + 5,
            "config is part of project and semantic keys"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn no_plugins_leave_ordinary_python_unchanged() {
        let mut runtime = Runtime::from_plugins(&[]).unwrap();
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        assert_eq!(
            runtime
                .execute(
                    &source,
                    "analyze-class",
                    json!({"class":{"qualified-name":"demo.Item"}})
                )
                .unwrap(),
            json!({"kind":"no-change"})
        );
        assert_eq!(runtime.executions, 0);
    }

    #[test]
    fn generic_arguments_preserve_nested_and_quoted_commas() {
        assert_eq!(
            generic_parts("QuerySet[Book, tuple[int, Literal['a,b']]]"),
            ("QuerySet", vec!["Book", "tuple[int, Literal['a,b']]"])
        );
        assert_eq!(
            generic_parts("tuple[int, str] | None"),
            ("tuple[int, str] | None", vec![])
        );
    }

    #[test]
    fn inherited_state_claim_requires_original_method_implementation() {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-routing-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let manifest = json!({
            "id":"native.routing", "name":"Native routing test", "version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},
            "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},
            "capabilities":{"call-state":true,"call-return":true},
            "claims":{
                "constructors":[{"kind":"subclass-of","base-qualified-name":"models.Base"}],
                "methods":[{"kind":"on-subclass-of","base-qualified-name":"models.Base","method-name":"save"}]
            }
        });
        let source = "class Base:\n    def save(self) -> None: pass\nclass Child(Base): pass\nclass Override(Base):\n    def save(self) -> None: pass\n";
        std::fs::write(directory.join("models.py"), source).unwrap();
        std::fs::write(directory.join("plugin.py"), "\n").unwrap();
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let project = Project::load(
            &directory,
            &[directory.join("models.py")],
            &manifest,
            json!({}),
        )
        .unwrap();
        let mut runtime =
            Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py")).unwrap();
        runtime.attach_project(Arc::new(project), json!({}));
        let source = Source::new(
            source.into(),
            directory.join("models.py").to_string_lossy().into_owned(),
            "models".into(),
        );
        let inherited = json!({"callee":{"expression":runtime.method_callee("models.Child","save")},"receiver":runtime.receiver_summary(&source,"Child")});
        let overridden = json!({"callee":{"expression":runtime.method_callee("models.Override","save")},"receiver":runtime.receiver_summary(&source,"Override")});
        assert!(runtime.allowed(&source, "adjust-call-state", &inherited, &runtime.manifest));
        assert!(!runtime.allowed(&source, "adjust-call-state", &overridden, &runtime.manifest));
        assert!(runtime.allowed(
            &source,
            "adjust-call-return",
            &overridden,
            &runtime.manifest
        ));
        let constructor = json!({"callee":{"expression":"models.Child"}});
        assert!(runtime.allowed(
            &source,
            "adjust-call-state",
            &constructor,
            &runtime.manifest
        ));
        assert!(!runtime.allowed(
            &source,
            "adjust-call-return",
            &constructor,
            &runtime.manifest
        ));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn exact_class_transform_claims_follow_ancestry_but_constructors_do_not() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-transform-ancestry-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let manifest = json!({"id":"test.ancestry","name":"Class ancestry","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},
            "capabilities":{"class-transform":true,"call-state":true},
            "claims":{"classes":[{"kind":"exact","qualified-name":"demo.Base"}],
                "constructors":[{"kind":"exact","qualified-name":"demo.Base"}]}});
        std::fs::write(
            directory.join("demo.py"),
            "class Base: pass\nclass Child(Base): pass\nclass Unrelated: pass\n",
        )?;
        std::fs::write(directory.join("plugin.py"), "pass")?;
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        let project = Project::load(
            &directory,
            &[directory.join("demo.py")],
            &manifest,
            json!({}),
        )?;
        let source = project.modules["demo"].source.clone();
        let mut runtime =
            Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py"))?;
        runtime.attach_project(Arc::new(project), json!({}));
        assert!(runtime.allowed(
            &source,
            "analyze-class",
            &json!({"class":{"qualified-name":"demo.Child"}}),
            &manifest
        ));
        assert!(!runtime.allowed(
            &source,
            "analyze-class",
            &json!({"class":{"qualified-name":"demo.Unrelated"}}),
            &manifest
        ));
        assert!(!runtime.allowed(
            &source,
            "adjust-call-state",
            &json!({"callee":{"expression":"demo.Child"}}),
            &manifest
        ));
        assert!(runtime.allowed(
            &source,
            "adjust-call-state",
            &json!({"callee":{"expression":"demo.Base"}}),
            &manifest
        ));
        let mut subclass_manifest = manifest.clone();
        subclass_manifest["claims"]["classes"] =
            json!([{"kind":"subclass-of","base-qualified-name":"demo.Base"}]);
        assert!(runtime.allowed(
            &source,
            "analyze-class",
            &json!({"class":{"qualified-name":"demo.Child"}}),
            &subclass_manifest
        ));
        assert!(!runtime.allowed(
            &source,
            "analyze-class",
            &json!({"class":{"qualified-name":"demo.Unrelated"}}),
            &subclass_manifest
        ));
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn oracle_display_forms_do_not_capture_unrelated_project_classes() {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-display-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let manifest = json!({"id":"native.display","name":"Display contract","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{},"claims":{}});
        std::fs::write(
            directory.join("library.py"),
            "class Literal: pass\nclass Prefetch: pass\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("consumer.py"),
            "from datetime import date, datetime\nfrom library import Prefetch as Imported\n",
        )
        .unwrap();
        std::fs::write(directory.join("plugin.py"), "\n").unwrap();
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let project = Project::load(
            &directory,
            &[directory.join("library.py"), directory.join("consumer.py")],
            &manifest,
            json!({}),
        )
        .unwrap();
        let mut runtime =
            Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py")).unwrap();
        runtime.attach_project(Arc::new(project), json!({}));
        let consumer = Source::new("\n".into(), "consumer.py".into(), "consumer".into());
        assert_eq!(
            runtime.canonical_name(&consumer, "Literal[1]"),
            "Literal[1]"
        );
        assert_eq!(
            runtime.type_expr(&consumer, "Literal[1]")["expression"],
            "Literal[1]"
        );
        let inferred = "Prefetch[Book, QuerySet[Book, Book], Literal[\"cached\"]]";
        assert_eq!(
            runtime.type_expr(&consumer, inferred)["expression"],
            inferred
        );
        let date = runtime.type_expr(&consumer, "date");
        assert_eq!(date["expression"], "date");
        assert_eq!(date["snapshot"]["qualified-name"], "datetime.date");
        let imported = runtime.type_expr(&consumer, "Imported[int]");
        assert_eq!(imported["expression"], "Imported[int]");
        assert_eq!(imported["snapshot"]["qualified-name"], "library.Prefetch");
        assert_eq!(
            imported["snapshot"]["arguments"][0]["qualified-name"],
            "builtins.int"
        );
        assert_eq!(
            runtime.type_expr(&consumer, "Literal[1]")["snapshot"],
            json!({"kind":"expression","expression":"Literal[1]","mode":"annotation"})
        );
        let protocol_type = runtime.qualified_type_expr("datetime.datetime");
        assert_eq!(
            protocol_type["snapshot"]["qualified-name"],
            "datetime.datetime"
        );
        let receiver =
            runtime.qualified_receiver_summary(&consumer, "library.Prefetch[datetime.datetime]");
        assert_eq!(
            receiver["generic-arguments"][0]["expression"],
            "datetime.datetime"
        );
        assert_eq!(
            runtime.type_expr(&consumer, "Self@select")["snapshot"]["kind"],
            "self-type"
        );
        let library = Source::new("\n".into(), "library.py".into(), "library".into());
        assert_eq!(
            runtime.canonical_name(&library, "Literal"),
            "library.Literal"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn chained_self_receiver_uses_its_bound_without_changing_display() {
        let value = json!({"expression":"Self","mode":"annotation","snapshot":{
            "kind":"self-type","bound":{"kind":"expression","expression":"demo.Selection[int]","mode":"annotation"}
        }});
        assert_eq!(
            Runtime::receiver_identity(&value).as_deref(),
            Some("demo.Selection[int]")
        );
        assert_eq!(value["expression"], "Self");
    }
}
