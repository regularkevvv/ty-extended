//! Project-wide syntax summaries and source forms for plugin-defined types.
//!
//! This module deliberately knows nothing about any particular framework. It
//! consumes the public plugin protocol, imports, and Python syntax. Settings are
//! interpreted statically; importing or executing a project's Python is forbidden.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail, ensure};
use ruff_python_ast::{self as ast, Expr, Stmt};
use serde_json::{Value, json};

use crate::model::{Source, span, typ};

#[derive(Clone)]
pub struct Module {
    pub source: Arc<Source>,
    pub suite: Arc<Vec<Stmt>>,
    pub imports: BTreeMap<String, String>,
    pub bindings: BTreeSet<String>,
    pub assignments: BTreeMap<String, Expr>,
    pub external: bool,
}

#[derive(Clone)]
struct IndexFunction {
    summary: Value,
    name_offset: usize,
}

#[derive(Clone)]
pub struct Project {
    pub root: PathBuf,
    source_roots: Vec<PathBuf>,
    pub modules: BTreeMap<String, Module>,
    pub classes: BTreeMap<String, Value>,
    pub config: Value,
    pub manifest: Value,
    /// Hygienic generated declaration name -> public protocol identity.
    pub virtual_identities: BTreeMap<String, String>,
    /// Public SDK module -> isolated physical module, independent of user imports.
    pub virtual_modules: BTreeMap<String, String>,
    /// Modules proven unavailable by the official checker's environment. Absence
    /// from the syntax index alone must never populate this set.
    pub unavailable_modules: BTreeSet<String>,
    /// Native-checker evidence for TypeSnapshot::Nominal resolution and arity.
    pub snapshot_nominals: BTreeMap<String, Option<usize>>,
    pub snapshot_symbols: BTreeMap<String, bool>,
    /// Modules with incomplete editor buffers are checked unchanged by ty.
    pub syntax_errors: BTreeSet<String>,
    allow_invalid: bool,
    notebook_sources: BTreeSet<PathBuf>,
    additional_sources: BTreeSet<PathBuf>,
    assignments: Vec<Value>,
    functions: Vec<IndexFunction>,
    settings: Vec<Value>,
}

pub fn parse_source(
    text: &str,
    source_type: ast::PySourceType,
) -> Result<ruff_python_parser::Parsed<ast::ModModule>> {
    let parsed = ruff_python_parser::parse_unchecked_source(text, source_type);
    if let Some(error) = parsed.errors().first() {
        bail!("{error}");
    }
    Ok(parsed)
}

fn collect_module_functions<'a>(suite: &'a [Stmt], functions: &mut Vec<&'a ast::StmtFunctionDef>) {
    for statement in suite {
        match statement {
            Stmt::FunctionDef(function) => functions.push(function),
            Stmt::If(branch) => {
                collect_module_functions(&branch.body, functions);
                for clause in &branch.elif_else_clauses {
                    collect_module_functions(&clause.body, functions);
                }
            }
            Stmt::While(loop_) => {
                collect_module_functions(&loop_.body, functions);
                collect_module_functions(&loop_.orelse, functions);
            }
            Stmt::For(loop_) => {
                collect_module_functions(&loop_.body, functions);
                collect_module_functions(&loop_.orelse, functions);
            }
            Stmt::With(with) => collect_module_functions(&with.body, functions),
            Stmt::Try(try_) => {
                collect_module_functions(&try_.body, functions);
                for ast::ExceptHandler::ExceptHandler(handler) in &try_.handlers {
                    collect_module_functions(&handler.body, functions);
                }
                collect_module_functions(&try_.orelse, functions);
                collect_module_functions(&try_.finalbody, functions);
            }
            Stmt::Match(match_) => {
                for case in &match_.cases {
                    collect_module_functions(&case.body, functions);
                }
            }
            // Do not descend into class bodies or function-local definitions.
            _ => {}
        }
    }
}

impl Project {
    #[cfg(test)]
    pub fn load(
        root: &Path,
        source_paths: &[PathBuf],
        manifest: &Value,
        config: Value,
    ) -> Result<Self> {
        Self::load_inner(root, source_paths, manifest, config, false, false)
    }

    #[cfg(test)]
    pub fn load_editor(root: &Path, manifest: &Value, config: Value) -> Result<Self> {
        Self::load_inner(root, &[], manifest, config, true, false)
    }

    pub fn load_selected(
        root: &Path,
        paths: &[PathBuf],
        manifest: &Value,
        config: Value,
        editor: bool,
    ) -> Result<Self> {
        Self::load_inner(root, paths, manifest, config, editor, true)
    }

