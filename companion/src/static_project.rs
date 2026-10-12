//! Bounded, native static projection for the external-checker experiment.
//! All offsets refer to original UTF-8 bytes; generated edits retain that origin.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use ruff_python_ast::{
    self as ast, Expr, ExprContext, Stmt,
    visitor::{self, Visitor},
};
use serde_json::{Value, json};

use crate::{
    mapping::Edits,
    model::{Analysis, ClassInfo, Source, Span, call_site, span, typ},
    runtime::{Runtime, call_request},
};

fn intrinsic(name: &str) -> bool {
    matches!(name, "assert_type" | "reveal_type")
}

/// Insert only after the module docstring and all immediately following future
/// imports. Leading comments, encoding cookies and shebangs remain untouched.
pub fn prelude_offset(source: &Source, suite: &[Stmt]) -> usize {
    let mut consumed = 0;
    if matches!(suite.first(), Some(Stmt::Expr(node)) if matches!(node.value.as_ref(), Expr::StringLiteral(_)))
    {
        consumed = 1;
    }
    while matches!(suite.get(consumed), Some(Stmt::ImportFrom(node)) if node.module.as_ref().is_some_and(|name| name.as_str() == "__future__"))
    {
        consumed += 1;
    }
    if consumed == 0 {
        return suite
            .first()
            .map_or(source.text.len(), |node| span(node).start);
    }
    let end = span(&suite[consumed - 1]).end;
    // AST statement ranges omit trailing comments and physical newlines.
    let suffix = &source.text.as_bytes()[end..];
    suffix
        .iter()
        .position(|byte| matches!(byte, b'\r' | b'\n'))
        .map_or(source.text.len(), |n| {
            end + n
                + if suffix[n] == b'\r' && suffix.get(n + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                }
        })
}

/// Keep the real leading docstring first, and split simple suites before
/// inserting generated declarations. All original statement bytes stay mapped.
pub(crate) fn class_insertion(
    source: &Source,
    class: &ruff_python_ast::StmtClassDef,
    edits: &mut Edits,
) -> Result<(usize, String)> {
    let first = class.body.first().context("empty class body")?;
    let first_start = span(first).start;
    let first_line = source.text[..first_start]
        .rfind(['\r', '\n'])
        .map_or(0, |offset| offset + 1);
    let leading = &source.text[first_line..first_start];
    let inline = !leading.chars().all(|c| c == ' ' || c == '\t');
    let indent = if inline {
        let class_start = span(class).start;
        let line_start = source.text[..class_start]
            .rfind(['\r', '\n'])
            .map_or(0, |offset| offset + 1);
        format!("{}    ", &source.text[line_start..class_start])
    } else {
        leading.to_owned()
    };
    if inline {
        edits.insert(first_start, format!("\n{indent}"));
    }
    let docstring =
        matches!(first, Stmt::Expr(node) if matches!(node.value.as_ref(), Expr::StringLiteral(_)));
    let insertion = if docstring {
        class
            .body
            .get(1)
            .map_or(span(first).end, |second| span(second).start)
    } else {
        first_start
    };
    if insertion != first_start {
        let line = source.text[..insertion]
            .rfind(['\r', '\n'])
            .map_or(0, |offset| offset + 1);
        if !source.text[line..insertion]
            .chars()
            .all(|c| c == ' ' || c == '\t')
        {
            edits.insert(insertion, format!("\n{indent}"));
        }
    }
    Ok((insertion, indent))
}

