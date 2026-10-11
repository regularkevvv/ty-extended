//! Preserve the user's ty options while relocating analysis-only source paths.
use anyhow::{Context, Result, ensure};
use globset::{GlobBuilder, GlobMatcher};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};
use toml::{Table, Value};

type PluginRegistration = (PathBuf, PathBuf, serde_json::Value);

pub struct Config {
    pub text: String,
    pub editor_configuration: Option<serde_json::Value>,
    pub python_version: String,
    pub platform: String,
    pub source: Option<PathBuf>,
    pub source_roots: Vec<PathBuf>,
    pub dependency_roots: Vec<PathBuf>,
    pub selection: Selection,
    pub plugins: Vec<PluginRegistration>,
    pub plugin_diagnostics: Vec<serde_json::Value>,
    pub plugin_watch_paths: Vec<PathBuf>,
    pub stub_roots: Vec<PathBuf>,
    pub typeshed: Option<PathBuf>,
    pub terminal: serde_json::Value,
}

#[derive(Clone)]
struct Pattern {
    matcher: GlobMatcher,
    directory_only: bool,
    negated: bool,
}
impl Pattern {
    fn new(pattern: &str, base: &Path) -> Result<Self> {
        let (negated, pattern) = pattern
            .strip_prefix('!')
            .map_or((false, pattern), |p| (true, p));
        let directory_only = pattern.ends_with('/');
        let pattern = pattern.trim_end_matches('/');
        let pattern = relocated(pattern, base, base, base);
        let matcher = GlobBuilder::new(&pattern)
            .literal_separator(true)
            .backslash_escape(false)
            .build()?
            .compile_matcher();
        Ok(Self {
            matcher,
            directory_only,
            negated,
        })
    }
    fn matches(&self, path: &Path) -> bool {
        path.ancestors()
            .enumerate()
            .any(|(index, p)| (!self.directory_only || index > 0) && self.matcher.is_match(p))
    }
}

