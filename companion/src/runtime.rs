use crate::model::{CallSite, Oracle, Source, Span, typ};
use crate::project::Project;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use ty_plugin_host::{HostError, MontyLimits, MontyRunner, PluginEnvironment, PluginHost};
use ty_plugin_protocol::{
    AnalyzeClassesRequest, ClassTransformRequest, PluginManifest, PluginRequest,
    method_name_pattern_matches,
};

pub struct Runtime {
    host: PluginHost<MontyRunner>,
    load_failures: BTreeMap<String, (String, String)>,
    plugins: Vec<Registration>,
    pub manifest: Value,
    method_names: BTreeSet<String>,
    method_patterns: Vec<String>,
    function_names: BTreeSet<String>,
    pub project: Option<Arc<Project>>,
    pub config: Value,
    pub python_version: String,
    pub platform: String,
    pub project_patches: BTreeMap<String, Value>,
    deferred_field_targets: BTreeSet<String>,
    contribution_field_targets: BTreeSet<String>,
    pub project_contributions: Vec<Value>,
    contribution_targets: BTreeMap<String, Vec<usize>>,
    unique_class_names: BTreeMap<String, Option<String>>,
    pub projected_receivers: BTreeMap<String, String>,
    protocol_types: BTreeMap<(String, String), BTreeMap<String, Value>>,
    virtual_shapes: BTreeMap<String, Value>,
    pub selected_virtual_scope: Option<String>,
    pub requested_virtual_types: BTreeSet<String>,
    pub nominal_candidates: BTreeMap<String, BTreeSet<usize>>,
    pub annotation_candidates: BTreeSet<String>,
    pub context_free_arguments: BTreeSet<(String, Span)>,
    /// Experimental projection strategy: retain native class field bindings
    /// and apply plugin refinements at the source accesses and calls instead.
    pub use_site_fields: bool,
    /// Editors need declarations for completion even before a member is typed.
    pub eager_class_fields: bool,
    cache: RequestCache,
    type_cache: RefCell<RequestCache>,
    projection_generation: u64,
    projection_base_generation: u64,
    protocol_observations: Cell<u64>,
    pub executions: usize,
    pub prefetched_executions: usize,
    pub hits: usize,
    pub execution_time: Duration,
    execution_profile: Option<BTreeMap<String, (usize, Duration)>>,
    batch_dispatches: usize,
    batch_requests: usize,
    batch_time: Duration,
    batched_kinds: BTreeSet<String>,
}

#[derive(Clone)]
struct Registration {
    id: String,
    manifest: Value,
    config: Value,
    native_member_abstentions: Option<String>,
    index: Option<Value>,
    class_indexes: BTreeMap<String, Value>,
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
    // JSON emits many tiny fragments. Hash the same byte stream in blocks,
    // without allocating another complete serialized project summary.
    {
        use std::io::Write as _;
        let mut buffered = std::io::BufWriter::new(&mut writer);
        serde_json::to_writer(&mut buffered, value)?;
        buffered.flush()?;
    }
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
        Some("analyze-classes") => {
            serde::Deserialize::deserialize(&payload).map(PluginRequest::AnalyzeClasses)
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

/// Move opaque author-owned JSON through the response encoder. The remaining
/// protocol fields retain their derived serialization and omission rules.
fn response_value(mut response: ty_plugin_protocol::PluginResponse) -> serde_json::Result<Value> {
    let indexes = if let ty_plugin_protocol::PluginResponse::ProjectIndex(index) = &mut response {
        Some((
            index.plugin_index.take(),
            std::mem::take(&mut index.class_indexes),
        ))
    } else {
        None
    };
    let mut value = serde_json::to_value(response)?;
    if let Some((index, classes)) = indexes {
        value["plugin-index"] = index;
        if !classes.is_empty() {
            value["class-indexes"] = Value::Object(classes.into_iter().collect());
        }
    }
    Ok(value)
}

#[derive(Default)]
struct RequestCache {
    entries: BTreeMap<RequestKey, CachedResponse>,
    oldest: BTreeSet<(u64, RequestKey)>,
    clock: u64,
    bytes: usize,
    oversized_skips: u64,
}

struct CachedResponse {
    value: Value,
    touched: u64,
    bytes: usize,
}

impl RequestCache {
    const MAX_ENTRIES: usize = 8192;
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    // A project index can outweigh thousands of class results. Admitting it
    // between sequential class scans can evict every reusable class response.
    // The response is still consumed normally; only memoization is declined.
    const MAX_ENTRY_BYTES: usize = Self::MAX_BYTES / 4;

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

    fn insert(&mut self, key: RequestKey, value: &Value) {
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
        let bytes = weight(value) + std::mem::size_of::<CachedResponse>() + 128;
        if bytes > Self::MAX_ENTRY_BYTES {
            self.oversized_skips += 1;
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
                value: value.clone(),
                touched: self.clock,
                bytes,
            },
        );
        self.oldest.insert((self.clock, key));
        self.bytes += bytes;
    }
}

/// Apply an author's explicit project-index input selection to this plugin's
/// private request. Other plugins and the source project retain complete data.
fn select_project_inputs(request: &mut Value, config: &Value) -> Result<()> {
    let Some(profile) = config
        .get("companion-project-inputs")
        .filter(|value| !value.is_null())
    else {
        return Ok(());
    };
    let object = profile
        .as_object()
        .context("companion-project-inputs must be an object")?;
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "assignment-kinds"
                | "class-source"
                | "field-call-keywords"
                | "field-call-argument-source"
                | "overlay-index"
        ) {
            bail!("unknown companion-project-inputs option: {key}");
        }
    }
    fn strings(profile: &Value, key: &str) -> Result<Option<BTreeSet<String>>> {
        profile
            .get(key)
            .map(|value| {
                value
                    .as_array()
                    .with_context(|| format!("{key} must be an array"))?
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .with_context(|| format!("{key} must contain strings"))
                    })
                    .collect()
            })
            .transpose()
    }
    let assignment_kinds = strings(profile, "assignment-kinds")?;
    if let Some(kinds) = &assignment_kinds {
        for kind in kinds {
            if !matches!(
                kind.as_str(),
                "call" | "literal" | "name" | "attribute" | "other"
            ) {
                bail!("unknown assignment kind: {kind}");
            }
        }
    }
    let keywords = strings(profile, "field-call-keywords")?;
    if let Some(mode) = profile.get("overlay-index")
        && !matches!(mode.as_str(), Some("complete" | "imports"))
    {
        bail!("overlay-index must be complete or imports");
    }
    let keep_source = profile
        .get("class-source")
        .map(|value| value.as_bool().context("class-source must be a boolean"))
        .transpose()?
        .unwrap_or(true);
    let keep_argument_source = profile
        .get("field-call-argument-source")
        .map(|value| {
            value
                .as_bool()
                .context("field-call-argument-source must be a boolean")
        })
        .transpose()?
        .unwrap_or(true);
    if let Some(kinds) = assignment_kinds
        && let Some(assignments) = request.get_mut("assignments").and_then(Value::as_array_mut)
    {
        assignments.retain(|assignment| {
            assignment
                .pointer("/assigned-value/kind")
                .and_then(Value::as_str)
                .is_none_or(|kind| {
                    // Selection must not hide malformed host input from the
                    // typed protocol decoder.
                    !matches!(kind, "call" | "literal" | "name" | "attribute" | "other")
                        || kinds.contains(kind)
                })
        });
    }
    fn class_inputs(
        class: &mut Value,
        keep_source: bool,
        keywords: Option<&BTreeSet<String>>,
        keep_argument_source: bool,
    ) {
        if !keep_source && let Some(class) = class.as_object_mut() {
            class.remove("source");
        }
        for field in class
            .get_mut("fields")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let Some(call) = field.get_mut("assigned-value") else {
                continue;
            };
            if call["kind"] == "call"
                && let Some(arguments) = call.get_mut("arguments").and_then(Value::as_array_mut)
            {
                if let Some(keywords) = keywords {
                    arguments.retain(|argument| {
                        argument["kind"] != "keyword"
                            || argument["name"]
                                .as_str()
                                .is_none_or(|name| keywords.contains(name))
                    });
                }
                if !keep_argument_source {
                    for argument in arguments {
                        if let Some(argument) = argument.as_object_mut() {
                            argument.remove("source");
                        }
                    }
                }
            }
        }
        for nested in class
            .get_mut("nested-classes")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            class_inputs(nested, keep_source, keywords, keep_argument_source);
        }
    }
    for class in request
        .get_mut("classes")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        class_inputs(class, keep_source, keywords.as_ref(), keep_argument_source);
    }
    Ok(())
}

fn early_return_method(config: &Value, method: &str) -> Result<bool> {
    configured_return_method(config, "companion-early-call-returns", method)
}

fn configured_return_method(config: &Value, key: &str, method: &str) -> Result<bool> {
    let Some(profile) = config.get(key) else {
        return Ok(false);
    };
    let methods = profile
        .as_array()
        .with_context(|| format!("{key} must be an array"))?;
    let mut selected = false;
    for value in methods {
        let name = value
            .as_str()
            .with_context(|| format!("{key} must contain method names"))?;
        ensure!(!name.is_empty(), "{key} requires nonempty method names");
        selected |= name == method;
    }
    Ok(selected)
}

impl Runtime {
    pub fn project_call_assignments_only(&self) -> Result<bool> {
        if !self.use_site_fields || self.plugins.is_empty() {
            return Ok(false);
        }
        let mut calls_only = true;
        for plugin in &self.plugins {
            select_project_inputs(&mut json!({}), &plugin.config)?;
            // Dependency hooks receive assignment summaries independently of
            // the project-index input selector, so keep their complete input.
            calls_only &= plugin.config["companion-project-inputs"]["assignment-kinds"]
                == json!(["call"])
                && plugin.manifest["capabilities"]["additional-dependencies"] != true;
        }
        Ok(calls_only)
    }

