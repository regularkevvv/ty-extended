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
    let seconds = now.duration_since(*previous).as_secs_f64();
    *phases.entry(name).or_default() += seconds;
    if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
        eprintln!("companion phase {name}: {seconds:.3}s");
    }
    *previous = now;
}

/// The first native hover section is the inferred type; subsequent sections
/// contain documentation. Preserve signatures and literals inside that section.
fn hover_type(hover: &Value) -> Option<&str> {
    let contents = hover.get("contents")?;
    let text = contents
        .get("value")
        .and_then(Value::as_str)
        .or_else(|| contents.as_str())?;
    if contents["kind"] == "markdown" {
        let (opening, rest) = text.split_once('\n')?;
        let fence_len = opening.bytes().take_while(|byte| *byte == b'`').count();
        if fence_len < 3 {
            return None;
        }
        let fence = &opening[..fence_len];
        rest.split_once(&format!("\n{fence}")).map(|(ty, _)| ty)
    } else {
        Some(
            text.split("\n---------------------------------------------\n")
                .next()?
                .trim(),
        )
    }
}

fn files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    copyable_files(root, out, false)
}
fn copyable_files(root: &Path, out: &mut Vec<PathBuf>, metadata: bool) -> Result<()> {
    collect_files(root, out, metadata, None)
}
fn snapshot_files(
    root: &Path,
    out: &mut Vec<PathBuf>,
    known: &mut std::collections::HashSet<PathBuf>,
) -> Result<()> {
    // Callers supply canonical directories. Recursion never follows directory
    // symlinks, so ordinary entries retain that identity; file symlinks keep
    // the full canonicalization path in both snapshot readers.
    collect_files(root, out, false, Some(known))
}
fn collect_files(
    root: &Path,
    out: &mut Vec<PathBuf>,
    metadata: bool,
    mut known: Option<&mut std::collections::HashSet<PathBuf>>,
) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !name.starts_with('.')
                && !matches!(
                    name.as_ref(),
                    "target" | "dist" | "__pycache__" | "node_modules"
                )
            {
                collect_files(&path, out, metadata, known.as_deref_mut())?;
            }
        } else if path
            .extension()
            .is_some_and(|x| x == "py" || x == "pyi" || x == "ipynb")
            || metadata && path.file_name().is_some_and(|name| name == "py.typed")
        {
            if kind.is_file()
                && let Some(known) = known.as_deref_mut()
            {
                known.insert(path.clone());
            }
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
fn sync_support_modules(
    stubs: &Path,
    units: &BTreeMap<PathBuf, File>,
    previous: &mut BTreeMap<PathBuf, String>,
    server: Option<&mut crate::checker::ProjectChecker>,
) -> Result<()> {
    let mut current = BTreeSet::new();
    let mut changes = Vec::new();
    for unit in units.values() {
        for (module, source) in &unit.analysis.support_modules {
            let path = stubs.join(format!("{module}.pyi"));
            current.insert(path.clone());
            if previous.get(&path) == Some(source) {
                continue;
            }
            let change = if path.exists() { 2 } else { 1 };
            write(&path, source)?;
            changes.push(json!({"uri":crate::checker::file_uri(&path)?,"type":change}));
            previous.insert(path, source.clone());
        }
    }
    let removed: Vec<_> = previous
        .keys()
        .filter(|path| !current.contains(*path))
        .cloned()
        .collect();
    for path in removed {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        changes.push(json!({"uri":crate::checker::file_uri(&path)?,"type":3}));
        previous.remove(&path);
    }
    if !changes.is_empty()
        && let Some(server) = server
    {
        server.notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":changes}),
        )?;
    }
    Ok(())
}

struct VirtualProjection {
    modules: BTreeMap<PathBuf, crate::project::VirtualModule>,
    materialized: Option<BTreeSet<String>>,
}

impl VirtualProjection {
    fn sync(
        &mut self,
        stubs: &Path,
        needed: &mut BTreeSet<String>,
        previous: &mut BTreeSet<PathBuf>,
        server: Option<&mut crate::checker::ProjectChecker>,
    ) -> Result<()> {
        if self.materialized.as_ref() == Some(needed) {
            return Ok(());
        }
        let sources = crate::project::requested_virtual_sources(&self.modules, needed);
        let next: BTreeSet<_> = sources.keys().map(|path| stubs.join(path)).collect();
        let mut changes = Vec::new();
        for path in previous.difference(&next) {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
            changes.push(json!({"uri":crate::checker::file_uri(path)?,"type":3}));
        }
        for (path, code) in sources {
            let path = stubs.join(path);
            if std::fs::read_to_string(&path).is_ok_and(|previous| previous == code) {
                continue;
            }
            let change = if path.exists() { 2 } else { 1 };
            let mut parent = path.parent();
            while let Some(directory) = parent.filter(|directory| !directory.exists()) {
                changes.push(json!({"uri":crate::checker::file_uri(directory)?,"type":1}));
                parent = directory.parent();
            }
            write(&path, &code)?;
            changes.push(json!({"uri":crate::checker::file_uri(&path)?,"type":change}));
        }
        *previous = next;
        self.materialized = Some(needed.clone());
        if !changes.is_empty()
            && let Some(server) = server
        {
            server.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes":changes}),
            )?;
        }
        Ok(())
    }
}
fn copy_python(root: &Path, target: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    copyable_files(root, &mut paths, true)?;
    copy_python_paths(root, target, &paths)?;
    paths.retain(|path| path.file_name().is_none_or(|name| name != "py.typed"));
    Ok(paths)
}
fn copy_python_paths(root: &Path, target: &Path, paths: &[PathBuf]) -> Result<()> {
    let mut directories = BTreeSet::new();
    let mut copies = Vec::with_capacity(paths.len());
    for path in paths {
        let dst = target.join(path.strip_prefix(root)?);
        if let Some(parent) = dst.parent()
            && directories.insert(parent.to_owned())
        {
            std::fs::create_dir_all(parent)?;
        }
        copies.push((path.clone(), dst));
    }
    // Overlay trees finish in precedence order; independent files within one
    // tree share the same bounded parallelism policy as source parsing.
    for result in
        crate::parallel::map_ordered(&copies, crate::parallel::workers(copies.len()), |copy| {
            copy_source_files(std::slice::from_ref(copy))
        })?
    {
        result?;
    }
    Ok(())
}

fn copy_source_files(copies: &[(PathBuf, PathBuf)]) -> Result<()> {
    use std::io::ErrorKind;
    for (path, dst) in copies {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dst)
        {
            Ok(mut output) => {
                // Analysis copies need bytes, not source permissions or clone
                // metadata. A new writable file also handles read-only package
                // caches without per-file stat/chmod calls. Never hard-link:
                // projections must not share writable storage with originals.
                std::io::copy(&mut std::fs::File::open(path)?, &mut output)?;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                write(dst, &std::fs::read_to_string(path)?)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
// Resolve overlay names using the stock checker's own target-dependent stdlib.
// Navigation alone is insufficient: an uncached vendored file has no LSP URI
// when the user's global cache is read-only. An isolated import check needs no
// extraction and also honors custom typeshed VERSIONS/platform restrictions.
fn stdlib_overlay_modules(
    config: &project_config::Config,
    options: &BTreeMap<String, String>,
    out: &Path,
    third_party: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let mut candidates = BTreeSet::new();
    for root in &config.stub_roots {
        let mut paths = Vec::new();
        files(root, &mut paths)?;
        for path in paths {
            let relative = path.strip_prefix(root)?;
            let Some(module) = overlay_top_level(relative) else {
                continue;
            };
            if config.typeshed.is_none() && third_party.contains(&module) {
                continue;
            }
            candidates.insert(module);
        }
    }
    candidates
        .retain(|module| ruff_python_parser::parse_module(&format!("import {module}\n")).is_ok());
    if candidates.is_empty() {
        return Ok(BTreeSet::new());
    }
    let probe_root = out.join("overlay-resolution");
    let empty_environment = probe_root.join("empty-python");
    std::fs::create_dir_all(empty_environment.join("Lib/site-packages"))?;
    std::fs::create_dir_all(
        empty_environment.join(format!("lib/python{}/site-packages", config.python_version)),
    )?;
    write(
        &empty_environment.join("pyvenv.cfg"),
        &format!(
            "home = .\nversion_info = {}.0\ninclude-system-site-packages = false\n",
            config.python_version
        ),
    )?;
    let mut probe_config = json!({
        "environment": {
            "python": empty_environment,
            "python-version": config.python_version,
            "python-platform": config.platform,
            "root": [probe_root],
            "extra-paths": []
        }
    });
    if let Some(typeshed) = &config.typeshed {
        probe_config["environment"]["typeshed"] = json!(typeshed);
    }
    let probe_config_path = probe_root.join("ty.toml");
    write(
        &probe_config_path,
        &toml::to_string(&toml::Value::try_from(probe_config)?)?,
    )?;
    let candidates = candidates.into_iter().collect::<Vec<_>>();
    let probe = probe_root.join("stdlib-probe.py");
    write(
        &probe,
        &candidates
            .iter()
            .map(|module| format!("import {module}\n"))
            .collect::<String>(),
    )?;
    let diagnostics = check(
        Path::new(options.get("ty").context("missing --ty")?),
        &probe_root,
        &probe_config_path,
        &[probe],
    )
    .context("resolve plugin overlay precedence against the official standard library")?;
    write(
        &probe_root.join("diagnostics.json"),
        &serde_json::to_string_pretty(&diagnostics)?,
    )?;
    let mut available = candidates.iter().cloned().collect::<BTreeSet<_>>();
    for diagnostic in diagnostics {
        if matches!(
            diagnostic["check_name"].as_str(),
            Some("unresolved-import" | "import-not-found")
        ) {
            let line = diagnostic
                .pointer("/location/positions/begin/line")
                .and_then(Value::as_u64)
                .context("stdlib resolution diagnostic is missing its source line")?;
            let module = candidates
                .get(line.saturating_sub(1) as usize)
                .context("stdlib resolution diagnostic is outside the probe")?;
            available.remove(module);
        }
    }
    write(
        &probe_root.join("stdlib-modules.json"),
        &serde_json::to_string_pretty(&available)?,
    )?;
    Ok(available)
}

fn overlay_top_level(relative: &Path) -> Option<String> {
    let first = relative.components().next()?.as_os_str().to_str()?;
    let top = if relative.components().count() == 1 {
        Path::new(first).file_stem()?.to_str()?
    } else {
        first
    };
    Some(top.strip_suffix("-stubs").unwrap_or(top).to_owned())
}
fn include_overlay(relative: &Path, stdlib: &BTreeSet<String>) -> bool {
    overlay_top_level(relative).is_none_or(|module| !stdlib.contains(&module))
}
fn copy_overlays(
    roots: &[PathBuf],
    target: &Path,
    stdlib: &BTreeSet<String>,
) -> Result<BTreeSet<PathBuf>> {
    let mut copied = BTreeSet::new();
    for root in roots.iter().rev() {
        let mut paths = Vec::new();
        copyable_files(root, &mut paths, true)?;
        paths.retain(|path| include_overlay(path.strip_prefix(root).unwrap(), stdlib));
        copied.extend(
            paths
                .iter()
                .map(|path| target.join(path.strip_prefix(root).unwrap())),
        );
        copy_python_paths(root, target, &paths)?;
    }
    Ok(copied)
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
    borrowed: Option<&Path>,
) -> Result<PathBuf> {
    if borrowed.is_some_and(|root| original.starts_with(root)) {
        return Ok(original.to_owned());
    }
    if let Some(relative) = resolved.get(original) {
        return Ok(stubs.join(relative));
    }
    roots
        .iter()
        .find_map(|root| original.strip_prefix(root).ok())
        .map(|relative| stubs.join(relative))
        .with_context(|| format!("stub {} is outside configured overlays", original.display()))
}

fn resolved_import_path(
    uri: &str,
    view: &Path,
    stubs: &Path,
    stub_roots: &[PathBuf],
    resolved: &BTreeMap<PathBuf, PathBuf>,
) -> Result<Option<PathBuf>> {
    if crate::lsp_mapping::decode_file_uri(uri)?.is_none() {
        return Ok(None);
    }
    // File URIs omit Windows' verbatim prefix. Use the same canonical identity
    // as the project roots before rejecting generated files or restoring an
    // overlay's original source. Never index a generated copy as a dependency.
    let path = crate::lsp_server::uri_path(uri)?;
    if path.starts_with(view) {
        return Ok(None);
    }
    if let Ok(relative) = path.strip_prefix(stubs) {
        return Ok(resolved
            .iter()
            .find_map(|(original, generated)| (generated == relative).then(|| original.clone()))
            .or_else(|| {
                stub_roots
                    .iter()
                    .map(|root| root.join(relative))
                    .find(|path| path.is_file())
            }));
    }
    Ok(Some(path))
}

fn check(binary: &Path, root: &Path, config: &Path, paths: &[PathBuf]) -> Result<Vec<Value>> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let output = Command::new(binary)
        .args(["check", "--output-format", "gitlab", "--config-file"])
        .arg(crate::checker::native_cli_path(config))
        .args(
            paths
                .iter()
                .map(|path| crate::checker::native_cli_path(path)),
        )
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

/// Compress an already selected set of generated files without broadening it.
/// A directory replaces its descendants only when every file in it is selected.
/// Symlinks and non-Python files prevent compression; they are never followed.
pub(crate) fn compact_check_paths(
    root: &Path,
    selected: &BTreeSet<PathBuf>,
) -> Result<Vec<PathBuf>> {
    fn visit(
        path: &Path,
        entry: Option<&std::fs::DirEntry>,
        remaining: &mut std::collections::HashSet<PathBuf>,
        ancestors: &std::collections::HashSet<&Path>,
    ) -> Result<(bool, Vec<PathBuf>)> {
        if !remaining.contains(path) && !ancestors.contains(path) {
            return Ok((false, Vec::new()));
        }
        let kind = entry.map_or_else(
            || std::fs::symlink_metadata(path).map(|m| m.file_type()),
            std::fs::DirEntry::file_type,
        )?;
        if !kind.is_dir() {
            let included = remaining.remove(path);
            let discoverable = kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension == "py" || extension == "pyi");
            return Ok((
                included && discoverable,
                if included { vec![path.into()] } else { vec![] },
            ));
        }
        let mut complete = true;
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let (included, children) = visit(&entry.path(), Some(&entry), remaining, ancestors)?;
            complete &= included;
            paths.extend(children);
        }
        if complete && !paths.is_empty() {
            paths = vec![path.into()];
        }
        let traversable = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_none_or(|name| {
                !name.starts_with('.')
                    && !project_config::DEFAULT_EXCLUDED_DIRECTORIES.contains(&name)
            });
        Ok((complete && traversable, paths))
    }
    let ancestors = selected
        .iter()
        .flat_map(|path| path.ancestors().skip(1))
        .collect::<std::collections::HashSet<_>>();
    let mut remaining = selected.iter().cloned().collect();
    let (_, mut paths) = visit(root, None, &mut remaining, &ancestors)?;
    paths.extend(remaining);
    paths.sort();
    Ok(paths)
}

/// The CLI can check a complete disk projection in parallel without opening
/// thousands of LSP documents. Selection has already been resolved by the
/// companion; disable native discovery filters only for this exact file union.
/// The derived configuration stays beside the original to preserve relative
/// environment paths and per-file rule overrides.
fn check_selected_files(
    binary: &Path,
    root: &Path,
    config: &Path,
    paths: &[PathBuf],
) -> Result<Vec<Value>> {
    let selected = paths.iter().cloned().collect();
    let paths = compact_check_paths(root, &selected)?;
    check_compacted_files(binary, root, config, &paths)
}

fn check_compacted_files(
    binary: &Path,
    root: &Path,
    config: &Path,
    paths: &[PathBuf],
) -> Result<Vec<Value>> {
    let mut table: toml::Table = std::fs::read_to_string(config)?.parse()?;
    let src = table
        .entry("src")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("src must be a table")?;
    src.insert("include".into(), toml::Value::Array(vec!["**".into()]));
    src.insert("exclude".into(), toml::Value::Array(vec![]));
    src.insert("respect-ignore-files".into(), false.into());
    src.insert("exclude-scripts".into(), false.into());
    let config = config.with_file_name("selected-checker.toml");
    write(&config, &toml::to_string(&table)?)?;
    check(binary, root, &config, paths)
}
fn path_of(d: &Value, root: &Path) -> PathBuf {
    let p = PathBuf::from(d["location"]["path"].as_str().unwrap_or(""));
    let path = if p.is_absolute() { p } else { root.join(p) };
    // Native CLI paths use ordinary Windows spelling, while internal project
    // keys use canonicalize()'s verbatim spelling. Restore that identity before
    // selecting a document; changing presentation cannot drop its diagnostics.
    #[cfg(windows)]
    let path = path.canonicalize().unwrap_or(path);
    path
}
fn imports(text: &str, source_type: ruff_python_ast::PySourceType) -> Result<BTreeSet<String>> {
    let parsed = crate::project::parse_source(text, source_type)?;
    Ok(imports_from_suite(&parsed.syntax().body))
}
fn imports_from_suite(suite: &[ruff_python_ast::Stmt]) -> BTreeSet<String> {
    let mut modules = BTreeSet::new();
    for statement in suite {
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
    modules
}
fn pure_import_probe(
    sources: &BTreeMap<PathBuf, String>,
    expected: &Path,
) -> Option<BTreeSet<String>> {
    if sources.is_empty() {
        return Some(BTreeSet::new());
    }
    if sources.len() != 1 {
        return None;
    }
    let text = sources.get(expected)?;
    let parsed = crate::project::parse_source(text, ruff_python_ast::PySourceType::Python).ok()?;
    let mut modules = BTreeSet::new();
    for statement in &parsed.syntax().body {
        let ruff_python_ast::Stmt::Import(import) = statement else {
            return None;
        };
        for alias in &import.names {
            if alias.asname.is_some() {
                return None;
            }
            modules.insert(alias.name.to_string());
        }
    }
    Some(modules)
}

fn import_prefixes(modules: &BTreeSet<String>) -> BTreeSet<String> {
    let mut prefixes = BTreeSet::new();
    for name in modules {
        let mut prefix = name.as_str();
        loop {
            prefixes.insert(prefix.to_owned());
            let Some((parent, _)) = prefix.rsplit_once('.') else {
                break;
            };
            prefix = parent;
        }
    }
    prefixes
}

struct File {
    source: Arc<Source>,
    suite: Arc<Vec<ruff_python_ast::Stmt>>,
    analysis: Analysis,
    path: PathBuf,
    oracle: Oracle,
    patches: ReturnPatches,
    return_types: BTreeMap<crate::model::Span, Value>,
    early_returns: BTreeMap<crate::model::Span, crate::runtime::EarlyCallReturn>,
    types: Types,
    diagnostics: Vec<Value>,
    rendered: Option<Rendered>,
    selected: bool,
    valid: bool,
    names: BTreeMap<String, String>,
    final_edits: Option<crate::mapping::Edits>,
    dirty: bool,
    /// No plugin-observable syntax or generated edit exists in this file.
    /// Native workspace diagnostics still check the complete original source.
    passthrough: bool,
    raw_diagnostics: Vec<Value>,
    flow_diagnostics: Vec<Value>,
    pass: Option<ProjectionPass>,
    reads_protocol_aliases: bool,
}

impl File {
    fn prepare_early_returns(&mut self, runtime: &mut Runtime) -> Result<bool> {
        if !runtime.early_returns_enabled() && self.early_returns.is_empty() {
            return Ok(false);
        }
        let mut next = BTreeMap::new();
        for call in self.analysis.calls.values() {
            if let Some(response) =
                crate::runtime::speculative_call_return(runtime, &self.source, call)?
            {
                next.insert(call.span, response);
            }
        }
        let changed = next != self.early_returns;
        for span in self.early_returns.keys() {
            self.patches.remove(span);
            self.return_types.remove(span);
        }
        self.analysis.early_calls.clear();
        for (span, response) in &next {
            if !response.native {
                let rendered = self.types.render(&response.response["return-type"])?;
                self.patches.insert(*span, rendered);
            }
            self.return_types
                .insert(*span, response.response["return-type"].clone());
            self.analysis.early_calls.insert(*span);
        }
        self.types.register_protocol_types(runtime);
        self.early_returns = next;
        Ok(changed)
    }

    fn write_projection(&self, generated: &Rendered, out: &Path) -> Result<()> {
        if self.path.starts_with(out)
            && self
                .rendered
                .as_ref()
                .is_none_or(|previous| previous.text != generated.text)
        {
            // This session owns the projection files. An identical in-memory
            // view needs no disk read or write during another inference pass.
            write(&self.path, &generated.text)?;
        }
        Ok(())
    }
}

/// Inputs to one lowering/flow pass within a fixed project revision. Inference
/// still queries every dirty file after reuse: an unchanged local view can
/// acquire different types when another module's projection changes.
#[derive(PartialEq)]
struct ProjectionInputs {
    generation: (u64, Option<u64>),
    oracle: Oracle,
    patches: ReturnPatches,
    return_types: BTreeMap<crate::model::Span, Value>,
    early_calls: BTreeSet<crate::model::Span>,
    prelude: String,
}

impl ProjectionInputs {
    fn new(unit: &File, runtime: &Runtime) -> Self {
        Self {
            generation: runtime.projection_context(unit.reads_protocol_aliases),
            oracle: unit.oracle.clone(),
            patches: unit.patches.clone(),
            return_types: unit.return_types.clone(),
            early_calls: unit.analysis.early_calls.clone(),
            prelude: unit.types.prelude(),
        }
    }
}

struct ProjectionPass {
    inputs: ProjectionInputs,
    flow: crate::flow::FlowResult,
}

/// Native diagnostics may cover the final check when no user type probes are
/// needed. Retain every checked source and generated dependency context: a
/// successful import/schema preflight alone does not prove a later projection.
#[derive(PartialEq, Eq)]
struct NativeCheckInputs {
    sources: BTreeMap<PathBuf, String>,
    paths: Vec<PathBuf>,
    generation: u64,
    virtual_types: Option<BTreeSet<String>>,
    support_sources: BTreeMap<PathBuf, String>,
}

impl NativeCheckInputs {
    fn new(
        units: &BTreeMap<PathBuf, File>,
        runtime: &Runtime,
        virtual_projection: &VirtualProjection,
        support_sources: &BTreeMap<PathBuf, String>,
    ) -> Self {
        Self {
            sources: units
                .iter()
                .map(|(path, unit)| (path.clone(), unit.rendered.as_ref().unwrap().text.clone()))
                .collect(),
            paths: units
                .values()
                .filter(|unit| unit.selected && unit.dirty)
                .map(|unit| unit.path.clone())
                .collect(),
            generation: runtime.projection_generation(),
            virtual_types: virtual_projection.materialized.clone(),
            support_sources: support_sources.clone(),
        }
    }
}

/// A native CLI result is reusable only while its complete observed
/// inputs still describe the final projection. It carries no mutable checker
/// state, and a rejected result falls back to the ordinary transport.
#[derive(Clone, Copy)]
struct NativeCliProjection<'a> {
    sources: &'a BTreeMap<PathBuf, String>,
    extras: &'a BTreeMap<PathBuf, String>,
    prepared: Option<&'a NativeCliPrepared>,
    bootstrap_imports: bool,
}

struct NativeCliCheck {
    root: PathBuf,
    thread: Option<std::thread::JoinHandle<Result<Vec<Value>>>>,
    originals: Arc<std::collections::HashMap<PathBuf, (crate::model::FileRevision, Arc<Source>)>>,
    file_revisions: Arc<std::collections::HashMap<PathBuf, crate::model::FileRevision>>,
    directories: BTreeMap<PathBuf, crate::model::FileRevision>,
    selected: Vec<PathBuf>,
    configuration: Vec<u8>,
    projected_sources: BTreeMap<PathBuf, String>,
    extra_sources: BTreeMap<PathBuf, String>,
    bootstrap_imports: Option<BTreeSet<String>>,
}

#[derive(Clone)]
struct NativeCliPrepared {
    originals: Arc<std::collections::HashMap<PathBuf, (crate::model::FileRevision, Arc<Source>)>>,
    file_revisions: Arc<std::collections::HashMap<PathBuf, crate::model::FileRevision>>,
    directories: BTreeMap<PathBuf, crate::model::FileRevision>,
    selected: Vec<PathBuf>,
    compact_paths: Vec<PathBuf>,
    configuration: Vec<u8>,
}

impl NativeCliPrepared {
    fn from_sources(
        root: &Path,
        config: &Path,
        paths: &[PathBuf],
        sources: &[(Arc<Source>, Option<crate::model::FileRevision>)],
        providers: &[PathBuf],
    ) -> Result<Option<Self>> {
        let Some(discovery) = NativeCliDiscovery::capture(root, config, paths, providers)? else {
            return Ok(None);
        };
        Self::with_discovery(discovery, root, sources, providers)
    }

    fn with_discovery(
        discovery: NativeCliDiscovery,
        root: &Path,
        sources: &[(Arc<Source>, Option<crate::model::FileRevision>)],
        providers: &[PathBuf],
    ) -> Result<Option<Self>> {
        let mut originals = std::collections::HashMap::with_capacity(sources.len());
        for (source, original_revision) in sources {
            let path = PathBuf::from(&source.path);
            if !std::iter::once(root)
                .chain(providers.iter().map(PathBuf::as_path))
                .any(|base| path.starts_with(base))
            {
                return Ok(None);
            }
            let revision = if let Some(revision) = *original_revision {
                revision
            } else {
                let (text, revision) = crate::project::read_snapshot(&path)?;
                if text != source.text {
                    return Ok(None);
                }
                let Some(revision) = revision else {
                    return Ok(None);
                };
                revision
            };
            if discovery.file_revisions.get(&path) != Some(&revision) {
                return Ok(None);
            }
            originals.insert(path, (revision, Arc::clone(source)));
        }
        if discovery
            .selected
            .iter()
            .any(|path| !discovery.file_revisions.contains_key(path) || !path.starts_with(root))
        {
            return Ok(None);
        }
        Ok(Some(Self {
            originals: Arc::new(originals),
            file_revisions: discovery.file_revisions,
            directories: discovery.directories,
            selected: discovery.selected,
            compact_paths: discovery.compact_paths,
            configuration: discovery.configuration,
        }))
    }
}

struct NativeCliDiscovery {
    file_revisions: Arc<std::collections::HashMap<PathBuf, crate::model::FileRevision>>,
    directories: BTreeMap<PathBuf, crate::model::FileRevision>,
    selected: Vec<PathBuf>,
    compact_paths: Vec<PathBuf>,
    configuration: Vec<u8>,
}

impl NativeCliDiscovery {
    fn capture(
        root: &Path,
        config: &Path,
        paths: &[PathBuf],
        providers: &[PathBuf],
    ) -> Result<Option<Self>> {
        let mut directories = BTreeMap::new();
        let mut files = Vec::new();
        // Observe discovery directories too: a new module can change import
        // resolution without modifying any previously loaded source file.
        // Large unrelated trees retain the ordinary checker route.
        let mut pending = std::iter::once(root.to_owned())
            .chain(providers.iter().cloned())
            .collect::<Vec<_>>();
        let mut visited = BTreeSet::new();
        let mut entries = 0usize;
        while let Some(directory) = pending.pop() {
            if !visited.insert(directory.clone()) {
                continue;
            }
            if visited.len() > 4096 {
                return Ok(None);
            }
            let Some(revision) = crate::model::file_revision(&std::fs::metadata(&directory)?)
            else {
                return Ok(None);
            };
            directories.insert(directory.clone(), revision);
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                entries += 1;
                if entries > 100_000 {
                    return Ok(None);
                }
                let name = entry.file_name();
                if matches!(
                    name.to_str(),
                    Some(
                        ".git"
                            | ".venv"
                            | "venv"
                            | "node_modules"
                            | "__pycache__"
                            | "target"
                            | "dist"
                    )
                ) {
                    continue;
                }
                let kind = entry.file_type()?;
                if kind.is_symlink() {
                    return Ok(None);
                }
                if kind.is_dir() {
                    pending.push(entry.path());
                } else if kind.is_file() {
                    let path = entry.path();
                    if path
                        .extension()
                        .is_some_and(|ext| ext == "py" || ext == "pyi")
                        || name == "py.typed"
                    {
                        files.push(path);
                    }
                }
            }
        }
        if paths.is_empty() {
            return Ok(None);
        }
        let revisions =
            crate::parallel::map_ordered(&files, crate::parallel::workers(files.len()), |path| {
                std::fs::metadata(path)
                    .ok()
                    .as_ref()
                    .and_then(crate::model::file_revision)
            })?;
        let Some(revisions) = revisions.into_iter().collect::<Option<Vec<_>>>() else {
            return Ok(None);
        };
        Ok(Some(Self {
            file_revisions: Arc::new(files.into_iter().zip(revisions).collect()),
            directories,
            selected: paths.to_vec(),
            compact_paths: compact_check_paths(root, &paths.iter().cloned().collect())?,
            configuration: std::fs::read(config)?,
        }))
    }
}

struct PendingNativeCliPreparation {
    thread: Option<std::thread::JoinHandle<Result<Option<NativeCliDiscovery>>>>,
    root: PathBuf,
    providers: Vec<PathBuf>,
}

impl PendingNativeCliPreparation {
    fn start(root: &Path, config: &Path, paths: &[PathBuf], providers: &[PathBuf]) -> Result<Self> {
        let task_root = root.to_owned();
        let config = config.to_owned();
        let paths = paths.to_vec();
        let task_providers = providers.to_vec();
        let thread = std::thread::Builder::new()
            .name("native-input-discovery".into())
            .spawn(move || {
                NativeCliDiscovery::capture(&task_root, &config, &paths, &task_providers)
            })?;
        Ok(Self {
            thread: Some(thread),
            root: root.to_owned(),
            providers: providers.to_vec(),
        })
    }
    fn finish(mut self, project: &Project) -> Option<NativeCliPrepared> {
        let discovery = self.thread.take()?.join().ok()?.ok()??;
        let sources = project
            .modules
            .values()
            .map(|module| (Arc::clone(&module.source), module.original_revision))
            .collect::<Vec<_>>();
        NativeCliPrepared::with_discovery(discovery, &self.root, &sources, &self.providers).ok()?
    }
}