struct Validate<'s> {
    top_level: BTreeSet<Span>,
    intrinsics: BTreeSet<String>,
    error: Option<anyhow::Error>,
    source: &'s Source,
}
impl Validate<'_> {
    fn reject(&mut self, reason: impl Into<String>) {
        if self.error.is_none() {
            self.error = Some(anyhow::anyhow!(reason.into()));
        }
    }
}
impl<'a> Visitor<'a> for Validate<'_> {
    fn visit_stmt(&mut self, node: &'a Stmt) {
        if self.error.is_some() {
            return;
        }
        match node {
            Stmt::FunctionDef(f) if f.is_async => {
                self.reject("prototype unsupported grammar: AsyncFunctionDef")
            }
            Stmt::For(_)
            | Stmt::While(_)
            | Stmt::Try(_)
            | Stmt::With(_)
            | Stmt::Match(_)
            | Stmt::AugAssign(_)
            | Stmt::Delete(_)
            | Stmt::Global(_)
            | Stmt::Nonlocal(_) => {
                self.reject(format!(
                    "prototype unsupported grammar at byte {}",
                    span(node).start
                ));
            }
            Stmt::FunctionDef(f) if intrinsic(f.name.as_str()) => {
                self.reject("prototype cannot resolve shadowed assert_type/reveal_type")
            }
            Stmt::ClassDef(c) if intrinsic(c.name.as_str()) => {
                self.reject("prototype cannot resolve shadowed assert_type/reveal_type")
            }
            Stmt::ClassDef(_) if !self.top_level.contains(&span(node)) => {
                self.reject("prototype does not support nested classes")
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.as_str())
                        .unwrap_or_else(|| alias.name.as_str().split('.').next().unwrap());
                    if intrinsic(bound) {
                        self.reject("prototype cannot resolve imported/shadowed typing intrinsic");
                    }
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    let bound = alias
                        .asname
                        .as_ref()
                        .map(|n| n.as_str())
                        .unwrap_or(alias.name.as_str());
                    if intrinsic(bound) {
                        if self.top_level.contains(&span(node))
                            && import.level == 0
                            && import.module.as_ref().is_some_and(|n| {
                                matches!(n.as_str(), "typing" | "typing_extensions")
                            })
                            && alias.name.as_str() == bound
                        {
                            self.intrinsics.insert(bound.to_owned());
                        } else {
                            self.reject(
                                "prototype cannot resolve imported/shadowed typing intrinsic",
                            );
                        }
                    }
                }
            }
            _ => {}
        }
        if self.error.is_none() {
            visitor::walk_stmt(self, node);
        }
    }
    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        if intrinsic(parameter.name.as_str()) {
            self.reject("prototype cannot resolve shadowed assert_type/reveal_type");
        }
        visitor::walk_parameter(self, parameter);
    }
    fn visit_expr(&mut self, expression: &'a Expr) {
        if self.error.is_some() {
            return;
        }
        match expression {
            Expr::Name(n) if n.ctx == ExprContext::Store && intrinsic(n.id.as_str()) => {
                self.reject("prototype cannot resolve shadowed assert_type/reveal_type")
            }
            Expr::Await(_)
            | Expr::Yield(_)
            | Expr::YieldFrom(_)
            | Expr::Named(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
            | Expr::Generator(_) => {
                self.reject(format!(
                    "prototype unsupported expression: {}",
                    self.source.slice(span(expression))
                ));
            }
            _ => {}
        }
        if self.error.is_none() {
            visitor::walk_expr(self, expression);
        }
    }
}

/// Collect bindings for an entire function scope, without entering nested scopes.
/// Python treats a later assignment as local at earlier uses as well.
#[derive(Default)]
struct LocalBindings {
    names: BTreeSet<String>,
}
impl<'a> Visitor<'a> for LocalBindings {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(f) => {
                self.names.insert(f.name.to_string());
            }
            Stmt::ClassDef(c) => {
                self.names.insert(c.name.to_string());
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    self.names.insert(
                        alias
                            .asname
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| {
                                alias.name.as_str().split('.').next().unwrap().to_owned()
                            }),
                    );
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    self.names.insert(
                        alias
                            .asname
                            .as_ref()
                            .map(ToString::to_string)
                            .unwrap_or_else(|| alias.name.to_string()),
                    );
                }
            }
            _ => visitor::walk_stmt(self, statement),
        }
    }
    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Name(n) if n.ctx == ExprContext::Store => {
                self.names.insert(n.id.to_string());
            }
            Expr::Lambda(_) => {}
            _ => visitor::walk_expr(self, expression),
        }
    }
}

