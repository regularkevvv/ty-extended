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
    pub python_version: String,
    pub platform: String,
    pub source: Option<PathBuf>,
    pub source_roots: Vec<PathBuf>,
    pub dependency_roots: Vec<PathBuf>,
    pub selection: Selection,
    pub plugins: Vec<PluginRegistration>,
    pub stub_roots: Vec<PathBuf>,
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
    normalized
        .strip_prefix(root)
        .map_or(normalized.clone(), |suffix| view.join(suffix))
        .to_string_lossy()
        .into_owned()
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
        let target: Vec<_> = Path::new(path).components().collect();
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

fn plugin_registrations(
    table: &Table,
    base: &Path,
    root: &Path,
    options: &BTreeMap<String, String>,
) -> Result<(Vec<PluginRegistration>, Vec<PathBuf>)> {
    let mut roots = Vec::new();
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
        return Ok((vec![(manifest, artifact, config)], roots));
    }
    let Some(plugins) = table.get("plugins").and_then(Value::as_table) else {
        return Ok((Vec::new(), roots));
    };
    let mut registrations = Vec::new();
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
    if plugins
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        for entry in &configured {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .context("plugin entry requires id")?;
            ensure!(
                entry
                    .get("trusted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                "Plugin `{id}` is not trusted to execute locally; set trusted=true for this plugin"
            );
            let runtime = entry
                .get("runtime")
                .and_then(Value::as_str)
                .unwrap_or("wasm");
            ensure!(
                runtime == "monty",
                "Plugin `{id}` uses {runtime}; this companion requires a Monty artifact and runtime='monty'"
            );
            let artifact = PathBuf::from(relocated(
                entry
                    .get("path")
                    .and_then(Value::as_str)
                    .context("plugin requires path")?,
                base,
                root,
                root,
            ))
            .canonicalize()?;
            let manifest = entry
                .get("manifest-path")
                .and_then(Value::as_str)
                .map(|p| PathBuf::from(relocated(p, base, root, root)))
                .unwrap_or_else(|| artifact.clone())
                .canonicalize()?;
            let parsed: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
            ensure!(
                parsed["id"].as_str() == Some(id),
                "Configured plugin id `{id}` differs from its manifest"
            );
            if let Some(stubs) = entry.get("stub-overlay-path").and_then(Value::as_str) {
                ensure!(
                    parsed
                        .pointer("/capabilities/stub-overlays")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    "Plugin `{id}` configures stubs without stub-overlays capability"
                );
                roots.push(PathBuf::from(relocated(stubs, base, root, root)).canonicalize()?);
            }
            let config = entry
                .get("config")
                .map(serde_json::to_value)
                .transpose()?
                .unwrap_or_else(|| serde_json::json!({}));
            registrations.push((manifest, artifact, config));
        }
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
                let Some(id) = parsed["id"].as_str().filter(|id| !id.is_empty()) else {
                    continue;
                };
                if !seen.insert(id.to_owned()) {
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
                ensure!(
                    artifact.is_file(),
                    "Installed plugin `{id}` artifact does not exist: {}",
                    artifact.display()
                );
                if parsed["stub-overlays"]
                    .as_array()
                    .is_some_and(|v| !v.is_empty())
                {
                    let stubs = package.join("stubs");
                    ensure!(
                        stubs.is_dir(),
                        "Installed plugin `{id}` stub overlay is missing"
                    );
                    roots.push(stubs.canonicalize()?);
                }
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
    Ok((registrations, roots))
}

/// Ask the unchanged checker to resolve its own Python-version policy. This
/// preserves requires-python/interpreter/default behavior across checker releases
/// without duplicating PEP 440 or hard-coding the checker's latest stable version.
fn resolve_python_version(
    root: &Path,
    view: &Path,
    table: &Table,
    options: &BTreeMap<String, String>,
) -> Result<String> {
    let directory = view
        .parent()
        .context("generated view parent")?
        .join("environment-probe");
    std::fs::create_dir_all(&directory)?;
    let mut metadata = Table::new();
    for parent in root.ancestors() {
        let path = parent.join("pyproject.toml");
        if path.is_file() {
            let original: Table = toml::from_str(&std::fs::read_to_string(path)?)?;
            if let Some(project) = original.get("project") {
                metadata.insert("project".into(), project.clone());
            }
            break;
        }
    }
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
    let (mut table, base, source) = discovered(root, options.get("ty-config"))?;
    if let Some(overrides) = options.get("cli-config") {
        fn merge(table: &mut Table, additions: Table) {
            for (key, value) in additions {
                if let Value::Table(additions) = value {
                    if let Some(Value::Table(existing)) = table.get_mut(&key) {
                        merge(existing, additions);
                    } else {
                        table.insert(key, Value::Table(additions));
                    }
                } else {
                    table.insert(key, value);
                }
            }
        }
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
            merge(&mut table, additions);
        }
    }
    let terminal = serde_json::to_value(table.get("terminal"))?;
    let selection = Selection::new(&table, &base, root)?;
    let (plugins, stub_roots) = plugin_registrations(&table, &base, root, options)?;
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
    extras.insert(0, Value::String(view.to_string_lossy().into_owned()));
    extras.insert(0, Value::String(stubs.to_string_lossy().into_owned()));
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
                    }
                }
            }
        }
    }
    let python_version = match configured_python_version {
        Some(version) => version,
        None => resolve_python_version(root, view, &table, options)?,
    };
    table
        .get_mut("environment")
        .and_then(Value::as_table_mut)
        .unwrap()
        .insert(
            "python-version".into(),
            Value::String(python_version.clone()),
        );
    Ok(Config {
        text: toml::to_string(&table)?,
        python_version,
        platform,
        source,
        source_roots,
        dependency_roots,
        selection,
        plugins,
        stub_roots,
        terminal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn installed_plugins_require_opt_in_and_preserve_configuration() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ty-plugin-discovery-{}", std::process::id()));
        let package = root.join(".venv/lib/python3.13/site-packages/example");
        std::fs::create_dir_all(&package)?;
        std::fs::write(
            package.join("ty-plugin.json"),
            r#"{"id":"example","runtime":{"kind":"monty","artifact":"plugin.py"},"stub-overlays":[]}"#,
        )?;
        std::fs::write(package.join("plugin.py"), "pass")?;
        let table: Table = toml::from_str(
            "[plugins]\nauto-discover=true\n[plugins.config.example]\nmode='strict'\n",
        )?;
        let (plugins, _) = plugin_registrations(&table, &root, &root, &BTreeMap::new())?;
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].2["mode"], "strict");
        let disabled: Table = toml::from_str("[plugins]\nauto-discover=false\n")?;
        assert!(
            plugin_registrations(&disabled, &root, &root, &BTreeMap::new())?
                .0
                .is_empty()
        );
        let disabled_worker: Table = toml::from_str(
            "[plugins]\nenabled=false\nauto-discover=false\nmonty-mode='worker'\n[[plugins.plugin]]\nid='obsolete'\nruntime='wasm'\npath='missing.wasm'\n",
        )?;
        assert!(
            plugin_registrations(&disabled_worker, &root, &root, &BTreeMap::new())?
                .0
                .is_empty()
        );
        let untrusted: Table = toml::from_str(
            "[plugins]\nenabled=true\n[[plugins.plugin]]\nid='example'\nruntime='monty'\npath='missing.py'\n",
        )?;
        assert!(
            plugin_registrations(&untrusted, &root, &root, &BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("not trusted")
        );
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
        options.insert(
            "cli-config".into(),
            serde_json::to_string(&["environment.python-version='3.13'"])?,
        );
        assert_eq!(load(&root, &view, &stubs, &options)?.python_version, "3.13");
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
            value["environment"]["extra-paths"][2].as_str(),
            Some(view.join("shared").to_str().unwrap())
        );
        assert_eq!(
            value["environment"]["python"].as_str(),
            Some(root.join("./.venv").to_str().unwrap())
        );
        assert_eq!(
            value["overrides"][0]["include"][0].as_str(),
            Some(view.join("tests/**").to_str().unwrap())
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