impl Drop for PendingNativeCliPreparation {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl NativeCliCheck {
    fn start(
        binary: &Path,
        root: &Path,
        config: &Path,
        paths: &[PathBuf],
        project: &Project,
        providers: &[PathBuf],
        projections: Option<NativeCliProjection<'_>>,
    ) -> Result<Option<Self>> {
        let mut prepared =
            if let Some(prepared) = projections.and_then(|projection| projection.prepared) {
                prepared.clone()
            } else {
                let sources = project
                    .modules
                    .values()
                    .map(|module| (Arc::clone(&module.source), module.original_revision))
                    .collect::<Vec<_>>();
                let Some(prepared) =
                    NativeCliPrepared::from_sources(root, config, paths, &sources, providers)?
                else {
                    return Ok(None);
                };
                prepared
            };
        if prepared.selected.as_slice() != paths {
            prepared.selected = paths.to_vec();
            prepared.compact_paths = compact_check_paths(root, &paths.iter().cloned().collect())?;
        }
        let NativeCliPrepared {
            originals,
            file_revisions,
            directories,
            selected,
            compact_paths,
            configuration,
        } = prepared;
        let mut projected_sources = BTreeMap::new();
        for (path, text) in projections
            .into_iter()
            .flat_map(|projection| projection.sources)
        {
            let Some((_, source)) = originals.get(path) else {
                return Ok(None);
            };
            if source.text != *text {
                if path.starts_with(root)
                    || providers.len() != 1
                    || !path.starts_with(&providers[0])
                    || path.extension().is_none_or(|ext| ext != "pyi")
                {
                    return Ok(None);
                }
                projected_sources.insert(path.clone(), text.clone());
            }
        }
        let mut actual_config = config.to_owned();
        let mut overlay_mapping = None;
        if !projected_sources.is_empty() {
            let provider = &providers[0];
            // An abandoned early attempt can have a larger patch set than its
            // retry. Every attempt needs a fresh directory, including when a
            // caller reuses an output directory left by an older process.
            static NEXT_OVERLAY: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let parent = config.parent().context("native CLI workspace")?;
            let mut overlay = None;
            for _ in 0..32 {
                let sequence = NEXT_OVERLAY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let candidate = parent.join(format!(
                    "native-cli-overlay-{}-{sequence}",
                    std::process::id()
                ));
                match std::fs::create_dir(&candidate) {
                    Ok(()) => {
                        overlay = Some(candidate);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let Some(overlay) = overlay else {
                return Ok(None);
            };
            for (path, text) in &projected_sources {
                let relative = path.strip_prefix(provider)?;
                write(&overlay.join(relative), text)?;
                for parent in relative
                    .parent()
                    .into_iter()
                    .flat_map(Path::ancestors)
                    .filter(|parent| !parent.as_os_str().is_empty())
                {
                    for init in ["__init__.pyi", "__init__.py"] {
                        let original = provider.join(parent).join(init);
                        let destination = overlay.join(parent).join(init);
                        if original.is_file() && !destination.exists() {
                            let Some((_, source)) = originals.get(&original) else {
                                return Ok(None);
                            };
                            write(&destination, &source.text)?;
                        }
                    }
                }
                if relative.components().count() > 1 {
                    let package = relative
                        .components()
                        .next()
                        .context("overlay package")?
                        .as_os_str();
                    write(&overlay.join(package).join("py.typed"), "partial\n")?;
                }
            }
            let mut document: toml::Value = toml::from_str(std::str::from_utf8(&configuration)?)?;
            document["environment"]["extra-paths"]
                .as_array_mut()
                .context("native CLI search paths")?
                .insert(
                    0,
                    toml::Value::String(overlay.to_string_lossy().into_owned()),
                );
            actual_config = config.with_file_name("native-cli-checker.toml");
            write(&actual_config, &toml::to_string(&document)?)?;
            overlay_mapping = Some((overlay, provider.clone()));
        }
        let extra_sources =
            projections.map_or_else(BTreeMap::new, |projection| projection.extras.clone());
        let bootstrap_imports = if projections.is_some_and(|p| p.bootstrap_imports) {
            let Some(imports) = pure_import_probe(
                &extra_sources,
                &config.with_file_name("native-bootstrap-imports.py"),
            ) else {
                return Ok(None);
            };
            Some(import_prefixes(&imports))
        } else {
            None
        };
        for (path, text) in &extra_sources {
            write(path, text)?;
        }
        let binary = binary.to_owned();
        let root = root.to_owned();
        let config = actual_config;
        let mut paths = compact_paths;
        paths.extend(extra_sources.keys().cloned());
        let checked_root = root.clone();
        let thread = std::thread::Builder::new()
            .name("native-cli-check".into())
            .spawn(move || {
                let start = Instant::now();
                let result = check_compacted_files(&binary, &root, &config, &paths).map(
                    |mut diagnostics| {
                        if let Some((overlay, provider)) = &overlay_mapping {
                            for diagnostic in &mut diagnostics {
                                let path = path_of(diagnostic, &root);
                                if let Ok(relative) = path.strip_prefix(overlay) {
                                    diagnostic["location"]["path"] = json!(provider.join(relative));
                                }
                            }
                        }
                        diagnostics
                    },
                );
                if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
                    eprintln!(
                        "companion native cli check: {:.6}s",
                        start.elapsed().as_secs_f64()
                    );
                }
                result
            })?;
        Ok(Some(Self {
            root: checked_root,
            thread: Some(thread),
            originals,
            file_revisions,
            directories,
            selected,
            configuration,
            projected_sources,
            extra_sources,
            bootstrap_imports,
        }))
    }

    fn finish(
        mut self,
        inputs: &NativeCheckInputs,
        config: &Path,
        extras: &BTreeMap<PathBuf, String>,
    ) -> Option<Vec<Value>> {
        let diagnostics = self.thread.take()?.join().ok()?.ok()?;
        let extras_match = if let Some(imports) = &self.bootstrap_imports {
            pure_import_probe(extras, &config.with_file_name("type-imports.py"))
                .is_some_and(|required| required.is_subset(imports))
                && diagnostics
                    .iter()
                    .all(|diagnostic| self.selected.contains(&path_of(diagnostic, &self.root)))
                && self.extra_sources.iter().all(|(path, source)| {
                    std::fs::read_to_string(path).is_ok_and(|actual| actual == *source)
                })
        } else {
            *extras == self.extra_sources
        };
        if !extras_match
            || inputs.paths != self.selected
            || self.selected.iter().any(|path| {
                !self.originals.contains_key(path) || !inputs.sources.contains_key(path)
            })
            || !inputs.support_sources.is_empty()
            || inputs
                .virtual_types
                .as_ref()
                .is_some_and(|types| !types.is_empty())
            || std::fs::read(config).ok()? != self.configuration
            || inputs.sources.iter().any(|(path, text)| {
                self.originals.get(path).is_none_or(|(_, source)| {
                    self.projected_sources.get(path).unwrap_or(&source.text) != text
                })
            })
        {
            if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
                let changed = inputs
                    .sources
                    .iter()
                    .filter(|(path, text)| {
                        self.originals.get(*path).is_none_or(|(_, source)| {
                            self.projected_sources.get(*path).unwrap_or(&source.text) != *text
                        })
                    })
                    .map(|(path, _)| path.display().to_string())
                    .collect::<Vec<_>>();
                eprintln!(
                    "companion native cli rejected: extras={} selection={} support={} virtual={} changed={changed:?}",
                    !extras_match,
                    inputs.paths != self.selected,
                    !inputs.support_sources.is_empty(),
                    inputs
                        .virtual_types
                        .as_ref()
                        .is_some_and(|types| !types.is_empty())
                );
            }
            return None;
        }
        let originals = self.file_revisions.iter().collect::<Vec<_>>();
        let valid = crate::parallel::map_ordered(
            &originals,
            crate::parallel::workers(originals.len()),
            |(path, revision)| {
                std::fs::metadata(path)
                    .ok()
                    .as_ref()
                    .and_then(crate::model::file_revision)
                    == Some(**revision)
            },
        )
        .ok()?;
        if valid.into_iter().any(|valid| !valid) {
            return None;
        }
        for (path, revision) in &self.directories {
            if std::fs::metadata(path)
                .ok()
                .as_ref()
                .and_then(crate::model::file_revision)
                != Some(*revision)
            {
                return None;
            }
        }
        Some(diagnostics)
    }

    fn observe_project(&mut self, project: &Project) -> bool {
        let originals = Arc::make_mut(&mut self.originals);
        for module in project.modules.values() {
            let path = PathBuf::from(&module.source.path);
            let Some(revision) = self.file_revisions.get(&path) else {
                return false;
            };
            if module.original_revision != Some(*revision) {
                return false;
            }
            originals.insert(path, (*revision, Arc::clone(&module.source)));
        }
        true
    }
}

impl Drop for NativeCliCheck {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn check_units(
    binary: &Path,
    root: &Path,
    config: &Path,
    units: &BTreeMap<PathBuf, File>,
    extra_sources: &BTreeMap<PathBuf, String>,
    paths: &[PathBuf],
    persistent: bool,
    server: &mut Option<crate::checker::ProjectChecker>,
    capabilities: Option<&Value>,
    memory_projection: bool,
) -> Result<Vec<Value>> {
    if !persistent {
        return check(binary, root, config, paths);
    }
    let mut sources: BTreeMap<_, _> = units
        .iter()
        .map(|(path, unit)| (path.clone(), unit.rendered.as_ref().unwrap().text.clone()))
        .collect();
    sources.extend(
        extra_sources
            .iter()
            .map(|(path, text)| (path.clone(), text.clone())),
    );
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
            projection_checker(binary, root, config, &sources, memory_projection)?
        });
    }
    server.as_mut().unwrap().check_files(&sources, paths)
}

fn projection_checker(
    binary: &Path,
    view: &Path,
    config: &Path,
    sources: &BTreeMap<PathBuf, String>,
    memory_projection: bool,
) -> Result<crate::checker::ProjectChecker> {
    let out = config.parent().context("generated checker workspace")?;
    if memory_projection {
        crate::checker::ProjectChecker::new_with_memory_projection(
            binary, view, out, config, sources,
        )
    } else {
        crate::checker::ProjectChecker::new_with_disk_projection(binary, out, config, sources)
    }
}

