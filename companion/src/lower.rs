//! Generic project-aware lowering of plugin class, field and callable patches.
use crate::{
    mapping::Edits,
    model::{Analysis, ClassInfo, Source, Span, call_site, expression_key, span},
    runtime::{Runtime, call_request},
    type_render::Types,
};
use anyhow::{Context, Result};
use ruff_python_ast::{
    Expr, ExprContext, Stmt,
    visitor::{self, Visitor},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn descriptor(types: &mut Types, helper: &str, access: &Value) -> Result<String> {
    let get = types.render(&access["instance-get-type"])?;
    let class = if access.get("class-type").is_some() {
        types.render(&access["class-type"])?
    } else {
        helper.into()
    };
    let typing = types.typing_alias();
    let builtins = types.builtins_alias();
    let mut code = format!(
        "class {helper}:\n    @{typing}.overload\n    def __get__(self, obj: None, owner: {builtins}.type[{builtins}.object]) -> {class}: ...\n    @{typing}.overload\n    def __get__(self, obj: {builtins}.object, owner: {builtins}.type[{builtins}.object]) -> {get}: ...\n    def __get__(self, obj: {builtins}.object, owner: {builtins}.type[{builtins}.object]) -> {builtins}.object: raise {builtins}.NotImplementedError\n"
    );
    if access.get("instance-set-type").is_some() {
        let set = types.render(&access["instance-set-type"])?;
        code.push_str(&format!(
            "    def __set__(self, obj: {builtins}.object, value: {set}) -> None: raise {builtins}.NotImplementedError\n"
        ));
    }
    Ok(code)
}
fn member(
    types: &mut Types,
    prefix: &str,
    index: usize,
    patch: &Value,
    class_scope: bool,
) -> Result<(String, String)> {
    let name = patch["name"].as_str().context("member patch name")?;
    let access = &patch["access"];
    Ok(match access["kind"].as_str().unwrap_or("value") {
        "callable" => {
            let signature = &access["signature"];
            let params = types.parameter_list(
                signature["parameters"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                true,
            )?;
            let ty = types.render(&signature["return-type"])?;
            let builtins = types.builtins_alias();
            (
                String::new(),
                format!("def {name}({params}) -> {ty}: raise {builtins}.NotImplementedError\n"),
            )
        }
        "descriptor" => {
            let helper = format!("{prefix}descriptor_{index}");
            (
                descriptor(types, &helper, access)?,
                format!("{name}: {helper} = {helper}()\n"),
            )
        }
        _ => {
            let ty = types.render(&access["type-expr"])?;
            (
                String::new(),
                format!(
                    "{name}: {}\n",
                    if class_scope {
                        format!("{}.ClassVar[{ty}]", types.typing_alias())
                    } else {
                        ty
                    }
                ),
            )
        }
    })
}

#[expect(
    clippy::too_many_arguments,
    reason = "Keeps source, plugin state, type rendering, and mapped output borrows separate"
)]
fn class_patch(
    source: &Source,
    class: &ruff_python_ast::StmtClassDef,
    qualified: &str,
    runtime: &mut Runtime,
    types: &mut Types,
    edits: &mut Edits,
    diagnostics: &mut Vec<Value>,
    generated: &mut crate::mapping::GeneratedDeclarations,
) -> Result<ClassInfo> {
    let project = runtime.project.as_ref().unwrap();
    let summary = project
        .class_summary(qualified)
        .cloned()
        .unwrap_or(json!({"qualified-name":qualified}));
    let mut response = runtime
        .project_patches
        .get(qualified)
        .cloned()
        .unwrap_or(json!({"kind":"no-change"}));
    if response["kind"] == "no-change" {
        response = runtime.execute(source, "analyze-class", json!({"class":summary}))?;
        runtime.set_project_patch(qualified.into(), response.clone());
    }
    let mut fields: BTreeMap<String, Value> = response["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|v| (v["name"].as_str().unwrap_or("").to_owned(), v.clone()))
        .collect();
    let class_transform_fields = response["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|field| field["name"].as_str())
        .collect::<BTreeSet<_>>();
    let mut members: BTreeMap<String, (Value, bool)> = BTreeMap::new();
    for (key, class_scope) in [("class-members", true), ("instance-members", false)] {
        for m in response[key].as_array().into_iter().flatten() {
            members.insert(
                m["name"].as_str().unwrap_or("").to_owned(),
                (m.clone(), class_scope),
            );
        }
    }
    let mut constructor = response.get("constructor").cloned();
    // Contributions are already ordered by the project hook. Conflicting patch
    // keys are checked when the project index is installed.
    for c in &runtime.project_contributions {
        if c["target"]["qualified-name"] != qualified {
            continue;
        }
        let patch = &c["patch"];
        match patch["kind"].as_str() {
            Some("field") => {
                fields
                    .entry(patch["name"].as_str().unwrap_or("").into())
                    .or_insert_with(|| patch.clone());
            }
            Some("member") => {
                members
                    .entry(patch["name"].as_str().unwrap_or("").into())
                    .or_insert_with(|| (patch.clone(), c["target"]["kind"] == "class"));
            }
            Some("constructor") => {
                constructor.get_or_insert_with(|| patch.clone());
            }
            _ => {}
        }
    }
    let (insertion, indent) = crate::static_project::class_insertion(source, class, edits)?;
    let previous_self = types.set_self_context(Some(crate::model::typ(qualified)));
    let mut declared = BTreeMap::new();
    let mut methods = BTreeSet::new();
    for stmt in &class.body {
        match stmt {
            Stmt::Assign(a) => {
                for target in &a.targets {
                    if let Expr::Name(n) = target {
                        declared.insert(n.id.to_string(), span(n));
                    }
                }
            }
            Stmt::AnnAssign(a) => {
                if let Expr::Name(n) = a.target.as_ref() {
                    declared.insert(n.id.to_string(), span(n));
                }
            }
            Stmt::FunctionDef(f) => {
                methods.insert(f.name.to_string());
            }
            _ => {}
        }
    }
    let mut class_code = String::new();
    let mut helper_code = String::new();
    let mut field_types = BTreeMap::new();
    let mut params = Vec::new();
    let mut patched_names = BTreeSet::new();
    for (index, (name, field)) in fields.iter().enumerate() {
        let class_type =
            field
                .get("descriptor")
                .and_then(|descriptor| match descriptor["kind"].as_str() {
                    Some("value") => descriptor.get("type-expr"),
                    Some("callable") => descriptor.get("fallback-type"),
                    Some("descriptor") => descriptor.get("class-type"),
                    _ => None,
                });
        let ty = types.render(&field["instance-get-type"])?;
        field_types.insert(name.clone(), ty.clone());
        // A native class-transform descriptor supplies a class *value*. Its
        // ordinary instance lookup takes precedence over the instance fallback;
        // it is not a Python descriptor with an inherited __set__ method.
        let class_transform = class_transform_fields.contains(name.as_str());
        if class_transform && let Some(class_type) = class_type {
            let class_type = types.render(class_type)?;
            class_code.push_str(&format!(
                "{name}: {class_type} = {}({class_type}, None)\n",
                source.cast_name()
            ));
            patched_names.insert(name.clone());
        } else if field["mode"] == "replace-existing" || !declared.contains_key(name) {
            // Returning the member through __get__ prevents a plugin value
            // which itself implements __get__ (for example a manager) from
            // accidentally binding as a class-body descriptor. The ordinary
            // write domain is the read type; exact class-transform setters are
            // projected separately at their receiver's assignment sites.
            let helper = format!("{}field_{}_{}", source.prefix, span(class).start, index);
            let access = json!({"kind":"descriptor","instance-get-type":field["instance-get-type"],"instance-set-type":field["instance-get-type"]});
            helper_code.push_str(&descriptor(types, &helper, &access)?);
            class_code.push_str(&format!("{name}: {helper} = {helper}()\n"));
            generated
                .descriptors
                .entry(qualified.to_owned())
                .or_default()
                .insert(name.clone());
            patched_names.insert(name.clone());
        }
        if let Some(param) = field.get("constructor-parameter") {
            let mut param = param.clone();
            if param.get("type-expr").is_none_or(Value::is_null) {
                param["type-expr"] = field
                    .get("instance-set-type")
                    .unwrap_or(&field["instance-get-type"])
                    .clone();
            }
            if field["has-default"] == true {
                param["required"] = json!(false);
            }
            params.push(param);
        }
    }
    for (index, (name, (patch, class_scope))) in members.iter().enumerate() {
        if !*class_scope
            && patch["mode"] != "replace-existing"
            && (declared.contains_key(name) || methods.contains(name))
        {
            continue;
        }
        let (helper, code) = member(
            types,
            &format!("{}member_{}_", source.prefix, span(class).start),
            index,
            patch,
            *class_scope,
        )?;
        helper_code.push_str(&helper);
        class_code.push_str(&code);
        if !*class_scope && patch["access"]["kind"] == "descriptor" {
            generated
                .descriptors
                .entry(qualified.to_owned())
                .or_default()
                .insert(name.clone());
        }
        patched_names.insert(name.clone());
    }
    if !methods.contains("__init__") && (constructor.is_some() || !params.is_empty()) {
        let params = constructor
            .as_ref()
            .and_then(|c| c["parameters"].as_array())
            .unwrap_or(&params);
        let signature = types.parameter_list(params, true)?;
        let builtins = types.builtins_alias();
        class_code.push_str(&format!(
            "def __init__({signature}) -> None: raise {builtins}.NotImplementedError\n"
        ));
        generated.constructors.insert(qualified.to_owned());
    }
    // Preserve original RHS evaluation and argument checking under a hygienic
    // binding; the declared plugin descriptor supplies accesses to this name.
    for name in patched_names {
        if let Some(target) = declared.get(&name) {
            edits.replace(
                *target,
                format!("{}original_{}_{}", source.prefix, span(class).start, name),
            );
        }
    }
    if !class_code.is_empty() {
        let indented = class_code
            .lines()
            .collect::<Vec<_>>()
            .join(&format!("\n{indent}"));
        edits.insert(insertion, format!("{indented}\n{indent}"));
    }
    types.definitions.push_str(&helper_code);
    for diagnostic in response["diagnostics"].as_array().into_iter().flatten() {
        diagnostics.push(json!({"diagnostic":diagnostic,"source":source.location(span(class))}));
    }
    types.set_self_context(previous_self);
    let declared = field_types.keys().cloned().collect();
    Ok(ClassInfo {
        origin: span(&class.name),
        fields: field_types,
        methods,
        insertion,
        indent,
        declared,
    })
}

#[derive(Default)]
struct Bindings {
    names: BTreeSet<String>,
}
impl<'a> Visitor<'a> for Bindings {
    fn visit_pattern(&mut self, pattern: &'a ruff_python_ast::Pattern) {
        self.names.extend(crate::model::pattern_bindings(pattern));
    }
    fn visit_stmt(&mut self, node: &'a Stmt) {
        match node {
            Stmt::FunctionDef(f) => {
                self.names.insert(f.name.to_string());
            }
            Stmt::ClassDef(c) => {
                self.names.insert(c.name.to_string());
            }
            Stmt::Import(i) => {
                for alias in &i.names {
                    self.names.insert(
                        alias
                            .asname
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| {
                                alias.name.as_str().split('.').next().unwrap().to_string()
                            }),
                    );
                }
            }
            Stmt::ImportFrom(i) => {
                for alias in &i.names {
                    self.names
                        .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
                }
            }
            _ => visitor::walk_stmt(self, node),
        }
    }
    fn visit_expr(&mut self, node: &'a Expr) {
        if crate::model::comprehension(node).is_some() {
            self.names
                .extend(crate::model::comprehension_outer_bindings(node));
            return;
        }
        match node {
            Expr::Name(name) if name.ctx == ExprContext::Store => {
                self.names.insert(name.id.to_string());
            }
            Expr::Lambda(_) => {}
            _ => visitor::walk_expr(self, node),
        }
    }
}

struct Calls<'a> {
    source: &'a Source,
    runtime: &'a Runtime,
    calls: BTreeMap<Span, crate::model::CallSite>,
    constructor_calls: BTreeMap<Span, String>,
    probes: BTreeMap<Span, String>,
    intrinsics: BTreeSet<String>,
    shadowed: BTreeSet<String>,
    available_imports: BTreeSet<String>,
    deferred: bool,
    unbound: BTreeSet<Span>,
    annotated_casts: BTreeMap<Span, Value>,
    cast_metadata: BTreeMap<Span, Vec<Span>>,
}

