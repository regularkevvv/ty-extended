//! Whole-project external-checker orchestration. Only generated views are written.
use crate::{
    mapping::{
        DescriptorAssignment, DiagnosticOrigins, GeneratedDeclarations, Rendered,
        map_diagnostics_with_origins, render,
    },
    model::{Analysis, Oracle, ReturnPatches, Source},
    project::Project,
    runtime::{Runtime, call_request},
    type_render::Types,
};
use anyhow::{Context, Result, ensure};
#[path = "project_config.rs"]
mod project_config;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Instant,
};

fn phase(phases: &mut BTreeMap<&'static str, f64>, name: &'static str, previous: &mut Instant) {
    let now = Instant::now();
    *phases.entry(name).or_default() += now.duration_since(*previous).as_secs_f64();
    *previous = now;
}

fn files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if entry.file_type()?.is_dir() {
            if !name.starts_with('.')
                && !matches!(
                    name.as_ref(),
                    "target" | "dist" | "__pycache__" | "node_modules"
                )
            {
                files(&path, out)?;
            }
        } else if path
            .extension()
            .is_some_and(|x| x == "py" || x == "pyi" || x == "ipynb")
        {
            out.push(path);
        }
    }
    Ok(())
}
fn write(path: &Path, text: &str) -> Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|previous| previous == text) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text)?;
    Ok(())
}
fn copy_python(root: &Path, target: &Path) -> Result<()> {
    let mut paths = Vec::new();
    files(root, &mut paths)?;
    for path in paths {
        let dst = target.join(path.strip_prefix(root)?);
        if !dst.exists() {
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // fs::copy can use the platform's clone/copy fast path; hard links
            // would let a later projection overwrite the original source.
            std::fs::copy(&path, &dst)?;
            // Package caches may contain read-only files; their isolated
            // projections must still be writable for later hook edits.
            let mut permissions = std::fs::metadata(&dst)?.permissions();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if permissions.mode() & 0o200 == 0 {
                    permissions.set_mode(permissions.mode() | 0o200);
                    std::fs::set_permissions(&dst, permissions)?;
                }
            }
            #[cfg(not(unix))]
            // This branch cannot widen Unix permissions; Windows clears only
            // the copied file's read-only attribute. Unix sets owner-write above.
            #[allow(clippy::permissions_set_readonly_false)]
            if permissions.readonly() {
                permissions.set_readonly(false);
                std::fs::set_permissions(&dst, permissions)?;
            }
        } else {
            write(&dst, &std::fs::read_to_string(path)?)?;
        }
    }
    Ok(())
}
fn projected_source_path(root: &Path, view: &Path, original: &Path) -> PathBuf {
    original
        .strip_prefix(root)
        .map_or_else(|_| original.to_owned(), |relative| view.join(relative))
}

fn projected_stub_path(
    stubs: &Path,
    roots: &[PathBuf],
    resolved: &BTreeMap<PathBuf, PathBuf>,
    original: &Path,
) -> Result<PathBuf> {
    if let Some(relative) = resolved.get(original) {
        return Ok(stubs.join(relative));
    }
    roots
        .iter()
        .find_map(|root| original.strip_prefix(root).ok())
        .map(|relative| stubs.join(relative))
        .with_context(|| format!("stub {} is outside configured overlays", original.display()))
}
fn check(binary: &Path, root: &Path, config: &Path, paths: &[PathBuf]) -> Result<Vec<Value>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let output = Command::new(binary)
        .args(["check", "--output-format", "gitlab", "--config-file"])
        .arg(config)
        .args(paths)
        .current_dir(root)
        .env("TY_UV", "0")
        .output()
        .context("run official checker")?;
    ensure!(
        matches!(output.status.code(), Some(0 | 1)),
        "official checker {}: {} {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("checker diagnostic JSON")
}
fn path_of(d: &Value, root: &Path) -> PathBuf {
    let p = PathBuf::from(d["location"]["path"].as_str().unwrap_or(""));
    if p.is_absolute() { p } else { root.join(p) }
}
fn imports(text: &str, source_type: ruff_python_ast::PySourceType) -> Result<BTreeSet<String>> {
    let parsed = crate::project::parse_source(text, source_type)?;
    let mut modules = BTreeSet::new();
    for statement in &parsed.syntax().body {
        match statement {
            ruff_python_ast::Stmt::Import(import) => {
                modules.extend(import.names.iter().map(|alias| alias.name.to_string()));
            }
            ruff_python_ast::Stmt::ImportFrom(import) if import.level == 0 => {
                if let Some(module) = &import.module
                    && module.as_str() != "__future__"
                {
                    modules.insert(module.to_string());
                }
            }
            _ => {}
        }
    }
    Ok(modules)
}
struct File {
    source: Arc<Source>,
    suite: Arc<Vec<ruff_python_ast::Stmt>>,
    analysis: Analysis,
    path: PathBuf,
    oracle: Oracle,
    patches: ReturnPatches,
    return_types: BTreeMap<crate::model::Span, Value>,
    types: Types,
    diagnostics: Vec<Value>,
    rendered: Option<Rendered>,
    selected: bool,
    valid: bool,
    names: BTreeMap<String, String>,
    final_edits: Option<crate::mapping::Edits>,
    dirty: bool,
    raw_diagnostics: Vec<Value>,
    flow_diagnostics: Vec<Value>,
}

#[allow(clippy::too_many_arguments)]
fn check_units(
    binary: &Path,
    root: &Path,
    config: &Path,
    units: &BTreeMap<PathBuf, File>,
    paths: &[PathBuf],
    persistent: bool,
    server: &mut Option<crate::checker::ProjectChecker>,
    capabilities: Option<&Value>,
) -> Result<Vec<Value>> {
    if !persistent {
        return check(binary, root, config, paths);
    }
    if server.is_none() {
        // Both generated source and overlaid stubs participate in inference.
        // ty publishes no diagnostics for files outside its workspace, even
        // when they are open and importable through extra-paths.
        let workspace = config.parent().context("generated checker workspace")?;
        *server = Some(if let Some(capabilities) = capabilities {
            crate::checker::ProjectChecker::new_editor(
                binary,
                workspace,
                config,
                capabilities.clone(),
            )?
        } else {
            crate::checker::ProjectChecker::new(binary, workspace, config)?
        });
    }
    let sources = units
        .iter()
        .map(|(path, unit)| (path.clone(), unit.rendered.as_ref().unwrap().text.clone()))
        .collect();
    server.as_mut().unwrap().check(&sources, paths)
}

fn diagnostic_origins(
    unit: &File,
    project: &Project,
    generated: &GeneratedDeclarations,
) -> DiagnosticOrigins {
    use crate::model::{expression_key, span};
    use ruff_python_ast::{
        Expr, Stmt,
        visitor::{self, Visitor},
    };
    let mut origins = DiagnosticOrigins {
        virtual_names: project.virtual_identities.clone(),
        ..DiagnosticOrigins::default()
    };
    for call in unit.analysis.calls.values() {
        let class = project.canonical_name(&unit.source.module, &call.callee);
        if generated.constructors.contains(&class) {
            origins.constructor_calls.insert(call.span, class);
        }
    }
    struct Assignments<'a> {
        unit: &'a File,
        project: &'a Project,
        generated: &'a GeneratedDeclarations,
        sites: Vec<DescriptorAssignment>,
    }
    impl Assignments<'_> {
        fn owner(&self, expression: &Expr) -> Option<String> {
            if let Expr::Call(call) = expression {
                // The call collector has already resolved imports and rejected
                // shadowed constructor names. Its full callee identity proves
                // this constructor creates the projected class.
                let call = self.unit.analysis.calls.get(&span(call))?;
                let owner = self
                    .project
                    .canonical_name(&self.unit.source.module, &call.callee);
                if self.project.classes.contains_key(&owner) {
                    return Some(owner);
                }
            }
            // Never guess a class from an unqualified printed name: another
            // module or a local class can use that same name. Full qualified
            // oracle identities can be checked against the project index.
            let value = self.unit.oracle.get(&expression_key(span(expression)))?;
            if let Some(owner) = self.project.virtual_identities.get(value) {
                return Some(owner.clone());
            }
            self.project
                .classes
                .contains_key(value)
                .then(|| value.clone())
        }
        fn assignment(&mut self, target: &Expr, value: &Expr) {
            let Expr::Attribute(attribute) = target else {
                return;
            };
            let Some(owner) = self.owner(&attribute.value) else {
                return;
            };
            if self
                .generated
                .descriptors
                .get(&owner)
                .is_some_and(|members| members.contains(attribute.attr.as_str()))
            {
                self.sites.push(DescriptorAssignment {
                    value: span(value),
                    target: span(target),
                    owner,
                    member: attribute.attr.to_string(),
                });
            }
        }
    }
    impl<'a> Visitor<'a> for Assignments<'_> {
        fn visit_stmt(&mut self, statement: &'a Stmt) {
            match statement {
                Stmt::Assign(assignment) => {
                    for target in &assignment.targets {
                        self.assignment(target, &assignment.value);
                    }
                }
                Stmt::AnnAssign(assignment) => {
                    if let Some(value) = &assignment.value {
                        self.assignment(&assignment.target, value);
                    }
                }
                _ => {}
            }
            visitor::walk_stmt(self, statement);
        }
    }
    let mut assignments = Assignments {
        unit,
        project,
        generated,
        sites: Vec::new(),
    };
    assignments.visit_body(&unit.suite);
    origins.descriptor_assignments = assignments.sites;
    origins
}
fn selected_paths(
    project: &Project,
    root: &Path,
    options: &BTreeMap<String, String>,
    selection: &project_config::Selection,
    ignored: &BTreeSet<PathBuf>,
    overlays: &BTreeMap<PathBuf, String>,
) -> Result<BTreeSet<PathBuf>> {
    let selected: Vec<PathBuf> = if let Some(path) = options.get("files") {
        let names: Vec<String> = serde_json::from_slice(&std::fs::read(path)?)?;
        let mut paths = Vec::new();
        for name in names {
            let p = root.join(name);
            if p.is_dir() {
                files(&p, &mut paths)?;
            } else {
                paths.push(p);
            }
        }
        paths
    } else {
        project
            .modules
            .values()
            .filter(|m| !m.external)
            .filter(|m| {
                overlays.contains_key(Path::new(&m.source.path))
                    || (selection.includes(Path::new(&m.source.path), &m.source.text)
                        && !ignored.contains(Path::new(&m.source.path)))
            })
            .map(|m| PathBuf::from(&m.source.path))
            .collect()
    };
    let selected: BTreeSet<_> = selected
        .into_iter()
        .map(|p| if p.exists() { p.canonicalize() } else { Ok(p) })
        .collect::<std::io::Result<_>>()?;
    Ok(selected)
}