struct Calls<'s> {
    source: &'s Source,
    globals: BTreeSet<String>,
    shadowed: BTreeSet<String>,
    calls: BTreeMap<Span, crate::model::CallSite>,
}
impl<'a> Visitor<'a> for Calls<'_> {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        if let Stmt::FunctionDef(function) = statement {
            // Defaults/decorators are evaluated in the parent scope.
            for decorator in &function.decorator_list {
                self.visit_decorator(decorator);
            }
            self.visit_parameters(&function.parameters);
            if let Some(returns) = &function.returns {
                self.visit_annotation(returns);
            }
            let parent = self.shadowed.clone();
            let mut locals = LocalBindings::default();
            locals
                .names
                .extend(function.parameters.iter().map(|p| p.name().to_string()));
            locals.visit_body(&function.body);
            self.shadowed.extend(locals.names);
            self.visit_body(&function.body);
            self.shadowed = parent;
        } else {
            visitor::walk_stmt(self, statement);
        }
    }
    fn visit_expr(&mut self, expression: &'a Expr) {
        if let Expr::Lambda(lambda) = expression {
            if let Some(parameters) = &lambda.parameters {
                self.visit_parameters(parameters);
            }
            let parent = self.shadowed.clone();
            if let Some(parameters) = &lambda.parameters {
                self.shadowed
                    .extend(parameters.iter().map(|p| p.name().to_string()));
            }
            self.visit_expr(&lambda.body);
            self.shadowed = parent;
            return;
        }
        if let Expr::Call(call) = expression
            && let Expr::Name(name) = call.func.as_ref()
            && !self.shadowed.contains(name.id.as_str())
            && self.globals.contains(name.id.as_str())
        {
            self.calls.insert(
                span(call),
                call_site(call, format!("{}.{}", self.source.module, name.id)),
            );
        }
        visitor::walk_expr(self, expression);
    }
}

fn expression(value: &Value, key: &str) -> Result<String> {
    let text = value
        .get(key)
        .and_then(|v| v.get("expression"))
        .and_then(Value::as_str)
        .with_context(|| format!("missing {key}.expression in plugin response"))?;
    ruff_python_parser::parse_expression(text)
        .with_context(|| format!("invalid plugin type expression: {text}"))?;
    Ok(text.to_owned())
}