fn diagnostic_origins(
    unit: &File,
    project: &Project,
    generated: &GeneratedDeclarations,
    virtual_scope: Option<&str>,
    virtual_names: &Arc<BTreeMap<String, String>>,
    nominal_names: &Arc<BTreeMap<String, String>>,
) -> DiagnosticOrigins {
    use crate::model::{expression_key, span};
    use ruff_python_ast::{
        Expr, Stmt,
        visitor::{self, Visitor},
    };
    let mut origins = DiagnosticOrigins {
        virtual_names: virtual_names.clone(),
        nominal_names: nominal_names.clone(),
        virtual_scope: virtual_scope.map(str::to_owned),
        ..DiagnosticOrigins::default()
    };
    for (span, class) in &unit.analysis.constructor_calls {
        if generated.constructors.contains(class) {
            origins.constructor_calls.insert(*span, class.clone());
        }
    }
    struct Assignments<'a> {
        unit: &'a File,
        project: &'a Project,
        generated: &'a GeneratedDeclarations,
        sites: Vec<DescriptorAssignment>,
        annotated_owners: BTreeMap<String, String>,
    }
    impl Assignments<'_> {
        fn owner(&self, expression: &Expr) -> Option<String> {
            if let Expr::Call(call) = expression {
                // The call collector has already resolved imports and rejected
                // shadowed constructor names. Its full callee identity proves
                // this constructor creates the projected class.
                let owner = self
                    .unit
                    .analysis
                    .constructor_calls
                    .get(&span(call))?
                    .clone();
                if self.project.classes.contains_key(&owner) {
                    return Some(owner);
                }
            }
            if let Expr::Name(name) = expression
                && let Some(owner) = self.annotated_owners.get(name.id.as_str())
            {
                let inferred = self.unit.oracle.get(&expression_key(span(expression)));
                if inferred.is_none_or(|inferred| {
                    inferred == owner || owner.rsplit('.').next() == Some(inferred)
                }) {
                    return Some(owner.clone());
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
        fn annotation(&self, expression: &Expr) -> Option<String> {
            if !matches!(expression, Expr::Name(_) | Expr::Attribute(_)) {
                return None;
            }
            let owner = self.project.canonical_name(
                &self.unit.source.module,
                self.unit.source.slice(span(expression)),
            );
            self.project.classes.contains_key(&owner).then_some(owner)
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
                Stmt::FunctionDef(function) => {
                    let previous = std::mem::take(&mut self.annotated_owners);
                    for parameter in function.parameters.iter_non_variadic_params() {
                        if let Some(owner) = parameter
                            .parameter
                            .annotation
                            .as_ref()
                            .and_then(|annotation| self.annotation(annotation))
                        {
                            self.annotated_owners
                                .insert(parameter.parameter.name.to_string(), owner);
                        }
                    }
                    self.visit_body(&function.body);
                    self.annotated_owners = previous;
                    return;
                }
                Stmt::Assign(assignment) => {
                    for target in &assignment.targets {
                        self.assignment(target, &assignment.value);
                        if let Expr::Name(name) = target {
                            self.annotated_owners.remove(name.id.as_str());
                        }
                    }
                }
                Stmt::AnnAssign(assignment) => {
                    if let Some(value) = &assignment.value {
                        self.assignment(&assignment.target, value);
                    }
                    if let Expr::Name(name) = assignment.target.as_ref() {
                        self.annotated_owners.remove(name.id.as_str());
                        if let Some(owner) = self.annotation(&assignment.annotation) {
                            self.annotated_owners.insert(name.id.to_string(), owner);
                        }
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
        annotated_owners: BTreeMap::new(),
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
    canonical_snapshot: bool,
) -> Result<BTreeSet<PathBuf>> {
    let known_canonical = canonical_snapshot.then(|| {
        project
            .modules
            .values()
            .filter(|module| !module.external)
            .map(|module| module.source.path.as_str())
            .collect::<std::collections::HashSet<_>>()
    });
    let prepared_selection = selection.prepare_paths(
        project
            .modules
            .values()
            .filter(|module| !module.external)
            .map(|module| Path::new(&module.source.path)),
    );
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
                    || (prepared_selection.includes(Path::new(&m.source.path), &m.source.text)
                        && !ignored.contains(Path::new(&m.source.path)))
            })
            .map(|m| PathBuf::from(&m.source.path))
            .collect()
    };
    let mut canonical_reuses = 0;
    let selected: BTreeSet<_> = selected
        .into_iter()
        .map(|p| {
            if known_canonical
                .as_ref()
                .is_some_and(|known| p.to_str().is_some_and(|text| known.contains(text)))
            {
                canonical_reuses += 1;
                Ok(p)
            } else if p.exists() {
                p.canonicalize()
            } else {
                Ok(p)
            }
        })
        .collect::<std::io::Result<_>>()?;
    if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
        eprintln!("companion selected-path canonical reuses: {canonical_reuses}");
    }
    Ok(selected)
}

fn editor_configuration(options: &BTreeMap<String, String>) -> Result<Value> {
    let read = |key: &str| -> Result<Value> {
        Ok(options
            .get(key)
            .map(|text| serde_json::from_str(text))
            .transpose()?
            .unwrap_or(Value::Null))
    };
    Ok(crate::editor_options::effective(
        &read("lsp-initialization-options")?,
        &read("lsp-workspace-configuration")?,
    ))
}

fn editor_backend_configuration(
    options: &BTreeMap<String, String>,
    configuration: Option<&Value>,
) -> Result<Value> {
    let mut settings = crate::editor_options::backend(&editor_configuration(options)?);
    if let Some(configuration) = configuration {
        settings["configuration"] = configuration.clone();
    }
    Ok(settings)
}

/// A reusable analysis session. Original files are never written, including unsaved buffers.
pub struct Session {
    root: PathBuf,
    out: PathBuf,
    view: PathBuf,
    stubs: PathBuf,
    stub_roots: Vec<PathBuf>,
    overlay_files: BTreeSet<PathBuf>,
    borrowed_overlay: Option<PathBuf>,
    options: BTreeMap<String, String>,
    mode: String,
    runtime: Runtime,
    base_project: Project,
    server: Option<crate::checker::ProjectChecker>,
    rich_server: Option<crate::checker::ProjectChecker>,
    rich_result: Option<(usize, Vec<Value>)>,
    documents: Vec<ProjectDocument>,
    virtual_files: BTreeSet<PathBuf>,
    support_files: BTreeMap<PathBuf, String>,
    checker_config_path: PathBuf,
    config: Value,
    capabilities: Option<Value>,
    last_result: Option<Value>,
    revisions: usize,
    config_source: Option<PathBuf>,
    config_diagnostics: Vec<Value>,
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
    cached_context: Option<Vec<u8>>,
    plugin_files: Vec<PathBuf>,
    dependency_roots: Vec<PathBuf>,
    index_files: Option<BTreeSet<PathBuf>>,
    memory_projection: bool,
    native_warmup: Option<crate::native_warmup::NativeWarmup>,
    native_cli: Option<NativeCliCheck>,
    native_cli_preparation: Option<PendingNativeCliPreparation>,
    native_warmup_reused: bool,
    native_warmup_seeds: usize,
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
        Self::new_with_checker(input, options, None)
    }

    pub fn new_with_checker(
        input: &Path,
        options: &BTreeMap<String, String>,
        checker: Option<crate::checker::ProjectChecker>,
    ) -> Result<Self> {
        let mut startup_phases = BTreeMap::new();
        let mut startup_tick = Instant::now();
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
        // Include explicit selections in transport eligibility. A notebook in
        // a hidden directory is absent from root discovery but still needs the
        // existing disk projection. Retain path errors for their original
        // position after configuration validation.
        let explicit_paths: Result<Vec<PathBuf>> = (|| {
            Ok(options
                .get("explicit-files")
                .map(|s| serde_json::from_str::<Vec<PathBuf>>(s))
                .transpose()?
                .unwrap_or_default()
                .into_iter()
                .map(|path| path.canonicalize())
                .collect::<std::io::Result<Vec<_>>>()?)
        })();
        let mut source_paths = Vec::new();
        let mut canonical_sources = std::collections::HashSet::new();
        let memory_requested = !cfg!(windows)
            && options
                .get("memory-projection")
                .is_some_and(|v| v == "true")
            && !options.contains_key("lsp-capabilities")
            && checker.is_none()
            && explicit_paths.is_ok()
            && project_config::supports_memory_projection(&root, options)?;
        if memory_requested {
            snapshot_files(&root, &mut source_paths, &mut canonical_sources)?;
            if let Ok(paths) = &explicit_paths {
                for path in paths {
                    if path == &root {
                        continue;
                    }
                    if path.is_dir() {
                        snapshot_files(path, &mut source_paths, &mut canonical_sources)?;
                    } else {
                        source_paths.push(path.clone());
                        canonical_sources.insert(path.clone());
                    }
                }
            }
        }
        let memory_projection = memory_requested
            && !root.to_string_lossy().contains('\\')
            && source_paths.iter().all(|path| {
                !path.to_string_lossy().contains('\\')
                    && path.extension().is_none_or(|ext| ext != "ipynb")
            });
        let view = if memory_projection {
            root.clone()
        } else {
            out.join("project")
        };
        let stubs = out.join("stubs");
        ensure!(
            (memory_projection || !view.exists()) && !stubs.exists(),
            "generated project or stubs already exist; use a fresh --out directory"
        );
        if !memory_projection {
            std::fs::create_dir_all(&view)?;
        }
        std::fs::create_dir_all(&stubs)?;
        if !memory_projection {
            source_paths = copy_python(&root, &view)?;
        }
        phase(
            &mut startup_phases,
            "startup_source_copy",
            &mut startup_tick,
        );
        let config: Value = if let Some(path) = options.get("config") {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            json!({})
        };
        let mut checker_config = project_config::load(&root, &view, &stubs, options)?;
        let checker_config_path = out.join("checker.toml");
        write(&checker_config_path, &checker_config.text)?;
        phase(
            &mut startup_phases,
            "startup_configuration",
            &mut startup_tick,
        );
        let mut runtime = Runtime::from_plugins(&checker_config.plugins)?;
        if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
            eprintln!("companion native fields: {}", runtime.use_site_fields);
        }
        phase(&mut startup_phases, "startup_runtime", &mut startup_tick);
        let stdlib = stdlib_overlay_modules(
            &checker_config,
            options,
            &out,
            &runtime.third_party_overlay_modules()?,
        )?;
        let borrow_requested = options.get("borrow-overlays").map_or_else(
            || {
                std::env::var_os("TY_COMPANION_BORROW_OVERLAYS")
                    .map_or(runtime.use_site_fields, |v| v == "1")
            },
            |value| value == "true",
        );
        let borrowed_overlay = (borrow_requested
            && memory_projection
            && options.get("public-cli").is_some_and(|v| v == "true")
            && checker_config.stub_roots.len() == 1
            && stdlib.is_empty()
            && !checker_config.stub_roots[0].starts_with(&out)
            && !out.starts_with(&checker_config.stub_roots[0])
            && checker_config.source_roots.iter().all(|source_root| {
                !source_root.starts_with(&checker_config.stub_roots[0])
                    && !checker_config.stub_roots[0].starts_with(source_root)
            })
            && checker_config.stub_roots[0]
                .to_str()
                .is_some_and(|s| !s.contains('\\')))
        .then(|| checker_config.stub_roots[0].clone());
        if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
            eprintln!("companion borrowed overlay: {}", borrowed_overlay.is_some());
        }
        let overlay_files = if let Some(provider) = &borrowed_overlay {
            // No package is copied into the support directory. Native imports
            // keep the complete provider tree and patches use its original URI.
            let mut document: toml::Value = toml::from_str(&checker_config.text)?;
            let extras = document["environment"]["extra-paths"]
                .as_array_mut()
                .context("native overlay extra paths")?;
            extras.push(toml::Value::String(provider.to_string_lossy().into_owned()));
            checker_config.text = toml::to_string(&document)?;
            write(&checker_config_path, &checker_config.text)?;
            BTreeSet::new()
        } else {
            copy_overlays(&checker_config.stub_roots, &stubs, &stdlib)?
        };
        phase(&mut startup_phases, "startup_overlays", &mut startup_tick);
        runtime.python_version = checker_config.python_version;
        runtime.platform = checker_config.platform;
        let capabilities: Option<Value> = options
            .get("lsp-capabilities")
            .map(|s| serde_json::from_str(s))
            .transpose()?;
        runtime.eager_class_fields = capabilities.is_some();
        let mut ignored = checker_config.selection.ignored_paths();
        let force_exclude = options.get("force-exclude").is_some_and(|v| v == "true");
        let mut explicit_files = BTreeSet::new();
        let explicit_paths = explicit_paths?;
        for path in &explicit_paths {
            if path.is_dir() && (!memory_projection || path != &root) {
                // Memory discovery already resolved ignore rules for this root.
                ignored.extend(checker_config.selection.ignored_paths_under(path));
            }
            if !path.is_dir() {
                explicit_files.insert(path.clone());
            }
        }
        if !memory_projection {
            for path in &explicit_paths {
                if path.is_dir() {
                    files(path, &mut source_paths)?;
                } else {
                    source_paths.push(path.clone());
                }
            }
        }
        source_paths.sort();
        source_paths.dedup();
        let prepared_selection = checker_config
            .selection
            .prepare_paths(source_paths.iter().map(PathBuf::as_path));
        let selected_sources = crate::parallel::map_ordered(
            &source_paths,
            crate::parallel::workers(source_paths.len()),
            |path| match checker_config.selection.selection_text(path) {
                Ok(text)
                    if (explicit_files.contains(path)
                        && prepared_selection.includes_explicit(path, &text, force_exclude)
                        && (!force_exclude || !ignored.contains(path)))
                        || (prepared_selection.includes(path, &text)
                            && !ignored.contains(path)) =>
                {
                    Ok(Some(path.clone()))
                }
                Ok(_) => Ok(None),
                Err(error) => Err(error),
            },
        )?;
        let source_paths = selected_sources
            .into_iter()
            .filter_map(std::result::Result::transpose)
            .collect::<std::io::Result<Vec<_>>>()?;
        phase(&mut startup_phases, "startup_selection", &mut startup_tick);
        // Opt-in while the concurrent path is qualified. Snapshot loading and
        // native disk warmup overlap; the real check still synchronizes the
        // loaded immutable source text. Editors retain normal loading.
        let warmup_requested = options.get("native-warmup").map_or_else(
            || {
                std::env::var_os("TY_COMPANION_NATIVE_WARMUP")
                    .map_or(runtime.use_site_fields, |v| v == "1")
            },
            |value| value == "true",
        );
        let preload_requested = warmup_requested
            && memory_projection
            && !checker_config.plugins.is_empty()
            && options.get("public-cli").is_some_and(|v| v == "true");
        let canonical_sources = Arc::new(canonical_sources);
        let prefer_native_cli = preload_requested && runtime.prefers_native_cli();
        let mut native_cli_preparation = if prefer_native_cli {
            PendingNativeCliPreparation::start(
                &root,
                &checker_config_path,
                &source_paths,
                &checker_config.stub_roots,
            )
            .ok()
        } else {
            None
        };
        let native_warmup = if preload_requested
            && !prefer_native_cli
            && !source_paths.is_empty()
            && source_paths.iter().all(|path| path.starts_with(&root))
        {
            crate::native_warmup::NativeWarmup::start(
                Path::new(required("ty")?),
                &root,
                &out,
                &checker_config_path,
                source_paths.clone(),
                borrowed_overlay.iter().cloned().collect(),
                Arc::clone(&canonical_sources),
            )
            .ok()
        } else {
            None
        };
        let mut native_cli = None;
        if prefer_native_cli
            && runtime.prepares_project_contributions_early()
            && let Some(provider) = &borrowed_overlay
        {
            // Only settings are needed to prepare provider contributions.
            // Full source and provider loading starts after native Ty does.
            let setup = (|| -> Result<Option<NativeCliCheck>> {
                let mut project = Project::load_selected(
                    &root,
                    &[],
                    &runtime.manifest,
                    config.clone(),
                    true,
                    crate::project::SourceLayout {
                        roots: checker_config.source_roots.clone(),
                        overlays: &[],
                        include: &|_| true,
                        sparse_overlays: true,
                        call_assignments_only: runtime.project_call_assignments_only()?,
                        capture_original_revisions: true,
                        known_source_revisions: None,
                        preloaded: None,
                    },
                )?;
                runtime.attach_project(Arc::new(project.clone()), config.clone());
                let mut request = project.build_request_for_files(None);
                request["context"]["python-version"] = json!(runtime.python_version);
                request["context"]["platform"] = json!(runtime.platform);
                Self::start_early_native_cli(
                    &mut runtime,
                    &mut project,
                    &request,
                    &source_paths.iter().cloned().collect(),
                    &root,
                    &out,
                    &checker_config_path,
                    Path::new(required("ty")?),
                    provider,
                    native_cli_preparation.take(),
                    true,
                )
            })();
            native_cli = setup.ok().flatten();
        }
        phase(
            &mut startup_phases,
            "startup_native_setup",
            &mut startup_tick,
        );
        let preloaded = (preload_requested && native_cli.is_none())
            .then(|| Project::preload_sources(&root, &source_paths, Some(&canonical_sources)))
            .flatten()
            .map(Arc::new);
        let mut base_project = Project::load_selected(
            &root,
            &source_paths,
            &runtime.manifest,
            config.clone(),
            capabilities.is_some()
                || options
                    .get("public-cli")
                    .is_some_and(|value| value == "true"),
            crate::project::SourceLayout {
                roots: checker_config.source_roots,
                overlays: &checker_config.stub_roots,
                include: &|relative| include_overlay(relative, &stdlib),
                // Fresh CLI memory projection has a single immutable input
                // snapshot. Editors retain the complete index across reloads.
                sparse_overlays: memory_projection && runtime.sparse_overlay_index()?,
                call_assignments_only: runtime.project_call_assignments_only()?,
                capture_original_revisions: memory_projection
                    && options
                        .get("public-cli")
                        .is_some_and(|value| value == "true"),
                known_source_revisions: native_cli
                    .as_ref()
                    .map(|worker| worker.file_revisions.as_ref()),
                preloaded: preloaded.as_deref(),
            },
        )?;
        if native_cli
            .as_mut()
            .is_some_and(|worker| !worker.observe_project(&base_project))
        {
            native_cli = None;
        }
        base_project.add_source_paths(explicit_paths.clone());
        phase(
            &mut startup_phases,
            "startup_source_and_stub_index",
            &mut startup_tick,
        );
        let server = if let Some(mut checker) = checker {
            checker.set_workspace_configuration(editor_backend_configuration(
                options,
                checker_config.editor_configuration.as_ref(),
            )?);
            Some(checker)
        } else if let Some(capabilities) = &capabilities {
            Some(
                crate::checker::ProjectChecker::new_editor_with_configuration(
                    Path::new(required("ty")?),
                    &out,
                    &checker_config_path,
                    capabilities.clone(),
                    options
                        .get("lsp-initialization-options")
                        .map(|s| serde_json::from_str(s))
                        .transpose()?
                        .unwrap_or(json!({})),
                    Some(editor_backend_configuration(
                        options,
                        checker_config.editor_configuration.as_ref(),
                    )?),
                )?,
            )
        } else {
            None
        };
        let plugin_files = checker_config.plugin_watch_paths;
        phase(&mut startup_phases, "startup_project", &mut startup_tick);
        Ok(Self {
            root,
            out,
            view,
            memory_projection,
            native_warmup,
            native_cli,
            native_cli_preparation,
            native_warmup_reused: false,
            native_warmup_seeds: 0,
            stubs,
            stub_roots: checker_config.stub_roots,
            overlay_files,
            borrowed_overlay,
            options: options.clone(),
            mode: mode.into(),
            runtime,
            base_project,
            server,
            rich_server: None,
            rich_result: None,
            documents: Vec::new(),
            virtual_files: BTreeSet::new(),
            support_files: BTreeMap::new(),
            checker_config_path,
            config,
            capabilities,
            last_result: None,
            revisions: 0,
            config_source: checker_config.source,
            config_diagnostics: checker_config.plugin_diagnostics,
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
            index_files: None,
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

    fn finish_native_warmup(&mut self, reuse: bool) {
        if !reuse && self.native_warmup_reused {
            self.server = None;
            self.native_warmup_reused = false;
        }
        if let Some(warmup) = self.native_warmup.take() {
            match warmup.finish() {
                Ok(mut checker) if reuse => {
                    self.native_warmup_seeds = checker.seed_original_sources(
                        self.base_project
                            .modules
                            .values()
                            .filter(|module| !module.external)
                            .map(|module| {
                                (
                                    Path::new(&module.source.path),
                                    module.source.text.as_str(),
                                    module.original_revision,
                                )
                            }),
                    );
                    self.server = Some(checker);
                    self.native_warmup_reused = true;
                }
                // Native failures retain the existing fresh-checker path.
                // Source/index discovery uses a different probe namespace.
                _ => self.native_warmup_reused = false,
            }
        }
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
        let mut raw = server.check_files(&sources, &paths)?;
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
    pub fn generated_root(&self) -> &Path {
        &self.out
    }
    pub fn original_root(&self) -> &Path {
        &self.root
    }
    pub fn projected_root(&self) -> &Path {
        &self.view
    }
    pub fn projected_stubs(&self) -> &Path {
        &self.stubs
    }
    pub fn checker_mut(&mut self) -> Option<&mut crate::checker::ProjectChecker> {
        self.server.as_mut()
    }

    pub fn set_editor_configuration(&mut self, configuration: Value) -> Result<bool> {
        let mut previous = editor_configuration(&self.options)?;
        self.options.insert(
            "lsp-workspace-configuration".into(),
            configuration.to_string(),
        );
        let mut current = editor_configuration(&self.options)?;
        // These restrictions belong to the outer editor; the native oracle
        // always keeps diagnostics and language services enabled for inference.
        for key in [
            "diagnosticMode",
            "showSyntaxErrors",
            "disableLanguageServices",
        ] {
            previous.as_object_mut().unwrap().remove(key);
            current.as_object_mut().unwrap().remove(key);
        }
        if previous == current {
            return Ok(false);
        }
        self.invalidate();
        // The pinned native server applies settings only during workspace
        // initialization. A changed configuration therefore starts a new
        // checker; unchanged replies retain the process and semantic caches.
        self.server = None;
        Ok(true)
    }

    /// Update the native editor's checked-file set without discarding closed
    /// modules needed for imports, inheritance, settings and later openings.
    pub fn set_index_files(&mut self, files: Option<BTreeSet<PathBuf>>) {
        if self.index_files != files {
            self.index_files = files;
            self.last_result = None;
            self.rich_result = None;
            self.cached_context = None;
        }
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
        Arc::make_mut(&mut self.base_project.unavailable_modules).clear();
    }

    fn reload_dependencies(&mut self) -> Result<()> {
        // Persistent/reconfigured checks restore the ordinary copied view.
        self.borrowed_overlay = None;
        let config = project_config::load(&self.root, &self.view, &self.stubs, &self.options)?;
        let runtime = Runtime::from_plugins(&config.plugins)?;
        let stdlib = stdlib_overlay_modules(
            &config,
            &self.options,
            &self.out,
            &runtime.third_party_overlay_modules()?,
        )?;
        if !self.stub_roots.is_empty() || !config.stub_roots.is_empty() {
            // Package markers and new/removed overlay directories alter resolver
            // search state, not just document text. Rebuild only on explicit
            // dependency invalidation; normal editor revisions retain the child.
            self.server = None;
        }
        let configuration_changed =
            std::fs::read_to_string(&self.checker_config_path)? != config.text;
        if configuration_changed || (self.capabilities.is_some() && self.server.is_none()) {
            write(&self.checker_config_path, &config.text)?;
            self.server = if let Some(capabilities) = &self.capabilities {
                Some(
                    crate::checker::ProjectChecker::new_editor_with_configuration(
                        Path::new(&self.options["ty"]),
                        &self.out,
                        &self.checker_config_path,
                        capabilities.clone(),
                        self.options
                            .get("lsp-initialization-options")
                            .map(|s| serde_json::from_str(s))
                            .transpose()?
                            .unwrap_or(json!({})),
                        Some(editor_backend_configuration(
                            &self.options,
                            config.editor_configuration.as_ref(),
                        )?),
                    )?,
                )
            } else {
                None
            };
        }
        self.terminal = config.terminal;
        self.config_source = config.source;
        self.config_diagnostics = config.plugin_diagnostics;
        self.config = if let Some(path) = self.options.get("config") {
            serde_json::from_slice(&std::fs::read(path)?)?
        } else {
            json!({})
        };
        self.runtime = runtime;
        self.plugin_files = config.plugin_watch_paths;
        self.has_plugins = !config.plugins.is_empty();
        if !self.has_plugins {
            for path in std::mem::take(&mut self.virtual_files) {
                if path.exists() {
                    std::fs::remove_file(path)?;
                }
            }
            for path in std::mem::take(&mut self.support_files).into_keys() {
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
            self.base_project
                .add_source_tree_filtered(root, |relative| include_overlay(relative, &stdlib))?;
        }
        let current: BTreeSet<_> = self
            .base_project
            .modules
            .values()
            .filter(|m| m.external)
            .map(|m| PathBuf::from(&m.source.path))
            .collect();
        for removed in previous.difference(&current) {
            let generated = projected_stub_path(
                &self.stubs,
                &self.stub_roots,
                &self.resolved_paths,
                removed,
                self.borrowed_overlay.as_deref(),
            )?;
            if generated.starts_with(&self.out) && generated.exists() {
                std::fs::remove_file(generated)?;
            }
        }
        self.stub_roots = config.stub_roots;
        self.dependency_roots = config.dependency_roots;
        // Markers matter too: deleting py.typed must remove partial-package
        // fallback instead of leaving yesterday's copied marker behind.
        for previous in std::mem::take(&mut self.overlay_files) {
            if previous.is_file() {
                std::fs::remove_file(previous)?;
            }
        }
        self.overlay_files = copy_overlays(&self.stub_roots, &self.stubs, &stdlib)?;
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
            self.finish_native_warmup(false);
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
                self.server = Some(projection_checker(
                    Path::new(self.options.get("ty").context("missing --ty")?),
                    &self.view,
                    &self.checker_config_path,
                    &sources,
                    self.memory_projection,
                )?);
            }
            self.register_checker_notebooks()?;
            let server = self.server.as_mut().unwrap();
            for path in sources.keys().filter(|path| !path.starts_with(&self.out)) {
                server.register_external_source(path)?;
            }
            server.sync_documents(&sources)?;
            let uri = crate::checker::file_uri(&probe)?;
            let mut resolved = Vec::new();
            let mut resolved_paths = self.resolved_paths.clone();
            for (line, module) in candidates.iter().enumerate() {
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
                let Some(path) = resolved_import_path(
                    uri,
                    &self.view,
                    &self.stubs,
                    &self.stub_roots,
                    &resolved_paths,
                )?
                else {
                    continue;
                };
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
                resolved.push((module.clone(), path.clone()));
                resolved_paths.insert(path.clone(), relative.clone());
            }
            self.base_project.add_resolved_modules(&resolved)?;
            self.resolved_imports.extend(candidates);
            self.resolved_paths = resolved_paths;
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
            false,
        )?;
        let mut documents = Vec::new();
        let mut sources = BTreeMap::new();
        let mut paths = Vec::new();
        self.runtime
            .attach_project(Arc::new(self.base_project.clone()), self.config.clone());
        for module in self.base_project.modules.values() {
            let path = if module.external {
                projected_stub_path(
                    &self.stubs,
                    &self.stub_roots,
                    &self.resolved_paths,
                    Path::new(&module.source.path),
                    self.borrowed_overlay.as_deref(),
                )?
            } else {
                projected_source_path(&self.root, &self.view, Path::new(&module.source.path))
            };
            let mut edits = crate::mapping::Edits::default();
            for span in
                crate::lower::annotated_cast_spans(&module.source, &module.suite, &self.runtime)
            {
                edits.suppress_diagnostic(span, "redundant-cast".into());
            }
            let rendered = render(&module.source, &edits)?;
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
        let mut raw = if self.memory_projection {
            self.server = Some(crate::checker::ProjectChecker::new_with_memory_projection(
                &binary,
                &self.root,
                &self.out,
                &self.checker_config_path,
                &sources,
            )?);
            self.server.as_mut().unwrap().seed_original_sources(
                self.base_project.modules.values().map(|module| {
                    (
                        Path::new(&module.source.path),
                        module.source.text.as_str(),
                        module.original_revision,
                    )
                }),
            );
            self.server
                .as_mut()
                .unwrap()
                .check_workspace(&sources, &paths)?
        } else if persistent {
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
            server.check_files(&sources, &paths)?
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
        let mut diagnostics = self.config_diagnostics.clone();
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

    fn infer_index_functions(&mut self) -> Result<()> {
        if self.runtime.manifest.pointer("/capabilities/project-index") != Some(&Value::Bool(true))
        {
            return Ok(());
        }
        let queries = self.base_project.index_function_queries();
        if queries.is_empty() {
            return Ok(());
        }
        self.finish_native_warmup(false);
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
        if self.server.is_none() {
            self.server = Some(projection_checker(
                Path::new(self.options.get("ty").context("missing --ty")?),
                &self.view,
                &self.checker_config_path,
                &sources,
                self.memory_projection,
            )?);
        }
        self.register_checker_notebooks()?;
        let server = self.server.as_mut().unwrap();
        for path in sources.keys().filter(|path| !path.starts_with(&self.out)) {
            server.register_external_source(path)?;
        }
        server.sync_documents(&sources)?;
        self.runtime
            .attach_project(Arc::new(self.base_project.clone()), self.config.clone());
        let mut types = BTreeMap::new();
        for (index, source, offset) in queries {
            let path = projected_source_path(&self.root, &self.view, Path::new(&source.path));
            let position = source.position(offset);
            let params = json!({"textDocument":{"uri":crate::checker::file_uri(&path)?},"position":{"line":position["line"].as_u64().unwrap()-1,"character":position["column"].as_u64().unwrap()-1}});
            let hover = server.request("textDocument/hover", params.clone())?;
            if let Some(ty) = hover_type(&hover).filter(|ty| !ty.is_empty()) {
                let mut value = self.runtime.type_expr(&source, ty);
                // A decorator can replace a function with an instance whose
                // class is not imported in this module. Ask ty for its identity
                // instead of resolving a display name against lexical imports.
                if value.pointer("/snapshot/kind").and_then(Value::as_str) == Some("nominal") {
                    let definitions = server.request("textDocument/typeDefinition", params)?;
                    if let Some([definition]) = definitions.as_array().map(Vec::as_slice) {
                        let uri = definition["targetUri"]
                            .as_str()
                            .or_else(|| definition["uri"].as_str());
                        let range = definition
                            .get("targetSelectionRange")
                            .unwrap_or(&definition["range"]);
                        if let Some(uri) = uri
                            && let Some(target) = crate::lsp_mapping::decode_file_uri(uri)?
                        {
                            let original = target.strip_prefix(&self.view).map_or_else(
                                |_| target.clone(),
                                |relative| self.root.join(relative),
                            );
                            if let (Some(line), Some(character)) = (
                                range["start"]["line"].as_u64(),
                                range["start"]["character"].as_u64(),
                            ) && let Some(name) = self
                                .base_project
                                .class_identity_at(&original, line, character)
                            {
                                value["snapshot"]["qualified-name"] = json!(name);
                            }
                        }
                    }
                }
                types.insert(index, value);
            }
        }
        self.base_project.set_index_function_types(types);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn start_early_native_cli(
        runtime: &mut Runtime,
        project: &mut Project,
        request: &Value,
        selected: &BTreeSet<PathBuf>,
        root: &Path,
        out: &Path,
        config: &Path,
        binary: &Path,
        provider: &Path,
        preparation: Option<PendingNativeCliPreparation>,
        before_source_loading: bool,
    ) -> Result<Option<NativeCliCheck>> {
        // Authors opt into an extra ordinary project-index call containing
        // settings and context only. Full model indexing still runs normally.
        let early = json!({"kind":"build-project-index", "context":request["context"], "settings":request["settings"], "classes":[], "functions":[], "assignments":[]});
        let response = runtime.execute_ephemeral_project_index(early)?;
        let contributions = response["contributions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if before_source_loading {
            let Some(names) = contributions
                .iter()
                .map(|contribution| {
                    contribution
                        .pointer("/target/qualified-name")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .collect::<Option<Vec<_>>>()
            else {
                return Ok(None);
            };
            if !project.add_contribution_sources(provider, &names)? {
                return Ok(None);
            }
            runtime.attach_project(Arc::new(project.clone()), project.config.clone());
        }
        let mut targets = BTreeSet::new();
        for contribution in &contributions {
            let Some(name) = contribution
                .pointer("/target/qualified-name")
                .and_then(Value::as_str)
            else {
                return Ok(None);
            };
            let Some(class) = project.class_summary(name) else {
                return Ok(None);
            };
            let Some(module) = class.pointer("/source/module").and_then(Value::as_str) else {
                return Ok(None);
            };
            let Some(source) = project.modules.get(module) else {
                return Ok(None);
            };
            if !source.external || !Path::new(&source.source.path).starts_with(provider) {
                return Ok(None);
            }
            targets.insert(module.to_owned());
        }
        runtime.set_project_contributions(contributions);
        let mut projected = BTreeMap::new();
        let mut modules = BTreeSet::from(["builtins".to_owned(), "typing".to_owned()]);
        if before_source_loading {
            modules.extend(
                project.selected_package_imports(&selected.iter().cloned().collect::<Vec<_>>())?,
            );
            fn claim_imports(value: &Value, provider: &Path, modules: &mut BTreeSet<String>) {
                match value {
                    Value::String(name) => {
                        let mut prefix = name.as_str();
                        loop {
                            let base = provider.join(prefix.replace('.', "/"));
                            if [
                                base.with_extension("pyi"),
                                base.with_extension("py"),
                                base.join("__init__.pyi"),
                                base.join("__init__.py"),
                            ]
                            .iter()
                            .any(|path| path.is_file())
                            {
                                modules.insert(prefix.to_owned());
                            }
                            let Some((parent, _)) = prefix.rsplit_once('.') else {
                                break;
                            };
                            prefix = parent;
                        }
                    }
                    Value::Array(values) => {
                        for value in values {
                            claim_imports(value, provider, modules);
                        }
                    }
                    Value::Object(values) => {
                        for value in values.values() {
                            claim_imports(value, provider, modules);
                        }
                    }
                    _ => {}
                }
            }
            claim_imports(&runtime.manifest["claims"], provider, &mut modules);
        }
        for module in project
            .modules
            .values()
            .filter(|module| !module.external && selected.contains(Path::new(&module.source.path)))
        {
            if !project.syntax_errors.contains(&module.source.module) {
                modules.extend(imports_from_suite(&module.suite));
            }
        }
        for name in targets {
            let module = &project.modules[&name];
            if project.syntax_errors.contains(&name) {
                return Ok(None);
            }
            let mut analysis =
                crate::static_project::analyze(&module.source, &module.suite, runtime)?;
            if !analysis.calls.is_empty() || !analysis.expression_probes.is_empty() {
                return Ok(None);
            }
            let flow = crate::flow::project_with_return_types(
                &module.source,
                &module.suite,
                &mut analysis,
                runtime,
                Some(&Oracle::new()),
                &ReturnPatches::new(),
                &BTreeMap::new(),
            )?;
            let types =
                Types::with_project(&format!("{}result_", module.source.prefix), &name, project);
            let mut edits = crate::build_edits(
                &module.source,
                &analysis,
                flow.edits,
                &ReturnPatches::new(),
                false,
            );
            edits.insert(analysis.prelude_offset, types.prelude());
            edits.insert(analysis.prelude_offset, flow.type_prelude);
            let text = render(&module.source, &edits)?.text;
            modules.extend(imports(
                &text,
                project.source_type(Path::new(&module.source.path)),
            )?);
            if text != module.source.text {
                projected.insert(PathBuf::from(&module.source.path), text);
            }
        }
        let code = modules
            .iter()
            .map(|module| format!("import {module}\n"))
            .collect::<String>();
        let extras = BTreeMap::from([(out.join("native-bootstrap-imports.py"), code)]);
        if pure_import_probe(&extras, &out.join("native-bootstrap-imports.py")).is_none() {
            return Ok(None);
        }
        let paths: Vec<_> = selected.iter().cloned().collect();
        let prepared = preparation.and_then(|pending| pending.finish(project));
        if before_source_loading && prepared.is_none() {
            return Ok(None);
        }
        NativeCliCheck::start(
            binary,
            root,
            config,
            &paths,
            project,
            &[provider.to_owned()],
            Some(NativeCliProjection {
                sources: &projected,
                extras: &extras,
                prepared: prepared.as_ref(),
                bootstrap_imports: true,
            }),
        )
    }

    pub fn check(&mut self, overlays: &BTreeMap<PathBuf, String>) -> Result<Value> {
        ensure!(
            !self.memory_projection || (self.revisions == 0 && overlays.is_empty()),
            "experimental memory projection supports one fresh check without editor overlays"
        );
        self.runtime.context_free_arguments.clear();
        let started = Instant::now();
        let mut phases = BTreeMap::new();
        let mut phase_started = started;
        let reloaded = self.invalidated;
        if reloaded {
            self.finish_native_warmup(false);
            self.reload_dependencies()?;
        }
        self.register_checker_notebooks()?;
        let executions_before = self.runtime.executions;
        let hits_before = self.runtime.hits;
        let monty_before = self.runtime.execution_time;
        let frozen_cli_snapshot = self.memory_projection
            && self.revisions == 0
            && !reloaded
            && self
                .options
                .get("public-cli")
                .is_some_and(|value| value == "true");
        // A fresh CLI check already loaded its complete source snapshot. Keep
        // that snapshot through the single memory-projection check; editors
        // and persistent sessions continue to observe filesystem changes here.
        let changed = if frozen_cli_snapshot {
            false
        } else {
            self.base_project
                .refresh_sources_filtered(overlays, |path, text| {
                    (self.explicit_files.contains(path)
                        && self
                            .selection
                            .includes_explicit(path, text, self.force_exclude)
                        && (!self.force_exclude || !self.ignored.contains(path)))
                        || (self.selection.includes(path, text) && !self.ignored.contains(path))
                })?
        };
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
            Arc::make_mut(&mut self.base_project.unavailable_modules).clear();
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
        self.infer_index_functions()?;
        phase(&mut phases, "function_summaries", &mut phase_started);
        let root = self.root.clone();
        let out = self.out.clone();
        let view = self.view.clone();
        let stubs = self.stubs.clone();
        let stub_roots = self.stub_roots.clone();
        let borrowed_overlay = self.borrowed_overlay.clone();
        let config = self.config.clone();
        let index_trace = std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some();
        let index_select_start = Instant::now();
        let mut project = self.base_project.clone();
        let selected = selected_paths(
            &project,
            &root,
            &self.options,
            &self.selection,
            &self.ignored,
            overlays,
            frozen_cli_snapshot,
        )?;
        let selected_lookup: std::collections::HashSet<_> =
            selected.iter().map(PathBuf::as_path).collect();
        self.runtime.selected_virtual_scope = project
            .modules
            .values()
            .filter(|module| selected_lookup.contains(Path::new(&module.source.path)))
            .filter(|module| module.source.protocol_module.is_none())
            .filter(|module| {
                self.index_files
                    .as_ref()
                    .is_none_or(|files| files.contains(Path::new(&module.source.path)))
            })
            .min_by_key(|module| &module.source.path)
            .map(|module| {
                module
                    .source
                    .protocol_module
                    .as_deref()
                    .unwrap_or(&module.source.module)
                    .to_owned()
            });
        if index_trace {
            eprintln!(
                "companion index selection: {:.6}s",
                index_select_start.elapsed().as_secs_f64()
            );
        }
        let index = {
            let attach_start = Instant::now();
            self.runtime.clear_project_patches();
            self.runtime.set_project_contributions(Vec::new());
            self.runtime
                .attach_project(Arc::new(project.clone()), config.clone());
            let mut request = project.build_request_for_files(self.index_files.as_ref());
            request["context"]["python-version"] = json!(self.runtime.python_version);
            request["context"]["platform"] = json!(self.runtime.platform);
            if frozen_cli_snapshot
                && self.server.is_none()
                && self.native_cli.is_none()
                && self.runtime.prepares_project_contributions_early()
                && let Some(provider) = &self.borrowed_overlay
            {
                self.native_cli = Self::start_early_native_cli(
                    &mut self.runtime,
                    &mut project,
                    &request,
                    &selected,
                    &self.root,
                    &self.out,
                    &self.checker_config_path,
                    Path::new(self.options.get("ty").context("official ty")?),
                    provider,
                    self.native_cli_preparation.take(),
                    false,
                )
                .ok()
                .flatten();
            }
            let runtime = &mut self.runtime;
            runtime.clear_project_patches();
            runtime.set_project_contributions(Vec::new());
            runtime.attach_project(Arc::new(project.clone()), config.clone());
            if index_trace {
                eprintln!(
                    "companion index attach: {:.6}s",
                    attach_start.elapsed().as_secs_f64()
                );
            }
            let request_start = Instant::now();
            if index_trace {
                eprintln!(
                    "companion index request: {:.6}s",
                    request_start.elapsed().as_secs_f64()
                );
            }
            if !self.options.contains_key("ephemeral-evidence") {
                write(
                    &out.join("project-request.json"),
                    &serde_json::to_string_pretty(&request)?,
                )?;
            }
            if frozen_cli_snapshot && self.options.contains_key("ephemeral-evidence") {
                runtime.execute_ephemeral_project_index(request)?
            } else {
                runtime.execute_project("build-project-index", request)?
            }
        };
        phase(&mut phases, "project_index", &mut phase_started);
        // Lowering and flow operate on immutable source snapshots. Keep the
        // disposable native request running until those in-memory passes end;
        // projection writes and checker requests wait below.
        if !frozen_cli_snapshot || !overlays.is_empty() {
            self.finish_native_warmup(false);
        }
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
        let project_diagnostics: Vec<_> = index["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        // The public CLI deletes its temporary view on exit. Avoid encoding
        // large debug copies that no caller can consume; explicit project
        // runs retain both protocol artifacts for investigation.
        if !options.contains_key("ephemeral-evidence") {
            write(
                &out.join("project-response.json"),
                &serde_json::to_string_pretty(&index)?,
            )?;
        }
        let mut contributions = index["contributions"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        // The checker retains the first contribution for each target/member pair.
        // Distinct source locations on identical settings contributions are normal.
        let mut contributed = BTreeSet::new();
        contributions.retain(|c| {
            contributed.insert((
                c["target"].to_string(),
                c["patch"]["name"].as_str().unwrap_or("__init__").to_owned(),
            ))
        });
        runtime.set_project_contributions(contributions);
        let definitions = index["virtual-types"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        runtime.set_virtual_shapes(
            definitions
                .iter()
                .filter_map(|definition| {
                    Some((
                        definition["name"].as_str()?.to_owned(),
                        definition["shape"].clone(),
                    ))
                })
                .collect(),
        );
        let mut virtual_projection = VirtualProjection {
            modules: project.prepare_virtual_types(&definitions)?,
            materialized: None,
        };
        runtime.requested_virtual_types.retain(|name| {
            project
                .classes
                .get(name)
                .is_some_and(|class| class["virtual"] == true)
        });
        runtime.attach_project(Arc::new(project.clone()), config);
        phase(&mut phases, "virtual_declarations", &mut phase_started);
        // Every model patch is available before flow analysis of importing modules.
        runtime.prepare_class_transforms(&project)?;
        for (name, summary) in project.classes.iter() {
            if runtime.keeps_native_class(name) {
                continue;
            }
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
                runtime.set_project_patch(name.clone(), response);
            }
        }
        phase(&mut phases, "class_projection", &mut phase_started);
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
        // Keep an exact canonical byte snapshot for cross-revision equality.
        // A second JSON object tree duplicates every class, patch and virtual
        // shape, including their allocation overhead, solely for this check.
        // Memory CLI checks have exactly one revision. Their incremental
        // comparison snapshot can never be reused; final-check equality below
        // still uses the complete, separate NativeCheckInputs value.
        let context = if frozen_cli_snapshot {
            None
        } else {
            Some(serde_json::to_vec(&(
                runtime.index_generations(),
                &index,
                &project.classes,
                &runtime.project_patches,
                &project.virtual_identities,
                &runtime.selected_virtual_scope,
            ))?)
        };
        let context_unchanged = context
            .as_ref()
            .is_some_and(|context| self.cached_context.as_ref() == Some(context));
        let module_set_unchanged = self.cached_sources.keys().eq(project.modules.keys());
        let mut dirty = if module_set_unchanged && context_unchanged {
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
        if !module_set_unchanged || !context_unchanged {
            Arc::make_mut(&mut project.snapshot_nominals).clear();
            Arc::make_mut(&mut project.snapshot_symbols).clear();
        } else {
            Arc::make_mut(&mut project.snapshot_nominals).retain(|name, _| {
                !dirty.iter().any(|module| {
                    name.strip_prefix(module)
                        .is_some_and(|tail| tail.starts_with('.'))
                })
            });
            Arc::make_mut(&mut project.snapshot_symbols).retain(|name, _| {
                !dirty.iter().any(|module| {
                    name.strip_prefix(module)
                        .is_some_and(|tail| tail.starts_with('.'))
                })
            });
        }
        runtime.nominal_candidates.clear();
        runtime.annotation_candidates.clear();
        runtime.attach_project(Arc::new(project.clone()), runtime.config.clone());
        let mut previous = std::mem::take(&mut self.cached_units);
        let mut units = Vec::with_capacity(project.modules.len());
        for (name, module) in project.modules.iter() {
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
                        borrowed_overlay.as_deref(),
                    )?
                }
            } else {
                projected_source_path(&root, &view, Path::new(&module.source.path))
            };
            let valid = !project.syntax_errors.contains(name);
            if !dirty.contains(name)
                && let Some(mut cached) = previous.remove(&path)
            {
                // Import/function preflights temporarily write original source.
                // Closed dependencies must regain their cached projection before
                // the checker reads them through a dirty importing module.
                if path.starts_with(&out)
                    && let Some(rendered) = &cached.rendered
                {
                    write(&path, &rendered.text)?;
                }
                cached.selected = selected.contains(Path::new(&module.source.path));
                cached.dirty = false;
                units.push((path, cached));
                continue;
            }
            let observations = runtime.protocol_observations();
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
            let passthrough = self.memory_projection
                && runtime.use_site_fields
                && valid
                && analysis.classes.is_empty()
                && analysis.calls.is_empty()
                && analysis.expression_probes.is_empty()
                && analysis.annotated_casts.is_empty()
                && analysis.metadata_probes.is_empty()
                && analysis.unbound_calls.is_empty()
                && analysis.support_modules.is_empty()
                && analysis.edits.is_empty();
            let rendered = passthrough
                .then(|| render(&module.source, &crate::mapping::Edits::default()))
                .transpose()?;
            units.push((
                path.clone(),
                File {
                    source: module.source.clone(),
                    suite: module.suite.clone(),
                    analysis,
                    path,
                    oracle: Oracle::new(),
                    patches: ReturnPatches::new(),
                    return_types: BTreeMap::new(),
                    early_returns: BTreeMap::new(),
                    types,
                    diagnostics: Vec::new(),
                    rendered,
                    selected: selected.contains(Path::new(&module.source.path)),
                    valid,
                    names: BTreeMap::new(),
                    final_edits: passthrough.then(crate::mapping::Edits::default),
                    dirty: true,
                    passthrough,
                    raw_diagnostics: Vec::new(),
                    flow_diagnostics: Vec::new(),
                    pass: None,
                    reads_protocol_aliases: runtime.protocol_observations() != observations,
                },
            ));
        }
        // Bulk construction sorts once and retains the last duplicate, like
        // ordered insertion, without walking long path prefixes for every item.
        let mut units: BTreeMap<_, _> = units.into_iter().collect();
        // An installed package retains its original resolution location. Only
        // the checker's in-memory document is changed, never dependency files.
        let mut source_cache_seeds = 0;
        let mut reused_native_warmup = frozen_cli_snapshot && self.native_warmup_reused;
        if !self.memory_projection
            && (!self.notebook_sources.is_empty()
                || units.keys().any(|path| !path.starts_with(&out)))
        {
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
        if frozen_cli_snapshot
            && self.native_cli.is_none()
            && runtime.prefers_native_cli()
            && server.is_none()
            && let Some(provider) = borrowed_overlay.as_ref()
        {
            let mut native_sources = BTreeMap::new();
            let mut eligible = true;
            for (path, unit) in &mut units {
                if path.starts_with(provider) && !unit.passthrough {
                    if !unit.valid
                        || !unit.analysis.calls.is_empty()
                        || !unit.analysis.expression_probes.is_empty()
                    {
                        eligible = false;
                        break;
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
                    let mut edits = crate::build_edits(
                        &unit.source,
                        &unit.analysis,
                        flow.edits,
                        &unit.patches,
                        false,
                    );
                    edits.insert(unit.analysis.prelude_offset, unit.types.prelude());
                    edits.insert(unit.analysis.prelude_offset, flow.type_prelude);
                    let projected = render(&unit.source, &edits)?.text;
                    if projected != unit.source.text {
                        native_sources.insert(path.clone(), projected);
                    }
                }
            }
            if eligible {
                let native_paths = units
                    .values()
                    .filter(|unit| unit.selected && unit.dirty)
                    .map(|unit| unit.path.clone())
                    .collect::<Vec<_>>();
                let mut modules = BTreeSet::new();
                for (path, unit) in &units {
                    if unit.valid && unit.dirty && !unit.passthrough {
                        modules.extend(imports(
                            native_sources.get(path).unwrap_or(&unit.source.text),
                            project.source_type(Path::new(&unit.source.path)),
                        )?);
                    }
                }
                let checked = project
                    .virtual_modules
                    .values()
                    .cloned()
                    .collect::<BTreeSet<_>>();
                let code = modules
                    .difference(&checked)
                    .map(|module| format!("import {module}\n"))
                    .collect::<String>();
                let extras = if code.is_empty() {
                    BTreeMap::new()
                } else {
                    BTreeMap::from([(out.join("type-imports.py"), code)])
                };
                let prepared = self
                    .native_cli_preparation
                    .take()
                    .and_then(|pending| pending.finish(&project));
                self.native_cli = NativeCliCheck::start(
                    &binary,
                    &root,
                    &checker_config_path,
                    &native_paths,
                    &project,
                    std::slice::from_ref(provider),
                    Some(NativeCliProjection {
                        sources: &native_sources,
                        extras: &extras,
                        prepared: prepared.as_ref(),
                        bootstrap_imports: false,
                    }),
                )
                .ok()
                .flatten();
            }
        }
        phase(&mut phases, "native_cli_prepare", &mut phase_started);
        let mut converged = false;
        let mut rounds = 0;
        let mut oracle_queries = 0;
        let mut analysis_passes = 0;
        let mut warmup_analysis_passes = 0;
        let mut hook_prefetch_seconds = 0.0;
        let mut reused_analysis_passes = 0;
        let mut reused_final_flows = 0;
        let mut checked_projection = None;
        let mut native_cli_reused = false;
        // These hygienic modules were just written into the checker's explicit
        // stub search path. Probing their existence would eagerly load every
        // generated model before any semantic query needs one.
        let mut checked_imports: BTreeSet<_> = project.virtual_modules.values().cloned().collect();
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
            // A rejected preflight or another inference pass invalidates the
            // entire earlier check, including diagnostics for unchanged files.
            checked_projection = None;
            rounds = iteration + 1;
            let mut pending_projections = Vec::new();
            for unit in units
                .values_mut()
                .filter(|unit| unit.dirty && !unit.passthrough)
            {
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
                    if self.native_warmup.is_some() {
                        if unit.path.starts_with(&out) {
                            pending_projections.push((unit.path.clone(), generated.text.clone()));
                        }
                    } else {
                        unit.write_projection(&generated, &out)?;
                    }
                    unit.rendered = Some(generated);
                    continue;
                }
                if frozen_cli_snapshot && iteration == 0 {
                    let started = Instant::now();
                    runtime.prefetch_call_hooks(&unit.source, unit.analysis.calls.values())?;
                    hook_prefetch_seconds += started.elapsed().as_secs_f64();
                }
                let mut inputs = ProjectionInputs::new(unit, runtime);
                if unit.pass.as_ref().is_some_and(|pass| pass.inputs == inputs) {
                    reused_analysis_passes += 1;
                    continue;
                }
                let alias_generation = runtime.projection_generation();
                let observations = runtime.protocol_observations();
                let initial_reads = iteration == 0 && unit.reads_protocol_aliases;
                analysis_passes += 1;
                // Initial lowering already used this revision's project and
                // empty oracle. Subsequent passes incorporate native evidence.
                if iteration > 0 {
                    unit.analysis = crate::lower::analyze_with_oracle(
                        &unit.source,
                        &unit.suite,
                        runtime,
                        Some(&unit.oracle),
                    )?;
                }
                let mut flow = crate::flow::project_with_return_types(
                    &unit.source,
                    &unit.suite,
                    &mut unit.analysis,
                    runtime,
                    Some(&unit.oracle),
                    &unit.patches,
                    &unit.return_types,
                )
                .with_context(|| format!("flow {}", unit.source.module))?;
                if frozen_cli_snapshot && unit.prepare_early_returns(runtime)? {
                    flow = crate::flow::project_with_return_types(
                        &unit.source,
                        &unit.suite,
                        &mut unit.analysis,
                        runtime,
                        Some(&unit.oracle),
                        &unit.patches,
                        &unit.return_types,
                    )?;
                }
                let generation = inputs.generation;
                inputs = ProjectionInputs::new(unit, runtime);
                inputs.generation = generation;
                let mut edits = crate::build_edits(
                    &unit.source,
                    &unit.analysis,
                    flow.edits.clone(),
                    &unit.patches,
                    true,
                );
                edits.insert(unit.analysis.prelude_offset, unit.types.prelude());
                edits.insert(unit.analysis.prelude_offset, flow.type_prelude.clone());
                let generated = render(&unit.source, &edits)
                    .with_context(|| format!("render {}", unit.source.module))?;
                if self.native_warmup.is_some() {
                    if unit.path.starts_with(&out) {
                        pending_projections.push((unit.path.clone(), generated.text.clone()));
                    }
                } else {
                    unit.write_projection(&generated, &out)?;
                }
                unit.rendered = Some(generated);
                // Alias changes affect consumers that actually read protocol
                // types, including cache hits. A producer with no such reads
                // can retain its pass when an unrelated consumer adds aliases.
                // Other project/receiver changes still invalidate every pass.
                unit.reads_protocol_aliases =
                    initial_reads || runtime.protocol_observations() != observations;
                inputs.generation.1 = unit.reads_protocol_aliases.then_some(alias_generation);
                unit.pass = (inputs.generation
                    == runtime.projection_context(unit.reads_protocol_aliases))
                .then_some(ProjectionPass { inputs, flow });
            }
            if iteration == 0 && self.memory_projection && self.native_cli.is_none() {
                if let Some(warmup) = self.native_warmup.take() {
                    warmup_analysis_passes = analysis_passes;
                    phase(&mut phases, "warmup_flow", &mut phase_started);
                    match warmup.finish() {
                        Ok(mut checker) => {
                            self.native_warmup_seeds = checker.seed_original_sources(
                                self.base_project
                                    .modules
                                    .values()
                                    .filter(|module| !module.external)
                                    .map(|module| {
                                        (
                                            Path::new(&module.source.path),
                                            module.source.text.as_str(),
                                            module.original_revision,
                                        )
                                    }),
                            );
                            *server = Some(checker);
                            self.native_warmup_reused = true;
                        }
                        _ => self.native_warmup_reused = false,
                    }
                    phase(&mut phases, "warmup_join", &mut phase_started);
                }
                reused_native_warmup = frozen_cli_snapshot && self.native_warmup_reused;
                mode = "push";
                let sources = units
                    .iter()
                    .map(|(path, unit)| (path.clone(), unit.source.text.clone()))
                    .collect();
                if reused_native_warmup
                    && units.keys().any(|path| {
                        !path.starts_with(&root)
                            && !path.starts_with(&out)
                            && !borrowed_overlay
                                .as_ref()
                                .is_some_and(|provider| path.starts_with(provider))
                    })
                {
                    // Late installed-package contributions may require additional
                    // workspace roots. Keep their existing fresh initialization.
                    reused_native_warmup = false;
                    self.native_warmup_reused = false;
                }
                if !reused_native_warmup {
                    *server = Some(
                        crate::checker::ProjectChecker::new_with_memory_projection_roots(
                            Path::new(required("ty")?),
                            &root,
                            &out,
                            &checker_config_path,
                            &sources,
                            &borrowed_overlay.iter().cloned().collect::<Vec<_>>(),
                        )?,
                    );
                }
                if frozen_cli_snapshot && reused_native_warmup {
                    source_cache_seeds = self.native_warmup_seeds;
                } else if frozen_cli_snapshot {
                    source_cache_seeds = server.as_mut().unwrap().seed_original_sources(
                        project.modules.values().map(|module| {
                            (
                                Path::new(&module.source.path),
                                module.source.text.as_str(),
                                module.original_revision,
                            )
                        }),
                    );
                }
            }
            for (path, text) in pending_projections {
                write(&path, &text)?;
            }
            virtual_projection.sync(
                &stubs,
                &mut runtime.requested_virtual_types,
                &mut self.virtual_files,
                server.as_mut(),
            )?;
            sync_support_modules(&stubs, &units, &mut self.support_files, server.as_mut())?;
            checked_imports.extend(
                units
                    .values()
                    .flat_map(|unit| unit.analysis.support_modules.keys().cloned()),
            );
            phase(&mut phases, "flow_and_render", &mut phase_started);
            let mut candidates = BTreeSet::new();
            for unit in units.values() {
                if unit.valid && unit.dirty && !unit.passthrough {
                    candidates.extend(imports(
                        &unit.rendered.as_ref().unwrap().text,
                        project.source_type(Path::new(&unit.source.path)),
                    )?);
                }
            }
            let candidates: Vec<_> = candidates.difference(&checked_imports).cloned().collect();
            let mut native_extras = BTreeMap::new();
            let import_preflight = out.join("type-imports.py");
            if !candidates.is_empty() {
                let code = candidates
                    .iter()
                    .map(|module| format!("import {module}\n"))
                    .collect::<String>();
                write(&import_preflight, &code)?;
                native_extras.insert(import_preflight.clone(), code);
            }
            // A TypeSnapshot nominal must resolve to a class literal and use
            // its exact generic context arity. Ask the native checker once per
            // name; arbitrary constructors or display-name guesses are not an
            // authoritative class schema.
            let mut nominal_candidates: Vec<_> = runtime
                .nominal_candidates
                .keys()
                .filter(|name| !project.snapshot_nominals.contains_key(*name))
                .cloned()
                .chain(
                    runtime
                        .annotation_candidates
                        .iter()
                        .filter(|name| !project.snapshot_symbols.contains_key(*name))
                        .cloned(),
                )
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            if !nominal_candidates.is_empty() {
                // Later hook responses commonly introduce builtin and None snapshots.
                // Establish these in the same native request instead of
                // restarting all inference for each newly encountered builtin.
                // Their class identities and arities still come from ty.
                for name in [
                    "bool",
                    "int",
                    "str",
                    "bytes",
                    "bytearray",
                    "memoryview",
                    "float",
                    "complex",
                    "object",
                    "tuple",
                    "list",
                    "dict",
                    "set",
                    "frozenset",
                    "type",
                ]
                .into_iter()
                .map(|name| format!("builtins.{name}"))
                .chain(["types.NoneType".to_owned()])
                {
                    if !project.snapshot_nominals.contains_key(&name) {
                        nominal_candidates.push(name);
                    }
                }
                nominal_candidates.sort();
                nominal_candidates.dedup();
            }
            let nominal_preflight = out.join("nominal-snapshots.py");
            let needs_schema = |name: &str| {
                runtime.nominal_candidates.contains_key(name)
                    || name.starts_with("builtins.")
                    || name == "types.NoneType"
            };
            let mut lines = BTreeMap::new();
            if !nominal_candidates.is_empty() {
                let mut probe_types =
                    Types::with_project("_ty_nominal_", "__snapshot_probe__", &project);
                let expressions = nominal_candidates
                    .iter()
                    .map(|name| probe_types.text(name))
                    .collect::<Result<Vec<_>>>()?;
                let mut code = format!(
                    "from typing_extensions import reveal_type as _ty_snapshot_reveal\n{}",
                    probe_types.prelude()
                );
                let mut line = code.lines().count() + 1;
                for (index, expression) in expressions.iter().enumerate() {
                    lines.insert(line, (index, true));
                    if needs_schema(&nominal_candidates[index]) {
                        lines.insert(line + 2, (index, false));
                        code.push_str(&format!("_ty_snapshot_reveal({expression})\ndef _ty_snapshot_probe_{index}(_value: {expression}):\n    _ty_snapshot_reveal(_value)\n"));
                        line += 3;
                    } else {
                        // Annotation fallbacks need only native symbol lookup.
                        // Introspecting every class's generic schema would emit
                        // two unnecessary reveal diagnostics per annotation.
                        code.push_str(&format!("{expression}\n"));
                        line += 1;
                    }
                }
                write(&nominal_preflight, &code)?;
                runtime
                    .requested_virtual_types
                    .extend(probe_types.virtual_references.iter().cloned());
                virtual_projection.sync(
                    &stubs,
                    &mut runtime.requested_virtual_types,
                    &mut self.virtual_files,
                    server.as_mut(),
                )?;
                native_extras.insert(nominal_preflight.clone(), code);
            }
            let query_paths: Vec<_> = units
                .values()
                .filter(|u| u.dirty && u.valid && u.analysis.needs_probes())
                .map(|u| u.path.clone())
                .collect();
            let mut native_paths = Vec::new();
            for path in [&import_preflight, &nominal_preflight] {
                if native_extras.contains_key(path) {
                    native_paths.push(path.clone());
                }
            }
            native_paths.extend(query_paths.iter().cloned());
            phase(&mut phases, "import_preflight", &mut phase_started);
            // These probes use the same provisional projection. Synchronize it
            // once, then validate imports and class schemas before consuming
            // any inferred user types. A failed preflight discards this batch
            // and takes the existing reprojection path.
            let complete_check = frozen_cli_snapshot
                && query_paths.is_empty()
                && units.values().any(|unit| !unit.early_returns.is_empty())
                && server
                    .as_ref()
                    .is_some_and(|checker| checker.supports_workspace_diagnostics());
            let mut inputs = (complete_check || self.native_cli.is_some()).then(|| {
                NativeCheckInputs::new(&units, runtime, &virtual_projection, &self.support_files)
            });
            let cli_raw = self.native_cli.take().and_then(|cli| {
                if query_paths.is_empty() {
                    cli.finish(
                        inputs.as_ref().unwrap(),
                        &checker_config_path,
                        &native_extras,
                    )
                } else {
                    None
                }
            });
            if cli_raw.is_none() && !complete_check {
                inputs = None;
            }
            let all_raw = if let Some(raw) = cli_raw {
                native_cli_reused = true;
                if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
                    eprintln!("companion native cli reuse: true");
                }
                raw
            } else if let Some(inputs) = &inputs {
                let mut sources = inputs.sources.clone();
                sources.extend(native_extras.clone());
                native_paths.extend(inputs.paths.iter().cloned());
                server
                    .as_mut()
                    .context("workspace checker")?
                    .check_workspace(&sources, &native_paths)?
            } else {
                check_units(
                    &binary,
                    &view,
                    &checker_config_path,
                    &units,
                    &native_extras,
                    &native_paths,
                    mode == "push",
                    server,
                    capabilities,
                    self.memory_projection,
                )?
            };
            phase(&mut phases, "oracle_check", &mut phase_started);
            oracle_queries += 1;
            let raw: Vec<_> = all_raw
                .iter()
                .filter(|diagnostic| !native_extras.contains_key(&path_of(diagnostic, &view)))
                .cloned()
                .collect();
            if let Some(inputs) = inputs {
                checked_projection = Some((inputs, raw.clone()));
            }
            if !candidates.is_empty() {
                let unavailable: Vec<_> = all_raw
                    .iter()
                    .filter(|diagnostic| path_of(diagnostic, &view) == import_preflight)
                    .cloned()
                    .collect();
                write(
                    &out.join(format!("type-imports-{iteration}.json")),
                    &serde_json::to_string_pretty(&unavailable)?,
                )?;
                let mut changed = false;
                for diagnostic in unavailable {
                    ensure!(
                        matches!(
                            diagnostic["check_name"].as_str(),
                            Some("unresolved-import" | "import-not-found")
                        ) && path_of(&diagnostic, &view) == import_preflight,
                        "unexpected type-import preflight diagnostic: {diagnostic}"
                    );
                    let line = diagnostic["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .context("import diagnostic line")? as usize;
                    let module = candidates
                        .get(line.checked_sub(1).context("import line zero")?)
                        .context("import diagnostic outside import_preflight")?;
                    changed |=
                        Arc::make_mut(&mut project.unavailable_modules).insert(module.clone());
                }
                checked_imports.extend(candidates);
                if changed {
                    if capabilities.is_none() {
                        *server = None;
                    }
                    virtual_projection.modules = project.prepare_virtual_types(&definitions)?;
                    virtual_projection.materialized = None;
                    runtime.attach_project(Arc::new(project.clone()), runtime.config.clone());
                    for unit in units.values_mut() {
                        if !unit.dirty {
                            unit.dirty = true;
                            unit.oracle.clear();
                            unit.patches.clear();
                            unit.return_types.clear();
                            unit.early_returns.clear();
                            unit.analysis.early_calls.clear();
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
            if !nominal_candidates.is_empty() {
                let diagnostics: Vec<_> = all_raw
                    .iter()
                    .filter(|diagnostic| path_of(diagnostic, &view) == nominal_preflight)
                    .cloned()
                    .collect();
                write(
                    &out.join(format!("nominal-snapshots-{iteration}.json")),
                    &serde_json::to_string_pretty(&diagnostics)?,
                )?;
                let mut class_literals = BTreeSet::new();
                let mut arities = BTreeMap::new();
                let mut missing_symbols = BTreeSet::new();
                for diagnostic in &diagnostics {
                    if path_of(diagnostic, &view) != nominal_preflight {
                        continue;
                    }
                    let line = diagnostic
                        .pointer("/location/positions/begin/line")
                        .and_then(Value::as_u64)
                        .context("nominal reveal source line")?
                        as usize;
                    let Some((index, class_object)) = lines.get(&line) else {
                        continue;
                    };
                    if *class_object
                        && matches!(
                            crate::native_rules::semantic_id(
                                diagnostic["check_name"].as_str().unwrap_or("")
                            ),
                            "unresolved-attribute" | "unresolved-reference"
                        )
                    {
                        missing_symbols.insert(*index);
                    }
                    if diagnostic["check_name"] != "revealed-type" {
                        continue;
                    }
                    let description = diagnostic["description"]
                        .as_str()
                        .context("nominal reveal description")?;
                    let Some(start) = description.find("Revealed type: `") else {
                        continue;
                    };
                    let display = &description[start + "Revealed type: `".len()
                        ..description.rfind('`').context("nominal reveal end")?];
                    if *class_object {
                        if display.starts_with("<class '") {
                            class_literals.insert(*index);
                        }
                    } else {
                        let (_, arguments) = crate::runtime::generic_parts(display);
                        arities.insert(
                            *index,
                            if nominal_candidates[*index] == "builtins.tuple" {
                                1
                            } else {
                                arguments.len()
                            },
                        );
                    }
                }
                let mut requires_reprojection = !missing_symbols.is_empty();
                for (index, name) in nominal_candidates.into_iter().enumerate() {
                    let arity = class_literals
                        .contains(&index)
                        .then(|| arities.get(&index).copied())
                        .flatten();
                    // Unconfirmed nominals already render as their declared
                    // name and arguments. Positive native evidence leaves those
                    // bytes unchanged; only a non-class or wrong arity changes
                    // the fallback and requires another projection pass.
                    if let Some(requested) = runtime.nominal_candidates.get(&name) {
                        requires_reprojection |= arity.is_none_or(|arity| {
                            requested.iter().any(|count| *count != 0 && *count != arity)
                        });
                    }
                    Arc::make_mut(&mut project.snapshot_symbols)
                        .insert(name.clone(), !missing_symbols.contains(&index));
                    if needs_schema(&name) {
                        Arc::make_mut(&mut project.snapshot_nominals).insert(name, arity);
                    }
                }
                if requires_reprojection {
                    runtime.attach_project(Arc::new(project.clone()), runtime.config.clone());
                    for unit in units.values_mut().filter(|unit| unit.dirty) {
                        unit.oracle.clear();
                        unit.patches.clear();
                        unit.return_types.clear();
                        unit.early_returns.clear();
                        unit.analysis.early_calls.clear();
                        unit.types = Types::with_project(
                            &format!("{}result_", unit.source.prefix),
                            &unit.source.module,
                            &project,
                        );
                    }
                    continue;
                }
                // Successful lookup and arity checks confirm the provisional
                // source without changing it. Retain the inference passes and
                // expose the proof to subsequent renders. Invalid names and
                // schemas take the reprojection path above.
                let attached = Arc::make_mut(runtime.project.as_mut().unwrap());
                attached.snapshot_nominals = project.snapshot_nominals.clone();
                attached.snapshot_symbols = project.snapshot_symbols.clone();
                for unit in units.values_mut() {
                    unit.types.confirm_native_evidence(&project);
                }
            }
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
                    let response = if let Some(response) = unit.early_returns.get(&call.span) {
                        response.response.clone()
                    } else {
                        call_request(
                            runtime,
                            &unit.source,
                            "adjust-call-return",
                            call,
                            Some(&unit.oracle),
                            receiver.as_deref(),
                        )?
                    };
                    // Returning the exact host fallback leaves the original
                    // checker type intact, including names absent from this module.
                    let unchanged = unit
                        .oracle
                        .get(&call.return_key())
                        .is_some_and(|expression| {
                            response["return-type"] == runtime.type_expr(&unit.source, expression)
                        });
                    if response["kind"] == "call-return-patch" && !unchanged {
                        let native = unit
                            .early_returns
                            .get(&call.span)
                            .is_some_and(|response| response.native);
                        if !native {
                            let native_self =
                                unit.oracle.get(&call.receiver_key()).filter(|value| {
                                    value.as_str() == "Self" || value.starts_with("Self@")
                                });
                            let receiver = native_self
                                .map(String::as_str)
                                .or(call.flow_receiver_type.as_deref())
                                .or(call.receiver_type.as_deref())
                                .or_else(|| {
                                    unit.oracle.get(&call.receiver_key()).map(String::as_str)
                                });
                            let previous = types.set_self_context(
                                receiver.map(|receiver| runtime.type_expr(&unit.source, receiver)),
                            );
                            let rendered = types.render(&response["return-type"])?;
                            types.set_self_context(previous);
                            next.insert(call.span, rendered);
                        }
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
                types.register_protocol_types(runtime);
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
            // A native nominal preflight can produce the same provisional
            // source while advancing class/arity knowledge. That is progress,
            // not a cycle in the complete inference state.
            state.update(serde_json::to_vec(&project.snapshot_nominals)?);
            state.update(serde_json::to_vec(&project.snapshot_symbols)?);
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
        for unit in units
            .values_mut()
            .filter(|unit| unit.dirty && !unit.passthrough)
        {
            if !unit.valid {
                continue;
            }
            let inputs = ProjectionInputs::new(unit, runtime);
            let flow = if unit.pass.as_ref().is_some_and(|pass| pass.inputs == inputs) {
                reused_final_flows += 1;
                unit.pass.take().unwrap().flow
            } else {
                unit.pass = None;
                crate::flow::project_with_return_types(
                    &unit.source,
                    &unit.suite,
                    &mut unit.analysis,
                    runtime,
                    Some(&unit.oracle),
                    &unit.patches,
                    &unit.return_types,
                )?
            };
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
            unit.write_projection(&generated, &out)?;
            unit.rendered = Some(generated);
            unit.flow_diagnostics = flow.diagnostics;
        }
        virtual_projection.sync(
            &stubs,
            &mut runtime.requested_virtual_types,
            &mut self.virtual_files,
            server.as_mut(),
        )?;
        phase(&mut phases, "final_render", &mut phase_started);
        let paths: Vec<_> = units
            .values()
            .filter(|u| u.selected && u.dirty)
            .map(|u| u.path.clone())
            .collect();
        let disk_batch = capabilities.is_none()
            && paths.len() >= 128
            && self.notebook_sources.is_empty()
            && units.keys().all(|path| path.starts_with(&out))
            && paths.iter().all(|path| path.starts_with(&view));
        let reused_native_check = checked_projection.as_ref().is_some_and(|(inputs, _)| {
            *inputs
                == NativeCheckInputs::new(&units, runtime, &virtual_projection, &self.support_files)
        });
        let mut raw = if reused_native_check {
            checked_projection.take().unwrap().1
        } else if (disk_batch || self.memory_projection)
            && server
                .as_ref()
                .is_some_and(|s| s.supports_workspace_diagnostics())
        {
            if std::env::var_os("TY_COMPANION_TRACE_PHASES").is_some() {
                eprintln!(
                    "companion transport: {}workspace diagnostics",
                    if self.memory_projection {
                        "memory projection "
                    } else {
                        "reused "
                    }
                );
            }
            let sources = units
                .iter()
                .map(|(path, unit)| (path.clone(), unit.rendered.as_ref().unwrap().text.clone()))
                .collect();
            server
                .as_mut()
                .context("workspace checker")?
                .check_workspace(&sources, &paths)?
        } else if disk_batch {
            check_selected_files(&binary, &view, &checker_config_path, &paths)?
        } else {
            check_units(
                &binary,
                &view,
                &checker_config_path,
                &units,
                &BTreeMap::new(),
                &paths,
                mode == "push",
                server,
                capabilities,
                self.memory_projection,
            )?
        };
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
        for unit in units.values().filter(|unit| unit.selected && !unit.dirty) {
            raw.extend(unit.raw_diagnostics.clone());
        }
        phase(&mut phases, "final_check", &mut phase_started);
        write(
            &out.join("raw-diagnostics.json"),
            &serde_json::to_string_pretty(&raw)?,
        )?;
        let mut diagnostics = self.config_diagnostics.clone();
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
        let virtual_scope = runtime.selected_virtual_scope.as_deref();
        // One immutable name context and one diagnostic partition per project,
        // rather than cloning every class name and scanning every diagnostic
        // again for each selected file.
        let virtual_names = project.virtual_identities.clone();
        let mut nominal_candidates: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (qualified, class) in project.classes.iter() {
            if class["virtual"] != true
                && let Some(short) = qualified.rsplit('.').next()
            {
                nominal_candidates.entry(short).or_default().push(qualified);
            }
        }
        let nominal_names = Arc::new(
            nominal_candidates
                .into_iter()
                .filter(|(_, names)| names.len() == 1)
                .map(|(short, names)| (short.to_owned(), names[0].to_owned()))
                .collect::<BTreeMap<_, _>>(),
        );
        let mut raw_by_path = BTreeMap::<PathBuf, Vec<Value>>::new();
        for diagnostic in raw {
            raw_by_path
                .entry(path_of(&diagnostic, &view))
                .or_default()
                .push(diagnostic);
        }
        // Refresh per-file caches from the same partition used for mapping.
        // Scanning every diagnostic for every file makes otherwise unrelated
        // source files multiply the cost of one file's errors (and, on Windows,
        // repeatedly canonicalizes those same diagnostic paths).
        for unit in units
            .values_mut()
            .filter(|unit| unit.selected && unit.dirty)
        {
            unit.raw_diagnostics = raw_by_path.get(&unit.path).cloned().unwrap_or_default();
        }
        let mut project_by_path = BTreeMap::<&str, Vec<&Value>>::new();
        for diagnostic in &project_diagnostics {
            if let Some(path) = diagnostic["location"]["file-path"].as_str() {
                project_by_path.entry(path).or_default().push(diagnostic);
            }
        }
        for unit in units.values().filter(|u| u.selected) {
            let local = raw_by_path
                .get(&unit.path)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut plugin = unit.analysis.plugin_diagnostics.clone();
            plugin.extend(unit.diagnostics.clone());
            plugin.extend(unit.flow_diagnostics.clone());
            plugin.extend(
                project_by_path
                    .get(unit.source.path.as_str())
                    .into_iter()
                    .flatten()
                    .map(|d| json!({"diagnostic":d,"source":d["location"]})),
            );
            if local.is_empty() && plugin.is_empty() {
                continue;
            }
            let origins = diagnostic_origins(
                unit,
                &project,
                &generated_declarations,
                virtual_scope,
                &virtual_names,
                &nominal_names,
            );
            let mut display_names = unit.analysis.helpers.clone();
            display_names.extend(unit.names.clone());
            display_names.extend(unit.types.display_names());
            let (mapped, hidden) = map_diagnostics_with_origins(
                &unit.source,
                unit.rendered.as_ref().unwrap(),
                local,
                &display_names,
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
        let discard_clean_cli_state = frozen_cli_snapshot
            && options
                .get("discard-clean-cli-state")
                .is_some_and(|value| value == "true")
            && diagnostics.is_empty();
        let mut result = json!({"diagnostics":diagnostics,"suppressed_generated":suppressed,"implementation":"rust","plugin_runtime":"embedded-monty","project":root,"checker":binary,"checker_mode":mode,"configuration":self.config_source,"revision":self.revisions,"external_resolution_requests":self.external_resolution_requests,"projection_rounds":rounds,"oracle_rounds":oracle_queries,"monty_executions":runtime.executions,"monty_cache_hits":runtime.hits,"monty_seconds":runtime.execution_time.as_secs_f64(),"seconds":started.elapsed().as_secs_f64()});
        result["memory_projection"] = json!(self.memory_projection);
        result["monty_prefetched_executions"] = json!(runtime.prefetched_executions);
        result["monty_prefetch_seconds"] = json!(hook_prefetch_seconds);
        result["native_warmup_reused"] = json!(reused_native_warmup);
        result["native_warmup_analysis_passes"] = json!(warmup_analysis_passes);
        result["borrowed_overlay"] = json!(borrowed_overlay);
        result["reused_native_check"] = json!(reused_native_check);
        result["passthrough_units"] = json!(units.values().filter(|unit| unit.passthrough).count());
        result["early_call_returns"] = json!(
            units
                .values()
                .map(|unit| unit.early_returns.len())
                .sum::<usize>()
        );
        result["native_call_returns"] = json!(
            units
                .values()
                .map(|unit| {
                    unit.early_returns
                        .values()
                        .filter(|response| response.native)
                        .count()
                })
                .sum::<usize>()
        );
        result["indexed_modules"] = json!(self.base_project.modules.len());
        result["indexed_external_modules"] = json!(
            self.base_project
                .modules
                .values()
                .filter(|module| module.external)
                .count()
        );
        if !discard_clean_cli_state {
            let virtual_names = runtime.virtual_display_names();
            self.names = project
                .virtual_identities
                .iter()
                .map(|(key, value)| {
                    (
                        key.clone(),
                        virtual_names
                            .get(value)
                            .unwrap_or(value)
                            .rsplit('.')
                            .next()
                            .unwrap_or(value)
                            .to_owned(),
                    )
                })
                .collect();
            for (generated, original) in virtual_names {
                let short = original.rsplit('.').next().unwrap_or(&original).to_owned();
                self.names.insert(generated, short.clone());
                self.names.insert(original, short);
            }
            // Direct type metadata may retain a logical virtual identity without
            // passing through the generated identifier. Both presentations use the
            // native declared class name; protocol identities remain qualified.
            for logical in project.virtual_identities.values() {
                self.names.insert(
                    logical.clone(),
                    logical.rsplit('.').next().unwrap_or(logical).to_owned(),
                );
            }
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
            let mut document_paths: BTreeSet<_> = self
                .documents
                .iter()
                .map(|document| document.path.clone())
                .collect();
            for module in project.modules.values().filter(|m| m.external) {
                let projected = projected_stub_path(
                    &stubs,
                    &stub_roots,
                    &self.resolved_paths,
                    Path::new(&module.source.path),
                    borrowed_overlay.as_deref(),
                )?;
                if !document_paths.contains(&projected) {
                    let path = if self
                        .resolved_paths
                        .contains_key(Path::new(&module.source.path))
                    {
                        PathBuf::from(&module.source.path)
                    } else {
                        projected
                    };
                    document_paths.insert(path.clone());
                    self.documents.push(ProjectDocument {
                        source: module.source.clone(),
                        path,
                        rendered: render(&module.source, &crate::mapping::Edits::default())?,
                        names: BTreeMap::new(),
                    });
                }
            }
        }
        result["followup_state_saved"] = json!(!discard_clean_cli_state);
        phase(&mut phases, "source_maps", &mut phase_started);
        result["projected_units"] = json!(units.values().filter(|unit| unit.dirty).count());
        result["analysis_passes"] = json!(analysis_passes);
        result["reused_analysis_passes"] = json!(reused_analysis_passes);
        result["reused_final_flows"] = json!(reused_final_flows);
        result["reused_units"] = json!(units.values().filter(|unit| !unit.dirty).count());
        result["monty_cache"] = runtime.cache_statistics();
        result["monty_execution_profile"] = runtime.execution_profile();
        if !discard_clean_cli_state {
            self.cached_sources = project
                .modules
                .iter()
                .map(|(name, module)| (name.clone(), module.source.clone()))
                .collect();
        }
        result["incremental_context_saved"] = json!(context.is_some());
        result["native_source_cache_seeds"] = json!(source_cache_seeds);
        result["native_cli_reused"] = json!(native_cli_reused);
        result["original_source_reads"] = json!(
            server
                .as_ref()
                .map_or(0, crate::checker::ProjectChecker::original_source_reads)
        );
        self.cached_context = context;
        self.base_project.unavailable_modules = project.unavailable_modules.clone();
        self.base_project.snapshot_nominals = project.snapshot_nominals.clone();
        self.base_project.snapshot_symbols = project.snapshot_symbols.clone();
        if !discard_clean_cli_state {
            self.cached_units = units;
        }
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
        if !discard_clean_cli_state {
            self.last_result = Some(result.clone());
        }
        Ok(result)
    }
}

pub fn run(input: &Path, options: &BTreeMap<String, String>) -> Result<Value> {
    Session::new(input, options)?.check(&BTreeMap::new())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn one_shot_cli_discards_only_unused_clean_state() -> anyhow::Result<()> {
        use super::*;
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory =
            Cleanup(std::env::temp_dir().join(format!("ty-one-shot-state-{}", std::process::id())));
        let provider = directory.0.join("provider");
        write(
            &provider.join("samplepkg/__init__.pyi"),
            "class Native: ...\n",
        )?;
        let artifact = directory.0.join("plugin.py");
        write(&artifact, "# Native provider only.\n")?;
        let manifest = directory.0.join("plugin.json");
        write(&manifest, &json!({"id":"native","name":"Native","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"stub-overlays":true},"default-config":{"companion-native-fields":true,"companion-native-cli":true,"companion-third-party-overlays":["samplepkg"]}}).to_string())?;
        let config = directory.0.join("config.toml");
        write(
            &config,
            &format!(
                "[environment]\npython-version='3.13'\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid='native'\nruntime='monty'\ntrusted=true\npath={}\nmanifest-path={}\nstub-overlay-path={}\n",
                json!(artifact),
                json!(manifest),
                json!(provider)
            ),
        )?;
        for negative in [false, true] {
            let root = directory
                .0
                .join(if negative { "negative" } else { "positive" });
            let path = root.join("main.py");
            write(
                &path,
                if negative {
                    "wrong: int = 'bad'\n"
                } else {
                    "good: int = 1\n"
                },
            )?;
            let before = std::fs::read(&path)?;
            let revision = crate::model::file_revision(&std::fs::metadata(&path)?);
            let mut expected = None;
            for discard in [false, true] {
                let options = BTreeMap::from([
                    ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
                    ("ty-config".into(), config.to_string_lossy().into_owned()),
                    (
                        "out".into(),
                        directory
                            .0
                            .join(format!("output-{negative}-{discard}"))
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    ("memory-projection".into(), "true".into()),
                    ("public-cli".into(), "true".into()),
                    ("ephemeral-evidence".into(), "true".into()),
                    ("discard-clean-cli-state".into(), discard.to_string()),
                ]);
                let mut session = Session::new(&root, &options)?;
                let result = session.check(&BTreeMap::new())?;
                assert_eq!(result["followup_state_saved"], !discard || negative);
                assert_eq!(session.documents().is_empty(), discard && !negative);
                assert_eq!(session.cached_sources.is_empty(), discard && !negative);
                assert_eq!(session.cached_units.is_empty(), discard && !negative);
                assert_eq!(session.last_result.is_none(), discard && !negative);
                let diagnostics = result["diagnostics"].clone();
                if let Some(expected) = &expected {
                    assert_eq!(&diagnostics, expected);
                } else {
                    expected = Some(diagnostics);
                }
                assert_eq!(
                    result["diagnostics"].as_array().unwrap().len(),
                    usize::from(negative)
                );
                assert_eq!(std::fs::read(&path)?, before);
                assert_eq!(
                    crate::model::file_revision(&std::fs::metadata(&path)?),
                    revision
                );
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn native_cli_bootstrap_probe_preserves_exact_errors_and_requires_import_coverage()
    -> anyhow::Result<()> {
        use super::*;
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker"),
        );
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Cleanup(
            std::env::temp_dir().join(format!("ty-bootstrap-probe-{}", std::process::id())),
        );
        for case in [
            "subset",
            "prefix",
            "empty",
            "new-module",
            "aliased",
            "code",
            "from-import",
            "wrong-path",
            "probe-error",
            "probe-edit",
        ] {
            let root = directory.0.join(case).join("project");
            let out = directory.0.join(case).join("output");
            write(
                &root.join("main.py"),
                "from dependency import value\nwrong: int = 'bad'\n",
            )?;
            write(&root.join("dependency.py"), "value: int = 1\n")?;
            write(&root.join("package/__init__.py"), "")?;
            write(&root.join("package/sub.py"), "value: int = 1\n")?;
            let root = root.canonicalize()?;
            let config = out.join("checker.toml");
            write(&config, "[environment]\npython-version='3.13'\n")?;
            let path = root.join("main.py");
            let project = Project::load(&root, std::slice::from_ref(&path), &json!({}), json!({}))?;
            let inputs = NativeCheckInputs {
                sources: project
                    .modules
                    .values()
                    .map(|module| {
                        (
                            PathBuf::from(&module.source.path),
                            module.source.text.clone(),
                        )
                    })
                    .collect(),
                paths: vec![path.clone()],
                generation: 0,
                virtual_types: None,
                support_sources: BTreeMap::new(),
            };
            let early_path = out.join("native-bootstrap-imports.py");
            let early = BTreeMap::from([(
                early_path.clone(),
                if case == "probe-error" {
                    "import absent_dependency\n"
                } else {
                    "import dependency\nimport package.sub\n"
                }
                .to_owned(),
            )]);
            let worker = NativeCliCheck::start(
                &binary,
                &root,
                &config,
                &inputs.paths,
                &project,
                &[],
                Some(NativeCliProjection {
                    sources: &BTreeMap::new(),
                    extras: &early,
                    prepared: None,
                    bootstrap_imports: true,
                }),
            )?
            .context("bootstrap CLI")?;
            let final_path = out.join(if case == "wrong-path" {
                "unexpected.py"
            } else {
                "type-imports.py"
            });
            let final_code = match case {
                "prefix" => "import package\n",
                "new-module" => "import typing\n",
                "aliased" => "import dependency as alias\n",
                "code" => "import dependency\nwrong: int = 'bad'\n",
                "from-import" => "from dependency import missing\n",
                _ => "import dependency\n",
            };
            let extras = if case == "empty" {
                BTreeMap::new()
            } else {
                BTreeMap::from([(final_path.clone(), final_code.to_owned())])
            };
            for (path, text) in &extras {
                write(path, text)?;
            }
            if case == "probe-edit" {
                write(&early_path, "import package\n")?;
            }
            let actual = worker.finish(&inputs, &config, &extras);
            if matches!(case, "subset" | "prefix" | "empty") {
                let mut paths = inputs.paths.clone();
                paths.extend(extras.keys().cloned());
                let expected = check_selected_files(&binary, &root, &config, &paths)?;
                assert_eq!(actual, Some(expected), "{case}");
                assert_eq!(actual.as_ref().unwrap().len(), 1, "native error retained");
            } else {
                assert!(actual.is_none(), "{case} must retain normal checking");
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn native_cli_partial_provider_matches_full_projection_and_preserves_originals()
    -> anyhow::Result<()> {
        use super::*;
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker"),
        );
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Cleanup(
            std::env::temp_dir().join(format!("ty-cli-partial-provider-{}", std::process::id())),
        );
        let root = directory.0.join("project");
        let provider = directory.0.join("provider");
        let out = directory.0.join("output");
        write(
            &root.join("main.py"),
            "from samplepkg.sub import Settings\nfrom samplepkg.extra import value\ns = Settings()\ngood: str = s.value\nfallback: str = value\nbad: int = s.value\n",
        )?;
        write(
            &provider.join("samplepkg/__init__.pyi"),
            "from .sub import Settings as Settings\n",
        )?;
        write(
            &provider.join("samplepkg/sub.pyi"),
            "class Settings:\n    value: int\n",
        )?;
        write(&provider.join("samplepkg/extra.pyi"), "value: str\n")?;
        let root = root.canonicalize()?;
        let provider = provider.canonicalize()?;
        let path = root.join("main.py");
        let config = out.join("checker.toml");
        write(
            &config,
            &format!(
                "[environment]\npython-version='3.13'\nextra-paths=[{}]\n",
                serde_json::to_string(&provider)?
            ),
        )?;
        let project = Project::load_selected(
            &root,
            std::slice::from_ref(&path),
            &json!({}),
            json!({}),
            false,
            crate::project::SourceLayout {
                roots: vec![root.clone()],
                overlays: std::slice::from_ref(&provider),
                include: &|_| true,
                sparse_overlays: false,
                call_assignments_only: false,
                capture_original_revisions: true,
                known_source_revisions: None,
                preloaded: None,
            },
        )?;
        let original = provider.join("samplepkg/sub.pyi");
        let bytes = std::fs::read(&original)?;
        let modified = std::fs::metadata(&original)?.modified()?;
        let mut sources: BTreeMap<_, _> = project
            .modules
            .values()
            .map(|module| {
                (
                    PathBuf::from(&module.source.path),
                    module.source.text.clone(),
                )
            })
            .collect();
        sources.insert(original.clone(), "class Settings:\n    value: str\n".into());
        let inputs = NativeCheckInputs {
            sources,
            paths: vec![path],
            generation: 0,
            virtual_types: None,
            support_sources: BTreeMap::new(),
        };
        let extras = BTreeMap::from([(
            out.join("type-imports.py"),
            "import missing_validation_module\n".into(),
        )]);
        let worker = NativeCliCheck::start(
            &binary,
            &root,
            &config,
            &inputs.paths,
            &project,
            std::slice::from_ref(&provider),
            Some(NativeCliProjection {
                sources: &inputs.sources,
                extras: &extras,
                prepared: None,
                bootstrap_imports: false,
            }),
        )?
        .context("partial CLI worker")?;
        let actual = worker
            .finish(&inputs, &config, &extras)
            .context("partial projection must qualify")?;
        assert_eq!(actual.len(), 2, "{actual:?}");
        assert!(
            actual
                .iter()
                .any(|d| path_of(d, &root) == root.join("main.py")
                    && d["location"]["positions"]["begin"]["line"] == 6)
        );
        assert!(
            actual
                .iter()
                .any(|d| path_of(d, &root) == out.join("type-imports.py"))
        );
        let full = out.join("full");
        for name in ["__init__.pyi", "extra.pyi", "sub.pyi"] {
            write(
                &full.join("samplepkg").join(name),
                &std::fs::read_to_string(provider.join("samplepkg").join(name))?,
            )?;
        }
        write(&full.join("samplepkg/sub.pyi"), &inputs.sources[&original])?;
        let reference = out.join("reference.toml");
        write(
            &reference,
            &format!(
                "[environment]\npython-version='3.13'\nextra-paths=[{}]\n",
                serde_json::to_string(&full)?
            ),
        )?;
        let mut checked = inputs.paths.clone();
        checked.extend(extras.keys().cloned());
        assert_eq!(
            actual,
            check_selected_files(&binary, &root, &reference, &checked)?
        );
        assert_eq!(std::fs::read(&original)?, bytes);
        assert_eq!(std::fs::metadata(&original)?.modified()?, modified);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn early_native_snapshot_rejects_changes_before_and_after_host_loading() -> anyhow::Result<()> {
        use super::*;
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker"),
        );
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory =
            Cleanup(std::env::temp_dir().join(format!("ty-early-snapshot-{}", std::process::id())));
        for case in [
            "positive",
            "negative",
            "before-host-read",
            "after-host-read",
            "provider-marker",
            "new-source",
            "replaced",
            "deleted",
        ] {
            let root = directory.0.join(case);
            let provider = directory.0.join(format!("{case}-provider"));
            let out = directory.0.join(format!("{case}-out"));
            write(
                &root.join("main.py"),
                if case == "negative" {
                    "wrong: int = 'bad'\n"
                } else {
                    "value: int = 1\n"
                },
            )?;
            write(&provider.join("pkg/__init__.pyi"), "class Settings: ...\n")?;
            write(&provider.join("pkg/py.typed"), "partial\n")?;
            let root = root.canonicalize()?;
            let provider = provider.canonicalize()?;
            let config = out.join("checker.toml");
            write(&config, "[environment]\npython-version='3.13'\n")?;
            let paths = vec![root.join("main.py")];
            let load = |known: &std::collections::HashMap<PathBuf, crate::model::FileRevision>| {
                Project::load_selected(
                    &root,
                    &paths,
                    &json!({}),
                    json!({}),
                    true,
                    crate::project::SourceLayout {
                        roots: vec![root.clone()],
                        overlays: &[],
                        include: &|_| true,
                        sparse_overlays: true,
                        call_assignments_only: true,
                        capture_original_revisions: true,
                        known_source_revisions: Some(known),
                        preloaded: None,
                    },
                )
            };
            let empty = Project::load_selected(
                &root,
                &[],
                &json!({}),
                json!({}),
                true,
                crate::project::SourceLayout {
                    roots: vec![root.clone()],
                    overlays: &[],
                    include: &|_| true,
                    sparse_overlays: true,
                    call_assignments_only: true,
                    capture_original_revisions: true,
                    known_source_revisions: None,
                    preloaded: None,
                },
            )?;
            assert!(
                empty.modules.is_empty(),
                "source parsing must wait until after launch"
            );
            let prepared = PendingNativeCliPreparation::start(
                &root,
                &config,
                &paths,
                std::slice::from_ref(&provider),
            )?
            .finish(&empty)
            .context("pre-read revisions")?;
            assert!(prepared.originals.is_empty());
            let mut worker = NativeCliCheck::start(
                &binary,
                &root,
                &config,
                &paths,
                &empty,
                std::slice::from_ref(&provider),
                Some(NativeCliProjection {
                    sources: &BTreeMap::new(),
                    extras: &BTreeMap::new(),
                    prepared: Some(&prepared),
                    bootstrap_imports: false,
                }),
            )?
            .context("pre-read native launch")?;
            if case == "before-host-read" {
                write(&paths[0], "value: str = 'changed'\n")?;
            }
            let project = load(&worker.file_revisions)?;
            let observed = worker.observe_project(&project);
            if case == "before-host-read" {
                assert!(!observed);
                continue;
            }
            assert!(observed, "{case}");
            let inputs = NativeCheckInputs {
                sources: project
                    .modules
                    .values()
                    .map(|module| {
                        (
                            PathBuf::from(&module.source.path),
                            module.source.text.clone(),
                        )
                    })
                    .collect(),
                paths: paths.clone(),
                generation: 0,
                virtual_types: None,
                support_sources: BTreeMap::new(),
            };
            match case {
                "after-host-read" => write(&paths[0], "value: str = 'changed'\n")?,
                "provider-marker" => write(&provider.join("pkg/py.typed"), "changed\n")?,
                "new-source" => write(&root.join("new.py"), "value = 1\n")?,
                "replaced" => {
                    write(
                        &out.join("replacement"),
                        &std::fs::read_to_string(&paths[0])?,
                    )?;
                    std::fs::rename(out.join("replacement"), &paths[0])?;
                }
                "deleted" => std::fs::remove_file(&paths[0])?,
                _ => {}
            }
            let actual = worker.finish(&inputs, &config, &BTreeMap::new());
            if matches!(case, "positive" | "negative") {
                assert_eq!(
                    actual,
                    Some(check_selected_files(&binary, &root, &config, &paths)?),
                    "{case}"
                );
            } else {
                assert!(actual.is_none(), "{case}");
            }
        }
        Ok(())
    }

    #[test]
    fn settings_bootstrap_loads_only_contributed_provider_sources() -> anyhow::Result<()> {
        use super::*;
        let directory =
            std::env::temp_dir().join(format!("ty-settings-bootstrap-{}", std::process::id()));
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Cleanup(directory);
        let root = directory.0.join("project");
        let provider = directory.0.join("provider");
        write(
            &root.join("settings.py"),
            "SECRET_SETTING: str = 'test-only'\n",
        )?;
        write(&root.join("src/app/main.py"), "class UnindexedModel: ...\n")?;
        write(&provider.join("pkg/__init__.pyi"), "")?;
        write(&provider.join("pkg/conf.pyi"), "class Settings: ...\n")?;
        write(
            &provider.join("pkg/unrelated.pyi"),
            "class Unrelated: ...\n",
        )?;
        let root = root.canonicalize()?;
        let provider = provider.canonicalize()?;
        let mut project = Project::load_selected(
            &root,
            &[],
            &json!({"claims":{"settings":[{"config-key":"settings-module"}]}}),
            json!({"settings-module":"settings"}),
            true,
            crate::project::SourceLayout {
                roots: vec![root.join("src"), root.clone()],
                overlays: &[],
                include: &|_| true,
                sparse_overlays: true,
                call_assignments_only: true,
                capture_original_revisions: true,
                known_source_revisions: None,
                preloaded: None,
            },
        )?;
        assert_eq!(project.modules.len(), 1);
        assert!(
            !project.build_request_for_files(None)["settings"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            project.selected_package_imports(&[root.join("src/app/main.py")])?,
            BTreeSet::from(["app".into()])
        );
        assert!(project.add_contribution_sources(&provider, &["pkg.conf.Settings".into()])?);
        assert!(project.class_summary("pkg.conf.Settings").is_some());
        assert!(!project.modules.contains_key("pkg.unrelated"));
        assert!(!project.add_contribution_sources(&provider, &["missing.Target".into()])?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn native_cli_attempts_do_not_reuse_discarded_provider_patches() -> anyhow::Result<()> {
        use super::*;
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker"),
        );
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Cleanup(
            std::env::temp_dir().join(format!("ty-native-retry-overlay-{}", std::process::id())),
        );
        let root = directory.0.join("project");
        let provider = directory.0.join("provider");
        let out = directory.0.join("output");
        write(
            &root.join("main.py"),
            "from pkg.a import Settings as A\nfrom pkg.b import Settings as B\nleft: str = A().value\nright: str = B().value\n",
        )?;
        let original = "class Settings:\n    value: int\n";
        let patch = "class Settings:\n    value: str\n";
        write(&provider.join("pkg/__init__.pyi"), "")?;
        for name in ["a", "b"] {
            write(&provider.join(format!("pkg/{name}.pyi")), original)?;
        }
        let root = root.canonicalize()?;
        let provider = provider.canonicalize()?;
        let paths = vec![root.join("main.py")];
        let config = out.join("checker.toml");
        write(
            &config,
            &format!(
                "[environment]\npython-version='3.13'\nextra-paths=[{}]\n",
                serde_json::to_string(&provider)?
            ),
        )?;
        let mut project = Project::load(&root, &paths, &json!({}), json!({}))?;
        project.add_source_tree_filtered(&provider, |_| true)?;
        let before = ["a", "b"].map(|name| {
            let path = provider.join(format!("pkg/{name}.pyi"));
            (
                std::fs::read(&path).unwrap(),
                std::fs::metadata(path).unwrap().modified().unwrap(),
            )
        });
        let mut second = None;
        for retry in [false, true] {
            let mut sources: BTreeMap<_, _> = project
                .modules
                .values()
                .map(|module| {
                    (
                        PathBuf::from(&module.source.path),
                        module.source.text.clone(),
                    )
                })
                .collect();
            for name in if retry { &["b"][..] } else { &["a", "b"][..] } {
                sources.insert(provider.join(format!("pkg/{name}.pyi")), patch.to_owned());
            }
            let inputs = NativeCheckInputs {
                sources,
                paths: paths.clone(),
                generation: 0,
                virtual_types: None,
                support_sources: BTreeMap::new(),
            };
            let worker = NativeCliCheck::start(
                &binary,
                &root,
                &config,
                &paths,
                &project,
                std::slice::from_ref(&provider),
                Some(NativeCliProjection {
                    sources: &inputs.sources,
                    extras: &BTreeMap::new(),
                    prepared: None,
                    bootstrap_imports: false,
                }),
            )?
            .context("native attempt")?;
            let actual = worker
                .finish(&inputs, &config, &BTreeMap::new())
                .context("qualified attempt")?;
            if retry {
                second = Some(actual);
            } else {
                assert!(actual.is_empty(), "{actual:?}");
            }
        }
        let full = out.join("fresh-provider");
        write(&full.join("pkg/__init__.pyi"), "")?;
        write(&full.join("pkg/a.pyi"), original)?;
        write(&full.join("pkg/b.pyi"), patch)?;
        let reference = out.join("reference.toml");
        write(
            &reference,
            &format!(
                "[environment]\npython-version='3.13'\nextra-paths=[{}]\n",
                serde_json::to_string(&full)?
            ),
        )?;
        let expected = check_selected_files(&binary, &root, &reference, &paths)?;
        assert_eq!(expected.len(), 1, "{expected:?}");
        assert_eq!(
            second,
            Some(expected),
            "a discarded attempt must not leave stale provider patches"
        );
        for (index, name) in ["a", "b"].iter().enumerate() {
            let path = provider.join(format!("pkg/{name}.pyi"));
            assert_eq!(std::fs::read(&path)?, before[index].0);
            assert_eq!(std::fs::metadata(path)?.modified()?, before[index].1);
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn early_native_reuse_requires_observed_selected_source_snapshots() -> anyhow::Result<()> {
        use super::*;
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker"),
        );
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory = Cleanup(
            std::env::temp_dir().join(format!("ty-unobserved-native-{}", std::process::id())),
        );
        let root = directory.0.join("project");
        write(&root.join("main.py"), "value: int = 1\n")?;
        let root = root.canonicalize()?;
        let config = directory.0.join("output/checker.toml");
        write(&config, "[environment]\npython-version='3.13'\n")?;
        let paths = vec![root.join("main.py")];
        let empty = Project::load_selected(
            &root,
            &[],
            &json!({}),
            json!({}),
            true,
            crate::project::SourceLayout {
                roots: vec![root.clone()],
                overlays: &[],
                include: &|_| true,
                sparse_overlays: true,
                call_assignments_only: true,
                capture_original_revisions: true,
                known_source_revisions: None,
                preloaded: None,
            },
        )?;
        let prepared = PendingNativeCliPreparation::start(&root, &config, &paths, &[])?
            .finish(&empty)
            .context("pre-read metadata")?;
        let worker = NativeCliCheck::start(
            &binary,
            &root,
            &config,
            &paths,
            &empty,
            &[],
            Some(NativeCliProjection {
                sources: &BTreeMap::new(),
                extras: &BTreeMap::new(),
                prepared: Some(&prepared),
                bootstrap_imports: false,
            }),
        )?
        .context("pre-read native attempt")?;
        let inputs = NativeCheckInputs {
            sources: BTreeMap::new(),
            paths,
            generation: 0,
            virtual_types: None,
            support_sources: BTreeMap::new(),
        };
        assert!(
            worker.finish(&inputs, &config, &BTreeMap::new()).is_none(),
            "selected source snapshots must be observed before accepting native diagnostics"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn native_cli_reuse_matches_fresh_reports_and_rejects_changed_inputs() -> anyhow::Result<()> {
        use super::*;
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker"),
        );
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory =
            Cleanup(std::env::temp_dir().join(format!("ty-cli-reuse-{}", std::process::id())));
        for case in [
            "positive",
            "negative",
            "projection",
            "selection",
            "unknown-source",
            "support",
            "virtual",
            "config",
            "source",
            "dependency",
            "atomic-replace",
            "new-module",
            "new-nested-module",
            "extra-probe",
        ] {
            let root = directory.0.join(case);
            let out = directory.0.join(format!("{case}-output"));
            write(
                &root.join("main.py"),
                if case == "negative" {
                    "wrong: int = 'bad'\n"
                } else {
                    "import dependency\nvalue: int = dependency.value\n"
                },
            )?;
            write(&root.join("dependency.py"), "value: int = 1\n")?;
            std::fs::create_dir_all(root.join("empty/nested"))?;
            let root = root.canonicalize()?;
            let path = root.join("main.py");
            let config = out.join("checker.toml");
            write(&config, "[environment]\npython-version='3.13'\n")?;
            let project = Project::load(&root, std::slice::from_ref(&path), &json!({}), json!({}))?;
            let mut inputs = NativeCheckInputs {
                sources: project
                    .modules
                    .values()
                    .map(|module| {
                        (
                            PathBuf::from(&module.source.path),
                            module.source.text.clone(),
                        )
                    })
                    .collect(),
                paths: vec![path.clone()],
                generation: 0,
                virtual_types: None,
                support_sources: BTreeMap::new(),
            };
            let prepared = PendingNativeCliPreparation::start(&root, &config, &inputs.paths, &[])?
                .finish(&project)
                .context("prepared native inputs")?;
            let mut extras = BTreeMap::new();
            let worker = NativeCliCheck::start(
                &binary,
                &root,
                &config,
                &inputs.paths,
                &project,
                &[],
                Some(NativeCliProjection {
                    sources: &BTreeMap::new(),
                    extras: &extras,
                    prepared: Some(&prepared),
                    bootstrap_imports: false,
                }),
            )?
            .context("native CLI worker")?;
            match case {
                "projection" => {
                    inputs
                        .sources
                        .get_mut(&path)
                        .unwrap()
                        .push_str("# changed projection\n");
                }
                "selection" => inputs.paths.push(root.join("dependency.py")),
                "unknown-source" => {
                    inputs
                        .sources
                        .insert(root.join("unknown.py"), String::new());
                }
                "support" => {
                    inputs
                        .support_sources
                        .insert(out.join("support.py"), String::new());
                }
                "virtual" => inputs.virtual_types = Some(BTreeSet::from(["Virtual".into()])),
                "config" => write(&config, "[environment]\npython-version='3.12'\n")?,
                "source" => write(&path, "value: str = 'changed'\n")?,
                "dependency" => write(&root.join("dependency.py"), "value: str = 'changed'\n")?,
                "atomic-replace" => {
                    let replacement = out.join("replacement.py");
                    write(&replacement, &std::fs::read_to_string(&path)?)?;
                    std::fs::rename(replacement, &path)?;
                }
                "new-module" => write(&root.join("new.py"), "value: int = 1\n")?,
                "new-nested-module" => {
                    write(&root.join("empty/nested/new.py"), "value: int = 1\n")?
                }
                "extra-probe" => {
                    extras.insert(
                        out.join("type-imports.py"),
                        "import missing_probe_module\n".into(),
                    );
                }
                _ => {}
            }
            let actual = worker.finish(&inputs, &config, &extras);
            if matches!(case, "positive" | "negative") {
                let expected = check_selected_files(&binary, &root, &config, &inputs.paths)?;
                assert_eq!(actual, Some(expected), "{case}");
                assert_eq!(actual.unwrap().is_empty(), case == "positive");
            } else {
                assert!(
                    actual.is_none(),
                    "{case} unexpectedly reused stale evidence"
                );
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discovery_catalog_preserves_paths_sources_and_symlink_resolution() -> anyhow::Result<()> {
        use super::*;
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let directory =
            std::env::temp_dir().join(format!("ty-discovery-catalog-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let directory = Cleanup(directory.canonicalize()?);
        let root = directory.0.join("project");
        for relative in [
            "main.py",
            "pkg/__init__.py",
            "pkg/real.pyi",
            ".hidden/skip.py",
            "dist/skip.py",
        ] {
            write(&root.join(relative), "value: int = 1\n")?;
        }
        let outside = directory.0.join("outside.py");
        write(&outside, "value: str = 'external'\n")?;
        let alias = root.join("alias.py");
        let external = root.join("external.py");
        std::os::unix::fs::symlink(root.join("pkg/real.pyi"), &alias)?;
        std::os::unix::fs::symlink(&outside, &external)?;
        std::os::unix::fs::symlink(root.join("pkg"), root.join("linked-directory"))?;
        let mut original = Vec::new();
        files(&root, &mut original)?;
        let mut selected = Vec::new();
        let mut known = std::collections::HashSet::new();
        snapshot_files(&root, &mut selected, &mut known)?;
        original.sort();
        selected.sort();
        assert_eq!(selected, original);
        assert!(!known.contains(&alias));
        assert!(!known.contains(&external));
        assert!(known.contains(&root.join("main.py")));
        for path in &known {
            assert_eq!(path.canonicalize()?, *path);
        }
        let before =
            Project::preload_sources(&root, &selected, None).context("reference snapshot")?;
        let after =
            Project::preload_sources(&root, &selected, Some(&known)).context("catalog snapshot")?;
        assert_eq!(before.len(), after.len());
        for (name, expected) in before {
            let actual = &after[&name];
            assert_eq!(actual.path, expected.path);
            assert_eq!(actual.text, expected.text);
            assert_eq!(actual.revision, expected.revision);
        }
        Ok(())
    }

    #[test]
    fn hash_compact_paths_match_ordered_reference_for_selections_and_symlinks() -> anyhow::Result<()>
    {
        use super::*;
        fn ordered_reference(root: &Path, selected: &BTreeSet<PathBuf>) -> Result<Vec<PathBuf>> {
            fn visit(
                path: &Path,
                remaining: &mut BTreeSet<PathBuf>,
            ) -> Result<(bool, Vec<PathBuf>)> {
                if !remaining
                    .range(path.to_owned()..)
                    .next()
                    .is_some_and(|candidate| candidate.starts_with(path))
                {
                    return Ok((false, Vec::new()));
                }
                let metadata = std::fs::symlink_metadata(path)?;
                if !metadata.is_dir() {
                    let included = remaining.remove(path);
                    let discoverable = metadata.is_file()
                        && path
                            .extension()
                            .is_some_and(|extension| extension == "py" || extension == "pyi");
                    return Ok((
                        included && discoverable,
                        if included { vec![path.into()] } else { vec![] },
                    ));
                }
                let mut complete = true;
                let mut paths = Vec::new();
                for entry in std::fs::read_dir(path)? {
                    let (included, children) = visit(&entry?.path(), remaining)?;
                    complete &= included;
                    paths.extend(children);
                }
                if complete && !paths.is_empty() {
                    paths = vec![path.into()];
                }
                let traversable =
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_none_or(|name| {
                            !name.starts_with('.')
                                && !project_config::DEFAULT_EXCLUDED_DIRECTORIES.contains(&name)
                        });
                Ok((complete && traversable, paths))
            }
            let mut remaining = selected.clone();
            let (_, mut paths) = visit(root, &mut remaining)?;
            paths.extend(remaining);
            paths.sort();
            Ok(paths)
        }
        let directory =
            std::env::temp_dir().join(format!("ty-compact-paths-{}", std::process::id()));
        let root = directory.join("project");
        let result = (|| -> anyhow::Result<()> {
            for relative in [
                "a.py",
                "b.pyi",
                "pkg/a.py",
                "pkg/not_py.txt",
                ".hidden/a.py",
                "target/a.py",
                "pkg/child/c.py",
            ] {
                write(&root.join(relative), "value = 1\n")?;
            }
            let outside = directory.join("outside.py");
            write(&outside, "value = 2\n")?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside, root.join("alias.py"))?;
            #[cfg(not(unix))]
            write(&root.join("alias.py"), "value = 2\n")?;
            let mut inputs = [
                "a.py",
                "b.pyi",
                "pkg/a.py",
                "pkg/not_py.txt",
                ".hidden/a.py",
                "target/a.py",
                "pkg/child/c.py",
                "alias.py",
                "missing.py",
            ]
            .map(|relative| root.join(relative))
            .to_vec();
            inputs.push(outside);
            for mask in 0..(1u32 << inputs.len()) {
                let selected = inputs
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1 << index) != 0)
                    .map(|(_, path)| path.clone())
                    .collect();
                assert_eq!(
                    compact_check_paths(&root, &selected)?,
                    ordered_reference(&root, &selected)?,
                    "selection {mask}"
                );
            }
            for selected in [
                BTreeSet::from([root.clone()]),
                BTreeSet::from([root.join("pkg"), root.join("pkg/a.py")]),
            ] {
                assert_eq!(
                    compact_check_paths(&root, &selected)?,
                    ordered_reference(&root, &selected)?
                );
            }
            Ok(())
        })();
        if directory.exists() {
            std::fs::remove_dir_all(directory)?;
        }
        result
    }
    #[cfg(unix)]
    #[test]
    fn borrowed_overlay_preserves_provider_bytes_native_errors_and_reload_fallback()
    -> anyhow::Result<()> {
        borrowed_overlay_case(false)
    }

    #[cfg(unix)]
    #[test]
    fn borrowed_overlay_preserves_provider_after_function_index_inference() -> anyhow::Result<()> {
        borrowed_overlay_case(true)
    }

    #[cfg(unix)]
    fn borrowed_overlay_case(infer_functions: bool) -> anyhow::Result<()> {
        use super::*;
        let Ok(binary) = std::env::var("TY_COMPANION_TEST_OFFICIAL") else {
            return Ok(());
        };
        struct Temporary(PathBuf);
        impl Drop for Temporary {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "ty-borrowed-overlay-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory)?;
        let directory = Temporary(directory.canonicalize()?);
        let root = directory.0.join("project");
        let provider = directory.0.join("provider");
        write(
            &root.join("main.py"),
            "from typing import assert_type\nfrom api import Item\nfrom api.extra import value\nassert_type(value, bytes)\nvalid: str = Item().value\ninvalid: int = Item().value\nnative_error: int = 'wrong'\n",
        )?;
        if infer_functions {
            let path = root.join("main.py");
            let source = std::fs::read_to_string(&path)?;
            write(
                &path,
                &format!("{source}\ndef inferred_item():\n    return Item()\n"),
            )?;
        }
        for (relative, text) in [
            ("api/__init__.pyi", "class Item:\n    value: int\n"),
            ("api/extra.pyi", "value: bytes\n"),
            ("api/py.typed", "partial\n"),
        ] {
            let path = provider.join(relative);
            write(&path, text)?;
            let mut permissions = path.metadata()?.permissions();
            permissions.set_readonly(true);
            std::fs::set_permissions(&path, permissions)?;
        }
        let provider = provider.canonicalize()?;
        let files = [
            provider.join("api/__init__.pyi"),
            provider.join("api/extra.pyi"),
            provider.join("api/py.typed"),
        ];
        let originals = files
            .iter()
            .map(|path| Ok((std::fs::read(path)?, path.metadata()?.modified()?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let manifest = directory.0.join("plugin.json");
        let artifact = directory.0.join("plugin.py");
        write(&manifest,&json!({"id":"test.borrowed","name":"Borrowed","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"project-index":true,"class-transform":true,"instance-member":true,"stub-overlays":true},"claims":{"modules":[{"name":"api","recursive":true}],"classes":[{"kind":"exact","qualified-name":"api.Item"}],"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"api.Item","attribute-name":"value"}]}}).to_string())?;
        write(
            &artifact,
            "@on_project_index\ndef index(request):\n    return project_index(contributions=[contribution({}, instance_target('api.Item'), member_contribution(member('value', type_annotation('str'), mode='replace-existing')), 'api.Item.value')])\n",
        )?;
        let mut expected = None;
        let mut expected_raw_lines = None;
        for (case, borrow, warm, reload) in [
            ("copy", false, false, false),
            ("copy-warm", false, true, false),
            ("borrow", true, false, false),
            ("borrow-warm", true, true, false),
            ("reload", true, true, true),
        ] {
            let out = directory.0.join(case);
            let options = BTreeMap::from([
                ("out".into(), out.to_string_lossy().into_owned()),
                ("ty".into(), binary.clone()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("plugin".into(), artifact.to_string_lossy().into_owned()),
                ("stubs".into(), provider.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), "true".into()),
                ("public-cli".into(), "true".into()),
                ("native-warmup".into(), warm.to_string()),
                ("borrow-overlays".into(), borrow.to_string()),
            ]);
            let mut session = Session::new(&root, &options)?;
            assert_eq!(
                session.borrowed_overlay.as_ref(),
                borrow.then_some(&provider)
            );
            assert_eq!(session.stubs.join("api/__init__.pyi").exists(), !borrow);
            if reload {
                session.invalidate();
            }
            let result = session.check(&BTreeMap::new())?;
            if reload {
                assert!(session.borrowed_overlay.is_none());
                assert!(session.stubs.join("api/__init__.pyi").is_file());
            }
            if warm && !reload && !infer_functions {
                assert_eq!(result["native_warmup_reused"], true, "{case}: {result:#}");
                assert!(
                    result["native_warmup_analysis_passes"].as_u64().unwrap() > 0,
                    "flow must finish before joining native warmup: {case}: {result:#}"
                );
            }
            let diagnostics = result["diagnostics"].clone();
            assert_eq!(
                diagnostics.as_array().unwrap().len(),
                2,
                "{case}: {result:#}"
            );
            let lines = diagnostics
                .as_array()
                .unwrap()
                .iter()
                .map(|d| {
                    d["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .unwrap()
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(lines, BTreeSet::from([6, 7]), "{case}: {result:#}");
            if let Some(expected) = &expected {
                assert_eq!(&diagnostics, expected, "{case}");
            } else {
                expected = Some(diagnostics);
            }
            let native_sources: BTreeMap<_, _> = session
                .documents
                .iter()
                .map(|document| (document.path.clone(), document.rendered.text.clone()))
                .collect();
            let raw = session
                .server
                .as_mut()
                .context("native checker")?
                .check_workspace(&native_sources, &[root.join("main.py")])?;
            assert_eq!(raw.len(), 2, "workspace {case}: {raw:#?}");
            let raw_lines = raw
                .iter()
                .map(|d| {
                    d["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .unwrap()
                })
                .collect::<BTreeSet<_>>();
            if let Some(expected) = &expected_raw_lines {
                assert_eq!(&raw_lines, expected, "workspace {case}");
            } else {
                expected_raw_lines = Some(raw_lines);
            }
            drop(session);
            std::fs::remove_dir_all(&out)?;
            for (path, (bytes, modified)) in files.iter().zip(&originals) {
                assert_eq!(&std::fs::read(path)?, bytes);
                assert_eq!(&path.metadata()?.modified()?, modified);
            }
        }
        // A provider containing a standard-library shadow keeps filtering and copying.
        write(&provider.join("json/__init__.pyi"), "poison: int\n")?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory
                    .0
                    .join("stdlib-fallback")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("ty".into(), binary),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("plugin".into(), artifact.to_string_lossy().into_owned()),
            ("stubs".into(), provider.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("public-cli".into(), "true".into()),
            ("borrow-overlays".into(), "true".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        assert!(session.borrowed_overlay.is_none());
        assert!(!session.stubs.join("json").exists());
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(Some(&result["diagnostics"]), expected.as_ref());
        drop(session);
        Ok(())
    }
    fn native_rule(diagnostic: &serde_json::Value) -> &str {
        crate::native_rules::semantic_id(diagnostic["check_name"].as_str().unwrap())
    }

    #[test]
    fn native_constructor_views_preserve_positional_diagnostic_contract() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-constructor-positional-{}", std::process::id()));
        let root = directory.join("input");
        write(
            &root.join("demo.py"),
            "class Target: pass\nTarget('wrong')\n",
        )?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    return class_patch(constructor=callable_signature([], type_annotation('Self')))\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"constructor.positional","name":"Constructor positional","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Target"}]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let result = session.check(&BTreeMap::new())?;
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{result:#}");
        assert_eq!(
            diagnostics[0]["description"],
            "too-many-positional-arguments: Too many positional arguments: expected 0, got 1"
        );
        assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 2);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn contributed_fields_preserve_class_access_inheritance_and_assignment_contract() -> Result<()>
    {
        let directory = std::env::temp_dir().join(format!(
            "ty-contributed-native-fields-{}",
            std::process::id()
        ));
        let root = directory.join("input");
        let text = "from typing import assert_type\nclass Target: pass\nclass Child(Target): pass\ndef check(target: Target, child: Child) -> None:\n    assert_type(target.contributed, int)\n    assert_type(child.contributed, int)\n    assert_type(Target.contributed, int)\n    assert_type(Child.contributed, int)\n    bad: str = target.contributed  # control\n    target.contributed = 'wrong'  # control\n    target.contributed = 1\n    child.contributed = 2\n";
        write(&root.join("demo.py"), text)?;
        let modified = root.join("demo.py").metadata()?.modified()?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_project_index\ndef index(request):\n    return project_index(contributions=[contribution({}, instance_target('demo.Target'), field_contribution(field('contributed',type_annotation('int'),instance_set_type=type_annotation('str'),has_default=True)), 'demo.Target.contributed')])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"contributed.fields","name":"Contributed fields","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"project-index":true}}).to_string())?;
        for memory in [false, true] {
            let options = BTreeMap::from([
                (
                    "out".into(),
                    directory
                        .join(format!("output-{memory}"))
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), memory.to_string()),
                ("public-cli".into(), "true".into()),
            ]);
            let mut session = Session::new(&root, &options)?;
            session.runtime.use_site_fields = true;
            let result = session.check(&BTreeMap::new())?;
            assert!(!session.runtime.projects_fields_at_uses("demo.Target"));
            assert!(!session.runtime.projects_fields_at_uses("demo.Child"));
            let diagnostics = result["diagnostics"].as_array().unwrap();
            // The historical SDK makes this an instance-only contribution:
            // class access is rejected, including through an inherited class.
            assert_eq!(diagnostics.len(), 6, "{result:#}");
            assert_eq!(
                diagnostics
                    .iter()
                    .map(|d| (
                        d["location"]["positions"]["begin"]["line"]
                            .as_u64()
                            .unwrap(),
                        native_rule(d),
                    ))
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    (7, "type-assertion-failure"),
                    (7, "unresolved-attribute"),
                    (8, "type-assertion-failure"),
                    (8, "unresolved-attribute"),
                    (9, "invalid-assignment"),
                    (10, "invalid-assignment"),
                ]),
                "{result:#}"
            );
        }
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, text);
        assert_eq!(root.join("demo.py").metadata()?.modified()?, modified);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn nullable_projected_members_narrow_and_invalidate_on_calls_assignments_and_joins()
    -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-nullable-member-views-{}", std::process::id()));
        let root = directory.join("input");
        let text = "from typing import assert_type\nclass Item:\n    name: str\nclass Node: pass\ndef mutate(node: Node) -> None: pass\ndef example(node: Node) -> None:\n    alias = node\n    if node.parent is not None:\n        assert_type(node.parent, Item)\n        assert_type(alias.parent, Item)\n        name: str = node.parent.name\n    else:\n        assert_type(node.parent, None)\n    outside: Item = node.parent  # control\n    if None is node.parent:\n        assert_type(node.parent, None)\n    elif node.parent is not None:\n        assert_type(node.parent, Item)\n    result: str = node.parent.name if node.parent is not None else ''\n    if node.parent is not None:\n        mutate(node)\n        unsafe: Item = node.parent  # control\n    if node.parent is not None:\n        node.parent = None\n        changed: Item = node.parent  # control\ndef guarded(node: Node) -> None:\n    if node.parent is None:\n        return\n    assert_type(node.parent, Item)\n    assert_type(node.parent.name, str)\ndef asserted(node: Node) -> None:\n    assert node.parent is not None\n    assert_type(node.parent, Item)\n    assert_type(node.parent.name, str)\n";
        write(&root.join("demo.py"), text)?;
        let modified = root.join("demo.py").metadata()?.modified()?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    return class_patch(fields=[field_patch('parent', type_annotation('demo.Item | None'), instance_set_type=type_annotation('demo.Item | None'), has_default=True)])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"nullable.members","name":"Nullable members","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Node"}]}}).to_string())?;
        let expected: BTreeSet<_> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("# control"))
            .map(|(index, _)| index as u64 + 1)
            .collect();
        for (memory, dynamic) in [(false, false), (true, false), (false, true), (true, true)] {
            if dynamic {
                // Django's lazy member hooks keep their field index opaque;
                // there need not be a host-side class field patch at all.
                write(
                    &plugin,
                    "@on_instance_member\ndef field(request):\n    return member_response(member_patch('parent', member_descriptor(type_annotation('demo.Item | None'), instance_set_type=type_annotation('demo.Item | None')), mode='replace-existing'))\n",
                )?;
                write(&manifest, &json!({"id":"nullable.members","name":"Nullable members","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"instance-member":true},"claims":{"attributes":[{"kind":"exact","owner-qualified-name":"demo.Node","attribute-name":"parent","scope":"instance"}]}}).to_string())?;
            }
            let options = BTreeMap::from([
                (
                    "out".into(),
                    directory
                        .join(format!("output-{memory}-{dynamic}"))
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), memory.to_string()),
                ("public-cli".into(), "true".into()),
            ]);
            let mut session = Session::new(&root, &options)?;
            session.runtime.use_site_fields = true;
            let result = session.check(&BTreeMap::new())?;
            if dynamic {
                assert!(
                    !session.runtime.has_use_site_fields(),
                    "the dynamic-hook case must not depend on class field patches"
                );
            }
            let diagnostics = result["diagnostics"].as_array().unwrap();
            assert_eq!(diagnostics.len(), expected.len(), "{result:#}");
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d["check_name"] == "invalid-assignment"),
                "{result:#}"
            );
            assert_eq!(
                diagnostics
                    .iter()
                    .map(|d| d["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .unwrap())
                    .collect::<BTreeSet<_>>(),
                expected,
                "{result:#}"
            );
        }
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, text);
        assert_eq!(root.join("demo.py").metadata()?.modified()?, modified);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn nested_member_views_preserve_distinct_owners_and_native_controls() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-nested-member-views-{}", std::process::id()));
        let root = directory.join("input");
        let text = "from typing import assert_type\nclass User: pass\nclass Profile: pass\nuser = User()\nassert_type(user.profile.user.name, str)\nwrong: int = user.profile.user.name\nassert_type(User().profile.user.name, str)\n";
        write(&root.join("demo.py"), text)?;
        let modified = root.join("demo.py").metadata()?.modified()?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    if request['class']['qualified-name'] == 'demo.User':\n        return class_patch(fields=[field_patch('profile', type_annotation('demo.Profile'), has_default=True), field_patch('name', type_annotation('str'), has_default=True)])\n    return class_patch(fields=[field_patch('user', type_annotation('demo.User'), has_default=True)])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"nested.members","name":"Nested members","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.User"},{"kind":"exact","qualified-name":"demo.Profile"}]}}).to_string())?;
        for memory in [false, true] {
            let options = BTreeMap::from([
                (
                    "out".into(),
                    directory
                        .join(format!("output-{memory}"))
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), memory.to_string()),
                ("public-cli".into(), "true".into()),
            ]);
            let mut session = Session::new(&root, &options)?;
            session.runtime.use_site_fields = true;
            let result = session.check(&BTreeMap::new())?;
            let diagnostics = result["diagnostics"].as_array().unwrap();
            assert_eq!(diagnostics.len(), 1, "{result:#}");
            assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
            assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 6);
            assert!(
                diagnostics[0]["description"]
                    .as_str()
                    .unwrap()
                    .contains("`str` is not assignable to `int`")
            );
        }
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, text);
        assert_eq!(root.join("demo.py").metadata()?.modified()?, modified);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn opted_in_literal_returns_preserve_diagnostics_and_unknown_argument_fallback() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-early-returns-{}", std::process::id()));
        let root = directory.join("input");
        write(
            &root.join("model.pyi"),
            "from typing import ClassVar, Self\nclass Manager[T]:\n    def get(self, *, key: int) -> T: ...\nclass Base:\n    objects: ClassVar[Manager[Self]]\nclass Item(Base):\n    value: str\n",
        )?;
        let text = "from typing import assert_type\nfrom model import Item\nassert_type(Item.objects.get(key=1).value, str)\nwrong: int = Item.objects.get(key=2).value  # control\nItem.objects.get(key=0)  # control\nvariable = 1\nassert_type(Item.objects.get(key=variable).value, str)\n";
        write(&root.join("checks.py"), text)?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_call_return\ndef result(request):\n    diagnostics = []\n    if request.get('arguments') and request['arguments'][0].get('value', {}).get('value') == 0:\n        diagnostics.append(diagnostic('test.zero-key', 'Zero key', severity='error'))\n    return call_return_patch(type_annotation('model.Item'), diagnostics)\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"test.early","name":"Early returns","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},"claims":{"methods":[{"kind":"exact","class-qualified-name":"model.Manager","method-name":"get"}]},"default-config":{"companion-early-call-returns":["get"]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("public-cli".into(), "true".into()),
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let memory = session.memory_projection;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["reused_native_check"], false, "{result:#}");
        assert_eq!(
            result["early_call_returns"],
            if memory { 3 } else { 0 },
            "{result:#}"
        );
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 2, "{result:#}");
        let expected: BTreeSet<_> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("# control"))
            .map(|(i, _)| (i + 1) as u64)
            .collect();
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d["location"]["positions"]["begin"]["line"]
                    .as_u64()
                    .unwrap())
                .collect::<BTreeSet<_>>(),
            expected,
            "{result:#}"
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d["check_name"] == "invalid-assignment")
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d["check_name"] == "plugin-configuration")
        );
        assert_eq!(std::fs::read_to_string(root.join("checks.py"))?, text);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn unpatched_native_classes_preserve_native_errors_and_inherited_member_hooks() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-unpatched-native-{}", std::process::id()));
        let root = directory.join("input");
        let plain = "class Plain:\n    value = 'native'\nclass Child(Plain): pass\nwrong: int = 'native error'\n";
        let checks = "from typing import assert_type\nfrom plain import Child\nitem = Child()\nassert_type(item.dynamic, int)\nwrong: str = item.dynamic\nitem.value = 1\nclass Reader(Child):\n    def read(self) -> int:\n        return self.dynamic\n";
        write(&root.join("plain.py"), plain)?;
        write(&root.join("checks.py"), checks)?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_instance_member\ndef member_hook(request):\n    return member_response(member('dynamic', type_annotation('int'), mode='replace-existing'))\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"native.unpatched","name":"Unpatched classes","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"instance-member":true},"claims":{"attributes":[{"scope":"instance","kind":"on-subclass-of","owner-base-qualified-name":"plain.Plain","attribute-name":"dynamic"}]}}).to_string())?;
        for mode in ["false", "true"] {
            let options = BTreeMap::from([
                (
                    "out".into(),
                    directory.join(mode).to_string_lossy().into_owned(),
                ),
                ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), mode.into()),
                ("public-cli".into(), "true".into()),
            ]);
            let original = [&root.join("plain.py"), &root.join("checks.py")]
                .into_iter()
                .map(|path| {
                    Ok((
                        path.clone(),
                        std::fs::read(path)?,
                        path.metadata()?.modified()?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let mut session = Session::new(&root, &options)?;
            session.runtime.use_site_fields = true;
            let memory = session.memory_projection;
            let result = session.check(&BTreeMap::new())?;
            let diagnostics = result["diagnostics"].as_array().unwrap();
            let actual = diagnostics
                .iter()
                .map(|diagnostic| {
                    (
                        Path::new(diagnostic["location"]["path"].as_str().unwrap())
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .into_owned(),
                        diagnostic["location"]["positions"]["begin"]["line"]
                            .as_u64()
                            .unwrap(),
                        diagnostic["check_name"].as_str().unwrap().to_owned(),
                    )
                })
                .collect::<BTreeSet<_>>();
            assert_eq!(diagnostics.len(), 3, "{result:#}");
            assert_eq!(
                actual,
                BTreeSet::from([
                    ("plain.py".into(), 4, "invalid-assignment".into()),
                    ("checks.py".into(), 5, "invalid-assignment".into()),
                    ("checks.py".into(), 6, "invalid-assignment".into()),
                ]),
                "{result:#}"
            );
            if memory {
                assert_eq!(result["passthrough_units"], 1, "{result:#}");
                let document = session
                    .documents()
                    .into_iter()
                    .find(|document| Path::new(&document.source.path).ends_with("plain.py"))
                    .unwrap();
                assert_eq!(document.rendered.text, plain);
            }
            for (path, bytes, modified) in original {
                assert_eq!(std::fs::read(&path)?, bytes);
                assert_eq!(path.metadata()?.modified()?, modified);
            }
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn complete_preflight_preserves_native_errors_reveals_and_frozen_sources() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-complete-preflight-{}", std::process::id()));
        let root = directory.join("input");
        write(
            &root.join("model.pyi"),
            "from typing import ClassVar, Self\nclass Manager[T]:\n    def get(self, *, key: int) -> T: ...\nclass Base:\n    objects: ClassVar[Manager[Self]]\nclass Item(Base):\n    value: str\n",
        )?;
        let text = "from typing import assert_type, reveal_type\nfrom model import Item\nassert_type(Item.objects.get(key=1).value, str)\nwrong: int = Item.objects.get(key=2).value\nItem.objects.get(key=0)\nreveal_type(Item.objects.get(key=3).value)\n";
        write(&root.join("checks.py"), text)?;
        let plain = root.join("plain.py");
        write(&plain, "wrong: int = 'loaded snapshot'\n")?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_call_return\ndef result(request):\n    diagnostics = []\n    if request['arguments'][0].get('value', {}).get('value') == 0:\n        diagnostics.append(diagnostic('test.zero-key', 'Zero key', severity='error'))\n    return call_return_patch(type_annotation('model.Item'), diagnostics)\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"test.complete","name":"Complete preflight","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},"claims":{"methods":[{"kind":"exact","class-qualified-name":"model.Manager","method-name":"get"}]},"default-config":{"companion-early-call-returns":["get"]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("public-cli".into(), "true".into()),
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let memory = session.memory_projection;
        // The loaded snapshot must remain the native check's input even when
        // original files change before this one-shot check begins.
        write(&plain, "wrong: int = 1\n")?;
        let modified = plain.metadata()?.modified()?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["reused_native_check"], memory, "{result:#}");
        assert_eq!(result["early_call_returns"], if memory { 4 } else { 0 });
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), if memory { 4 } else { 3 }, "{result:#}");
        let mut checks: Vec<_> = diagnostics
            .iter()
            .map(|d| {
                (
                    Path::new(d["location"]["path"].as_str().unwrap())
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    d["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .unwrap(),
                    d["check_name"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        checks.sort();
        let mut expected = vec![
            ("checks.py".into(), 4, "invalid-assignment".into()),
            ("checks.py".into(), 5, "plugin-configuration".into()),
            ("checks.py".into(), 6, "revealed-type".into()),
        ];
        if memory {
            expected.push(("plain.py".into(), 1, "invalid-assignment".into()));
        }
        assert_eq!(checks, expected, "{result:#}");
        assert_eq!(std::fs::read_to_string(root.join("checks.py"))?, text);
        assert_eq!(std::fs::read_to_string(&plain)?, "wrong: int = 1\n");
        assert_eq!(plain.metadata()?.modified()?, modified);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn native_return_delegation_preserves_source_and_native_diagnostics() -> Result<()> {
        for delegate in [false, true] {
            for incorrect_native_result in [false, true] {
                let directory = std::env::temp_dir().join(format!(
                    "ty-native-return-{}-{delegate}-{incorrect_native_result}",
                    std::process::id()
                ));
                let root = directory.join("input");
                let result_type = if incorrect_native_result { "int" } else { "T" };
                write(
                    &root.join("model.pyi"),
                    &format!(
                        "from typing import ClassVar, Self\nclass Manager[T]:\n    def get(self, *, key: int) -> {result_type}: ...\nclass Base:\n    objects: ClassVar[Manager[Self]]\nclass Item(Base):\n    value: str\n"
                    ),
                )?;
                let text = "from typing import assert_type\nfrom model import Item\nassert_type(Item.objects.get(key=1).value, str)\nwrong: int = Item.objects.get(key=2).value\nItem.objects.get(key='bad')\n";
                write(&root.join("checks.py"), text)?;
                let plugin = directory.join("plugin.py");
                write(
                    &plugin,
                    "@on_call_return\ndef result(request):\n    return call_return_patch(type_annotation('model.Item'))\n",
                )?;
                let manifest = directory.join("plugin.json");
                write(&manifest, &json!({"id":"test.native-return","name":"Native return","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},"claims":{"methods":[{"kind":"exact","class-qualified-name":"model.Manager","method-name":"get"}]},"default-config":{"companion-early-call-returns":["get"],"companion-native-call-returns":if delegate {json!(["get"])} else {json!([])}}}).to_string())?;
                let binary = PathBuf::from(std::env::var("TY_COMPANION_TEST_OFFICIAL")?);
                let options = BTreeMap::from([
                    (
                        "out".into(),
                        directory.join("output").to_string_lossy().into_owned(),
                    ),
                    ("ty".into(), binary.to_string_lossy().into_owned()),
                    ("plugin".into(), plugin.to_string_lossy().into_owned()),
                    ("manifest".into(), manifest.to_string_lossy().into_owned()),
                    ("checker-mode".into(), "push".into()),
                    ("memory-projection".into(), "true".into()),
                    ("native-warmup".into(), "true".into()),
                    ("public-cli".into(), "true".into()),
                    ("python-version".into(), "3.13".into()),
                ]);
                let mut session = Session::new(&root, &options)?;
                session.runtime.use_site_fields = true;
                let native = delegate && session.memory_projection;
                let result = session.check(&BTreeMap::new())?;
                assert_eq!(
                    result["native_warmup_reused"], session.memory_projection,
                    "{result:#}"
                );
                assert_eq!(
                    result["native_call_returns"],
                    if native { 3 } else { 0 },
                    "{result:#}"
                );
                let diagnostics = result["diagnostics"].as_array().unwrap();
                if native {
                    let documents = session.documents();
                    let document = documents
                        .iter()
                        .find(|d| d.source.path.ends_with("checks.py"))
                        .unwrap();
                    assert_eq!(document.rendered.text, text, "{result:#}");
                    let config = directory.join("native.toml");
                    write(&config, "[environment]\npython-version='3.13'\n")?;
                    let baseline = check(&binary, &root, &config, &[root.join("checks.py")])?;
                    let normalize = |values: &[Value]| {
                        let mut values: Vec<_> = values.iter().map(|d| json!({
                            "code":d["check_name"],"description":d["description"],"severity":d["severity"],"positions":d["location"]["positions"]
                        })).collect();
                        values.sort_by_key(Value::to_string);
                        values
                    };
                    assert_eq!(normalize(diagnostics), normalize(&baseline), "{result:#}");
                    if incorrect_native_result {
                        assert!(diagnostics.iter().any(|d| crate::native_rules::semantic_id(
                            d["check_name"].as_str().unwrap()
                        ) == "unresolved-attribute"));
                    }
                } else {
                    assert_eq!(diagnostics.len(), 2, "{result:#}");
                    assert_eq!(
                        diagnostics
                            .iter()
                            .map(|d| d["location"]["positions"]["begin"]["line"]
                                .as_u64()
                                .unwrap())
                            .collect::<BTreeSet<_>>(),
                        BTreeSet::from([4, 5])
                    );
                }
                assert_eq!(std::fs::read_to_string(root.join("checks.py"))?, text);
                drop(session);
                std::fs::remove_dir_all(directory)?;
            }
        }
        Ok(())
    }

    #[test]
    fn native_initializers_preserve_native_errors_and_observed_operations() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-native-initializers-{}", std::process::id()));
        let root = directory.join("input");
        write(
            &root.join("library.pyi"),
            "TOKEN: int\nclass Item: pass\ndef make(*, count: int = 0, sentinel: int = 0) -> Item: ...\ndef effect() -> Item: ...\n",
        )?;
        let text = "from typing import assert_type\nimport library\nclass Plain:\n    field = library.make(count=1, sentinel=library.TOKEN)\n    typed: library.Item = library.make(count=2)\n    bad = library.make(count='wrong')  # control\nclass Read:\n    field = library.make()\n    alias = field\n    assert_type(alias.dynamic, int)\nclass Nested:\n    field = library.make(count=library.effect().dynamic)\nclass Shadowed:\n    library = library.make()\n    field = library.dynamic\n    assert_type(field, int)\ndef declares():\n    class Inner:\n        field = library.make(count=1)\nordinary: int = 'wrong'  # control\n";
        write(&root.join("checks.py"), text)?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_instance_member\ndef member_hook(request):\n    return member_response(member('dynamic',type_annotation('int')))\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"test.initializers","name":"Initializers","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"instance-member":true,"call-state":true},"claims":{"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"library.Item","attribute-name":"dynamic"}]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("public-cli".into(), "true".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let result = session.check(&BTreeMap::new())?;
        let expected: BTreeSet<_> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("# control"))
            .map(|(i, _)| (i + 1) as u64)
            .collect();
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), expected.len(), "{result:#}");
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d["location"]["positions"]["begin"]["line"]
                    .as_u64()
                    .unwrap())
                .collect::<BTreeSet<_>>(),
            expected,
            "{result:#}"
        );
        assert_eq!(std::fs::read_to_string(root.join("checks.py"))?, text);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn fresh_memory_cli_preserves_loaded_snapshot_and_checks_passthrough_sources() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-fresh-source-snapshot-{}", std::process::id()));
        let root = directory.join("input");
        let file = root.join("demo.py");
        let loaded = "value: int = 'loaded snapshot'\n";
        let updated = "value: int = 1\n";
        write(&file, loaded)?;
        let same_size = root.join("same_size.py");
        let replaced = root.join("replacement.py");
        let untouched = root.join("untouched.py");
        for path in [&same_size, &replaced, &untouched] {
            write(path, loaded)?;
        }
        let original_time = std::fs::metadata(&same_size)?.modified()?;
        let replacement_time = std::fs::metadata(&replaced)?.modified()?;
        #[cfg(unix)]
        let original_inode = {
            use std::os::unix::fs::MetadataExt as _;
            std::fs::metadata(&replaced)?.ino()
        };
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_project_index\ndef index(request):\n    return project_index({})\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"test.frozen-source","name":"Frozen source","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"project-index":true}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("public-cli".into(), "true".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let memory = session.memory_projection;
        let revision_count = session
            .base_project
            .modules
            .values()
            .filter(|module| module.original_revision.is_some())
            .count();
        write(&file, updated)?;
        let padded = format!(
            "value: int = 1{}\n",
            " ".repeat(loaded.len() - updated.len())
        );
        assert_eq!(padded.len(), loaded.len());
        write(&same_size, &padded)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&same_size)?
            .set_modified(original_time)?;
        let replacement = directory.join("replacement.tmp");
        write(&replacement, &padded)?;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&replacement)?
            .set_modified(replacement_time)?;
        std::fs::rename(replacement, &replaced)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            assert_ne!(std::fs::metadata(&replaced)?.ino(), original_inode);
        }
        let result = session.check(&BTreeMap::new())?;
        let diagnostics = result["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), if memory { 4 } else { 1 }, "{result:#}");
        if memory {
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic["check_name"] == "invalid-assignment")
            );
            assert_eq!(result["passthrough_units"], 4);
            assert_eq!(session.documents()[0].source.text, loaded);
            assert!(session.cached_context.is_none());
            assert_eq!(result["incremental_context_saved"], false);
            assert_eq!(result["native_source_cache_seeds"], revision_count);
            if revision_count == 4 {
                // The unchanged file uses loaded bytes. All three changed
                // files read through, including equal size/mtime and replacement.
                assert_eq!(result["original_source_reads"], 3);
            }
        }
        assert_eq!(std::fs::read_to_string(&file)?, updated);
        assert_eq!(std::fs::read_to_string(&same_size)?, padded);
        assert_eq!(std::fs::read_to_string(&replaced)?, padded);
        assert_eq!(std::fs::read_to_string(&untouched)?, loaded);
        assert_eq!(std::fs::metadata(&same_size)?.modified()?, original_time);
        assert_eq!(std::fs::metadata(&replaced)?.modified()?, replacement_time);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn use_site_fields_preserve_class_bindings_and_check_reads_writes_and_constructors()
    -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-use-site-fields-{}", std::process::id()));
        let root = directory.join("input");
        let model = "class Record:\n    value = 1\nclass Child(Record):\n    pass\nclass CustomInit(Child):\n    def __init__(self, count: int) -> None: pass\n";
        write(&root.join("model.py"), model)?;
        write(
            &root.join("checks.py"),
            "from typing import assert_type\nfrom model import Child, CustomInit, Record\nassert_type(Record.value, int)\nassert_type(Record().value, str)\nRecord(value='ok')\nRecord(value=1)\nRecord(other='bad')\nitem = Record()\nitem.value = 2\nitem.value = 'bad'\nChild(value='ok')\nChild(value=1)\nassert_type(Child().value, str)\nCustomInit(1)\nCustomInit('bad')\n",
        )?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    if request['class']['qualified-name'] != 'model.Record':\n        return None\n    return class_patch(fields=[field('value',type_annotation('str'),mode='replace-existing',instance_set_type=type_annotation('int'),constructor_parameter=keyword_only('value',type_annotation('str'),required=False),has_default=True)])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"test.use-sites","name":"Use site fields","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"model.Record"}]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), binary),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let result = session.check(&BTreeMap::new())?;
        let diagnostics = result["diagnostics"].as_array().unwrap();
        let locations: BTreeSet<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                diagnostic["location"]["positions"]["begin"]["line"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(locations, BTreeSet::from([6, 7, 10, 12, 15]), "{result:#}");
        assert_eq!(diagnostics.len(), 5, "{result:#}");
        let projected = session
            .documents()
            .into_iter()
            .find(|document| document.source.path.ends_with("model.py"))
            .unwrap();
        assert!(
            projected.rendered.text.contains("value = 1"),
            "{}",
            projected.rendered.text
        );
        assert_eq!(
            projected.rendered.text.matches("def __init__").count(),
            1,
            "Keep the user-defined initializer without synthesizing another"
        );
        assert!(!projected.rendered.text.contains("field_value"));
        assert_eq!(std::fs::read_to_string(root.join("model.py"))?, model);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn native_member_hooks_check_setters_constructors_and_branch_defaults() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-native-member-hooks-{}", std::process::id()));
        let root = directory.join("input");
        write(
            &root.join("model.py"),
            "class Record:\n    value: int = 0\n    def save(self) -> None: pass\n",
        )?;
        let checks = "from typing import assert_type\nfrom model import Record\nimport model as imported\ndef check(item: Record) -> None:\n    assert_type(item.value, int | None)\n    item.value = None\n    item.value = 'bad'  # control\n    row = Record(value=1)\n    try:\n        row.save()\n    except ValueError:\n        pass\n    assert_type(row.value, int | None)\nRecord(value='bad')  # control\nRecord(unknown=1)  # control\nassert_type(imported.Record().value, None)\ndef shadow(imported: Record) -> None:\n    assert_type(imported.value, int | None)\n    imported.value = 'bad'  # control\nfresh = Record()\nassert_type(fresh.generated, None)\nfresh.save()\nassert_type(fresh.generated, int)\n";
        write(&root.join("checks.py"), checks)?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_instance_member\ndef field(request):\n    return member_response(member_patch(request['member-name'], member_descriptor(type_annotation('int | None'), instance_set_type=type_annotation('int | None')), mode='replace-existing'))\n@on_call_signature\ndef signature(request):\n    if request.get('receiver') is None:\n        return call_signature_patch(callable_signature([keyword_only('value',type_annotation('int | None'),required=False)],type_annotation('model.Record')))\n@on_call_state\ndef state(request):\n    if request.get('receiver') is None:\n        return call_state_patch(result_members={'value':type_annotation('None'),'generated':type_annotation('None')},fresh_result=True)\n    return call_state_patch(receiver_members={'value':type_annotation('int'),'generated':type_annotation('int')})\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"test.native-members","name":"Native members","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"instance-member":true,"call-signature":true,"call-state":true},"claims":{"constructors":[{"kind":"exact","qualified-name":"model.Record"}],"methods":[{"kind":"exact","class-qualified-name":"model.Record","method-name":"save"}],"attributes":[{"kind":"exact","owner-qualified-name":"model.Record","attribute-name":"value","scope":"instance"},{"kind":"exact","owner-qualified-name":"model.Record","attribute-name":"generated","scope":"instance"}]}}).to_string())?;
        let options = BTreeMap::from([
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("ty".into(), std::env::var("TY_COMPANION_TEST_OFFICIAL")?),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        session.runtime.use_site_fields = true;
        let result = session.check(&BTreeMap::new())?;
        let diagnostics = result["diagnostics"].as_array().unwrap();
        let locations: BTreeSet<_> = diagnostics
            .iter()
            .map(|diagnostic| {
                diagnostic["location"]["positions"]["begin"]["line"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        let expected = checks
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains("# control"))
            .map(|(index, _)| (index + 1) as u64)
            .collect();
        assert_eq!(locations, expected, "{result:#}");
        assert_eq!(diagnostics.len(), 4, "{result:#}");
        for entry in std::fs::read_dir(directory.join("output"))? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("oracle-") && name.ends_with(".json"))
            {
                let raw = std::fs::read_to_string(&path)?;
                assert!(
                    !raw.contains("<module 'model'>"),
                    "Unnecessary module observation: {}",
                    path.display()
                );
            }
        }
        assert_eq!(std::fs::read_to_string(root.join("checks.py"))?, checks);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn parallel_copies_keep_overlay_precedence_and_original_bytes() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "ty-parallel-copies-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let higher = directory.join("higher");
        let lower = directory.join("lower");
        let output = directory.join("output");
        for index in 0..160 {
            write(
                &higher.join(format!("nested/file{index}.pyi")),
                "value: str\n",
            )?;
            write(
                &lower.join(format!("nested/file{index}.pyi")),
                "value: int\n",
            )?;
        }
        let original = higher.join("nested/file0.pyi");
        let permissions = std::fs::metadata(&original)?.permissions();
        let mut read_only = permissions.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(&original, read_only)?;
        let copied = copy_overlays(&[higher.clone(), lower], &output, &BTreeSet::new())?;
        assert_eq!(copied.len(), 160);
        for path in copied {
            assert_eq!(std::fs::read_to_string(path)?, "value: str\n");
        }
        std::fs::write(output.join("nested/file0.pyi"), "value: bytes\n")?;
        assert_eq!(std::fs::read_to_string(&original)?, "value: str\n");
        assert!(std::fs::metadata(&original)?.permissions().readonly());
        std::fs::set_permissions(original, permissions)?;
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn compact_disk_check_preserves_exact_selection_and_rule_overrides() -> Result<()> {
        let binary = PathBuf::from(std::env::var("TY_COMPANION_TEST_OFFICIAL")?);
        let directory = std::env::temp_dir()
            .canonicalize()?
            .join(format!("ty-compact-selection-{}", std::process::id()));
        let root = directory.join("project");
        let bad = "value: int = 'wrong'\n";
        let files = [
            ("all/a.py", "value: int = 1\n"),
            ("all/[literal].py", bad),
            ("partial/selected.py", bad),
            ("partial/skip.py", bad),
            (".hidden/script.py", "# /// script\n# ///\nvalue: int = 1\n"),
            (".venv/explicit.py", bad),
            ("complete/plain.py", "value: int = 1\n"),
            ("complete/.venv/explicit.py", bad),
        ];
        for (name, text) in files {
            write(&root.join(name), text)?;
        }
        write(&root.join(".gitignore"), "*\n")?;
        let selected: BTreeSet<_> = files
            .into_iter()
            .filter(|(name, _)| !name.ends_with("skip.py"))
            .map(|(name, _)| root.join(name))
            .collect();
        let compact = compact_check_paths(&root, &selected)?;
        assert!(compact.contains(&root.join("all")));
        assert!(compact.contains(&root.join("partial/selected.py")));
        assert!(!compact.contains(&root));
        assert!(!compact.contains(&root.join("partial")));
        assert!(!compact.contains(&root.join("complete")));
        assert!(compact.contains(&root.join("complete/.venv")));
        let config = directory.join("checker.toml");
        write(
            &config,
            "[environment]\npython-version='3.13'\n[src]\ninclude=['missing']\nexclude=['**']\nrespect-ignore-files=true\nexclude-scripts=true\n[[overrides]]\ninclude=['**/partial/**']\n[overrides.rules]\ninvalid-assignment='warn'\n",
        )?;
        let result = check_selected_files(
            &binary,
            &root,
            &config,
            &selected.into_iter().collect::<Vec<_>>(),
        )?;
        let actual: BTreeMap<_, _> = result
            .iter()
            .map(|d| {
                assert_eq!(d["check_name"], "invalid-assignment");
                (path_of(d, &root), d["severity"].clone())
            })
            .collect();
        assert_eq!(actual.len(), 4, "{result:#?}");
        assert!(actual.contains_key(&root.join("all/[literal].py")));
        assert!(actual.contains_key(&root.join(".venv/explicit.py")));
        assert!(actual.contains_key(&root.join("complete/.venv/explicit.py")));
        assert_eq!(actual[&root.join("partial/selected.py")], "minor");
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn dictionary_probe_context_is_recovered_and_reset_after_signature_edits() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-dict-context-{}", std::process::id()));
        let root = directory.join("project");
        let source = "from typing import Any, TypedDict, assert_type\nclass Model: pass\nclass Row(TypedDict):\n    age: Model\ndef consume(value: dict[str, Any]) -> object: return value\nassert_type(consume({'age': Model()}), dict[str, Model])\n";
        write(&root.join("demo.py"), source)?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_call_return_of('demo.consume')\ndef consume(request):\n    args = positional_arguments(request)\n    return call_return_patch(argument_type(args[0])) if args and argument_type(args[0]) else None\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"dictionary","name":"Dictionary context regression","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},"claims":{"functions":[{"qualified-name":"demo.consume"}]}}).to_string())?;
        let options = BTreeMap::from([
            ("ty".into(), binary),
            (
                "out".into(),
                directory.join("output").to_string_lossy().into_owned(),
            ),
            ("plugin".into(), plugin.to_string_lossy().into_owned()),
            ("manifest".into(), manifest.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        // Keep the dictionary at identical offsets while changing its expected
        // type to a TypedDict. A previous context-free decision must expire.
        let changed = source
            .replace(
                "value: dict[str, Any]",
                &format!("value: {:width$}", "Row", width = "dict[str, Any]".len()),
            )
            .replace(", dict[str, Model])", ", Row)");
        assert_eq!(
            source.find("{'age': Model()}"),
            changed.find("{'age': Model()}")
        );
        let path = root.join("demo.py").canonicalize()?;
        let result = session.check(&BTreeMap::from([(path, changed)]))?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, source);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn nested_subclass_workaround_preserves_unrelated_authored_overrides() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-nested-override-{}", std::process::id()));
        let root = directory.join("project");
        let source = "class Parent:\n    field: int = 1\n    class Meta: pass\nclass Valid(Parent):\n    class Meta(Parent.Meta): pass\nclass Wrong(Parent):\n    field: str = ''\n    class Meta: pass\n";
        write(&root.join("demo.py"), source)?;
        let modified = root.join("demo.py").metadata()?.modified()?;
        // Older supported ty releases do not implement these override rules.
        // Preserve every authored error the selected official checker reports;
        // only its false positive for the explicit nested subclass may vanish.
        let native_config = directory.join("native.toml");
        write(&native_config, "[environment]\npython-version='3.13'\n")?;
        let native = check(
            Path::new(&binary),
            &root,
            &native_config,
            &[root.join("demo.py")],
        )?;
        let signature = |item: &Value| {
            json!({"code":item["check_name"],"description":item["description"],"positions":item["location"]["positions"]})
                .to_string()
        };
        let mut expected = Vec::new();
        for diagnostic in native {
            assert_eq!(native_rule(&diagnostic), "invalid-attribute-override");
            let line = diagnostic["location"]["positions"]["begin"]["line"]
                .as_u64()
                .unwrap();
            assert!([5, 7, 8].contains(&line), "{diagnostic}");
            if line != 5 {
                expected.push(signature(&diagnostic));
            }
        }
        expected.sort();
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    return {'kind': 'no-change'}\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"nested","name":"Nested override regression","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Parent"}]}}).to_string())?;
        for mode in ["cli", "push", "memory"] {
            let options = BTreeMap::from([
                ("ty".into(), binary.clone()),
                (
                    "out".into(),
                    directory.join(mode).to_string_lossy().into_owned(),
                ),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                (
                    "checker-mode".into(),
                    if mode == "memory" { "push" } else { mode }.into(),
                ),
                ("memory-projection".into(), (mode == "memory").to_string()),
                ("python-version".into(), "3.13".into()),
            ]);
            let result = Session::new(&root, &options)?.check(&BTreeMap::new())?;
            let diagnostics = result["diagnostics"]
                .as_array()
                .context("nested override diagnostics")?;
            let mut actual = diagnostics.iter().map(signature).collect::<Vec<_>>();
            actual.sort();
            assert_eq!(actual, expected, "{mode}: {result:#}");
        }
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, source);
        assert_eq!(root.join("demo.py").metadata()?.modified()?, modified);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn memory_projection_survives_unavailable_import_restarts() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-memory-restart-{}", std::process::id()));
        let root = directory.join("project");
        let source = "from typing import assert_type\nfrom manager import Base\nclass Model(Base): pass\nassert_type(Model().number, int)\noptional = Model().optional\nassert_type(Model.objects.get(key=1), Model)\nwrong: int = 'wrong'\n";
        write(&root.join("demo.py"), source)?;
        write(
            &root.join("manager.pyi"),
            "from typing import ClassVar, Self\nclass Manager[T]:\n    def get(self, *, key: int) -> T: ...\nclass Base:\n    objects: ClassVar[Manager[Self]]\n",
        )?;
        write(&root.join("schema.py"), "class Schema[T, U]: pass\n")?;
        let modified = root.join("demo.py").metadata()?.modified()?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    number = {'expression':'int', 'mode':'annotation', 'snapshot':{'kind':'nominal', 'qualified-name':'schema.Schema', 'arguments':[{'kind':'nominal', 'qualified-name':'builtins.str'}]}}\n    return class_patch(fields=[field_patch('number', number, has_default=True), field_patch('optional', type_annotation('missing_companion_dependency.Value'), has_default=True)])\n@on_call_return\ndef result(request):\n    return call_return_patch(type_annotation('demo.Model'))\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"restart","name":"Restart regression","version":"0.0.0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.86.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true,"call-return":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Model"}],"methods":[{"kind":"exact","class-qualified-name":"manager.Manager","method-name":"get"}]},"default-config":{"companion-early-call-returns":["get"]}}).to_string())?;
        for (memory, fresh) in [(false, false), (true, false), (true, true)] {
            let options = BTreeMap::from([
                ("ty".into(), binary.clone()),
                (
                    "out".into(),
                    directory
                        .join(format!("output-{memory}-{fresh}"))
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), memory.to_string()),
                ("public-cli".into(), fresh.to_string()),
                ("python-version".into(), "3.13".into()),
            ]);
            let mut session = Session::new(&root, &options)?;
            if fresh {
                session.runtime.use_site_fields = true;
            }
            let result = session.check(&BTreeMap::new())?;
            if fresh && session.memory_projection {
                assert_eq!(result["early_call_returns"], 1, "{result:#}");
                assert!(
                    result["projection_rounds"].as_u64().unwrap() > 1,
                    "{result:#}"
                );
            }
            assert_eq!(
                session.base_project.snapshot_nominals.get("schema.Schema"),
                Some(&Some(2)),
                "a missing import must not allow an invalid class specialization to bypass native validation"
            );
            assert!(
                session
                    .base_project
                    .unavailable_modules
                    .contains("missing_companion_dependency"),
                "{result:#}"
            );
            let diagnostics = result["diagnostics"]
                .as_array()
                .context("restart diagnostics")?;
            assert_eq!(diagnostics.len(), 1, "{result:#}");
            assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
            assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 7);
        }
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, source);
        assert_eq!(root.join("demo.py").metadata()?.modified()?, modified);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn imported_generic_members_preserve_same_named_classes_end_to_end() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-imported-generics-{}", std::process::id()));
        let root = directory.join("project");
        for (module, code) in [
            (
                "library",
                "class Manager[T]:\n    def get(self) -> object: ...\n",
            ),
            ("left", "class Record:\n    value: str = ''\n"),
            ("right", "class Record:\n    value: int = 0\n"),
        ] {
            write(&root.join(format!("{module}.py")), code)?;
        }
        let source = "from typing import Any, assert_type\nimport left as one\nimport right as two\nassert_type(one.Record.objects.get().value, str)\nassert_type(two.Record.objects.get().value, int)\ndef aliased():\n    record = one.Record\n    assert_type(record.objects.get().value, str)\ndef shadowed(one: Any):\n    assert_type(one.Record.objects.get(), Any)\nwrong: int = one.Record.objects.get().value\nwrong_other: str = two.Record.objects.get().value\n";
        write(&root.join("demo.py"), source)?;
        let modified = root.join("demo.py").metadata()?.modified()?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    model = request['class']['qualified-name']\n    return class_patch(class_members=[member('objects', type_annotation('library.Manager[' + model + ']'), mode='replace-existing')])\n@on_call_return\ndef returns(request):\n    return call_return_patch(request['receiver']['generic-arguments'][0])\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest, &json!({"id":"qualified-members","name":"Qualified members","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true,"call-return":true},"claims":{"classes":[{"kind":"exact","qualified-name":"left.Record"},{"kind":"exact","qualified-name":"right.Record"}],"methods":[{"kind":"exact","class-qualified-name":"library.Manager","method-name":"get"}]}}).to_string())?;
        for memory in [false, true] {
            let options = BTreeMap::from([
                ("ty".into(), binary.clone()),
                (
                    "out".into(),
                    directory
                        .join(format!("output-{memory}"))
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("memory-projection".into(), memory.to_string()),
                ("python-version".into(), "3.13".into()),
            ]);
            let result = Session::new(&root, &options)?.check(&BTreeMap::new())?;
            let mut positions = result["diagnostics"]
                .as_array()
                .context("imported member diagnostics")?
                .iter()
                .map(|diagnostic| {
                    assert_eq!(diagnostic["check_name"], "invalid-assignment", "{result:#}");
                    diagnostic["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .unwrap()
                })
                .collect::<Vec<_>>();
            positions.sort();
            assert_eq!(positions, [11, 12], "{result:#}");
        }
        assert_eq!(std::fs::read_to_string(root.join("demo.py"))?, source);
        assert_eq!(root.join("demo.py").metadata()?.modified()?, modified);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
    fn native_cli_and_lsp_preserve_canonical_self_import_identity() -> Result<()> {
        let binary = PathBuf::from(
            std::env::var_os("TY_COMPANION_TEST_OFFICIAL").context("official checker")?,
        );
        let directory =
            std::env::temp_dir().join(format!("ty-canonical-self-import-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let path = directory.join("demo.py");
        let source = "from typing import assert_type\nimport demo as imported\nclass Model: pass\nclass Generic[T]: pass\nassert_type(imported.Model(), Model)\nassert_type(imported.Generic[Model](), Generic[Model])\nwrong: str = imported.Model()\n";
        write(&path, source)?;
        let config = directory.join("ty.toml");
        write(
            &config,
            &format!(
                "[environment]\npython-version = '3.13'\nroot = [{}]\nextra-paths = [{}]\n",
                serde_json::to_string(&directory)?,
                serde_json::to_string(&directory)?,
            ),
        )?;
        let cli = check(&binary, &directory, &config, std::slice::from_ref(&path))?;
        let mut server = crate::checker::ProjectChecker::new(&binary, &directory, &config)?;
        let lsp = server.check(
            &BTreeMap::from([(path.clone(), source.to_owned())]),
            std::slice::from_ref(&path),
        )?;
        for (mode, diagnostics) in [("cli", cli), ("lsp", lsp)] {
            assert_eq!(diagnostics.len(), 1, "{mode}: {diagnostics:#?}");
            let diagnostic = &diagnostics[0];
            assert_eq!(diagnostic["check_name"], "invalid-assignment", "{mode}");
            assert_eq!(
                diagnostic["location"]["positions"]["begin"]["line"], 7,
                "{mode}"
            );
            assert_eq!(path_of(diagnostic, &directory), path, "{mode}");
        }
        drop(server);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn setter_receiver_views_keep_factory_return_identity() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory =
            std::env::temp_dir().join(format!("ty-setter-receiver-cycle-{}", std::process::id()));
        let root = directory.join("project");
        let plain = "# Ordinary files keep their original bytes and diagnostics.\nvalue: int = 1\n";
        write(&root.join("plain.py"), plain)?;
        write(
            &root.join("demo.py"),
            "class Model: pass\ndef make() -> Model: return Model()\nmodel = make()\nmodel.value = 'first'\nmodel.value = 'second'\n",
        )?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_class_transform\ndef transform(request):\n    return {'kind':'class-patch','fields':[{'name':'value','mode':'fill-on-miss','instance-get-type':{'expression':'int','mode':'annotation'},'instance-set-type':{'expression':'str','mode':'annotation'},'has-default':True}]}\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest,&json!({"id":"setter.identity","name":"Setter identity","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Model"}]}}).to_string())?;
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
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert_eq!(
            std::fs::read_to_string(session.view.join("plain.py"))?,
            plain
        );
        let path = root.join("demo.py").canonicalize()?;
        let invalid = std::fs::read_to_string(&path)?.replace("'second'", "17");
        let result = session.check(&BTreeMap::from([(path, invalid)]))?;
        let diagnostics = result["diagnostics"]
            .as_array()
            .context("setter diagnostics")?;
        assert_eq!(diagnostics.len(), 1, "{result:#}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
        assert_eq!(
            std::fs::read_to_string(session.view.join("plain.py"))?,
            plain
        );
        let invalid_plain = plain.replace("= 1", "= 'wrong'");
        let result = session.check(&BTreeMap::from([(
            root.join("plain.py").canonicalize()?,
            invalid_plain.clone(),
        )]))?;
        assert_eq!(
            std::fs::read_to_string(session.view.join("plain.py"))?,
            invalid_plain
        );
        assert_eq!(
            result["diagnostics"].as_array().unwrap().len(),
            1,
            "{result:#}"
        );
        assert_eq!(result["diagnostics"][0]["check_name"], "invalid-assignment");
        assert_eq!(std::fs::read_to_string(root.join("plain.py"))?, plain);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn nominal_snapshot_cache_tracks_equal_length_generic_header_edits() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-nominal-snapshot-revision-{}",
            std::process::id()
        ));
        let root = directory.join("project");
        let first = "class Model[T, U]: pass\n";
        let second = "class Model[TYPE]: pass\n";
        assert_eq!(first.len(), second.len());
        write(&root.join("model.py"), first)?;
        write(
            &root.join("demo.py"),
            "from typing import assert_type\nfrom model import Model\ndef load() -> object: return object()\ndef later(value: object) -> object: return object()\nassert_type(load(), bytes)\nassert_type(later(load()), None)\n",
        )?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            "@on_call_return\ndef returns(request):\n    if request['callee']['expression'] == 'demo.later':\n        argument = request['arguments'][0].get('type-expr', {}).get('expression', 'Unknown')\n        if argument in ('object', 'Unknown'):\n            return {'kind':'no-change'}\n        return {'kind':'call-return-patch','return-type':{'expression':'None','mode':'annotation','snapshot':{'kind':'nominal','qualified-name':'types.NoneType','arguments':[]}}}\n    return {'kind':'call-return-patch','return-type':{'expression':'bytes','mode':'annotation','snapshot':{'kind':'nominal','qualified-name':'model.Model','arguments':[{'kind':'nominal','qualified-name':'builtins.str'}]}}}\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(&manifest,&json!({"id":"snapshot.revision","name":"Snapshot revision","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},"claims":{"functions":[{"qualified-name":"demo.load"},{"qualified-name":"demo.later"}]}}).to_string())?;
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
            ("python-version".into(), "3.13".into()),
        ]);
        let mut session = Session::new(&root, &options)?;
        assert_eq!(session.check(&BTreeMap::new())?["diagnostics"], json!([]));
        let preflights = std::fs::read_dir(directory.join("output"))?
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name();
                let round = name
                    .to_str()?
                    .strip_prefix("nominal-snapshots-")?
                    .strip_suffix(".json")?
                    .parse::<usize>()
                    .ok()?;
                Some((round, entry.path()))
            })
            .collect::<BTreeMap<_, _>>();
        let first_preflight = preflights
            .first_key_value()
            .context("native nominal preflight")?
            .1;
        let preflight: Value = serde_json::from_slice(&std::fs::read(first_preflight)?)?;
        assert!(
            preflight.as_array().unwrap().iter().any(|diagnostic| {
                diagnostic["description"]
                    .as_str()
                    .is_some_and(|description| description.contains("<class 'int'>"))
            }),
            "first native preflight must establish common builtins before later hook snapshots need them"
        );
        assert_eq!(
            session.base_project.snapshot_nominals.get("builtins.int"),
            Some(&Some(0))
        );
        assert!(
            preflight.as_array().unwrap().iter().any(|diagnostic| {
                diagnostic["description"]
                    .as_str()
                    .is_some_and(|description| description.contains("<class 'NoneType'>"))
            }),
            "the first native preflight must establish NoneType before the later hook can return its snapshot"
        );
        assert_eq!(
            session.base_project.snapshot_nominals.get("types.NoneType"),
            Some(&Some(0))
        );
        assert_eq!(
            preflights.len(),
            1,
            "the later NoneType response must reuse native proof without another preflight"
        );
        let path = root.join("model.py").canonicalize()?;
        let changed = session.check(&BTreeMap::from([(path.clone(), second.to_owned())]))?;
        let diagnostics = changed["diagnostics"].as_array().context("diagnostics")?;
        assert_eq!(diagnostics.len(), 1, "{changed:#}");
        assert_eq!(diagnostics[0]["check_name"], "type-assertion-failure");
        assert!(
            diagnostics[0]["description"]
                .as_str()
                .unwrap()
                .contains("Model[str]")
        );
        for invalid in [first, "class Model: pass\n", "Model = int(0)\n"] {
            // Both a wrong generic arity and a callable/non-class must retain
            // the fallback annotation. Positive native confirmation must never
            // suppress the reprojection that these negative cases require.
            assert_eq!(
                session.check(&BTreeMap::from([(path.clone(), invalid.to_owned())]))?["diagnostics"],
                json!([]),
                "{invalid}"
            );
        }
        let restored = session.check(&BTreeMap::from([(path, second.to_owned())]))?;
        assert_eq!(
            restored["diagnostics"].as_array().unwrap().len(),
            1,
            "{restored:#}"
        );
        assert!(
            restored["diagnostics"][0]["description"]
                .as_str()
                .unwrap()
                .contains("Model[str]")
        );
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
    use super::*;

    #[test]
    fn descriptor_origins_use_annotated_aliases_without_guessing_short_names() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("ty-descriptor-origins-{}", std::process::id()));
        write(&directory.join("owner.py"), "class Target: pass\n")?;
        let text = "from owner import Target as Alias\ndef check(target: Alias, unknown):\n    target.field = 'bad'\n    unknown.field = 'bad'\n    target = unknown\n    target.field = 'bad'\n";
        write(&directory.join("demo.py"), text)?;
        let project = Project::load(&directory, &[], &json!({}), json!({}))?;
        let module = &project.modules["demo"];
        let unit = File {
            source: module.source.clone(),
            suite: module.suite.clone(),
            analysis: Analysis::default(),
            path: directory.join("demo.py"),
            oracle: Oracle::new(),
            patches: ReturnPatches::new(),
            return_types: BTreeMap::new(),
            early_returns: BTreeMap::new(),
            types: Types::with_project("_test_", "demo", &project),
            diagnostics: vec![],
            rendered: None,
            selected: true,
            valid: true,
            names: BTreeMap::new(),
            final_edits: None,
            dirty: true,
            passthrough: false,
            raw_diagnostics: vec![],
            flow_diagnostics: vec![],
            pass: None,
            reads_protocol_aliases: false,
        };
        let generated = GeneratedDeclarations {
            descriptors: BTreeMap::from([(
                "owner.Target".into(),
                BTreeSet::from(["field".into()]),
            )]),
            ..Default::default()
        };
        let virtual_names = project.virtual_identities.clone();
        let nominal_names = Arc::new(BTreeMap::from([("Target".into(), "owner.Target".into())]));
        let origins = diagnostic_origins(
            &unit,
            &project,
            &generated,
            None,
            &virtual_names,
            &nominal_names,
        );
        assert!(Arc::ptr_eq(&origins.virtual_names, &virtual_names));
        assert!(Arc::ptr_eq(&origins.nominal_names, &nominal_names));
        assert_eq!(origins.descriptor_assignments.len(), 1);
        let found = &origins.descriptor_assignments[0];
        assert_eq!(found.owner, "owner.Target");
        assert_eq!(found.target.start, text.find("target.field").unwrap());
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn function_hover_metadata_excludes_documentation_and_preserves_embedded_fences() {
        let plain = json!({"contents":{"kind":"plaintext","value":"def load() -> int\n---------------------------------------------\nDocumentation"}});
        assert_eq!(hover_type(&plain), Some("def load() -> int"));
        let markdown = json!({"contents":{"kind":"markdown","value":"````python\nLiteral[\"```\"]\n````\n---\nDocumentation"}});
        assert_eq!(hover_type(&markdown), Some("Literal[\"```\"]"));
        assert_eq!(hover_type(&Value::Null), None);
        assert_eq!(
            hover_type(&json!({"contents":{"kind":"markdown","value":"Documentation only"}})),
            None
        );
    }

    #[test]
    fn overlay_precedence_and_partial_markers_survive_editor_reloads() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory = std::env::temp_dir().join(format!(
            "ty-overlay-precedence-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("project");
        let stubs = directory.join("overlays");
        let site = directory.join("environment/lib/python3.13/site-packages");
        let windows_site = directory.join("environment/Lib/site-packages");
        let file_extra = directory.join("file-extra");
        let inline_extra = directory.join("inline-extra");
        for (path, text) in [
            (root.join("first.py"), "value: int = 1\n"),
            (stubs.join("first.pyi"), "value: str\n"),
            (
                stubs.join("json/__init__.pyi"),
                "def dumps(value: object) -> int: ...\n",
            ),
            (stubs.join("partial/__init__.pyi"), ""),
            (stubs.join("partial/py.typed"), "partial\n"),
            (stubs.join("partial/overlay.pyi"), "value: str\n"),
            (site.join("partial/__init__.py"), ""),
            (site.join("partial/runtime.py"), "value: bytes = b''\n"),
            (windows_site.join("partial/__init__.py"), ""),
            (
                windows_site.join("partial/runtime.py"),
                "value: bytes = b''\n",
            ),
            (stubs.join("file_priority.pyi"), "value: int\n"),
            (stubs.join("inline_priority.pyi"), "value: int\n"),
            (root.join("inline_priority.py"), "value: int = 1\n"),
            (file_extra.join("file_priority.pyi"), "value: bytes\n"),
            (inline_extra.join("file_priority.pyi"), "value: str\n"),
            (inline_extra.join("inline_priority.pyi"), "value: str\n"),
            (
                directory.join("environment/pyvenv.cfg"),
                "home = .\nversion_info = 3.13.0\ninclude-system-site-packages = false\n",
            ),
            (directory.join("plugin.py"), "# Overlay-only plugin.\n"),
        ] {
            write(&path, text)?;
        }
        let source = "from typing import assert_type\nimport first, json, file_priority, inline_priority\nfrom partial import overlay, runtime\nassert_type(first.value, int)\nassert_type(json.dumps(1), str)\nassert_type(overlay.value, str)\nassert_type(runtime.value, bytes)\nassert_type(file_priority.value, bytes)\nassert_type(inline_priority.value, str)\n";
        write(&root.join("main.py"), source)?;
        write(
            &root.join("ty.toml"),
            &format!(
                "[environment]\npython-version='3.13'\npython={}\nextra-paths=[{}]\n",
                serde_json::to_string(&directory.join("environment"))?,
                serde_json::to_string(&file_extra)?
            ),
        )?;
        write(&directory.join("plugin.json"), &json!({"id":"research.overlays","name":"Overlays","version":"0.0.1","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"stub-overlays":true}}).to_string())?;
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
                directory.join("plugin.json").to_string_lossy().into_owned(),
            ),
            (
                "plugin".into(),
                directory.join("plugin.py").to_string_lossy().into_owned(),
            ),
            ("stubs".into(), stubs.to_string_lossy().into_owned()),
            ("checker-mode".into(), "push".into()),
            ("lsp-capabilities".into(), "{}".into()),
            (
                "lsp-workspace-configuration".into(),
                json!({"configuration":{"environment":{"extra-paths":[inline_extra]}}}).to_string(),
            ),
        ]);
        let mut session = Session::new(&root, &options)?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert!(!session.stubs.join("json").exists());
        assert!(!session.base_project.modules.contains_key("json"));
        assert_eq!(
            std::fs::read_to_string(session.stubs.join("partial/py.typed"))?,
            "partial\n"
        );
        // A changed overlay invalidates its inferred type without touching the
        // original document. Restoring it restores the original diagnostic set.
        write(&stubs.join("partial/overlay.pyi"), "value: int\n")?;
        session.invalidate();
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(
            result["diagnostics"].as_array().unwrap().len(),
            1,
            "{result:#}"
        );
        write(&stubs.join("partial/overlay.pyi"), "value: str\n")?;
        std::fs::remove_file(stubs.join("partial/py.typed"))?;
        session.invalidate();
        let result = session.check(&BTreeMap::new())?;
        assert!(
            result["diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| matches!(
                    d["check_name"].as_str(),
                    Some("unresolved-import" | "import-not-found")
                )),
            "{result:#}"
        );
        assert!(!session.stubs.join("partial/py.typed").exists());
        write(&stubs.join("partial/py.typed"), "partial\n")?;
        session.invalidate();
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert_eq!(std::fs::read_to_string(root.join("main.py"))?, source);
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn third_party_overlay_declarations_skip_only_declared_native_probes() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory =
            std::env::temp_dir().join(format!("ty-overlay-declaration-{}", std::process::id()));
        let root = directory.join("project");
        let stubs = directory.join("overlays");
        write(
            &root.join("ty.toml"),
            "[environment]\npython-version='3.13'\n",
        )?;
        write(&stubs.join("django-stubs/__init__.pyi"), "value: str\n")?;
        write(&stubs.join("django-stubs/db/models.pyi"), "value: str\n")?;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
        let options = BTreeMap::from([
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            ("stubs".into(), stubs.to_string_lossy().into_owned()),
            (
                "plugin".into(),
                fixtures.join("plugin.py").to_string_lossy().into_owned(),
            ),
            (
                "manifest".into(),
                fixtures
                    .join("plugin.plugin.json")
                    .to_string_lossy()
                    .into_owned(),
            ),
        ]);
        let config = project_config::load(
            &root,
            &directory.join("view"),
            &directory.join("target"),
            &options,
        )?;
        let declared = BTreeSet::from(["django".into()]);
        let skipped = directory.join("skipped");
        assert!(stdlib_overlay_modules(&config, &options, &skipped, &declared)?.is_empty());
        assert!(!skipped.join("overlay-resolution").exists());
        assert!(
            stdlib_overlay_modules(
                &config,
                &options,
                &directory.join("unclaimed"),
                &BTreeSet::new()
            )?
            .is_empty()
        );
        assert!(
            directory
                .join("unclaimed/overlay-resolution/stdlib-probe.py")
                .exists()
        );
        write(&stubs.join("json/__init__.pyi"), "value: str\n")?;
        let mixed = directory.join("mixed");
        assert_eq!(
            stdlib_overlay_modules(&config, &options, &mixed, &declared)?,
            BTreeSet::from(["json".into()])
        );
        assert_eq!(
            std::fs::read_to_string(mixed.join("overlay-resolution/stdlib-probe.py"))?,
            "import json\n"
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn overlay_stdlib_probe_honors_custom_typeshed_version_ranges() -> Result<()> {
        let binary = std::env::var_os("TY_COMPANION_TEST_OFFICIAL").expect("official checker");
        let directory =
            std::env::temp_dir().join(format!("ty-overlay-stdlib-{}", std::process::id()));
        let root = directory.join("project");
        let stubs = directory.join("overlays");
        let typeshed = directory.join("typeshed");
        let out = directory.join("output");
        write(&stubs.join("future_module.pyi"), "value: str\n")?;
        write(
            &typeshed.join("stdlib/VERSIONS"),
            "builtins: 3.0-\nfuture_module: 3.13-\n",
        )?;
        write(&typeshed.join("stdlib/builtins.pyi"), "class object: ...\n")?;
        write(&typeshed.join("stdlib/future_module.pyi"), "value: int\n")?;
        write(
            &root.join("ty.toml"),
            &format!(
                "[environment]\ntypeshed={}\npython-version='3.12'\n",
                serde_json::to_string(&typeshed)?
            ),
        )?;
        let mut options = BTreeMap::from([
            (
                "ty".into(),
                PathBuf::from(binary).to_string_lossy().into_owned(),
            ),
            ("stubs".into(), stubs.to_string_lossy().into_owned()),
        ]);
        // Explicit legacy stubs are accepted only alongside a plugin registration.
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
        options.insert(
            "plugin".into(),
            fixtures.join("plugin.py").to_string_lossy().into_owned(),
        );
        options.insert(
            "manifest".into(),
            fixtures
                .join("plugin.plugin.json")
                .to_string_lossy()
                .into_owned(),
        );
        let config =
            project_config::load(&root, &out.join("project"), &out.join("stubs"), &options)?;
        assert!(
            stdlib_overlay_modules(
                &config,
                &options,
                &out,
                &BTreeSet::from(["future_module".into()])
            )?
            .is_empty()
        );
        options.insert("python-version".into(), "3.13".into());
        let config =
            project_config::load(&root, &out.join("project"), &out.join("stubs"), &options)?;
        assert_eq!(
            stdlib_overlay_modules(
                &config,
                &options,
                &out,
                &BTreeSet::from(["future_module".into()])
            )?,
            BTreeSet::from(["future_module".into()])
        );
        options.insert("out".into(), out.to_string_lossy().into_owned());
        let mut session = Session::new(&root, &options)?;
        let versions = typeshed.join("stdlib/VERSIONS").canonicalize()?;
        assert!(session.watch_paths().contains(&typeshed.canonicalize()?));
        assert!(session.path_requires_reload(&versions));
        assert!(!session.stubs.join("future_module.pyi").exists());
        write(&versions, "builtins: 3.0-\nfuture_module: 3.14-\n")?;
        session.invalidate();
        session.reload_dependencies()?;
        assert!(session.stubs.join("future_module.pyi").exists());
        write(&versions, "builtins: 3.0-\nfuture_module: 3.13-\n")?;
        session.invalidate();
        session.reload_dependencies()?;
        assert!(!session.stubs.join("future_module.pyi").exists());
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn resolved_import_uris_restore_originals_and_reject_generated_dependencies() -> Result<()> {
        let directory = std::env::temp_dir().join(format!(
            "ty-resolution-uri-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        for name in [
            "generated/project/local.py",
            "generated/stubs/overlay.pyi",
            "generated/stubs/indexed.pyi",
            "generated/stubs/unknown.pyi",
            "originals/overlay.pyi",
            "originals/indexed-source.pyi",
            "external.py",
        ] {
            write(&directory.join(name), "class Base: ...\n")?;
        }
        let directory = directory.canonicalize()?;
        let view = directory.join("generated/project");
        let stubs = directory.join("generated/stubs");
        let originals = directory.join("originals");
        let indexed = originals.join("indexed-source.pyi");
        let resolved = BTreeMap::from([(indexed.clone(), PathBuf::from("indexed.pyi"))]);

        // Windows URI decoding yields a non-verbatim path. On Unix a directory
        // alias exercises the same lexical mismatch without needing Windows.
        #[cfg(unix)]
        let generated = {
            let alias = directory.join("generated-alias");
            std::os::unix::fs::symlink(directory.join("generated"), &alias)?;
            alias
        };
        #[cfg(not(unix))]
        let generated = directory.join("generated");
        let local_uri = crate::checker::file_uri(&generated.join("project/local.py"))?;
        let decoded = crate::lsp_mapping::decode_file_uri(&local_uri)?.unwrap();
        assert!(
            !decoded.starts_with(&view),
            "the regression must exercise different URI and canonical path spellings"
        );
        let map = |uri: &str| {
            resolved_import_path(
                uri,
                &view,
                &stubs,
                std::slice::from_ref(&originals),
                &resolved,
            )
        };
        assert_eq!(map(&local_uri)?, None);
        for (name, expected) in [
            ("overlay.pyi", Some(originals.join("overlay.pyi"))),
            ("indexed.pyi", Some(indexed)),
            ("unknown.pyi", None),
        ] {
            let uri = crate::checker::file_uri(&generated.join("stubs").join(name))?;
            assert_eq!(map(&uri)?, expected, "{name}");
        }
        let external = directory.join("external.py");
        assert_eq!(map(&crate::checker::file_uri(&external)?)?, Some(external));
        assert_eq!(map("untitled:demo.py")?, None);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    #[cfg(not(windows))]
    fn memory_projection_preserves_rich_diagnostics_for_explicit_paths() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-memory-rich-diagnostics-{}", std::process::id()));
        let root = directory.join("input [source] café");
        let output = directory.join("output");
        let source = root.join("missing.py");
        let text = "import missing_companion_rich_diagnostics\n";
        write(&source, text)?;
        write(&root.join("ty.toml"), "[src]\ninclude=['missing.py']\n")?;
        write(&root.join("excluded.py"), "this is !!! invalid python\n")?;
        let modified = source.metadata()?.modified()?;
        let options = BTreeMap::from([
            ("out".into(), output.to_string_lossy().into_owned()),
            ("ty".into(), binary),
            ("checker-mode".into(), "push".into()),
            ("memory-projection".into(), "true".into()),
            ("public-cli".into(), "true".into()),
            (
                "explicit-files".into(),
                serde_json::to_string(&vec![source.canonicalize()?])?,
            ),
        ]);
        let result = (|| -> Result<()> {
            let mut session = Session::new(&root, &options)?;
            assert!(session.memory_projection);
            assert!(output.join("environment-probe/probe.py").is_file());
            assert!(!directory.join("environment-probe").exists());
            let result = session.check(&BTreeMap::new())?;
            let diagnostics = result["diagnostics"].as_array().unwrap();
            assert_eq!(diagnostics.len(), 1, "{result:#}");
            assert!(matches!(
                diagnostics[0]["check_name"].as_str(),
                Some("import-not-found" | "unresolved-import")
            ));
            let rich = session.rich_diagnostics()?;
            assert_eq!(rich.len(), 1, "{rich:#?}");
            assert_eq!(rich[0]["check_name"], diagnostics[0]["check_name"]);
            assert_eq!(rich[0]["location"], diagnostics[0]["location"]);
            assert_eq!(session.rich_diagnostics()?, rich);
            assert_eq!(std::fs::read_to_string(&source)?, text);
            assert_eq!(source.metadata()?.modified()?, modified);
            Ok(())
        })();
        std::fs::remove_dir_all(directory)?;
        result
    }

    #[test]
    #[cfg(not(windows))]
    fn memory_projection_falls_back_for_per_file_overrides() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory =
            std::env::temp_dir().join(format!("ty-memory-override-{}", std::process::id()));
        let root = directory.join("input source");
        let source = root.join("demo.py");
        let text = "wrong: int = 'bad'\n";
        write(&source, text)?;
        let modified = source.metadata()?.modified()?;
        let result = (|| -> Result<()> {
            for from_cli in [false, true] {
                let config = if from_cli {
                    "[environment]\npython-version='3.13'\n"
                } else {
                    "[environment]\npython-version='3.13'\n[[overrides]]\ninclude=['demo.py']\n[overrides.rules]\ninvalid-assignment='warn'\n"
                };
                write(&root.join("ty.toml"), config)?;
                let mut options = BTreeMap::from([
                    (
                        "out".into(),
                        directory
                            .join(from_cli.to_string())
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    ("ty".into(), binary.clone()),
                    ("checker-mode".into(), "push".into()),
                    ("memory-projection".into(), "true".into()),
                    ("public-cli".into(), "true".into()),
                ]);
                if from_cli {
                    options.insert(
                        "cli-config".into(),
                        serde_json::to_string(&vec![
                            "overrides=[{include=['demo.py'],rules.invalid-assignment='warn'}]",
                        ])?,
                    );
                }
                let mut session = Session::new(&root, &options)?;
                assert!(!session.memory_projection);
                let result = session.check(&BTreeMap::new())?;
                let diagnostics = result["diagnostics"].as_array().unwrap();
                assert_eq!(diagnostics.len(), 1, "{result:#}");
                assert_eq!(diagnostics[0]["check_name"], "invalid-assignment");
                assert_eq!(diagnostics[0]["severity"], "minor");
            }
            assert_eq!(std::fs::read_to_string(&source)?, text);
            assert_eq!(source.metadata()?.modified()?, modified);
            Ok(())
        })();
        std::fs::remove_dir_all(directory)?;
        result
    }

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
    fn structural_return_nominal_probes_work_before_typing_reveal_type_exists() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory = std::env::temp_dir().join(format!(
            "ty-snapshot-old-target-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("input");
        std::fs::create_dir_all(&root)?;
        write(
            &root.join("models.py"),
            "from typing_extensions import assert_type\ndef row() -> object: return {}\nassert_type(row()['title'], str)\nwrong: int = row()['title']\n",
        )?;
        let manifest = directory.join("plugin.json");
        let plugin = directory.join("plugin.py");
        write(&manifest, &json!({"id":"test.old-target", "name":"Old target snapshot", "version":"0.0.0", "protocol-version":{"major":0,"minor":7}, "ty-compatibility":{"requirement":">=0.84.0"}, "runtime":{"kind":"monty","artifact":"plugin.py"}, "capabilities":{"call-return":true}, "claims":{"functions":[{"qualified-name":"models.row"}]}}).to_string())?;
        write(
            &plugin,
            "@on_call_return\ndef returns(request):\n    return call_return_patch({'expression':'object', 'mode':'annotation', 'snapshot':{'kind':'typed-dict', 'fields':[{'name':'title', 'type-snapshot':{'kind':'nominal', 'qualified-name':'builtins.str', 'arguments':[]}, 'required':True, 'read-only':False}], 'closed':True}})\n",
        )?;
        for version in ["3.10", "3.14"] {
            let mut session = Session::new(
                &root,
                &BTreeMap::from([
                    ("ty".into(), binary.clone()),
                    (
                        "out".into(),
                        directory.join(version).to_string_lossy().into_owned(),
                    ),
                    ("plugin".into(), plugin.to_string_lossy().into_owned()),
                    ("manifest".into(), manifest.to_string_lossy().into_owned()),
                    ("python-version".into(), version.into()),
                    ("checker-mode".into(), "push".into()),
                ]),
            )?;
            let result = session.check(&BTreeMap::new())?;
            let diagnostics = result["diagnostics"].as_array().unwrap();
            assert_eq!(diagnostics.len(), 1, "{version}: {result:#}");
            assert_eq!(
                diagnostics[0]["check_name"], "invalid-assignment",
                "{version}: {result:#}"
            );
            assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 4);
            assert!(session.base_project.snapshot_nominals["builtins.str"].is_some());
        }
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
            "import middle as bridge\nfrom demo import item\ntyped: int = bridge.alias\nlabel: str = item.label\n",
        )?;
        write(
            &root.join("demo.py"),
            "class Resource: pass\nitem = Resource()\nlabel: str = item.label\ndef trigger_index_preflight() -> None: pass\n",
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
            "class Resource:\n    broken: int\nitem = Resource()\nlabel: str = item.label\n".into(),
        )]);
        let fallback = session.check(&failing)?;
        assert_eq!(
            fallback["diagnostics"].as_array().unwrap().len(),
            2,
            "{fallback:#}"
        );
        for diagnostic in fallback["diagnostics"].as_array().unwrap() {
            assert_eq!(native_rule(diagnostic), "unresolved-attribute");
        }
        assert_eq!(
            session.check(&failing)?["diagnostics"],
            fallback["diagnostics"],
            "recoverable hook failure must not reuse the former successful patch"
        );
        assert_eq!(session.check(&BTreeMap::new())?["diagnostics"], json!([]));
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn projection_inputs_observe_cross_module_alias_changes_without_local_changes() -> Result<()> {
        let source = Arc::new(Source::new(
            "import producer\nvalue = producer.record\n".into(),
            "consumer.py".into(),
            "consumer".into(),
        ));
        let unit = File {
            source,
            suite: Arc::new(Vec::new()),
            analysis: Analysis::default(),
            path: "consumer.py".into(),
            oracle: BTreeMap::from([("expr:24:39".into(), "producer.Generated".into())]),
            patches: ReturnPatches::new(),
            return_types: BTreeMap::new(),
            early_returns: BTreeMap::new(),
            types: Types::new("_consumer_", "consumer"),
            diagnostics: Vec::new(),
            rendered: None,
            selected: true,
            valid: true,
            names: BTreeMap::new(),
            final_edits: None,
            dirty: true,
            passthrough: false,
            raw_diagnostics: Vec::new(),
            flow_diagnostics: Vec::new(),
            pass: None,
            reads_protocol_aliases: true,
        };
        let aliases = |field: &str| {
            BTreeMap::from([(
                "Generated".into(),
                json!({
                    "expression":"<TypedDict with items item>", "mode":"annotation",
                    "snapshot":{"kind":"typed-dict", "closed":true, "fields":[{
                        "name":"item", "required":true, "read-only":false,
                        "type-snapshot":{"kind":"nominal", "qualified-name":field, "arguments":[]}
                    }]}
                }),
            )])
        };
        let mut runtime = Runtime::from_plugins(&[])?;
        runtime.register_protocol_types("producer", "generated_", aliases("builtins.str"));
        let before = ProjectionInputs::new(&unit, &runtime);
        assert_eq!(
            runtime.type_expr(&unit.source, "producer.Generated")["snapshot"]["fields"][0]["type-snapshot"]
                ["qualified-name"],
            "builtins.str"
        );
        runtime.register_protocol_types("producer", "generated_", aliases("builtins.str"));
        assert!(before == ProjectionInputs::new(&unit, &runtime));

        runtime.register_protocol_types("producer", "generated_", aliases("builtins.int"));
        let after = ProjectionInputs::new(&unit, &runtime);
        assert_eq!(before.oracle, after.oracle);
        assert_eq!(before.patches, after.patches);
        assert_eq!(before.return_types, after.return_types);
        assert_eq!(before.prelude, after.prelude);
        assert!(
            before != after,
            "changed producer aliases must reject a cached consumer pass"
        );
        assert_eq!(
            runtime.type_expr(&unit.source, "producer.Generated")["snapshot"]["fields"][0]["type-snapshot"]
                ["qualified-name"],
            "builtins.int"
        );
        Ok(())
    }

    #[test]
    fn projection_reuse_invalidates_cross_module_aliases_with_unchanged_spelling() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory = std::env::temp_dir().join(format!(
            "ty-cross-module-alias-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("project");
        let consumer = root.join("a_consumer.py");
        // ty displays structural aliases without their originating module. Bind
        // the generated name explicitly so this internal fixture exercises
        // cache invalidation through supported qualified-alias resolution.
        let source = "from z_producer import row, _ty_ext_result_snapshot_0\ndef consume(row: object, check: object) -> None: pass\nconsume(row, 1)\n";
        write(&consumer, source)?;
        write(&root.join("b_unrelated.py"), "class Unrelated: pass\n")?;
        write(
            &root.join("z_producer.py"),
            "def seed() -> object: return 1\ndef advance(value: object) -> object: return value\ndef make(value: object) -> object: return {'item': value}\nrow = make(advance(advance(seed())))\n",
        )?;
        let manifest = directory.join("plugin.json");
        write(
            &manifest,
            &json!({
                "id":"research.cross-module-alias", "name":"Cross-module alias", "version":"0.0.1",
                "protocol-version":{"major":0,"minor":7}, "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
                "runtime":{"kind":"monty","artifact":"plugin.py"},
                "capabilities":{"call-return":true,"call-signature":true},
                "claims":{"functions":[
                    {"qualified-name":"z_producer.seed"}, {"qualified-name":"z_producer.advance"},
                    {"qualified-name":"z_producer.make"}, {"qualified-name":"a_consumer.consume"}
                ]}
            }).to_string(),
        )?;
        let plugin = directory.join("plugin.py");
        write(
            &plugin,
            r#"@on_call_return
def returns(request):
    callee = request['callee']['expression']
    if callee == 'z_producer.seed':
        return call_return_patch(type_annotation('int'))
    arguments = request.get('arguments', [])
    argument = arguments[0].get('type-expr') if arguments else None
    if callee == 'z_producer.advance' and argument:
        return call_return_patch(argument)
    if callee == 'z_producer.make':
        snapshot = argument.get('snapshot', {}) if argument else {}
        concrete = snapshot.get('qualified-name') == 'builtins.int'
        item = {'kind':'nominal', 'qualified-name':'builtins.int' if concrete else 'builtins.object', 'arguments':[]}
        return call_return_patch({'expression':'object', 'mode':'annotation', 'snapshot':{
            'kind':'typed-dict', 'fields':[{'name':'item', 'type-snapshot':item, 'required':True, 'read-only':False}], 'closed':True
        }})
    return None

@on_call_signature
def signature(request):
    if request['callee']['expression'] != 'a_consumer.consume':
        return None
    arguments = request.get('arguments', [])
    snapshot = arguments[0].get('type-expr', {}).get('snapshot', {}) if arguments else {}
    fields = snapshot.get('fields', [])
    concrete = bool(fields) and fields[0].get('type-snapshot', {}).get('qualified-name') == 'builtins.int'
    return call_signature_patch({
        'parameters':[
            parameter('row', type=type_annotation('object'), required=True),
            parameter('check', type=type_annotation('int' if concrete else 'str'), required=True)
        ],
        'return-type':type_annotation('None')
    })
"#,
        )?;
        let mut session = Session::new(
            &root,
            &BTreeMap::from([
                ("ty".into(), binary),
                (
                    "out".into(),
                    directory.join("output").to_string_lossy().into_owned(),
                ),
                ("plugin".into(), plugin.to_string_lossy().into_owned()),
                ("manifest".into(), manifest.to_string_lossy().into_owned()),
                ("checker-mode".into(), "push".into()),
                ("lsp-capabilities".into(), "{}".into()),
                ("python-version".into(), "3.13".into()),
            ]),
        )?;
        let result = session.check(&BTreeMap::new())?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert!(
            result["projection_rounds"].as_u64().unwrap() >= 5,
            "{result:#}"
        );
        assert!(
            result["reused_analysis_passes"].as_u64().unwrap() > 0,
            "{result:#}"
        );
        // Consumer lowering precedes producer lowering lexically. The delayed
        // return chain changes snapshot_0's field from object to int after the
        // consumer has already seen the same generated alias name in ty.
        let producer = session
            .cached_units
            .values()
            .find(|unit| unit.source.module == "z_producer")
            .unwrap();
        assert!(producer.types.prelude().contains("snapshot_0"));
        let unrelated = session
            .cached_units
            .values()
            .find(|unit| unit.source.module == "b_unrelated")
            .unwrap();
        assert!(!unrelated.reads_protocol_aliases);
        // Final rendering consumes reused passes, so the cached pass itself is
        // absent here. The dependency flag survives that consumption.
        let changed = session.check(&BTreeMap::from([(
            consumer.canonicalize()?,
            source.replace("consume(row, 1)", "consume(row, 'wrong')"),
        )]))?;
        let diagnostics = changed["diagnostics"].as_array().unwrap();
        assert_eq!(diagnostics.len(), 1, "{changed:#}");
        assert_eq!(diagnostics[0]["check_name"], "invalid-argument-type");
        assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 3);
        assert_eq!(session.check(&BTreeMap::new())?["diagnostics"], json!([]));
        drop(session);
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn external_resolution_retries_the_whole_wave_after_a_parse_failure() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let directory = std::env::temp_dir().join(format!(
            "ty-external-retry-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
        let root = directory.join("project");
        let library = directory.join("library");
        write(&library.join("a_good.pyi"), "class Good: ...\n")?;
        let broken = library.join("z_bad.pyi");
        write(&broken, "class Bad: ...\nbroken = (\n")?;
        write(
            &root.join("ty.toml"),
            &format!(
                "[environment]\nextra-paths=[{}]\npython-version='3.13'\n",
                serde_json::to_string(&library)?
            ),
        )?;
        write(
            &root.join("demo.py"),
            "from a_good import Good\nfrom z_bad import Bad\nclass Resource(Good, Bad): pass\nlabel: str = Resource().label\n",
        )?;
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
        let mut session = Session::new(
            &root,
            &BTreeMap::from([
                ("ty".into(), binary),
                (
                    "out".into(),
                    directory.join("output").to_string_lossy().into_owned(),
                ),
                (
                    "plugin".into(),
                    fixtures.join("plugin.py").to_string_lossy().into_owned(),
                ),
                (
                    "manifest".into(),
                    fixtures
                        .join("plugin.plugin.json")
                        .to_string_lossy()
                        .into_owned(),
                ),
                ("checker-mode".into(), "push".into()),
                ("lsp-capabilities".into(), "{}".into()),
            ]),
        )?;
        let error = session.check(&BTreeMap::new()).unwrap_err();
        assert!(format!("{error:#}").contains("z_bad.pyi"), "{error:#}");
        for module in ["a_good", "z_bad"] {
            assert!(!session.base_project.modules.contains_key(module));
            assert!(!session.resolved_imports.contains(module));
        }
        assert!(session.resolved_paths.is_empty());
        let requests = session.external_resolution_requests;
        let pid = session.server.as_ref().unwrap().process_id();
        write(&broken, "class Bad: ...\n")?;
        session.server.as_mut().unwrap().notify(
            "workspace/didChangeWatchedFiles",
            json!({"changes":[{"uri":crate::checker::file_uri(&broken)?,"type":2}]}),
        )?;
        // Retry the same session without invalidate(), which would hide stale
        // per-wave resolution markers and lose the recovery guarantee.
        let recovered = session.check(&BTreeMap::new())?;
        assert_eq!(recovered["diagnostics"], json!([]), "{recovered:#}");
        assert!(session.external_resolution_requests >= requests + 2);
        assert_eq!(session.server.as_ref().unwrap().process_id(), pid);
        for module in ["a_good", "z_bad"] {
            assert!(session.base_project.modules[module].external);
            assert!(session.resolved_imports.contains(module));
        }
        assert!(
            session
                .base_project
                .is_subclass("demo.Resource", "a_good.Good")
        );
        assert!(
            session
                .base_project
                .is_subclass("demo.Resource", "z_bad.Bad")
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
                .any(|d| native_rule(d) == "unresolved-attribute"),
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
                .any(|d| native_rule(d) == "unresolved-attribute"
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
