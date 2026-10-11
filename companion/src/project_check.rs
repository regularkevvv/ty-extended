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
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Instant,
};

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
        } else if path.extension().is_some_and(|x| x == "py" || x == "pyi") {
            out.push(path);
        }
    }
    Ok(())
}
fn write(path: &Path, text: &str) -> Result<()> {
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
        write(&dst, &std::fs::read_to_string(path)?)?;
    }
    Ok(())
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
fn imports(text: &str) -> Result<BTreeSet<String>> {
    let parsed = ruff_python_parser::parse_module(text)?;
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
    suite: Vec<ruff_python_ast::Stmt>,
    analysis: Analysis,
    path: PathBuf,
    oracle: Oracle,
    patches: ReturnPatches,
    return_types: BTreeMap<crate::model::Span, Value>,
    types: Types,
    diagnostics: Vec<Value>,
    rendered: Option<Rendered>,
    selected: bool,
}

fn check_units(
    binary: &Path,
    root: &Path,
    config: &Path,
    units: &BTreeMap<PathBuf, File>,
    paths: &[PathBuf],
    persistent: bool,
    server: &mut Option<crate::checker::ProjectChecker>,
) -> Result<Vec<Value>> {
    if !persistent {
        return check(binary, root, config, paths);
    }
    if server.is_none() {
        // Both generated source and overlaid stubs participate in inference.
        // ty publishes no diagnostics for files outside its workspace, even
        // when they are open and importable through extra-paths.
        let workspace = config.parent().context("generated checker workspace")?;
        *server = Some(crate::checker::ProjectChecker::new(
            binary, workspace, config,
        )?);
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
pub fn run(input: &Path, options: &BTreeMap<String, String>) -> Result<Value> {
    let started = Instant::now();
    let required = |key: &str| options.get(key).with_context(|| format!("missing --{key}"));
    let mode = options
        .get("checker-mode")
        .map(String::as_str)
        .unwrap_or("cli");
    ensure!(
        matches!(mode, "cli" | "push"),
        "project --checker-mode must be cli or push"
    );
    let mut server = None;
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
    let original_stubs = PathBuf::from(required("stubs")?);
    copy_python(&original_stubs, &stubs)?;
    let config: Value = if let Some(path) = options.get("config") {
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        json!({})
    };
    let python_version = options
        .get("python-version")
        .map(String::as_str)
        .unwrap_or("3.13");
    let mut runtime = Runtime::new(
        Path::new(required("manifest")?),
        Path::new(required("plugin")?),
    )?;
    runtime.python_version = python_version.into();
    let mut project = Project::load(&root, &[], &runtime.manifest, config.clone())?;
    project.add_source_tree(&original_stubs)?;
    runtime.attach_project(Arc::new(project.clone()), config.clone());
    let mut request = project.build_request();
    request["context"]["python-version"] = json!(python_version);
    write(
        &out.join("project-request.json"),
        &serde_json::to_string_pretty(&request)?,
    )?;
    let index = runtime.execute_project("build-project-index", request)?;
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
    for (path, code) in project.register_virtual_types(&definitions)? {
        write(&stubs.join(path), &code)?;
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
            .map(|m| PathBuf::from(&m.source.path))
            .collect()
    };
    let selected: BTreeSet<_> = selected
        .into_iter()
        .map(|p| p.canonicalize())
        .collect::<std::io::Result<_>>()?;
    let external_targets: BTreeSet<_> = runtime
        .project_contributions
        .iter()
        .filter_map(|c| c.pointer("/target/qualified-name").and_then(Value::as_str))
        .filter_map(|name| name.rsplit_once('.').map(|v| v.0.to_owned()))
        .collect();
    let mut units = BTreeMap::new();
    for (name, module) in &project.modules {
        if module.external && !external_targets.contains(name) {
            continue;
        }
        let path = if module.external {
            stubs.join(Path::new(&module.source.path).strip_prefix(&original_stubs)?)
        } else {
            view.join(Path::new(&module.source.path).strip_prefix(&root)?)
        };
        let analysis = crate::static_project::analyze(&module.source, &module.suite, &mut runtime)
            .with_context(|| format!("lower {name}"))?;
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
            },
        );
    }
    let mut checker_config = format!(
        "[environment]\npython-version = {}\nextra-paths = [{}, {}]\n",
        serde_json::to_string(python_version)?,
        serde_json::to_string(&stubs)?,
        serde_json::to_string(&view)?
    );
    if let Some(python) = options.get("python") {
        checker_config.push_str(&format!("python = {}\n", serde_json::to_string(python)?));
    }
    let checker_config_path = out.join("checker.toml");
    write(&checker_config_path, &checker_config)?;
    let binary = PathBuf::from(required("ty")?);
    let mut converged = false;
    let mut rounds = 0;
    let mut oracle_queries = 0;
    let mut checked_imports = BTreeSet::new();
    for iteration in 0..12 {
        rounds = iteration + 1;
        for unit in units.values_mut() {
            let flow = crate::flow::project_with_return_types(
                &unit.source,
                &unit.suite,
                &mut unit.analysis,
                &mut runtime,
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
            write(&unit.path, &generated.text)?;
            unit.rendered = Some(generated);
        }
        let mut candidates = BTreeSet::new();
        for unit in units.values() {
            candidates.extend(imports(&unit.rendered.as_ref().unwrap().text)?);
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
            let unavailable = check(
                &binary,
                &view,
                &checker_config_path,
                std::slice::from_ref(&preflight),
            )?;
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
                server = None;
                for (path, code) in project.register_virtual_types(&definitions)? {
                    write(&stubs.join(path), &code)?;
                }
                runtime.attach_project(Arc::new(project.clone()), runtime.config.clone());
                for unit in units.values_mut() {
                    unit.analysis =
                        crate::static_project::analyze(&unit.source, &unit.suite, &mut runtime)?;
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
        let query_paths: Vec<_> = units
            .values()
            .filter(|u| !u.analysis.calls.is_empty() || !u.analysis.expression_probes.is_empty())
            .map(|u| u.path.clone())
            .collect();
        let raw = check_units(
            &binary,
            &view,
            &checker_config_path,
            &units,
            &query_paths,
            mode == "push",
            &mut server,
        )?;
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
        for unit in units.values_mut() {
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
                    let description = d["description"].as_str().context("reveal description")?;
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
        let mut stable = iteration > 0;
        for unit in units.values_mut() {
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
                    &mut runtime,
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
        if stable {
            converged = true;
            break;
        }
    }
    ensure!(
        converged,
        "project did not converge within 12 inference rounds"
    );
    let mut flow_diagnostics = BTreeMap::new();
    for unit in units.values_mut() {
        let flow = crate::flow::project_with_return_types(
            &unit.source,
            &unit.suite,
            &mut unit.analysis,
            &mut runtime,
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
        let generated = render(&unit.source, &edits)?;
        write(&unit.path, &generated.text)?;
        unit.rendered = Some(generated);
        flow_diagnostics.insert(unit.path.clone(), flow.diagnostics);
    }
    let paths: Vec<_> = units
        .values()
        .filter(|u| u.selected)
        .map(|u| u.path.clone())
        .collect();
    let raw = check_units(
        &binary,
        &view,
        &checker_config_path,
        &units,
        &paths,
        mode == "push",
        &mut server,
    )?;
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
        plugin.extend(flow_diagnostics.remove(&unit.path).unwrap_or_default());
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
    let result = json!({"diagnostics":diagnostics,"suppressed_generated":suppressed,"implementation":"rust","plugin_runtime":"embedded-monty","project":root,"checker":binary,"checker_mode":mode,"projection_rounds":rounds,"oracle_rounds":oracle_queries,"monty_executions":runtime.executions,"monty_cache_hits":runtime.hits,"monty_seconds":runtime.execution_time.as_secs_f64(),"seconds":started.elapsed().as_secs_f64()});
    write(
        &out.join("results.json"),
        &serde_json::to_string_pretty(&result)?,
    )?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

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
