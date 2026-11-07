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

#[cfg(test)]
mod descriptor_tests {
    use super::*;
    use crate::model::typ;
    use std::path::Path;

    #[test]
    fn identical_descriptor_shapes_share_methods_without_merging_identities() -> Result<()> {
        let mut types = Types::new("_ty_ext_", "demo");
        let access = json!({"kind":"descriptor","instance-get-type":typ("str"),"instance-set-type":typ("str")});
        let first = descriptor(&mut types, "_ty_ext_first", &access)?;
        let second = descriptor(&mut types, "_ty_ext_second", &access)?;
        let source = format!("{}\n{first}\n{second}", types.prelude());
        assert!(source.contains("class _ty_ext_first"));
        assert!(source.contains("class _ty_ext_second"));
        assert_eq!(
            source.matches("def __get__").count(),
            3,
            "Repeated field shapes must not duplicate descriptor methods"
        );
        assert_eq!(source.matches("def __set__").count(), 1);
        Ok(())
    }

    #[test]
    fn outlined_descriptors_preserve_nominal_identity_and_errors() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let root =
            std::env::temp_dir().join(format!("ty-outlined-descriptors-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let mut types = Types::new("_ty_ext_", "demo");
        let access = json!({"kind":"descriptor","instance-get-type":typ("str"),"instance-set-type":typ("str")});
        for index in 0..800 {
            let code = descriptor(&mut types, &format!("_ty_ext_field_{index}"), &access)?;
            types.definitions.push_str(&code);
        }
        let placeholder = types.placeholder();
        let inline = types.prelude();
        let (outlined, modules) = types.outlined_prelude()?;
        assert_eq!(modules.len(), 1);
        for (module, source) in modules {
            std::fs::write(root.join(format!("{module}.pyi")), source)?;
        }
        for persistent in [false, true] {
            for prelude in [&inline, &outlined] {
                let source = format!(
                    "{prelude}\nfrom typing import assert_type\nclass A:\n    value: _ty_ext_field_0 = {placeholder}\nclass B:\n    value: _ty_ext_field_1 = {placeholder}\nassert_type(A().value, str)\nassert_type(A.value, _ty_ext_field_0)\nassert_type(B.value, _ty_ext_field_1)\nA().value = 1\nwrong: _ty_ext_field_0 = B.value\n"
                );
                let mut checker =
                    crate::checker::Checker::new(Path::new(&binary), &root, persistent)?;
                let diagnostics = checker.check(&root.join("demo.py"), &source)?;
                assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
                assert!(
                    diagnostics
                        .iter()
                        .all(|diagnostic| diagnostic["check_name"] == "invalid-assignment"),
                    "{diagnostics:#?}"
                );
            }
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn generic_descriptor_bodies_preserve_distinct_access_types() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let root =
            std::env::temp_dir().join(format!("ty-generic-descriptors-{}", std::process::id()));
        let mut types = Types::new("_ty_ext_", "demo");
        let mut fields = String::new();
        for (name, access) in [
            (
                "a",
                json!({"instance-get-type":typ("str"),"instance-set-type":typ("str")}),
            ),
            (
                "b",
                json!({"instance-get-type":typ("int"),"instance-set-type":typ("int")}),
            ),
            (
                "c",
                json!({"instance-get-type":typ("str"),"class-type":typ("int"),"instance-set-type":typ("bytes")}),
            ),
            (
                "d",
                json!({"instance-get-type":typ("str"),"class-type":typ("bytes")}),
            ),
            ("e", json!({"instance-get-type":typ("bytes")})),
        ] {
            fields.push_str(&descriptor(
                &mut types,
                &format!("_ty_ext_{name}"),
                &access,
            )?);
        }
        assert_eq!(types.descriptor_bases.len(), 4);
        let placeholder = types.placeholder();
        let source = format!(
            "{}\n{fields}\nfrom typing import assert_type\nclass Model:\n    a: _ty_ext_a = {placeholder}\n    b: _ty_ext_b = {placeholder}\n    c: _ty_ext_c = {placeholder}\n    d: _ty_ext_d = {placeholder}\n    e: _ty_ext_e = {placeholder}\nassert_type(Model().a, str)\nassert_type(Model().b, int)\nassert_type(Model.a, _ty_ext_a)\nassert_type(Model.b, _ty_ext_b)\nassert_type(Model().c, str)\nassert_type(Model.c, int)\nassert_type(Model().d, str)\nassert_type(Model.d, bytes)\nassert_type(Model().e, bytes)\nassert_type(Model.e, _ty_ext_e)\nModel().a = 'ok'\nModel().b = 1\nModel().c = b'ok'\nModel().a = 1\nModel().b = 'bad'\nModel().c = 'bad'\nwrong: _ty_ext_a = Model.b\n",
            types.prelude()
        );
        for persistent in [false, true] {
            let mut checker = crate::checker::Checker::new(Path::new(&binary), &root, persistent)?;
            let diagnostics = checker.check(&root.join("demo.py"), &source)?;
            assert_eq!(diagnostics.len(), 4, "{diagnostics:#?}");
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic["check_name"] == "invalid-assignment"),
                "{diagnostics:#?}"
            );
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    #[ignore = "requires an official ty executable"]
    fn shared_descriptor_methods_preserve_instance_and_class_access() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let root =
            std::env::temp_dir().join(format!("ty-descriptor-sharing-{}", std::process::id()));
        let mut types = Types::new("_ty_ext_", "demo");
        let access = json!({"kind":"descriptor","instance-get-type":typ("str"),"instance-set-type":typ("str")});
        let first = descriptor(&mut types, "_ty_ext_first", &access)?;
        let second = descriptor(&mut types, "_ty_ext_second", &access)?;
        let source = format!(
            "{}\n{first}\n{second}\nfrom typing import assert_type\nclass A:\n    value: _ty_ext_first = _ty_ext_first()\nclass B:\n    value: _ty_ext_second = _ty_ext_second()\nassert_type(A().value, str)\nassert_type(B().value, str)\nassert_type(A.value, _ty_ext_first)\nassert_type(B.value, _ty_ext_second)\nA().value = 1\nwrong: _ty_ext_first = B.value\n",
            types.prelude()
        );
        for persistent in [false, true] {
            let mut checker = crate::checker::Checker::new(Path::new(&binary), &root, persistent)?;
            let diagnostics = checker.check(&root.join("demo.py"), &source)?;
            assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
            assert!(
                diagnostics
                    .iter()
                    .all(|d| d["check_name"] == "invalid-assignment"),
                "{diagnostics:#?}"
            );
        }
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}

fn descriptor(types: &mut Types, helper: &str, access: &Value) -> Result<String> {
    let get = types.render(&access["instance-get-type"])?;
    let class = if access.get("class-type").is_some() {
        Some(types.render(&access["class-type"])?)
    } else {
        None
    };
    let set = access
        .get("instance-set-type")
        .map(|value| types.render(value))
        .transpose()?;
    let key = (class.is_some(), set.is_some());
    let base = if let Some(base) = types.descriptor_bases.get(&key) {
        base.clone()
    } else {
        let base = format!("{helper}_base");
        let typing = types.typing_alias();
        let extensions = types.typing_extensions_alias();
        let builtins = types.builtins_alias();
        let get_parameter = format!("{base}_Get");
        let class_parameter = format!("{base}_Class");
        let set_parameter = format!("{base}_Set");
        let mut parameters = vec![get_parameter.clone()];
        let mut code = format!(
            "{get_parameter} = {extensions}.TypeVar(\"{get_parameter}\", covariant=True)\n"
        );
        if class.is_some() {
            code.push_str(&format!(
                "{class_parameter} = {extensions}.TypeVar(\"{class_parameter}\", covariant=True)\n"
            ));
            parameters.push(class_parameter.clone());
        }
        if set.is_some() {
            code.push_str(&format!(
                "{set_parameter} = {extensions}.TypeVar(\"{set_parameter}\", contravariant=True)\n"
            ));
            parameters.push(set_parameter.clone());
        }
        // Self binds to the individual field subclass, preserving the nominal
        // identity exposed by descriptors without an explicit class type.
        let class_parameter = if class.is_some() {
            class_parameter
        } else {
            format!("{extensions}.Self")
        };
        let parameters = parameters.join(", ");
        code.push_str(&format!(
            "class {base}({typing}.Generic[{parameters}]):\n    @{typing}.overload\n    def __get__(self, obj: None, owner: {builtins}.type[{builtins}.object]) -> {class_parameter}: ...\n    @{typing}.overload\n    def __get__(self, obj: {builtins}.object, owner: {builtins}.type[{builtins}.object]) -> {get_parameter}: ...\n    def __get__(self, obj: {builtins}.object, owner: {builtins}.type[{builtins}.object]) -> {builtins}.object: raise {builtins}.NotImplementedError\n"
        ));
        if set.is_some() {
            code.push_str(&format!(
                "    def __set__(self, obj: {builtins}.object, value: {set_parameter}) -> None: raise {builtins}.NotImplementedError\n"
            ));
        }
        types.definitions.push_str(&code);
        types.descriptor_bases.insert(key, base.clone());
        base
    };
    let arguments = std::iter::once(get)
        .chain(class)
        .chain(set)
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("class {helper}({base}[{arguments}]):\n    pass\n"))
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
                format!("{name}: {helper} = {}\n", types.placeholder()),
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
    for c in runtime.contributions_for(qualified) {
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
            class_code.push_str(&format!("{name}: {helper} = {}\n", types.placeholder()));
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
                    let project = runtime.project.as_ref().unwrap();
                    // ty accepts Child.Meta as type[Parent.Meta], but some
                    // releases still reject that same explicit nested-class
                    // inheritance as an attribute override. Limit the exception
                    // to a single, known parent and undecorated nested classes.
                    // Unrelated replacements and ordinary field overrides keep
                    // the official checker's diagnostic.
                    if prefix != source.module
                        && let Some(owner) = project.class_summary(prefix)
                        && let Some(parents) = owner["bases"].as_array()
                        && parents.len() == 1
                        && let Some(parent) = parents[0]["expression"].as_str()
                        && let Some(nested) = project.class_summary(&name)
                        && let Some(parent_nested) =
                            project.class_summary(&format!("{parent}.{}", c.name))
                        && nested["decorators"].as_array().is_some_and(Vec::is_empty)
                        && parent_nested["decorators"]
                            .as_array()
                            .is_some_and(Vec::is_empty)
                        && nested["bases"]
                            .as_array()
                            .is_some_and(|bases| bases.len() == 1)
                        && project.is_subclass(&name, &format!("{parent}.{}", c.name))
                    {
                        edits.suppress_diagnostic(
                            span(&c.name),
                            "invalid-attribute-override".into(),
                        );
                    }
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
    let metadata_probes = collector
        .cast_metadata
        .values()
        .flatten()
        .copied()
        .collect();
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
    let mut context_free_arguments = BTreeSet::new();
    for call in calls.values() {
        for (index, argument) in call.arguments.iter().enumerate() {
            if argument.literal["kind"] == "dict"
                && argument.literal["entries"]
                    .as_array()
                    .is_some_and(|entries| !entries.is_empty())
                && let Some(inferred) =
                    oracle.and_then(|oracle| oracle.get(&call.argument_key(index)))
            {
                let (origin, arguments) = crate::runtime::generic_parts(inferred);
                if matches!(origin, "dict" | "builtins.dict")
                    && arguments.len() == 2
                    && matches!(arguments[1], "Any" | "typing.Any")
                {
                    // Retain the decision for this revision's fixed point;
                    // recovering the precise value must not toggle it back.
                    runtime
                        .context_free_arguments
                        .insert((source.path.clone(), argument.span));
                }
            }
            if runtime
                .context_free_arguments
                .contains(&(source.path.clone(), argument.span))
            {
                context_free_arguments.insert(argument.span);
            }
        }
    }
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
    let (prelude, support_modules) = types.outlined_prelude()?;
    // An unaffected module needs neither helper imports nor rewritten bytes.
    // Retain the prelude for potential flow/probe edits discovered later.
    if !edits.is_empty()
        || !prelude.is_empty()
        || !classes.is_empty()
        || !calls.is_empty()
        || !expression_probes.is_empty()
    {
        edits.insert(
            offset,
            format!(
                "from __future__ import annotations\nfrom typing import cast as {}\n{}",
                source.cast_name(),
                prelude
            ),
        );
    }
    helpers.extend(types.display_names());
    for module in support_modules.keys() {
        helpers.extend(
            helpers
                .clone()
                .into_iter()
                .map(|(name, display)| (format!("{module}.{name}"), display)),
        );
    }
    types.register_protocol_types(runtime);
    Ok(Analysis {
        prelude_offset: offset,
        classes,
        intrinsics,
        calls,
        constructor_calls,
        unbound_calls,
        annotated_casts,
        metadata_probes,
        edits,
        helpers,
        expression_probes,
        required_expression_probes: None,
        context_free_arguments,
        plugin_diagnostics: diagnostics,
        generated_declarations: generated,
        support_modules,
    })
}