/// A reusable analysis session. Original files are never written, including unsaved buffers.
pub struct Session {
    root: PathBuf,
    out: PathBuf,
    view: PathBuf,
    stubs: PathBuf,
    stub_roots: Vec<PathBuf>,
    options: BTreeMap<String, String>,
    mode: String,
    runtime: Runtime,
    base_project: Project,
    server: Option<crate::checker::ProjectChecker>,
    rich_server: Option<crate::checker::ProjectChecker>,
    rich_result: Option<(usize, Vec<Value>)>,
    documents: Vec<ProjectDocument>,
    virtual_files: BTreeSet<PathBuf>,
    checker_config_path: PathBuf,
    config: Value,
    capabilities: Option<Value>,
    last_result: Option<Value>,
    revisions: usize,
    config_source: Option<PathBuf>,
    invalidated: bool,
    names: BTreeMap<String, String>,
    selection: project_config::Selection,
    ignored: BTreeSet<PathBuf>,
    has_plugins: bool,
    resolved_paths: BTreeMap<PathBuf, PathBuf>,
    resolved_imports: BTreeSet<String>,
    external_resolution_requests: usize,
    projection_edits: BTreeMap<String, crate::mapping::Edits>,
    active_documents: BTreeSet<PathBuf>,
    notebook_sources: BTreeSet<PathBuf>,
    explicit_files: BTreeSet<PathBuf>,
    explicit_paths: Vec<PathBuf>,
    force_exclude: bool,
    terminal: Value,
    cached_units: BTreeMap<PathBuf, File>,
    cached_sources: BTreeMap<String, Arc<Source>>,
    cached_context: Option<Value>,
    plugin_files: Vec<PathBuf>,
    dependency_roots: Vec<PathBuf>,
}

#[derive(Clone)]
pub struct ProjectDocument {
    pub source: Arc<Source>,
    pub path: PathBuf,
    pub rendered: Rendered,
    pub names: BTreeMap<String, String>,
}