pub fn analyze(source: &Source, suite: &[Stmt], runtime: &mut Runtime) -> Result<Analysis> {
    if runtime.project.is_some() {
        return crate::lower::analyze(source, suite, runtime);
    }
    let mut validation = Validate {
        top_level: suite.iter().map(span).collect(),
        intrinsics: BTreeSet::new(),
        error: None,
        source,
    };
    validation.visit_body(suite);
    if let Some(error) = validation.error {
        return Err(error);
    }
    let mut classes = BTreeMap::new();
    let mut functions = BTreeSet::new();
    let mut edits = Edits::default();
    for statement in suite {
        match statement {
            Stmt::ClassDef(class) => {
                let (insertion, indent) = class_insertion(source, class, &mut edits)?;
                let mut fields = BTreeMap::new();
                let mut methods = BTreeSet::new();
                for member in &class.body {
                    match member {
                        Stmt::AnnAssign(field) => {
                            if let Expr::Name(name) = field.target.as_ref() {
                                fields.insert(
                                    name.id.to_string(),
                                    source.slice(span(field.annotation.as_ref())).to_owned(),
                                );
                            }
                        }
                        Stmt::FunctionDef(method) => {
                            methods.insert(method.name.to_string());
                        }
                        _ => {}
                    }
                }
                let declared = fields.keys().cloned().collect();
                classes.insert(
                    class.name.to_string(),
                    ClassInfo {
                        origin: span(&class.name),
                        fields,
                        methods,
                        insertion,
                        indent,
                        declared,
                    },
                );
            }
            Stmt::FunctionDef(function) => {
                functions.insert(function.name.to_string());
            }
            _ => {}
        }
    }
    for statement in suite {
        let Stmt::ClassDef(class) = statement else {
            continue;
        };
        let info = classes.get_mut(class.name.as_str()).unwrap();
        let bases: Vec<Value> = class.arguments.as_ref().map_or_else(Vec::new, |args| {
            args.args
                .iter()
                .map(|base| typ(source.slice(span(base))))
                .collect()
        });
        // SDK field order follows the source declaration order, independent of
        // the lookup map used by later flow passes.
        let fields: Vec<Value> = class
            .body
            .iter()
            .filter_map(|member| {
                let Stmt::AnnAssign(field) = member else {
                    return None;
                };
                let Expr::Name(name) = field.target.as_ref() else {
                    return None;
                };
                Some(json!({"name":name.id.as_str(),"annotation":typ(source.slice(span(field.annotation.as_ref())))}))
            })
            .collect();
        let response = runtime.execute(source, "analyze-class", json!({"class": {"qualified-name":format!("{}.{}", source.module, class.name), "bases":bases, "fields":fields}}))?;
        if response["kind"] == "class-patch" {
            for field in response
                .get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = field["name"]
                    .as_str()
                    .context("class patch field without name")?;
                ensure!(
                    !info.fields.contains_key(name),
                    "prototype does not replace existing class fields: {name}"
                );
                info.fields
                    .insert(name.to_owned(), expression(field, "instance-get-type")?);
            }
        }
    }
    let mut collector = Calls {
        source,
        globals: functions.iter().chain(classes.keys()).cloned().collect(),
        shadowed: BTreeSet::new(),
        calls: BTreeMap::new(),
    };
    collector.visit_body(suite);
    let mut helpers = BTreeMap::new();
    let mut definitions = String::new();
    let mut diagnostics = Vec::new();
    for call in collector.calls.values() {
        let response = call_request(runtime, source, "adjust-call-signature", call, None, None)?;
        for diagnostic in response["diagnostics"].as_array().into_iter().flatten() {
            diagnostics.push(json!({"diagnostic":diagnostic,"source":source.location(call.span)}));
        }
        if response["kind"] != "call-signature-patch" {
            continue;
        }
        let signature = &response["signature"];
        let mut parameters = Vec::new();
        for (index, parameter) in signature
            .get("parameters")
            .and_then(Value::as_array)
            .context("signature missing parameters")?
            .iter()
            .enumerate()
        {
            if parameter["kind"] != "positional-or-keyword" {
                bail!("prototype helper supports simple parameter kinds");
            }
            let name = parameter["name"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("arg{index}"));
            let annotation = if parameter.get("type-expr").is_some() {
                expression(parameter, "type-expr")?
            } else {
                "object".to_owned()
            };
            let default = if parameter["required"].as_bool().unwrap_or(false) {
                ""
            } else {
                " = ..."
            };
            parameters.push(format!("{name}: {annotation}{default}"));
        }
        let helper = format!("{}call_{}", source.prefix, call.span.start);
        let return_type = expression(signature, "return-type")?;
        let definition = format!(
            "def {helper}({}) -> {return_type}:\n    raise NotImplementedError\n",
            parameters.join(", ")
        );
        ruff_python_parser::parse_module(&definition)
            .context("plugin signature cannot be represented as helper")?;
        definitions.push_str(&definition);
        helpers.insert(helper.clone(), call.callee.clone());
        edits.replace(call.function, helper);
    }
    let offset = prelude_offset(source, suite);
    let separator = if offset > 0 && !source.text[..offset].ends_with('\n') {
        "\n"
    } else {
        ""
    };
    edits.insert(offset, format!("{separator}from __future__ import annotations\nfrom typing import cast as {}\n{definitions}",source.cast_name()));
    Ok(Analysis {
        prelude_offset: offset,
        classes,
        intrinsics: validation.intrinsics,
        calls: collector.calls,
        unbound_calls: BTreeSet::new(),
        annotated_casts: BTreeMap::new(),
        constructor_calls: BTreeMap::new(),
        edits,
        helpers,
        expression_probes: BTreeMap::new(),
        plugin_diagnostics: diagnostics,
        generated_declarations: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injected_members_preserve_leading_class_docstrings_and_inline_suites() {
        for text in [
            "class Example:\n    'Class docs'\n    pass\n",
            "class Example:\n    'Class docs'\n",
            "class Example: 'Class docs'\n",
            "class Example: 'Class docs'; pass\n",
            "class Outer:\n    class Example: 'Class docs'; pass\n",
        ] {
            let source = Source::new(text.to_owned(), "demo.py".into(), "demo".into());
            let suite = ruff_python_parser::parse_module(text).unwrap();
            let Stmt::ClassDef(outer) = &suite.syntax().body[0] else {
                panic!()
            };
            let class = if let Some(Stmt::ClassDef(inner)) = outer.body.first() {
                inner
            } else {
                outer
            };
            let mut edits = Edits::default();
            let (offset, indent) = class_insertion(&source, class, &mut edits).unwrap();
            edits.insert(offset, format!("generated: int\n{indent}"));
            let rendered = crate::mapping::render(&source, &edits).unwrap();
            let parsed = ruff_python_parser::parse_module(&rendered.text)
                .unwrap_or_else(|error| panic!("{error}: {}", rendered.text));
            let Stmt::ClassDef(outer) = &parsed.syntax().body[0] else {
                panic!()
            };
            let class = if let Some(Stmt::ClassDef(inner)) = outer.body.first() {
                inner
            } else {
                outer
            };
            assert!(
                matches!(&class.body[0], Stmt::Expr(node) if matches!(node.value.as_ref(), Expr::StringLiteral(_))),
                "{}",
                rendered.text
            );
            assert!(
                matches!(&class.body[1], Stmt::AnnAssign(_)),
                "{}",
                rendered.text
            );
        }
    }

    fn source(text: &str) -> Source {
        Source::new(text.to_owned(), "demo.py".into(), "demo".into())
    }

    #[test]
    fn insertion_follows_docstring_and_future_imports() {
        for newline in ["\n", "\r\n", "\r"] {
            let source = source(
                &"#!/usr/bin/env python\n\"\"\"module docs\"\"\"\nfrom __future__ import annotations # retained\nfrom typing import assert_type\n".replace('\n',newline),
            );
            let parsed = ruff_python_parser::parse_module(&source.text).unwrap();
            let offset = prelude_offset(&source, &parsed.syntax().body);
            assert!(source.text[offset..].starts_with("from typing import assert_type"));
        }
    }

    #[test]
    fn local_binding_and_lambda_parameter_prevent_global_hook_dispatch() {
        let source = source(
            "def decode(value: object) -> object:\n    return value\ndef example(decode):\n    return decode('42')\ndef later():\n    result = decode('42')\n    decode = lambda item: item\n    return result\nvalue = (lambda decode: decode('42'))(lambda item: item)\nactual = decode('42')\n",
        );
        let parsed = ruff_python_parser::parse_module(&source.text).unwrap();
        let mut calls = Calls {
            source: &source,
            globals: BTreeSet::from(["decode".into()]),
            shadowed: BTreeSet::new(),
            calls: BTreeMap::new(),
        };
        calls.visit_body(&parsed.syntax().body);
        assert_eq!(calls.calls.len(), 1);
        assert_eq!(
            calls.calls.values().next().unwrap().span.start,
            source.text.rfind("decode('42')").unwrap()
        );
    }

    #[test]
    fn foreign_or_shadowed_typing_intrinsics_are_rejected() {
        for input in [
            "from other import assert_type\n",
            "from typing import assert_type\ndef f(assert_type):\n    pass\n",
            "from typing import reveal_type\nreveal_type = str\n",
            "while True:\n    pass\n",
        ] {
            let source = source(input);
            let parsed = ruff_python_parser::parse_module(&source.text).unwrap();
            let mut validation = Validate {
                top_level: parsed.syntax().body.iter().map(span).collect(),
                intrinsics: BTreeSet::new(),
                error: None,
                source: &source,
            };
            validation.visit_body(&parsed.syntax().body);
            assert!(
                validation.error.is_some(),
                "accepted unsupported source: {input}"
            );
        }
    }
}