    fn load_inner(
        root: &Path,
        source_paths: &[PathBuf],
        manifest: &Value,
        config: Value,
        allow_invalid: bool,
        exact_paths: bool,
    ) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("project root {}", root.display()))?;
        let mut project = Self {
            source_roots: vec![root.clone()],
            root,
            modules: BTreeMap::new(),
            classes: BTreeMap::new(),
            config,
            manifest: manifest.clone(),
            virtual_identities: BTreeMap::new(),
            virtual_modules: BTreeMap::new(),
            unavailable_modules: BTreeSet::new(),
            snapshot_nominals: BTreeMap::new(),
            snapshot_symbols: BTreeMap::new(),
            syntax_errors: BTreeSet::new(),
            allow_invalid,
            notebook_sources: BTreeSet::new(),
            additional_sources: BTreeSet::new(),
            assignments: vec![],
            functions: vec![],
            settings: vec![],
        };
        let mut paths = source_paths.to_vec();
        if paths.is_empty() && !exact_paths {
            collect_python(&project.root, &mut paths)?;
        }
        for path in paths {
            let path = if path.is_absolute() {
                path
            } else {
                project.root.join(path)
            };
            let path = path
                .canonicalize()
                .with_context(|| format!("source file {}", path.display()))?;
            if !path.starts_with(&project.root) {
                project.additional_sources.insert(path.clone());
            }
            let module = project.source_module_name(&path)?;
            project.add_module(&module, &path, false)?;
        }
        // Claims select settings modules; they are dependencies even when user
        // code never imports them directly.
        for name in project.settings_modules() {
            if let Some(path) = find_module(&project.root, &name) {
                project.add_module(&name, &path, false)?;
            }
        }
        project.load_imports()?;
        project.rebuild()?;
        Ok(project)
    }

    /// Match ty's configured source roots when resolving module identities.
    pub fn set_source_roots(&mut self, roots: Vec<PathBuf>) -> Result<()> {
        if self.source_roots == roots {
            return Ok(());
        }
        self.source_roots = roots;
        let locals: Vec<_> = self
            .modules
            .values()
            .filter(|m| !m.external)
            .cloned()
            .collect();
        self.modules.retain(|_, m| m.external);
        self.syntax_errors.clear();
        for module in locals {
            let path = Path::new(&module.source.path);
            let name = self.source_module_name(path)?;
            self.add_module_text(&name, path, false, module.source.text.clone())?;
        }
        self.load_imports()?;
        self.rebuild()
    }

    pub fn register_notebook_source(&mut self, path: &Path) -> Result<()> {
        if self.notebook_sources.insert(path.into())
            && let Some((name, module)) = self
                .modules
                .iter()
                .find(|(_, m)| Path::new(&m.source.path) == path)
                .map(|(n, m)| (n.clone(), m.clone()))
        {
            self.add_module_text(&name, path, module.external, module.source.text.clone())?;
            self.rebuild()?;
        }
        Ok(())
    }

    pub fn source_type(&self, path: &Path) -> ruff_python_ast::PySourceType {
        if self.notebook_sources.contains(path)
            || path.extension().is_some_and(|ext| ext == "ipynb")
        {
            ruff_python_ast::PySourceType::Ipynb
        } else {
            ruff_python_ast::PySourceType::Python
        }
    }

    pub fn add_source_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        self.additional_sources.extend(paths);
    }

    fn source_module_name(&self, path: &Path) -> Result<String> {
        if self.source_type(path) == ast::PySourceType::Ipynb
            || !path.starts_with(&self.root)
                && !self.source_roots.iter().any(|root| path.starts_with(root))
        {
            use sha2::{Digest, Sha256};
            return Ok(format!(
                "_ty_nonmodule_{:x}",
                Sha256::digest(path.to_string_lossy().as_bytes())
            ));
        }
        let root = self
            .source_roots
            .iter()
            .find(|root| path.starts_with(root))
            .unwrap_or(&self.root);
        module_name(root, path)
    }
    fn find_source_module(&self, name: &str) -> Option<PathBuf> {
        self.source_roots
            .iter()
            .find_map(|root| find_module(root, name))
    }

    pub fn add_resolved_module(&mut self, name: &str, path: &Path) -> Result<()> {
        if self.modules.contains_key(name) {
            return Ok(());
        }
        self.add_module(name, path, true)?;
        self.rebuild()
    }

    /// Resolve only superclass chains used by first-party declarations. Ordinary
    /// external functions/types remain the official checker's responsibility.
    pub fn ancestry_imports(&self) -> BTreeSet<String> {
        let mut visited = BTreeSet::new();
        let mut queue = self
            .classes
            .iter()
            .filter(|(_, summary)| {
                summary
                    .pointer("/source/module")
                    .and_then(Value::as_str)
                    .and_then(|name| self.modules.get(name))
                    .is_some_and(|module| !module.external)
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        let mut requests = BTreeSet::new();
        while let Some(name) = queue.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let Some(class) = self.classes.get(&name) else {
                continue;
            };
            let module = class
                .pointer("/source/module")
                .and_then(Value::as_str)
                .unwrap_or("");
            for base in class["bases"].as_array().into_iter().flatten() {
                let Some(expression) = base["expression"].as_str() else {
                    continue;
                };
                let canonical = self.canonical_name(module, expression);
                let nominal = canonical.split('[').next().unwrap_or(&canonical);
                if self.classes.contains_key(nominal) {
                    queue.push(nominal.into());
                    continue;
                }
                if let Some((base_module, _)) = nominal.rsplit_once('.')
                    && !self.modules.contains_key(base_module)
                {
                    requests.insert(base_module.to_owned());
                }
            }
        }
        requests
    }

    /// Conservative dependency edges for incremental projection. Include imports
    /// in every scope/branch, star imports, package initializers, and submodules.
    pub fn import_dependencies(&self) -> BTreeMap<String, BTreeSet<String>> {
        use ruff_python_ast::visitor::{self, Visitor};
        struct Imports<'a> {
            module: &'a str,
            package: bool,
            targets: BTreeSet<String>,
        }
        impl<'a> Visitor<'a> for Imports<'a> {
            fn visit_stmt(&mut self, statement: &'a Stmt) {
                match statement {
                    Stmt::Import(import) => self
                        .targets
                        .extend(import.names.iter().map(|alias| alias.name.to_string())),
                    Stmt::ImportFrom(import) => {
                        let module = relative_import(
                            self.module,
                            self.package,
                            import.level,
                            import.module.as_ref().map(|name| name.as_str()),
                        );
                        self.targets.insert(module.clone());
                        self.targets.extend(
                            import
                                .names
                                .iter()
                                .filter(|alias| alias.name.as_str() != "*")
                                .map(|alias| format!("{module}.{}", alias.name)),
                        );
                    }
                    _ => {}
                }
                visitor::walk_stmt(self, statement);
            }
        }
        self.modules
            .iter()
            .map(|(name, module)| {
                let mut imports = Imports {
                    module: name,
                    package: Path::new(&module.source.path)
                        .file_stem()
                        .is_some_and(|name| name == "__init__"),
                    targets: BTreeSet::new(),
                };
                imports.visit_body(&module.suite);
                let mut dependencies = BTreeSet::new();
                for target in imports.targets {
                    let mut prefix = target.as_str();
                    loop {
                        if self.modules.contains_key(prefix) {
                            dependencies.insert(prefix.to_owned());
                        }
                        let Some((parent, _)) = prefix.rsplit_once('.') else {
                            break;
                        };
                        prefix = parent;
                    }
                }
                (name.clone(), dependencies)
            })
            .collect()
    }

    pub fn retain_external_roots(&mut self, roots: &[PathBuf]) -> Result<()> {
        self.modules.retain(|_, module| {
            !module.external
                || roots
                    .iter()
                    .any(|root| Path::new(&module.source.path).starts_with(root))
        });
        self.rebuild()
    }

    /// Add a stub/source overlay's import graph. External classes participate in
    /// name and ancestry resolution but are not sent as user model declarations.
    pub fn add_source_tree_filtered(
        &mut self,
        root: &Path,
        include: impl Fn(&Path) -> bool,
    ) -> Result<()> {
        let mut paths = vec![];
        collect_python(root, &mut paths)?;
        paths.retain(|path| include(path.strip_prefix(root).unwrap()));
        let current: BTreeSet<_> = paths.iter().cloned().collect();
        self.modules.retain(|_, module| {
            !module.external
                || !Path::new(&module.source.path).starts_with(root)
                || current.contains(Path::new(&module.source.path))
        });
        paths.sort_by_key(|p| p.extension().is_some_and(|x| x == "pyi"));
        for path in paths {
            let module = module_name(root, &path)?;
            if self.modules.get(&module).is_some_and(|m| !m.external) {
                continue;
            }
            self.add_module(&module, &path, true)?;
        }
        self.rebuild()
    }

    fn add_module(&mut self, name: &str, path: &Path, external: bool) -> Result<()> {
        let text = read_source(path)?;
        if self.modules.get(name).is_some_and(|m| {
            m.external == external && m.source.text == text && Path::new(&m.source.path) == path
        }) {
            return Ok(());
        }
        self.add_module_text(name, path, external, text)
    }

    fn add_module_text(
        &mut self,
        name: &str,
        path: &Path,
        external: bool,
        text: String,
    ) -> Result<()> {
        let source_type = self.source_type(path);
        let parsed = if source_type == ast::PySourceType::Ipynb
            && let Ok(notebook) = ruff_notebook::Notebook::from_path(path)
            && notebook.source_code() == text
        {
            ruff_python_parser::parse_cells_unchecked(
                &text,
                notebook.cell_offsets().content_ranges(),
                &source_type.into(),
            )
        } else {
            ruff_python_parser::parse_unchecked_source(&text, source_type)
        };
        let suite = match parsed.errors().first() {
            None => {
                self.syntax_errors.remove(name);
                parsed.syntax().body.to_vec()
            }
            Some(_) if self.allow_invalid && !external => {
                self.syntax_errors.insert(name.to_owned());
                let first_error = parsed
                    .errors()
                    .first()
                    .map(|error| usize::from(error.location.start()))
                    .unwrap_or(text.len());
                // A malformed suffix remains byte-for-byte in Source. Safe earlier
                // declarations still receive plugin fields for editor completion.
                parsed
                    .syntax()
                    .body
                    .iter()
                    .filter(|statement| span(*statement).end <= first_error)
                    .cloned()
                    .collect()
            }
            Some(error) => bail!("parse {}: {error}", path.display()),
        };
        let mut source = Source::new(text, path.to_string_lossy().into_owned(), name.to_owned());
        if self.source_type(path) == ast::PySourceType::Ipynb
            || (!external && !self.source_roots.iter().any(|root| path.starts_with(root)))
        {
            source.protocol_module = Some(String::new());
        }
        let source = Arc::new(source);
        let is_package = path.file_stem().is_some_and(|x| x == "__init__");
        let mut module = Module {
            source,
            suite: Arc::new(suite),
            imports: BTreeMap::new(),
            bindings: BTreeSet::new(),
            assignments: BTreeMap::new(),
            external,
        };
        collect_bindings(
            &module.suite.clone(),
            name,
            is_package,
            &mut module.imports,
            &mut module.bindings,
            &mut module.assignments,
        );
        self.modules.insert(name.to_owned(), module);
        Ok(())
    }

    /// Refresh project files without reparsing unchanged modules or external stubs.
    /// Overlay keys are absolute original paths; missing disk files may be new editor documents.
    /// Passing no overlay restores the on-disk contents after didClose.
    #[cfg(test)]
    pub fn refresh_sources(&mut self, overlays: &BTreeMap<PathBuf, String>) -> Result<bool> {
        self.refresh_sources_filtered(overlays, |_, _| true)
    }

    pub fn refresh_sources_filtered(
        &mut self,
        overlays: &BTreeMap<PathBuf, String>,
        include: impl Fn(&Path, &str) -> bool,
    ) -> Result<bool> {
        // Arc-backed syntax snapshots are cheap to retain. A read/parse failure
        // must not commit half a revision and make a later retry appear fresh.
        let modules = self.modules.clone();
        let syntax_errors = self.syntax_errors.clone();
        let result = self.refresh_sources_inner(overlays, include);
        if result.is_err() {
            self.modules = modules;
            self.syntax_errors = syntax_errors;
            self.rebuild()
                .context("restore project after failed refresh")?;
        }
        result
    }

    fn refresh_sources_inner(
        &mut self,
        overlays: &BTreeMap<PathBuf, String>,
        include: impl Fn(&Path, &str) -> bool,
    ) -> Result<bool> {
        let mut paths = Vec::new();
        collect_python(&self.root, &mut paths)?;
        paths.extend(overlays.keys().cloned());
        for path in &self.additional_sources {
            if path.is_dir() {
                collect_python(path, &mut paths)?;
            } else if path.exists() {
                paths.push(path.clone());
            }
        }
        paths.sort();
        paths.dedup();
        let previous_modules = self.modules.keys().cloned().collect::<BTreeSet<_>>();
        let mut current = BTreeSet::new();
        let mut changed = false;
        let mut summaries_changed = BTreeSet::new();
        for path in paths {
            let name = self.source_module_name(&path)?;
            let text = match overlays.get(&path) {
                Some(text) => text.clone(),
                None => read_source(&path)?,
            };
            if !include(&path, &text) && !overlays.contains_key(&path) {
                continue;
            }
            current.insert(name.clone());
            if self
                .modules
                .get(&name)
                .is_some_and(|m| m.source.text == text && !m.external)
            {
                continue;
            }
            let previous = self.modules.get(&name).cloned();
            self.add_module_text(&name, &path, false, text)?;
            let current = &self.modules[&name];
            let same_summary = previous.as_ref().is_some_and(|old| {
                if old.suite != current.suite || self.syntax_errors.contains(&name) {
                    return false;
                }
                let end = current
                    .suite
                    .last()
                    .map_or(0, |statement| span(statement).end);
                old.source.text.get(..end) == current.source.text.get(..end)
            });
            if !same_summary {
                summaries_changed.insert(name.clone());
            }
            changed = true;
        }
        self.modules.retain(|name, module| {
            let keep = module.external || current.contains(name);
            changed |= !keep;
            if !keep {
                summaries_changed.insert(name.clone());
            }
            keep
        });
        self.syntax_errors.retain(|name| current.contains(name));
        if !summaries_changed.is_empty() {
            self.load_imports()?;
            if previous_modules.iter().eq(self.modules.keys()) {
                let dependencies = self.import_dependencies();
                loop {
                    let affected = dependencies
                        .iter()
                        .filter(|(name, imports)| {
                            !summaries_changed.contains(*name)
                                && imports.iter().any(|name| summaries_changed.contains(name))
                        })
                        .map(|(name, _)| name.clone())
                        .collect::<Vec<_>>();
                    if affected.is_empty() {
                        break;
                    }
                    summaries_changed.extend(affected);
                }
                self.rebuild_selected(&summaries_changed)?;
            } else {
                self.rebuild()?;
            }
        }
        Ok(changed)
    }

    fn load_imports(&mut self) -> Result<()> {
        loop {
            let mut additions = BTreeMap::new();
            for module in self.modules.values() {
                for target in module.imports.values() {
                    let mut candidate = target.as_str();
                    loop {
                        if !self.modules.contains_key(candidate)
                            && let Some(path) = self.find_source_module(candidate)
                        {
                            additions.insert(candidate.to_owned(), path);
                        }
                        let Some((parent, _)) = candidate.rsplit_once('.') else {
                            break;
                        };
                        candidate = parent;
                    }
                }
            }
            if additions.is_empty() {
                break;
            }
            for (name, path) in additions {
                self.add_module(&name, &path, false)?;
            }
        }
        Ok(())
    }

    fn rebuild(&mut self) -> Result<()> {
        self.classes.clear();
        self.assignments.clear();
        self.functions.clear();
        self.settings.clear();
        // First register every class name, then resolve forward references.
        for (name, module) in &self.modules {
            register_class_names(&module.suite, name, &module.imports, &mut self.classes);
        }
        let mut classes = BTreeMap::new();
        let mut assignments = vec![];
        let mut functions = vec![];
        for (name, module) in &self.modules {
            self.summarize_body(name, module, &module.suite, &mut classes, &mut assignments)?;
            functions.extend(self.summarize_functions(name, module));
        }
        self.classes = classes;
        self.assignments = assignments;
        self.functions = functions;
        self.rebuild_settings();
        Ok(())
    }

    fn rebuild_selected(&mut self, affected: &BTreeSet<String>) -> Result<()> {
        let paths = affected
            .iter()
            .filter_map(|name| self.modules.get(name))
            .map(|module| module.source.path.as_str())
            .collect::<BTreeSet<_>>();
        let retain = |value: &Value| {
            value
                .pointer("/source/file-path")
                .and_then(Value::as_str)
                .is_none_or(|path| !paths.contains(path))
        };
        self.classes.retain(|_, summary| retain(summary));
        self.assignments.retain(retain);
        self.functions.retain(|function| retain(&function.summary));
        for name in affected {
            if let Some(module) = self.modules.get(name) {
                register_class_names(&module.suite, name, &module.imports, &mut self.classes);
            }
        }
        let mut classes = BTreeMap::new();
        let mut assignments = Vec::new();
        let mut functions = Vec::new();
        for name in affected {
            if let Some(module) = self.modules.get(name) {
                self.summarize_body(name, module, &module.suite, &mut classes, &mut assignments)?;
                functions.extend(self.summarize_functions(name, module));
            }
        }
        self.classes.extend(classes);
        self.assignments.extend(assignments);
        self.functions.extend(functions);
        // Keep the exact full-rebuild request order, including explicit roots.
        let order = self
            .modules
            .values()
            .enumerate()
            .map(|(index, module)| (module.source.path.as_str(), index))
            .collect::<BTreeMap<_, _>>();
        let key = |value: &Value| {
            (
                order
                    .get(
                        value
                            .pointer("/source/file-path")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
                    .copied()
                    .unwrap_or(usize::MAX),
                value["source"]["start"]["line"].as_u64().unwrap_or(0),
                value["source"]["start"]["column"].as_u64().unwrap_or(0),
            )
        };
        self.assignments.sort_by_key(key);
        self.functions
            .sort_by_key(|function| key(&function.summary));
        self.rebuild_settings();
        Ok(())
    }

    fn rebuild_settings(&mut self) {
        self.settings.clear();
        for name in self.settings_modules() {
            if let Some(module) = self.modules.get(&name) {
                let mut values = vec![];
                let mut diagnostics = vec![];
                let mut dependencies = BTreeSet::from([module.source.path.clone()]);
                for statement in module.suite.iter() {
                    let pair = match statement {
                        Stmt::Assign(assignment) => assignment.targets.first().and_then(|target| {
                            if let Expr::Name(target) = target {
                                Some((target, assignment.value.as_ref()))
                            } else {
                                None
                            }
                        }),
                        Stmt::AnnAssign(assignment) => {
                            if let (Expr::Name(target), Some(value)) =
                                (assignment.target.as_ref(), assignment.value.as_deref())
                            {
                                Some((target, value))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    let Some((target, expression)) = pair else {
                        continue;
                    };
                    let key = target.id.as_str();
                    if !is_setting(key) {
                        continue;
                    }
                    let value = self.settings_value(
                        &name,
                        expression,
                        &mut BTreeSet::new(),
                        &mut dependencies,
                    );
                    let mut location = module.source.location(span(target));
                    location["qualified-name"] = json!(format!("{name}.{key}"));
                    if value["kind"] == "unknown" {
                        diagnostics.push(json!({"id":"ty.settings.unsupported-value","message":format!("Setting `{name}.{key}` is not a supported static literal"),"severity":self.settings_severity(),"location":{"file-path":module.source.path,"start":location["start"],"end":location["end"]}}));
                    } else {
                        values.push(json!({"name":key,"value":value,"source":location}));
                    }
                }
                self.settings.push(json!({"module":name,"values":values,"dependencies":dependencies.into_iter().map(|path|json!({"path":path})).collect::<Vec<_>>(),"diagnostics":diagnostics,"source":{"module":name,"file-path":module.source.path}}));
            } else {
                self.settings.push(json!({"module":name,"values":[],"dependencies":[],"diagnostics":[{"id":"ty.settings.module-not-found","message":format!("Settings module `{name}` could not be resolved"),"severity":self.settings_severity()}],"source":{"module":name}}));
            }
        }
    }

    fn settings_modules(&self) -> Vec<String> {
        let mut names = Vec::new();
        for claim in self.manifest["claims"]["settings"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(key) = claim["config-key"].as_str() {
                if let Some(name) = self.config[key].as_str() {
                    names.push(name.to_owned());
                }
            } else if let Some(name) = claim["module"].as_str().filter(|name| !name.is_empty()) {
                names.push(name.to_owned());
            }
        }
        names
    }

    fn settings_severity(&self) -> &'static str {
        if self.config["strict-settings"] == true || self.config["strict_settings"] == true {
            "error"
        } else {
            "warning"
        }
    }

    pub fn class_summary(&self, qualified: &str) -> Option<&Value> {
        self.classes.get(qualified)
    }

    /// Original declaration provenance, independent of plugin field summaries.
    /// In particular, SDK ClassSummary intentionally omits ClassVar members,
    /// while receiver recovery still needs their declared generic types.
    pub fn declared_member_type(&self, class: &str, member: &str) -> Option<Value> {
        let class = class.split('[').next()?.trim();
        let summary = self.classes.get(class)?;
        let module_name = summary["source"]["module"].as_str()?;
        let module = self.modules.get(module_name)?;
        let path: Vec<_> = class
            .strip_prefix(&format!("{module_name}."))?
            .split('.')
            .collect();
        fn find<'a>(
            suite: &'a [Stmt],
            path: &[&str],
            imports: &BTreeMap<String, String>,
        ) -> Option<&'a ast::StmtClassDef> {
            for statement in suite {
                match statement {
                    Stmt::ClassDef(class) if Some(class.name.as_str()) == path.first().copied() => {
                        if path.len() == 1 {
                            return Some(class);
                        }
                        if let Some(found) = find(&class.body, &path[1..], imports) {
                            return Some(found);
                        }
                    }
                    Stmt::If(branch) => {
                        for suite in branch_suites(branch, imports) {
                            if let Some(found) = find(suite, path, imports) {
                                return Some(found);
                            }
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        let class = find(&module.suite, &path, &module.imports)?;
        fn annotation<'a>(
            suite: &'a [Stmt],
            member: &str,
            imports: &BTreeMap<String, String>,
        ) -> Option<&'a Expr> {
            for statement in suite {
                match statement {
                    Stmt::AnnAssign(assignment) if matches!(assignment.target.as_ref(),Expr::Name(name) if name.id.as_str()==member) =>
                    {
                        return Some(&assignment.annotation);
                    }
                    Stmt::If(branch) => {
                        for suite in branch_suites(branch, imports) {
                            if let Some(found) = annotation(suite, member, imports) {
                                return Some(found);
                            }
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        annotation(&class.body, member, &module.imports)
            .map(|expression| self.annotation(module_name, module, expression))
    }

    /// Resolve imported aliases throughout a type expression while preserving
    /// literals and punctuation. Builtins stay unqualified, as in ty's protocol.
    pub fn canonical_name(&self, module: &str, expression: &str) -> String {
        rewrite_names(expression, |name| {
            self.resolve_symbol(module, name, &mut BTreeSet::new())
        })
    }

    fn resolve_symbol(&self, module: &str, name: &str, seen: &mut BTreeSet<String>) -> String {
        if builtin(name) || name == "Self" {
            return name.to_owned();
        }
        let key = format!("{module}:{name}");
        if !seen.insert(key) {
            return name.to_owned();
        }
        let (head, tail) = name.split_once('.').unwrap_or((name, ""));
        if let Some(qualified) = self.virtual_identities.get(head) {
            return if tail.is_empty() {
                qualified.clone()
            } else {
                format!("{qualified}.{tail}")
            };
        }
        if let Some((_, short)) = name.rsplit_once('.')
            && let Some(qualified) = self.virtual_identities.get(short)
        {
            return qualified.clone();
        }
        if let Some(info) = self.modules.get(module) {
            if let Some(target) = info.imports.get(head) {
                let resolved = if tail.is_empty() {
                    target.clone()
                } else {
                    format!("{target}.{tail}")
                };
                return self.resolve_qualified(&resolved, seen);
            }
            if info.bindings.contains(head) {
                if let Some(value) = info.assignments.get(head) {
                    fn symbol(expr: &Expr) -> Option<String> {
                        match expr {
                            Expr::Name(name) => Some(name.id.to_string()),
                            Expr::Attribute(attr) => {
                                Some(format!("{}.{}", symbol(&attr.value)?, attr.attr))
                            }
                            _ => None,
                        }
                    }
                    if let Some(target) = symbol(value) {
                        let resolved = self.resolve_symbol(module, &target, seen);
                        return if tail.is_empty() {
                            resolved
                        } else {
                            self.resolve_qualified(&format!("{resolved}.{tail}"), seen)
                        };
                    }
                }
                return format!("{module}.{name}");
            }
        }
        if self.classes.contains_key(name)
            || self
                .modules
                .keys()
                .any(|m| name.starts_with(&format!("{m}.")))
        {
            return self.resolve_qualified(name, seen);
        }
        name.to_owned()
    }

    fn resolve_qualified(&self, name: &str, seen: &mut BTreeSet<String>) -> String {
        // Expand re-exports using the longest known module prefix.
        let mut positions: Vec<_> = name.match_indices('.').map(|(i, _)| i).collect();
        positions.reverse();
        for at in positions {
            let (module, member) = (&name[..at], &name[at + 1..]);
            if self.modules.contains_key(module) {
                let resolved = self.resolve_symbol(module, member, seen);
                if resolved != member {
                    return resolved;
                }
            }
        }
        name.to_owned()
    }

    pub fn is_subclass(&self, class: &str, base: &str) -> bool {
        let class = class.split('[').next().unwrap_or(class).trim();
        let base = base.split('[').next().unwrap_or(base).trim();
        let mut todo = vec![class.to_owned()];
        let mut seen = BTreeSet::new();
        while let Some(current) = todo.pop() {
            if current == base {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            if let Some(summary) = self.classes.get(&current) {
                for parent in summary["bases"].as_array().into_iter().flatten() {
                    if let Some(expression) = parent["expression"].as_str() {
                        todo.push(
                            expression
                                .split('[')
                                .next()
                                .unwrap_or(expression)
                                .trim()
                                .to_owned(),
                        );
                    }
                }
            }
        }
        false
    }

    /// Python C3 linearization. Cyclic or inconsistent bases return no answer;
    /// callers must not assume a mutation hook's target in that case.
    pub fn mro(&self, class: &str) -> Option<Vec<String>> {
        fn resolve(
            project: &Project,
            class: &str,
            active: &mut BTreeSet<String>,
        ) -> Option<Vec<String>> {
            let class = class.split('[').next()?.trim();
            if !active.insert(class.to_owned()) {
                return None;
            }
            let bases: Vec<String> = project
                .classes
                .get(class)
                .into_iter()
                .flat_map(|s| s["bases"].as_array().into_iter().flatten())
                .filter_map(|base| base["expression"].as_str())
                .map(|base| base.split('[').next().unwrap_or(base).trim().to_owned())
                .collect();
            let mut sequences = vec![];
            for base in &bases {
                sequences.push(resolve(project, base, active)?);
            }
            sequences.push(bases);
            let mut result = vec![class.to_owned()];
            loop {
                sequences.retain(|sequence| !sequence.is_empty());
                if sequences.is_empty() {
                    break;
                }
                let candidate = sequences
                    .iter()
                    .map(|sequence| &sequence[0])
                    .find(|candidate| {
                        sequences
                            .iter()
                            .all(|sequence| !sequence[1..].contains(candidate))
                    })?
                    .clone();
                result.push(candidate.clone());
                for sequence in &mut sequences {
                    if sequence.first() == Some(&candidate) {
                        sequence.remove(0);
                    }
                }
            }
            active.remove(class);
            Some(result)
        }
        resolve(self, class, &mut BTreeSet::new())
    }

    pub fn method_owner(&self, class: &str, method: &str) -> Option<String> {
        self.mro(class)?.into_iter().find(|name| {
            self.classes.get(name).is_some_and(|summary| {
                summary["methods"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|candidate| candidate["name"] == method)
            })
        })
    }

    /// Constructor provenance is safe only when ordinary allocation and
    /// metaclass calling are known to apply. Unknown ancestors and class
    /// decorators deliberately decline rather than guessing a nominal result.
    pub fn default_constructor_result(&self, class: &str) -> Option<String> {
        let nominal = class.split('[').next()?.trim();
        self.classes.get(nominal)?;
        let mut metaclasses = BTreeSet::new();
        for owner in self.mro(nominal)? {
            if matches!(owner.as_str(), "object" | "builtins.object") {
                continue;
            }
            let summary = self.classes.get(&owner)?;
            if summary["decorators"]
                .as_array()
                .is_some_and(|values| !values.is_empty())
                || declares_member(summary, "__new__")
            {
                return None;
            }
            if let Some(meta) = summary["metaclass"]["expression"].as_str() {
                metaclasses.insert(meta);
            }
        }
        for metaclass in metaclasses {
            for owner in self.mro(metaclass)? {
                if matches!(
                    owner.as_str(),
                    "type" | "builtins.type" | "object" | "builtins.object"
                ) {
                    continue;
                }
                let summary = self.classes.get(&owner)?;
                if summary["decorators"]
                    .as_array()
                    .is_some_and(|values| !values.is_empty())
                    || declares_member(summary, "__call__")
                {
                    return None;
                }
            }
        }
        Some(class.to_owned())
    }

    pub fn index_function_queries(&self) -> Vec<(usize, Arc<Source>, usize)> {
        self.functions
            .iter()
            .enumerate()
            .filter_map(|(index, function)| {
                let name = function.summary["qualified-name"].as_str()?;
                let (module, _) = name.rsplit_once('.')?;
                let module = self
                    .modules
                    .get(module)
                    .filter(|module| module.source.protocol_module.is_none())?;
                Some((index, module.source.clone(), function.name_offset))
            })
            .collect()
    }

    pub fn set_index_function_types(&mut self, types: BTreeMap<usize, Value>) {
        for (index, ty) in types {
            if let Some(function) = self.functions.get_mut(index) {
                function.summary["inferred-type"] = ty;
            }
        }
    }

    pub fn class_identity_at(&self, path: &Path, line: u64, character: u64) -> Option<String> {
        fn find(
            suite: &[Stmt],
            parent: &str,
            source: &Source,
            line: u64,
            character: u64,
        ) -> Option<String> {
            for statement in suite {
                if let Stmt::ClassDef(class) = statement {
                    let name = format!("{parent}.{}", class.name);
                    let position = source.position(span(&class.name).start);
                    if position["line"].as_u64()? == line + 1
                        && position["column"].as_u64()? == character + 1
                    {
                        return Some(name);
                    }
                    if let Some(found) = find(&class.body, &name, source, line, character) {
                        return Some(found);
                    }
                } else if let Stmt::If(branch) = statement {
                    for body in std::iter::once(&branch.body)
                        .chain(branch.elif_else_clauses.iter().map(|clause| &clause.body))
                    {
                        if let Some(found) = find(body, parent, source, line, character) {
                            return Some(found);
                        }
                    }
                }
            }
            None
        }
        let (name, module) = self
            .modules
            .iter()
            .find(|(_, module)| Path::new(&module.source.path) == path)?;
        find(&module.suite, name, &module.source, line, character)
    }

    #[cfg(test)]
    pub fn build_request(&self) -> Value {
        self.build_request_for_files(None)
    }

    /// Native project-index summaries use should_check_file, while settings
    /// claims and the semantic module graph remain independent of that scope.
    pub fn build_request_for_files(&self, checked: Option<&BTreeSet<PathBuf>>) -> Value {
        let in_checked_file =
            |path: &str| checked.is_none_or(|files| files.contains(Path::new(path)));
        let classes: Vec<_> = self
            .classes
            .values()
            .filter(|c| {
                let module = c["source"]["module"].as_str().unwrap_or("");
                self.modules.get(module).is_some_and(|m| {
                    !m.external
                        && m.source.protocol_module.is_none()
                        && in_checked_file(&m.source.path)
                })
            })
            .cloned()
            .collect();
        let in_regular_module = |value: &&Value| {
            value
                .pointer("/source/file-path")
                .and_then(Value::as_str)
                .is_none_or(|path| {
                    in_checked_file(path)
                        && self
                            .modules
                            .values()
                            .find(|m| m.source.path == path)
                            .is_none_or(|m| m.source.protocol_module.is_none())
                })
        };
        let assignments = self
            .assignments
            .iter()
            .filter(in_regular_module)
            .collect::<Vec<_>>();
        let functions = self
            .functions
            .iter()
            .map(|function| &function.summary)
            .filter(in_regular_module)
            .collect::<Vec<_>>();
        json!({"kind":"build-project-index","context":{"root":"","python-version":"3.13","platform":std::env::consts::OS.replace("macos","darwin"),"config":self.config},
            "classes":classes,"settings":self.settings,"assignments":assignments,"functions":functions})
    }

    pub fn register_virtual_types(
        &mut self,
        definitions: &[Value],
    ) -> Result<BTreeMap<PathBuf, String>> {
        let mut prefix = String::from("_ty_ext_virtual_");
        while self.modules.values().any(|module| {
            module.source.text.contains(&prefix)
                || module
                    .source
                    .module
                    .split('.')
                    .any(|part| part.starts_with(&prefix))
        }) || definitions.iter().any(|definition| {
            definition["name"]
                .as_str()
                .is_some_and(|name| name.contains(&prefix))
        }) {
            prefix.push('_');
        }
        self.virtual_modules.clear();
        let mut names = BTreeMap::new();
        for definition in definitions {
            let name = definition["name"]
                .as_str()
                .context("virtual type without name")?;
            let module = name
                .rsplit_once('.')
                .map_or("_ty_ext_virtual", |(module, _)| module);
            use sha2::Digest as _;
            let digest = sha2::Sha256::digest(module.as_bytes());
            self.virtual_modules
                .insert(module.to_owned(), format!("{prefix}module_{digest:x}"));
            let internal = virtual_internal_name_with_prefix(name, &prefix);
            self.virtual_identities
                .insert(internal.clone(), name.to_owned());
            names.insert(name.to_owned(), internal);
            self.classes.insert(name.to_owned(),json!({"qualified-name":name,"bases":definition["shape"]["bases"].as_array().cloned().unwrap_or_default(),"virtual":true}));
        }
        let mut rendered_definitions = Value::Array(definitions.to_vec());
        crate::type_render::replace_unavailable_types(
            &mut rendered_definitions,
            &self.unavailable_modules,
        )?;
        for definition in rendered_definitions.as_array().unwrap() {
            let name = definition["name"].as_str().unwrap();
            if let Some(summary) = self.classes.get_mut(name) {
                summary["bases"] = json!(
                    definition["shape"]["bases"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                );
            }
        }
        render_virtual_types_with_names(
            rendered_definitions.as_array().unwrap(),
            &names,
            Some(self),
        )
    }

    /// Restore identities in oracle type expressions before plugin dispatch.
    /// Quoted Literal values are intentionally preserved.
    pub fn rewrite_virtual_identities(&self, text: &str) -> String {
        rewrite_names(text, |name| {
            self.resolve_symbol("", name, &mut BTreeSet::new())
        })
    }

    fn summarize_body(
        &self,
        name: &str,
        module: &Module,
        suite: &[Stmt],
        classes: &mut BTreeMap<String, Value>,
        assignments: &mut Vec<Value>,
    ) -> Result<()> {
        for stmt in suite {
            match stmt {
                Stmt::ClassDef(class) => {
                    self.summarize_class(name, module, class, classes)?;
                }
                Stmt::Assign(assignment) if !module.external => {
                    for target in &assignment.targets {
                        if let Expr::Name(target) = target {
                            assignments.push(json!({"name":target.id.as_str(),"qualified-name":format!("{name}.{}",target.id),"assigned-value":self.assigned(name,module,&assignment.value),"source":module.source.location(span(target))}));
                        }
                    }
                }
                Stmt::AnnAssign(assignment) if !module.external => {
                    if let (Expr::Name(target), Some(value)) =
                        (assignment.target.as_ref(), assignment.value.as_ref())
                    {
                        assignments.push(json!({"name":target.id.as_str(),"qualified-name":format!("{name}.{}",target.id),"assigned-value":self.assigned(name,module,value),"inferred-type":self.annotation(name,module,&assignment.annotation),"source":module.source.location(span(target))}));
                    }
                }
                Stmt::If(branch) => {
                    for body in branch_suites(branch, &module.imports) {
                        self.summarize_body(name, module, body, classes, assignments)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn summarize_functions(&self, name: &str, module: &Module) -> Vec<IndexFunction> {
        if module.external {
            return Vec::new();
        }
        let mut definitions = Vec::new();
        collect_module_functions(&module.suite, &mut definitions);
        definitions.into_iter().map(|function| {
            let qualified = format!("{name}.{}",function.name);
            let mut source = module.source.location(span(function));
            source["qualified-name"] = json!(qualified);
            IndexFunction {
                summary: json!({"qualified-name":qualified,"decorators":function.decorator_list.iter().map(|d|self.function_decorator(name,module,&d.expression)).collect::<Vec<_>>(),"source":source}),
                name_offset: span(&function.name).start,
            }
        }).collect()
    }

    fn summarize_class(
        &self,
        parent: &str,
        module: &Module,
        class: &ast::StmtClassDef,
        classes: &mut BTreeMap<String, Value>,
    ) -> Result<()> {
        let name = format!("{parent}.{}", class.name);
        let module_name = &module.source.module;
        let mut fields = vec![];
        let mut methods = vec![];
        let mut constants = vec![];
        let mut nested = vec![];
        for statement in &class.body {
            match statement {
                Stmt::Assign(assignment) => {
                    for target in &assignment.targets {
                        if let Expr::Name(target) = target {
                            fields.push(json!({"name":target.id.as_str(),"assigned-value":self.assigned(module_name,module,&assignment.value),"has-default":true,"source":module.source.location(span(target))}));
                            let value = syntactic_literal(&assignment.value);
                            if value["kind"] != "unknown" {
                                constants.push(json!({"name":target.id.as_str(),"value":value,"source":module.source.location(span(target))}));
                            }
                        }
                    }
                }
                Stmt::AnnAssign(assignment) => {
                    if let Expr::Name(target) = assignment.target.as_ref() {
                        let mut field = json!({"name":target.id.as_str(),"annotation":self.annotation(module_name,module,&assignment.annotation),"has-default":assignment.value.is_some(),"source":module.source.location(span(target))});
                        if let Some(value) = &assignment.value {
                            field["assigned-value"] = self.assigned(module_name, module, value);
                            let value = syntactic_literal(value);
                            if value["kind"] != "unknown" {
                                constants.push(json!({"name":target.id.as_str(),"value":value,"source":module.source.location(span(target))}));
                            }
                        }
                        if !field["annotation"]["expression"]
                            .as_str()
                            .is_some_and(|ty| {
                                ty.starts_with("typing.ClassVar[")
                                    || ty.starts_with("dataclasses.InitVar[")
                            })
                        {
                            fields.push(field);
                        }
                    }
                }
                Stmt::FunctionDef(function) => {
                    let mut method = json!({"name":function.name.as_str(),"decorators":function.decorator_list.iter().map(|d|self.decorator(module_name,module,&d.expression)).collect::<Vec<_>>(),"parameters":self.parameters(module_name,module,&function.parameters),"is-public":!function.name.starts_with('_'),"source":module.source.location(span(function))});
                    method["source"]["qualified-name"] = json!(format!("{name}.{}", function.name));
                    if let Some(ret) = &function.returns {
                        let annotation = self.annotation(module_name, module, ret);
                        if annotation["expression"].as_str().is_some_and(|expression| {
                            expression == "Self" || expression.ends_with(".Self")
                        }) {
                            method["return-type"] = json!({"expression":"Self","mode":"annotation","snapshot":{"kind":"self-type","bound":{"kind":"expression","expression":name,"mode":"annotation"}}});
                        } else {
                            method["return-type"] = annotation;
                        }
                    }
                    methods.push(method);
                }
                Stmt::ClassDef(child) => {
                    self.summarize_class(&name, module, child, classes)?;
                    let child_name = format!("{name}.{}", child.name);
                    let summary = &classes[&child_name];
                    nested.push(json!({"name":child.name.as_str(),"qualified-name":child_name,"bases":summary["bases"],"class-constants":summary["class-constants"],"source":summary["source"]}));
                }
                _ => {}
            }
        }
        let bases: Vec<_> = class
            .arguments
            .as_ref()
            .into_iter()
            .flat_map(|a| a.args.iter())
            .map(|base| self.annotation(module_name, module, base))
            .collect();
        let mut summary = json!({"qualified-name":name,"bases":bases,"decorators":class.decorator_list.iter().map(|d|self.decorator(module_name,module,&d.expression)).collect::<Vec<_>>(),"fields":fields,"methods":methods,"nested-classes":nested,"class-constants":constants,"source":module.source.location(span(class))});
        summary["source"]["qualified-name"] = json!(name);
        if let Some(meta) = class.arguments.as_ref().and_then(|a| {
            a.keywords
                .iter()
                .find(|kw| kw.arg.as_ref().is_some_and(|s| s == "metaclass"))
        }) {
            summary["metaclass"] = self.annotation(module_name, module, &meta.value);
        }
        classes.insert(name, summary);
        Ok(())
    }

    pub fn annotation(&self, name: &str, module: &Module, expr: &Expr) -> Value {
        let raw = if let Expr::StringLiteral(s) = expr {
            s.value.to_str()
        } else {
            module.source.slice(span(expr))
        };
        typ(self.canonical_name(name, raw))
    }

    pub fn parameters(
        &self,
        name: &str,
        module: &Module,
        parameters: &ast::Parameters,
    ) -> Vec<Value> {
        let mut result = vec![];
        for (kind, items) in [
            ("positional-only", &parameters.posonlyargs),
            ("positional-or-keyword", &parameters.args),
        ] {
            for parameter in items {
                result.push(self.parameter(
                    name,
                    module,
                    &parameter.parameter,
                    kind,
                    parameter.default.is_none(),
                ));
            }
        }
        if let Some(parameter) = &parameters.vararg {
            result.push(self.parameter(name, module, parameter, "var-args", false));
        }
        for parameter in &parameters.kwonlyargs {
            result.push(self.parameter(
                name,
                module,
                &parameter.parameter,
                "keyword-only",
                parameter.default.is_none(),
            ));
        }
        if let Some(parameter) = &parameters.kwarg {
            result.push(self.parameter(name, module, parameter, "kwargs", false));
        }
        result
    }

    fn parameter(
        &self,
        name: &str,
        module: &Module,
        parameter: &ast::Parameter,
        kind: &str,
        required: bool,
    ) -> Value {
        let mut value = json!({"name":parameter.name.as_str(),"kind":kind,"required":required});
        if let Some(annotation) = &parameter.annotation {
            value["type-expr"] = self.annotation(name, module, annotation);
        }
        value
    }

    fn decorator(&self, name: &str, module: &Module, expr: &Expr) -> Value {
        if let Expr::Call(call) = expr {
            self.call(name, module, call)
        } else if let Some(symbol) = syntactic_symbol(expr) {
            json!({"kind":"symbol","qualified-name":symbol})
        } else {
            json!({"kind":"other"})
        }
    }

    fn function_decorator(&self, name: &str, module: &Module, expr: &Expr) -> Value {
        self.decorator(name, module, expr)
    }

    pub fn assigned(&self, name: &str, module: &Module, expr: &Expr) -> Value {
        match expr {
            Expr::Call(call) => self.call(name, module, call),
            _ => {
                let value = syntactic_literal(expr);
                if value["kind"] == "unknown" {
                    json!({"kind":"other"})
                } else {
                    json!({"kind":"literal","value":value})
                }
            }
        }
    }

    fn call(&self, name: &str, module: &Module, call: &ast::ExprCall) -> Value {
        let callee = syntactic_symbol(&call.func)
            .unwrap_or_else(|| module.source.slice(span(call.func.as_ref())).to_owned());
        let mut arguments = vec![];
        for expression in &call.arguments.args {
            let (kind, expression) = if let Expr::Starred(star) = expression {
                ("star-args", star.value.as_ref())
            } else {
                ("positional", expression)
            };
            arguments.push(self.argument(name, module, expression, kind, None));
        }
        for keyword in &call.arguments.keywords {
            arguments.push(self.argument(
                name,
                module,
                &keyword.value,
                if keyword.arg.is_some() {
                    "keyword"
                } else {
                    "star-kwargs"
                },
                keyword.arg.as_ref().map(|n| n.as_str()),
            ));
        }
        let mut result =
            json!({"kind":"call","callee":{"qualified-name":callee},"arguments":arguments});
        if let Expr::Attribute(attribute) = call.func.as_ref()
            && let Some(receiver) = syntactic_symbol(&attribute.value)
        {
            result["receiver"] = json!({"symbol":{"qualified-name":receiver}});
        }
        result
    }

    fn argument(
        &self,
        name: &str,
        module: &Module,
        expr: &Expr,
        kind: &str,
        key: Option<&str>,
    ) -> Value {
        // The SDK distinguishes lexical values from separately inferred types.
        // Resolving constants here loses enum-ref/name identity (for example,
        // `models.CASCADE` must remain an enum-ref even when its value is a str).

        let mut argument = json!({"kind":kind,"value":syntactic_literal(expr),"source":module.source.location(span(expr))});
        if let Some(key) = key {
            argument["name"] = json!(key);
        }
        if let Some(symbol) = self.symbol(name, expr) {
            // Class arguments are intentionally represented as class names,
            // matching the existing protocol's ClassRef contract.
            if self.classes.contains_key(&symbol) {
                argument["type-expr"] = typ(symbol);
            }
        }
        argument
    }

    fn symbol(&self, module: &str, expr: &Expr) -> Option<String> {
        fn raw(expr: &Expr) -> Option<String> {
            match expr {
                Expr::Name(n) => Some(n.id.to_string()),
                Expr::Attribute(a) => Some(format!("{}.{}", raw(&a.value)?, a.attr)),
                _ => None,
            }
        }
        raw(expr).map(|name| self.canonical_name(module, &name))
    }

    fn setting(&self, module: &str, name: &str, seen: &mut BTreeSet<String>) -> Value {
        let key = format!("{module}.{name}");
        if !seen.insert(key.clone()) {
            return json!({"kind":"unknown"});
        }
        let result = if let Some(info) = self.modules.get(module) {
            if let Some(value) = info.assignments.get(name) {
                self.value(module, value, seen)
            } else if let Some(target) = info.imports.get(name) {
                if let Some((owner, member)) = target.rsplit_once('.') {
                    self.setting(owner, member, seen)
                } else {
                    json!({"kind":"unknown"})
                }
            } else {
                json!({"kind":"unknown"})
            }
        } else {
            json!({"kind":"unknown"})
        };
        seen.remove(&key);
        result
    }

    fn settings_value(
        &self,
        module: &str,
        expression: &Expr,
        seen: &mut BTreeSet<String>,
        dependencies: &mut BTreeSet<String>,
    ) -> Value {
        match expression {
            Expr::Name(name) => {
                let symbol = self
                    .modules
                    .get(module)
                    .and_then(|info| info.imports.get(name.id.as_str()))
                    .cloned()
                    .unwrap_or_else(|| format!("{module}.{}", name.id));
                let Some((owner, key)) = symbol.rsplit_once('.') else {
                    return json!({"kind":"unknown"});
                };
                if !seen.insert(symbol.clone()) {
                    return json!({"kind":"unknown"});
                }
                let result = if let Some(info) = self.modules.get(owner) {
                    dependencies.insert(info.source.path.clone());
                    if is_setting(key) {
                        info.assignments.get(key).map_or_else(
                            || json!({"kind":"unknown"}),
                            |value| self.settings_value(owner, value, seen, dependencies),
                        )
                    } else {
                        json!({"kind":"unknown"})
                    }
                } else {
                    json!({"kind":"unknown"})
                };
                seen.remove(&symbol);
                result
            }
            Expr::Attribute(_) => {
                let symbol = self.symbol(module, expression).unwrap_or_default();
                if let Some((owner, key)) = symbol.rsplit_once('.')
                    && let Some(info) = self.modules.get(owner)
                    && is_setting(key)
                    && seen.insert(symbol.clone())
                {
                    dependencies.insert(info.source.path.clone());
                    let result = info.assignments.get(key).map_or_else(
                        || json!({"kind":"unknown"}),
                        |value| self.settings_value(owner, value, seen, dependencies),
                    );
                    seen.remove(&symbol);
                    return result;
                }
                json!({"kind":"unknown"})
            }
            Expr::Tuple(values) => {
                json!({"kind":"tuple","items":values.elts.iter().map(|value|self.settings_value(module,value,seen,dependencies)).collect::<Vec<_>>()})
            }
            Expr::List(values) => {
                json!({"kind":"list","items":values.elts.iter().map(|value|self.settings_value(module,value,seen,dependencies)).collect::<Vec<_>>()})
            }
            Expr::Dict(values) => {
                json!({"kind":"dict","entries":values.items.iter().filter_map(|item|item.key.as_ref().map(|key|json!({"key":self.settings_value(module,key,seen,dependencies),"value":self.settings_value(module,&item.value,seen,dependencies)}))).collect::<Vec<_>>()})
            }
            Expr::BinOp(binary) if binary.op == ast::Operator::Add => add_literals(
                self.settings_value(module, &binary.left, seen, dependencies),
                self.settings_value(module, &binary.right, seen, dependencies),
            ),
            Expr::UnaryOp(_) => json!({"kind":"unknown"}),
            _ => self.value(module, expression, seen),
        }
    }

    fn value(&self, module: &str, expr: &Expr, seen: &mut BTreeSet<String>) -> Value {
        match expr {
            Expr::StringLiteral(value) => json!({"kind":"str","value":value.value.to_str()}),
            Expr::BooleanLiteral(value) => json!({"kind":"bool","value":value.value}),
            Expr::NoneLiteral(_) => json!({"kind":"none"}),
            Expr::NumberLiteral(value) => match &value.value {
                ast::Number::Int(i) => i.as_i64().map_or_else(
                    || json!({"kind":"unknown"}),
                    |n| json!({"kind":"int","value":n}),
                ),
                _ => json!({"kind":"unknown"}),
            },
            Expr::Tuple(values) => {
                json!({"kind":"tuple","items":values.elts.iter().map(|e|self.value(module,e,seen)).collect::<Vec<_>>()})
            }
            Expr::List(values) => {
                json!({"kind":"list","items":values.elts.iter().map(|e|self.value(module,e,seen)).collect::<Vec<_>>()})
            }
            Expr::Dict(values) => {
                json!({"kind":"dict","entries":values.items.iter().filter_map(|e|e.key.as_ref().map(|key|json!({"key":self.value(module,key,seen),"value":self.value(module,&e.value,seen)}))).collect::<Vec<_>>()})
            }
            Expr::Name(name) => {
                let value = self.setting(module, name.id.as_str(), seen);
                if value["kind"] != "unknown" {
                    return value;
                }
                let symbol = self.canonical_name(module, name.id.as_str());
                json!({"kind":if self.classes.contains_key(&symbol){"class-ref"}else{"symbol-ref"},"qualified-name":symbol})
            }
            Expr::Attribute(_) => {
                let symbol = self.symbol(module, expr).unwrap_or_default();
                if let Some((owner, key)) = symbol.rsplit_once('.') {
                    let value = self.setting(owner, key, seen);
                    if value["kind"] != "unknown" {
                        return value;
                    }
                }
                json!({"kind":if self.classes.contains_key(&symbol){"class-ref"}else{"symbol-ref"},"qualified-name":symbol})
            }
            Expr::BinOp(binary) if binary.op == ast::Operator::Add => add_literals(
                self.value(module, &binary.left, seen),
                self.value(module, &binary.right, seen),
            ),
            Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::USub => {
                let value = self.value(module, &unary.operand, seen);
                value["value"]
                    .as_i64()
                    .and_then(i64::checked_neg)
                    .map_or_else(
                        || json!({"kind":"unknown"}),
                        |n| json!({"kind":"int","value":n}),
                    )
            }
            _ => json!({"kind":"unknown"}),
        }
    }
}

fn is_setting(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

fn declares_member(summary: &Value, name: &str) -> bool {
    ["methods", "fields"].iter().any(|kind| {
        summary[*kind]
            .as_array()
            .into_iter()
            .flatten()
            .any(|member| member["name"] == name)
    })
}

fn add_literals(left: Value, right: Value) -> Value {
    if left["kind"] == "str" && right["kind"] == "str" {
        json!({"kind":"str","value":format!("{}{}",left["value"].as_str().unwrap(),right["value"].as_str().unwrap())})
    } else if left["kind"] == right["kind"]
        && matches!(left["kind"].as_str(), Some("list" | "tuple"))
    {
        let mut items = left["items"].as_array().unwrap().clone();
        items.extend(right["items"].as_array().unwrap().iter().cloned());
        json!({"kind":left["kind"],"items":items})
    } else if left["kind"] == "int" && right["kind"] == "int" {
        left["value"]
            .as_i64()
            .and_then(|left| {
                right["value"]
                    .as_i64()
                    .and_then(|right| left.checked_add(right))
            })
            .map_or_else(
                || json!({"kind":"unknown"}),
                |value| json!({"kind":"int","value":value}),
            )
    } else {
        json!({"kind":"unknown"})
    }
}

/// Read the Python cells through the same notebook library used by ty.
/// Non-Python notebooks intentionally have no Python source.
pub fn read_source(path: &Path) -> Result<String> {
    if path.extension().is_some_and(|ext| ext == "ipynb") {
        let notebook = ruff_notebook::Notebook::from_path(path)
            .with_context(|| format!("read notebook {}", path.display()))?;
        Ok(if notebook.is_python_notebook() {
            notebook.source_code().to_owned()
        } else {
            String::new()
        })
    } else {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
    }
}

fn collect_python(root: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for item in std::fs::read_dir(root)? {
        let item = item?;
        let path = item.path();
        let name = item.file_name();
        let name = name.to_string_lossy();
        if item.file_type()?.is_dir() {
            if !name.starts_with('.')
                && !matches!(name.as_ref(), "__pycache__" | "target" | "node_modules")
            {
                collect_python(&path, files)?;
            }
        } else if path
            .extension()
            .is_some_and(|e| e == "py" || e == "pyi" || e == "ipynb")
        {
            files.push(path);
        }
    }
    Ok(())
}

pub fn module_name(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside project {}", path.display(), root.display()))?;
    let mut parts: Vec<String> = relative
        .with_extension("")
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if parts.last().is_some_and(|p| p == "__init__") {
        parts.pop();
    }
    ensure!(
        !parts.is_empty(),
        "project root cannot be its own package module"
    );
    Ok(parts.join("."))
}

fn find_module(root: &Path, name: &str) -> Option<PathBuf> {
    let base = root.join(name.replace('.', "/"));
    [
        base.with_extension("pyi"),
        base.with_extension("py"),
        base.join("__init__.pyi"),
        base.join("__init__.py"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

fn relative_import(module: &str, is_package: bool, level: u32, target: Option<&str>) -> String {
    if level == 0 {
        return target.unwrap_or("").to_owned();
    }
    let mut parts: Vec<_> = module.split('.').collect();
    if !is_package {
        parts.pop();
    }
    for _ in 1..level {
        parts.pop();
    }
    if let Some(target) = target {
        parts.extend(target.split('.'));
    }
    parts.join(".")
}

fn collect_bindings(
    suite: &[Stmt],
    module: &str,
    is_package: bool,
    imports: &mut BTreeMap<String, String>,
    bindings: &mut BTreeSet<String>,
    assignments: &mut BTreeMap<String, Expr>,
) {
    for stmt in suite {
        match stmt {
            Stmt::Import(import) => {
                for alias in &import.names {
                    let (bound, target) = if let Some(alias_name) = &alias.asname {
                        (alias_name.to_string(), alias.name.to_string())
                    } else {
                        let head = alias.name.split('.').next().unwrap().to_owned();
                        (head.clone(), head)
                    };
                    imports.insert(bound, target);
                }
            }
            Stmt::ImportFrom(import) => {
                let target = relative_import(
                    module,
                    is_package,
                    import.level,
                    import.module.as_ref().map(|n| n.as_str()),
                );
                for alias in &import.names {
                    if alias.name.as_str() == "*" {
                        continue;
                    }
                    imports.insert(
                        alias.asname.as_ref().unwrap_or(&alias.name).to_string(),
                        format!("{target}.{}", alias.name)
                            .trim_start_matches('.')
                            .to_owned(),
                    );
                }
            }
            Stmt::ClassDef(class) => {
                bindings.insert(class.name.to_string());
            }
            Stmt::FunctionDef(function) => {
                bindings.insert(function.name.to_string());
            }
            Stmt::Assign(assignment) => {
                for target in &assignment.targets {
                    if let Expr::Name(name) = target {
                        bindings.insert(name.id.to_string());
                        assignments.insert(name.id.to_string(), (*assignment.value).clone());
                    }
                }
            }
            Stmt::AnnAssign(assignment) => {
                if let Expr::Name(name) = assignment.target.as_ref() {
                    bindings.insert(name.id.to_string());
                    if let Some(value) = &assignment.value {
                        assignments.insert(name.id.to_string(), (**value).clone());
                    }
                }
            }
            Stmt::If(branch) => {
                for body in branch_suites(branch, imports) {
                    collect_bindings(body, module, is_package, imports, bindings, assignments);
                }
            }
            _ => {}
        }
    }
}

fn register_class_names(
    suite: &[Stmt],
    parent: &str,
    imports: &BTreeMap<String, String>,
    classes: &mut BTreeMap<String, Value>,
) {
    for statement in suite {
        match statement {
            Stmt::ClassDef(class) => {
                let name = format!("{parent}.{}", class.name);
                classes.insert(name.clone(), json!({}));
                register_class_names(&class.body, &name, imports, classes);
            }
            Stmt::If(branch) => {
                for body in branch_suites(branch, imports) {
                    register_class_names(body, parent, imports, classes);
                }
            }
            _ => {}
        }
    }
}

pub fn branch_suites<'a>(
    branch: &'a ast::StmtIf,
    imports: &BTreeMap<String, String>,
) -> Vec<&'a [Stmt]> {
    fn truth(expr: &Expr, imports: &BTreeMap<String, String>) -> Option<bool> {
        match expr {
            Expr::BooleanLiteral(value) => Some(value.value),
            Expr::Name(name) => matches!(
                imports.get(name.id.as_str()).map(String::as_str),
                Some("typing.TYPE_CHECKING" | "typing_extensions.TYPE_CHECKING")
            )
            .then_some(true),
            Expr::Attribute(attribute) if attribute.attr.as_str() == "TYPE_CHECKING" => {
                if let Expr::Name(owner) = attribute.value.as_ref() {
                    matches!(
                        imports.get(owner.id.as_str()).map(String::as_str),
                        Some("typing" | "typing_extensions")
                    )
                    .then_some(true)
                } else {
                    None
                }
            }
            Expr::UnaryOp(unary) if unary.op == ast::UnaryOp::Not => {
                truth(&unary.operand, imports).map(|value| !value)
            }
            _ => None,
        }
    }
    let mut suites = vec![];
    match truth(&branch.test, imports) {
        Some(true) => return vec![branch.body.as_slice()],
        Some(false) => {}
        None => suites.push(branch.body.as_slice()),
    }
    for clause in &branch.elif_else_clauses {
        match clause
            .test
            .as_ref()
            .and_then(|test| truth(test, imports))
            .or_else(|| clause.test.is_none().then_some(true))
        {
            Some(true) => {
                suites.push(clause.body.as_slice());
                break;
            }
            Some(false) => {}
            None => suites.push(clause.body.as_slice()),
        }
    }
    suites
}

fn builtin(name: &str) -> bool {
    matches!(
        name,
        "int"
            | "str"
            | "float"
            | "complex"
            | "bool"
            | "bytes"
            | "bytearray"
            | "memoryview"
            | "object"
            | "type"
            | "list"
            | "tuple"
            | "set"
            | "frozenset"
            | "dict"
            | "None"
            | "True"
            | "False"
            | "classmethod"
            | "staticmethod"
            | "property"
            | "Exception"
            | "BaseException"
            | "NotImplementedError"
            | "super"
            | "len"
    )
}

/// Token-level rewriting avoids accidentally editing quoted literals or a name
/// merely because its spelling is a substring of another identifier.
pub(crate) fn rewrite_names(text: &str, mut rewrite: impl FnMut(&str) -> String) -> String {
    let mut output = String::new();
    let mut at = 0;
    while at < text.len() {
        let c = text[at..].chars().next().unwrap();
        if c == '\'' || c == '"' {
            let start = at;
            at += c.len_utf8();
            let mut escaped = false;
            while at < text.len() {
                let next = text[at..].chars().next().unwrap();
                at += next.len_utf8();
                if next == c && !escaped {
                    break;
                }
                escaped = next == '\\' && !escaped;
            }
            output.push_str(&text[start..at]);
        } else if c == '_' || c.is_alphabetic() {
            let start = at;
            at += c.len_utf8();
            while at < text.len() {
                let next = text[at..].chars().next().unwrap();
                if next == '_' || next == '.' || next.is_alphanumeric() {
                    at += next.len_utf8();
                } else {
                    break;
                }
            }
            output.push_str(&rewrite(&text[start..at]));
        } else {
            output.push(c);
            at += c.len_utf8();
        }
    }
    output
}

pub fn substitute_self(expression: &str, concrete: &str) -> String {
    rewrite_names(expression, |name| {
        if matches!(name, "Self" | "typing.Self" | "typing_extensions.Self") {
            concrete.to_owned()
        } else {
            name.to_owned()
        }
    })
}

/// Preserve TypeExpr's lexical import bindings when the host needs to pass its
/// expression through a receiver summary or another expression-only channel.
pub fn type_expression(value: &Value) -> Option<String> {
    let expression = value["expression"].as_str()?;
    let bindings: BTreeMap<_, _> = value["imports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|binding| {
            let module = binding["module"].as_str()?;
            let name = binding["name"].as_str()?;
            let alias = binding["alias"].as_str().unwrap_or(name);
            Some((alias, format!("{module}.{name}")))
        })
        .collect();
    Some(rewrite_names(expression, |name| {
        let head = name.split('.').next().unwrap_or(name);
        bindings.get(head).map_or_else(
            || name.to_owned(),
            |qualified| format!("{qualified}{}", &name[head.len()..]),
        )
    }))
}

/// Render definitions into importable module stubs. Qualified plugin names are
/// module paths, never identifiers containing dots.
#[cfg(test)]
pub fn render_virtual_types(definitions: &[Value]) -> Result<BTreeMap<PathBuf, String>> {
    let names = definitions
        .iter()
        .filter_map(|definition| definition["name"].as_str())
        .map(|name| (name.to_owned(), virtual_internal_name(name)))
        .collect();
    render_virtual_types_with_names(definitions, &names, None)
}

fn render_virtual_types_with_names(
    definitions: &[Value],
    names: &BTreeMap<String, String>,
    project: Option<&Project>,
) -> Result<BTreeMap<PathBuf, String>> {
    let mut groups: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for definition in definitions {
        let qualified = definition["name"]
            .as_str()
            .context("virtual type without name")?;
        let (module, _) = qualified
            .rsplit_once('.')
            .unwrap_or(("_ty_ext_virtual", qualified));
        groups
            .entry(module.to_owned())
            .or_default()
            .push(definition);
    }
    let modules: Vec<_> = groups.keys().cloned().collect();
    let mut files = BTreeMap::new();
    for (module, definitions) in groups {
        let first_name = definitions[0]["name"].as_str().unwrap();
        let prefix = format!("{}_", names[first_name]);
        let mut types = project.map_or_else(
            || crate::type_render::Types::new(&prefix, &module),
            |project| crate::type_render::Types::with_project(&prefix, &module, project),
        );
        for module in &modules {
            types.add_module(module);
        }
        let mut body = String::new();
        for definition in definitions {
            let qualified = definition["name"].as_str().unwrap();
            let public_name = qualified.rsplit('.').next().unwrap();
            ensure!(
                valid_identifier(public_name),
                "virtual type has invalid identifier {public_name}"
            );
            let name = names
                .get(qualified)
                .context("missing generated virtual identity")?;
            let shape = &definition["shape"];
            match shape["kind"]
                .as_str()
                .context("virtual type has no shape")?
            {
                "class" => {
                    let bases: Vec<_> = shape["bases"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|v| types.render(v))
                        .collect::<Result<_>>()?;
                    body.push_str(&format!(
                        "class {name}({}):\n",
                        if bases.is_empty() {
                            format!("{}.object", types.builtins_alias())
                        } else {
                            bases.join(", ")
                        }
                    ));
                    let members = shape["members"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(&[]);
                    if members.is_empty() {
                        body.push_str("    pass\n");
                    }
                    for (index, member) in members.iter().enumerate() {
                        body.push_str(&render_virtual_member(
                            member,
                            &format!("{name}_descriptor_{index}"),
                            &mut types,
                        )?);
                    }
                }
                "typed-dict" | "named-tuple" => {
                    let kind = shape["kind"].as_str().unwrap();
                    let base = if kind == "typed-dict" {
                        format!("{}.TypedDict", types.typing_extensions_alias())
                    } else {
                        format!("{}.NamedTuple", types.typing_alias())
                    };
                    let total = if kind == "typed-dict" && shape["total"] == false {
                        ", total=False"
                    } else {
                        ""
                    };
                    let closed = if kind == "typed-dict" {
                        ", closed=True"
                    } else {
                        ""
                    };
                    let fields = shape["fields"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                    let mut rendered_fields = Vec::new();
                    for field in fields {
                        let field_name = field["name"]
                            .as_str()
                            .context("virtual field without name")?;
                        let mut annotation = types.render(&field["type-expr"])?;
                        if kind == "typed-dict" {
                            if field["read-only"] == true {
                                annotation = format!(
                                    "{}.ReadOnly[{annotation}]",
                                    types.typing_extensions_alias()
                                );
                            }
                            if field["required"] == false {
                                annotation = format!(
                                    "{}.NotRequired[{annotation}]",
                                    types.typing_extensions_alias()
                                );
                            }
                        }
                        rendered_fields.push((field_name, annotation));
                    }
                    if kind == "typed-dict"
                        && rendered_fields
                            .iter()
                            .any(|(field, _)| !valid_identifier(field))
                    {
                        let fields = rendered_fields
                            .iter()
                            .map(|(field, ty)| {
                                Ok(format!("{}: {ty}", serde_json::to_string(field)?))
                            })
                            .collect::<Result<Vec<_>>>()?
                            .join(", ");
                        body.push_str(&format!(
                            "{name} = {base}(\"{name}\", {{{fields}}}{total}{closed})\n"
                        ));
                    } else {
                        body.push_str(&format!("class {name}({base}{total}{closed}):\n"));
                        if rendered_fields.is_empty() {
                            body.push_str("    pass\n");
                        }
                        for (field, annotation) in rendered_fields {
                            ensure!(
                                valid_identifier(field),
                                "virtual field has invalid identifier {field}"
                            );
                            body.push_str(&format!("    {field}: {annotation}\n"));
                        }
                    }
                }
                kind => bail!("unsupported virtual type shape {kind}"),
            }
            body.push_str(&format!("{public_name} = {name}\n\n"));
        }
        let mut prelude = format!("from __future__ import annotations\n{}\n", types.prelude());
        prelude.push_str(&body);
        ruff_python_parser::parse_module(&prelude).context("invalid generated virtual stub")?;
        let physical = project
            .and_then(|project| project.virtual_modules.get(&module))
            .unwrap_or(&module);
        files.insert(
            PathBuf::from(physical.replace('.', "/")).with_extension("pyi"),
            prelude,
        );
    }
    Ok(files)
}

fn render_virtual_member(
    member: &Value,
    helper: &str,
    types: &mut crate::type_render::Types,
) -> Result<String> {
    let name = member["name"].as_str().context("member without name")?;
    ensure!(valid_identifier(name), "member has invalid name {name}");
    let access = &member["access"];
    match access["kind"].as_str().context("member without access")? {
        "value" => Ok(format!(
            "    {name}: {}\n",
            types.render(&access["type-expr"])?
        )),
        "callable" => {
            let signature = &access["signature"];
            let parameters = types.parameter_list(
                signature["parameters"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                true,
            )?;
            let return_type = types.render(&signature["return-type"])?;
            Ok(format!(
                "    def {name}({parameters}) -> {return_type}: ...\n"
            ))
        }
        "descriptor" => {
            let get = types.render(&access["instance-get-type"])?;
            if access["class-type"].is_object() {
                let class_type = types.render(&access["class-type"])?;
                let typing = types.typing_alias();
                let builtins = types.builtins_alias();
                // The fork's virtual-class builder contributes independent
                // class and instance members. Instance reads union both; writes
                // use the class member. It ignores setter/read-only metadata.
                // Preserve that behavior rather than strengthen its contract.
                let definition = format!(
                    "class {helper}:\n    @{typing}.overload\n    def __get__(self, obj: None, owner: {builtins}.type[{builtins}.object]) -> {class_type}: ...\n    @{typing}.overload\n    def __get__(self, obj: {builtins}.object, owner: {builtins}.type[{builtins}.object]) -> ({class_type}) | ({get}): ...\n    def __set__(self, obj: {builtins}.object, value: {class_type}) -> None: ...\n"
                );
                types.definitions.push_str(&definition);
                Ok(format!("    {name}: {helper}\n"))
            } else {
                // An instance annotation in a method, unlike a property or a
                // class-body annotation, is absent from the class namespace.
                // A private method avoids changing an inherited constructor.
                Ok(format!(
                    "    def {helper}_declare(self) -> None:\n        self.{name}: {get}\n"
                ))
            }
        }
        kind => bail!("unsupported member access {kind}"),
    }
}

/// Reversible encoding avoids hash collisions and is stable when unrelated
/// definitions are added. Only generated declarations use these names.
#[cfg(test)]
pub fn virtual_internal_name(qualified: &str) -> String {
    virtual_internal_name_with_prefix(qualified, "_ty_ext_virtual_")
}
fn virtual_internal_name_with_prefix(qualified: &str, prefix: &str) -> String {
    let mut name = prefix.to_owned();
    use std::fmt::Write as _;
    for byte in qualified.bytes() {
        write!(name, "{byte:02x}").expect("writing to a String cannot fail");
    }
    name
}

fn syntactic_literal(expr: &Expr) -> Value {
    match expr {
        Expr::BinOp(binary) if binary.op == ast::Operator::Add => add_literals(
            syntactic_literal(&binary.left),
            syntactic_literal(&binary.right),
        ),
        Expr::Tuple(values) => {
            json!({"kind":"tuple","items":values.elts.iter().map(syntactic_literal).collect::<Vec<_>>()})
        }
        Expr::List(values) => {
            json!({"kind":"list","items":values.elts.iter().map(syntactic_literal).collect::<Vec<_>>()})
        }
        Expr::Dict(values) => {
            json!({"kind":"dict","entries":values.items.iter().filter_map(|item|Some(json!({"key":syntactic_literal(item.key.as_ref()?),"value":syntactic_literal(&item.value)}))).collect::<Vec<_>>()})
        }
        _ => crate::model::literal(expr),
    }
}

fn syntactic_symbol(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attribute) => Some(format!(
            "{}.{}",
            syntactic_symbol(&attribute.value)?,
            attribute.attr
        )),
        _ => None,
    }
}

fn valid_identifier(name: &str) -> bool {
    crate::type_render::identifier(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new(files: &[(&str, &str)]) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "ty-companion-project-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            for (name, text) in files {
                let path = root.join(name);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, text).unwrap();
            }
            Self(root)
        }
        fn project(&self, manifest: Value, config: Value) -> Project {
            Project::load(&self.0, &[self.0.join("models.py")], &manifest, config).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn class_call_arguments_preserve_lexical_symbols_and_nested_literal_addition() {
        let fixture = Fixture::new(&[(
            "models.py",
            "TOKEN = 'resolved'\nclass Constants:\n    CASCADE = 'cascade'\nclass Book:\n    value = Field(TOKEN, mode=Constants.CASCADE, options=[TOKEN, {'x': Constants.CASCADE}, 'a' + 'b'])\n    copied = TOKEN\n    mode = Constants.CASCADE\n",
        )]);
        let project = fixture.project(json!({}), json!({}));
        let summary = project.class_summary("models.Book").unwrap();
        assert_eq!(
            summary["fields"][1]["assigned-value"],
            json!({"kind":"literal", "value":{"kind":"symbol-ref", "qualified-name":"TOKEN"}})
        );
        assert_eq!(
            summary["fields"][2]["assigned-value"],
            json!({"kind":"literal", "value":{"kind":"enum-ref", "qualified-name":"Constants.CASCADE"}})
        );
        assert_eq!(
            summary["class-constants"][0]["value"],
            json!({"kind":"symbol-ref", "qualified-name":"TOKEN"})
        );
        let args = &summary["fields"][0]["assigned-value"]["arguments"];
        assert_eq!(
            args[0]["value"],
            json!({"kind":"symbol-ref","qualified-name":"TOKEN"})
        );
        assert_eq!(
            args[1]["value"],
            json!({"kind":"enum-ref","qualified-name":"Constants.CASCADE"})
        );
        assert_eq!(
            args[2]["value"],
            json!({"kind":"list","items":[
                {"kind":"symbol-ref","qualified-name":"TOKEN"},
                {"kind":"dict","entries":[{"key":{"kind":"str","value":"x"},"value":{"kind":"enum-ref","qualified-name":"Constants.CASCADE"}}]},
                {"kind":"str","value":"ab"}
            ]})
        );
    }

    #[test]
    fn editor_overlays_reuse_unchanged_modules_and_restore_disk_text() -> Result<()> {
        let fixture = Fixture::new(&[("a.py", "class A: pass\n"), ("b.py", "value = 1\n")]);
        let root = fixture.0.canonicalize()?;
        let mut project = Project::load_editor(&root, &json!({}), json!({}))?;
        let unchanged = project.modules["a"].source.clone();
        let b = root.join("b.py");
        assert!(project.refresh_sources(&BTreeMap::from([(
            b.clone(),
            "value: str = 'new'\n".into()
        )]))?);
        assert!(Arc::ptr_eq(&unchanged, &project.modules["a"].source));
        assert_eq!(std::fs::read_to_string(&b)?, "value = 1\n");
        assert!(!project.refresh_sources(&BTreeMap::from([(
            b.clone(),
            "value: str = 'new'\n".into()
        )]))?);
        assert!(project.refresh_sources(&BTreeMap::from([(b.clone(), "value = (\n".into())]))?);
        assert!(project.syntax_errors.contains("b"));
        assert_eq!(project.modules["b"].source.text, "value = (\n");
        let new = root.join("new.py");
        assert!(
            project
                .refresh_sources(&BTreeMap::from([(new.clone(), "class New: pass\n".into())]))?
        );
        assert!(project.classes.contains_key("new.New"));
        assert!(!new.exists());
        assert!(!project.syntax_errors.contains("b"));
        assert_eq!(project.modules["b"].source.text, "value = 1\n");
        assert!(project.refresh_sources(&BTreeMap::new())?);
        assert!(!project.modules.contains_key("new"));
        Ok(())
    }

    #[test]
    fn incremental_summaries_equal_full_rebuild_with_aliases_and_settings() -> Result<()> {
        let fixture = Fixture::new(&[
            ("base.py", "VALUE = 1\nTYPE = int\nclass Base: pass\n"),
            (
                "bridge.py",
                "from base import VALUE, TYPE, Base as Parent\n",
            ),
            (
                "models.py",
                "from bridge import VALUE, TYPE, Parent\nclass Model(Parent):\n    field: TYPE\n    constant = VALUE\n",
            ),
            ("settings.py", "from bridge import VALUE\nDEFAULT = VALUE\n"),
        ]);
        let root = fixture.0.canonicalize()?;
        let manifest = json!({"claims":{"settings":[{"module":"settings"}]}});
        let mut project = Project::load_editor(&root, &manifest, json!({}))?;
        for source in [
            "VALUE = 2\nTYPE = str\nclass Base: pass\n",
            "VALUE = 3\nTYPE = bytes\nclass Alternative: pass\n",
            "VALUE = 1\nTYPE = int\nclass Base: pass\n",
        ] {
            project.refresh_sources(&BTreeMap::from([(root.join("base.py"), source.into())]))?;
            let incremental = project.build_request();
            let classes = project.classes.clone();
            let mut rebuilt = project.clone();
            rebuilt.rebuild()?;
            assert_eq!(incremental, rebuilt.build_request());
            assert_eq!(classes, rebuilt.classes);
        }
        Ok(())
    }

    #[test]
    fn project_imports_fields_decorators_and_nested_metadata_are_protocol_valid() {
        let fixture = Fixture::new(&[
            (
                "base.py",
                "class Entity: pass\nclass Field: pass\ndef receiver(*args, **kwargs): pass\n",
            ),
            (
                "models.py",
                "from base import Entity as Base, Field, receiver\nclass Parent(Base):\n    key = Field(default=7, nullable=True)\n    class Meta:\n        abstract = True\n    def save(self, *, force: bool = False) -> None: pass\nclass Child(Parent): pass\n@receiver(sender=Child)\ndef event(sender: type[Child]) -> None: pass\n",
            ),
        ]);
        let project = fixture.project(json!({}), json!({}));
        let summary = project.class_summary("models.Parent").unwrap();
        assert_eq!(summary["bases"][0]["expression"], "base.Entity");
        assert_eq!(
            summary["fields"][0]["assigned-value"]["callee"]["qualified-name"],
            "Field"
        );
        assert_eq!(
            summary["fields"][0]["assigned-value"]["arguments"][0]["value"],
            json!({"kind":"int","value":7})
        );
        assert_eq!(
            summary["nested-classes"][0]["class-constants"][0]["value"],
            json!({"kind":"bool","value":true})
        );
        assert_eq!(
            summary["methods"][0]["parameters"][1]["kind"],
            "keyword-only"
        );
        assert_eq!(
            project.method_owner("models.Child", "save").as_deref(),
            Some("models.Parent")
        );
        let request = project.build_request();
        let event = request["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["qualified-name"] == "models.event")
            .unwrap();
        assert_eq!(
            event["decorators"][0]["arguments"][0]["value"]["qualified-name"],
            "Child"
        );
        serde_json::from_value::<ty_plugin_protocol::PluginRequest>(request).unwrap();
    }

    #[test]
    fn checked_file_scope_filters_index_without_removing_resolution_or_settings() {
        let fixture = Fixture::new(&[
            (
                "models.py",
                "from closed import Closed\nclass Open(Closed): pass\nOPEN = 1\ndef opened(): pass\n",
            ),
            (
                "closed.py",
                "class Closed: pass\nCLOSED = 2\ndef closed(): pass\n",
            ),
            ("settings.py", "VALUE = 'available while closed'\n"),
        ]);
        let project = fixture.project(
            json!({"claims":{"settings":[{"module":"settings"}]}}),
            json!({}),
        );
        let open = BTreeSet::from([fixture.0.join("models.py").canonicalize().unwrap()]);
        let request = project.build_request_for_files(Some(&open));
        for key in ["classes", "assignments", "functions"] {
            let values = request[key].as_array().unwrap();
            assert!(!values.is_empty(), "{key}: {request:#}");
            assert!(
                values
                    .iter()
                    .all(|value| value["source"]["module"] == "models"),
                "{key}: {request:#}"
            );
        }
        assert_eq!(request["settings"][0]["values"][0]["name"], "VALUE");
        assert!(project.class_summary("closed.Closed").is_some());
        assert!(project.modules.contains_key("closed"));
        let closed = project.build_request_for_files(Some(&BTreeSet::new()));
        assert_eq!(closed["classes"], json!([]));
        assert_eq!(closed["assignments"], json!([]));
        assert_eq!(closed["functions"], json!([]));
        assert_eq!(closed["settings"], request["settings"]);
        assert!(
            project.build_request()["classes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|class| class["qualified-name"] == "closed.Closed")
        );
    }

    #[test]
    fn settings_imports_are_interpreted_without_executing_source() {
        let fixture = Fixture::new(&[
            ("models.py", "class Thing: pass\n"),
            ("base_settings.py", "PREFIX = 'user'\nITEMS = ['one']\n"),
            (
                "settings.py",
                "from base_settings import PREFIX, ITEMS\nAUTH_MODEL = PREFIX + '.Person'\nCHOICES = ITEMS + ['two']\nFLAG = True\nraise RuntimeError('must not execute')\n",
            ),
        ]);
        let project = fixture.project(
            json!({"claims":{"settings":[{"config-key":"settings-module"}]}}),
            json!({"settings-module":"settings"}),
        );
        let request = project.build_request();
        let values = request["settings"][0]["values"].as_array().unwrap();
        assert_eq!(
            values.iter().find(|v| v["name"] == "AUTH_MODEL").unwrap()["value"],
            json!({"kind":"str","value":"user.Person"})
        );
        assert_eq!(
            values.iter().find(|v| v["name"] == "CHOICES").unwrap()["value"],
            json!({"kind":"list","items":[{"kind":"str","value":"one"},{"kind":"str","value":"two"}]})
        );
    }

    #[test]
    fn c3_diamond_preserves_python_method_owner_order() {
        let fixture = Fixture::new(&[(
            "models.py",
            "class A:\n    def save(self): pass\nclass B(A): pass\nclass C(A):\n    def save(self): pass\nclass D(B, C): pass\n",
        )]);
        let project = fixture.project(json!({}), json!({}));
        assert_eq!(
            project.mro("models.D").unwrap(),
            vec!["models.D", "models.B", "models.C", "models.A"]
        );
        assert_eq!(
            project.method_owner("models.D", "save").as_deref(),
            Some("models.C")
        );
    }

    #[test]
    fn virtual_type_modules_preserve_qualified_identity_and_members() {
        let definitions = vec![
            json!({"name":"plugin.virtual.models.Thing.Manager","shape":{"kind":"class","bases":[typ("library.Manager[models.Thing]")],"members":[{"name":"active","access":{"kind":"callable","signature":{"parameters":[{"name":"limit","kind":"keyword-only","required":false,"type-expr":typ("int")}],"return-type":typ("models.Thing")},"fallback-type":typ("object")}}]}}),
            json!({"name":"plugin.virtual.models.Thing.Row","shape":{"kind":"typed-dict","fields":[{"name":"label","type-expr":typ("str"),"read-only":true,"required":false}]}}),
        ];
        let rendered = render_virtual_types(&definitions).unwrap();
        let text = &rendered[&PathBuf::from("plugin/virtual/models/Thing.pyi")];
        let imported = |module: &str| {
            text.lines()
                .find_map(|line| line.strip_prefix(&format!("import {module} as ")))
                .unwrap()
                .to_owned()
        };
        let models = imported("models");
        let library = imported("library");
        let typing = imported("typing");
        let typing_extensions = imported("typing_extensions");
        let builtins = imported("builtins");
        let internal = virtual_internal_name("plugin.virtual.models.Thing.Manager");
        assert!(text.contains(&format!(
            "class {internal}({library}.Manager[{models}.Thing]):"
        )));
        assert!(text.contains(&format!("Manager = {internal}")));
        assert!(text.contains(&format!("def active(self, *, limit: {builtins}.int = {typing}.cast({typing}.Any, ...)) -> {models}.Thing: ...")));
        assert!(text.contains(&format!(
            "label: {typing_extensions}.NotRequired[{typing_extensions}.ReadOnly[{builtins}.str]]"
        )));
    }

    #[test]
    fn unique_virtual_names_restore_protocol_identity_without_literal_rewriting() {
        let fixture = Fixture::new(&[("models.py", "class Thing: pass\n")]);
        let mut project = fixture.project(json!({}), json!({}));
        let definitions = vec![
            json!({"name":"left.Manager","shape":{"kind":"class"}}),
            json!({"name":"right.Manager","shape":{"kind":"class"}}),
        ];
        let rendered = project.register_virtual_types(&definitions).unwrap();
        let left = virtual_internal_name("left.Manager");
        let right = virtual_internal_name("right.Manager");
        assert_ne!(left, right);
        assert!(
            rendered[&PathBuf::from(&project.virtual_modules["left"]).with_extension("pyi")]
                .contains(&format!("Manager = {left}"))
        );
        assert_eq!(
            project.canonical_name("models", &format!("tuple[{left}, {right}]")),
            "tuple[left.Manager, right.Manager]"
        );
        assert_eq!(
            project.rewrite_virtual_identities(&format!("Literal['{left}'] | {left}")),
            format!("Literal['{left}'] | left.Manager")
        );
    }

    #[test]
    fn official_checker_keeps_virtual_alias_identity_generics_and_self() {
        let checker = std::env::var_os("TY_COMPANION_TEST_OFFICIAL")
            .expect("TY_COMPANION_TEST_OFFICIAL must name the unchanged official ty binary");
        let fixture = Fixture::new(&[
            (
                "base.pyi",
                "from typing_extensions import Self\nclass Base[T]:\n    value: T\n    def same(self) -> Self: ...\n",
            ),
            (
                "models.py",
                "from typing import assert_type, reveal_type\nfrom left import Manager as Left\nfrom right import Manager as Right\nleft = Left()\nright = Right()\nassert_type(left.value, int)\nassert_type(left.same(), Left)\nreveal_type(left)\nreveal_type(right)\nwrong: Left = right\n",
            ),
        ]);
        let definitions = vec![
            json!({"name":"left.Manager","shape":{"kind":"class","bases":[typ("base.Base[int]")]}}),
            json!({"name":"right.Manager","shape":{"kind":"class","bases":[typ("base.Base[int]")]}}),
        ];
        for (path, text) in render_virtual_types(&definitions).unwrap() {
            std::fs::write(fixture.0.join(path), text).unwrap();
        }
        let output = std::process::Command::new(checker)
            .args([
                "check",
                "--python-version",
                "3.13",
                "--output-format",
                "gitlab",
                "models.py",
            ])
            .current_dir(&fixture.0)
            .env("TY_UV", "0")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let diagnostics: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        let codes: Vec<_> = diagnostics
            .iter()
            .map(|d| d["check_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            codes,
            vec!["revealed-type", "revealed-type", "invalid-assignment"],
            "{diagnostics:#?}"
        );
        assert!(
            diagnostics[0]["description"]
                .as_str()
                .unwrap()
                .contains(&virtual_internal_name("left.Manager"))
        );
        assert!(
            diagnostics[1]["description"]
                .as_str()
                .unwrap()
                .contains(&virtual_internal_name("right.Manager"))
        );
    }

    #[test]
    fn virtual_modules_do_not_shadow_real_modules_or_lose_cross_references() -> Result<()> {
        let checker = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let fixture = Fixture::new(&[
            ("library.py", "class Real: pass\n"),
            ("models.py", "# original module\n"),
        ]);
        let original = std::fs::read(fixture.0.join("library.py"))?;
        let mut project = fixture.project(json!({}), json!({}));
        let definitions = vec![
            json!({"name":"library.Manager","shape":{"kind":"class","members":[{"name":"count","access":{"kind":"value","type-expr":typ("int")}}, {"name":"other","access":{"kind":"value","type-expr":typ("library.virtual.nested.Other")}}, {"name":"real","access":{"kind":"value","type-expr":typ("library.Real")}}, {"name":"peer","access":{"kind":"value","type-expr":typ("library.Peer")}}]}}),
            json!({"name":"library.Peer","shape":{"kind":"class","members":[{"name":"label","access":{"kind":"value","type-expr":typ("str")}}]}}),
            json!({"name":"library.virtual.nested.Other","shape":{"kind":"class","members":[{"name":"label","access":{"kind":"value","type-expr":typ("str")}}]}}),
        ];
        for (path, text) in project.register_virtual_types(&definitions)? {
            assert!(!path.starts_with("library"));
            std::fs::write(fixture.0.join(path), text)?;
        }
        let mut types = crate::type_render::Types::with_project("_test_", "models", &project);
        let manager = types.render(&typ("library.Manager"))?;
        let real = types.render(&typ("library.Real"))?;
        let imported_manager = types.render(&json!({"expression":"M", "imports":[{"module":"library","name":"Manager","alias":"M"}]}))?;
        let imported_real = types.render(
            &json!({"expression":"R", "imports":[{"module":"library","name":"Real","alias":"R"}]}),
        )?;
        let text = format!(
            "from typing import assert_type, reveal_type\nimport library\n{}\ndef check(value: {manager}, real: {real}, imported: {imported_manager}, imported_real: {imported_real}) -> None:\n    assert_type(value.count, int)\n    assert_type(value.other.label, str)\n    assert_type(value.real, library.Real)\n    assert_type(value.peer.label, str)\n    assert_type(real, library.Real)\n    assert_type(imported_real, library.Real)\n    assert_type(imported.count, int)\n    assert_type(library.Real(), library.Real)\n    wrong: str = value.count\n    reveal_type(value)\n",
            types.prelude()
        );
        std::fs::write(fixture.0.join("models.py"), text)?;
        let result = std::process::Command::new(checker)
            .args([
                "check",
                "--python-version",
                "3.13",
                "--output-format",
                "gitlab",
                "models.py",
            ])
            .current_dir(&fixture.0)
            .env("TY_UV", "0")
            .output()?;
        assert_eq!(
            result.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let diagnostics: Vec<Value> = serde_json::from_slice(&result.stdout)?;
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d["check_name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["invalid-assignment", "revealed-type"],
            "{diagnostics:#?}"
        );
        assert_eq!(std::fs::read(fixture.0.join("library.py"))?, original);
        let internal = project
            .virtual_identities
            .iter()
            .find_map(|(internal, public)| (public == "library.Manager").then_some(internal))
            .unwrap();
        assert!(
            diagnostics[1]["description"]
                .as_str()
                .unwrap()
                .contains(internal)
        );
        assert_eq!(
            project.rewrite_virtual_identities(internal),
            "library.Manager"
        );
        Ok(())
    }

    #[test]
    fn virtual_identity_prefix_escapes_existing_project_identifiers() {
        let colliding = virtual_internal_name("left.Manager");
        let source = format!("class {colliding}: pass\n");
        let fixture = Fixture::new(&[("models.py", &source)]);
        let mut project = fixture.project(json!({}), json!({}));
        let output = project
            .register_virtual_types(&[json!({"name":"left.Manager","shape":{"kind":"class"}})])
            .unwrap();
        let internal = project
            .virtual_identities
            .iter()
            .find_map(|(key, value)| (value == "left.Manager").then_some(key))
            .unwrap();
        assert_ne!(internal, &colliding);
        assert!(
            output[&PathBuf::from(&project.virtual_modules["left"]).with_extension("pyi")]
                .contains(&format!("Manager = {internal}"))
        );
        assert_eq!(project.canonical_name("models", internal), "left.Manager");
        assert_eq!(
            project.canonical_name("models", &colliding),
            format!("models.{colliding}")
        );
    }

    #[test]
    fn classvar_declaration_provenance_survives_sdk_field_filtering() {
        let fixture = Fixture::new(&[
            (
                "models.py",
                "from typing import ClassVar\nfrom typing_extensions import Self\nfrom external import Metadata\nclass Parent:\n    metadata: ClassVar[Metadata[Self]]\nclass Child(Parent): pass\n",
            ),
            ("external.pyi", "class Metadata[T]: pass\n"),
        ]);
        let project = fixture.project(json!({}), json!({}));
        assert!(
            project.class_summary("models.Parent").unwrap()["fields"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            project
                .declared_member_type("models.Parent", "metadata")
                .unwrap()["expression"],
            "typing.ClassVar[external.Metadata[Self]]"
        );
        assert_eq!(
            substitute_self(
                "tuple[Self, typing.Self, typing_extensions.Self, Literal['Self', 'typing.Self']]",
                "models.Child",
            ),
            "tuple[models.Child, models.Child, models.Child, Literal['Self', 'typing.Self']]"
        );
        assert_eq!(type_expression(&json!({"expression":"tuple[Alias, Alias.Nested, Literal['Alias']]", "imports":[{"module":"vendor","name":"Container","alias":"Alias"}]})).as_deref(), Some("tuple[vendor.Container, vendor.Container.Nested, Literal['Alias']]"));
    }

    #[test]
    fn nominal_constructor_provenance_rejects_unknown_allocation_or_metaclass_calls() {
        let fixture = Fixture::new(&[(
            "models.py",
            "class Normal: pass\nclass Meta(type): pass\nclass Parent(metaclass=Meta): pass\nclass Child(Parent): pass\nclass CustomNew:\n    def __new__(cls) -> str: return 'other'\nclass InheritedNew(CustomNew): pass\nclass CallingMeta(type):\n    def __call__(cls) -> str: return 'other'\nclass CustomCall(metaclass=CallingMeta): pass\nclass InheritedCall(CustomCall): pass\nclass UnknownBase(Missing): pass\n",
        )]);
        let project = fixture.project(json!({}), json!({}));
        for name in ["Normal", "Parent", "Child"] {
            assert_eq!(
                project.default_constructor_result(&format!("models.{name}")),
                Some(format!("models.{name}"))
            );
        }
        for name in [
            "CustomNew",
            "InheritedNew",
            "CustomCall",
            "InheritedCall",
            "UnknownBase",
        ] {
            assert_eq!(
                project.default_constructor_result(&format!("models.{name}")),
                None
            );
        }
    }

    #[test]
    fn official_checker_supports_virtual_descriptor_and_arbitrary_typed_dict_keys() {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL")
            .expect("TY_COMPANION_TEST_OFFICIAL must point to the unchanged official checker");
        let fixture = Fixture::new(&[(
            "models.py",
            "from typing import assert_type\nfrom generated import Row, Value\ndef check(row: Row) -> None:\n    assert_type(row['x-y'], int)\n    assert_type(row['nested']['value'], str)\ncheck({'x-y': 1, 'nested': {'value': 'ok'}})\nvalue = Value()\nassert_type(value.number, str | int)\nassert_type(Value.number, str)\nvalue.number = 'class contract'\nwrong: int = Value.number\n",
        )]);
        let ty = |expression: &str| json!({"expression":expression,"mode":"annotation"});
        let definitions = vec![
            json!({"name":"generated.Row","shape":{"kind":"typed-dict","fields":[
                {"name":"x-y","type-expr":ty("int")},
                {"name":"class","type-expr":ty("str"),"required":false,"read-only":true},
                {"name":"nested","type-expr":ty("TypedDict({'value': str})")}
            ]}}),
            json!({"name":"generated.Value","shape":{"kind":"class","members":[
                {"name":"number","access":{"kind":"descriptor","class-type":ty("str"),"instance-get-type":ty("int"),"instance-set-type":ty("int")}}
            ]}}),
        ];
        for (path, text) in render_virtual_types(&definitions).unwrap() {
            let path = fixture.0.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let output = std::process::Command::new(binary)
            .args([
                "check",
                "--python-version",
                "3.13",
                "--output-format",
                "gitlab",
                "models.py",
                "generated.pyi",
            ])
            .current_dir(&fixture.0)
            .env("TY_UV", "0")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let diagnostics: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 11);
    }
}