#[derive(Clone)]
pub struct Selection {
    includes: Option<Vec<Pattern>>,
    excludes: Vec<Pattern>,
    root: PathBuf,
    respect_ignore: bool,
    exclude_scripts: bool,
}
impl Selection {
    fn new(table: &Table, base: &Path, root: &Path) -> Result<Self> {
        let src = table.get("src").and_then(Value::as_table);
        let patterns = |key: &str| -> Result<Option<Vec<Pattern>>> {
            src.and_then(|t| t.get(key))
                .map(|v| {
                    v.as_array()
                        .context("source pattern must be an array")?
                        .iter()
                        .map(|v| {
                            Pattern::new(
                                v.as_str().context("source pattern must be a string")?,
                                base,
                            )
                        })
                        .collect()
                })
                .transpose()
        };
        let defaults = [
            ".bzr",
            ".direnv",
            ".eggs",
            ".git",
            ".git-rewrite",
            ".hg",
            ".mypy_cache",
            ".nox",
            ".pants.d",
            ".pytype",
            ".ruff_cache",
            ".svn",
            ".tox",
            ".venv",
            "__pypackages__",
            "_build",
            "buck-out",
            "dist",
            "node_modules",
            "venv",
        ];
        let mut excludes = defaults
            .into_iter()
            .map(|p| Pattern::new(&format!("**/{p}/"), root))
            .collect::<Result<Vec<_>>>()?;
        excludes.extend(patterns("exclude")?.unwrap_or_default());
        Ok(Self {
            includes: patterns("include")?,
            excludes,
            root: root.to_owned(),
            respect_ignore: src
                .and_then(|t| t.get("respect-ignore-files"))
                .and_then(Value::as_bool)
                .unwrap_or(true),
            exclude_scripts: src
                .and_then(|t| t.get("exclude-scripts"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
    pub fn includes(&self, path: &Path, text: &str) -> bool {
        if self.exclude_scripts && text.lines().any(|line| line.trim() == "# /// script") {
            return false;
        }
        if self
            .includes
            .as_ref()
            .is_some_and(|patterns| !patterns.iter().any(|p| p.matches(path)))
        {
            return false;
        }
        let mut excluded = false;
        for pattern in &self.excludes {
            if pattern.matches(path) {
                excluded = !pattern.negated;
            }
        }
        !excluded
    }
    pub fn includes_explicit(&self, path: &Path, text: &str, force_exclude: bool) -> bool {
        !force_exclude || self.includes(path, text)
    }
    pub fn ignored_paths(&self) -> std::collections::BTreeSet<PathBuf> {
        self.ignored_paths_under(&self.root)
    }
    pub fn ignored_paths_under(&self, root: &Path) -> std::collections::BTreeSet<PathBuf> {
        if !self.respect_ignore {
            return std::collections::BTreeSet::new();
        }
        let mut all = std::collections::BTreeSet::new();
        let mut visible = std::collections::BTreeSet::new();
        for (respect, paths) in [(false, &mut all), (true, &mut visible)] {
            let mut walker = ignore::WalkBuilder::new(root);
            walker
                .hidden(false)
                .git_ignore(respect)
                .git_global(respect)
                .git_exclude(respect)
                .ignore(respect)
                .parents(respect);
            // Python environments and VCS objects cannot be first-party sources.
            walker.filter_entry(|entry| {
                !entry.file_type().is_some_and(|t| t.is_dir())
                    || !matches!(
                        entry.file_name().to_str(),
                        Some(".git" | ".venv" | "venv" | "node_modules" | "target" | "__pycache__")
                    )
            });
            for entry in walker.build().filter_map(|entry| entry.ok()) {
                if entry.file_type().is_some_and(|t| t.is_file())
                    && entry
                        .path()
                        .extension()
                        .is_some_and(|e| e == "py" || e == "pyi" || e == "ipynb")
                {
                    paths.insert(entry.into_path());
                }
            }
        }
        all.difference(&visible).cloned().collect()
    }
}

fn discovered(root: &Path, explicit: Option<&String>) -> Result<(Table, PathBuf, Option<PathBuf>)> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path).canonicalize()?;
        let value: Table = toml::from_str(&std::fs::read_to_string(&path)?)?;
        let table = if path.file_name().is_some_and(|n| n == "pyproject.toml") {
            value
                .get("tool")
                .and_then(|v| v.get("ty"))
                .and_then(Value::as_table)
                .cloned()
                .unwrap_or_default()
        } else {
            value
        };
        return Ok((table, path.parent().unwrap().to_owned(), Some(path)));
    }
    for directory in root.ancestors() {
        let path = directory.join("ty.toml");
        if path.exists() {
            return Ok((
                toml::from_str(&std::fs::read_to_string(&path)?)?,
                directory.to_owned(),
                Some(path),
            ));
        }
        let path = directory.join("pyproject.toml");
        if path.exists() {
            let value: Table = toml::from_str(&std::fs::read_to_string(&path)?)?;
            if let Some(table) = value
                .get("tool")
                .and_then(|v| v.get("ty"))
                .and_then(Value::as_table)
            {
                return Ok((table.clone(), directory.to_owned(), Some(path)));
            }
        }
    }
    Ok((Table::new(), root.to_owned(), None))
}

/// Merge a higher-precedence ty configuration layer. ty_combine places higher
/// precedence array items last, while scalar values replace earlier values.
fn merge_layer(table: &mut Table, additions: Table, combine: bool) {
    fn merge(table: &mut Table, additions: Table, path: &mut Vec<String>, combine: bool) {
        for (key, value) in additions {
            // A plugin's configuration is opaque JSON, not another ty options
            // struct. PluginsOptions.config combines its map entries atomically.
            if combine && path.as_slice() == ["plugins", "config"] {
                table.insert(key, value);
                continue;
            }
            match (table.get_mut(&key), value) {
                (Some(Value::Table(existing)), Value::Table(additions)) => {
                    path.push(key);
                    merge(existing, additions, path, combine);
                    path.pop();
                }
                (Some(Value::Array(existing)), Value::Array(additions)) if combine => {
                    existing.extend(additions);
                }
                (_, value) => {
                    table.insert(key, value);
                }
            }
        }
    }
    merge(table, additions, &mut Vec::new(), combine);
}

fn expanded_editor_path(value: &str, root: &Path) -> Result<PathBuf> {
    let expanded = shellexpand::full_with_context(
        value,
        || std::env::var("HOME").ok(),
        |name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(error) => Err(error),
        },
    )
    .context("expand editor configuration path")?;
    Ok(PathBuf::from(relocated(&expanded, root, root, root)))
}

/// Keep each inline path's original workspace origin before merging it with
/// paths read from a (possibly different) configuration directory.
fn anchor_editor_paths(table: &mut Table, root: &Path) -> Result<()> {
    if let Some(environment) = table.get_mut("environment").and_then(Value::as_table_mut) {
        for key in ["root", "extra-paths"] {
            if let Some(values) = environment.get_mut(key).and_then(Value::as_array_mut) {
                for value in values {
                    if let Some(path) = value.as_str() {
                        *value = Value::String(
                            expanded_editor_path(path, root)?
                                .to_string_lossy()
                                .into_owned(),
                        );
                    }
                }
            }
        }
        for key in ["python", "typeshed"] {
            if let Some(value) = environment.get_mut(key)
                && let Some(path) = value.as_str()
            {
                *value = Value::String(
                    expanded_editor_path(path, root)?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let anchor_globs = |table: &mut Table| {
        for key in ["include", "exclude"] {
            if let Some(value) = table.get_mut(key) {
                relocate_list(value, root, root, root);
            }
        }
    };
    if let Some(src) = table.get_mut("src").and_then(Value::as_table_mut) {
        anchor_globs(src);
    }
    if let Some(overrides) = table.get_mut("overrides").and_then(Value::as_array_mut) {
        for entry in overrides.iter_mut().filter_map(Value::as_table_mut) {
            anchor_globs(entry);
        }
    }
    if let Some(plugins) = table.get_mut("plugins").and_then(Value::as_table_mut)
        && let Some(entries) = plugins.get_mut("plugin").and_then(Value::as_array_mut)
    {
        for entry in entries.iter_mut().filter_map(Value::as_table_mut) {
            for key in ["path", "manifest-path", "stub-overlay-path"] {
                if let Some(value) = entry.get_mut(key)
                    && let Some(path) = value.as_str()
                {
                    *value = Value::String(
                        expanded_editor_path(path, root)?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
            }
        }
    }
    Ok(())
}

fn editor_options(options: &BTreeMap<String, String>) -> Result<serde_json::Value> {
    let read = |key: &str| -> Result<serde_json::Value> {
        options
            .get(key)
            .map(|value| serde_json::from_str(value).with_context(|| format!("invalid {key}")))
            .transpose()
            .map(|value| value.unwrap_or(serde_json::Value::Null))
    };
    Ok(crate::editor_options::effective(
        &read("lsp-initialization-options")?,
        &read("lsp-workspace-configuration")?,
    ))
}

/// The checked-in schema supplies serialization shape, not filesystem or glob
/// semantics; those remain the native checker's responsibility.
fn editor_schema() -> Result<&'static serde_json::Value> {
    static SCHEMA: std::sync::OnceLock<std::result::Result<serde_json::Value, String>> =
        std::sync::OnceLock::new();
    SCHEMA
        .get_or_init(|| {
            let schema: serde_json::Value =
                serde_json::from_str(include_str!("../../ruff/ty.schema.json"))
                    .map_err(|error| error.to_string())?;
            audit_editor_schema(&schema, &schema).map_err(|error| error.to_string())?;
            Ok(schema)
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("unsupported pinned ty configuration schema: {error}"))
}

fn audit_editor_schema(schema: &serde_json::Value, root: &serde_json::Value) -> Result<()> {
    if schema.is_boolean() {
        return Ok(());
    }
    let object = schema
        .as_object()
        .context("schema must be an object or boolean")?;
    for (key, value) in object {
        match key.as_str() {
            "$schema" | "title" | "description" | "default" | "const" => {}
            "type" => {
                let supported = |name: &serde_json::Value| {
                    matches!(
                        name.as_str(),
                        Some(
                            "object"
                                | "array"
                                | "string"
                                | "boolean"
                                | "null"
                                | "number"
                                | "integer"
                        )
                    )
                };
                ensure!(
                    supported(value)
                        || value
                            .as_array()
                            .is_some_and(|types| !types.is_empty() && types.iter().all(supported)),
                    "unsupported schema type {value}"
                );
            }
            "enum" => ensure!(
                value.as_array().is_some_and(|values| !values.is_empty()),
                "schema enum must be a nonempty array"
            ),
            "required" => ensure!(
                value
                    .as_array()
                    .is_some_and(|keys| keys.iter().all(serde_json::Value::is_string)),
                "schema required must contain strings"
            ),
            "$ref" => {
                let pointer = value
                    .as_str()
                    .and_then(|value| value.strip_prefix('#'))
                    .context("schema reference must be local")?;
                ensure!(
                    root.pointer(pointer).is_some(),
                    "missing schema reference {pointer}"
                );
            }
            "properties" | "definitions" => {
                for child in value
                    .as_object()
                    .context("schema properties must be an object")?
                    .values()
                {
                    audit_editor_schema(child, root)?;
                }
            }
            "additionalProperties" | "items" => audit_editor_schema(value, root)?,
            "oneOf" | "anyOf" | "allOf" => {
                for child in value
                    .as_array()
                    .context("schema alternatives must be an array")?
                {
                    audit_editor_schema(child, root)?;
                }
            }
            _ => anyhow::bail!("unhandled validation keyword {key}"),
        }
    }
    Ok(())
}

fn matches_editor_schema(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    depth: usize,
) -> bool {
    if depth > 128 {
        return false;
    }
    if let Some(result) = schema.as_bool() {
        return result;
    }
    if let Some(reference) = schema.get("$ref").and_then(serde_json::Value::as_str) {
        return reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer))
            .is_some_and(|referenced| matches_editor_schema(value, referenced, root, depth + 1));
    }
    let type_matches = |name: &str| match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        _ => false,
    };
    if let Some(types) = schema.get("type") {
        let matches = types.as_str().map(type_matches).unwrap_or_else(|| {
            types.as_array().is_some_and(|types| {
                types
                    .iter()
                    .any(|name| name.as_str().is_some_and(type_matches))
            })
        });
        if !matches {
            return false;
        }
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
        || schema
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|choices| !choices.contains(value))
    {
        return false;
    }
    for (key, predicate) in [("allOf", 0), ("anyOf", 1), ("oneOf", 2)] {
        if let Some(choices) = schema.get(key).and_then(serde_json::Value::as_array) {
            let matches = choices
                .iter()
                .filter(|choice| matches_editor_schema(value, choice, root, depth + 1))
                .count();
            if match predicate {
                0 => matches != choices.len(),
                1 => matches == 0,
                _ => matches != 1,
            } {
                return false;
            }
        }
    }
    if let Some(object) = value.as_object() {
        if schema
            .get("required")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|keys| {
                keys.iter()
                    .any(|key| key.as_str().is_none_or(|key| !object.contains_key(key)))
            })
        {
            return false;
        }
        for (key, value) in object {
            let child = schema
                .get("properties")
                .and_then(|properties| properties.get(key))
                .or_else(|| schema.get("additionalProperties"));
            if child.is_some_and(|child| !matches_editor_schema(value, child, root, depth + 1)) {
                return false;
            }
        }
    }
    if let Some(array) = value.as_array()
        && let Some(items) = schema.get("items")
        && !array
            .iter()
            .all(|value| matches_editor_schema(value, items, root, depth + 1))
    {
        return false;
    }
    true
}

fn remove_null_options(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            object.retain(|_, value| !value.is_null());
            for value in object.values_mut() {
                remove_null_options(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                remove_null_options(value);
            }
        }
        _ => {}
    }
}

fn editor_python_version(editor: &serde_json::Value) -> Option<String> {
    let version = editor.pointer("/pythonExtension/activeEnvironment/version")?;
    let major = version["major"].as_u64()?;
    let minor = version["minor"].as_u64()?;
    // Match SupportedPythonVersion in the pinned upstream project options.
    (major == 3 && (7..=15).contains(&minor)).then(|| format!("3.{minor}"))
}

fn project_metadata(
    root: &Path,
    base: &Path,
    source: Option<&Path>,
    explicit: bool,
) -> Result<Table> {
    if explicit {
        // Upstream ProjectMetadata::from_config_file deliberately bypasses the
        // workspace pyproject metadata, including requires-python.
        return Ok(Table::new());
    }
    let path = if source.is_some() {
        Some(base.join("pyproject.toml"))
    } else {
        root.ancestors()
            .map(|parent| parent.join("pyproject.toml"))
            .find(|path| path.is_file())
    };
    let Some(path) = path.filter(|path| path.is_file()) else {
        return Ok(Table::new());
    };
    let original: Table = toml::from_str(&std::fs::read_to_string(path)?)?;
    let mut metadata = Table::new();
    if let Some(project) = original.get("project") {
        metadata.insert("project".into(), project.clone());
    }
    Ok(metadata)
}

fn relocate_native_layer(table: &mut Table, base: &Path, root: &Path, view: &Path) -> Result<()> {
    table.remove("plugins");
    if let Some(environment) = table.get_mut("environment").and_then(Value::as_table_mut) {
        for key in ["root", "extra-paths"] {
            if let Some(value) = environment.get_mut(key) {
                relocate_list(value, base, root, view);
            }
        }
        for key in ["python", "typeshed"] {
            if let Some(value) = environment.get_mut(key)
                && let Some(path) = value.as_str()
            {
                *value = Value::String(
                    expanded_editor_path(path, base)?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    let relocate_globs = |table: &mut Table| -> Result<()> {
        for key in ["include", "exclude"] {
            if let Some(value) = table.get_mut(key) {
                relocate_list(value, base, root, view);
                if key == "exclude" {
                    preserve_negated_globs(value, view.parent().context("checker root")?)?;
                }
                #[cfg(windows)]
                portable_windows_globs(value, view.parent().context("checker root")?)?;
            }
        }
        Ok(())
    };
    if let Some(src) = table.get_mut("src").and_then(Value::as_table_mut) {
        relocate_globs(src)?;
    }
    if let Some(overrides) = table.get_mut("overrides").and_then(Value::as_array_mut) {
        for entry in overrides.iter_mut().filter_map(Value::as_table_mut) {
            relocate_globs(entry)?;
        }
    }
    Ok(())
}

fn relocated(value: &str, base: &Path, root: &Path, view: &Path) -> String {
    let path = Path::new(value);
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    // Remove `.`/`..` lexically: overlays can name files which do not exist yet.
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    #[cfg(windows)]
    let comparable = windows_glob_path(&normalized.to_string_lossy());
    #[cfg(windows)]
    let comparable_root = windows_glob_path(&root.to_string_lossy());
    #[cfg(windows)]
    {
        windows_relative_path(&comparable, &comparable_root)
            .map_or(normalized.clone(), |suffix| view.join(suffix))
            .to_string_lossy()
            .into_owned()
    }
    #[cfg(not(windows))]
    normalized
        .strip_prefix(root)
        .map_or(normalized.clone(), |suffix| view.join(suffix))
        .to_string_lossy()
        .into_owned()
}

#[cfg(any(windows, test))]
fn windows_glob_path(value: &str) -> String {
    let value = value.replace('\\', "/");
    if let Some(path) = value.strip_prefix("//?/UNC/") {
        format!("//{path}")
    } else {
        value.strip_prefix("//?/").unwrap_or(&value).to_owned()
    }
}

#[cfg(any(windows, test))]
fn windows_relative_path<'a>(path: &'a str, root: &str) -> Option<&'a str> {
    let root = root.trim_end_matches('/');
    let prefix = path.get(..root.len())?;
    if !prefix.eq_ignore_ascii_case(root) {
        return None;
    }
    let suffix = &path[root.len()..];
    if suffix.is_empty() {
        Some(suffix)
    } else {
        suffix.strip_prefix('/')
    }
}

#[cfg(any(windows, test))]
fn windows_glob_pattern(value: &str, checker_root: &str) -> Result<String> {
    let (negated, value) = value
        .strip_prefix('!')
        .map_or((false, value), |v| (true, v));
    let path = windows_glob_path(value);
    let root = windows_glob_path(checker_root);
    // ty's portable glob parser rejects drive colons and verbatim prefixes.
    // Relative patterns let ty anchor and escape the Windows root itself.
    let pattern = match windows_relative_path(&path, &root) {
        Some("") => ".",
        Some(relative) => relative,
        None => &path,
    };
    ensure!(
        pattern.as_bytes().get(1) != Some(&b':')
            && !pattern.split('/').any(|component| component == ".."),
        "Windows glob {value:?} is outside checker root {checker_root:?}; official ty cannot represent drive-qualified or parent-directory glob patterns"
    );
    Ok(if negated {
        format!("!{pattern}")
    } else {
        pattern.to_owned()
    })
}

#[cfg(windows)]
fn portable_windows_globs(value: &mut Value, checker_root: &Path) -> Result<()> {
    let root = checker_root.to_string_lossy();
    for value in value.as_array_mut().into_iter().flatten() {
        if let Some(path) = value.as_str() {
            *value = Value::String(windows_glob_pattern(path, &root)?);
        }
    }
    Ok(())
}

fn relocate_list(value: &mut Value, base: &Path, root: &Path, view: &Path) {
    if let Some(values) = value.as_array_mut() {
        for value in values {
            if let Some(path) = value.as_str() {
                let (negated, path) = path.strip_prefix('!').map_or((false, path), |s| (true, s));
                let path = relocated(path, base, root, view);
                *value = Value::String(if negated { format!("!{path}") } else { path });
            }
        }
    }
}

fn preserve_negated_globs(value: &mut Value, checker_root: &Path) -> Result<()> {
    for value in value.as_array_mut().into_iter().flatten() {
        let Some(path) = value.as_str().and_then(|path| path.strip_prefix('!')) else {
            continue;
        };
        #[cfg(windows)]
        let normalized_target = PathBuf::from(windows_glob_path(path));
        #[cfg(windows)]
        let normalized_base = PathBuf::from(windows_glob_path(&checker_root.to_string_lossy()));
        #[cfg(windows)]
        let target: Vec<_> = normalized_target.components().collect();
        #[cfg(windows)]
        let base: Vec<_> = normalized_base.components().collect();
        #[cfg(not(windows))]
        let target: Vec<_> = Path::new(path).components().collect();
        #[cfg(not(windows))]
        let base: Vec<_> = checker_root.components().collect();
        let common = target
            .iter()
            .zip(&base)
            .take_while(|(left, right)| left == right)
            .count();
        ensure!(
            common > 0,
            "negated exclusion {path:?} is on another filesystem root"
        );
        let mut relative = PathBuf::new();
        for _ in &base[common..] {
            relative.push("..");
        }
        for component in &target[common..] {
            relative.push(component.as_os_str());
        }
        // Pinned ty drops the `!` for absolute patterns in PortableGlobPattern::
        // into_absolute. Relative exclusions retain it and have equal meaning.
        *value = Value::String(format!(
            "!{}",
            relative.to_string_lossy().replace('\\', "/")
        ));
    }
    Ok(())
}

#[derive(Default)]
struct PluginRegistrations {
    plugins: Vec<PluginRegistration>,
    roots: Vec<PathBuf>,
    diagnostics: Vec<serde_json::Value>,
    watch_paths: Vec<PathBuf>,
}

// Parse value spans through TOML's own deserializer, including quoted keys,
// inline tables, multiline strings and pyproject's [tool.ty] nesting.
#[derive(Default, serde::Deserialize)]
struct SpannedPlugins {
    #[serde(default)]
    plugin: Vec<BTreeMap<String, toml::Spanned<Value>>>,
}
#[derive(Default, serde::Deserialize)]
struct SpannedOptions {
    #[serde(default)]
    plugins: SpannedPlugins,
}
#[derive(Default, serde::Deserialize)]
struct SpannedTool {
    #[serde(default)]
    ty: SpannedOptions,
}
#[derive(Default, serde::Deserialize)]
struct SpannedConfig {
    #[serde(default)]
    plugins: SpannedPlugins,
    #[serde(default)]
    tool: SpannedTool,
}

struct PluginOrigin {
    source: Option<PathBuf>,
    text: String,
    entries: Vec<BTreeMap<String, toml::Spanned<Value>>>,
    inline: &'static str,
}
impl PluginOrigin {
    fn new(source: Option<&Path>, options: &BTreeMap<String, String>) -> Result<Self> {
        let mut inline = None;
        if editor_options(options)?
            .pointer("/configuration/plugins/plugin")
            .is_some()
        {
            inline = Some("editor settings");
        }
        if let Some(overrides) = options.get("cli-config") {
            for text in serde_json::from_str::<Vec<String>>(overrides)? {
                let table: Table = toml::from_str(&text)?;
                if table.get("plugins").and_then(|p| p.get("plugin")).is_some() {
                    inline = Some("CLI");
                }
            }
        }
        let source = source.filter(|_| inline.is_none()).map(Path::to_owned);
        let text = source
            .as_ref()
            .map(std::fs::read_to_string)
            .transpose()?
            .unwrap_or_default();
        let parsed: SpannedConfig = toml::from_str(&text)?;
        let entries = if source
            .as_ref()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "pyproject.toml"))
        {
            parsed.tool.ty.plugins.plugin
        } else {
            parsed.plugins.plugin
        };
        Ok(Self {
            source,
            text,
            entries,
            inline: inline.unwrap_or("CLI"),
        })
    }

    fn diagnostic(
        &self,
        index: usize,
        key: &str,
        message: String,
        label: &str,
        warning: bool,
        info: Option<&str>,
    ) -> serde_json::Value {
        let span = self
            .entries
            .get(index)
            .and_then(|entry| entry.get(key))
            .map(toml::Spanned::span);
        let position = |offset: usize| {
            let before = &self.text[..offset.min(self.text.len())];
            serde_json::json!({"line":before.bytes().filter(|&b| b == b'\n').count() + 1,"column":before.rsplit('\n').next().unwrap_or("").chars().count() + 1})
        };
        let mut notes = Vec::new();
        if self.source.is_none() {
            notes.push(if label == "plugin configuration" {
                format!(
                    "info: The plugin option was specified {}{}.",
                    if self.inline == "CLI" {
                        "on the "
                    } else {
                        "in the "
                    },
                    self.inline
                )
            } else {
                format!(
                    "info: The plugin path was specified {}{}: {label}",
                    if self.inline == "CLI" {
                        "on the "
                    } else {
                        "in the "
                    },
                    self.inline
                )
            });
        }
        if let Some(info) = info {
            notes.push(format!("info: {info}"));
        }
        serde_json::json!({
            "check_name":"plugin-configuration", "severity":if warning {"minor"} else {"major"},
            "description":format!("plugin-configuration: {message}{}", if self.source.is_some() {format!(": {label}")} else {String::new()}),
            "location":{"path":self.source.as_ref().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),"positions":{
                "begin":position(span.as_ref().map_or(0, |r| r.start)),"end":position(span.as_ref().map_or(0, |r| r.end))}},
            "details":notes.join("\n"), "configuration-diagnostic":true,
        })
    }
}

fn plugin_path_display(path: &Path) -> String {
    // Canonical Windows paths carry a \\?\ prefix internally. URI identity
    // serialization removes that implementation prefix while preserving drive,
    // UNC, Unicode and percent characters in user-visible diagnostic labels.
    crate::checker::file_uri(path)
        .ok()
        .and_then(|uri| crate::lsp_mapping::decode_file_uri(&uri).ok().flatten())
        .unwrap_or_else(|| path.to_owned())
        .display()
        .to_string()
}

fn plugin_registrations(
    table: &Table,
    base: &Path,
    root: &Path,
    options: &BTreeMap<String, String>,
    source: Option<&Path>,
) -> Result<PluginRegistrations> {
    let mut result = PluginRegistrations::default();
    let roots = &mut result.roots;
    if let Some(stubs) = options.get("stubs") {
        roots.push(PathBuf::from(stubs).canonicalize()?);
    }
    if options.contains_key("manifest") || options.contains_key("plugin") {
        let manifest =
            PathBuf::from(options.get("manifest").context("missing --manifest")?).canonicalize()?;
        let artifact =
            PathBuf::from(options.get("plugin").context("missing --plugin")?).canonicalize()?;
        let config = if let Some(path) = options.get("config") {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            serde_json::json!({})
        };
        result
            .watch_paths
            .extend([manifest.clone(), artifact.clone()]);
        result.plugins.push((manifest, artifact, config));
        return Ok(result);
    }
    let Some(plugins) = table.get("plugins").and_then(Value::as_table) else {
        return Ok(result);
    };
    let origin = PluginOrigin::new(source, options)?;
    let registrations = &mut result.plugins;
    let configured = plugins
        .get("plugin")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let configured_ids: std::collections::BTreeSet<_> = configured
        .iter()
        .filter_map(|v| v.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    let enabled = plugins
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    for (index, entry) in configured.iter().enumerate() {
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .context("plugin entry requires id")?;
        let artifact = PathBuf::from(relocated(
            entry
                .get("path")
                .and_then(Value::as_str)
                .context("plugin requires path")?,
            base,
            root,
            root,
        ));
        let manifest = entry
            .get("manifest-path")
            .and_then(Value::as_str)
            .map(|p| PathBuf::from(relocated(p, base, root, root)))
            .unwrap_or_else(|| artifact.clone());
        let overlay = entry
            .get("stub-overlay-path")
            .and_then(Value::as_str)
            .map(|p| PathBuf::from(relocated(p, base, root, root)));
        result
            .watch_paths
            .extend([artifact.clone(), manifest.clone()]);
        result.watch_paths.extend(overlay.iter().cloned());
        let mut report =
            |key: &str, message: String, label: &str, warning: bool, info: Option<&str>| {
                result
                    .diagnostics
                    .push(origin.diagnostic(index, key, message, label, warning, info));
            };
        if !enabled {
            report(
                "id",
                format!("Plugin `{id}` is configured but plugins are disabled"),
                "plugin configuration",
                true,
                Some("Set `plugins.enabled = true` to enable configured plugins."),
            );
            continue;
        }
        if !artifact.is_file() {
            report(
                "path",
                format!(
                    "Plugin `{id}` points to an artifact path that does not exist or is not a file"
                ),
                &format!(
                    "`{}` does not exist or is not a file",
                    plugin_path_display(&artifact)
                ),
                false,
                None,
            );
            continue;
        }
        if !entry
            .get("trusted")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            report(
                "id",
                format!("Plugin `{id}` is not trusted to execute local code"),
                "plugin configuration",
                false,
                Some("Set `trusted = true` for this plugin only if you trust the artifact."),
            );
            continue;
        }
        let runtime = entry
            .get("runtime")
            .and_then(Value::as_str)
            .unwrap_or("wasm");
        ensure!(
            runtime == "monty",
            "Plugin `{id}` uses {runtime}; this companion requires a Monty artifact and runtime='monty'"
        );
        if let Some(overlay) = &overlay
            && !overlay.is_dir()
        {
            report(
                "stub-overlay-path",
                format!(
                    "Plugin `{id}` points to a stub overlay path that does not exist or is not a directory"
                ),
                &format!(
                    "`{}` does not exist or is not a directory",
                    plugin_path_display(overlay)
                ),
                false,
                None,
            );
            continue;
        }
        let manifest_key = if entry.get("manifest-path").is_some() {
            "manifest-path"
        } else {
            "path"
        };
        if !manifest.is_file() {
            report(
                manifest_key,
                format!(
                    "Plugin `{id}` points to a manifest path that does not exist or is not a file"
                ),
                &format!(
                    "`{}` does not exist or is not a file",
                    plugin_path_display(&manifest)
                ),
                false,
                None,
            );
            continue;
        }
        let content = match std::fs::read_to_string(&manifest) {
            Ok(content) => content,
            Err(error) => {
                report(
                    manifest_key,
                    format!("Failed to read manifest for plugin `{id}`"),
                    &error.to_string(),
                    false,
                    None,
                );
                continue;
            }
        };
        let parsed: ty_plugin_protocol::PluginManifest = match serde_json::from_str(&content) {
            Ok(parsed) => parsed,
            Err(error) => {
                report(
                    manifest_key,
                    format!("Failed to parse manifest for plugin `{id}`"),
                    &error.to_string(),
                    false,
                    None,
                );
                continue;
            }
        };
        if parsed.id != id {
            report(
                "id",
                format!(
                    "Configured plugin id `{id}` does not match manifest id `{}`",
                    parsed.id
                ),
                "plugin configuration",
                false,
                None,
            );
            continue;
        }
        if overlay.is_some() && !parsed.capabilities.stub_overlays {
            report(
                "stub-overlay-path",
                format!(
                    "Plugin `{id}` configures a stub overlay path but the manifest does not declare the stub-overlays capability"
                ),
                "manifest must set `capabilities.stub-overlays = true`",
                false,
                None,
            );
            continue;
        }
        if let Some(overlay) = overlay {
            roots.push(overlay.canonicalize()?);
        }
        let config = entry
            .get("config")
            .map(serde_json::to_value)
            .transpose()?
            .unwrap_or_else(|| serde_json::json!({}));
        registrations.push((manifest.canonicalize()?, artifact.canonicalize()?, config));
    }
    if plugins
        .get("auto-discover")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        let environment = table.get("environment").and_then(Value::as_table);
        let python = options
            .get("python")
            .cloned()
            .or_else(|| {
                environment
                    .and_then(|e| e.get("python"))
                    .and_then(Value::as_str)
                    .map(|p| relocated(p, base, root, root))
            })
            .or_else(|| std::env::var("VIRTUAL_ENV").ok())
            .or_else(|| {
                root.join(".venv")
                    .is_dir()
                    .then(|| root.join(".venv").to_string_lossy().into_owned())
            });
        let mut sites = Vec::new();
        if let Some(python) = python {
            let path = PathBuf::from(python);
            let prefix = if path.is_file() {
                path.parent()
                    .and_then(Path::parent)
                    .unwrap_or(&path)
                    .to_owned()
            } else {
                path
            };
            let windows = prefix.join("Lib/site-packages");
            if windows.is_dir() {
                sites.push(windows);
            }
            if let Ok(versions) = std::fs::read_dir(prefix.join("lib")) {
                for version in versions.flatten() {
                    let site = version.path().join("site-packages");
                    if site.is_dir() {
                        sites.push(site);
                    }
                }
            }
        }
        sites.sort();
        sites.dedup();
        let mut seen = configured_ids;
        for site in sites {
            let mut packages = std::fs::read_dir(site)?
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| p.is_dir())
                .collect::<Vec<_>>();
            packages.sort();
            for package in packages {
                let manifest = package.join("ty-plugin.json");
                if !manifest.is_file() {
                    continue;
                }
                let Ok(parsed) = std::fs::read(&manifest)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                    .ok_or(())
                else {
                    continue;
                };
                // Installed package discovery silently skips malformed manifests,
                // unlike explicit entries, which produce ranged diagnostics.
                if serde_json::from_value::<ty_plugin_protocol::PluginManifest>(parsed.clone())
                    .is_err()
                {
                    continue;
                }
                let Some(id) = parsed["id"].as_str().filter(|id| !id.is_empty()) else {
                    continue;
                };
                if seen.contains(id) {
                    continue;
                }
                let runtime = parsed
                    .pointer("/runtime/kind")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                ensure!(
                    runtime == "monty",
                    "Installed plugin `{id}` uses {runtime}; configure its Monty manifest/artifact explicitly before using this companion"
                );
                let artifact = package.join(
                    parsed
                        .pointer("/runtime/artifact")
                        .and_then(serde_json::Value::as_str)
                        .context("installed Monty plugin requires artifact")?,
                );
                result
                    .watch_paths
                    .extend([manifest.clone(), artifact.clone()]);
                if !artifact.is_file() {
                    continue;
                }
                if parsed["stub-overlays"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
                {
                    let stubs = package.join("stubs");
                    result.watch_paths.push(stubs.clone());
                    if !stubs.is_dir()
                        || parsed.pointer("/capabilities/stub-overlays")
                            != Some(&serde_json::Value::Bool(true))
                    {
                        continue;
                    }
                    roots.push(stubs.canonicalize()?);
                }
                seen.insert(id.to_owned());
                let config = plugins
                    .get("config")
                    .and_then(|c| c.get(id))
                    .map(serde_json::to_value)
                    .transpose()?
                    .unwrap_or_else(|| serde_json::json!({}));
                registrations.push((manifest.canonicalize()?, artifact.canonicalize()?, config));
            }
        }
    }
    if !registrations.is_empty() {
        ensure!(
            plugins
                .get("monty-mode")
                .and_then(Value::as_str)
                .unwrap_or("in-process")
                == "in-process",
            "This companion embeds Monty; plugins.monty-mode=worker is not supported. Select in-process or use ty-extended's worker runtime."
        );
    }
    // Manifest host validation is atomic upstream: a malformed environment
    // disables all semantic registrations, while per-entry path/trust errors
    // above only skip that entry.
    let manifests = registrations
        .iter()
        .map(|(path, _, _)| {
            Ok(
                serde_json::from_slice::<ty_plugin_protocol::PluginManifest>(&std::fs::read(
                    path,
                )?)?,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    if let Err(error) = ty_plugin_host::PluginEnvironment::from_manifests(manifests.clone()) {
        let (id, message) = match &error {
            ty_plugin_host::HostError::DuplicatePluginId(id) => (
                id.clone(),
                format!("Plugin id `{id}` is declared by multiple plugin manifests"),
            ),
            ty_plugin_host::HostError::UnsupportedProtocolVersion {
                plugin_id,
                major,
                minor,
                supported_major,
                supported_minor,
            } => (
                plugin_id.clone(),
                format!(
                    "Plugin `{plugin_id}` uses unsupported protocol version {major}.{minor}; ty supports {supported_major}.{supported_minor}"
                ),
            ),
            _ => {
                let text = error.to_string();
                let id = text.split('`').nth(1).unwrap_or("").to_owned();
                let message = format!("{}{}", text[..1].to_uppercase(), &text[1..]);
                (id, message)
            }
        };
        let path = manifests
            .iter()
            .position(|manifest| manifest.id == id)
            .map(|i| registrations[i].0.clone());
        result.diagnostics.push(serde_json::json!({"check_name":"plugin-configuration","severity":"major","description":format!("plugin-configuration: {message}{}",if path.is_some() {": plugin manifest"} else {""}),"location":{"path":path,"positions":{"begin":{"line":1,"column":1},"end":{"line":1,"column":1}}},"configuration-diagnostic":true}));
        registrations.clear();
        roots.clear();
    }
    result.watch_paths.extend(
        registrations
            .iter()
            .flat_map(|(manifest, artifact, _)| [manifest.clone(), artifact.clone()]),
    );
    for path in &mut result.watch_paths {
        // Match the canonical identities used by didChangeWatchedFiles, even
        // when the configured file is missing beneath a symlinked directory.
        *path = crate::lsp_server::uri_path(&crate::checker::file_uri(path)?)?;
    }
    result.watch_paths.sort();
    result.watch_paths.dedup();
    Ok(result)
}

/// Ask the unchanged checker to resolve its own Python-version policy. This
/// preserves requires-python/interpreter/default behavior across checker releases
/// without duplicating PEP 440 or hard-coding the checker's latest stable version.
fn resolve_python_version(
    view: &Path,
    table: &Table,
    options: &BTreeMap<String, String>,
    project_metadata: &Table,
) -> Result<String> {
    let directory = view
        .parent()
        .context("generated view parent")?
        .join("environment-probe");
    std::fs::create_dir_all(&directory)?;
    let mut metadata = project_metadata.clone();
    let mut tool = Table::new();
    tool.insert("ty".into(), Value::Table(table.clone()));
    metadata.insert("tool".into(), Value::Table(tool));
    std::fs::write(
        directory.join("pyproject.toml"),
        toml::to_string(&metadata)?,
    )?;
    let probe = directory.join("probe.py");
    std::fs::write(&probe, "")?;
    let output = std::process::Command::new(
        options
            .get("ty")
            .context("missing --ty for Python environment discovery")?,
    )
    .args([
        "check",
        "--verbose",
        "--color",
        "never",
        "--output-format",
        "gitlab",
    ])
    .arg(&probe)
    .current_dir(&directory)
    .env("TY_UV", "0")
    .env_remove("TY_CONFIG_FILE")
    .output()
    .context("resolve Python version through official ty")?;
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "official ty environment discovery failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    let version = stderr
        .lines()
        .find_map(|line| {
            line.split_once("Python version: Python ")
                .and_then(|(_, rest)| rest.split_once(',').map(|(version, _)| version.trim()))
        })
        .context("official ty did not report its resolved Python version")?;
    ensure!(
        version.starts_with("3.") && version[2..].chars().all(|c| c.is_ascii_digit()),
        "unexpected resolved Python version {version}"
    );
    Ok(version.to_owned())
}

pub fn load(
    root: &Path,
    view: &Path,
    stubs: &Path,
    options: &BTreeMap<String, String>,
) -> Result<Config> {
    let is_editor = options.contains_key("lsp-initialization-options")
        || options.contains_key("lsp-workspace-configuration");
    let editor = editor_options(options)?;
    let editor_file = editor
        .get("configurationFile")
        .and_then(serde_json::Value::as_str)
        .map(|path| expanded_editor_path(path, root))
        .transpose()?
        .map(|path| path.to_string_lossy().into_owned());
    let explicit = options.get("ty-config").or(editor_file.as_ref());
    let (mut table, mut base, source) = discovered(root, explicit)?;
    let metadata = if is_editor {
        project_metadata(root, &base, source.as_deref(), explicit.is_some())?
    } else {
        Table::new()
    };
    if options.get("ty-config").is_none() && editor_file.is_some() {
        // The LSP config-file path is resolved against the workspace, and its
        // file-relative settings use that same ProjectMetadata root upstream.
        base = root.to_owned();
    }
    let mut native_base = table.clone();
    let mut native_inline = None;
    let mut invalid_inline = None;
    if let Some(configuration) = editor.get("configuration").filter(|v| !v.is_null()) {
        let mut native = configuration.clone();
        // Plugins are frontend options. Their opaque configuration must not be
        // validated or normalized as native ty options, nor sent to stock ty.
        let plugins = native
            .as_object_mut()
            .and_then(|object| object.remove("plugins"));
        let schema = editor_schema()?;
        if matches_editor_schema(&native, schema, schema, 0) {
            // JSON null deserializes as None for native Option fields; TOML has
            // no null representation. Validate first so invalid null map values
            // cannot disappear and accidentally turn a rejected layer valid.
            remove_null_options(&mut native);
            if let Some(plugins) = plugins.filter(|value| !value.is_null()) {
                native
                    .as_object_mut()
                    .unwrap()
                    .insert("plugins".into(), plugins);
            }
            let mut inline: Table =
                serde_json::from_value(native).context("invalid editor plugin configuration")?;
            anchor_editor_paths(&mut inline, root)?;
            native_inline = Some(inline.clone());
            merge_layer(&mut table, inline, true);
        } else {
            // Native ty rejects the whole inline layer but continues with file
            // and Python-extension settings. Preserve its original invalid JSON
            // so the native server can report the same configuration error.
            invalid_inline = Some(native);
        }
    }
    if let Some(overrides) = options.get("cli-config") {
        let overrides: Vec<String> = serde_json::from_str(overrides)?;
        let cwd = std::env::current_dir()?;
        for text in overrides {
            let mut additions: Table =
                toml::from_str(&text).context("invalid --config override")?;
            // CLI paths are relative to the invoking directory, independently
            // of the selected project's configuration file.
            if let Some(environment) = additions
                .get_mut("environment")
                .and_then(Value::as_table_mut)
            {
                for key in ["root", "extra-paths"] {
                    if let Some(value) = environment.get_mut(key) {
                        relocate_list(value, &cwd, &cwd, &cwd);
                    }
                }
                for key in ["python", "typeshed"] {
                    if let Some(Value::String(path)) = environment.get_mut(key)
                        && !Path::new(path).is_absolute()
                    {
                        *path = cwd.join(&*path).to_string_lossy().into_owned();
                    }
                }
            }
            if let Some(src) = additions.get_mut("src").and_then(Value::as_table_mut) {
                for key in ["include", "exclude"] {
                    if let Some(value) = src.get_mut(key) {
                        relocate_list(value, &cwd, &cwd, &cwd);
                    }
                }
            }
            if let Some(inline) = &mut native_inline {
                merge_layer(inline, additions.clone(), true);
            } else {
                merge_layer(&mut native_base, additions.clone(), is_editor);
            }
            // Keep the existing CLI override behavior outside editor sessions.
            merge_layer(&mut table, additions, is_editor);
        }
    }
    let mut editor_python_fallback = false;
    let mut editor_version_fallback = false;
    if let Some(python) = editor
        .pointer("/pythonExtension/activeEnvironment/executable/sysPrefix")
        .and_then(serde_json::Value::as_str)
    {
        let environment = table
            .entry("environment")
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .context("environment must be a table")?;
        if !environment.contains_key("python") {
            editor_python_fallback = !options.contains_key("python");
            environment.insert(
                "python".into(),
                Value::String(
                    expanded_editor_path(python, root)?
                        .to_string_lossy()
                        .into_owned(),
                ),
            );
        }
        // Discovery applies requires-python to the project layer before the
        // editor's lowest-precedence fallback layer. The stock checker remains
        // responsible for parsing and resolving that Python version policy.
        editor_version_fallback = !environment.contains_key("python-version")
            && !options.contains_key("python-version")
            && metadata
                .get("project")
                .and_then(|project| project.get("requires-python"))
                .is_none();
        if editor_version_fallback && let Some(version) = editor_python_version(&editor) {
            environment.insert("python-version".into(), Value::String(version));
        }
    }
    let terminal = serde_json::to_value(table.get("terminal"))?;
    let selection = Selection::new(&table, &base, root)?;
    let registration = plugin_registrations(&table, &base, root, options, source.as_deref())?;
    let plugins = registration.plugins;
    let stub_roots = registration.roots;
    // Plugins belong to this frontend, not to the stock checker configuration.
    table.remove("plugins");
    let environment = table
        .entry("environment")
        .or_insert_with(|| Value::Table(Table::new()))
        .as_table_mut()
        .context("environment must be a table")?;
    let source_roots = if let Some(roots) = environment.get("root").and_then(Value::as_array) {
        let mut roots = roots
            .iter()
            .map(|v| {
                v.as_str()
                    .context("environment.root must contain strings")
                    .map(|p| PathBuf::from(relocated(p, &base, root, root)))
            })
            .collect::<Result<Vec<_>>>()?;
        if !roots.iter().any(|p| p == root) {
            roots.push(root.to_owned());
        }
        roots
    } else {
        let mut roots = Vec::new();
        let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("");
        for candidate in [root.join("src"), root.join(name), root.join("python")] {
            if candidate.is_dir()
                && !candidate.join("__init__.py").exists()
                && !candidate.join("__init__.pyi").exists()
                && (candidate != root.join(name) || candidate.join(name).is_dir())
            {
                roots.push(candidate);
            }
        }
        roots.push(root.to_owned());
        roots
    };
    let mut dependency_roots = source_roots
        .iter()
        .filter(|path| !path.starts_with(root))
        .cloned()
        .collect::<Vec<_>>();
    if let Some(extras) = environment.get("extra-paths").and_then(Value::as_array) {
        dependency_roots.extend(
            extras
                .iter()
                .filter_map(Value::as_str)
                .map(|path| PathBuf::from(relocated(path, &base, root, root))),
        );
    }
    // Observe new environments and newly installed modules too: negative import
    // resolutions have no existing module file that can be watched individually.
    dependency_roots.push(root.join(".venv"));
    for key in ["root", "extra-paths"] {
        if let Some(value) = environment.get_mut(key) {
            relocate_list(value, &base, root, view);
        }
    }
    for key in ["python", "typeshed"] {
        if let Some(value) = environment.get_mut(key)
            && let Some(path) = value.as_str()
        {
            let path = Path::new(path);
            *value = Value::String(
                if path.is_absolute() {
                    path.to_owned()
                } else {
                    base.join(path)
                }
                .to_string_lossy()
                .into_owned(),
            );
        }
    }
    for key in ["python-version", "python-platform", "python"] {
        if let Some(value) = options.get(key) {
            environment.insert(key.to_owned(), Value::String(value.clone()));
        }
    }
    if !environment.contains_key("python") && root.join(".venv").is_dir() {
        environment.insert(
            "python".into(),
            Value::String(root.join(".venv").to_string_lossy().into_owned()),
        );
    }
    if let Some(typeshed) = environment.get("typeshed").and_then(Value::as_str) {
        dependency_roots.push(PathBuf::from(typeshed));
    }
    let python = environment
        .get("python")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("VIRTUAL_ENV").map(PathBuf::from));
    if let Some(python) = python {
        if python.is_dir() {
            dependency_roots.push(python);
        } else if let Some(prefix) = python.parent().and_then(Path::parent) {
            // Executable paths may point to a shared interpreter; observe only
            // Python libraries instead of recursively watching an entire /usr.
            let libraries = prefix.join("lib");
            if let Ok(versions) = std::fs::read_dir(&libraries) {
                for version in versions.flatten() {
                    if version.file_name().to_string_lossy().starts_with("python") {
                        dependency_roots.push(version.path());
                    }
                }
            }
            let windows = prefix.join("Lib");
            if windows.is_dir() {
                dependency_roots.push(windows);
            }
        }
    }
    dependency_roots = dependency_roots
        .into_iter()
        .map(|path| path.canonicalize().unwrap_or(path))
        .collect();
    dependency_roots.sort();
    dependency_roots.dedup();
    let configured_python_version = environment
        .get("python-version")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let host_platform = match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win32",
        "android" => "android",
        "ios" => "ios",
        _ => "linux",
    };
    let platform = environment
        .get("python-platform")
        .and_then(Value::as_str)
        .unwrap_or(host_platform)
        .to_owned();
    environment.insert("python-platform".into(), Value::String(platform.clone()));
    let extras = environment
        .entry("extra-paths")
        .or_insert_with(|| Value::Array(vec![]))
        .as_array_mut()
        .context("environment.extra-paths must be an array")?;
    // Native plugin overlays follow user extras and all first-party roots.
    // The stock checker has no overlay slot, so Session filters stdlib shadows
    // before placing the remaining overlay after these explicit search roots.
    extras.extend(source_roots.iter().map(|path| {
        Value::String(if path == root {
            view.to_string_lossy().into_owned()
        } else {
            relocated(&path.to_string_lossy(), root, root, view)
        })
    }));
    extras.push(Value::String(stubs.to_string_lossy().into_owned()));
    // Explicit root paths prevent ty's temporary workspace from discovering unrelated sources.
    environment.entry("root").or_insert_with(|| {
        Value::Array(
            source_roots
                .iter()
                .map(|p| Value::String(relocated(&p.to_string_lossy(), root, root, view)))
                .collect(),
        )
    });
    // The upstream server does not know which files were explicit CLI paths.
    // Preserve that selection in the generated configuration as exact file
    // exceptions; directory descendants continue to obey ordinary exclusions.
    if options
        .get("force-exclude")
        .is_none_or(|value| value != "true")
        && let Some(paths) = options.get("explicit-files")
    {
        let paths: Vec<PathBuf> = serde_json::from_str(paths)?;
        let src = table
            .entry("src")
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .context("src must be a table")?;
        for path in paths.into_iter().filter(|path| path.is_file()) {
            if let Some(includes) = src.get_mut("include").and_then(Value::as_array_mut) {
                includes.push(Value::String(path.to_string_lossy().into_owned()));
            }
            src.entry("exclude")
                .or_insert_with(|| Value::Array(vec![]))
                .as_array_mut()
                .context("src.exclude must be an array")?
                .push(Value::String(format!("!{}", path.display())));
        }
    }
    if let Some(src) = table.get_mut("src").and_then(Value::as_table_mut) {
        for key in ["include", "exclude"] {
            if let Some(value) = src.get_mut(key) {
                relocate_list(value, &base, root, view);
                if key == "exclude" {
                    preserve_negated_globs(value, view.parent().context("checker root")?)?;
                }
                #[cfg(windows)]
                portable_windows_globs(value, view.parent().context("checker root")?)?;
            }
        }
    }
    if let Some(overrides) = table.get_mut("overrides").and_then(Value::as_array_mut) {
        for entry in overrides {
            if let Some(entry) = entry.as_table_mut() {
                for key in ["include", "exclude"] {
                    if let Some(value) = entry.get_mut(key) {
                        relocate_list(value, &base, root, view);
                        if key == "exclude" {
                            preserve_negated_globs(value, view.parent().context("checker root")?)?;
                        }
                        #[cfg(windows)]
                        portable_windows_globs(value, view.parent().context("checker root")?)?;
                    }
                }
            }
        }
    }
    let python_version = match configured_python_version {
        Some(version) => version,
        None if is_editor => resolve_python_version(view, &table, options, &metadata)?,
        None => resolve_python_version(
            view,
            &table,
            options,
            &project_metadata(root, root, None, false)?,
        )?,
    };
    table
        .get_mut("environment")
        .and_then(Value::as_table_mut)
        .unwrap()
        .insert(
            "python-version".into(),
            Value::String(python_version.clone()),
        );
    // The native editor server must still receive these as Python-extension
    // fallbacks, not explicit config values. Apart from precedence this preserves
    // its diagnostic explanation of where the selected Python version came from.
    let environment = table
        .get_mut("environment")
        .and_then(Value::as_table_mut)
        .unwrap();
    if editor_python_fallback {
        environment.remove("python");
    }
    if editor_version_fallback {
        environment.remove("python-version");
    }
    let typeshed = environment
        .get("typeshed")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let editor_configuration = if is_editor {
        // Keep native configuration layers separate. Re-serializing the merged
        // options as a file loses editor provenance and forwarding inline arrays
        // on top of that merged file would apply every item twice.
        if let Some(inline) = &mut native_inline {
            let environment = inline
                .entry("environment")
                .or_insert_with(|| Value::Table(Table::new()))
                .as_table_mut()
                .context("environment must be a table")?;
            for key in ["python-version", "python-platform", "python"] {
                if let Some(value) = options.get(key) {
                    environment.insert(key.into(), Value::String(value.clone()));
                }
            }
            relocate_native_layer(inline, root, root, view)?;
        }
        relocate_native_layer(&mut native_base, &base, root, view)?;
        let native_environment = native_base
            .entry("environment")
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .context("environment must be a table")?;
        for key in ["python-version", "python-platform", "python"] {
            if let Some(value) = options.get(key) {
                native_environment.insert(key.into(), Value::String(value.clone()));
            }
        }
        if !native_environment.contains_key("python")
            && !editor_python_fallback
            && root.join(".venv").is_dir()
        {
            native_environment.insert(
                "python".into(),
                Value::String(root.join(".venv").to_string_lossy().into_owned()),
            );
        }

        let inline_has_root = native_inline
            .as_ref()
            .and_then(|inline| inline.get("environment"))
            .is_some_and(|environment| environment.get("root").is_some());
        if !inline_has_root {
            native_environment.entry("root").or_insert_with(|| {
                Value::Array(
                    source_roots
                        .iter()
                        .map(|path| {
                            Value::String(if path == root {
                                view.to_string_lossy().into_owned()
                            } else {
                                relocated(&path.to_string_lossy(), root, root, view)
                            })
                        })
                        .collect(),
                )
            });
        }
        if !native_environment.contains_key("python-version")
            && metadata
                .get("project")
                .and_then(|project| project.get("requires-python"))
                .is_some()
        {
            let version = resolve_python_version(view, &native_base, options, &metadata)?;
            native_base
                .get_mut("environment")
                .and_then(Value::as_table_mut)
                .unwrap()
                .insert("python-version".into(), Value::String(version));
        }
        // ty concatenates file extras before inline extras. Append generated
        // roots to the last valid layer, after every user-configured extra.
        let final_layer = native_inline.as_mut().unwrap_or(&mut native_base);
        let extras = final_layer
            .entry("environment")
            .or_insert_with(|| Value::Table(Table::new()))
            .as_table_mut()
            .context("environment must be a table")?
            .entry("extra-paths")
            .or_insert_with(|| Value::Array(vec![]))
            .as_array_mut()
            .context("environment.extra-paths must be an array")?;
        extras.extend(source_roots.iter().map(|path| {
            Value::String(if path == root {
                view.to_string_lossy().into_owned()
            } else {
                relocated(&path.to_string_lossy(), root, root, view)
            })
        }));
        extras.push(Value::String(stubs.to_string_lossy().into_owned()));
        table = native_base;
        invalid_inline.or(native_inline.map(serde_json::to_value).transpose()?)
    } else {
        None
    };
    Ok(Config {
        text: toml::to_string(&table)?,
        editor_configuration,
        python_version,
        platform,
        source,
        source_roots,
        dependency_roots,
        selection,
        plugins,
        plugin_diagnostics: registration.diagnostics,
        plugin_watch_paths: registration.watch_paths,
        stub_roots,
        typeshed,
        terminal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_fixture(name: &str) -> Result<PathBuf> {
        let root = std::env::temp_dir().join(format!("ty-editor-{name}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("generated/project"))?;
        std::fs::create_dir_all(root.join("generated/stubs"))?;
        Ok(root.canonicalize()?)
    }

    #[test]
    fn editor_schema_is_fully_supported_and_rejects_schema_drift() -> Result<()> {
        let schema = editor_schema()?;
        audit_editor_schema(schema, schema)?;
        for changed in [
            serde_json::json!({"definitions": {"future": {"minimum": 0}}}),
            serde_json::json!({"$ref": "https://example.invalid/schema"}),
            serde_json::json!({"$ref": "#/definitions/missing"}),
            serde_json::json!({"type": "future-type"}),
            serde_json::json!({"items": [{"type": "string"}]}),
        ] {
            assert!(
                audit_editor_schema(&changed, &changed).is_err(),
                "{changed}"
            );
        }
        Ok(())
    }

    #[test]
    fn editor_null_options_use_file_or_extension_fallbacks() -> Result<()> {
        let root = editor_fixture("null-options")?;
        std::fs::write(root.join("ty.toml"), "[rules]\ninvalid-assignment='warn'\n")?;
        let options = BTreeMap::from([(
            "lsp-workspace-configuration".into(),
            serde_json::json!({
                "configuration": {
                    "environment": {"python-version": null, "extra-paths": null},
                    "src": {"include": null}, "rules": null, "plugins": null
                },
                "pythonExtension": {"activeEnvironment": {
                    "executable": {"sysPrefix": root.join("selected")},
                    "version": {"major": 3, "minor": 13}
                }}
            })
            .to_string(),
        )]);
        let config = load(
            &root,
            &root.join("generated/project"),
            &root.join("generated/stubs"),
            &options,
        )?;
        assert_eq!(config.python_version, "3.13");
        assert!(config.selection.includes(&root.join("any.py"), ""));
        let base: Table = toml::from_str(&config.text)?;
        assert_eq!(base["rules"]["invalid-assignment"].as_str(), Some("warn"));
        assert!(base["environment"].get("python-version").is_none());
        assert_eq!(
            config.editor_configuration,
            Some(
                serde_json::json!({"environment": {"extra-paths": [root.join("generated/project"), root.join("generated/stubs")]}, "src": {}})
            )
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn invalid_editor_ty_layer_is_ignored_wholly_and_preserved_for_native_error() -> Result<()> {
        let root = editor_fixture("invalid-options")?;
        std::fs::write(root.join("ty.toml"), "[rules]\ninvalid-assignment='warn'\n")?;
        for invalid in [
            serde_json::json!({"environment": {"python-version": "3.99"}}),
            serde_json::json!({"environment": "invalid"}),
            serde_json::json!({"environment": {"extra-paths": [null]}}),
            serde_json::json!({"src": {"include": [1]}}),
            serde_json::json!({"rules": {"invalid-assignment": null}}),
            serde_json::json!({"rules": {"invalid-assignment": "invalid"}}),
            serde_json::json!({"unknown-option": true}),
        ] {
            let mut configuration = serde_json::json!({
                "environment": {"python-version": "3.14"},
                "src": {"include": ["inline-only"]},
                "rules": {"invalid-assignment": "error"}
            });
            configuration
                .as_object_mut()
                .unwrap()
                .extend(invalid.as_object().unwrap().clone());
            let options = BTreeMap::from([(
                "lsp-workspace-configuration".into(),
                serde_json::json!({"configuration": configuration, "pythonExtension": {"activeEnvironment": {
                    "executable": {"sysPrefix": root.join("selected")},
                    "version": {"major": 3, "minor": 13}
                }}}).to_string(),
            )]);
            let config = load(
                &root,
                &root.join("generated/project"),
                &root.join("generated/stubs"),
                &options,
            )?;
            assert_eq!(config.python_version, "3.13", "{configuration}");
            assert!(
                config.selection.includes(&root.join("any.py"), ""),
                "{configuration}"
            );
            let base: Table = toml::from_str(&config.text)?;
            assert_eq!(base["rules"]["invalid-assignment"].as_str(), Some("warn"));
            assert_eq!(config.editor_configuration.as_ref(), Some(&configuration));
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn editor_inline_options_override_files_and_append_arrays() -> Result<()> {
        let root = editor_fixture("layers")?;
        std::fs::write(
            root.join("ty.toml"),
            "[environment]\npython-version='3.12'\nextra-paths=['file-shared']\n[src]\ninclude=['file-src']\n[rules]\ninvalid-assignment='warn'\n[[overrides]]\ninclude=['file-tests/**']\n[overrides.rules]\nunknown-argument='ignore'\n",
        )?;
        let mut options = BTreeMap::from([
            (
                "lsp-initialization-options".into(),
                serde_json::json!({"configuration": {
                    "environment": {"python-version": "3.13", "extra-paths": ["inline-shared"], "root": ["inline-root"]},
                    "src": {"include": ["inline-src"]},
                    "rules": {"invalid-assignment": "error"},
                    "overrides": [{"include": ["inline-tests/**"], "rules": {"unknown-argument": "warn"}}]
                }}).to_string(),
            ),
            (
                "lsp-workspace-configuration".into(),
                serde_json::json!({"configuration": {
                    "environment": {"python-version": "3.10"},
                    "rules": {"unresolved-reference": "ignore"}
                }}).to_string(),
            ),
        ]);
        let view = root.join("generated/project");
        let stubs = root.join("generated/stubs");
        let config = load(&root, &view, &stubs, &options)?;
        let mut table: Table = toml::from_str(&config.text)?;
        assert_eq!(table["rules"]["invalid-assignment"].as_str(), Some("warn"));
        assert_eq!(
            table["environment"]["python-version"].as_str(),
            Some("3.12")
        );
        assert_eq!(
            table["environment"]["extra-paths"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(table["environment"].get("root").is_none());
        assert_eq!(table["overrides"].as_array().unwrap().len(), 1);
        let inline = config.editor_configuration.as_ref().unwrap();
        assert_eq!(inline["environment"]["python-version"], "3.13");
        assert_eq!(
            inline["environment"]["extra-paths"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
        assert_eq!(
            inline["environment"]["root"][0].as_str(),
            view.join("inline-root").to_str()
        );
        merge_layer(&mut table, serde_json::from_value(inline.clone())?, true);
        assert_eq!(config.python_version, "3.13");
        assert_eq!(table["rules"]["invalid-assignment"].as_str(), Some("error"));
        // ClientOptions.configuration is atomic between initialization and
        // workspace settings, even though ty options merge with the file layer.
        assert!(table["rules"].get("unresolved-reference").is_none());
        let extras = table["environment"]["extra-paths"].as_array().unwrap();
        assert_eq!(extras.len(), 5);
        assert_eq!(extras[0].as_str(), view.join("file-shared").to_str());
        assert_eq!(extras[1].as_str(), view.join("inline-shared").to_str());
        assert_eq!(extras[2].as_str(), view.join("inline-root").to_str());
        assert_eq!(extras[3].as_str(), view.to_str());
        assert_eq!(extras[4].as_str(), stubs.to_str());
        assert_eq!(table["overrides"].as_array().unwrap().len(), 2);
        assert!(config.selection.includes(&root.join("file-src/a.py"), ""));
        assert!(config.selection.includes(&root.join("inline-src/a.py"), ""));
        assert!(!config.selection.includes(&root.join("other/a.py"), ""));
        options.insert("python-version".into(), "3.11".into());
        assert_eq!(load(&root, &view, &stubs, &options)?.python_version, "3.11");
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn editor_configuration_file_expands_paths_and_keeps_workspace_origin() -> Result<()> {
        let root = editor_fixture("file")?;
        std::fs::create_dir_all(root.join("settings"))?;
        std::fs::write(
            root.join("ty.toml"),
            "[environment]\npython-version='3.10'\n",
        )?;
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nrequires-python='>=3.11'\n",
        )?;
        std::fs::write(
            root.join("settings/editor.toml"),
            "[environment]\nextra-paths=['shared']\n",
        )?;
        let name = format!(
            "TY_COMPANION_MISSING_CONFIG_VARIABLE_{}",
            std::process::id()
        );
        assert!(std::env::var_os(&name).is_none());
        let options = BTreeMap::from([(
            "lsp-workspace-configuration".into(),
            serde_json::json!({
                "configurationFile": format!("${{{name}:-settings/editor.toml}}"),
                "pythonExtension": {"activeEnvironment": {
                    "executable": {"sysPrefix": root.join("selected-environment")},
                    "version": {"major": 3, "minor": 13}
                }}
            })
            .to_string(),
        )]);
        let view = root.join("generated/project");
        let config = load(&root, &view, &root.join("generated/stubs"), &options)?;
        assert_eq!(config.source, Some(root.join("settings/editor.toml")));
        // An explicitly selected config bypasses automatic project metadata.
        assert_eq!(config.python_version, "3.13");
        let table: Table = toml::from_str(&config.text)?;
        assert_eq!(
            table["environment"]["extra-paths"][0].as_str(),
            view.join("shared").to_str()
        );
        assert!(table["environment"].get("python-version").is_none());
        assert!(table["environment"].get("python").is_none());
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(
                expanded_editor_path("~/editor.toml", &root)?,
                Path::new(&home).join("editor.toml")
            );
            assert_eq!(
                expanded_editor_path("${HOME}/editor.toml", &root)?,
                Path::new(&home).join("editor.toml")
            );
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn editor_python_is_fallback_and_drives_plugin_discovery() -> Result<()> {
        let root = editor_fixture("environment")?;
        let selected = root.join("selected-environment");
        let package = selected.join("lib/python3.13/site-packages/example");
        std::fs::create_dir_all(&package)?;
        std::fs::write(
            package.join("ty-plugin.json"),
            r#"{"id":"example","name":"example","version":"0.1.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{},"claims":{},"stub-overlays":[]}"#,
        )?;
        std::fs::write(package.join("plugin.py"), "pass")?;
        std::fs::write(root.join("ty.toml"), "[plugins]\nauto-discover=true\n")?;
        let mut editor = serde_json::json!({"pythonExtension": {"activeEnvironment": {
            "executable": {"sysPrefix": selected}, "version": {"major": 3, "minor": 13}
        }}});
        let mut options =
            BTreeMap::from([("lsp-workspace-configuration".into(), editor.to_string())]);
        let view = root.join("generated/project");
        let stubs = root.join("generated/stubs");
        let config = load(&root, &view, &stubs, &options)?;
        assert_eq!(config.python_version, "3.13");
        assert_eq!(config.plugins.len(), 1);
        assert!(config.dependency_roots.contains(&selected));
        let table: Table = toml::from_str(&config.text)?;
        assert!(table["environment"].get("python").is_none());
        assert!(table["environment"].get("python-version").is_none());
        std::fs::write(
            root.join("ty.toml"),
            "[environment]\npython='project-environment'\npython-version='3.12'\n",
        )?;
        let config = load(&root, &view, &stubs, &options)?;
        let table: Table = toml::from_str(&config.text)?;
        assert_eq!(config.python_version, "3.12");
        assert_eq!(
            table["environment"]["python"].as_str(),
            root.join("project-environment").to_str()
        );
        editor["configuration"] = serde_json::json!({"environment": {"python": "inline-environment", "python-version": "3.14"}});
        options.insert("lsp-workspace-configuration".into(), editor.to_string());
        let config = load(&root, &view, &stubs, &options)?;
        let table: Table = toml::from_str(&config.text)?;
        assert_eq!(config.python_version, "3.14");
        assert_eq!(
            table["environment"]["python"].as_str(),
            root.join("project-environment").to_str()
        );
        assert_eq!(
            config.editor_configuration.as_ref().unwrap()["environment"]["python"].as_str(),
            root.join("inline-environment").to_str()
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn editor_layers_keep_plugin_configuration_opaque_and_ignore_unsupported_versions() -> Result<()>
    {
        let mut table: Table =
            toml::from_str("[plugins.config.example]\nold=true\nvalues=['old']\n")?;
        merge_layer(
            &mut table,
            toml::from_str("[plugins.config.example]\nvalues=['new']\n")?,
            true,
        );
        assert!(table["plugins"]["config"]["example"].get("old").is_none());
        assert_eq!(
            table["plugins"]["config"]["example"]["values"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        for version in [
            serde_json::json!({"major": 2, "minor": 7}),
            serde_json::json!({"major": 3, "minor": 99}),
            serde_json::json!({"major": 3, "minor": -1}),
        ] {
            assert!(editor_python_version(&serde_json::json!({"pythonExtension": {"activeEnvironment": {"version": version}}})).is_none());
        }
        Ok(())
    }

    #[test]
    fn windows_glob_paths_normalize_disk_and_unc_prefixes() {
        for (original, expected) in [
            (r"C:\work\project\src", "C:/work/project/src"),
            (r"\\?\C:\work\project\src", "C:/work/project/src"),
            (r"\\server\share\src", "//server/share/src"),
            (r"\\?\UNC\server\share\src", "//server/share/src"),
            (
                r"\\?\C:\work\project\**\test_[ab]?.py",
                "C:/work/project/**/test_[ab]?.py",
            ),
        ] {
            assert_eq!(windows_glob_path(original), expected);
        }
    }

    #[test]
    fn windows_globs_anchor_relatively_without_losing_negation_or_wildcards() {
        for (pattern, root, expected) in [
            (
                r"\\?\C:\work\output\project\src",
                r"C:\work\output",
                "project/src",
            ),
            (
                r"C:\work\output\project\tests\**",
                r"\\?\C:\work\output",
                "project/tests/**",
            ),
            (
                r"!\\?\C:\work\output\project\**\test_[ab]?.py",
                r"C:\work\output",
                "!project/**/test_[ab]?.py",
            ),
            (
                r"\\?\UNC\server\share\output\project\src",
                r"\\server\share\output",
                "project/src",
            ),
            (
                r"!\\server\other\**\test_?.py",
                r"\\?\UNC\server\share\output",
                "!//server/other/**/test_?.py",
            ),
            (
                r"c:\WORK\output\project\src",
                r"C:\work\output",
                "project/src",
            ),
            (r"\\?\C:\work\output", r"C:\work\output", "."),
        ] {
            assert_eq!(windows_glob_pattern(pattern, root).unwrap(), expected);
        }
        for pattern in [
            r"C:\work\output-other\src",
            r"D:\work\output\src",
            "../shared",
            "!../shared/**",
        ] {
            let error = windows_glob_pattern(pattern, r"C:\work\output").unwrap_err();
            assert!(
                error.to_string().contains("outside checker root"),
                "{error}"
            );
        }
    }
    #[test]
    fn source_filters_match_directories_negation_and_scripts() -> Result<()> {
        let root = Path::new("/workspace");
        let table: Table = toml::from_str(
            "[src]\ninclude=['src']\nexclude=['src/generated', '!src/generated/keep.py']\nexclude-scripts=true\n",
        )?;
        let selection = Selection::new(&table, root, root)?;
        assert!(selection.includes(&root.join("src/app.py"), "x=1"));
        assert!(!selection.includes(&root.join("tests/test.py"), "x=1"));
        assert!(!selection.includes(&root.join("src/generated/bad.py"), "x=1"));
        assert!(selection.includes(&root.join("src/generated/keep.py"), "x=1"));
        assert!(!selection.includes(
            &root.join("src/script.py"),
            "# /// script\n# dependencies=[]\n# ///\n"
        ));
        assert!(!selection.includes(&root.join("src/.venv/a.py"), "x=1"));
        Ok(())
    }

    #[test]
    fn plugin_diagnostic_origins_use_parsed_spans_and_override_provenance() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("ty-plugin-config-origins-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let path = root.join("pyproject.toml");
        let text = "# café 😀\n[tool.ty.plugins]\nenabled=false\nplugin=[{id='café', path='missing.py',runtime='monty'}]\n";
        std::fs::write(&path, text)?;
        let origin = PluginOrigin::new(Some(&path), &BTreeMap::new())?;
        let diagnostic = origin.diagnostic(
            0,
            "id",
            "disabled".into(),
            "plugin configuration",
            true,
            None,
        );
        let start = text.find("'café'").unwrap();
        assert_eq!(
            origin.entries[0]["id"].span(),
            start..start + "'café'".len()
        );
        assert_eq!(
            diagnostic["location"]["positions"]["begin"],
            serde_json::json!({"line":4,"column":13})
        );
        assert_eq!(
            diagnostic["location"]["positions"]["end"],
            serde_json::json!({"line":4,"column":19})
        );
        let options = BTreeMap::from([(
            "cli-config".into(),
            serde_json::to_string(&["plugins.plugin=[{id='café',path='missing.py'}]"])?,
        )]);
        let overridden = PluginOrigin::new(Some(&path), &options)?.diagnostic(
            0,
            "id",
            "disabled".into(),
            "plugin configuration",
            true,
            None,
        );
        assert_eq!(overridden["location"]["path"], "");
        assert_eq!(overridden["description"], "plugin-configuration: disabled");
        assert_eq!(
            overridden["details"],
            "info: The plugin option was specified on the CLI."
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn invalid_configured_plugin_paths_remain_watched_for_recovery() -> Result<()> {
        let root =
            std::env::temp_dir().join(format!("ty-plugin-config-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let table: Table = toml::from_str(
            "[plugins]\nenabled=true\n[[plugins.plugin]]\nid='missing'\npath='missing.py'\nmanifest-path='missing.json'\nruntime='monty'\ntrusted=true\n",
        )?;
        let result = plugin_registrations(&table, &root, &root, &BTreeMap::new(), None)?;
        assert!(result.plugins.is_empty());
        assert_eq!(result.diagnostics.len(), 1);
        assert!(
            result.diagnostics[0]["description"]
                .as_str()
                .unwrap()
                .contains("artifact path")
        );
        assert!(result.watch_paths.contains(&root.join("missing.py")));
        assert!(result.watch_paths.contains(&root.join("missing.json")));
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn installed_plugins_require_opt_in_and_preserve_configuration() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ty-plugin-discovery-{}", std::process::id()));
        let package = root.join(".venv/lib/python3.13/site-packages/example");
        std::fs::create_dir_all(&package)?;
        std::fs::write(
            package.join("ty-plugin.json"),
            r#"{"id":"example","name":"example","version":"0.1.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{},"claims":{},"stub-overlays":[]}"#,
        )?;
        std::fs::write(package.join("plugin.py"), "pass")?;
        let table: Table = toml::from_str(
            "[plugins]\nauto-discover=true\n[plugins.config.example]\nmode='strict'\n",
        )?;
        let plugins = plugin_registrations(&table, &root, &root, &BTreeMap::new(), None)?.plugins;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].2["mode"], "strict");
        let omitted = Table::new();
        assert!(
            plugin_registrations(&omitted, &root, &root, &BTreeMap::new(), None)?
                .plugins
                .is_empty()
        );
        let disabled: Table = toml::from_str("[plugins]\nauto-discover=false\n")?;
        assert!(
            plugin_registrations(&disabled, &root, &root, &BTreeMap::new(), None)?
                .plugins
                .is_empty()
        );
        let disabled_worker: Table = toml::from_str(
            "[plugins]\nenabled=false\nauto-discover=false\nmonty-mode='worker'\n[[plugins.plugin]]\nid='obsolete'\nruntime='wasm'\npath='missing.wasm'\n",
        )?;
        assert!(
            plugin_registrations(&disabled_worker, &root, &root, &BTreeMap::new(), None)?
                .plugins
                .is_empty()
        );
        let untrusted: Table = toml::from_str(
            "[plugins]\nenabled=true\n[[plugins.plugin]]\nid='example'\nruntime='monty'\npath='missing.py'\n",
        )?;
        assert!(
            plugin_registrations(&untrusted, &root, &root, &BTreeMap::new(), None)?.diagnostics[0]
                ["description"]
                .as_str()
                .unwrap()
                .contains("artifact path")
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn installed_overlay_discovery_respects_explicit_plugin_override() -> Result<()> {
        let root = editor_fixture("discovery-precedence")?;
        let package = root.join(".venv/lib/python3.13/site-packages/example");
        std::fs::create_dir_all(package.join("stubs"))?;
        let mut manifest = serde_json::json!({
            "id":"example", "name":"example", "version":"0.1.0",
            "protocol-version":{"major":0,"minor":7},
            "ty-compatibility":{"requirement":">=0.86.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},
            "capabilities":{"stub-overlays":true},
            "stub-overlays":[{"module":"external","path":"stubs/external.pyi"}]
        });
        std::fs::write(
            package.join("ty-plugin.json"),
            serde_json::to_vec(&manifest)?,
        )?;
        std::fs::write(package.join("plugin.py"), "pass")?;
        std::fs::write(package.join("stubs/external.pyi"), "value: int\n")?;
        let table: Table = toml::from_str("[plugins]\nauto-discover=true\n")?;
        let discovered = plugin_registrations(&table, &root, &root, &BTreeMap::new(), None)?;
        assert!(discovered.diagnostics.is_empty());
        assert_eq!(discovered.plugins.len(), 1);
        assert_eq!(discovered.roots, [package.join("stubs").canonicalize()?]);
        assert!(discovered.watch_paths.contains(&package.join("stubs")));
        manifest["stub-overlays"] = serde_json::json!([]);
        std::fs::write(root.join("explicit.json"), serde_json::to_vec(&manifest)?)?;
        std::fs::write(root.join("explicit.py"), "pass")?;
        let table: Table = toml::from_str(
            "[plugins]\nenabled=true\nauto-discover=true\n[[plugins.plugin]]\nid='example'\nruntime='monty'\ntrusted=true\npath='explicit.py'\nmanifest-path='explicit.json'\nconfig={mode='explicit'}\n",
        )?;
        let explicit = plugin_registrations(&table, &root, &root, &BTreeMap::new(), None)?;
        assert!(
            explicit.diagnostics.is_empty(),
            "{:?}",
            explicit.diagnostics
        );
        assert_eq!(explicit.plugins.len(), 1);
        assert_eq!(explicit.plugins[0].1, root.join("explicit.py"));
        assert_eq!(explicit.plugins[0].2["mode"], "explicit");
        assert!(explicit.roots.is_empty());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
    fn official_checker_resolves_requires_python_and_explicit_precedence() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ty-requires-python-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='version-probe'\nversion='0.0.0'\nrequires-python='>=3.12'\n",
        )?;
        let view = root.join("generated/project");
        let stubs = root.join("generated/stubs");
        std::fs::create_dir_all(&view)?;
        std::fs::create_dir_all(&stubs)?;
        let mut options =
            BTreeMap::from([("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?)]);
        assert_eq!(load(&root, &view, &stubs, &options)?.python_version, "3.12");
        let selected = root.join("selected-environment");
        std::fs::create_dir_all(selected.join("lib/python3.13/site-packages"))?;
        options.insert(
            "lsp-initialization-options".into(),
            serde_json::json!({"pythonExtension": {"activeEnvironment": {
                "executable": {"sysPrefix": selected},
                "version": {"major": 3, "minor": 13}
            }}})
            .to_string(),
        );
        let config = load(&root, &view, &stubs, &options)?;
        assert_eq!(config.python_version, "3.12");
        let table: Table = toml::from_str(&config.text)?;
        assert_eq!(
            table["environment"]["python-version"].as_str(),
            Some("3.12")
        );
        assert!(table["environment"].get("python").is_none());
        options.insert(
            "cli-config".into(),
            serde_json::to_string(&["environment.python-version='3.13'"])?,
        );
        assert_eq!(load(&root, &view, &stubs, &options)?.python_version, "3.13");
        options.remove("cli-config");
        std::fs::write(
            root.join("pyproject.toml"),
            "[project]\nname='version-probe'\nversion='0.0.0'\n",
        )?;
        let config = load(&root, &view, &stubs, &options)?;
        assert_eq!(config.python_version, "3.13");
        let table: Table = toml::from_str(&config.text)?;
        assert!(table["environment"].get("python-version").is_none());
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn preserves_rules_and_relocates_project_paths() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ty-config-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.ty.environment]\npython-version='3.12'\npython-platform='win32'\nextra-paths=['./shared']\npython='./.venv'\n[tool.ty.rules]\ninvalid-assignment='warn'\n[[tool.ty.overrides]]\ninclude=['tests/**']\n[tool.ty.overrides.rules]\nunknown-argument='ignore'\n",
        )?;
        let view = root.join("generated");
        let stubs = root.join("stubs");
        let config = load(&root, &view, &stubs, &BTreeMap::new())?;
        let value: Table = toml::from_str(&config.text)?;
        assert_eq!(config.python_version, "3.12");
        assert_eq!(config.platform, "win32");
        assert!(config.dependency_roots.contains(&root.join("shared")));
        assert!(config.dependency_roots.contains(&root.join(".venv")));
        assert_eq!(value["rules"]["invalid-assignment"].as_str(), Some("warn"));
        assert_eq!(
            value["environment"]["extra-paths"][0].as_str(),
            Some(view.join("shared").to_str().unwrap())
        );
        assert_eq!(
            value["environment"]["python"].as_str(),
            Some(root.join("./.venv").to_str().unwrap())
        );
        #[cfg(windows)]
        let expected_glob = "generated/tests/**".to_owned();
        #[cfg(not(windows))]
        let expected_glob = view.join("tests/**").to_string_lossy().into_owned();
        assert_eq!(
            value["overrides"][0]["include"][0].as_str(),
            Some(expected_glob.as_str())
        );
        std::fs::write(
            root.join("ty.toml"),
            "[environment]\npython-version='3.14'\n",
        )?;
        assert_eq!(
            load(&root, &view, &stubs, &BTreeMap::new())?.python_version,
            "3.14"
        );
        assert_eq!(
            load(
                &root,
                &view,
                &stubs,
                &BTreeMap::from([("python-version".into(), "3.11".into())])
            )?
            .python_version,
            "3.11"
        );
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