    pub fn sparse_overlay_index(&self) -> Result<bool> {
        if !self.use_site_fields || self.plugins.is_empty() {
            return Ok(false);
        }
        let mut sparse = true;
        for plugin in &self.plugins {
            // Validate every selection before indexing. One plugin requiring
            // complete syntax keeps the shared host index complete for all.
            select_project_inputs(&mut json!({}), &plugin.config)?;
            sparse &= plugin.config["companion-project-inputs"]["overlay-index"] == "imports";
        }
        Ok(sparse)
    }

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
            if let Some(value) = config
                .get("companion-native-fields")
                .filter(|v| !v.is_null())
            {
                ensure!(
                    value.is_boolean(),
                    "companion-native-fields must be a boolean"
                );
            }
            if let Some(value) = config
                .get("companion-parallel-calls")
                .filter(|v| !v.is_null())
            {
                ensure!(
                    value.is_boolean(),
                    "companion-parallel-calls must be a boolean"
                );
            }
            if let Some(value) = config.get("companion-native-cli").filter(|v| !v.is_null()) {
                ensure!(value.is_boolean(), "companion-native-cli must be a boolean");
            }
            if let Some(value) = config
                .get("companion-early-project-contributions")
                .filter(|v| !v.is_null())
            {
                ensure!(
                    value.is_boolean(),
                    "companion-early-project-contributions must be a boolean"
                );
            }
            if let Some(value) = config
                .get("companion-stateless-call-batches")
                .filter(|v| !v.is_null())
            {
                ensure!(
                    value.is_boolean(),
                    "companion-stateless-call-batches must be a boolean"
                );
            }
            declared_third_party_overlays(&config)?;
            let native_member_abstentions = config
                .get("companion-native-member-abstentions")
                .filter(|value| !value.is_null())
                .map(|value| {
                    let key = value.as_str().context(
                        "companion-native-member-abstentions must name an index property",
                    )?;
                    ensure!(
                        !key.is_empty(),
                        "companion-native-member-abstentions requires a nonempty index property"
                    );
                    Ok::<_, anyhow::Error>(key.to_owned())
                })
                .transpose()?;
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
                native_member_abstentions,
                index: None,
                class_indexes: BTreeMap::new(),
                index_generation: 0,
                virtual_names: BTreeMap::new(),
                inverse_virtual_names: BTreeMap::new(),
            });
            manifests.push(manifest);
        }
        // Preserve lexical plugin-id order for first-claim and first-field
        // precedence across registered plugins.
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
        for plugin in &plugins {
            early_return_method(&plugin.config, "")?;
        }
        let mut method_names = BTreeSet::new();
        let mut method_patterns = Vec::new();
        let mut function_names = BTreeSet::new();
        for claim in manifest_value["claims"]["methods"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(name) = claim["method-name"].as_str() {
                method_names.insert(name.to_owned());
            }
            if let Some(pattern) = claim["method-name-pattern"].as_str() {
                method_patterns.push(pattern.to_owned());
            }
        }
        for claim in manifest_value["claims"]["functions"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(name) = claim["qualified-name"].as_str() {
                function_names.insert(name.to_owned());
                if let Some((_, method)) = name.rsplit_once('.') {
                    method_names.insert(method.to_owned());
                }
            }
        }
        // An author can select native field handling for ordinary consumers.
        // Mixed registrations retain the general strategy unless every plugin
        // opts in. An explicit environment value still selects either route.
        let native_fields = std::env::var_os("TY_COMPANION_NATIVE_FIELDS").map_or_else(
            || {
                !plugins.is_empty()
                    && plugins
                        .iter()
                        .all(|plugin| plugin.config["companion-native-fields"] == true)
            },
            |value| value == "1",
        );
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
            method_names,
            method_patterns,
            function_names,
            project: None,
            project_patches: BTreeMap::new(),
            deferred_field_targets: BTreeSet::new(),
            contribution_field_targets: BTreeSet::new(),
            project_contributions: Vec::new(),
            contribution_targets: BTreeMap::new(),
            unique_class_names: BTreeMap::new(),
            projected_receivers: BTreeMap::new(),
            protocol_types: BTreeMap::new(),
            virtual_shapes: BTreeMap::new(),
            selected_virtual_scope: None,
            requested_virtual_types: BTreeSet::new(),
            nominal_candidates: BTreeMap::new(),
            annotation_candidates: BTreeSet::new(),
            context_free_arguments: BTreeSet::new(),
            use_site_fields: native_fields,
            eager_class_fields: false,
            cache: RequestCache::default(),
            type_cache: RefCell::default(),
            projection_generation: 0,
            projection_base_generation: 0,
            protocol_observations: Cell::new(0),
            executions: 0,
            prefetched_executions: 0,
            hits: 0,
            execution_time: Duration::ZERO,
            batch_dispatches: 0,
            batch_requests: 0,
            batch_time: Duration::ZERO,
            batched_kinds: BTreeSet::new(),
            execution_profile: std::env::var_os("TY_COMPANION_TRACE_PHASES")
                .map(|_| BTreeMap::new()),
        })
    }

    /// Every registered plugin must agree before bypassing a native stdlib
    /// lookup. A custom typeshed always retains the native precedence probe.
    pub fn third_party_overlay_modules(&self) -> Result<BTreeSet<String>> {
        let mut plugins = self.plugins.iter();
        let Some(first) = plugins.next() else {
            return Ok(BTreeSet::new());
        };
        let mut names = declared_third_party_overlays(&first.config)?;
        for plugin in plugins {
            let declared = declared_third_party_overlays(&plugin.config)?;
            names.retain(|name| declared.contains(name));
        }
        Ok(names)
    }

    pub fn prepares_project_contributions_early(&self) -> bool {
        self.prefers_native_cli()
            && self
                .plugins
                .iter()
                .all(|plugin| plugin.config["companion-early-project-contributions"] == true)
    }

    pub fn prefers_native_cli(&self) -> bool {
        self.use_site_fields
            && !self.plugins.is_empty()
            && self
                .plugins
                .iter()
                .all(|plugin| plugin.config["companion-native-cli"] == true)
    }

    pub fn attach_project(&mut self, project: Arc<Project>, config: Value) {
        // A type snapshot can depend on imports, ancestry and structural record
        // declarations anywhere in the project. Retain memoized snapshots only
        // within this immutable project revision; never key them by an address.
        self.type_cache.get_mut().clear();
        self.advance_projection_generation();
        self.protocol_types
            .retain(|(module, _), _| project.modules.contains_key(module));
        self.unique_class_names.clear();
        for qualified in project.classes.keys() {
            let short = qualified.rsplit('.').next().unwrap_or(qualified);
            self.unique_class_names
                .entry(short.to_owned())
                .and_modify(|name| *name = None)
                .or_insert_with(|| Some(qualified.clone()));
        }
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

    pub fn claims_method_name(&self, name: &str) -> bool {
        self.method_names.contains(name)
            || self
                .method_patterns
                .iter()
                .any(|pattern| method_name_pattern_matches(pattern, name))
    }

    pub fn early_returns_enabled(&self) -> bool {
        self.use_site_fields
            && self
                .plugins
                .iter()
                .any(|plugin| plugin.config.get("companion-early-call-returns").is_some())
    }

    /// Populate ordinary cache entries from static call inputs. Flow still
    /// constructs each real request and uses its complete key; a changed
    /// receiver, argument, context, or index falls back to normal execution.
    pub fn prefetch_call_hooks<'a>(
        &mut self,
        source: &Source,
        calls: impl Iterator<Item = &'a CallSite>,
    ) -> Result<usize> {
        if !self.use_site_fields
            || !self
                .plugins
                .iter()
                .any(|plugin| plugin.config["companion-parallel-calls"] == true)
        {
            return Ok(0);
        }
        struct Invocation {
            owner: usize,
            key: RequestKey,
            kind: &'static str,
            request: PluginRequest,
        }
        let mut invocations = Vec::new();
        let mut seen = BTreeSet::new();
        'calls: for call in calls {
            if !call.arguments.iter().all(|arg| {
                matches!(
                    arg.literal["kind"].as_str(),
                    Some("int" | "str" | "bool" | "none")
                )
            }) {
                continue;
            }
            for (kind, speculative) in [("adjust-call-return", true), ("adjust-call-state", false)]
            {
                let Ok(PreparedCall::Request(payload)) =
                    prepare_call_request(self, source, kind, call, None, None, speculative)
                else {
                    continue;
                };
                for owner in 0..self.plugins.len() {
                    let Some(request) = self.semantic_request(owner, source, kind, &payload)?
                    else {
                        continue;
                    };
                    let plugin = &self.plugins[owner];
                    // Calls select their first claimant even when it abstains.
                    // Non-opted-in plugins and failures retain that exact path.
                    if plugin.config["companion-parallel-calls"] != true
                        || plugin.manifest["capabilities"]["project-index-lookup"] != true
                        || plugin.index.is_none()
                        || self.load_failures.contains_key(&plugin.id)
                    {
                        break;
                    }
                    let key = self.semantic_key(owner, &request)?;
                    if !self.cache.entries.contains_key(&key) && seen.insert(key) {
                        let Ok(request) = decode_request(request) else {
                            break;
                        };
                        invocations.push(Invocation {
                            owner,
                            key,
                            kind,
                            request,
                        });
                        // Bound speculative work and retained replies even
                        // for a module containing thousands of call sites.
                        if invocations.len() == 256 {
                            break 'calls;
                        }
                    }
                    break;
                }
            }
        }
        if invocations.len() < 8 {
            return Ok(0);
        }
        // Only an explicit author promise permits shared module initialization.
        // Each bounded group owns one plugin and that plugin's immutable index.
        let mut groups = Vec::new();
        let mut first = 0;
        while first < invocations.len() {
            let owner = invocations[first].owner;
            let opted_in = self.plugins[owner].config["companion-stateless-call-batches"] == true;
            let mut end = first + 1;
            if opted_in {
                while end < invocations.len() && end - first < 16 && invocations[end].owner == owner
                {
                    end += 1;
                }
            }
            groups.push((first..end, opted_in && end - first > 1));
            first = end;
        }
        let results = {
            let host = &self.host;
            let plugins = &self.plugins;
            crate::parallel::map_ordered(
                &groups,
                crate::parallel::worker_limit(),
                |(range, batched)| {
                    let started = Instant::now();
                    let plugin = &plugins[invocations[range.start].owner];
                    let replies = if *batched {
                        let requests = invocations[range.clone()]
                            .iter()
                            .map(|invocation| &invocation.request)
                            .collect::<Vec<_>>();
                        match host.execute_stateless_batch_with_project_index(
                            &plugin.id,
                            &requests,
                            plugin.index.as_ref().expect("indexed prefetch"),
                        ) {
                            Ok(replies) => replies.into_iter().map(Some).collect(),
                            Err(_) => std::iter::repeat_with(|| None).take(range.len()).collect(),
                        }
                    } else {
                        let invocation = &invocations[range.start];
                        vec![
                            host.execute_with_project_index(
                                &plugin.id,
                                &invocation.request,
                                plugin.index.as_ref().expect("indexed prefetch"),
                            )
                            .ok(),
                        ]
                    };
                    (replies, started.elapsed())
                },
            )?
        };
        for ((range, batched), (replies, elapsed)) in groups.iter().zip(results) {
            self.execution_time += elapsed;
            if *batched {
                self.batch_dispatches += 1;
                self.batch_requests += range.len();
                self.batch_time += elapsed;
            }
            for (invocation, reply) in invocations[range.clone()].iter().zip(replies) {
                self.executions += 1;
                self.prefetched_executions += 1;
                if *batched {
                    self.batched_kinds.insert(invocation.kind.to_owned());
                }
                if let Some(profile) = &mut self.execution_profile {
                    let entry = profile.entry(invocation.kind.to_owned()).or_default();
                    entry.0 += 1;
                    // Per-handler timing is unavailable inside a shared run.
                    // Report null plus the separate complete batch duration.
                    if !*batched {
                        entry.1 += elapsed;
                    }
                }
                let response = match reply {
                    Some(ty_plugin_protocol::PluginResponse::Error(_)) => {
                        json!({"kind":"no-change"})
                    }
                    Some(response) => response_value(response)?,
                    // No speculative batch failure becomes a real-call result.
                    // The existing path retries with fresh request state.
                    None => continue,
                };
                self.cache.insert(invocation.key, &response);
            }
        }
        Ok(invocations.len())
    }

    pub fn claims_function(&self, source: &Source, name: &str) -> bool {
        self.function_names
            .contains(&self.protocol_name(source, name))
    }

    /// Semantic results can depend on each plugin's opaque project index even
    /// when the merged contributions and virtual types have not changed.
    pub fn index_generations(&self) -> Vec<(String, u64)> {
        self.plugins
            .iter()
            .map(|plugin| (plugin.id.clone(), plugin.index_generation))
            .collect()
    }

    pub fn cache_statistics(&self) -> Value {
        json!({
            "entries": self.cache.entries.len(),
            "bytes": self.cache.bytes,
            "oversized_skips": self.cache.oversized_skips,
            "stateless_batch_dispatches": self.batch_dispatches,
            "stateless_batch_requests": self.batch_requests,
            "stateless_batch_seconds": self.batch_time.as_secs_f64(),
        })
    }

    /// Host execution attempts only; request-cache hits never enter this path.
    pub fn execution_profile(&self) -> Value {
        self.execution_profile
            .as_ref()
            .map_or(Value::Null, |profile| {
                Value::Object(
                    profile
                        .iter()
                        .map(|(kind, (executions, elapsed))| {
                            (
                                kind.clone(),
                                if self.batched_kinds.contains(kind) {
                                    json!({"executions":executions,"seconds":null,"seconds_unbatched":elapsed.as_secs_f64(),"timing":"See total stateless_batch_seconds; individual batch-handler durations are not measured."})
                                } else { json!({"executions":executions,"seconds":elapsed.as_secs_f64()}) },
                            )
                        })
                        .collect(),
                )
            })
    }

    /// Changes observable by another file during one fixed-point loop. The
    /// surrounding check fixes plugin registrations/configuration, the opaque
    /// project indexes, contributions and the virtual display scope before
    /// entering that loop. This token is not a cross-revision cache key.
    pub fn projection_generation(&self) -> u64 {
        self.projection_generation
    }

    pub fn projection_context(&self, reads_protocol_aliases: bool) -> (u64, Option<u64>) {
        (
            self.projection_base_generation,
            reads_protocol_aliases.then_some(self.projection_generation),
        )
    }

    pub fn protocol_observations(&self) -> u64 {
        self.protocol_observations.get()
    }

    fn observe_protocol_aliases(&self) {
        self.protocol_observations.set(
            self.protocol_observations
                .get()
                .checked_add(1)
                .expect("protocol observation counter exhausted"),
        );
    }

    fn advance_projection_generation(&mut self) {
        self.projection_base_generation = self
            .projection_base_generation
            .checked_add(1)
            .expect("projection base generation exhausted");
        self.advance_alias_generation();
    }

    fn advance_alias_generation(&mut self) {
        self.projection_generation = self
            .projection_generation
            .checked_add(1)
            .expect("projection generation exhausted");
    }

    pub fn set_project_patch(&mut self, qualified: String, patch: Value) {
        if patch["fields"]
            .as_array()
            .is_some_and(|fields| !fields.is_empty())
            || patch.get("constructor").is_some_and(Value::is_object)
        {
            self.deferred_field_targets.insert(qualified.clone());
        } else {
            self.deferred_field_targets.remove(&qualified);
        }
        if self.project_patches.get(&qualified) != Some(&patch) {
            self.project_patches.insert(qualified, patch);
            self.advance_projection_generation();
        }
    }

    pub fn clear_project_patches(&mut self) {
        self.project_patches.clear();
        self.deferred_field_targets.clear();
    }

    pub fn set_project_contributions(&mut self, contributions: Vec<Value>) {
        self.contribution_targets.clear();
        self.contribution_field_targets.clear();
        for (index, contribution) in contributions.iter().enumerate() {
            if let Some(target) = contribution["target"]["qualified-name"].as_str() {
                if matches!(
                    contribution["patch"]["kind"].as_str(),
                    Some("field" | "constructor")
                ) {
                    self.contribution_field_targets.insert(target.to_owned());
                }
                self.contribution_targets
                    .entry(target.to_owned())
                    .or_default()
                    .push(index);
            }
        }
        self.project_contributions = contributions;
    }

    pub fn contributions_for(&self, target: &str) -> impl DoubleEndedIterator<Item = &Value> {
        self.contribution_targets
            .get(target)
            .into_iter()
            .flatten()
            .map(|index| &self.project_contributions[*index])
    }

    /// In native mode an untouched class needs no generated class declaration.
    /// Class hooks, installed patches and contributions retain full lowering.
    pub fn keeps_native_class(&self, qualified: &str) -> bool {
        self.use_site_fields
            && self
                .plugins
                .iter()
                .all(|plugin| plugin.manifest["capabilities"]["class-transform"] != true)
            && self.project_patches.get(qualified).is_none_or(|patch| {
                patch.as_object().is_some_and(|patch| {
                    patch.len() == 1 && patch.get("kind").is_some_and(|kind| kind == "no-change")
                })
            })
            && self.contributions_for(qualified).next().is_none()
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
                    && let Some(Some(candidate)) = self.unique_class_names.get(nominal)
                {
                    return format!("{candidate}{}", &qualified[nominal.len()..]);
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
            for contribution in self.contributions_for(&owner).rev() {
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
        // Cache hits still observe the alias registry: replacing a producer's
        // record must invalidate every consumer that used its cached snapshot.
        self.observe_protocol_aliases();
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
        self.type_cache.borrow_mut().insert(key, &value);
        value
    }

    /// Types obtained from protocol metadata and established flow identities
    /// are already qualified. Source aliases must not be applied a second time.
    pub fn qualified_type_expr(&self, expression: &str) -> Value {
        self.qualified_type_expr_in_module(expression, None)
    }

    fn qualified_type_expr_in_module(&self, expression: &str, module: Option<&str>) -> Value {
        self.observe_protocol_aliases();
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
        self.type_cache.borrow_mut().insert(key, &value);
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
            self.advance_alias_generation();
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
            if module == Some(owner.as_str()) || name.contains('.') {
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
            if self
                .project
                .as_ref()
                .is_some_and(|project| project.renderer_virtual_class_names().contains(name))
            {
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
        for contribution in self.contributions_for(owner).rev() {
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

    pub fn has_use_site_fields(&self) -> bool {
        self.use_site_fields
            && !self.eager_class_fields
            && (!self.deferred_field_targets.is_empty()
                || !self.contribution_field_targets.is_empty())
    }

    pub fn projects_fields_at_uses(&self, class: &str) -> bool {
        if !self.has_use_site_fields() {
            return false;
        }
        let Some(project) = self.project.as_ref() else {
            return false;
        };
        let Some(owners) = project.mro(class) else {
            return false;
        };
        // Field contributions retain their native class declaration and
        // assignment rules. A receiver-only view cannot reproduce that field
        // contract, including inherited contributions.
        if owners
            .iter()
            .any(|owner| self.contribution_field_targets.contains(owner))
            || !owners
                .iter()
                .any(|owner| self.deferred_field_targets.contains(owner))
        {
            return false;
        }
        let Some((_, _, original)) = project.original_class(class) else {
            return false;
        };
        if self.project_patches.get(class).is_some_and(|patch| {
            patch["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|field| {
                    let descriptor = &field["descriptor"];
                    matches!(descriptor["kind"].as_str(), Some("value" | "callable"))
                        || descriptor.get("class-type").is_some()
                })
        }) {
            return false;
        }
        if !original.decorator_list.is_empty()
            || original.type_params.is_some()
            || original.arguments.as_ref().is_some_and(|arguments| {
                arguments
                    .args
                    .iter()
                    .any(|base| matches!(base, ruff_python_ast::Expr::Subscript(_)))
            })
            || project.default_constructor_result(class).is_none()
        {
            return false;
        }
        project.class_summary(class).is_some_and(|summary| {
            !summary["methods"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|method| method["name"] == "__init__")
        })
    }

    fn use_site_field(&self, source: &Source, owner: &str, member: &str) -> Option<Value> {
        let nominal = self.nominal_name(source, owner);
        let owners = self.project.as_ref()?.mro(&nominal)?;
        for owner in owners {
            if !self.projects_fields_at_uses(&owner) {
                continue;
            }
            if let Some(field) = self
                .project_patches
                .get(&owner)
                .and_then(|patch| patch["fields"].as_array())
                .and_then(|fields| fields.iter().find(|field| field["name"] == member))
            {
                if field["mode"] != "replace-existing"
                    && self
                        .project
                        .as_ref()?
                        .class_summary(&owner)
                        .is_some_and(|class| {
                            class["fields"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|value| value["name"] == member)
                        })
                {
                    return None;
                }
                return Some(field.clone());
            }
            for contribution in self.contributions_for(&owner) {
                if contribution["patch"]["kind"] == "field"
                    && contribution["patch"]["name"] == member
                {
                    return Some(contribution["patch"].clone());
                }
            }
        }
        None
    }

    fn use_site_constructor(&self, source: &Source, callee: &str) -> Option<Value> {
        let owner = self.canonical_name(source, callee);
        if !self.projects_fields_at_uses(&owner) {
            return None;
        }
        let project = self.project.as_ref()?;
        // Match Python's inherited __init__ lookup. A subclass that receives
        // no patch still inherits its nearest generated constructor; a real
        // intervening initializer or eagerly emitted patch remains native.
        for candidate in project.mro(&owner)? {
            if project.class_summary(&candidate).is_some_and(|class| {
                class["methods"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|method| method["name"] == "__init__")
            }) {
                return None;
            }
            let patch = self.project_patches.get(&candidate);
            let mut fields = BTreeMap::new();
            for field in patch
                .and_then(|patch| patch["fields"].as_array())
                .into_iter()
                .flatten()
            {
                fields.insert(field["name"].as_str()?, field);
            }
            let mut explicit = patch
                .and_then(|patch| patch.get("constructor"))
                .filter(|value| value.is_object());
            for contribution in self.contributions_for(&candidate) {
                let value = &contribution["patch"];
                match value["kind"].as_str() {
                    Some("field") => {
                        fields.entry(value["name"].as_str()?).or_insert(value);
                    }
                    Some("constructor") => {
                        explicit = explicit.or(Some(value));
                    }
                    _ => {}
                }
            }
            let mut parameters = Vec::new();
            for field in fields.values() {
                if let Some(parameter) = field.get("constructor-parameter") {
                    let mut parameter = parameter.clone();
                    if parameter.get("type-expr").is_none_or(Value::is_null) {
                        parameter["type-expr"] = field
                            .get("instance-set-type")
                            .unwrap_or(&field["instance-get-type"])
                            .clone();
                    }
                    if field["has-default"] == true {
                        parameter["required"] = json!(false);
                    }
                    parameters.push(parameter);
                }
            }
            if explicit.is_none() && parameters.is_empty() {
                continue;
            }
            if !self.projects_fields_at_uses(&candidate) {
                return None;
            }
            let mut signature = explicit
                .cloned()
                .unwrap_or_else(|| json!({"parameters":parameters}));
            signature["return-type"] =
                self.qualified_type_expr(&project.default_constructor_result(&owner)?);
            return Some(json!({"kind":"call-signature-patch","signature":signature}));
        }
        None
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
        if !class_access && let Some(field) = self.use_site_field(source, owner, member) {
            return Ok(
                json!({"kind":"member-patch","name":member,"mode":"replace-existing", "read-only":false,
                "access":{"kind":"value","type-expr":field["instance-get-type"]}}),
            );
        }
        Ok(json!({"kind":"no-change"}))
    }

    fn native_member_abstains(&self, index: usize, kind: &str, request: &Value) -> bool {
        let plugin = &self.plugins[index];
        if kind != "resolve-instance-member"
            || !self.use_site_fields
            || self.load_failures.contains_key(&plugin.id)
        {
            return false;
        }
        let Some(key) = &plugin.native_member_abstentions else {
            return false;
        };
        let (Some(owner), Some(member)) = (
            request["owner"]["expression"].as_str(),
            request["member-name"].as_str(),
        ) else {
            return false;
        };
        plugin
            .index
            .as_ref()
            .and_then(|index| index.get(key))
            .and_then(|owners| owners.get(owner))
            .and_then(|members| members.get(member))
            .and_then(Value::as_bool)
            == Some(true)
    }

    fn selected_signature(&self, index: usize, source: &Source, payload: &Value) -> Result<bool> {
        let plugin = &self.plugins[index];
        let key = "companion-call-signature-methods";
        if !plugin
            .config
            .as_object()
            .is_some_and(|config| config.contains_key(key))
        {
            return Ok(true);
        }
        let callee = payload["callee"]["expression"].as_str().unwrap_or("");
        let method = callee.rsplit('.').next().unwrap_or(callee);
        let selected = configured_return_method(&plugin.config, key, method)?;
        // Constructors and explicit function claims retain their full hooks.
        // A selected-out owner still owns the call: no later plugin may run.
        let function = plugin.manifest["claims"]["functions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|claim| claim["qualified-name"] == self.protocol_name(source, callee));
        Ok(selected
            || payload.get("receiver").is_none()
            || function
            || self.load_failures.contains_key(&plugin.id))
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
                if (kind == "adjust-call-state"
                    || (self.use_site_fields && kind == "adjust-call-signature"))
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
        self.dispatch_with_key(id, payload, key, None)
    }

    fn dispatch_with_key(
        &mut self,
        id: &str,
        payload: Value,
        key: RequestKey,
        index_owner: Option<usize>,
    ) -> Result<Value> {
        if let Some(value) = self.cache.get(&key) {
            self.hits += 1;
            return Ok(value.clone());
        }
        let kind = payload["kind"].as_str().unwrap_or("").to_owned();
        let request = decode_request(payload).context("invalid plugin request")?;
        let response = self.run_request(id, &kind, &request, index_owner)?;
        // Like the checker's Salsa memo, cache failures for the exact request.
        // Diagnostics remain in the response and are replayed at each current
        // source location, never accumulated globally across editor revisions.
        self.cache.insert(key, &response);
        Ok(response)
    }

    fn run_request(
        &mut self,
        id: &str,
        kind: &str,
        request: &PluginRequest,
        index_owner: Option<usize>,
    ) -> Result<Value> {
        let start = Instant::now();
        if let Some((message, hint)) = self.load_failures.get(id) {
            return Ok(hook_failure_message(id, kind, message, hint));
        }
        let result =
            if let Some(index) = index_owner.and_then(|owner| self.plugins[owner].index.as_ref()) {
                self.host.execute_with_project_index(id, request, index)
            } else {
                self.host.execute(id, request)
            };
        let elapsed = start.elapsed();
        self.execution_time += elapsed;
        if let Some(profile) = &mut self.execution_profile {
            let entry = profile.entry(kind.to_owned()).or_default();
            entry.0 += 1;
            entry.1 += elapsed;
        }
        if matches!(kind, "build-project-index" | "analyze-classes")
            && std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some()
        {
            eprintln!("companion runtime {kind}: {:.6}s", elapsed.as_secs_f64());
        }
        self.executions += 1;
        let response = match result {
            // An explicit protocol Error is an abstention, unlike a host
            // failure (trap, execution limit or undecodable response).
            Ok(ty_plugin_protocol::PluginResponse::Error(_)) => json!({"kind":"no-change"}),
            Ok(response) => response_value(response)?,
            Err(error) => hook_failure_response(id, kind, error),
        };
        Ok(response)
    }

    fn dispatch_semantic(&mut self, index: usize, mut payload: Value) -> Result<Value> {
        let id = self.plugins[index].id.clone();
        // A generation identifies the exact opaque index supplied to this
        // plugin. Equal indexes preserve cached results across source revisions
        // without cloning or hashing a megabyte-sized index for every hook.
        let key = self.semantic_key(index, &payload)?;
        if let Some(response) = self.cache.get(&key) {
            self.hits += 1;
            return Ok(response.clone());
        }
        let plugin = &self.plugins[index];
        let class_index = (payload["kind"] == "analyze-class")
            .then(|| payload["class"]["qualified-name"].as_str())
            .flatten()
            .and_then(|name| plugin.class_indexes.get(name));
        let lazy = class_index.is_none()
            && plugin.manifest["capabilities"]["project-index-lookup"] == true;
        if !lazy && let Some(project_index) = class_index.or(plugin.index.as_ref()) {
            payload["project-index"] = project_index.clone();
        }
        self.dispatch_with_key(&id, payload, key, lazy.then_some(index))
    }

    fn semantic_key(&self, index: usize, payload: &Value) -> Result<RequestKey> {
        request_key(&(
            "semantic",
            &self.plugins[index].id,
            self.plugins[index].index_generation,
            payload,
        ))
    }

    fn semantic_request(
        &self,
        index: usize,
        source: &Source,
        kind: &str,
        payload: &Value,
    ) -> Result<Option<Value>> {
        let plugin = &self.plugins[index];
        if plugin.virtual_names.is_empty() && !self.allowed(source, kind, payload, &plugin.manifest)
        {
            return Ok(None);
        }
        let mut request = payload.clone();
        rewrite_protocol_names(&mut request, &plugin.inverse_virtual_names);
        if !plugin.virtual_names.is_empty()
            && !self.allowed(source, kind, &request, &plugin.manifest)
        {
            return Ok(None);
        }
        let object = request
            .as_object_mut()
            .context("hook payload must be object")?;
        object.insert("kind".into(), json!(kind));
        object.insert("context".into(), json!({"module":source.protocol_module.as_ref().unwrap_or(&source.module),"file-path":source.path,
            "python-version":self.python_version,"platform":self.platform,"config":plugin.config,"speculative":payload["context"]["speculative"].as_bool().unwrap_or(false)}));
        if source.protocol_module.is_some() {
            rewrite_notebook_request(&mut request, source, |name| {
                self.protocol_name(source, name)
            });
        }
        Ok(Some(request))
    }

    /// Populate the ordinary per-request cache through an explicitly opted-in
    /// batch hook. Only complete class-scoped indexes qualify; an unlisted or
    /// oversized class retains the existing individual hook and index lookup.
    pub fn prepare_class_transforms(&mut self, project: &Project) -> Result<()> {
        const MAX_ITEMS: usize = 128;
        const MAX_BYTES: usize = 1024 * 1024;
        for index in 0..self.plugins.len() {
            if self.plugins[index].manifest["capabilities"]["class-batch"] != true {
                continue;
            }
            let mut requests = Vec::new();
            let mut keys = Vec::new();
            // Reserve the outer object and commas as well as item payloads.
            let mut bytes = 64;
            for (name, summary) in project.classes.iter() {
                let module = summary
                    .pointer("/source/module")
                    .and_then(Value::as_str)
                    .or_else(|| name.rsplit_once('.').map(|v| v.0));
                let Some(module) = module.and_then(|name| project.modules.get(name)) else {
                    continue;
                };
                if module.external {
                    continue;
                }
                let Some(mut request) = self.semantic_request(
                    index,
                    &module.source,
                    "analyze-class",
                    &json!({"class":summary}),
                )?
                else {
                    continue;
                };
                let key = self.semantic_key(index, &request)?;
                if self.cache.get(&key).is_some() {
                    continue;
                }
                let Some(class_index) = request["class"]["qualified-name"]
                    .as_str()
                    .and_then(|name| self.plugins[index].class_indexes.get(name))
                else {
                    continue;
                };
                request["project-index"] = class_index.clone();
                let PluginRequest::AnalyzeClass(request) = decode_request(request)? else {
                    bail!("class batch preparation received a non-class request");
                };
                let request = ClassTransformRequest::AnalyzeClass(request);
                // Measure the exact typed wire representation, including
                // protocol defaults, rather than a pre-decoding approximation.
                let size = serde_json::to_vec(&request)?.len() + 1;
                if size + 64 > MAX_BYTES {
                    continue;
                }
                if requests.len() == MAX_ITEMS || bytes + size > MAX_BYTES {
                    self.dispatch_class_batch(index, &mut requests, &mut keys)?;
                    bytes = 64;
                }
                requests.push(request);
                keys.push(key);
                bytes += size;
            }
            self.dispatch_class_batch(index, &mut requests, &mut keys)?;
        }
        Ok(())
    }

    fn dispatch_class_batch(
        &mut self,
        index: usize,
        requests: &mut Vec<ClassTransformRequest>,
        keys: &mut Vec<RequestKey>,
    ) -> Result<()> {
        if requests.is_empty() {
            return Ok(());
        }
        let id = self.plugins[index].id.clone();
        // Cache individual answers only. Retaining a second copy of each batch
        // wastes the cache budget and can evict items before their first use.
        let request = PluginRequest::AnalyzeClasses(AnalyzeClassesRequest {
            requests: std::mem::take(requests),
        });
        let response = self.run_request(&id, "analyze-classes", &request, None)?;
        let responses = response["responses"]
            .as_array()
            .filter(|responses| response["kind"] == "class-batch" && responses.len() == keys.len());
        if let Some(responses) = responses {
            // Decode validated every item before any result becomes visible.
            // Explicit protocol errors abstain just like individual hooks.
            for (key, response) in keys.drain(..).zip(responses) {
                self.cache.insert(
                    key,
                    &if response["kind"] == "error" {
                        json!({"kind":"no-change"})
                    } else {
                        response.clone()
                    },
                );
            }
        } else {
            if response["kind"] != "no-change" {
                eprintln!(
                    "warning: Plugin `{id}` returned a class batch with the wrong result count; retrying individual hooks"
                );
            }
            keys.clear();
        }
        Ok(())
    }

    pub fn execute_project(&mut self, kind: &str, payload: Value) -> Result<Value> {
        self.execute_project_inner(kind, payload, true)
    }

    /// A disposable CLI revision consumes opaque indexes here. Its caller
    /// needs contributions and diagnostics, but never a second copy of the
    /// index, nor a cache entry for another project-index request.
    pub fn execute_ephemeral_project_index(&mut self, payload: Value) -> Result<Value> {
        self.execute_project_inner("build-project-index", payload, false)
    }

    fn execute_project_inner(
        &mut self,
        kind: &str,
        payload: Value,
        retain_index_response: bool,
    ) -> Result<Value> {
        let capability = match kind {
            "build-project-index" => "project-index",
            "additional-dependencies" => "additional-dependencies",
            _ => bail!("not a project hook: {kind}"),
        };
        let last = self
            .plugins
            .iter()
            .rposition(|plugin| plugin.manifest["capabilities"][capability] == true);
        let mut payload = Some(payload);
        let mut responses = Vec::new();
        for index in 0..self.plugins.len() {
            let plugin = &self.plugins[index];
            if plugin.manifest["capabilities"][capability] != true {
                continue;
            }
            let mut request = if Some(index) == last {
                payload.take().expect("last project hook owns the payload")
            } else {
                payload.as_ref().expect("remaining project hooks").clone()
            };
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
            let input_start = Instant::now();
            if kind == "build-project-index" && self.use_site_fields {
                select_project_inputs(&mut request, &plugin.config)?;
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
            if kind == "build-project-index"
                && std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some()
            {
                eprintln!(
                    "companion index input selection: {:.6}s",
                    input_start.elapsed().as_secs_f64()
                );
            }
            let mut response = if retain_index_response {
                self.dispatch(&id, request)?
            } else {
                let decode_start = Instant::now();
                // This disposable request comes from the companion's typed
                // source-summary builder. Consume its strings and containers
                // instead of cloning them through borrowed deserialization.
                // External and cached requests retain decode_request's exact
                // enum fallback and invalid-input diagnostics.
                let request =
                    serde_json::from_value::<ty_plugin_protocol::BuildProjectIndexRequest>(request)
                        .map(PluginRequest::BuildProjectIndex)
                        .context("invalid plugin request")?;
                if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
                    eprintln!(
                        "companion index protocol decode: {:.6}s",
                        decode_start.elapsed().as_secs_f64()
                    );
                }
                self.run_request(&id, kind, &request, None)?
            };
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
                let (next_index, next_classes) = if retain_index_response {
                    (
                        response.get("plugin-index").cloned(),
                        response["class-indexes"]
                            .as_object()
                            .map(|classes| {
                                classes
                                    .iter()
                                    .map(|(name, value)| (name.clone(), value.clone()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                } else {
                    let object = response.as_object_mut().context("plugin response object")?;
                    let index = object.remove("plugin-index");
                    let classes = match object.remove("class-indexes") {
                        Some(Value::Object(classes)) => classes.into_iter().collect(),
                        _ => BTreeMap::new(),
                    };
                    (index, classes)
                };
                if self.plugins[index].index != next_index
                    || self.plugins[index].class_indexes != next_classes
                {
                    self.plugins[index].index_generation = self.plugins[index]
                        .index_generation
                        .checked_add(1)
                        .context("plugin index generation exhausted")?;
                    self.plugins[index].index = next_index;
                    self.plugins[index].class_indexes = next_classes;
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
        if kind == "adjust-call-signature"
            && payload.get("receiver").is_none()
            && let Some(callee) = payload
                .pointer("/callee/expression")
                .and_then(Value::as_str)
            && let Some(patch) = self.use_site_constructor(source, callee)
        {
            return Ok(patch);
        }
        let mut responses = Vec::new();
        for index in 0..self.plugins.len() {
            let Some(request) = self.semantic_request(index, source, kind, &payload)? else {
                continue;
            };
            if self.native_member_abstains(index, kind, &request) {
                // A member abstention still permits later claiming plugins.
                continue;
            }
            if kind == "adjust-call-signature"
                && !self.selected_signature(index, source, &request)?
            {
                return Ok(json!({"kind":"no-change"}));
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

fn declared_third_party_overlays(config: &Value) -> Result<BTreeSet<String>> {
    let Some(names) = config
        .get("companion-third-party-overlays")
        .filter(|v| !v.is_null())
    else {
        return Ok(BTreeSet::new());
    };
    names
        .as_array()
        .context("companion-third-party-overlays must be an array")?
        .iter()
        .map(|name| {
            let name = name
                .as_str()
                .context("companion-third-party-overlays must contain module names")?;
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|ch| ch == b'_' || ch.is_ascii_alphanumeric())
                    && !name.as_bytes()[0].is_ascii_digit()
                    && ruff_python_parser::parse_module(&format!("import {name}\n")).is_ok(),
                "companion-third-party-overlays must contain top-level Python module names"
            );
            Ok(name.to_owned())
        })
        .collect()
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
                        | "class-indexes"
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
                        | "class-indexes"
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
    call_request_with_speculation(runtime, source, kind, call, oracle, receiver, false)
}

#[derive(Clone, PartialEq)]
pub struct EarlyCallReturn {
    pub response: Value,
    /// The author opts into keeping native call typing while providing the
    /// same nominal identity for subsequent flow hooks. This is local host
    /// policy; it does not change or decorate a plugin protocol response.
    pub native: bool,
}

pub fn speculative_call_return(
    runtime: &mut Runtime,
    source: &Source,
    call: &CallSite,
) -> Result<Option<EarlyCallReturn>> {
    if !runtime.early_returns_enabled()
        || call.flow_receiver_type.is_none()
        || !call.arguments.iter().all(|arg| {
            matches!(
                arg.literal["kind"].as_str(),
                Some("int" | "str" | "bool" | "none")
            )
        })
    {
        return Ok(None);
    }
    let response = call_request_with_speculation(
        runtime,
        source,
        "adjust-call-return",
        call,
        None,
        None,
        true,
    )?;
    let ground = response["return-type"]["mode"] == "annotation"
        && response["return-type"]["expression"]
            .as_str()
            .is_some_and(|name| {
                runtime
                    .project
                    .as_ref()
                    .is_some_and(|project| project.plain_source_class(name))
            });
    if response["kind"] != "call-return-patch" || !ground {
        return Ok(None);
    }
    // Complex snapshots, annotation imports and opaque result metadata retain
    // their existing projection/validation path. Native delegation is limited
    // to the plain source class already accepted above.
    let mut native = response["return-type"]["snapshot"].is_null()
        && response["return-type"]["imports"]
            .as_array()
            .is_none_or(Vec::is_empty)
        && response["result-metadata"].is_null();
    let method = call.callee.rsplit('.').next().unwrap_or(&call.callee);
    // Unrelated plugins also retain the existing path unless they explicitly
    // agree. A shared call must never drop another plugin's replacement.
    for plugin in &runtime.plugins {
        native &=
            configured_return_method(&plugin.config, "companion-native-call-returns", method)?;
    }
    Ok(Some(EarlyCallReturn { response, native }))
}

enum PreparedCall {
    Response(Value),
    Request(Value),
}

fn call_request_with_speculation(
    runtime: &mut Runtime,
    source: &Source,
    kind: &str,
    call: &CallSite,
    oracle: Option<&Oracle>,
    receiver: Option<&str>,
    speculative: bool,
) -> Result<Value> {
    match prepare_call_request(runtime, source, kind, call, oracle, receiver, speculative)? {
        PreparedCall::Response(response) => Ok(response),
        PreparedCall::Request(payload) => runtime.execute(source, kind, payload),
    }
}

fn prepare_call_request(
    runtime: &mut Runtime,
    source: &Source,
    kind: &str,
    call: &CallSite,
    oracle: Option<&Oracle>,
    receiver: Option<&str>,
    speculative: bool,
) -> Result<PreparedCall> {
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
    // Deferred class fields contribute a constructor independently of call
    // hook claims. Resolve it before the plugin-only routing fast path.
    if kind == "adjust-call-signature"
        && summary.is_none()
        && let Some(patch) = runtime.use_site_constructor(source, &callee)
    {
        return Ok(PreparedCall::Response(patch));
    }
    // Claims inspect only the callee and nominal receiver. Avoid constructing
    // argument/return snapshots and source locations for calls no plugin can
    // observe. Use the same namespace translation and claim implementation as
    // execute(), including notebooks and inherited state-hook restrictions.
    let mut routing = json!({"callee":{"expression":callee,"mode":"expression"}});
    if let Some(summary) = &summary {
        routing["receiver"] = json!({"nominal-class":summary["nominal-class"]});
    }
    let mut claimed = false;
    for (index, plugin) in runtime.plugins.iter().enumerate() {
        let translated = (!plugin.inverse_virtual_names.is_empty()).then(|| {
            let mut value = routing.clone();
            rewrite_protocol_names(&mut value, &plugin.inverse_virtual_names);
            value
        });
        let routed = translated.as_ref().unwrap_or(&routing);
        let matches = runtime.allowed(source, kind, routed, &plugin.manifest);
        if matches {
            if kind == "adjust-call-signature" {
                if !runtime.selected_signature(index, source, routed)? {
                    return Ok(PreparedCall::Response(json!({"kind":"no-change"})));
                }
                claimed = true;
                break;
            }
            if speculative
                && !early_return_method(
                    &plugin.config,
                    callee.rsplit('.').next().unwrap_or(&callee),
                )?
            {
                return Ok(PreparedCall::Response(json!({"kind":"no-change"})));
            }
            claimed = true;
        }
    }
    if !claimed {
        return Ok(PreparedCall::Response(json!({"kind":"no-change"})));
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
    if speculative {
        value["context"] = json!({"speculative":true});
    }
    Ok(PreparedCall::Request(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefetched_hooks_preserve_serial_responses_keys_and_failures() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-prefetched-calls-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let manifest = directory.join("plugin.json");
        let artifact = directory.join("plugin.py");
        let source_path = directory.join("demo.py");
        std::fs::write(
            &source_path,
            "class Row: pass\nclass Manager:\n    def get(self, *, key: int) -> Row: ...\n",
        )?;
        std::fs::write(&manifest, json!({"id":"prefetch","name":"Prefetch","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"project-index":true,"project-index-lookup":true,"call-return":true,"call-state":true},"claims":{"methods":[{"kind":"exact","class-qualified-name":"demo.Manager","method-name":"get"}]},"default-config":{"companion-native-fields":true,"companion-parallel-calls":true,"companion-early-call-returns":["get"]}}).to_string())?;
        std::fs::write(
            &artifact,
            r#"
counter = 0
@on_project_index
def index(request):
    return project_index({'annotation': request['context']['root']})
@on_call_return
def returns(request):
    global counter
    counter += 1
    assert counter == 1
    if request['arguments'][0]['value']['value'] == -1:
        return {'kind': 'malformed-response'}
    return call_return_patch(type_annotation(project_index_value(request, ['annotation'])))
@on_call_state
def state(request):
    return call_state_patch(result_members={'value': type_annotation(project_index_value(request, ['annotation']))}, fresh_result=True)
"#,
        )?;
        let mut serial = Runtime::new(&manifest, &artifact)?;
        let mut parallel = Runtime::new(&manifest, &artifact)?;
        serial.use_site_fields = true;
        parallel.use_site_fields = true;
        let project = Arc::new(Project::load(
            &directory,
            std::slice::from_ref(&source_path),
            &serial.manifest,
            json!({}),
        )?);
        serial.attach_project(Arc::clone(&project), json!({}));
        parallel.attach_project(Arc::clone(&project), json!({}));
        let source = &project.modules["demo"].source;
        let calls: Vec<_> = (0..16).map(|position| CallSite {
            span: Span { start: position, end: position + 1 },
            function: Span { start: position, end: position + 1 },
            callee: "demo.Manager.get".into(),
            arguments: vec![crate::model::Argument { span: Span { start: position, end: position + 1 }, name: Some("key".into()), literal: json!({"kind":"int","value": if position == 0 {-1} else {position as i64}}), kind: "keyword", inferred_type: None }],
            receiver_span: None,
            receiver_type: Some("demo.Manager".into()),
            flow_receiver_type: Some("demo.Manager".into()),
        }).collect();
        for annotation in ["str", "bytes"] {
            for runtime in [&mut serial, &mut parallel] {
                runtime.execute_project("build-project-index", json!({"context":{"root":annotation,"python-version":"3.13","platform":"linux"}}))?;
            }
            let before = parallel.executions;
            assert_eq!(parallel.prefetch_call_hooks(source, calls.iter())?, 32);
            assert_eq!(parallel.executions - before, 32);
            for call in &calls {
                for (kind, speculative) in
                    [("adjust-call-return", true), ("adjust-call-state", false)]
                {
                    let expected = call_request_with_speculation(
                        &mut serial,
                        source,
                        kind,
                        call,
                        None,
                        None,
                        speculative,
                    )?;
                    let actual = call_request_with_speculation(
                        &mut parallel,
                        source,
                        kind,
                        call,
                        None,
                        None,
                        speculative,
                    )?;
                    assert_eq!(actual, expected);
                }
            }
            // One malformed return is retried normally; every successful
            // prefetched request must match its real invocation's exact key.
            assert_eq!(parallel.executions - before, 33);
            let mut changed = calls[1].clone();
            changed.arguments[0].literal = json!({"kind":"str","value":"changed"});
            let before = parallel.executions;
            assert_eq!(
                call_request(
                    &mut parallel,
                    source,
                    "adjust-call-state",
                    &changed,
                    None,
                    None
                )?,
                call_request(
                    &mut serial,
                    source,
                    "adjust-call-state",
                    &changed,
                    None,
                    None
                )?
            );
            assert_eq!(parallel.executions, before + 1);
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn stateless_prefetched_batches_preserve_serial_keys_failures_and_revisions() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "ty-stateless-prefetched-calls-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory)?;
        let manifest = directory.join("plugin.json");
        let artifact = directory.join("plugin.py");
        let source_path = directory.join("demo.py");
        std::fs::write(
            &source_path,
            "class Row: pass\nclass Manager:\n    def get(self, *, key: int) -> Row: ...\n",
        )?;
        std::fs::write(&manifest, json!({"id":"prefetch","name":"Prefetch","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"project-index":true,"project-index-lookup":true,"call-return":true,"call-state":true},"claims":{"methods":[{"kind":"exact","class-qualified-name":"demo.Manager","method-name":"get"}]},"default-config":{"companion-native-fields":true,"companion-parallel-calls":true,"companion-stateless-call-batches":true,"companion-early-call-returns":["get"]}}).to_string())?;
        std::fs::write(
            &artifact,
            r#"
@on_project_index
def index(request):
    return project_index({'annotation': request['context']['root']})
@on_call_return
def returns(request):
    if request['arguments'][0]['value']['kind'] == 'str':
        assert request['arguments'][0]['value']['value'] == 'café😀\n"quoted"'
    if request['arguments'][0]['value']['value'] == -1:
        return {'kind': 'malformed-response'}
    return call_return_patch(type_annotation(project_index_value(request, ['annotation'])))
@on_call_state
def state(request):
    return call_state_patch(result_members={'value': type_annotation(project_index_value(request, ['annotation']))}, fresh_result=True)
"#,
        )?;
        let mut serial = Runtime::new(&manifest, &artifact)?;
        let mut parallel = Runtime::new(&manifest, &artifact)?;
        serial.use_site_fields = true;
        parallel.use_site_fields = true;
        let project = Arc::new(Project::load(
            &directory,
            std::slice::from_ref(&source_path),
            &serial.manifest,
            json!({}),
        )?);
        serial.attach_project(Arc::clone(&project), json!({}));
        parallel.attach_project(Arc::clone(&project), json!({}));
        let source = &project.modules["demo"].source;
        let mut calls: Vec<_> = (0..16).map(|position| CallSite {
            span: Span { start: position, end: position + 1 },
            function: Span { start: position, end: position + 1 },
            callee: "demo.Manager.get".into(),
            arguments: vec![crate::model::Argument { span: Span { start: position, end: position + 1 }, name: Some("key".into()), literal: json!({"kind":"int","value": if position == 0 {-1} else {position as i64}}), kind: "keyword", inferred_type: None }],
            receiver_span: None,
            receiver_type: Some("demo.Manager".into()),
            flow_receiver_type: Some("demo.Manager".into()),
        }).collect();
        calls.last_mut().unwrap().arguments[0].literal =
            json!({"kind":"str", "value":"café😀\n\"quoted\""});
        for annotation in ["str", "bytes"] {
            for runtime in [&mut serial, &mut parallel] {
                runtime.execute_project("build-project-index", json!({"context":{"root":annotation,"python-version":"3.13","platform":"linux"}}))?;
            }
            let before = parallel.executions;
            assert_eq!(parallel.prefetch_call_hooks(source, calls.iter())?, 32);
            assert_eq!(parallel.executions - before, 32);
            for call in &calls {
                for (kind, speculative) in
                    [("adjust-call-return", true), ("adjust-call-state", false)]
                {
                    let expected = call_request_with_speculation(
                        &mut serial,
                        source,
                        kind,
                        call,
                        None,
                        None,
                        speculative,
                    )?;
                    let actual = call_request_with_speculation(
                        &mut parallel,
                        source,
                        kind,
                        call,
                        None,
                        None,
                        speculative,
                    )?;
                    assert_eq!(actual, expected);
                }
            }
            // One malformed reply discards its whole speculative group. Every
            // real request still matches serial execution and its complete key.
            assert_eq!(parallel.executions - before, 48);
            assert!(parallel.batch_dispatches >= 2);
            assert!(parallel.batch_requests >= 32);
            let mut changed = calls[1].clone();
            changed.arguments[0].literal = json!({"kind":"str","value":"changed"});
            let before = parallel.executions;
            assert_eq!(
                call_request(
                    &mut parallel,
                    source,
                    "adjust-call-state",
                    &changed,
                    None,
                    None
                )?,
                call_request(
                    &mut serial,
                    source,
                    "adjust-call-state",
                    &changed,
                    None,
                    None
                )?
            );
            assert_eq!(parallel.executions, before + 1);
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn signature_method_selection_preserves_first_owner_functions_constructors_and_errors()
    -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-signature-selection-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let mut paths = Vec::new();
        for (id, annotation) in [("a", "str"), ("b", "bool")] {
            let manifest = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&manifest,json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"call-signature":true,"call-state":true},"claims":{"functions":[{"qualified-name":"demo.free"}],"constructors":[{"kind":"exact","qualified-name":"demo.Row"}],"methods":[{"kind":"exact","class-qualified-name":"demo.Manager","method-name":"get"},{"kind":"exact","class-qualified-name":"demo.Manager","method-name":"from_queryset"}]}}).to_string())?;
            std::fs::write(
                &artifact,
                format!(
                    "@on_call_signature\ndef signature(request):\n    return call_signature_patch(callable_signature(return_type=type_annotation('{annotation}')))\n"
                ),
            )?;
            paths.push((manifest, artifact, json!({})));
        }
        let mut runtime = Runtime::from_plugins(&paths)?;
        runtime.use_site_fields = true;
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        let get = json!({"callee":typ("demo.Manager.get"),"receiver":{"nominal-class":"demo.Manager","type-expr":typ("demo.Manager")}});
        let baseline = runtime.execute(&source, "adjust-call-signature", get.clone())?;
        assert_eq!(baseline["signature"]["return-type"]["expression"], "str");
        runtime.plugins[0].config["companion-call-signature-methods"] = json!(["from_queryset"]);
        let executions = runtime.executions;
        assert_eq!(
            runtime.execute(&source, "adjust-call-signature", get.clone())?["kind"],
            "no-change"
        );
        assert_eq!(
            runtime.executions, executions,
            "first owner's abstention cannot invoke a later plugin"
        );
        let mut selected = get.clone();
        selected["callee"] = typ("demo.Manager.from_queryset");
        assert_eq!(
            runtime.execute(&source, "adjust-call-signature", selected)?["signature"]["return-type"]
                ["expression"],
            "str"
        );
        for payload in [
            json!({"callee":typ("demo.free")}),
            json!({"callee":typ("demo.free"),"receiver":{"nominal-class":"demo.Manager","type-expr":typ("demo.Manager")}}),
            json!({"callee":typ("demo.Row")}),
        ] {
            assert_eq!(
                runtime.execute(&source, "adjust-call-signature", payload)?["signature"]["return-type"]
                    ["expression"],
                "str"
            );
        }
        for malformed in [json!("get"), json!([""]), json!([3]), json!(null)] {
            runtime.plugins[0].config["companion-call-signature-methods"] = malformed;
            assert!(
                runtime
                    .execute(&source, "adjust-call-signature", get.clone())
                    .is_err()
            );
        }
        runtime.plugins[0].config["companion-call-signature-methods"] = json!(["from_queryset"]);
        runtime
            .load_failures
            .insert("a".into(), ("load failed".into(), "reload".into()));
        let failed = runtime.execute(&source, "adjust-call-signature", get.clone())?;
        assert_eq!(failed["kind"], "no-change");
        assert_eq!(failed["diagnostics"].as_array().map(Vec::len), Some(1));
        runtime.load_failures.clear();
        runtime.plugins[0].manifest["claims"]["methods"] = json!([]);
        assert_eq!(
            runtime.execute(&source, "adjust-call-signature", get)?["signature"]["return-type"]["expression"],
            "bool"
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn consumed_project_indexes_preserve_hooks_scopes_namespaces_and_revisions() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-consumed-index-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let source_path = directory.join("demo.py");
        std::fs::write(
            &source_path,
            "from typing import ClassVar\nsettings = {'modes': (1, True, None)}\ndef register(*args, **kwargs): ...\nclass Base[T]: pass\n@register('row', enabled=True)\nclass Row(Base[str]):\n    value: ClassVar[int] = 3\n    field = register(default={'nested': [None, 'text']})\n    class Meta:\n        abstract = True\n    def method(self, value: str | None = None, *, count: int = 1) -> tuple[str, int]:\n        return ('text', count)\n",
        )?;
        let mut paths = Vec::new();
        for (id, annotation) in [("a", "bool"), ("b", "str")] {
            let manifest = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&manifest, json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"project-index":true,"class-transform":true,"instance-member":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Row"},{"kind":"exact","qualified-name":"demo.Missing"}],"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"demo.Row","attribute-name":"dynamic"}]}}).to_string())?;
            std::fs::write(
                &artifact,
                r#"
@on_project_index
def index(request):
    return project_index(
        {'annotation': request['context']['root'], 'opaque': {'qualified-name': 'virtual.Record'}},
        class_indexes={'demo.Row': {'annotation': request['context']['config']['annotation']}},
        virtual_types=[virtual_type('virtual.Record', virtual_class(members=[member('value', type_annotation('int'))]))],
    )
@on_class_transform
def transform(request):
    return class_patch(fields=[field(request['context']['config']['id'], type_annotation(request['project-index']['annotation']))])
@on_instance_member
def dynamic(request):
    assert request['project-index']['opaque'] == {'qualified-name': 'virtual.Record'}
    return member_response(member('dynamic', type_annotation(request['project-index']['annotation'])))
"#,
            )?;
            paths.push((manifest, artifact, json!({"id":id,"annotation":annotation})));
        }
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        for count in [1, 2] {
            let mut ordinary = Runtime::from_plugins(&paths[..count])?;
            let mut consumed = Runtime::from_plugins(&paths[..count])?;
            let project = Project::load(
                &directory,
                std::slice::from_ref(&source_path),
                &ordinary.manifest,
                json!({}),
            )?;
            for annotation in ["int", "bytes", "str"] {
                let mut request = project.build_request();
                request["context"]["root"] = json!(annotation);
                request["context"]["python-version"] = json!("3.13");
                request["context"]["platform"] = json!("darwin");
                assert!(!request["classes"].as_array().unwrap().is_empty());
                assert!(!request["functions"].as_array().unwrap().is_empty());
                let mut expected =
                    ordinary.execute_project("build-project-index", request.clone())?;
                let actual = consumed.execute_ephemeral_project_index(request)?;
                if let Some(object) = expected.as_object_mut() {
                    object.remove("plugin-index");
                    object.remove("class-indexes");
                }
                assert_eq!(actual, expected);
                assert_eq!(
                    consumed.virtual_display_names(),
                    ordinary.virtual_display_names()
                );
                for name in ["demo.Row", "demo.Missing"] {
                    let payload = json!({"class":{"qualified-name":name}});
                    let expected = ordinary.execute(&source, "analyze-class", payload.clone())?;
                    let actual = consumed.execute(&source, "analyze-class", payload)?;
                    assert_eq!(actual, expected);
                    for field in actual["fields"].as_array().unwrap() {
                        let ty = if name == "demo.Row" {
                            if field["name"] == "a" { "bool" } else { "str" }
                        } else {
                            annotation
                        };
                        assert_eq!(field["instance-get-type"]["expression"], ty);
                    }
                }
                let payload = json!({"owner":typ("demo.Row"),"member-name":"dynamic"});
                let expected =
                    ordinary.execute(&source, "resolve-instance-member", payload.clone())?;
                let actual = consumed.execute(&source, "resolve-instance-member", payload)?;
                assert_eq!(actual, expected);
                assert_eq!(actual["access"]["type-expr"]["expression"], annotation);
            }
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn native_cli_requires_native_fields_unanimous_opt_in_and_boolean_config() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-native-cli-config-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let artifact = directory.join("plugin.py");
        std::fs::write(&artifact, "# Overlay-only plugin.\n")?;
        let mut paths = Vec::new();
        for id in ["one", "two"] {
            let manifest = directory.join(format!("{id}.json"));
            std::fs::write(&manifest,json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"stub-overlays":true},"default-config":{"companion-native-fields":true,"companion-native-cli":true}}).to_string())?;
            paths.push((manifest, artifact.clone(), json!({})));
        }
        let mut runtime = Runtime::from_plugins(&paths)?;
        assert_eq!(runtime.prefers_native_cli(), runtime.use_site_fields);
        runtime.use_site_fields = false;
        assert!(!runtime.prefers_native_cli());
        paths[1].2 = json!({"companion-native-cli":false});
        assert!(!Runtime::from_plugins(&paths)?.prefers_native_cli());
        paths[1].2 = json!({"companion-native-cli":null});
        assert!(!Runtime::from_plugins(&paths)?.prefers_native_cli());
        paths[1].2 = json!({"companion-native-cli":"yes"});
        assert!(Runtime::from_plugins(&paths).is_err());
        assert!(!Runtime::from_plugins(&[])?.prefers_native_cli());
        paths[1].2 = json!({});
        assert!(!Runtime::from_plugins(&paths)?.prepares_project_contributions_early());
        paths[0].2 = json!({"companion-early-project-contributions":true});
        assert!(!Runtime::from_plugins(&paths)?.prepares_project_contributions_early());
        paths[1].2 = paths[0].2.clone();
        let mut early = Runtime::from_plugins(&paths)?;
        early.use_site_fields = true;
        assert!(early.prepares_project_contributions_early());
        early.use_site_fields = false;
        assert!(!early.prepares_project_contributions_early());
        for invalid in [json!("yes"), json!(0), json!([])] {
            paths[1].2 = json!({"companion-early-project-contributions":invalid});
            assert!(Runtime::from_plugins(&paths).is_err());
        }
        for disabled in [json!(false), json!(null)] {
            paths[1].2 = json!({"companion-early-project-contributions":disabled});
            assert!(!Runtime::from_plugins(&paths)?.prepares_project_contributions_early());
        }

        for invalid in [json!("yes"), json!(1), json!([])] {
            paths[1].2 = json!({"companion-stateless-call-batches":invalid});
            assert!(Runtime::from_plugins(&paths).is_err());
        }
        for valid in [json!(true), json!(false), json!(null)] {
            paths[1].2 = json!({"companion-stateless-call-batches":valid});
            assert!(Runtime::from_plugins(&paths).is_ok());
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn third_party_overlay_declarations_require_all_plugins_and_valid_names() -> Result<()> {
        assert!(declared_third_party_overlays(&json!({}))?.is_empty());
        assert!(
            declared_third_party_overlays(&json!({"companion-third-party-overlays":null}))?
                .is_empty()
        );
        for value in [
            json!(true),
            json!("django"),
            json!([1]),
            json!([""]),
            json!(["django.db"]),
            json!(["x\nimport os"]),
            json!(["42name"]),
            json!(["class"]),
        ] {
            assert!(
                declared_third_party_overlays(&json!({"companion-third-party-overlays":value}))
                    .is_err()
            );
        }
        let directory =
            std::env::temp_dir().join(format!("ty-third-party-overlays-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let artifact = directory.join("plugin.py");
        std::fs::write(&artifact, "# Overlay-only plugin.\n")?;
        let mut paths = Vec::new();
        for id in ["first", "second"] {
            let manifest = directory.join(format!("{id}.json"));
            std::fs::write(&manifest,json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"stub-overlays":true},"default-config":{"companion-third-party-overlays":["django","another"]}}).to_string())?;
            paths.push((manifest, artifact.clone(), json!({})));
        }
        assert!(
            Runtime::from_plugins(&[])?
                .third_party_overlay_modules()?
                .is_empty()
        );
        assert_eq!(
            Runtime::from_plugins(&paths)?.third_party_overlay_modules()?,
            BTreeSet::from(["django".into(), "another".into()])
        );
        paths[1].2 = json!({"companion-third-party-overlays":["django"]});
        assert_eq!(
            Runtime::from_plugins(&paths)?.third_party_overlay_modules()?,
            BTreeSet::from(["django".into()])
        );
        paths[1].2 = json!({"companion-third-party-overlays":null});
        assert!(
            Runtime::from_plugins(&paths)?
                .third_party_overlay_modules()?
                .is_empty()
        );
        paths[0].2 = json!({"companion-third-party-overlays":true});
        assert!(Runtime::from_plugins(&paths).is_err());
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn early_return_opt_in_preserves_mixed_plugins_and_missing_native_inputs() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-early-opt-in-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("demo.py");
        std::fs::write(
            &path,
            "class Item: pass\nclass Generic[T]: pass\nclass Manager:\n    def get(self, *, key: int) -> Item: raise NotImplementedError\nvalue = Manager().get(key=1)\n",
        )?;
        let mut paths = Vec::new();
        for id in ["first", "second"] {
            let manifest = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&manifest, json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"call-return":true},"claims":{"methods":[{"kind":"exact","class-qualified-name":"demo.Manager","method-name":"get"}]}}).to_string())?;
            std::fs::write(
                &artifact,
                "@on_call_return\ndef result(request):\n    return call_return_patch(type_annotation('demo.Item' if request['context']['speculative'] else 'str'))\n",
            )?;
            paths.push((
                manifest,
                artifact,
                if id == "first" {
                    json!({"companion-early-call-returns":["get"]})
                } else {
                    json!({})
                },
            ));
        }
        let mut runtime = Runtime::from_plugins(&paths)?;
        runtime.use_site_fields = true;
        let project = Project::load(&directory, &[path], &runtime.manifest, json!({}))?;
        let module = project.modules["demo"].clone();
        let ruff_python_ast::Stmt::Assign(assignment) = module.suite.last().unwrap() else {
            unreachable!()
        };
        let ruff_python_ast::Expr::Call(expression) = assignment.value.as_ref() else {
            unreachable!()
        };
        let mut call = crate::model::call_site(expression, "demo.Manager.get".into());
        call.flow_receiver_type = Some("demo.Manager".into());
        runtime.attach_project(Arc::new(project), json!({}));
        let before = runtime.executions;
        assert!(speculative_call_return(&mut runtime, &module.source, &call)?.is_none());
        assert_eq!(
            runtime.executions, before,
            "Every observing plugin must opt in"
        );
        for plugin in &mut runtime.plugins {
            plugin.config["companion-early-call-returns"] = json!(["get"]);
        }
        let response = speculative_call_return(&mut runtime, &module.source, &call)?.unwrap();
        assert_eq!(response.response["return-type"]["expression"], "demo.Item");
        assert!(!response.native);
        runtime.plugins[0].config["companion-native-call-returns"] = json!(["get"]);
        assert!(
            !speculative_call_return(&mut runtime, &module.source, &call)?
                .unwrap()
                .native
        );
        for plugin in &mut runtime.plugins {
            plugin.config["companion-native-call-returns"] = json!(["get"]);
        }
        assert!(
            speculative_call_return(&mut runtime, &module.source, &call)?
                .unwrap()
                .native
        );
        let normal = call_request(
            &mut runtime,
            &module.source,
            "adjust-call-return",
            &call,
            None,
            None,
        )?;
        assert_eq!(
            normal["return-type"]["expression"], "str",
            "Speculative cache entries must not replace normal calls"
        );
        runtime.plugins[0].config["companion-native-call-returns"] = json!("get");
        assert!(speculative_call_return(&mut runtime, &module.source, &call).is_err());
        runtime.plugins[0].config["companion-native-call-returns"] = json!(["get"]);
        let before = runtime.executions;
        call.arguments[0].literal = json!({"kind":"unknown"});
        assert!(speculative_call_return(&mut runtime, &module.source, &call)?.is_none());
        assert_eq!(runtime.executions, before);
        call.arguments[0].literal = json!({"kind":"int","value":1});
        runtime.plugins[0].config["companion-early-call-returns"] = json!("get");
        assert!(speculative_call_return(&mut runtime, &module.source, &call).is_err());
        assert_eq!(runtime.executions, before);
        assert!(
            runtime
                .project
                .as_ref()
                .unwrap()
                .plain_source_class("demo.Item")
        );
        assert!(
            !runtime
                .project
                .as_ref()
                .unwrap()
                .plain_source_class("demo.Generic")
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn project_input_selection_is_explicit_isolated_and_validated() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-input-selection-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let profile = json!({"companion-project-inputs":{"assignment-kinds":["call"],"class-source":false,"field-call-keywords":["null"],"field-call-argument-source":false}});
        let mut paths = Vec::new();
        for (id, config) in [("selected", profile), ("full", json!({}))] {
            let artifact = directory.join(format!("{id}.py"));
            let manifest = directory.join(format!("{id}.json"));
            std::fs::write(
                &artifact,
                "@on_project_index\ndef index(request):\n    return project_index({'classes':request.get('classes',[]),'assignments':request.get('assignments',[])})\n",
            )?;
            std::fs::write(
                &manifest,
                serde_json::to_vec(
                    &json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"project-index":true}}),
                )?,
            )?;
            paths.push((manifest, artifact, config));
        }
        let source = json!({"module":"demo","file-path":"demo.py","start":{"line":1,"column":1},"end":{"line":1,"column":2}});
        let mut request = json!({"kind":"build-project-index","context":{"root":"","python-version":"3.13","platform":"linux","config":{}},"classes":[{"qualified-name":"demo.Row","source":source,"fields":[{"name":"value","source":source,"assigned-value":{"kind":"call","callee":{"qualified-name":"library.Field"},"arguments":[{"kind":"positional","value":{"kind":"str","value":"Target"}},{"kind":"keyword","name":"null","value":{"kind":"bool","value":true}},{"kind":"keyword","name":"max_length","value":{"kind":"int","value":80}},{"kind":"star-kwargs","value":{"kind":"unknown"}}]}}],"nested-classes":[{"name":"Meta","qualified-name":"demo.Row.Meta","source":source}]}],"assignments":[{"name":"constant","qualified-name":"demo.constant","assigned-value":{"kind":"literal","value":{"kind":"int","value":1}}},{"name":"factory","qualified-name":"demo.factory","assigned-value":{"kind":"call","callee":{"qualified-name":"library.factory"}}}]});
        request["classes"][0]["fields"][0]["assigned-value"]["arguments"][0]["source"] =
            source.clone();
        request["classes"][0]["nested-classes"][0]["fields"] =
            request["classes"][0]["fields"].clone();
        let original = request.clone();
        let mut source_only = request.clone();
        select_project_inputs(
            &mut source_only,
            &json!({"companion-project-inputs":{"field-call-argument-source":false}}),
        )?;
        assert_eq!(source_only["classes"][0]["source"], source);
        assert_eq!(source_only["classes"][0]["fields"][0]["source"], source);
        assert_eq!(
            source_only["classes"][0]["fields"][0]["assigned-value"]["arguments"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert!(
            source_only["classes"][0]["fields"][0]["assigned-value"]["arguments"][0]
                .get("source")
                .is_none()
        );
        assert!(source_only["classes"][0]["nested-classes"][0]["fields"][0]["assigned-value"]["arguments"][0].get("source").is_none());
        let mut malformed = request.clone();
        malformed["assignments"][0]["assigned-value"]["kind"] = json!("unexpected");
        select_project_inputs(&mut malformed, &paths[0].2)?;
        assert_eq!(
            malformed["assignments"][0]["assigned-value"]["kind"],
            "unexpected"
        );
        let mut runtime = Runtime::from_plugins(&paths)?;
        runtime.use_site_fields = true;
        assert!(!runtime.sparse_overlay_index()?);
        assert!(!runtime.project_call_assignments_only()?);
        runtime.execute_project("build-project-index", request.clone())?;
        assert_eq!(request, original);
        let selected_index = runtime
            .plugins
            .iter()
            .position(|plugin| plugin.id == "selected")
            .unwrap();
        let full_index = runtime
            .plugins
            .iter()
            .position(|plugin| plugin.id == "full")
            .unwrap();
        let selected = runtime.plugins[selected_index].index.as_ref().unwrap();
        let full = runtime.plugins[full_index].index.as_ref().unwrap();
        assert_eq!(selected["assignments"].as_array().unwrap().len(), 1);
        assert_eq!(full["assignments"].as_array().unwrap().len(), 2);
        assert!(selected["classes"][0].get("source").is_none());
        assert!(
            selected["classes"][0]["nested-classes"][0]
                .get("source")
                .is_none()
        );
        assert_eq!(selected["classes"][0]["fields"][0]["source"], source);
        assert_eq!(full["classes"][0]["source"], source);
        let arguments = selected["classes"][0]["fields"][0]["assigned-value"]["arguments"]
            .as_array()
            .unwrap();
        assert_eq!(arguments.len(), 3);
        assert_eq!(arguments[0]["kind"], "positional");
        assert!(arguments[0].get("source").is_none());
        assert_eq!(
            full["classes"][0]["fields"][0]["assigned-value"]["arguments"][0]["source"],
            source
        );
        assert_eq!(arguments[1]["name"], "null");
        assert_eq!(arguments[2]["kind"], "star-kwargs");
        runtime.use_site_fields = false;
        runtime.execute_project("build-project-index", request.clone())?;
        assert_eq!(
            runtime.plugins[0].index, runtime.plugins[1].index,
            "Default SDK requests remain complete"
        );
        runtime.use_site_fields = true;
        for invalid in [
            json!({"unknown":true}),
            json!({"class-source":"false"}),
            json!({"assignment-kinds":["bogus"]}),
            json!({"field-call-keywords":[1]}),
            json!({"field-call-argument-source":"false"}),
            json!({"overlay-index":true}),
            json!({"overlay-index":"bogus"}),
        ] {
            runtime.plugins[selected_index].config["companion-project-inputs"] = invalid;
            let executions = runtime.executions;
            assert!(
                runtime
                    .execute_project("build-project-index", request.clone())
                    .is_err()
            );
            assert_eq!(
                runtime.executions, executions,
                "Reject malformed input selection before invoking a plugin"
            );
        }
        for plugin in &mut runtime.plugins {
            plugin.config["companion-project-inputs"] =
                json!({"overlay-index":"imports", "assignment-kinds":["call"]});
        }
        assert!(runtime.sparse_overlay_index()?);
        assert!(runtime.project_call_assignments_only()?);
        runtime.plugins[full_index].manifest["capabilities"]["additional-dependencies"] =
            json!(true);
        assert!(!runtime.project_call_assignments_only()?);
        runtime.plugins[full_index].manifest["capabilities"]["additional-dependencies"] =
            json!(false);
        runtime.plugins[full_index].config = json!({});
        assert!(!runtime.sparse_overlay_index()?);
        assert!(!runtime.project_call_assignments_only()?);
        runtime.plugins[full_index].config =
            json!({"companion-project-inputs":{"overlay-index":"imports"}});
        runtime.use_site_fields = false;
        assert!(!runtime.sparse_overlay_index()?);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn class_batches_preserve_individual_results_limits_fallback_and_revisions() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-class-batch-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let source_path = directory.join("demo.py");
        let mut source = "class Base: pass\nclass Fallback(Base): pass\nclass Huge(Base): pass\nclass Ignored: pass\n".to_owned();
        for index in 0..260 {
            source.push_str(&format!("class C{index:03}(Base): pass\n"));
        }
        std::fs::write(&source_path, source)?;
        let manifest = json!({"id":"batch","name":"batch","version":"0.0.0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true,"class-batch":true,"project-index":true},"claims":{"classes":[{"kind":"subclass-of","base-qualified-name":"demo.Base"}]}});
        let manifest_path = directory.join("plugin.json");
        let artifact = directory.join("plugin.py");
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest)?)?;
        std::fs::write(
            &artifact,
            r#"
_seen = []
@on_project_index
def index(request):
    annotation = request['context']['root']
    scopes = {item['qualified-name']: {'annotation': annotation, 'opaque': {'qualified-name': 'opaque.Name'}} for item in request['classes'] if item['qualified-name'] != 'demo.Fallback'}
    if annotation == 'bytes':
        for name in scopes:
            if name.startswith('demo.C0'): scopes[name]['padding'] = 'x' * (32 * 1024)
    scopes['demo.Huge']['large'] = 'x' * (1024 * 1024)
    return project_index({'annotation': annotation}, class_indexes=scopes)
@on_class_transform
def transform(request):
    assert request['kind'] == 'analyze-class'
    assert request['context']['module'] == 'demo'
    assert request['context']['file-path'].endswith('demo.py')
    index = request['project-index']
    if 'opaque' in index: assert index['opaque'] == {'qualified-name': 'opaque.Name'}
    if request['context']['config']['mode'] == 'mixed':
        if request['class']['qualified-name'] == 'demo.C000': return None
        if request['class']['qualified-name'] == 'demo.C001': return error('individual abstention')
    return class_patch(fields=[field(request['class']['qualified-name'].split('.')[-1], type_annotation(index['annotation']))])
@on_class_batch
def batch(request):
    assert not _seen
    _seen.append(1)
    assert 0 < len(request['requests']) <= 128
    assert len(json.dumps(request, separators=(',', ':'))) <= 1024 * 1024
    assert all(item['class']['qualified-name'] != 'demo.Ignored' for item in request['requests'])
    assert all(item['class']['qualified-name'] != 'demo.Huge' for item in request['requests'])
    mode = request['requests'][0]['context']['config']['mode']
    if mode == 'abstain': return None
    if mode == 'error': raise ValueError('batch failure')
    if mode == 'invalid-item': return {'kind': 'class-batch', 'responses': [{'kind': 'project-index'}]}
    results = [transform(item) for item in request['requests']]
    if mode == 'wrong-count': results.pop()
    return class_batch(results)
"#,
        )?;
        let project = Project::load(
            &directory,
            std::slice::from_ref(&source_path),
            &manifest,
            json!({}),
        )?;
        let source = &project.modules["demo"].source;
        let mut runtime =
            Runtime::from_plugins(&[(manifest_path, artifact, json!({"mode":"ok"}))])?;
        runtime.attach_project(Arc::new(project.clone()), json!({}));
        for annotation in ["int", "bytes", "str"] {
            let mut request = project.build_request_for_files(None);
            request["context"]["root"] = json!(annotation);
            runtime.execute_project("build-project-index", request)?;
            let before = runtime.executions;
            runtime.prepare_class_transforms(&project)?;
            // Base itself also matches the subclass claim and has a scope.
            if annotation == "bytes" {
                assert!(
                    runtime.executions - before > 3,
                    "byte limits must split batches before the item limit"
                );
            } else {
                assert_eq!(runtime.executions - before, 3);
            }
            let prepared = runtime.executions;
            let mut batched = Vec::new();
            for index in 0..260 {
                let name = format!("demo.C{index:03}");
                let response = runtime.execute(
                    source,
                    "analyze-class",
                    json!({"class":project.classes[&name]}),
                )?;
                assert_eq!(response["fields"][0]["name"], format!("C{index:03}"));
                assert_eq!(
                    response["fields"][0]["instance-get-type"]["expression"],
                    annotation
                );
                batched.push(response);
            }
            assert_eq!(
                runtime.executions, prepared,
                "every batched response must populate the ordinary request cache"
            );
            for name in ["demo.Huge", "demo.Fallback"] {
                let response = runtime.execute(
                    source,
                    "analyze-class",
                    json!({"class":project.classes[name]}),
                )?;
                assert_eq!(
                    response["fields"][0]["instance-get-type"]["expression"],
                    annotation
                );
            }
            assert_eq!(runtime.executions, prepared + 2);
            assert_eq!(
                runtime.execute(
                    source,
                    "analyze-class",
                    json!({"class":project.classes["demo.Ignored"]})
                )?["kind"],
                "no-change"
            );
            runtime.plugins[0].manifest["capabilities"]["class-batch"] = json!(false);
            runtime.cache.clear();
            let individual = runtime.executions;
            runtime.prepare_class_transforms(&project)?;
            assert_eq!(
                runtime.executions, individual,
                "legacy plugins cannot be batched implicitly"
            );
            for (index, expected) in batched.into_iter().enumerate() {
                let name = format!("demo.C{index:03}");
                assert_eq!(
                    runtime.execute(
                        source,
                        "analyze-class",
                        json!({"class":project.classes[&name]})
                    )?,
                    expected
                );
            }
            runtime.plugins[0].manifest["capabilities"]["class-batch"] = json!(true);
        }
        for mode in ["abstain", "error", "invalid-item", "wrong-count"] {
            runtime.plugins[0].config["mode"] = json!(mode);
            runtime.prepare_class_transforms(&project)?;
            let before = runtime.executions;
            let response = runtime.execute(
                source,
                "analyze-class",
                json!({"class":project.classes["demo.C000"]}),
            )?;
            assert_eq!(
                response["fields"][0]["instance-get-type"]["expression"], "str",
                "{mode}"
            );
            assert_eq!(
                runtime.executions,
                before + 1,
                "an invalid batch must leave no partial cached answers: {mode}"
            );
        }
        runtime.plugins[0].config["mode"] = json!("mixed");
        runtime.prepare_class_transforms(&project)?;
        let mut batched = Vec::new();
        for name in ["demo.C000", "demo.C001", "demo.C002"] {
            batched.push(runtime.execute(
                source,
                "analyze-class",
                json!({"class":project.classes[name]}),
            )?);
        }
        assert_eq!(batched[0]["kind"], "no-change");
        assert_eq!(batched[1]["kind"], "no-change");
        assert_eq!(batched[2]["kind"], "class-patch");
        runtime.plugins[0].manifest["capabilities"]["class-batch"] = json!(false);
        runtime.cache.clear();
        for (name, expected) in ["demo.C000", "demo.C001", "demo.C002"]
            .into_iter()
            .zip(batched)
        {
            assert_eq!(
                runtime.execute(
                    source,
                    "analyze-class",
                    json!({"class":project.classes[name]})
                )?,
                expected
            );
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn lazy_indexes_preserve_plugin_revision_and_invocation_isolation() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-lazy-index-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let mut paths = Vec::new();
        for id in ["a", "b"] {
            let manifest = json!({"id":id,"name":id,"version":"0.0.0","protocol-version":{"major":0,"minor":8},"ty-compatibility":{"requirement":">=0.87.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"class-transform":true,"project-index":true,"project-index-lookup":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Row"}]}});
            let path = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
            std::fs::write(
                &artifact,
                r#"
_seen = []
@on_project_index
def index(request):
    return project_index({'items': {'record': {'type': request['context']['root'], 'owner': request['context']['config']['id']}}, 'null': None, 'a/b': {'qualified-name': 'opaque.Row'}, 'unused': 'x' * 1000000})
@on_class_transform
def transform(request):
    _seen.append(1)
    assert len(_seen) == 1
    assert request.get('project-index') is None
    value = project_index_value(request, ['items', 'record'])
    annotation = value['type']
    assert value['owner'] == request['context']['config']['id']
    value['type'] = 'local mutation'
    assert project_index_value(request, ['items', 'record'])['type'] == annotation
    assert project_index_keys(request, ['items']) == ['record']
    assert project_index_keys(request, ['null']) == []
    assert project_index_value(request, ['null'], 'missing') is None
    assert project_index_value(request, ['absent'], 'missing') == 'missing'
    assert project_index_value(request, ['a/b']) == {'qualified-name': 'opaque.Row'}
    return class_patch(fields=[field(value['owner'], type_annotation(annotation))])
"#,
            )?;
            paths.push((path, artifact, json!({"id":id})));
        }
        let mut runtime = Runtime::from_plugins(&paths)?;
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        for annotation in ["int", "str", "bytes"] {
            runtime.execute_project(
                "build-project-index",
                json!({"context":{"root":annotation,"python-version":"3.13","platform":"darwin"}}),
            )?;
            let response = runtime.execute(
                &source,
                "analyze-class",
                json!({"class":{"qualified-name":"demo.Row"}}),
            )?;
            assert_eq!(
                response["fields"].as_array().map(Vec::len),
                Some(2),
                "{response:#}"
            );
            for field in response["fields"].as_array().unwrap() {
                assert_eq!(field["instance-get-type"]["expression"], annotation);
            }
            let executions = runtime.executions;
            assert_eq!(
                runtime.execute(
                    &source,
                    "analyze-class",
                    json!({"class":{"qualified-name":"demo.Row"}})
                )?,
                response
            );
            assert_eq!(runtime.executions, executions);
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn class_indexes_preserve_fallback_isolation_and_revision_invalidation() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-class-indexes-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let mut paths = Vec::new();
        for (id, full, scoped) in [("a", "int", ""), ("b", "float", "bool")] {
            let manifest = json!({"id":id,"name":id,"version":"0.0.0","protocol-version":{"major":0,"minor":8},"ty-compatibility":{"requirement":">=0.87.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"class-transform":true,"project-index":true,"instance-member":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Row"},{"kind":"exact","qualified-name":"demo.Missing"},{"kind":"exact","qualified-name":"demo.Null"}],"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"demo.Row","attribute-name":"dynamic"}]}});
            let path = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
            std::fs::write(
                &artifact,
                r#"
@on_project_index
def index(request):
    root = request['context']['root']
    config = request['context']['config']
    if root == 'abstain': return None
    scopes = {} if root == 'removed' else {
        'demo.Row': {'annotation': config['scoped'] or root, 'opaque': {'qualified-name': 'virtual.Record'}},
        'demo.Null': None,
    }
    return project_index({'annotation': config['full']}, class_indexes=scopes)
@on_class_transform
def transform(request):
    index = request.get('project-index')
    annotation = 'None' if index is None else index['annotation']
    if index is not None:
        if 'opaque' in index:
            assert index['opaque'] == {'qualified-name': 'virtual.Record'}
        index['annotation'] = 'changed only inside this invocation'
    return class_patch(fields=[field(request['context']['config']['id'], type_annotation(annotation))])
@on_instance_member
def member_hook(request):
    return member_response(member('dynamic', type_annotation(request['project-index']['annotation'])))
"#,
            )?;
            paths.push((path, artifact, json!({"id":id,"full":full,"scoped":scoped})));
        }
        let mut runtime = Runtime::from_plugins(&paths)?;
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        let index_request = |root: &str| json!({"context":{"root":root,"python-version":"3.13","platform":"darwin"}});
        let class_request = |name: &str| json!({"class":{"qualified-name":name}});
        let fields = |response: Value| {
            response["fields"]
                .as_array()
                .unwrap()
                .iter()
                .map(|field| {
                    (
                        field["name"].as_str().unwrap().to_owned(),
                        field["instance-get-type"]["expression"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        runtime.execute_project("build-project-index", index_request("str"))?;
        assert_eq!(
            fields(runtime.execute(&source, "analyze-class", class_request("demo.Row"))?),
            BTreeMap::from([("a".into(), "str".into()), ("b".into(), "bool".into())])
        );
        let executions = runtime.executions;
        runtime.execute(&source, "analyze-class", class_request("demo.Row"))?;
        assert_eq!(
            runtime.executions, executions,
            "equal scoped requests stay cached"
        );
        assert_eq!(
            runtime.plugins[0].class_indexes["demo.Row"]["annotation"],
            "str"
        );
        assert_eq!(
            fields(runtime.execute(&source, "analyze-class", class_request("demo.Missing"))?),
            BTreeMap::from([("a".into(), "int".into()), ("b".into(), "float".into())])
        );
        assert!(
            fields(runtime.execute(&source, "analyze-class", class_request("demo.Null"))?)
                .values()
                .all(|annotation| annotation == "None")
        );
        let member = runtime.execute(
            &source,
            "resolve-instance-member",
            json!({"owner":typ("demo.Row"),"member-name":"dynamic"}),
        )?;
        assert_eq!(
            member["access"]["type-expr"]["expression"], "int",
            "other hooks retain the complete index"
        );
        let generation = runtime.plugins[0].index_generation;
        runtime.execute_project("build-project-index", index_request("bytes"))?;
        assert!(runtime.plugins[0].index_generation > generation);
        assert_eq!(
            fields(runtime.execute(&source, "analyze-class", class_request("demo.Row"))?)["a"],
            "bytes"
        );
        runtime.execute_project("build-project-index", index_request("removed"))?;
        assert_eq!(
            fields(runtime.execute(&source, "analyze-class", class_request("demo.Row"))?)["a"],
            "int"
        );
        runtime.execute_project("build-project-index", index_request("abstain"))?;
        assert!(
            fields(runtime.execute(&source, "analyze-class", class_request("demo.Row"))?)
                .values()
                .all(|annotation| annotation == "None")
        );
        assert!(
            runtime
                .plugins
                .iter()
                .all(|plugin| plugin.class_indexes.is_empty())
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn moved_response_indexes_preserve_exact_protocol_json() -> Result<()> {
        for index in [
            Value::Null,
            json!(true),
            json!(42),
            json!(1.5),
            json!("é\n"),
            json!([]),
            json!({"nested":[{"a/b~c":[null,false,1,"value"]}]}),
        ] {
            for classes in [
                BTreeMap::new(),
                BTreeMap::from([
                    ("demo.Row".to_owned(), index.clone()),
                    ("demo.Empty".to_owned(), Value::Null),
                ]),
            ] {
                let response = ty_plugin_protocol::PluginResponse::ProjectIndex(
                    ty_plugin_protocol::ProjectIndexResponse {
                        plugin_index: index.clone(),
                        class_indexes: classes,
                        contributions: Vec::new(),
                        virtual_types: Vec::new(),
                        dependencies: Vec::new(),
                        diagnostics: Vec::new(),
                    },
                );
                let reference = serde_json::to_value(&response)?;
                assert_eq!(response_value(response)?, reference);
            }
        }
        for value in [
            json!({"kind":"no-change"}),
            json!({"kind":"class-patch","fields":[]}),
            json!({"kind":"call-return-patch","return-type":typ("int")}),
        ] {
            let response: ty_plugin_protocol::PluginResponse = serde_json::from_value(value)?;
            let reference = serde_json::to_value(&response)?;
            assert_eq!(response_value(response)?, reference);
        }
        Ok(())
    }

    #[test]
    fn prepared_member_abstentions_preserve_routing_namespaces_and_failures() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "ty-native-member-abstentions-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory)?;
        let mut paths = Vec::new();
        for (id, result) in [("a.first", "str"), ("b.second", "int")] {
            let manifest = directory.join(format!("{id}.json"));
            let artifact = directory.join(format!("{id}.py"));
            std::fs::write(
                &artifact,
                format!(
                    "@on_instance_member\ndef instance(request):\n    return member_response(member(request['member-name'], type_annotation('{result}'), mode='replace-existing'))\n"
                ),
            )?;
            let claims: Vec<_> = ["value", "other"].into_iter().map(|member|json!({"kind":"exact","scope":"instance","owner-qualified-name":"demo.Row","attribute-name":member})).collect();
            std::fs::write(&manifest,json!({"id":id,"name":id,"version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"instance-member":true},"claims":{"attributes":claims},"default-config":{"companion-native-member-abstentions":"native"}}).to_string())?;
            paths.push((manifest, artifact, json!({})));
        }
        let source = Source::new(String::new(), "demo.py".into(), "demo".into());
        let request = json!({"owner":typ("demo.Row"),"member-name":"value"});
        let declaration = json!({"native":{"demo.Row":{"value":true,"other":"true"}}});
        let mut runtime = Runtime::from_plugins(&paths)?;
        runtime.use_site_fields = true;
        runtime.plugins[0].index = Some(declaration.clone());
        assert_eq!(
            runtime.execute(&source, "resolve-instance-member", request.clone())?["access"]["type-expr"]
                ["expression"],
            "int"
        );
        assert_eq!(
            runtime.executions, 1,
            "only the later plugin should execute"
        );
        assert_eq!(
            runtime.execute(
                &source,
                "resolve-instance-member",
                json!({"owner":typ("demo.Row"),"member-name":"other"})
            )?["access"]["type-expr"]["expression"],
            "str"
        );
        assert!(!runtime.native_member_abstains(0, "resolve-class-member", &request));
        assert!(!runtime.native_member_abstains(0, "adjust-call-state", &request));
        runtime.use_site_fields = false;
        assert_eq!(
            runtime.execute(&source, "resolve-instance-member", request.clone())?["access"]["type-expr"]
                ["expression"],
            "str"
        );
        runtime.use_site_fields = true;
        runtime.plugins[0].index = None;
        assert!(!runtime.native_member_abstains(0, "resolve-instance-member", &request));

        let mut isolated = Runtime::from_plugins(&paths[..1])?;
        isolated.use_site_fields = true;
        isolated.plugins[0].index = Some(declaration);
        isolated.plugins[0]
            .virtual_names
            .insert("demo.Row".into(), "_ty_plugin_test.Row".into());
        isolated.plugins[0]
            .inverse_virtual_names
            .insert("_ty_plugin_test.Row".into(), "demo.Row".into());
        assert_eq!(
            isolated.execute(
                &source,
                "resolve-instance-member",
                json!({"owner":typ("_ty_plugin_test.Row"),"member-name":"value"})
            )?["kind"],
            "no-change"
        );
        assert_eq!(isolated.executions, 0);
        isolated.load_failures.insert(
            "a.first".into(),
            ("broken artifact".into(), "reload".into()),
        );
        assert!(!isolated.native_member_abstains(0, "resolve-instance-member", &request));
        isolated.execute(&source, "resolve-instance-member", request)?;
        for invalid in [json!(false), json!([]), json!("")] {
            let mut malformed = paths[..1].to_vec();
            malformed[0].2 = json!({"companion-native-member-abstentions":invalid});
            assert!(Runtime::from_plugins(&malformed).is_err());
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn buffered_request_keys_preserve_exact_json_bytes() -> Result<()> {
        for size in [0, 1, 8191, 8192, 8193, 32768] {
            let payload = json!({"unicode":"é文\n\"\\", "values":[true, null, -1, 1.5], "large":"x".repeat(size)});
            let expected: RequestKey = Sha256::digest(serde_json::to_vec(&payload)?).into();
            assert_eq!(request_key(&payload)?, expected);
            let mut changed = payload.clone();
            changed["values"][0] = json!(false);
            assert_ne!(request_key(&changed)?, expected);
        }
        Ok(())
    }

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
        let observed = runtime.protocol_observations();
        assert_eq!(runtime.qualified_type_expr("str"), value);
        assert!(
            runtime.protocol_observations() > observed,
            "cached reads must remain dependencies"
        );

        let record = BTreeMap::from([("Alias".into(), runtime.qualified_type_expr("str"))]);
        let independent = runtime.projection_context(false);
        let dependent = runtime.projection_context(true);
        runtime.register_protocol_types("models", "static_", record.clone());
        assert_eq!(runtime.projection_context(false), independent);
        assert_ne!(runtime.projection_context(true), dependent);
        runtime.set_project_patch("models.Model".into(), json!({"kind":"no-change"}));
        assert_ne!(runtime.projection_context(false), independent);
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
        runtime.execution_profile = Some(BTreeMap::new());
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
        assert_eq!(
            runtime.execution_profile()["adjust-call-return"]["executions"],
            1
        );
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
            cache.insert(request_key(&index).unwrap(), &json!(index));
        }
        assert_eq!(cache.get(&first), Some(&json!(0)));
        cache.insert(
            request_key(&RequestCache::MAX_ENTRIES).unwrap(),
            &json!("new"),
        );
        assert!(cache.get(&second).is_none());
        assert!(cache.get(&first).is_some());
        assert_eq!(cache.entries.len(), RequestCache::MAX_ENTRIES);
        assert_eq!(cache.entries.len(), cache.oldest.len());
        assert!(cache.bytes <= RequestCache::MAX_BYTES);
    }

    #[test]
    fn large_index_responses_do_not_evict_reusable_semantic_results() {
        let mut cache = RequestCache::default();
        // A sequential class scan nearly fills the shared budget. A new large
        // index used to evict the first classes, whose recomputation would then
        // evict the next classes, defeating reuse throughout the scan.
        let semantic = json!({"payload": "x".repeat(2 * 1024 * 1024)});
        let keys = (0..24)
            .map(|index| request_key(&("class", index)).unwrap())
            .collect::<Vec<_>>();
        for key in &keys {
            cache.insert(*key, &semantic);
        }
        let before = (cache.entries.len(), cache.bytes, cache.clock);
        let index_key = request_key(&"project-index").unwrap();
        let index_response = json!({"payload": "y".repeat(RequestCache::MAX_ENTRY_BYTES)});
        cache.insert(index_key, &index_response);
        assert_eq!((cache.entries.len(), cache.bytes, cache.clock), before);
        assert_eq!(cache.oversized_skips, 1);
        assert!(cache.get(&index_key).is_none());
        for key in keys {
            assert!(cache.get(&key).is_some(), "class result was evicted");
        }
        // Admission limits do not modify the response consumed by the caller.
        assert_eq!(
            index_response["payload"].as_str().unwrap().len(),
            RequestCache::MAX_ENTRY_BYTES
        );
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