/// The plugin-free path still implements the fork's transparent Annotated
/// casts. Reuse lexical call resolution without dispatching any hook.
pub fn annotated_cast_spans(source: &Source, suite: &[Stmt], runtime: &Runtime) -> BTreeSet<Span> {
    let mut collector = Calls {
        source,
        runtime,
        calls: BTreeMap::new(),
        probes: BTreeMap::new(),
        intrinsics: BTreeSet::new(),
        shadowed: BTreeSet::new(),
        available_imports: BTreeSet::new(),
        deferred: false,
        unbound: BTreeSet::new(),
        annotated_casts: BTreeMap::new(),
        cast_metadata: BTreeMap::new(),
        constructor_calls: BTreeMap::new(),
    };
    collector.visit_body(suite);
    collector.annotated_casts.into_keys().collect()
}
impl<'a> Visitor<'a> for Calls<'_> {
    fn visit_annotation(&mut self, _node: &'a Expr) {}
    fn visit_expr(&mut self, node: &'a Expr) {
        if let Some((generators, elements)) = crate::model::comprehension(node) {
            let Some((first, rest)) = generators.split_first() else {
                return;
            };
            self.visit_expr(&first.iter);
            let previous = self.shadowed.clone();
            let mut bindings = Bindings::default();
            bindings.visit_expr(&first.target);
            self.shadowed.extend(bindings.names);
            for condition in &first.ifs {
                self.visit_expr(condition);
            }
            for generator in rest {
                self.visit_expr(&generator.iter);
                let mut bindings = Bindings::default();
                bindings.visit_expr(&generator.target);
                self.shadowed.extend(bindings.names);
                for condition in &generator.ifs {
                    self.visit_expr(condition);
                }
            }
            for element in elements {
                self.visit_expr(element);
            }
            self.shadowed = previous;
            return;
        }
        if let Expr::Lambda(lambda) = node {
            let previous = self.shadowed.clone();
            let deferred = self.deferred;
            self.deferred = true;
            if let Some(parameters) = &lambda.parameters {
                self.shadowed.extend(
                    parameters
                        .iter()
                        .map(|parameter| parameter.name().to_string()),
                );
            }
            self.visit_expr(&lambda.body);
            self.shadowed = previous;
            self.deferred = deferred;
            return;
        }
        match node {
            Expr::Attribute(a) => {
                self.probes.insert(
                    span(a.value.as_ref()),
                    expression_key(span(a.value.as_ref())),
                );
            }
            Expr::Subscript(s) => {
                self.probes.insert(
                    span(s.value.as_ref()),
                    expression_key(span(s.value.as_ref())),
                );
            }
            _ => {}
        }
        if let Expr::Call(call) = node {
            let raw = self.source.slice(span(call.func.as_ref()));
            let canonical = self.runtime.canonical_name(self.source, raw);
            let last = raw.rsplit('.').next().unwrap_or(raw);
            let root = raw.split('.').next().unwrap_or(raw);
            if !self.deferred
                && self.runtime.project.as_ref().is_some_and(|project| {
                    project
                        .modules
                        .get(&self.source.module)
                        .is_some_and(|module| module.imports.contains_key(root))
                })
                && !self.available_imports.contains(root)
            {
                self.unbound.insert(span(call));
                visitor::walk_expr(self, node);
                return;
            }
            let shadowed = self.shadowed.contains(root);
            if !shadowed
                && self
                    .runtime
                    .project
                    .as_ref()
                    .is_some_and(|project| project.classes.contains_key(&canonical))
            {
                self.constructor_calls.insert(span(call), canonical.clone());
            }
            if !shadowed
                && matches!(
                    canonical.as_str(),
                    "typing.assert_type"
                        | "typing_extensions.assert_type"
                        | "typing.reveal_type"
                        | "typing_extensions.reveal_type"
                )
            {
                if let Some(value) = call.arguments.args.first() {
                    self.visit_expr(value);
                }
                return;
            }
            if !shadowed && matches!(canonical.as_str(), "typing.cast" | "typing_extensions.cast") {
                if let Some(target) = call.arguments.args.first() {
                    let ty = self
                        .runtime
                        .type_expr(self.source, self.source.slice(span(target)));
                    if ty["snapshot"]["kind"] == "annotated" {
                        self.annotated_casts.insert(span(call), ty);
                        if let Expr::Subscript(target) = target
                            && let Expr::Tuple(arguments) = target.slice.as_ref()
                        {
                            let metadata: Vec<_> =
                                arguments.elts.iter().skip(1).map(span).collect();
                            for span in &metadata {
                                self.probes.insert(*span, expression_key(*span));
                            }
                            self.cast_metadata.insert(span(call), metadata);
                        }
                    }
                }
                if let Some(value) = call.arguments.args.get(1) {
                    self.visit_expr(value);
                }
                return;
            }
            let claims = &self.runtime.manifest["claims"];
            let method = matches!(call.func.as_ref(), Expr::Attribute(_))
                && (claims["methods"].as_array().into_iter().flatten().any(|c| {
                    c["method-name"] == last
                        || c["method-name-pattern"].as_str().is_some_and(|pattern| {
                            ty_plugin_protocol::method_name_pattern_matches(pattern, last)
                        })
                }) || claims["functions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|c| {
                        // Exact function claims can name a bound method too. Its
                        // receiver may need the oracle (e.g. factory().method()),
                        // so defer the full identity check to runtime dispatch.
                        c["qualified-name"].as_str().is_some_and(|name| {
                            name.rsplit_once('.')
                                .is_some_and(|(_, method)| method == last)
                        })
                    }));
            let exact = !shadowed
                && claims["functions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|c| {
                        c["qualified-name"] == self.runtime.protocol_name(self.source, &canonical)
                    });
            let constructor = !shadowed
                && self.runtime.project.as_ref().is_some_and(|p| {
                    claims["constructors"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|c| {
                            c["qualified-name"]
                                == self.runtime.protocol_name(self.source, &canonical)
                                || c["base-qualified-name"].as_str().is_some_and(|base| {
                                    let base = if self.source.protocol_module.is_some() {
                                        self.runtime.canonical_name(self.source, base)
                                    } else {
                                        base.to_owned()
                                    };
                                    p.is_subclass(&canonical, &base)
                                })
                        })
                });
            if method || exact || constructor {
                let mut site = call_site(call, canonical);
                if let Expr::Attribute(a) = call.func.as_ref() {
                    site.receiver_span = Some(span(a.value.as_ref()));
                    if !shadowed {
                        if let Some(receiver) = self.runtime.expression_type(self.source, &a.value)
                        {
                            site.receiver_type = receiver["expression"].as_str().map(str::to_owned);
                        }
                        let receiver = self
                            .runtime
                            .canonical_name(self.source, self.source.slice(span(a.value.as_ref())));
                        if let Some(project) = &self.runtime.project
                            && let Some(owner) = project.method_owner(&receiver, a.attr.as_str())
                        {
                            let class_method = project
                                .class_summary(&owner)
                                .and_then(|class| class["methods"].as_array())
                                .and_then(|methods| {
                                    methods
                                        .iter()
                                        .find(|method| method["name"] == a.attr.as_str())
                                })
                                .is_some_and(|method| {
                                    method["decorators"].as_array().into_iter().flatten().any(
                                        |decorator| {
                                            matches!(
                                                decorator["qualified-name"].as_str(),
                                                Some("classmethod" | "builtins.classmethod")
                                            )
                                        },
                                    )
                                });
                            if class_method {
                                site.receiver_type = Some(format!("type[{receiver}]"));
                            }
                        }
                    }
                }
                self.calls.insert(span(call), site);
            }
        }
        visitor::walk_expr(self, node);
    }
    fn visit_stmt(&mut self, node: &'a Stmt) {
        match node {
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.visit_expr(&decorator.expression);
                }
                for parameter in function
                    .parameters
                    .posonlyargs
                    .iter()
                    .chain(&function.parameters.args)
                    .chain(&function.parameters.kwonlyargs)
                {
                    if let Some(default) = &parameter.default {
                        self.visit_expr(default);
                    }
                }
                let previous = self.shadowed.clone();
                let deferred = self.deferred;
                let imports = self.available_imports.clone();
                self.deferred = true;
                let mut bindings = Bindings::default();
                bindings.visit_body(&function.body);
                self.shadowed.extend(bindings.names);
                self.shadowed.extend(
                    function
                        .parameters
                        .iter()
                        .map(|parameter| parameter.name().to_string()),
                );
                self.visit_body(&function.body);
                self.shadowed = previous;
                self.deferred = deferred;
                self.available_imports = imports;
            }
            Stmt::ClassDef(class) => {
                let imports = self.available_imports.clone();
                self.visit_body(&class.body);
                self.available_imports = imports;
            }
            Stmt::Assign(assignment) => {
                visitor::walk_stmt(self, node);
                if matches!(assignment.value.as_ref(), Expr::Call(_))
                    && self.runtime.manifest["capabilities"]["call-state"] == true
                    && !self.calls.contains_key(&span(assignment.value.as_ref()))
                {
                    let range = span(assignment.value.as_ref());
                    self.probes.insert(range, expression_key(range));
                }
            }
            Stmt::AnnAssign(assignment) => {
                self.visit_expr(&assignment.target);
                if let Some(value) = &assignment.value {
                    self.visit_expr(value);
                    if matches!(value.as_ref(), Expr::Call(_))
                        && self.runtime.manifest["capabilities"]["call-state"] == true
                        && !self.calls.contains_key(&span(value.as_ref()))
                    {
                        let range = span(value.as_ref());
                        self.probes.insert(range, expression_key(range));
                    }
                }
            }
            Stmt::TypeAlias(_) => {}
            Stmt::If(branch) => {
                self.visit_expr(&branch.test);
                let imports = self
                    .runtime
                    .project
                    .as_ref()
                    .and_then(|project| project.modules.get(&self.source.module))
                    .map(|module| module.imports.clone())
                    .unwrap_or_default();
                for body in crate::project::branch_suites(branch, &imports) {
                    self.visit_body(body);
                }
            }
            Stmt::ImportFrom(import) => {
                self.available_imports.extend(
                    import
                        .names
                        .iter()
                        .map(|alias| alias.asname.as_ref().unwrap_or(&alias.name).to_string()),
                );
                if import
                    .module
                    .as_ref()
                    .is_some_and(|m| matches!(m.as_str(), "typing" | "typing_extensions"))
                {
                    for alias in &import.names {
                        if matches!(alias.name.as_str(), "assert_type" | "reveal_type") {
                            self.intrinsics
                                .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
                        }
                    }
                }
            }
            Stmt::Import(import) => {
                self.available_imports
                    .extend(import.names.iter().map(|alias| {
                        alias
                            .asname
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| {
                                alias.name.as_str().split('.').next().unwrap().to_owned()
                            })
                    }));
            }
            _ => visitor::walk_stmt(self, node),
        }
    }
}