impl Session {
    pub fn new(input: &Path, options: &BTreeMap<String, String>) -> Result<Self> {
        let required = |key: &str| options.get(key).with_context(|| format!("missing --{key}"));
        let mode = options
            .get("checker-mode")
            .map(String::as_str)
            .unwrap_or("cli");
        ensure!(
            matches!(mode, "cli" | "push"),
            "project --checker-mode must be cli or push"
        );
        let root = std::fs::canonicalize(input)?;
        let out = PathBuf::from(required("out")?);
        std::fs::create_dir_all(&out)?;
        let out = out.canonicalize()?;
        ensure!(
            !out.starts_with(&root),
            "generated output must be outside original project"
        );
        let view = out.join("project");
        let stubs = out.join("stubs");
        ensure!(
            !view.exists() && !stubs.exists(),
            "generated project or stubs already exist; use a fresh --out directory"
        );
        std::fs::create_dir_all(&view)?;
        std::fs::create_dir_all(&stubs)?;
        copy_python(&root, &view)?;
        let config: Value = if let Some(path) = options.get("config") {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            json!({})
        };
        let checker_config = project_config::load(&root, &view, &stubs, options)?;
        let checker_config_path = out.join("checker.toml");
        write(&checker_config_path, &checker_config.text)?;
        let mut runtime = Runtime::from_plugins(&checker_config.plugins)?;
        for root in checker_config.stub_roots.iter().rev() {
            copy_python(root, &stubs)?;
        }
        runtime.python_version = checker_config.python_version;
        runtime.platform = checker_config.platform;
        let capabilities: Option<Value> = options
            .get("lsp-capabilities")
            .map(|s| serde_json::from_str(s))
            .transpose()?;
        let mut ignored = checker_config.selection.ignored_paths();
        let force_exclude = options.get("force-exclude").is_some_and(|v| v == "true");
        let mut explicit_files = BTreeSet::new();
        let explicit_paths = options
            .get("explicit-files")
            .map(|s| serde_json::from_str::<Vec<PathBuf>>(s))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|path| path.canonicalize())
            .collect::<std::io::Result<Vec<_>>>()?;
        for path in &explicit_paths {
            if path.is_dir() {
                ignored.extend(checker_config.selection.ignored_paths_under(path));
            }
            if !path.is_dir() {
                explicit_files.insert(path.clone());
            }
        }
        let mut source_paths = Vec::new();
        files(&root, &mut source_paths)?;
        for path in &explicit_paths {
            if path.is_dir() {
                files(path, &mut source_paths)?;
            } else {
                source_paths.push(path.clone());
            }
        }
        source_paths.sort();
        source_paths.dedup();
        let source_paths = source_paths
            .into_iter()
            .filter_map(|path| match std::fs::read_to_string(&path) {
                Ok(text)
                    if (explicit_files.contains(&path)
                        && checker_config.selection.includes_explicit(
                            &path,
                            &text,
                            force_exclude,
                        )
                        && (!force_exclude || !ignored.contains(&path)))
                        || (checker_config.selection.includes(&path, &text)
                            && !ignored.contains(&path)) =>
                {
                    Some(Ok(path))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        let mut base_project = Project::load_selected(
            &root,
            &source_paths,
            &runtime.manifest,
            config.clone(),
            capabilities.is_some()
                || options
                    .get("public-cli")
                    .is_some_and(|value| value == "true"),
        )?;
        base_project.set_source_roots(checker_config.source_roots)?;
        base_project.add_source_paths(explicit_paths.clone());
        for root in checker_config.stub_roots.iter().rev() {
            base_project.add_source_tree(root)?;
        }
        let server = if let Some(capabilities) = &capabilities {
            Some(crate::checker::ProjectChecker::new_editor_with_options(
                Path::new(required("ty")?),
                &out,
                &checker_config_path,
                capabilities.clone(),
                options
                    .get("lsp-initialization-options")
                    .map(|s| serde_json::from_str(s))
                    .transpose()?
                    .unwrap_or(json!({})),
            )?)
        } else {
            None
        };
        let plugin_files = checker_config
            .plugins
            .iter()
            .flat_map(|(manifest, artifact, _)| [manifest.clone(), artifact.clone()])
            .collect();
        Ok(Self {
            root,
            out,
            view,
            stubs,
            stub_roots: checker_config.stub_roots,
            options: options.clone(),
            mode: mode.into(),
            runtime,
            base_project,
            server,
            rich_server: None,
            rich_result: None,
            documents: Vec::new(),
            virtual_files: BTreeSet::new(),
            checker_config_path,
            config,
            capabilities,
            last_result: None,
            revisions: 0,
            config_source: checker_config.source,
            invalidated: false,
            names: BTreeMap::new(),
            ignored,
            selection: checker_config.selection,
            has_plugins: !checker_config.plugins.is_empty(),
            resolved_paths: BTreeMap::new(),
            resolved_imports: BTreeSet::new(),
            external_resolution_requests: 0,
            projection_edits: BTreeMap::new(),
            active_documents: BTreeSet::new(),
            notebook_sources: source_paths
                .iter()
                .filter(|p| p.extension().is_some_and(|e| e == "ipynb"))
                .cloned()
                .collect(),
            explicit_files,
            explicit_paths,
            force_exclude,
            terminal: checker_config.terminal,
            cached_units: BTreeMap::new(),
            cached_sources: BTreeMap::new(),
            cached_context: None,
            plugin_files,
            dependency_roots: checker_config.dependency_roots,
        })
    }

    pub fn register_notebook_source(&mut self, path: &Path) -> Result<()> {
        if self.notebook_sources.insert(path.into()) {
            self.base_project.register_notebook_source(path)?;
            self.last_result = None;
            self.cached_context = None;
        }
        self.register_checker_notebooks()
    }

    fn register_checker_notebooks(&mut self) -> Result<()> {
        if let Some(server) = self.server.as_mut() {
            for path in &self.notebook_sources {
                let generated = projected_source_path(&self.root, &self.view, path);
                server.register_notebook_source(&generated)?;
                if !generated.starts_with(&self.out) {
                    server.register_external_source(&generated)?;
                }
            }
        }
        Ok(())
    }

    pub fn watch_paths(&self) -> Vec<PathBuf> {
        let mut paths = BTreeSet::from([self.root.clone()]);
        paths.extend(self.config_source.iter().cloned());
        paths.extend(self.explicit_paths.iter().cloned());
        paths.extend(self.options.get("config").map(PathBuf::from));
        paths.extend(self.plugin_files.iter().cloned());
        paths.extend(self.stub_roots.iter().cloned());
        paths.extend(self.dependency_roots.iter().cloned());
        paths.extend(self.resolved_paths.keys().cloned());
        paths.into_iter().collect()
    }

    pub fn path_requires_reload(&self, path: &Path) -> bool {
        self.config_source.as_deref() == Some(path)
            || self
                .options
                .get("config")
                .is_some_and(|config| Path::new(config) == path)
            || self.plugin_files.iter().any(|file| file == path)
            || self.stub_roots.iter().any(|root| path.starts_with(root))
            || self
                .dependency_roots
                .iter()
                .any(|root| path.starts_with(root))
            || self.resolved_paths.contains_key(path)
            || ((path.starts_with(&self.root)
                || self
                    .explicit_paths
                    .iter()
                    .any(|root| path.starts_with(root)))
                && path.file_name().is_some_and(|name| {
                    name == "ty.toml"
                        || name == "pyproject.toml"
                        || name == ".gitignore"
                        || name == ".ignore"
                }))
    }

    /// Rich presentation is fetched separately: upstream changes its primary
    /// diagnostic message when related-information support is enabled.
    pub fn rich_diagnostics(&mut self) -> Result<Vec<Value>> {
        if let Some((revision, diagnostics)) = &self.rich_result
            && *revision == self.revisions
        {
            return Ok(diagnostics.clone());
        }
        let originals = self
            .last_result
            .as_ref()
            .and_then(|result| result["diagnostics"].as_array())
            .into_iter()
            .flatten()
            .filter(|diagnostic| diagnostic["lsp_diagnostic"].is_object())
            .filter_map(|diagnostic| diagnostic.pointer("/location/path").and_then(Value::as_str))
            .collect::<BTreeSet<_>>();
        let paths = self
            .documents
            .iter()
            .filter(|document| originals.contains(document.source.path.as_str()))
            .map(|document| document.path.clone())
            .collect::<Vec<_>>();
        if paths.is_empty() {
            return Ok(Vec::new());
        }
        if self.rich_server.is_none() {
            self.rich_server = Some(crate::checker::ProjectChecker::new_editor(
                Path::new(&self.options["ty"]),
                &self.out,
                &self.checker_config_path,
                json!({"general":{},"workspace":{},"textDocument":{"publishDiagnostics":{"relatedInformation":true}}}),
            )?);
        }
        let server = self.rich_server.as_mut().unwrap();
        let mut sources = BTreeMap::new();
        for document in self
            .documents
            .iter()
            .filter(|document| self.active_documents.contains(&document.path))
        {
            if Path::new(&document.source.path)
                .extension()
                .is_some_and(|e| e == "ipynb")
            {
                server.register_notebook_source(&document.path)?;
            }
            if !document.path.starts_with(&self.out) {
                server.register_external_source(&document.path)?;
            }
            sources.insert(document.path.clone(), document.rendered.text.clone());
        }
        let mut raw = server.check(&sources, &paths)?;
        for document in self.documents.iter().filter(|document| {
            paths.contains(&document.path)
                && Path::new(&document.source.path)
                    .extension()
                    .is_some_and(|e| e == "ipynb")
        }) {
            let native = crate::notebook::check_disk(document, server)?;
            raw.retain(|diagnostic| path_of(diagnostic, &self.view) != document.path);
            raw.extend(native);
        }
        self.rich_result = Some((self.revisions, raw.clone()));
        Ok(raw)
    }

    pub fn terminal_options(&self) -> &Value {
        &self.terminal
    }

    pub fn documents(&self) -> Vec<ProjectDocument> {
        self.documents.clone()
    }
    pub fn checker_mut(&mut self) -> Option<&mut crate::checker::ProjectChecker> {
        self.server.as_mut()
    }

    pub fn names(&self) -> BTreeMap<String, String> {
        self.names.clone()
    }
    pub fn invalidate(&mut self) {
        self.rich_server = None;
        self.rich_result = None;
        self.invalidated = true;
        self.last_result = None;
        self.cached_context = None;
        self.cached_units.clear();
        self.cached_sources.clear();
        self.base_project.unavailable_modules.clear();
    }

    fn reload_dependencies(&mut self) -> Result<()> {
        let config = project_config::load(&self.root, &self.view, &self.stubs, &self.options)?;
        let configuration_changed =
            std::fs::read_to_string(&self.checker_config_path)? != config.text;
        if configuration_changed {
            write(&self.checker_config_path, &config.text)?;
            self.server = if let Some(capabilities) = &self.capabilities {
                Some(crate::checker::ProjectChecker::new_editor_with_options(
                    Path::new(&self.options["ty"]),
                    &self.out,
                    &self.checker_config_path,
                    capabilities.clone(),
                    self.options
                        .get("lsp-initialization-options")
                        .map(|s| serde_json::from_str(s))
                        .transpose()?
                        .unwrap_or(json!({})),
                )?)
            } else {
                None
            };
        }
        self.terminal = config.terminal;
        self.config_source = config.source;
        self.config = if let Some(path) = self.options.get("config") {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            json!({})
        };
        self.runtime = Runtime::from_plugins(&config.plugins)?;
        self.plugin_files = config
            .plugins
            .iter()
            .flat_map(|(manifest, artifact, _)| [manifest.clone(), artifact.clone()])
            .collect();
        self.has_plugins = !config.plugins.is_empty();
        if !self.has_plugins {
            for path in std::mem::take(&mut self.virtual_files) {
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
            }
        }
        self.runtime.python_version = config.python_version;
        self.runtime.platform = config.platform;
        self.base_project.set_source_roots(config.source_roots)?;
        self.ignored = config.selection.ignored_paths();
        for path in self.explicit_paths.iter().filter(|path| path.is_dir()) {
            self.ignored
                .extend(config.selection.ignored_paths_under(path));
        }
        self.selection = config.selection;
        self.base_project.config = self.config.clone();
        self.base_project.manifest = self.runtime.manifest.clone();
        let previous: BTreeSet<_> = self
            .base_project
            .modules
            .values()
            .filter(|m| m.external)
            .map(|m| PathBuf::from(&m.source.path))
            .collect();
        self.base_project
            .retain_external_roots(&config.stub_roots)?;
        for root in config.stub_roots.iter().rev() {
            self.base_project.add_source_tree(root)?;
        }
        let current: BTreeSet<_> = self
            .base_project
            .modules
            .values()
            .filter(|m| m.external)
            .map(|m| PathBuf::from(&m.source.path))
            .collect();
        for removed in previous.difference(&current) {
            let generated =
                projected_stub_path(&self.stubs, &self.stub_roots, &self.resolved_paths, removed)?;
            if generated.exists() {
                std::fs::remove_file(generated)?;
            }
        }
        self.stub_roots = config.stub_roots;
        self.dependency_roots = config.dependency_roots;
        for root in self.stub_roots.iter().rev() {
            copy_python(root, &self.stubs)?;
        }
        self.resolved_imports.clear();
        self.resolved_paths.clear();
        self.invalidated = false;
        Ok(())
    }

    fn resolve_external_imports(&mut self, additional: &BTreeSet<String>) -> Result<()> {
        loop {
            let mut pending = self.base_project.ancestry_imports();
            pending.extend(
                additional
                    .iter()
                    .filter(|name| !self.base_project.modules.contains_key(*name))
                    .cloned(),
            );
            let candidates = pending
                .difference(&self.resolved_imports)
                .cloned()
                .collect::<Vec<_>>();
            if candidates.is_empty() {
                return Ok(());
            }
            let probe = self.out.join("resolution-probe.py");
            let text = candidates
                .iter()
                .map(|module| format!("import {module}\n"))
                .collect::<String>();
            write(&probe, &text)?;
            let mut sources = BTreeMap::new();
            for module in self
                .base_project
                .modules
                .values()
                .filter(|module| !module.external)
            {
                let path =
                    projected_source_path(&self.root, &self.view, Path::new(&module.source.path));
                if path.starts_with(&self.out) {
                    write(&path, &module.source.text)?;
                }
                sources.insert(path, module.source.text.clone());
            }
            sources.insert(probe.clone(), text);
            if self.server.is_none() {
                self.server = Some(crate::checker::ProjectChecker::new(
                    Path::new(self.options.get("ty").context("missing --ty")?),
                    &self.out,
                    &self.checker_config_path,
                )?);
            }
            self.register_checker_notebooks()?;
            let server = self.server.as_mut().unwrap();
            for path in sources.keys().filter(|path| !path.starts_with(&self.out)) {
                server.register_external_source(path)?;
            }
            server.sync_documents(&sources)?;
            let uri = crate::checker::file_uri(&probe)?;
            for (line, module) in candidates.iter().enumerate() {
                self.resolved_imports.insert(module.clone());
                self.external_resolution_requests += 1;
                let result=server.request("textDocument/declaration",json!({"textDocument":{"uri":uri},"position":{"line":line,"character":7+module.chars().count().saturating_sub(1)}}))?;
                let location = if let Some(locations) = result.as_array() {
                    locations.first()
                } else {
                    Some(&result)
                };
                let Some(uri) = location.and_then(|location| {
                    location["targetUri"]
                        .as_str()
                        .or_else(|| location["uri"].as_str())
                }) else {
                    continue;
                };
                let Some(mut path) = crate::lsp_mapping::decode_file_uri(uri)? else {
                    continue;
                };
                if path.starts_with(&self.view) {
                    continue;
                }
                // A configured overlay resolves through its generated copy; map
                // it back before indexing so the plugin sees original locations.
                if let Ok(relative) = path.strip_prefix(&self.stubs) {
                    let relative = relative.to_owned();
                    if let Some(original) =
                        self.resolved_paths
                            .iter()
                            .find_map(|(original, generated)| {
                                (generated == &relative).then(|| original.clone())
                            })
                    {
                        path = original;
                    } else if let Some(original) = self
                        .stub_roots
                        .iter()
                        .map(|root| root.join(&relative))
                        .find(|path| path.is_file())
                    {
                        path = original;
                    } else {
                        continue;
                    }
                }
                if !path.is_file() || !path.extension().is_some_and(|e| e == "py" || e == "pyi") {
                    continue;
                }
                // Navigation on an imported symbol can land on a class rather
                // than a module. Require a module's start position for probes.
                let position = location
                    .unwrap()
                    .get("targetSelectionRange")
                    .or_else(|| location.unwrap().get("range"))
                    .and_then(|range| range.get("start"));
                if position.is_some_and(|p| p["line"] != 0 || p["character"] != 0) {
                    continue;
                }
                let suffix = if path.file_stem().is_some_and(|name| name == "__init__") {
                    format!(
                        "{}/__init__.{}",
                        module.replace('.', "/"),
                        path.extension().unwrap().to_string_lossy()
                    )
                } else {
                    format!(
                        "{}.{}",
                        module.replace('.', "/"),
                        path.extension().unwrap().to_string_lossy()
                    )
                };
                let relative = PathBuf::from(suffix);
                self.base_project.add_resolved_module(module, &path)?;
                self.resolved_paths.insert(path.clone(), relative.clone());
            }
        }
    }

    fn reuse_trailing_trivia(&mut self, started: Instant) -> Result<Option<Value>> {
        if self.last_result.is_none() || !self.base_project.syntax_errors.is_empty() {
            return Ok(None);
        }
        let locals = self
            .base_project
            .modules
            .values()
            .filter(|m| !m.external)
            .collect::<Vec<_>>();
        let old_local_count = self
            .documents
            .iter()
            .filter(|d| d.path.starts_with(&self.view))
            .count();
        if locals.len() != old_local_count {
            return Ok(None);
        }
        let mut replacements = Vec::new();
        for module in locals {
            let Some((index, old)) = self
                .documents
                .iter()
                .enumerate()
                .find(|(_, d)| d.source.path == module.source.path)
            else {
                return Ok(None);
            };
            if old.source.text == module.source.text {
                continue;
            }
            if old.source.prefix != module.source.prefix {
                return Ok(None);
            }
            let Ok(old_parsed) = crate::project::parse_source(
                &old.source.text,
                self.base_project.source_type(Path::new(&old.source.path)),
            ) else {
                return Ok(None);
            };
            if old_parsed.syntax().body.as_slice() != module.suite.as_slice() {
                return Ok(None);
            }
            let offset = module
                .suite
                .last()
                .map_or(0, |statement| crate::model::span(statement).end);
            if old.source.text[..offset] != module.source.text[..offset] {
                return Ok(None);
            }
            let harmless = |tail: &str| {
                tail.lines().all(|line| {
                    line.trim().is_empty()
                        || (line.trim_start().starts_with('#')
                            && !line.contains("ty:")
                            && !line.contains("type:")
                            && !line.contains("///"))
                })
            };
            if !harmless(&old.source.text[offset..]) || !harmless(&module.source.text[offset..]) {
                return Ok(None);
            }
            let Some(edits) = self.projection_edits.get(&module.source.path) else {
                return Ok(None);
            };
            let Ok(rendered) = render(&module.source, edits) else {
                return Ok(None);
            };
            replacements.push((index, module.source.clone(), rendered));
        }
        if replacements.is_empty() {
            return Ok(None);
        }
        for (index, source, rendered) in replacements {
            let document = &mut self.documents[index];
            if document.path.starts_with(&self.out) {
                write(&document.path, &rendered.text)?;
            }
            document.source = source;
            document.rendered = rendered;
            if let Some(unit) = self.cached_units.get_mut(&document.path) {
                unit.source = document.source.clone();
                unit.rendered = Some(document.rendered.clone());
            }
            self.cached_sources
                .insert(document.source.module.clone(), document.source.clone());
        }
        if let Some(server) = &mut self.server {
            let sources = self
                .documents
                .iter()
                .filter(|document| self.active_documents.contains(&document.path))
                .map(|document| (document.path.clone(), document.rendered.text.clone()))
                .collect();
            server.sync_documents(&sources)?;
        }
        let mut result = self.last_result.clone().unwrap();
        result["revision"] = json!(self.revisions);
        result["projection_rounds"] = json!(0);
        result["oracle_rounds"] = json!(0);
        result["reused_projection"] = json!(true);
        result["reuse_reason"] = json!("trailing-trivia");
        result["projected_units"] = json!(0);
        result["reused_units"] = json!(self.cached_units.len());
        result["monty_executions_delta"] = json!(0);
        result["monty_cache_hits_delta"] = json!(0);
        result["monty_seconds_delta"] = json!(0.0);
        result["seconds"] = json!(started.elapsed().as_secs_f64());
        result["phases"] = json!({"source_scan_and_trivia_update":started.elapsed().as_secs_f64()});
        self.last_result = Some(result.clone());
        write(
            &self.out.join("results.json"),
            &serde_json::to_string_pretty(&result)?,
        )?;
        Ok(Some(result))
    }

    fn check_without_plugins(
        &mut self,
        overlays: &BTreeMap<PathBuf, String>,
        started: Instant,
    ) -> Result<Value> {
        let selected = selected_paths(
            &self.base_project,
            &self.root,
            &self.options,
            &self.selection,
            &self.ignored,
            overlays,
        )?;
        let mut documents = Vec::new();
        let mut sources = BTreeMap::new();
        let mut paths = Vec::new();
        for module in self.base_project.modules.values() {
            let path = if module.external {
                projected_stub_path(
                    &self.stubs,
                    &self.stub_roots,
                    &self.resolved_paths,
                    Path::new(&module.source.path),
                )?
            } else {
                projected_source_path(&self.root, &self.view, Path::new(&module.source.path))
            };
            let rendered = render(&module.source, &crate::mapping::Edits::default())?;
            if path.starts_with(&self.out) {
                write(&path, &rendered.text)?;
            }
            sources.insert(path.clone(), rendered.text.clone());
            if selected.contains(Path::new(&module.source.path)) {
                paths.push(path.clone());
            }
            documents.push(ProjectDocument {
                source: module.source.clone(),
                path,
                rendered,
                names: BTreeMap::new(),
            });
        }
        for old in &self.documents {
            if old.path.starts_with(&self.out)
                && !sources.contains_key(&old.path)
                && old.path.exists()
            {
                std::fs::remove_file(&old.path)?;
            }
        }
        let binary = PathBuf::from(self.options.get("ty").context("missing --ty")?);
        let persistent = self.mode == "push"
            || !self.notebook_sources.is_empty()
            || sources.keys().any(|p| !p.starts_with(&self.out));
        let mut raw = if persistent {
            if self.server.is_none() {
                self.server = Some(crate::checker::ProjectChecker::new(
                    &binary,
                    &self.out,
                    &self.checker_config_path,
                )?);
            }
            self.register_checker_notebooks()?;
            let server = self.server.as_mut().unwrap();
            for path in sources.keys().filter(|p| !p.starts_with(&self.out)) {
                server.register_external_source(path)?;
            }
            server.check(&sources, &paths)?
        } else {
            check(&binary, &self.view, &self.checker_config_path, &paths)?
        };
        if self.capabilities.is_none() {
            for document in documents.iter().filter(|d| {
                paths.contains(&d.path)
                    && Path::new(&d.source.path)
                        .extension()
                        .is_some_and(|e| e == "ipynb")
            }) {
                let native = crate::notebook::check_disk(
                    document,
                    self.server.as_mut().context("notebook checker")?,
                )?;
                raw.retain(|diagnostic| path_of(diagnostic, &self.view) != document.path);
                raw.extend(native);
            }
        }
        let mut diagnostics = Vec::new();
        for document in &documents {
            let local = raw
                .iter()
                .filter(|d| path_of(d, &self.view) == document.path)
                .cloned()
                .collect::<Vec<_>>();
            let (mapped, _) = map_diagnostics_with_origins(
                &document.source,
                &document.rendered,
                &local,
                &BTreeMap::new(),
                &[],
                &DiagnosticOrigins::default(),
            )?;
            diagnostics.extend(mapped);
        }
        diagnostics.sort_by_key(Value::to_string);
        diagnostics.dedup();
        self.names.clear();
        self.active_documents = sources.keys().cloned().collect();
        self.documents = documents;
        let result = json!({"diagnostics":diagnostics,"suppressed_generated":[],"implementation":"rust","plugin_runtime":"embedded-monty","project":self.root,"checker":binary,"checker_mode":self.mode,"checker_mode_effective":if persistent { "push" } else { "cli" },"configuration":self.config_source,"revision":self.revisions,"projection_rounds":0,"oracle_rounds":0,"monty_executions":0,"monty_cache_hits":0,"monty_seconds":0.0,"seconds":started.elapsed().as_secs_f64()});
        write(
            &self.out.join("raw-diagnostics.json"),
            &serde_json::to_string_pretty(&raw)?,
        )?;
        write(
            &self.out.join("results.json"),
            &serde_json::to_string_pretty(&result)?,
        )?;
        self.last_result = Some(result.clone());
        Ok(result)
    }

    pub fn check(&mut self, overlays: &BTreeMap<PathBuf, String>) -> Result<Value> {
        let started = Instant::now();
        let mut phases = BTreeMap::new();
        let mut phase_started = started;
        if self.invalidated {
            self.reload_dependencies()?;
        }
        self.register_checker_notebooks()?;
        let executions_before = self.runtime.executions;
        let hits_before = self.runtime.hits;
        let monty_before = self.runtime.execution_time;
        let changed = self
            .base_project
            .refresh_sources_filtered(overlays, |path, text| {
                (self.explicit_files.contains(path)
                    && self
                        .selection
                        .includes_explicit(path, text, self.force_exclude)
                    && (!self.force_exclude || !self.ignored.contains(path)))
                    || (self.selection.includes(path, text) && !self.ignored.contains(path))
            })?;
        self.notebook_sources.extend(
            self.base_project
                .modules
                .values()
                .filter(|m| {
                    Path::new(&m.source.path)
                        .extension()
                        .is_some_and(|e| e == "ipynb")
                })
                .map(|m| PathBuf::from(&m.source.path)),
        );
        self.register_checker_notebooks()?;
        phase(&mut phases, "source_scan", &mut phase_started);
        if !changed && let Some(result) = &self.last_result {
            let mut result = result.clone();
            result["seconds"] = json!(started.elapsed().as_secs_f64());
            result["reused_projection"] = json!(true);
            result["reuse_reason"] = json!("unchanged");
            result["projected_units"] = json!(0);
            result["reused_units"] = json!(self.cached_units.len());
            result["phases"] = json!(phases);
            result["monty_executions_delta"] = json!(0);
            result["monty_cache_hits_delta"] = json!(0);
            result["monty_seconds_delta"] = json!(0.0);
            result["oracle_rounds"] = json!(0);
            result["projection_rounds"] = json!(0);
            return Ok(result);
        }
        self.revisions += 1;
        if !self
            .cached_sources
            .keys()
            .eq(self.base_project.modules.keys())
        {
            self.base_project.unavailable_modules.clear();
        }
        if !self.has_plugins {
            self.last_result = None;
            return self.check_without_plugins(overlays, started);
        }
        if let Some(result) = self.reuse_trailing_trivia(started)? {
            return Ok(result);
        }
        // A failed revision must be retried, never reported as an unchanged
        // success using diagnostics from the previous source snapshot.
        self.last_result = None;
        self.resolve_external_imports(&BTreeSet::new())?;
        phase(&mut phases, "external_resolution", &mut phase_started);
        let root = self.root.clone();
        let out = self.out.clone();
        let view = self.view.clone();
        let stubs = self.stubs.clone();
        let stub_roots = self.stub_roots.clone();
        let config = self.config.clone();
        let mut project = self.base_project.clone();
        let index = {
            let runtime = &mut self.runtime;
            runtime.project_patches.clear();
            runtime.project_contributions.clear();
            runtime.project_index = None;
            runtime.attach_project(Arc::new(project.clone()), config.clone());
            let mut request = project.build_request();
            request["context"]["python-version"] = json!(runtime.python_version);
            request["context"]["platform"] = json!(runtime.platform);
            write(
                &out.join("project-request.json"),
                &serde_json::to_string_pretty(&request)?,
            )?;
            runtime.execute_project("build-project-index", request)?
        };
        phase(&mut phases, "project_index", &mut phase_started);
        let targets = index["contributions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| c.pointer("/target/qualified-name").and_then(Value::as_str))
            .filter_map(|name| name.rsplit_once('.').map(|(module, _)| module.to_owned()))
            .filter(|module| !project.modules.contains_key(module))
            .collect::<BTreeSet<_>>();
        if !targets.is_empty() {
            // A qualified target may contain nested class names. Ask ty for
            // progressively shorter prefixes; declaration positions reject
            // class symbols and retain only actual module resolutions.
            let targets = targets
                .iter()
                .flat_map(|target| {
                    let mut prefixes = Vec::new();
                    let mut prefix = target.as_str();
                    loop {
                        prefixes.push(prefix.to_owned());
                        let Some((parent, _)) = prefix.rsplit_once('.') else {
                            break;
                        };
                        prefix = parent;
                    }
                    prefixes
                })
                .collect();
            self.resolve_external_imports(&targets)?;
            project = self.base_project.clone();
            phase(&mut phases, "external_resolution", &mut phase_started);
        }
        let options = &self.options;
        let required = |key: &str| options.get(key).with_context(|| format!("missing --{key}"));
        let mut mode = self.mode.as_str();
        let capabilities = self.capabilities.as_ref();
        let runtime = &mut self.runtime;
        let server = &mut self.server;
        let checker_config_path = self.checker_config_path.clone();
        let mut project_diagnostics = project.settings_diagnostics();
        project_diagnostics.extend(
            index["diagnostics"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
        write(
            &out.join("project-response.json"),
            &serde_json::to_string_pretty(&index)?,
        )?;
        runtime.project_index = index.get("plugin-index").cloned();
        runtime.project_contributions = index["contributions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // The checker retains the first contribution for each target/member pair.
        // Distinct source locations on identical settings contributions are normal.
        let mut contributed = BTreeSet::new();
        runtime.project_contributions.retain(|c| {
            contributed.insert((
                c["target"].to_string(),
                c["patch"]["name"].as_str().unwrap_or("__init__").to_owned(),
            ))
        });
        let definitions = index["virtual-types"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let virtual_sources = project.register_virtual_types(&definitions)?;
        let next_virtual_files: BTreeSet<_> =
            virtual_sources.keys().map(|p| stubs.join(p)).collect();
        let mut virtual_file_changes = Vec::new();
        for path in self.virtual_files.difference(&next_virtual_files) {
            virtual_file_changes.push(json!({"uri":crate::checker::file_uri(path)?,"type":3}));
        }
        for removed in self.virtual_files.difference(&next_virtual_files) {
            if removed.exists() {
                std::fs::remove_file(removed)?;
            }
        }
        self.virtual_files = next_virtual_files;
        for (path, code) in virtual_sources {
            let path = stubs.join(path);
            if std::fs::read_to_string(&path).is_ok_and(|previous| previous == code) {
                continue;
            }
            let change = if path.exists() { 2 } else { 1 };
            let mut parent = path.parent();
            while let Some(directory) = parent.filter(|directory| !directory.exists()) {
                virtual_file_changes
                    .push(json!({"uri":crate::checker::file_uri(directory)?,"type":1}));
                parent = directory.parent();
            }
            write(&path, &code)?;
            virtual_file_changes
                .push(json!({"uri":crate::checker::file_uri(&path)?,"type":change}));
        }
        if !virtual_file_changes.is_empty()
            && let Some(server) = server.as_mut()
        {
            server.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes":virtual_file_changes}),
            )?;
        }
        runtime.attach_project(Arc::new(project.clone()), config);
        // Every model patch is available before flow analysis of importing modules.
        for (name, summary) in &project.classes {
            let module = summary
                .pointer("/source/module")
                .and_then(Value::as_str)
                .or_else(|| name.rsplit_once('.').map(|v| v.0));
            if let Some(module) = module.and_then(|name| project.modules.get(name)) {
                if module.external {
                    continue;
                }
                let response =
                    runtime.execute(&module.source, "analyze-class", json!({"class":summary}))?;
                runtime.project_patches.insert(name.clone(), response);
            }
        }
        phase(&mut phases, "class_projection", &mut phase_started);
        let selected = selected_paths(
            &project,
            &root,
            options,
            &self.selection,
            &self.ignored,
            overlays,
        )?;
        let external_targets: BTreeSet<_> = runtime
            .project_contributions
            .iter()
            .filter_map(|c| c.pointer("/target/qualified-name").and_then(Value::as_str))
            .filter_map(|name| {
                project
                    .classes
                    .get(name)
                    .and_then(|class| class.pointer("/source/module"))
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| name.rsplit_once('.').map(|v| v.0.to_owned()))
            })
            .collect();
        // Every plugin can inspect the complete index and class snapshots.
        // Reuse only if those complete responses remain identical, and the
        // module set is unchanged (new/deleted modules can alter resolution).
        let context = json!({"plugin-index-generations":runtime.index_generations(),"index":index,"classes":project.classes,"patches":runtime.project_patches,"virtual-identities":project.virtual_identities});
        let module_set_unchanged = self.cached_sources.keys().eq(project.modules.keys());
        let mut dirty = if module_set_unchanged && self.cached_context.as_ref() == Some(&context) {
            project
                .modules
                .iter()
                .filter(|(name, module)| {
                    self.cached_sources.get(*name).is_none_or(|source| {
                        source.text != module.source.text || source.path != module.source.path
                    })
                })
                .map(|(name, _)| name.clone())
                .collect::<BTreeSet<_>>()
        } else {
            project.modules.keys().cloned().collect()
        };
        let dependencies = project.import_dependencies();
        loop {
            let next = dependencies
                .iter()
                .filter(|(name, imports)| {
                    !dirty.contains(*name)
                        && imports.iter().any(|dependency| dirty.contains(dependency))
                })
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>();
            if next.is_empty() {
                break;
            }
            dirty.extend(next);
        }
        let mut previous = std::mem::take(&mut self.cached_units);
        let mut units = BTreeMap::new();
        for (name, module) in &project.modules {
            if module.external && !external_targets.contains(name) {
                continue;
            }
            let path = if module.external {
                if self
                    .resolved_paths
                    .contains_key(Path::new(&module.source.path))
                {
                    PathBuf::from(&module.source.path)
                } else {
                    projected_stub_path(
                        &stubs,
                        &stub_roots,
                        &self.resolved_paths,
                        Path::new(&module.source.path),
                    )?
                }
            } else {
                projected_source_path(&root, &view, Path::new(&module.source.path))
            };
            let valid = !project.syntax_errors.contains(name);
            if !dirty.contains(name)
                && let Some(mut cached) = previous.remove(&path)
            {
                cached.selected = selected.contains(Path::new(&module.source.path));
                cached.dirty = false;
                units.insert(path, cached);
                continue;
            }
            let analysis = if !valid && (capabilities.is_none() || module.suite.is_empty()) {
                // CLI checking/fixing must preserve original syntax diagnostics.
                // Editors may still project complete prefix declarations for
                // completion in a later incomplete expression.
                Analysis::default()
            } else {
                crate::static_project::analyze(&module.source, &module.suite, runtime)
                    .with_context(|| format!("lower {name}"))?
            };
            let types =
                Types::with_project(&format!("{}result_", module.source.prefix), name, &project);
            units.insert(
                path.clone(),
                File {
                    source: module.source.clone(),
                    suite: module.suite.clone(),
                    analysis,
                    path,
                    oracle: Oracle::new(),
                    patches: ReturnPatches::new(),
                    return_types: BTreeMap::new(),
                    types,
                    diagnostics: Vec::new(),
                    rendered: None,
                    selected: selected.contains(Path::new(&module.source.path)),
                    valid,
                    names: BTreeMap::new(),
                    final_edits: None,
                    dirty: true,
                    raw_diagnostics: Vec::new(),
                    flow_diagnostics: Vec::new(),
                },
            );
        }
        // An installed package retains its original resolution location. Only
        // the checker's in-memory document is changed, never dependency files.
        if !self.notebook_sources.is_empty() || units.keys().any(|path| !path.starts_with(&out)) {
            mode = "push";
            if server.is_none() {
                *server = Some(crate::checker::ProjectChecker::new(
                    Path::new(required("ty")?),
                    &out,
                    &checker_config_path,
                )?);
            }
            for path in &self.notebook_sources {
                server
                    .as_mut()
                    .unwrap()
                    .register_notebook_source(&projected_source_path(&root, &view, path))?;
            }
            for path in units.keys().filter(|path| !path.starts_with(&out)) {
                server.as_mut().unwrap().register_external_source(path)?;
            }
        }
        let originals: BTreeMap<_, _> = project
            .modules
            .values()
            .map(|module| (module.source.path.as_str(), module))
            .collect();
        for document in &self.documents {
            if document.path.starts_with(&self.out) && !units.contains_key(&document.path) {
                if let Some(module) = originals.get(document.source.path.as_str()) {
                    // A removed contribution restores its underlying stub instead
                    // of deleting an otherwise importable dependency.
                    write(&document.path, &module.source.text)?;
                } else if document.path.exists() {
                    std::fs::remove_file(&document.path)?;
                }
            }
        }
        let binary = PathBuf::from(required("ty")?);
        phase(&mut phases, "initial_lowering", &mut phase_started);
        let mut converged = false;
        let mut rounds = 0;
        let mut oracle_queries = 0;
        let mut checked_imports = BTreeSet::new();
        // Each oracle round can advance a plugin-dependent type by one call
        // site. Bound acyclic chains by this program's sites, not a fixed depth.
        let iteration_limit = 16usize.saturating_add(
            units
                .values()
                .map(|unit| unit.analysis.calls.len() + unit.analysis.expression_probes.len() + 1)
                .sum::<usize>()
                .saturating_mul(2),
        );
        let mut seen_states = BTreeSet::new();
        for iteration in 0..iteration_limit {
            rounds = iteration + 1;
            for unit in units.values_mut().filter(|unit| unit.dirty) {
                if !unit.valid {
                    let edits = crate::build_edits(
                        &unit.source,
                        &unit.analysis,
                        crate::mapping::Edits::default(),
                        &unit.patches,
                        false,
                    );
                    let generated = render(&unit.source, &edits)?;
                    unit.final_edits = Some(edits);
                    if unit.path.starts_with(&out) {
                        write(&unit.path, &generated.text)?;
                    }
                    unit.rendered = Some(generated);
                    continue;
                }
                unit.analysis = crate::lower::analyze_with_oracle(
                    &unit.source,
                    &unit.suite,
                    runtime,
                    Some(&unit.oracle),
                )?;
                let flow = crate::flow::project_with_return_types(
                    &unit.source,
                    &unit.suite,
                    &mut unit.analysis,
                    runtime,
                    Some(&unit.oracle),
                    &unit.patches,
                    &unit.return_types,
                )
                .with_context(|| format!("flow {}", unit.source.module))?;
                let mut edits = crate::build_edits(
                    &unit.source,
                    &unit.analysis,
                    flow.edits,
                    &unit.patches,
                    true,
                );
                edits.insert(unit.analysis.prelude_offset, unit.types.prelude());
                edits.insert(unit.analysis.prelude_offset, flow.type_prelude);
                let generated = render(&unit.source, &edits)
                    .with_context(|| format!("render {}", unit.source.module))?;
                if unit.path.starts_with(&out) {
                    write(&unit.path, &generated.text)?;
                }
                unit.rendered = Some(generated);
            }
            phase(&mut phases, "flow_and_render", &mut phase_started);
            let mut candidates = BTreeSet::new();
            for unit in units.values() {
                if unit.valid && unit.dirty {
                    candidates.extend(imports(
                        &unit.rendered.as_ref().unwrap().text,
                        project.source_type(Path::new(&unit.source.path)),
                    )?);
                }
            }
            let candidates: Vec<_> = candidates.difference(&checked_imports).cloned().collect();
            if !candidates.is_empty() {
                let preflight = out.join("type-imports.py");
                write(
                    &preflight,
                    &candidates
                        .iter()
                        .map(|module| format!("import {module}\n"))
                        .collect::<String>(),
                )?;
                let unavailable = if mode == "push" {
                    if server.is_none() {
                        *server = Some(crate::checker::ProjectChecker::new(
                            &binary,
                            &out,
                            &checker_config_path,
                        )?);
                    }
                    let mut sources: BTreeMap<_, _> = units
                        .iter()
                        .map(|(p, u)| (p.clone(), u.rendered.as_ref().unwrap().text.clone()))
                        .collect();
                    sources.insert(preflight.clone(), std::fs::read_to_string(&preflight)?);
                    server
                        .as_mut()
                        .unwrap()
                        .check(&sources, std::slice::from_ref(&preflight))?
                } else {
                    check(
                        &binary,
                        &view,
                        &checker_config_path,
                        std::slice::from_ref(&preflight),
                    )?
                };
                write(
                    &out.join(format!("type-imports-{iteration}.json")),
                    &serde_json::to_string_pretty(&unavailable)?,
                )?;
                let mut changed = false;
                for diagnostic in unavailable {
                    ensure!(
                        diagnostic["check_name"] == "unresolved-import"
                            && path_of(&diagnostic, &view) == preflight,
                        "unexpected type-import preflight diagnostic: {diagnostic}"
                    );
                    let line = diagnostic["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .context("import diagnostic line")? as usize;
                    let module = candidates
                        .get(line.checked_sub(1).context("import line zero")?)
                        .context("import diagnostic outside preflight")?;
                    changed |= project.unavailable_modules.insert(module.clone());
                }
                checked_imports.extend(candidates);
                if changed {
                    if capabilities.is_none() {
                        *server = None;
                    }
                    for (path, code) in project.register_virtual_types(&definitions)? {
                        write(&stubs.join(path), &code)?;
                    }
                    runtime.attach_project(Arc::new(project.clone()), runtime.config.clone());
                    for unit in units.values_mut() {
                        if !unit.dirty {
                            unit.dirty = true;
                            unit.oracle.clear();
                            unit.patches.clear();
                            unit.return_types.clear();
                            unit.names.clear();
                        }
                        unit.analysis =
                            if !unit.valid && (capabilities.is_none() || unit.suite.is_empty()) {
                                Analysis::default()
                            } else {
                                crate::static_project::analyze(&unit.source, &unit.suite, runtime)?
                            };
                        unit.types = Types::with_project(
                            &format!("{}result_", unit.source.prefix),
                            &unit.source.module,
                            &project,
                        );
                        unit.patches = unit
                            .return_types
                            .iter()
                            .map(|(span, value)| Ok((*span, unit.types.render(value)?)))
                            .collect::<Result<_>>()?;
                    }
                    continue;
                }
            }
            phase(&mut phases, "import_preflight", &mut phase_started);
            let query_paths: Vec<_> = units
                .values()
                .filter(|u| {
                    u.dirty
                        && u.valid
                        && (!u.analysis.calls.is_empty()
                            || !u.analysis.expression_probes.is_empty())
                })
                .map(|u| u.path.clone())
                .collect();
            let raw = check_units(
                &binary,
                &view,
                &checker_config_path,
                &units,
                &query_paths,
                mode == "push",
                server,
                capabilities,
            )?;
            phase(&mut phases, "oracle_check", &mut phase_started);
            oracle_queries += 1;
            write(
                &out.join(format!("oracle-{iteration}.json")),
                &serde_json::to_string_pretty(&raw)?,
            )?;
            for path in &query_paths {
                let unit = &units[path];
                let diagnostics: Vec<_> = raw
                    .iter()
                    .filter(|diagnostic| path_of(diagnostic, &view) == *path)
                    .cloned()
                    .collect();
                crate::mapping::validate_probe_health(
                    &unit.source,
                    unit.rendered.as_ref().context("rendered query source")?,
                    &diagnostics,
                )?;
            }
            let old_oracles: BTreeMap<_, _> = units
                .iter()
                .map(|(path, unit)| (path.clone(), unit.oracle.clone()))
                .collect();
            for unit in units.values_mut().filter(|unit| unit.dirty) {
                unit.oracle.clear();
            }
            for d in &raw {
                if d["check_name"] != "revealed-type" {
                    continue;
                }
                if let Some(unit) = units.get_mut(&path_of(d, &view)) {
                    let pos = &d["location"]["positions"]["begin"];
                    let coordinate = (
                        pos["line"].as_u64().unwrap_or(0) as usize,
                        pos["column"].as_u64().unwrap_or(0) as usize,
                    );
                    if let Some(key) = unit.rendered.as_ref().unwrap().probes.get(&coordinate) {
                        let description =
                            d["description"].as_str().context("reveal description")?;
                        let start = description
                            .find("Revealed type: `")
                            .context("type oracle format")?
                            + "Revealed type: `".len();
                        let end = description.rfind('`').context("type oracle end")?;
                        unit.oracle
                            .insert(key.clone(), description[start..end].into());
                    }
                }
            }
            let mut stable = iteration > 0 || query_paths.is_empty();
            for unit in units.values_mut().filter(|unit| unit.dirty) {
                let mut next = ReturnPatches::new();
                let mut return_types = BTreeMap::new();
                let mut types = Types::with_project(
                    &format!("{}result_", unit.source.prefix),
                    &unit.source.module,
                    &project,
                );
                let mut diagnostics = Vec::new();
                for call in unit.analysis.calls.values() {
                    let receiver = call
                        .receiver_span
                        .and_then(|span| unit.return_types.get(&span))
                        .and_then(Runtime::receiver_identity);
                    let response = call_request(
                        runtime,
                        &unit.source,
                        "adjust-call-return",
                        call,
                        Some(&unit.oracle),
                        receiver.as_deref(),
                    )?;
                    // Returning the exact host fallback leaves the original
                    // checker type intact, including names absent from this module.
                    let unchanged = unit
                        .oracle
                        .get(&call.return_key())
                        .is_some_and(|expression| {
                            response["return-type"] == runtime.type_expr(&unit.source, expression)
                        });
                    if response["kind"] == "call-return-patch" && !unchanged {
                        next.insert(call.span, types.render(&response["return-type"])?);
                        return_types.insert(call.span, response["return-type"].clone());
                    }
                    for d in response["diagnostics"].as_array().into_iter().flatten() {
                        diagnostics
                            .push(json!({"diagnostic":d,"source":unit.source.location(call.span)}));
                    }
                }
                stable &= next == unit.patches && old_oracles.get(&unit.path) == Some(&unit.oracle);
                unit.patches = next;
                unit.return_types = return_types;
                unit.types = types;
                unit.diagnostics = diagnostics;
            }
            phase(&mut phases, "hook_responses", &mut phase_started);
            if stable {
                converged = true;
                break;
            }
            use sha2::{Digest, Sha256};
            let mut state = Sha256::new();
            for unit in units.values().filter(|unit| unit.dirty) {
                state.update(serde_json::to_vec(&(
                    &unit.path,
                    &unit.oracle,
                    unit.patches
                        .iter()
                        .map(|(span, value)| (span.start, span.end, value))
                        .collect::<Vec<_>>(),
                    unit.rendered.as_ref().map(|rendered| &rendered.text),
                ))?);
            }
            ensure!(
                seen_states.insert(state.finalize().to_vec()),
                "plugin inference repeated a non-converged state after {rounds} rounds"
            );
        }
        ensure!(
            converged,
            "plugin inference did not converge within {iteration_limit} rounds for this project's hook sites"
        );
        for unit in units.values_mut().filter(|unit| unit.dirty) {
            if !unit.valid {
                continue;
            }
            let flow = crate::flow::project_with_return_types(
                &unit.source,
                &unit.suite,
                &mut unit.analysis,
                runtime,
                Some(&unit.oracle),
                &unit.patches,
                &unit.return_types,
            )?;
            unit.names.extend(flow.type_names.clone());
            let mut edits = crate::build_edits(
                &unit.source,
                &unit.analysis,
                flow.edits,
                &unit.patches,
                false,
            );
            edits.insert(unit.analysis.prelude_offset, unit.types.prelude());
            edits.insert(unit.analysis.prelude_offset, flow.type_prelude);
            let generated = render(&unit.source, &edits)?;
            unit.final_edits = Some(edits);
            if unit.path.starts_with(&out) {
                write(&unit.path, &generated.text)?;
            }
            unit.rendered = Some(generated);
            unit.flow_diagnostics = flow.diagnostics;
        }
        phase(&mut phases, "final_render", &mut phase_started);
        let paths: Vec<_> = units
            .values()
            .filter(|u| u.selected && u.dirty)
            .map(|u| u.path.clone())
            .collect();
        let mut raw = check_units(
            &binary,
            &view,
            &checker_config_path,
            &units,
            &paths,
            mode == "push",
            server,
            capabilities,
        )?;
        if capabilities.is_none() {
            for unit in units.values().filter(|u| {
                u.selected
                    && u.dirty
                    && Path::new(&u.source.path)
                        .extension()
                        .is_some_and(|e| e == "ipynb")
            }) {
                let document = ProjectDocument {
                    source: unit.source.clone(),
                    path: unit.path.clone(),
                    rendered: unit.rendered.as_ref().unwrap().clone(),
                    names: unit.names.clone(),
                };
                let native = crate::notebook::check_disk(
                    &document,
                    server.as_mut().context("notebook checker")?,
                )?;
                raw.retain(|diagnostic| path_of(diagnostic, &view) != unit.path);
                raw.extend(native);
            }
        }
        for unit in units.values_mut().filter(|unit| unit.selected) {
            if unit.dirty {
                unit.raw_diagnostics = raw
                    .iter()
                    .filter(|diagnostic| path_of(diagnostic, &view) == unit.path)
                    .cloned()
                    .collect();
            } else {
                raw.extend(unit.raw_diagnostics.clone());
            }
        }
        phase(&mut phases, "final_check", &mut phase_started);
        write(
            &out.join("raw-diagnostics.json"),
            &serde_json::to_string_pretty(&raw)?,
        )?;
        let mut diagnostics = Vec::new();
        let mut suppressed = Vec::new();
        let mut generated_declarations = GeneratedDeclarations::default();
        for definition in &definitions {
            let Some(owner) = definition["name"].as_str() else {
                continue;
            };
            for member in definition["shape"]["members"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let access = &member["access"];
                if access["kind"] == "descriptor"
                    && access.get("class-type").is_some()
                    && let Some(name) = member["name"].as_str()
                {
                    generated_declarations
                        .descriptors
                        .entry(owner.to_owned())
                        .or_default()
                        .insert(name.to_owned());
                }
            }
        }
        for unit in units.values() {
            generated_declarations.extend(&unit.analysis.generated_declarations);
        }
        for unit in units.values().filter(|u| u.selected) {
            let local: Vec<_> = raw
                .iter()
                .filter(|d| path_of(d, &view) == unit.path)
                .cloned()
                .collect();
            let mut plugin = unit.analysis.plugin_diagnostics.clone();
            plugin.extend(unit.diagnostics.clone());
            plugin.extend(unit.flow_diagnostics.clone());
            plugin.extend(
                project_diagnostics
                    .iter()
                    .filter(|d| d["location"]["file-path"] == unit.source.path)
                    .map(|d| json!({"diagnostic":d,"source":d["location"]})),
            );
            let origins = diagnostic_origins(unit, &project, &generated_declarations);
            let (mapped, hidden) = map_diagnostics_with_origins(
                &unit.source,
                unit.rendered.as_ref().unwrap(),
                &local,
                &unit.analysis.helpers,
                &plugin,
                &origins,
            )
            .with_context(|| {
                format!(
                    "diagnostic mapping {}; see raw-diagnostics.json",
                    unit.source.module
                )
            })?;
            diagnostics.extend(mapped);
            suppressed.extend(hidden);
        }
        diagnostics.sort_by_key(|d| d.to_string());
        diagnostics.dedup();
        phase(&mut phases, "diagnostic_mapping", &mut phase_started);
        let mut result = json!({"diagnostics":diagnostics,"suppressed_generated":suppressed,"implementation":"rust","plugin_runtime":"embedded-monty","project":root,"checker":binary,"checker_mode":mode,"configuration":self.config_source,"revision":self.revisions,"external_resolution_requests":self.external_resolution_requests,"projection_rounds":rounds,"oracle_rounds":oracle_queries,"monty_executions":runtime.executions,"monty_cache_hits":runtime.hits,"monty_seconds":runtime.execution_time.as_secs_f64(),"seconds":started.elapsed().as_secs_f64()});
        let virtual_names = runtime.virtual_display_names();
        self.names = project
            .virtual_identities
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    virtual_names.get(value).unwrap_or(value).clone(),
                )
            })
            .collect();
        self.names.extend(virtual_names);
        for unit in units.values_mut() {
            unit.names.extend(unit.analysis.helpers.clone());
            unit.names.extend(unit.types.display_names());
        }
        self.active_documents = units.keys().cloned().collect();
        self.projection_edits = units
            .values()
            .filter_map(|unit| {
                unit.final_edits
                    .clone()
                    .map(|edits| (unit.source.path.clone(), edits))
            })
            .collect();
        self.documents = units
            .values()
            .map(|unit| ProjectDocument {
                source: unit.source.clone(),
                path: unit.path.clone(),
                rendered: unit.rendered.as_ref().unwrap().clone(),
                names: unit.names.clone(),
            })
            .collect();
        for module in project.modules.values().filter(|m| m.external) {
            let projected = projected_stub_path(
                &stubs,
                &stub_roots,
                &self.resolved_paths,
                Path::new(&module.source.path),
            )?;
            if !self
                .documents
                .iter()
                .any(|document| document.path == projected)
            {
                let path = if self
                    .resolved_paths
                    .contains_key(Path::new(&module.source.path))
                {
                    PathBuf::from(&module.source.path)
                } else {
                    projected
                };
                self.documents.push(ProjectDocument {
                    source: module.source.clone(),
                    path,
                    rendered: render(&module.source, &crate::mapping::Edits::default())?,
                    names: BTreeMap::new(),
                });
            }
        }
        phase(&mut phases, "source_maps", &mut phase_started);
        result["projected_units"] = json!(units.values().filter(|unit| unit.dirty).count());
        result["reused_units"] = json!(units.values().filter(|unit| !unit.dirty).count());
        self.cached_sources = project
            .modules
            .iter()
            .map(|(name, module)| (name.clone(), module.source.clone()))
            .collect();
        self.cached_context = Some(context);
        self.base_project.unavailable_modules = project.unavailable_modules.clone();
        self.cached_units = units;
        result["checker_mode_effective"] = json!(mode);
        result["phases"] = json!(phases);
        result["monty_executions_delta"] =
            json!(runtime.executions.saturating_sub(executions_before));
        result["monty_cache_hits_delta"] = json!(runtime.hits.saturating_sub(hits_before));
        result["monty_seconds_delta"] = json!(
            runtime
                .execution_time
                .saturating_sub(monty_before)
                .as_secs_f64()
        );
        result["seconds"] = json!(started.elapsed().as_secs_f64());
        write(
            &out.join("results.json"),
            &serde_json::to_string_pretty(&result)?,
        )?;
        self.last_result = Some(result.clone());
        Ok(result)
    }
}

pub fn run(input: &Path, options: &BTreeMap<String, String>) -> Result<Value> {
    Session::new(input, options)?.check(&BTreeMap::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_config_filters_before_parsing_and_preserves_source_roots() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL")
            .expect("TY_COMPANION_TEST_OFFICIAL must point to the unchanged official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-session-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("input");
        let output = directory.join("output");
        write(
            &root.join("ty.toml"),
            "[environment]\nroot=['src']\npython-platform='win32'\n[src]\ninclude=['src']\nexclude=['src/ignored.py']\n[rules]\ninvalid-assignment='warn'\n",
        )?;
        write(&root.join("src/pkg/__init__.py"), "")?;
        write(&root.join("src/pkg/model.py"), "class Model: pass\n")?;
        write(
            &root.join("src/check.py"),
            "from pkg.model import Model\nmodel = Model()\nvalue: int = 'bad'\n",
        )?;
        write(&root.join("src/ignored.py"), "this is !!! invalid python")?;
        write(&root.join("excluded.py"), "this is !!! invalid python")?;
        let options = BTreeMap::from([
            ("out".into(), output.to_string_lossy().into_owned()),
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            ("checker-mode".into(), "push".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        assert_eq!(session.runtime.platform, "win32");
        assert!(session.base_project.classes.contains_key("pkg.model.Model"));
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["oracle_rounds"], 0);
        assert_eq!(result["monty_executions"], 0);
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{result:#}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        assert_eq!(diagnostics[0]["severity"], "minor");
        let canonical = result["diagnostics"].clone();
        let rich = session.rich_diagnostics()?;
        assert_eq!(rich.len(), 1);
        assert_eq!(rich[0]["check_name"], "invalid-assignment");
        let presentation_pid = session.rich_server.as_ref().unwrap().process_id();
        assert_eq!(session.rich_diagnostics()?, rich);
        assert_eq!(
            session.rich_server.as_ref().unwrap().process_id(),
            presentation_pid
        );
        assert_eq!(
            session.last_result.as_ref().unwrap()["diagnostics"],
            canonical
        );
        assert_eq!(session.runtime.executions, 0);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn editor_session_reuses_checker_and_projects_unsaved_source() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL")
            .expect("TY_COMPANION_TEST_OFFICIAL must point to the unchanged official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-session-editor-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("input");
        let output = directory.join("output");
        let stubs = directory.join("stubs");
        std::fs::create_dir_all(&root)?;
        std::fs::create_dir_all(&stubs)?;
        let original = "class Resource: pass\nitem = Resource()\nvalue: str = item.label\n";
        let file = root.join("demo.py");
        write(&file, original)?;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
        let options = BTreeMap::from([
            ("out".into(), output.to_string_lossy().into_owned()),
            ("stubs".into(), stubs.to_string_lossy().into_owned()),
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            (
                "manifest".into(),
                fixtures
                    .join("plugin.plugin.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "plugin".into(),
                fixtures.join("plugin.py").to_string_lossy().into_owned(),
            ),
            ("checker-mode".into(), "push".into()),
            ("lsp-capabilities".into(), "{}".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        let pid = session.checker_mut().unwrap().process_id();
        let first = session.check(&BTreeMap::new())?;
        assert_eq!(first["diagnostics"], json!([]), "{first:#}");
        assert!(first["monty_seconds_delta"].as_f64().unwrap() > 0.0);
        let same = session.check(&BTreeMap::new())?;
        assert_eq!(same["reused_projection"], true);
        assert_eq!(same["monty_executions"], first["monty_executions"]);
        let file = file.canonicalize()?;
        for comment in ["# first edit\n", "# differently sized second edit\n"] {
            let trivia = session.check(&BTreeMap::from([(
                file.clone(),
                format!("{original}{comment}"),
            )]))?;
            assert_eq!(trivia["reuse_reason"], "trailing-trivia");
            assert_eq!(trivia["monty_executions_delta"], 0);
            assert_eq!(trivia["monty_cache_hits_delta"], 0);
            assert_eq!(trivia["monty_seconds_delta"], 0.0);
            assert_eq!(trivia["oracle_rounds"], 0);
            assert_eq!(trivia["diagnostics"], first["diagnostics"]);
        }
        let edited = original.replace("value: str", "value: int");
        let changed = session.check(&BTreeMap::from([(file.clone(), edited)]))?;
        assert!(
            changed["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["check_name"] == "invalid-assignment"),
            "{changed:#}"
        );
        assert_eq!(session.checker_mut().unwrap().process_id(), pid);
        assert_eq!(std::fs::read_to_string(&file)?, original);
        let partial = "class Resource: pass\nitem = Resource()\nitem.\n";
        session.check(&BTreeMap::from([(file.clone(), partial.into())]))?;
        let projected = session
            .documents()
            .into_iter()
            .find(|d| d.source.path == file.to_string_lossy())
            .unwrap();
        assert_eq!(projected.source.text, partial);
        assert!(
            projected.rendered.text.contains("label:"),
            "{}",
            projected.rendered.text
        );
        let restored = session.check(&BTreeMap::new())?;
        assert_eq!(restored["diagnostics"], json!([]), "{restored:#}");
        assert_eq!(session.checker_mut().unwrap().process_id(), pid);
        session.invalidate();
        let rechecked = session.check(&BTreeMap::new())?;
        assert!(rechecked.get("reused_projection").is_none());
        assert_eq!(rechecked["diagnostics"], json!([]), "{rechecked:#}");
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn incremental_projection_invalidates_import_dependents_index_and_module_changes() -> Result<()>
    {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-incremental-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("input");
        write(&root.join("source.py"), "value = 1\n")?;
        write(
            &root.join("middle.py"),
            "from source import *\nalias = value\n",
        )?;
        write(
            &root.join("consumer.py"),
            "import middle as bridge\ntyped: int = bridge.alias\n",
        )?;
        write(
            &root.join("demo.py"),
            "class Resource: pass\nitem = Resource()\nlabel: str = item.label\n",
        )?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_project_index\ndef index(request):\n    return project_index(plugin_index={'count': len(request.get('assignments', []))})\n\n@on_class_transform\ndef transform(request):\n    if request['class'].get('fields'):\n        raise ValueError('intentional revision failure')\n    return class_patch(fields=[field('label', type_annotation('str'))])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest,&json!({"id":"research.incremental","name":"Incremental projection","version":"0.0.1","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true,"project-index":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Resource"}]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("lsp-capabilities".into(), "{}".into()),
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        let first = session.check(&BTreeMap::new())?;
        assert_eq!(first["diagnostics"], json!([]), "{first:#}");
        assert_eq!(first["projected_units"], 4);
        let file = root.join("source.py").canonicalize()?;
        let changed = session.check(&BTreeMap::from([(
            file.clone(),
            "value = 'wrong'\n".into(),
        )]))?;
        assert_eq!(changed["projected_units"], 3, "{changed:#}");
        assert_eq!(changed["reused_units"], 1, "{changed:#}");
        let diagnostics = changed["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{changed:#}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        assert!(
            diagnostics[0]["location"]["path"]
                .as_str()
                .unwrap()
                .ends_with("consumer.py")
        );
        let index_changed = session.check(&BTreeMap::from([(
            file.clone(),
            "value = 'wrong'\nmarker = 1\n".into(),
        )]))?;
        assert_eq!(index_changed["projected_units"], 4, "{index_changed:#}");
        assert_eq!(index_changed["reused_units"], 0);
        write(
            &root.join("new.py"),
            "# module addition changes import resolution\n",
        )?;
        let added = session.check(&BTreeMap::from([(
            file.clone(),
            "value = 'wrong'\nmarker = 1\n".into(),
        )]))?;
        assert_eq!(added["projected_units"], 5, "{added:#}");
        std::fs::remove_file(root.join("new.py"))?;
        let removed = session.check(&BTreeMap::from([(
            file,
            "value = 'wrong'\nmarker = 1\n".into(),
        )]))?;
        assert_eq!(removed["projected_units"], 4, "{removed:#}");
        assert_eq!(removed["diagnostics"], index_changed["diagnostics"]);
        let failing = BTreeMap::from([(
            root.join("demo.py").canonicalize()?,
            "class Resource:\n    broken: int\n".into(),
        )]);
        assert!(session.check(&failing).is_err());
        assert!(
            session.check(&failing).is_err(),
            "failed revision must not reuse old diagnostics on retry"
        );
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn external_ancestry_uses_official_stub_resolution_and_local_shadowing() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-external-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("project");
        let library = directory.join("library");
        write(
            &library.join("external.pyi"),
            "class Base: ...\nclass Child(Base): ...\n",
        )?;
        write(
            &library.join("external.py"),
            "class Base: pass\nclass Child: pass\n",
        )?;
        write(
            &root.join("ty.toml"),
            &format!(
                "[environment]\nextra-paths=[{}]\npython-version='3.13'\n",
                serde_json::to_string(&library)?
            ),
        )?;
        write(&library.join("other.pyi"), "class Other: ...\n")?;
        let source = "from external import Child\nfrom other import Other\nclass Resource(Child): pass\nitem = Resource()\nvalue: str = item.label\nexternal_value: int = Child().extra\nunrelated: int = Other().flag\n";
        write(&root.join("demo.py"), source)?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    return class_patch(fields=[field('label', type_annotation('str'))])\n\n@on_project_index\ndef index(request):\n    return project_index(contributions=[contribution({}, instance_target('external.Base'), field_contribution(field('extra', type_annotation('int'))), 'extra'), contribution({}, instance_target('other.Other'), field_contribution(field('flag', type_annotation('int'))), 'flag')])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest,&json!({"id":"research.external","name":"External ancestry","version":"0.0.1","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true,"project-index":true},"claims":{"classes":[{"kind":"subclass-of","base-qualified-name":"external.Base"}]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("lsp-capabilities".into(), "{}".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        let external = session
            .base_project
            .modules
            .get("external")
            .context("external module indexed")?;
        assert!(
            external.source.path.ends_with("external.pyi"),
            "{}",
            external.source.path
        );
        assert!(session.external_resolution_requests > 0);
        assert!(session.base_project.modules.contains_key("other"));
        assert_eq!(
            std::fs::read_to_string(library.join("other.pyi"))?,
            "class Other: ...\n"
        );
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, source);
        assert_eq!(
            std::fs::read_to_string(library.join("external.pyi"))?,
            "class Base: ...\nclass Child(Base): ...\n"
        );
        write(&root.join("external.py"), "class Child: pass\n")?;
        session.invalidate();
        let shadowed = session.check(&BTreeMap::new())?;
        assert!(
            shadowed["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["check_name"] == "unresolved-attribute"),
            "{shadowed:#}"
        );
        assert!(!session.base_project.modules["external"].external);
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    return class_patch()\n\n@on_project_index\ndef index(request):\n    return project_index()\n",
        )?;
        session.invalidate();
        let removed = session.check(&BTreeMap::new())?;
        assert!(
            removed["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["check_name"] == "unresolved-attribute"
                    && d["description"]
                        .as_str()
                        .is_some_and(|message| message.contains("flag"))),
            "{removed:#}"
        );
        assert_eq!(
            std::fs::read_to_string(library.join("other.pyi"))?,
            "class Other: ...\n"
        );
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn disk_notebooks_preserve_cells_and_coexist_with_same_named_python_modules() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-session-notebook-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("input");
        write(&root.join("demo.py"), "class Resource: pass\n")?;
        let notebook = root.join("demo.ipynb");
        let original = json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[
            {"id":"a","cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":["from demo import Resource\n", "item = Resource()"]},
            {"id":"b","cell_type":"markdown","metadata":{},"source":["Preserved markdown"]},
            {"id":"c","cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":["value: int = item.label"]}
        ]}).to_string();
        write(&notebook, &original)?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            (
                "manifest".into(),
                fixture
                    .join("plugin.plugin.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
            (
                "plugin".into(),
                fixture.join("plugin.py").to_string_lossy().into_owned(),
            ),
            ("public-cli".into(), "true".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        assert_eq!(session.base_project.modules.len(), 2);
        assert!(session.base_project.modules.contains_key("demo"));
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["checker_mode_effective"], "push");
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{result:#}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        assert_eq!(
            Path::new(diagnostics[0]["location"]["path"].as_str().unwrap()),
            notebook.canonicalize()?
        );
        assert_eq!(std::fs::read_to_string(&notebook)?, original);
        write(&notebook, &original.replace("value: int", "value: str"))?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        let split_statement = json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[
            {"id":"a","cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":["if True:"]},
            {"id":"b","cell_type":"code","metadata":{},"execution_count":null,"outputs":[],"source":["    value = 1"]}
        ]}).to_string();
        write(&notebook, &split_statement)?;
        let result = session.check(&BTreeMap::new())?;
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["check_name"] == "invalid-syntax"),
            "{result:#}"
        );
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn explicit_outside_sources_use_memory_overlays_and_follow_directory_changes() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-session-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("input");
        let external = directory.join("external");
        write(
            &root.join("ignored.py"),
            "root_value: int = 'not selected'\n",
        )?;
        let path = external.join("source.py");
        let original = "value: int = 'bad'\n";
        write(&path, original)?;
        let selected = directory.join("selected.json");
        let selection = serde_json::to_string(&vec![&external])?;
        write(&selected, &selection)?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            ("files".into(), selected.to_string_lossy().into_owned()),
            ("explicit-files".into(), selection),
            ("public-cli".into(), "true".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["checker_mode_effective"], "push");
        assert_eq!(
            result["diagnostics"].as_array().unwrap().len(),
            1,
            "{result:#}"
        );
        assert_eq!(result["diagnostics"][0]["check_name"], "invalid-assignment");
        assert_eq!(
            session
                .documents
                .iter()
                .find(|d| Path::new(&d.source.path) == path.canonicalize().unwrap())
                .unwrap()
                .path,
            path.canonicalize()?
        );
        assert_eq!(std::fs::read_to_string(&path)?, original);
        let extra = external.join("extra.py");
        write(&extra, "new: str = 123\n")?;
        assert_eq!(
            session.check(&BTreeMap::new())?["diagnostics"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        std::fs::remove_file(&path)?;
        assert_eq!(
            session.check(&BTreeMap::new())?["diagnostics"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn reused_projection_output_is_rejected_before_stale_files_can_affect_checking() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-native-project-output-{}", std::process::id()));
        let input = directory.join("input");
        let output = directory.join("output");
        std::fs::create_dir_all(&input)?;
        let options = BTreeMap::from([("out".into(), output.to_string_lossy().into_owned())]);
        for generated in ["project", "stubs"] {
            let marker = output.join(generated).join("stale.py");
            write(&marker, "stale = 1\n")?;
            let error = run(&input, &options).unwrap_err();
            assert!(
                error.to_string().contains("use a fresh --out directory"),
                "{error:#}"
            );
            assert_eq!(std::fs::read_to_string(&marker)?, "stale = 1\n");
            std::fs::remove_dir_all(&output)?;
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