pub fn analyze(source: &Source, suite: &[Stmt], runtime: &mut Runtime) -> Result<Analysis> {
    analyze_with_oracle(source, suite, runtime, None)
}

pub fn analyze_with_oracle(
    source: &Source,
    suite: &[Stmt],
    runtime: &mut Runtime,
    oracle: Option<&crate::model::Oracle>,
) -> Result<Analysis> {
    let offset = crate::static_project::prelude_offset(source, suite);
    let mut types = Types::with_project(
        &format!("{}static_", source.prefix),
        &source.module,
        runtime.project.as_ref().unwrap(),
    );
    let mut edits = Edits::default();
    let mut classes = BTreeMap::new();
    let mut diagnostics = Vec::new();
    let mut generated = crate::mapping::GeneratedDeclarations::default();
    #[expect(
        clippy::too_many_arguments,
        reason = "Recursive traversal shares independent lowering outputs without owning them"
    )]
    fn classes_in(
        source: &Source,
        suite: &[Stmt],
        prefix: &str,
        runtime: &mut Runtime,
        types: &mut Types,
        edits: &mut Edits,
        classes: &mut BTreeMap<String, ClassInfo>,
        diagnostics: &mut Vec<Value>,
        generated: &mut crate::mapping::GeneratedDeclarations,
    ) -> Result<()> {
        for stmt in suite {
            match stmt {
                Stmt::ClassDef(c) => {
                    let name = format!("{prefix}.{}", c.name);
                    let info = class_patch(
                        source,
                        c,
                        &name,
                        runtime,
                        types,
                        edits,
                        diagnostics,
                        generated,
                    )?;
                    classes.insert(name.clone(), info);
                    classes_in(
                        source,
                        &c.body,
                        &name,
                        runtime,
                        types,
                        edits,
                        classes,
                        diagnostics,
                        generated,
                    )?;
                }
                Stmt::If(i) => {
                    let imports = runtime
                        .project
                        .as_ref()
                        .and_then(|project| project.modules.get(&source.module))
                        .map(|module| module.imports.clone())
                        .unwrap_or_default();
                    for body in crate::project::branch_suites(i, &imports) {
                        classes_in(
                            source,
                            body,
                            prefix,
                            runtime,
                            types,
                            edits,
                            classes,
                            diagnostics,
                            generated,
                        )?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
    classes_in(
        source,
        suite,
        &source.module,
        runtime,
        &mut types,
        &mut edits,
        &mut classes,
        &mut diagnostics,
        &mut generated,
    )?;
    let mut collector = Calls {
        source,
        runtime,
        calls: BTreeMap::new(),
        constructor_calls: BTreeMap::new(),
        probes: BTreeMap::new(),
        intrinsics: BTreeSet::new(),
        shadowed: BTreeSet::new(),
        available_imports: BTreeSet::new(),
        deferred: false,
        unbound: BTreeSet::new(),
        annotated_casts: BTreeMap::new(),
        cast_metadata: BTreeMap::new(),
    };
    collector.visit_body(suite);
    let calls = collector.calls;
    let constructor_calls = collector.constructor_calls;
    let unbound_calls = collector.unbound;
    let mut annotated_casts = collector.annotated_casts;
    for (span, metadata) in collector.cast_metadata {
        let Some(value) = annotated_casts.get_mut(&span) else {
            continue;
        };
        for (index, source_span) in metadata.into_iter().enumerate() {
            let Some(display) = oracle.and_then(|oracle| oracle.get(&expression_key(source_span)))
            else {
                continue;
            };
            let inferred = runtime.type_expr(source, display);
            let snapshot = &inferred["snapshot"];
            let metadata = if snapshot["kind"] == "nominal" {
                json!({"qualified-name":snapshot["qualified-name"],"arguments":snapshot.get("arguments").cloned().unwrap_or_else(||json!([]))})
            } else if display.starts_with("<class '") && source.slice(source_span).contains('[') {
                let qualified = runtime.canonical_name(source, source.slice(source_span));
                let (name, arguments) = crate::runtime::generic_parts(&qualified);
                json!({"qualified-name":name,"arguments":arguments.iter().map(|argument| runtime.qualified_type_expr(argument)["snapshot"].clone()).collect::<Vec<_>>()})
            } else {
                json!({"qualified-name":display,"arguments":[]})
            };
            value["snapshot"]["metadata"][index] = metadata;
        }
    }
    for span in annotated_casts.keys() {
        edits.suppress_diagnostic(*span, "redundant-cast".into());
    }
    let expression_probes = collector.probes;
    let intrinsics = collector.intrinsics;
    let mut helpers = BTreeMap::new();
    for call in calls.values() {
        let response = call_request(runtime, source, "adjust-call-signature", call, oracle, None)?;
        if response["kind"] == "call-signature-patch" {
            let receiver = call
                .flow_receiver_type
                .as_ref()
                .or(call.receiver_type.as_ref())
                .or_else(|| oracle.and_then(|oracle| oracle.get(&call.receiver_key())));
            let previous_self = types
                .set_self_context(receiver.map(|receiver| runtime.type_expr(source, receiver)));
            let signature = &response["signature"];
            let parameters = types.parameter_list(
                signature["parameters"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                false,
            )?;
            let result = types.render(&signature["return-type"])?;
            types.set_self_context(previous_self);
            let helper = format!(
                "{}call_{}_{}",
                source.prefix, call.span.start, call.span.end
            );
            let builtins = types.builtins_alias();
            types.definitions.push_str(&format!(
                "def {helper}({parameters}) -> {result}: raise {builtins}.NotImplementedError\n"
            ));
            helpers.insert(helper.clone(), call.callee.clone());
            // Keep the original callable expression available for checking and
            // nested probes while selecting the plugin's callable signature.
            edits.wrap_callable(call.function, "(".into(), format!(", {helper})[1]"), helper);
        }
        // The native signature hook consumes the callable shape only. Runtime
        // failures are reported by the host; patch diagnostics are not emitted.
        for diagnostic in response["diagnostics"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|diagnostic| diagnostic.get("runtime-error").is_some())
        {
            diagnostics.push(json!({"diagnostic":diagnostic,"source":source.location(call.span)}));
        }
    }
    edits.insert(
        offset,
        format!(
            "from __future__ import annotations\nfrom typing import cast as {}\n{}",
            source.cast_name(),
            types.prelude()
        ),
    );
    helpers.extend(types.display_names());
    types.register_protocol_types(runtime);
    Ok(Analysis {
        prelude_offset: offset,
        classes,
        intrinsics,
        calls,
        constructor_calls,
        unbound_calls,
        annotated_casts,
        edits,
        helpers,
        expression_probes,
        plugin_diagnostics: diagnostics,
        generated_declarations: generated,
    })
}
