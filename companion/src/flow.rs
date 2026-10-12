//! Native flow projection for the bounded companion compatibility fixtures.
//!
//! The real source AST is never rewritten. Each inferred member view becomes a
//! source-mapped wrapper, so synthetic casts cannot change object identities or
//! be mistaken for user calls. Unsupported control-flow constructs are rejected.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use anyhow::{Result, bail};
use ruff_python_ast::{ElifElseClause, Expr, ExprContext, Pattern, Stmt, StmtFunctionDef};
use serde_json::{Value, json};

use crate::mapping::Edits;
use crate::model::{
    Analysis, Oracle, ReturnPatches, Source, Span, call_site, expression_key, literal, span, typ,
};
use crate::runtime::{Runtime, call_request};
use crate::type_render::Types;

#[derive(Clone)]
pub struct FlowResult {
    pub edits: Edits,
    pub diagnostics: Vec<Value>,
    pub type_prelude: String,
    pub type_names: BTreeMap<String, String>,
}

type Object = usize;
type Facts = Arc<BTreeMap<String, Value>>;

/// Find outer bindings that a newly created nested function can replace.
/// Intervening function locals shadow outer cells; class namespaces do not.
fn captured_nonlocals(function: &StmtFunctionDef, outer: &BTreeSet<String>) -> BTreeSet<String> {
    use ruff_python_ast::visitor::{self, Visitor};
    #[derive(Default)]
    struct Bindings {
        locals: BTreeSet<String>,
        nonlocals: BTreeSet<String>,
        globals: BTreeSet<String>,
    }
    impl<'a> Visitor<'a> for Bindings {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::Nonlocal(stmt) => self
                    .nonlocals
                    .extend(stmt.names.iter().map(ToString::to_string)),
                Stmt::Global(stmt) => self
                    .globals
                    .extend(stmt.names.iter().map(ToString::to_string)),
                Stmt::FunctionDef(stmt) => {
                    self.locals.insert(stmt.name.to_string());
                }
                Stmt::ClassDef(stmt) => {
                    self.locals.insert(stmt.name.to_string());
                }
                _ => visitor::walk_stmt(self, stmt),
            }
        }
        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Name(name) if name.ctx == ExprContext::Store => {
                    self.locals.insert(name.id.to_string());
                }
                Expr::Lambda(_) => {}
                _ if crate::model::comprehension(expr).is_some() => {
                    self.locals
                        .extend(crate::model::comprehension_outer_bindings(expr));
                }
                _ => visitor::walk_expr(self, expr),
            }
        }
        fn visit_pattern(&mut self, pattern: &'a Pattern) {
            self.locals.extend(crate::model::pattern_bindings(pattern));
        }
    }
    struct Captures<'a> {
        outer: &'a BTreeSet<String>,
        shadowed: BTreeSet<String>,
        captured: BTreeSet<String>,
    }
    impl Captures<'_> {
        fn function(&mut self, function: &StmtFunctionDef) {
            let mut bindings = Bindings::default();
            bindings.visit_body(&function.body);
            bindings
                .locals
                .extend(function.parameters.iter().map(|p| p.name().to_string()));
            self.captured.extend(
                bindings
                    .nonlocals
                    .iter()
                    .filter(|name| self.outer.contains(*name) && !self.shadowed.contains(*name))
                    .cloned(),
            );
            let previous = self.shadowed.clone();
            self.shadowed.extend(
                bindings
                    .locals
                    .difference(&bindings.nonlocals)
                    .filter(|name| !bindings.globals.contains(*name))
                    .cloned(),
            );
            self.visit_body(&function.body);
            self.shadowed = previous;
        }
    }
    impl<'a> Visitor<'a> for Captures<'_> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            if let Stmt::FunctionDef(function) = stmt {
                self.function(function);
            } else {
                visitor::walk_stmt(self, stmt);
            }
        }
    }
    let mut captures = Captures {
        outer,
        shadowed: BTreeSet::new(),
        captured: BTreeSet::new(),
    };
    captures.function(function);
    captures.captured
}

#[derive(Clone, Default)]
struct Scope {
    names: BTreeMap<String, Object>,
    facts: BTreeMap<Object, Facts>,
    types: BTreeMap<Object, String>,
    fresh: BTreeSet<Object>,
    objects: BTreeMap<Span, Object>,
    module: bool,
    terminated: bool,
    shadowed: BTreeSet<String>,
    truths: BTreeMap<String, bool>,
    external_names: BTreeSet<String>,
    class_owner: Option<String>,
    protocol_names: BTreeMap<String, Value>,
    protocol_values: BTreeMap<Span, Value>,
}

impl Scope {
    fn protocol_type(&self, expr: &Expr) -> Option<&Value> {
        if let Expr::Name(name) = expr {
            self.protocol_names.get(name.id.as_str())
        } else {
            self.protocol_values.get(&span(expr))
        }
    }
    fn identity(&self, expr: &Expr) -> Option<Object> {
        if let Expr::Name(name) = expr {
            self.names.get(name.id.as_str()).copied()
        } else {
            self.objects.get(&span(expr)).copied()
        }
    }

    fn invalidate(&mut self) {
        self.facts.clear();
        // At module level an unknown call can also rebind globals. Local names
        // retain identity while member facts are invalidated.
        if self.module {
            self.names.clear();
            self.truths.clear();
            self.protocol_names.clear();
        } else {
            self.names
                .retain(|name, _| !self.external_names.contains(name));
            self.truths
                .retain(|name, _| !self.external_names.contains(name));
            self.protocol_names
                .retain(|name, _| !self.external_names.contains(name));
        }
    }
}

struct Project<'a> {
    source: &'a Source,
    analysis: &'a mut Analysis,
    runtime: &'a mut Runtime,
    result: FlowResult,
    next_object: Object,
    oracle: Option<&'a Oracle>,
    returns: &'a ReturnPatches,
    raw_returns: &'a BTreeMap<Span, Value>,
    types: Types,
}

pub fn project(
    source: &Source,
    suite: &[Stmt],
    analysis: &mut Analysis,
    runtime: &mut Runtime,
) -> Result<FlowResult> {
    project_with_oracle(
        source,
        suite,
        analysis,
        runtime,
        None,
        &ReturnPatches::new(),
    )
}

pub fn project_with_oracle(
    source: &Source,
    suite: &[Stmt],
    analysis: &mut Analysis,
    runtime: &mut Runtime,
    oracle: Option<&Oracle>,
    returns: &ReturnPatches,
) -> Result<FlowResult> {
    project_with_return_types(
        source,
        suite,
        analysis,
        runtime,
        oracle,
        returns,
        &BTreeMap::new(),
    )
}

pub fn project_with_return_types(
    source: &Source,
    suite: &[Stmt],
    analysis: &mut Analysis,
    runtime: &mut Runtime,
    oracle: Option<&Oracle>,
    returns: &ReturnPatches,
    raw_returns: &BTreeMap<Span, Value>,
) -> Result<FlowResult> {
    for call in analysis.calls.values_mut() {
        call.flow_receiver_type = None;
        for argument in &mut call.arguments {
            argument.inferred_type = None;
        }
    }
    let prefix = format!("{}state_", source.prefix);
    let types = runtime.project.as_ref().map_or_else(
        || Types::new(&prefix, &source.module),
        |project| Types::with_project(&prefix, &source.module, project),
    );
    let mut project = Project {
        source,
        analysis,
        runtime,
        result: FlowResult {
            edits: Edits::default(),
            diagnostics: Vec::new(),
            type_prelude: String::new(),
            type_names: BTreeMap::new(),
        },
        next_object: 0,
        oracle,
        returns,
        raw_returns,
        types,
    };
    let mut scope = Scope {
        module: true,
        ..Scope::default()
    };
    project.body(&mut scope, suite)?;
    project.result.type_prelude = project.types.prelude();
    project.result.type_names = project.types.display_names();
    project.types.register_protocol_types(project.runtime);
    Ok(project.result)
}

impl Project<'_> {
    fn fresh(&mut self, scope: &mut Scope, owner: &str, guaranteed: bool) -> Object {
        self.next_object += 1;
        let id = self.next_object;
        scope.types.insert(id, owner.to_string());
        if guaranteed {
            scope.fresh.insert(id);
        }
        id
    }

    fn body(&mut self, scope: &mut Scope, suite: &[Stmt]) -> Result<()> {
        for stmt in suite {
            if scope.terminated {
                break;
            }
            self.stmt(scope, stmt)?;
        }
        Ok(())
    }

    fn function(&mut self, function: &StmtFunctionDef, owner: Option<&str>) -> Result<()> {
        if function.is_async && self.runtime.project.is_none() {
            bail!("Rust companion does not implement async function flow");
        }
        let mut sub = Scope::default();
        sub.shadowed.extend(
            function
                .parameters
                .iter()
                .map(|parameter| parameter.name().to_string()),
        );
        struct Bindings(BTreeSet<String>);
        impl<'a> ruff_python_ast::visitor::Visitor<'a> for Bindings {
            fn visit_pattern(&mut self, pattern: &'a Pattern) {
                self.0.extend(crate::model::pattern_bindings(pattern));
            }
            fn visit_stmt(&mut self, statement: &'a Stmt) {
                match statement {
                    Stmt::FunctionDef(function) => {
                        self.0.insert(function.name.to_string());
                    }
                    Stmt::ClassDef(class) => {
                        self.0.insert(class.name.to_string());
                    }
                    _ => ruff_python_ast::visitor::walk_stmt(self, statement),
                }
            }
            fn visit_expr(&mut self, expression: &'a Expr) {
                if crate::model::comprehension(expression).is_some() {
                    self.0
                        .extend(crate::model::comprehension_outer_bindings(expression));
                    return;
                }
                match expression {
                    Expr::Name(name) if name.ctx == ExprContext::Store => {
                        self.0.insert(name.id.to_string());
                    }
                    Expr::Lambda(_) => {}
                    _ => ruff_python_ast::visitor::walk_expr(self, expression),
                }
            }
        }
        let mut bindings = Bindings(BTreeSet::new());
        ruff_python_ast::visitor::Visitor::visit_body(&mut bindings, &function.body);
        sub.shadowed.extend(bindings.0);
        let decorators: Vec<_> = function
            .decorator_list
            .iter()
            .map(|decorator| {
                self.runtime
                    .canonical_name(self.source, self.source.slice(span(&decorator.expression)))
            })
            .collect();
        let implicit_parameter = owner.and_then(|owner| {
            if decorators
                .iter()
                .any(|name| matches!(name.as_str(), "staticmethod" | "builtins.staticmethod"))
            {
                return None;
            }
            let first = function.parameters.iter_non_variadic_params().next()?;
            let ty = if decorators
                .iter()
                .any(|name| matches!(name.as_str(), "classmethod" | "builtins.classmethod"))
            {
                format!("type[{owner}]")
            } else {
                owner.to_owned()
            };
            let id = self.fresh(&mut sub, &ty, false);
            sub.names.insert(first.name().to_string(), id);
            Some(first.name().to_string())
        });
        for parameter in function.parameters.iter_non_variadic_params() {
            if let Some(annotation) = parameter.annotation()
                && let Some(truthy) = Self::type_truthiness(self.source.slice(span(annotation)))
            {
                sub.truths.insert(parameter.name().to_string(), truthy);
            }
            if implicit_parameter.as_deref() != Some(parameter.name().as_str())
                && let Some(annotation) = parameter.annotation()
            {
                let annotation = self.source.slice(span(annotation));
                let owner = self.runtime.canonical_name(self.source, annotation);
                if self.analysis.classes.contains_key(&owner)
                    || self.runtime.project.as_ref().is_some_and(|project| {
                        project
                            .class_summary(crate::runtime::generic_parts(&owner).0)
                            .is_some()
                    })
                {
                    let id = self.fresh(&mut sub, &owner, false);
                    sub.names.insert(parameter.name().to_string(), id);
                }
            }
        }
        self.body(&mut sub, &function.body)
    }

    fn stmt(&mut self, scope: &mut Scope, stmt: &Stmt) -> Result<()> {
        match stmt {
            Stmt::ClassDef(class) => {
                // Bases, keywords and decorators execute in the enclosing
                // scope. The class namespace is separate; method bodies are
                // deferred and cannot establish facts at declaration time.
                for decorator in &class.decorator_list {
                    self.expr(scope, &decorator.expression)?;
                }
                if let Some(arguments) = &class.arguments {
                    for expression in &arguments.args {
                        self.expr(scope, expression)?;
                    }
                    for keyword in &arguments.keywords {
                        self.expr(scope, &keyword.value)?;
                    }
                }
                let owner = scope.class_owner.as_ref().map_or_else(
                    || {
                        self.runtime
                            .canonical_name(self.source, class.name.as_str())
                    },
                    |parent| format!("{parent}.{}", class.name),
                );
                let mut class_scope = Scope {
                    class_owner: Some(owner),
                    ..Scope::default()
                };
                self.body(&mut class_scope, &class.body)?;
                scope.truths.remove(class.name.as_str());
            }
            Stmt::FunctionDef(function) => {
                for decorator in &function.decorator_list {
                    self.expr(scope, &decorator.expression)?;
                }
                for parameter in function.parameters.iter_non_variadic_params() {
                    if let Some(default) = &parameter.default {
                        self.expr(scope, default)?;
                    }
                }
                scope
                    .external_names
                    .extend(captured_nonlocals(function, &scope.shadowed));
                self.function(function, scope.class_owner.as_deref())?;
                scope.truths.remove(function.name.as_str());
            }
            Stmt::Assign(assign) => {
                self.expr(scope, &assign.value)?;
                let identity = self.identity(scope, &assign.value);
                for target in &assign.targets {
                    self.assignment(scope, target, Some(&assign.value), identity)?;
                }
            }
            Stmt::AnnAssign(assign) => {
                // Annotation syntax is not an object operation. The initializer
                // has ordinary assignment identity and mutation semantics.
                if let Some(value) = &assign.value {
                    self.expr(scope, value)?;
                    let identity = self.identity(scope, value);
                    self.assignment(scope, &assign.target, Some(value), identity)?;
                }
            }
            Stmt::If(branch) => {
                self.expr(scope, &branch.test)?;
                if let Some(truthy) = self.truthiness(scope, &branch.test) {
                    if truthy {
                        self.body(scope, &branch.body)?;
                    } else {
                        self.else_clauses(scope, &branch.elif_else_clauses)?;
                    }
                    return Ok(());
                }
                let initial = scope.clone();
                self.body(scope, &branch.body)?;
                let left = scope.clone();
                *scope = initial;
                self.else_clauses(scope, &branch.elif_else_clauses)?;
                let right = scope.clone();
                self.join(scope, left, right);
            }
            Stmt::Match(statement) if self.runtime.project.is_some() => {
                self.expr(scope, &statement.subject)?;
                let mut unmatched = scope.clone();
                let mut branches = Vec::new();
                let mut exhaustive = false;
                for case in &statement.cases {
                    let mut branch = unmatched.clone();
                    self.pattern(&mut branch, &case.pattern)?;
                    if let Some(guard) = &case.guard {
                        self.expr(&mut branch, guard)?;
                        // Pattern failure does not execute the guard; guard
                        // failure retains effects from evaluating the guard.
                        let failed_guard = branch.clone();
                        let failed_pattern = unmatched.clone();
                        self.join(&mut unmatched, failed_pattern, failed_guard);
                    }
                    self.body(&mut branch, &case.body)?;
                    branches.push(branch);
                    if case.guard.is_none() && case.pattern.is_wildcard() {
                        exhaustive = true;
                        break;
                    }
                }
                if !exhaustive {
                    branches.push(unmatched);
                }
                if let Some(first) = branches.pop() {
                    *scope = first;
                    for branch in branches {
                        let previous = scope.clone();
                        self.join(scope, previous, branch);
                    }
                }
            }
            Stmt::Expr(expr) => self.expr(scope, &expr.value)?,
            Stmt::Return(ret) => {
                if let Some(value) = &ret.value {
                    self.expr(scope, value)?;
                }
                scope.terminated = true;
            }
            Stmt::Raise(raise) => {
                if let Some(value) = &raise.exc {
                    self.expr(scope, value)?;
                }
                if let Some(value) = &raise.cause {
                    self.expr(scope, value)?;
                }
                scope.terminated = true;
            }
            Stmt::Assert(assert) => {
                self.expr(scope, &assert.test)?;
                if let Some(value) = &assert.msg {
                    // The message executes only on the raising branch.
                    let mut failed = scope.clone();
                    self.expr(&mut failed, value)?;
                }
            }
            Stmt::Try(attempt) if self.runtime.project.is_some() => {
                let before = scope.clone();
                self.body(scope, &attempt.body)?;
                self.body(scope, &attempt.orelse)?;
                let mut successful = scope.clone();
                for handler in &attempt.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    let mut exceptional = before.clone();
                    // The failure may occur at any point before a call returns;
                    // none of its successful-call postconditions hold here.
                    exceptional.invalidate();
                    if let Some(name) = &handler.name {
                        exceptional.names.remove(name.as_str());
                        exceptional.truths.remove(name.as_str());
                    }
                    self.body(&mut exceptional, &handler.body)?;
                    let previous = successful.clone();
                    self.join(&mut successful, previous, exceptional);
                }
                *scope = successful;
                let terminated = scope.terminated;
                scope.terminated = false;
                self.body(scope, &attempt.finalbody)?;
                scope.terminated |= terminated;
            }
            Stmt::For(loop_) if self.runtime.project.is_some() => {
                self.expr(scope, &loop_.iter)?;
                scope.invalidate();
                let initial = scope.clone();
                self.assignment(scope, &loop_.target, None, None)?;
                self.body(scope, &loop_.body)?;
                let mut executed = scope.clone();
                executed.terminated = false;
                self.join(scope, initial, executed);
                scope.terminated = false;
                scope.invalidate();
                self.loop_else(scope, &loop_.orelse)?;
            }
            Stmt::While(loop_) if self.runtime.project.is_some() => {
                self.expr(scope, &loop_.test)?;
                scope.invalidate();
                let initial = scope.clone();
                self.body(scope, &loop_.body)?;
                let mut executed = scope.clone();
                executed.terminated = false;
                self.join(scope, initial, executed);
                scope.terminated = false;
                scope.invalidate();
                self.loop_else(scope, &loop_.orelse)?;
            }
            Stmt::With(with) if self.runtime.project.is_some() => {
                for item in &with.items {
                    self.expr(scope, &item.context_expr)?;
                    scope.invalidate();
                    if let Some(target) = &item.optional_vars {
                        self.assignment(scope, target, None, None)?;
                    }
                }
                self.body(scope, &with.body)?;
                scope.invalidate();
            }
            Stmt::AugAssign(assign) if self.runtime.project.is_some() => {
                self.expr(scope, &assign.target)?;
                self.expr(scope, &assign.value)?;
                if let Expr::Subscript(target) = assign.target.as_ref() {
                    // The pinned checker reports augmented writes through its
                    // ordinary ItemSet hook, using the target as the value's
                    // source expression (its literal summary is unknown).
                    self.item_mutation(scope, target, Some(&assign.target))?;
                }
                scope.invalidate();
            }
            Stmt::Delete(delete) if self.runtime.project.is_some() => {
                for target in &delete.targets {
                    if let Expr::Name(name) = target {
                        scope.names.remove(name.id.as_str());
                        scope.truths.remove(name.id.as_str());
                    } else {
                        self.expr(scope, target)?;
                    }
                }
                scope.invalidate();
            }
            Stmt::Break(_) | Stmt::Continue(_) if self.runtime.project.is_some() => {
                scope.terminated = true
            }
            Stmt::Global(statement) if self.runtime.project.is_some() => {
                scope
                    .external_names
                    .extend(statement.names.iter().map(ToString::to_string));
                scope.invalidate();
            }
            Stmt::Nonlocal(statement) if self.runtime.project.is_some() => {
                scope
                    .external_names
                    .extend(statement.names.iter().map(ToString::to_string));
                scope.invalidate();
            }
            Stmt::TypeAlias(_) if self.runtime.project.is_some() => {}
            Stmt::Import(import) => {
                for alias in &import.names {
                    let name = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
                    scope.truths.remove(name.split('.').next().unwrap_or(name));
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    scope
                        .truths
                        .remove(alias.asname.as_ref().unwrap_or(&alias.name).as_str());
                }
            }
            Stmt::Pass(_) => {}
            _ => bail!(
                "Rust companion unsupported statement flow at {:?}",
                span(stmt)
            ),
        }
        Ok(())
    }

    fn loop_else(&mut self, scope: &mut Scope, suite: &[Stmt]) -> Result<()> {
        // A break bypasses this suite, including its alias assignments and
        // return/raise. Preserve both possible continuations without treating
        // nested-loop or nested-function breaks as breaks of the outer loop.
        let skipped = scope.clone();
        self.body(scope, suite)?;
        let executed = scope.clone();
        self.join(scope, skipped, executed);
        // The fork resets member facts at every loop boundary, including the
        // zero-iteration path. The checker still owns exact reachability.
        scope.invalidate();
        scope.terminated = false;
        Ok(())
    }

    fn pattern(&mut self, scope: &mut Scope, pattern: &Pattern) -> Result<()> {
        for name in crate::model::pattern_bindings(pattern) {
            scope.truths.remove(&name);
        }
        match pattern {
            Pattern::MatchValue(pattern) => self.expr(scope, &pattern.value)?,
            Pattern::MatchSingleton(_) => {}
            Pattern::MatchSequence(pattern) => {
                for pattern in &pattern.patterns {
                    self.pattern(scope, pattern)?;
                }
            }
            Pattern::MatchMapping(pattern) => {
                for key in &pattern.keys {
                    self.expr(scope, key)?;
                }
                for pattern in &pattern.patterns {
                    self.pattern(scope, pattern)?;
                }
                if let Some(name) = &pattern.rest {
                    scope.names.remove(name.as_str());
                    scope.shadowed.insert(name.to_string());
                }
            }
            Pattern::MatchClass(pattern) => {
                self.expr(scope, &pattern.cls)?;
                for pattern in &pattern.arguments.patterns {
                    self.pattern(scope, pattern)?;
                }
                for keyword in &pattern.arguments.keywords {
                    self.pattern(scope, &keyword.pattern)?;
                }
            }
            Pattern::MatchStar(pattern) => {
                if let Some(name) = &pattern.name {
                    scope.names.remove(name.as_str());
                    scope.shadowed.insert(name.to_string());
                }
            }
            Pattern::MatchAs(pattern) => {
                if let Some(pattern) = &pattern.pattern {
                    self.pattern(scope, pattern)?;
                }
                if let Some(name) = &pattern.name {
                    scope.names.remove(name.as_str());
                    scope.shadowed.insert(name.to_string());
                }
            }
            Pattern::MatchOr(pattern) => {
                let initial = scope.clone();
                let mut branches = Vec::new();
                for pattern in &pattern.patterns {
                    let mut branch = initial.clone();
                    self.pattern(&mut branch, pattern)?;
                    branches.push(branch);
                }
                if let Some(first) = branches.pop() {
                    *scope = first;
                    for branch in branches {
                        let previous = scope.clone();
                        self.join(scope, previous, branch);
                    }
                }
            }
        }
        Ok(())
    }

    fn item_mutation(
        &mut self,
        scope: &mut Scope,
        target: &ruff_python_ast::ExprSubscript,
        value: Option<&Expr>,
    ) -> Result<()> {
        if let Some(owner) = self.identity(scope, &target.value) {
            let mut payload = json!({
                "operation":"item-set",
                "receiver":self.runtime.qualified_type_expr(&scope.types[&owner]),
                "key":{"kind":"positional", "value":literal(&target.slice), "source":self.source.location(span(target.slice.as_ref()))},
                "source":self.source.location(span(target)),
            });
            if let Some(value) = value {
                payload["value"] = json!({"kind":"positional", "value":literal(value), "source":self.source.location(span(value))});
            }
            let patch = self
                .runtime
                .execute(self.source, "validate-mutation", payload)?;
            for diagnostic in patch["diagnostics"].as_array().into_iter().flatten() {
                self.result.diagnostics.push(json!({
                    "diagnostic":diagnostic,
                    "source":self.source.location(span(target)),
                }));
            }
        }
        Ok(())
    }

    fn assignment(
        &mut self,
        scope: &mut Scope,
        target: &Expr,
        value: Option<&Expr>,
        identity: Option<Object>,
    ) -> Result<()> {
        match target {
            Expr::Name(name) => {
                let protocol = value.and_then(|value| scope.protocol_type(value)).cloned();
                scope.protocol_names.remove(name.id.as_str());
                if let Some(protocol) = protocol {
                    scope.protocol_names.insert(name.id.to_string(), protocol);
                }
                let truthy = value.and_then(|value| self.truthiness(scope, value));
                scope.truths.remove(name.id.as_str());
                if let Some(truthy) = truthy {
                    scope.truths.insert(name.id.to_string(), truthy);
                }
                scope.shadowed.insert(name.id.to_string());
                if let Some(identity) = identity {
                    scope.names.insert(name.id.to_string(), identity);
                } else {
                    scope.names.remove(name.id.as_str());
                }
            }
            Expr::Subscript(target) => {
                // Python evaluates the RHS first, then the target receiver and
                // key. Calls here can change state or return a refined object.
                self.expr(scope, &target.value)?;
                self.expr(scope, &target.slice)?;
                self.item_mutation(scope, target, value)?;
                scope.invalidate();
            }
            Expr::Attribute(target) => {
                self.expr(scope, &target.value)?;
                if let Some(identity) = self.identity(scope, &target.value) {
                    let owner = scope.types[&identity].clone();
                    let class = crate::runtime::generic_parts(&owner).0;
                    let setter = self
                        .runtime
                        .project_patches
                        .get(class)
                        .and_then(|patch| patch["fields"].as_array())
                        .and_then(|fields| {
                            fields
                                .iter()
                                .find(|field| field["name"] == target.attr.as_str())
                        })
                        .and_then(|field| field.get("instance-set-type"))
                        .cloned();
                    if let Some(setter) = setter {
                        // Native class-transform setters are checked on the
                        // receiver's own class before ordinary assignment.
                        // They do not turn into inherited Python descriptors.
                        let previous = self
                            .types
                            .set_self_context(Some(self.runtime.qualified_type_expr(&owner)));
                        let setter = self.types.render(&setter)?;
                        self.types.set_self_context(previous);
                        let helper = format!("{}setter_{}", self.source.prefix, span(target).start);
                        self.runtime.set_projected_receiver(
                            format!("{}.{}", self.source.module, helper),
                            owner,
                        );
                        let typing = self.types.typing_alias();
                        self.types.definitions.push_str(&format!(
                            "class {helper}({typing}.Protocol):\n    {}: {setter}\n",
                            target.attr
                        ));
                        self.result.edits.wrap(
                            span(target.value.as_ref()),
                            format!("{}({helper}, ", self.source.cast_name()),
                            ")".into(),
                            None,
                        );
                    }
                }
                scope.invalidate();
            }
            Expr::Tuple(tuple) if self.runtime.project.is_some() => {
                for item in &tuple.elts {
                    self.assignment(scope, item, None, None)?;
                }
            }
            Expr::List(list) if self.runtime.project.is_some() => {
                for item in &list.elts {
                    self.assignment(scope, item, None, None)?;
                }
            }
            Expr::Starred(starred) if self.runtime.project.is_some() => {
                self.assignment(scope, &starred.value, None, None)?
            }
            _ => bail!(
                "Rust companion unsupported assignment target at {:?}",
                span(target)
            ),
        }
        Ok(())
    }

    fn else_clauses(&mut self, scope: &mut Scope, clauses: &[ElifElseClause]) -> Result<()> {
        if let Some((clause, rest)) = clauses.split_first() {
            if let Some(test) = &clause.test {
                self.expr(scope, test)?;
                if let Some(truthy) = self.truthiness(scope, test) {
                    if truthy {
                        self.body(scope, &clause.body)?;
                    } else {
                        self.else_clauses(scope, rest)?;
                    }
                    return Ok(());
                }
                let initial = scope.clone();
                self.body(scope, &clause.body)?;
                let left = scope.clone();
                *scope = initial;
                self.else_clauses(scope, rest)?;
                let right = scope.clone();
                self.join(scope, left, right);
            } else {
                self.body(scope, &clause.body)?;
            }
        }
        Ok(())
    }

    fn join(&mut self, scope: &mut Scope, mut left: Scope, mut right: Scope) {
        if left.terminated {
            *scope = right;
            return;
        }
        if right.terminated {
            *scope = left;
            return;
        }
        // Different allocations on the two branches can still establish the
        // same facts and alias relationship. Pair identities, rather than
        // dropping every name whose allocation site differs. As in ty's
        // ObjectState::join these merged identities are external, not fresh:
        // pairs with only partial overlap may alias and must invalidate safely.
        let mut pairs = BTreeMap::new();
        for name in left.names.keys().cloned().collect::<Vec<_>>() {
            let first = left.names[&name];
            let Some(second) = right.names.get(&name).copied() else {
                continue;
            };
            if first == second || left.types.get(&first) != right.types.get(&second) {
                continue;
            }
            let identity = *pairs.entry((first, second)).or_insert_with(|| {
                self.next_object += 1;
                self.next_object
            });
            let owner = left.types[&first].clone();
            left.types.insert(identity, owner.clone());
            right.types.insert(identity, owner);
            if let Some(facts) = left.facts.get(&first).cloned() {
                left.facts.insert(identity, facts);
            }
            if let Some(facts) = right.facts.get(&second).cloned() {
                right.facts.insert(identity, facts);
            }
            left.names.insert(name.clone(), identity);
            right.names.insert(name, identity);
        }
        scope.shadowed = left.shadowed.union(&right.shadowed).cloned().collect();
        scope.truths = left
            .truths
            .into_iter()
            .filter(|(name, truthy)| right.truths.get(name) == Some(truthy))
            .collect();
        scope.protocol_names = left
            .protocol_names
            .into_iter()
            .filter(|(name, value)| right.protocol_names.get(name) == Some(value))
            .collect();
        scope.protocol_values = left.protocol_values;
        scope.protocol_values.extend(right.protocol_values);
        scope.external_names = left
            .external_names
            .union(&right.external_names)
            .cloned()
            .collect();
        scope.names = left
            .names
            .into_iter()
            .filter(|(name, identity)| right.names.get(name) == Some(identity))
            .collect();
        scope.types = left.types;
        scope.types.extend(right.types);
        scope.fresh = left.fresh.union(&right.fresh).copied().collect();
        scope.objects = left.objects;
        scope.objects.extend(right.objects);
        scope.facts.clear();
        for identity in left.facts.keys().chain(
            right
                .facts
                .keys()
                .filter(|identity| !left.facts.contains_key(identity)),
        ) {
            let first = left.facts.get(identity);
            let second = right.facts.get(identity);
            if let (Some(first), Some(second)) = (first, second)
                && (Arc::ptr_eq(first, second) || first == second)
            {
                scope.facts.insert(*identity, first.clone());
                continue;
            }
            let defaults = self
                .analysis
                .classes
                .get(&scope.types[identity])
                .map(|class| &class.fields);
            let mut facts = BTreeMap::new();
            for member in first.into_iter().flat_map(|facts| facts.keys()).chain(
                second
                    .into_iter()
                    .flat_map(|facts| facts.keys())
                    .filter(|member| first.is_none_or(|facts| !facts.contains_key(*member))),
            ) {
                let default = self
                    .runtime
                    .default_member_expr(&scope.types[identity], member)
                    .or_else(|| defaults.and_then(|fields| fields.get(member)).map(typ))
                    .unwrap_or_else(|| typ("object"));
                let mut types = [
                    first
                        .and_then(|facts| facts.get(member))
                        .unwrap_or(&default),
                    second
                        .and_then(|facts| facts.get(member))
                        .unwrap_or(&default),
                ];
                // ty appends the nullable fallback after the established
                // member type when joining a conditional postcondition.
                if types[0]["expression"] == "None" && types[1]["expression"] != "None" {
                    types.swap(0, 1);
                }
                let value = if types[0] == types[1] {
                    types[0].clone()
                } else {
                    let elements: Vec<_> = types
                        .iter()
                        .map(|value| {
                            value.get("snapshot").cloned().unwrap_or_else(|| {
                                let mut snapshot = (*value).clone();
                                snapshot["kind"] = json!("expression");
                                snapshot
                            })
                        })
                        .collect();
                    json!({"expression":format!("{} | {}",types[0]["expression"].as_str().unwrap_or("object"),types[1]["expression"].as_str().unwrap_or("object")),"mode":"annotation","snapshot":{"kind":"union","elements":elements}})
                };
                facts.insert(member.clone(), value);
            }
            scope.facts.insert(*identity, Arc::new(facts));
        }
    }

    fn member_type(&self, owner: &str, member: &str, class_access: bool) -> Option<Value> {
        let existing = self
            .runtime
            .member_type(self.source, owner, member, class_access);
        if existing.is_some() || self.runtime.project.is_some() {
            return existing;
        }
        // The retained single-file command stores declarations in Analysis
        // instead of attaching a project. Those declarations still count as
        // existing members for both the request and fill-on-miss dispatch.
        let (nominal, arguments) = crate::runtime::generic_parts(owner);
        let nominal = if matches!(nominal, "type" | "builtins.type") {
            arguments.first().copied().unwrap_or(nominal)
        } else {
            nominal
        };
        let (nominal, _) = crate::runtime::generic_parts(nominal);
        let nominal = nominal
            .strip_prefix(&format!("{}.", self.source.module))
            .unwrap_or(nominal);
        self.analysis
            .classes
            .get(nominal)
            .and_then(|class| class.fields.get(member))
            .map(typ)
    }

    fn identity(&mut self, scope: &mut Scope, expr: &Expr) -> Option<Object> {
        if let Some(identity) = scope.identity(expr) {
            return Some(identity);
        }
        let known = if let Expr::Attribute(attribute) = expr {
            self.identity(scope, &attribute.value).and_then(|identity| {
                let owner = &scope.types[&identity];
                let (nominal, _) = crate::runtime::generic_parts(owner);
                self.member_type(
                    owner,
                    attribute.attr.as_str(),
                    matches!(nominal, "type" | "builtins.type"),
                )
            })
        } else if matches!(expr, Expr::Name(name) if scope.shadowed.contains(name.id.as_str()))
            || matches!(expr, Expr::Call(_) if !self.analysis.calls.contains_key(&span(expr)))
        {
            None
        } else {
            self.runtime.expression_type(self.source, expr)
        };
        let inferred = known
            .as_ref()
            .and_then(Runtime::receiver_identity)
            .or_else(|| {
                self.oracle
                    .and_then(|oracle| oracle.get(&expression_key(span(expr))))
                    .map(|expression| self.runtime.canonical_name(self.source, expression))
            })?;
        if !self.runtime.nominal_receiver(self.source, &inferred) {
            return None;
        }
        let identity = self.fresh(scope, &inferred, false);
        scope.objects.insert(span(expr), identity);
        if let Expr::Name(name) = expr {
            scope.names.insert(name.id.to_string(), identity);
        }
        Some(identity)
    }

    fn call(&mut self, scope: &mut Scope, call: &ruff_python_ast::ExprCall) -> Result<()> {
        if let Some(value) = self.analysis.annotated_casts.get(&span(call)).cloned() {
            for argument in &call.arguments.args {
                self.expr(scope, argument)?;
            }
            // Annotated is a static view, not a new object. Keep the runtime
            // identity of cast's value separately from its protocol metadata.
            if let Some(argument) = call.arguments.args.get(1)
                && let Some(identity) = self.identity(scope, argument)
            {
                scope.objects.insert(span(call), identity);
            }
            scope.protocol_values.insert(span(call), value);
            return Ok(());
        }
        if let Expr::Name(name) = call.func.as_ref()
            && self.analysis.intrinsics.contains(name.id.as_str())
        {
            if let Some(first) = call.arguments.args.first() {
                self.expr(scope, first)?;
            }
            return Ok(());
        }

        self.expr(scope, &call.func)?;
        // Python binds the method before evaluating its arguments. A walrus
        // expression or nested call may subsequently rebind the receiver name.
        let bound_receiver = if let Expr::Attribute(attribute) = call.func.as_ref() {
            self.identity(scope, &attribute.value)
        } else {
            None
        };
        let mut argument_types = BTreeMap::new();
        for argument in &call.arguments.args {
            self.expr(scope, argument)?;
            if let Some(value) = scope.protocol_type(argument) {
                argument_types.insert(span(argument), value.clone());
            }
        }
        for keyword in &call.arguments.keywords {
            self.expr(scope, &keyword.value)?;
            if let Some(value) = scope.protocol_type(&keyword.value) {
                argument_types.insert(span(&keyword.value), value.clone());
            }
        }
        if self.analysis.unbound_calls.contains(&span(call)) {
            scope.invalidate();
            return Ok(());
        }

        // A display such as `Record` cannot distinguish two imported models
        // with that name. Keep the precise type already established for a
        // parameter, local alias, or returned object alongside its display.
        let nominal_arguments: BTreeMap<_, _> = call
            .arguments
            .args
            .iter()
            .chain(call.arguments.keywords.iter().map(|keyword| &keyword.value))
            .filter_map(|argument| {
                let identity = scope.identity(argument)?;
                let owner = scope.types.get(&identity)?;
                Some((span(argument), self.runtime.qualified_type_expr(owner)))
            })
            .collect();
        for (span, value) in nominal_arguments {
            argument_types.entry(span).or_insert(value);
        }
        if let Some(site) = self.analysis.calls.get_mut(&span(call)) {
            for argument in &mut site.arguments {
                argument.inferred_type = argument_types.get(&argument.span).cloned();
            }
        }

        let mut site = self.analysis.calls.get(&span(call)).cloned();
        let constructor = site.as_ref().is_some_and(|site| {
            self.runtime
                .project
                .as_ref()
                .is_some_and(|project| project.class_summary(&site.callee).is_some())
        });
        let receiver = if constructor {
            None
        } else if let Expr::Attribute(attribute) = call.func.as_ref() {
            bound_receiver.or_else(|| {
                let inferred = site.as_ref()?.receiver_type.clone().or_else(|| {
                    self.oracle
                        .and_then(|oracle| oracle.get(&site.as_ref()?.receiver_key()))
                        .map(|expression| self.runtime.canonical_name(self.source, expression))
                })?;
                if !self.runtime.nominal_receiver(self.source, &inferred) {
                    return None;
                }
                let identity = self.fresh(scope, &inferred, false);
                scope
                    .objects
                    .insert(span(attribute.value.as_ref()), identity);
                Some(identity)
            })
        } else {
            None
        };
        if let (Some(receiver), Expr::Attribute(attribute)) = (receiver, call.func.as_ref()) {
            let owner = &scope.types[&receiver];
            if let Some(existing) = self.analysis.calls.get_mut(&span(call)) {
                existing.flow_receiver_type = Some(owner.clone());
            }
            let callee = if self.runtime.project.is_some() {
                Some(self.runtime.method_callee(owner, attribute.attr.as_str()))
            } else {
                self.analysis
                    .classes
                    .get(owner)
                    .filter(|class| class.methods.contains(attribute.attr.as_str()))
                    .map(|_| format!("{}.{}.{}", self.source.module, owner, attribute.attr))
            };
            if let Some(callee) = callee {
                if let Some(site) = &mut site {
                    site.callee = callee;
                } else {
                    site = Some(call_site(call, callee));
                }
            }
        }

        let result_type = self
            .raw_returns
            .get(&span(call))
            .and_then(Runtime::receiver_identity)
            .or_else(|| self.returns.get(&span(call)).cloned())
            .or_else(|| {
                site.as_ref().and_then(|site| {
                    self.oracle
                        .and_then(|oracle| oracle.get(&site.return_key()))
                        .map(|expression| self.runtime.canonical_name(self.source, expression))
                })
            });
        let patch = if let Some(site) = &site {
            let owner = receiver
                .and_then(|id| scope.types.get(&id))
                .map(String::as_str);
            call_request(
                self.runtime,
                self.source,
                "adjust-call-state",
                site,
                self.oracle,
                owner,
            )?
        } else {
            json!({"kind":"no-change"})
        };

        for diagnostic in patch["diagnostics"].as_array().into_iter().flatten() {
            self.result.diagnostics.push(json!({
                "diagnostic":diagnostic,
                "source":self.source.location(span(call)),
            }));
        }
        let receiver_name_still_bound = match (receiver, call.func.as_ref()) {
            (Some(receiver), Expr::Attribute(attribute)) => {
                matches!(attribute.value.as_ref(), Expr::Name(name) if scope.names.get(name.id.as_str()) == Some(&receiver))
            }
            _ => false,
        };

        if patch["kind"] != "call-state-patch"
            || !patch["preserves-other-objects"].as_bool().unwrap_or(false)
        {
            scope.invalidate();
        } else if let Some(receiver) = receiver {
            scope.facts.retain(|identity, _| {
                *identity != receiver
                    && scope.fresh.contains(&receiver)
                    && scope.fresh.contains(identity)
            });
        }
        if patch["kind"] == "call-state-patch"
            && let Some(receiver) = receiver
        {
            extend_members(
                scope.facts.entry(receiver).or_default(),
                &patch["receiver-members"],
            );
            // Invalidation at module scope must not lose the receiver name
            // whose successful postcondition this very call just established.
            if let Expr::Attribute(attribute) = call.func.as_ref()
                && let Expr::Name(name) = attribute.value.as_ref()
                && receiver_name_still_bound
            {
                scope.names.insert(name.id.to_string(), receiver);
            }
        }
        let is_fresh = patch["fresh-result"].as_bool().unwrap_or(false);
        let owner = if self.runtime.project.is_some() {
            let constructor = site.as_ref().and_then(|site| {
                self.runtime
                    .project
                    .as_ref()?
                    .default_constructor_result(&site.callee)
            });
            // A normal constructor's qualified source identity is more precise
            // than an abbreviated oracle display. A plugin return replacement
            // still takes precedence, and custom construction paths abstain.
            self.raw_returns
                .get(&span(call))
                .and_then(Runtime::receiver_identity)
                .or(constructor)
                .or(result_type)
                .filter(|value| !matches!(value.as_str(), "Unknown" | "Any" | "@Todo" | "None"))
        } else if is_fresh {
            site.as_ref()
                .and_then(|site| site.callee.rsplit('.').next())
                .map(str::to_owned)
        } else {
            None
        };
        if let Some(owner) = owner {
            if is_fresh
                && self.runtime.project.is_none()
                && !self.analysis.classes.contains_key(&owner)
            {
                bail!("Rust companion fresh-result needs a known constructor: {owner}");
            }
            let identity = self.fresh(scope, &owner, is_fresh);
            scope.objects.insert(span(call), identity);
            if is_fresh {
                extend_members(
                    scope.facts.entry(identity).or_default(),
                    &patch["result-members"],
                );
            }
        } else if is_fresh
            && self.oracle.is_some_and(|oracle| {
                site.as_ref()
                    .is_some_and(|site| oracle.contains_key(&site.return_key()))
            })
        {
            bail!(
                "fresh-result hook needs an inferred result type at {:?}",
                span(call)
            );
        }
        Ok(())
    }

    /// Only truthiness established by syntax or the checker can remove a
    /// control-flow edge. Mutable containers and user-defined __bool__ remain
    /// unknown, even when they look constant to this projection.
    fn literal_truthiness(expression: &Expr) -> Option<bool> {
        match expression {
            Expr::BooleanLiteral(value) => Some(value.value),
            Expr::NoneLiteral(_) => Some(false),
            Expr::StringLiteral(value) => Some(!value.value.to_str().is_empty()),
            Expr::BytesLiteral(value) => Some(!value.value.is_empty()),
            Expr::NumberLiteral(value) => match &value.value {
                ruff_python_ast::Number::Int(value) => Some(value.as_i64() != Some(0)),
                _ => None,
            },
            Expr::Tuple(value) if value.elts.is_empty() => Some(false),
            Expr::Tuple(value)
                if value
                    .elts
                    .iter()
                    .any(|value| !matches!(value, Expr::Starred(_))) =>
            {
                Some(true)
            }
            Expr::UnaryOp(value) if value.op == ruff_python_ast::UnaryOp::Not => {
                Self::literal_truthiness(&value.operand).map(|truthy| !truthy)
            }
            _ => None,
        }
    }

    fn type_truthiness(inferred: &str) -> Option<bool> {
        if inferred == "None" {
            return Some(false);
        }
        let (base, arguments) = crate::runtime::generic_parts(inferred);
        if !matches!(
            base,
            "Literal" | "typing.Literal" | "typing_extensions.Literal"
        ) {
            return None;
        }
        let mut truths = arguments.iter().map(|literal| {
            let parsed = ruff_python_parser::parse_expression(literal).ok()?;
            Self::literal_truthiness(parsed.expr())
        });
        let first = truths.next()??;
        truths.all(|next| next == Some(first)).then_some(first)
    }

    fn truthiness(&self, scope: &Scope, expression: &Expr) -> Option<bool> {
        Self::literal_truthiness(expression).or_else(|| {
            if let Expr::Name(name) = expression
                && let Some(truthy) = scope.truths.get(name.id.as_str())
            {
                return Some(*truthy);
            }
            if let Expr::UnaryOp(value) = expression
                && value.op == ruff_python_ast::UnaryOp::Not
            {
                return self.truthiness(scope, &value.operand).map(|truthy| !truthy);
            }
            let inferred = self.oracle.and_then(|oracle| {
                oracle.get(&expression_key(span(expression))).or_else(|| {
                    self.analysis
                        .calls
                        .get(&span(expression))
                        .and_then(|call| oracle.get(&call.return_key()))
                })
            })?;
            Self::type_truthiness(inferred)
        })
    }

    fn boolean_values(
        &mut self,
        scope: &mut Scope,
        operator: ruff_python_ast::BoolOp,
        values: &[Expr],
    ) -> Result<()> {
        let Some((first, rest)) = values.split_first() else {
            return Ok(());
        };
        self.expr(scope, first)?;
        if rest.is_empty() {
            return Ok(());
        }
        if let Some(truthy) = self.truthiness(scope, first) {
            if truthy == (operator == ruff_python_ast::BoolOp::And) {
                self.boolean_values(scope, operator, rest)?;
            }
        } else {
            let skipped = scope.clone();
            self.boolean_values(scope, operator, rest)?;
            let executed = scope.clone();
            self.join(scope, skipped, executed);
        }
        Ok(())
    }

    fn expr(&mut self, scope: &mut Scope, expr: &Expr) -> Result<()> {
        match expr {
            Expr::Call(call) => self.call(scope, call)?,
            Expr::Attribute(attribute) => {
                self.expr(scope, &attribute.value)?;
                if attribute.ctx != ExprContext::Load {
                    return Ok(());
                }
                let Some(identity) = self.identity(scope, &attribute.value) else {
                    return Ok(());
                };
                let member = attribute.attr.as_str();
                let owner = scope.types[&identity].clone();
                let class_access = matches!(
                    crate::runtime::generic_parts(&owner).0,
                    "type" | "builtins.type"
                );
                let expression = scope
                    .facts
                    .get(&identity)
                    .and_then(|facts| facts.get(member))
                    .cloned();
                if expression.is_none() {
                    let existing = self.member_type(&owner, member, class_access);
                    // ty invokes dynamic class-member hooks only after normal
                    // class lookup misses, regardless of the response mode.
                    if class_access && existing.is_some() {
                        return Ok(());
                    }
                    let response = self.runtime.resolve_member(
                        self.source,
                        &owner,
                        member,
                        class_access,
                        existing.as_ref(),
                    )?;
                    if response["kind"] == "member-patch" && response["name"] == member {
                        if response["mode"].as_str().unwrap_or("fill-on-miss") == "fill-on-miss"
                            && existing.is_some()
                        {
                            return Ok(());
                        }
                        let helper =
                            format!("{}member_{}", self.source.prefix, span(attribute).start);
                        self.runtime.set_projected_receiver(
                            format!("{}.{}", self.source.module, helper),
                            owner.clone(),
                        );
                        let previous_self = self
                            .types
                            .set_self_context(Some(self.runtime.qualified_type_expr(&owner)));
                        let resolved = self
                            .types
                            .member_access(&response["access"], &format!("{helper}_callable"))?;
                        self.types.set_self_context(previous_self);
                        let typing = self.types.typing_alias();
                        // A receiver view also handles imported classes: adding
                        // only a cast around the result would retain ty's
                        // unresolved-attribute diagnostic for the original read.
                        self.types.definitions.push_str(&format!(
                            "class {helper}({typing}.Protocol):\n    {member}: {resolved}\n"
                        ));
                        self.result.edits.wrap(
                            span(attribute.value.as_ref()),
                            format!("{}({helper}, ", self.source.cast_name()),
                            ")".into(),
                            None,
                        );
                        self.result.edits.annotate_type(span(attribute), resolved);
                    }
                }
                if let Some(expression) = expression {
                    let previous_self = self
                        .types
                        .set_self_context(Some(self.runtime.qualified_type_expr(&owner)));
                    let expression = self.types.render(&expression)?;
                    self.types.set_self_context(previous_self);
                    self.result.edits.wrap_with_type(
                        span(attribute),
                        format!("{}({}, ", self.source.cast_name(), expression),
                        ")".into(),
                        None,
                        expression.clone(),
                    );
                }
            }
            Expr::Lambda(lambda) => {
                // Deferred code receives only the nominal identities of captured
                // variables. Its body cannot mutate this expression's scope.
                let mut sub = Scope {
                    shadowed: scope.shadowed.clone(),
                    ..Scope::default()
                };
                for (name, identity) in &scope.names {
                    let new = self.fresh(&mut sub, &scope.types[identity], false);
                    sub.names.insert(name.clone(), new);
                }
                if let Some(parameters) = &lambda.parameters {
                    for parameter in parameters.iter() {
                        sub.names.remove(parameter.name().as_str());
                        sub.shadowed.insert(parameter.name().to_string());
                    }
                }
                self.expr(&mut sub, &lambda.body)?;
            }
            Expr::BoolOp(boolean) => {
                self.boolean_values(scope, boolean.op, &boolean.values)?;
            }
            Expr::If(branch) => {
                self.expr(scope, &branch.test)?;
                if let Some(truthy) = self.truthiness(scope, &branch.test) {
                    self.expr(scope, if truthy { &branch.body } else { &branch.orelse })?;
                    return Ok(());
                }
                let initial = scope.clone();
                self.expr(scope, &branch.body)?;
                let left = scope.clone();
                *scope = initial;
                self.expr(scope, &branch.orelse)?;
                let right = scope.clone();
                self.join(scope, left, right);
            }
            Expr::BinOp(operator) => {
                self.expr(scope, &operator.left)?;
                self.expr(scope, &operator.right)?;
                if operator.op != ruff_python_ast::Operator::BitOr
                    || !self.builtin_union_operand(scope, &operator.left)
                    || !self.builtin_union_operand(scope, &operator.right)
                {
                    scope.invalidate();
                }
            }
            Expr::UnaryOp(operator) => {
                self.expr(scope, &operator.operand)?;
                scope.invalidate();
            }
            Expr::Compare(compare) => {
                if let Some(first) = compare.operands.first() {
                    self.expr(scope, first)?;
                }
                for (operator, operand) in compare.ops.iter().zip(compare.operands.iter().skip(1)) {
                    self.expr(scope, operand)?;
                    if !matches!(
                        operator,
                        ruff_python_ast::CmpOp::Is | ruff_python_ast::CmpOp::IsNot
                    ) {
                        scope.invalidate();
                    }
                }
            }
            Expr::Subscript(subscript) => {
                self.expr(scope, &subscript.value)?;
                self.expr(scope, &subscript.slice)?;
            }
            Expr::Starred(starred) => self.expr(scope, &starred.value)?,
            Expr::List(list) => {
                for value in &list.elts {
                    self.expr(scope, value)?;
                }
            }
            Expr::Tuple(tuple) => {
                for value in &tuple.elts {
                    self.expr(scope, value)?;
                }
            }
            Expr::Set(set) => {
                for value in &set.elts {
                    self.expr(scope, value)?;
                }
            }
            Expr::Dict(dict) => {
                for item in &dict.items {
                    if let Some(key) = &item.key {
                        self.expr(scope, key)?;
                    }
                    self.expr(scope, &item.value)?;
                }
            }
            Expr::Slice(slice) => {
                for value in [&slice.lower, &slice.upper, &slice.step]
                    .into_iter()
                    .flatten()
                {
                    self.expr(scope, value)?;
                }
            }
            Expr::Name(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_) => {}
            Expr::Await(await_) if self.runtime.project.is_some() => {
                self.expr(scope, &await_.value)?;
                // Other tasks may mutate captured objects while suspended.
                scope.invalidate();
                if let Some(awaitable) = scope.identity(&await_.value) {
                    let owner = &scope.types[&awaitable];
                    let (origin, arguments) = crate::runtime::generic_parts(owner);
                    let result = match origin {
                        "Coroutine" | "typing.Coroutine" | "collections.abc.Coroutine" => {
                            arguments.get(2)
                        }
                        "Awaitable" | "typing.Awaitable" | "collections.abc.Awaitable" => {
                            arguments.first()
                        }
                        _ => None,
                    }
                    .map(|result| (*result).to_owned());
                    if let Some(result) = result
                        && self.runtime.nominal_receiver(self.source, &result)
                    {
                        // The awaited value has its own identity. It need not
                        // be fresh, and no pre-suspension member facts transfer.
                        let identity = self.fresh(scope, &result, false);
                        scope.objects.insert(span(await_), identity);
                    }
                }
            }
            Expr::Named(named) if self.runtime.project.is_some() => {
                self.expr(scope, &named.value)?;
                let identity = self.identity(scope, &named.value);
                self.assignment(scope, &named.target, Some(&named.value), identity)?;
            }
            Expr::ListComp(comp) if self.runtime.project.is_some() => {
                self.comprehension(scope, &comp.generators, &[&comp.elt])?;
            }
            Expr::SetComp(comp) if self.runtime.project.is_some() => {
                self.comprehension(scope, &comp.generators, &[&comp.elt])?;
            }
            Expr::DictComp(comp) if self.runtime.project.is_some() => {
                let mut elements = Vec::new();
                if let Some(key) = &comp.key {
                    elements.push(key.as_ref());
                }
                elements.push(comp.value.as_ref());
                self.comprehension(scope, &comp.generators, &elements)?;
            }
            Expr::Generator(comp) if self.runtime.project.is_some() => {
                self.comprehension(scope, &comp.generators, &[&comp.elt])?;
            }
            Expr::Yield(yield_) if self.runtime.project.is_some() => {
                if let Some(value) = &yield_.value {
                    self.expr(scope, value)?;
                }
                scope.invalidate();
            }
            Expr::YieldFrom(yield_) if self.runtime.project.is_some() => {
                self.expr(scope, &yield_.value)?;
                scope.invalidate();
            }
            _ if self.runtime.project.is_some() => {
                // Formatting and other ordinary expression containers retain
                // the fork's state semantics: only their child operations add
                // invalidations. Yield/await have dedicated handling above.
                struct Children<'a>(Vec<&'a Expr>);
                impl<'a> ruff_python_ast::visitor::Visitor<'a> for Children<'a> {
                    fn visit_expr(&mut self, expression: &'a Expr) {
                        self.0.push(expression);
                    }
                }
                let mut children = Children(Vec::new());
                ruff_python_ast::visitor::walk_expr(&mut children, expr);
                for child in children.0 {
                    self.expr(scope, child)?;
                }
            }
            _ => bail!(
                "Rust companion unsupported expression flow at {:?}",
                span(expr)
            ),
        }
        Ok(())
    }

    fn builtin_union_operand(&self, scope: &Scope, expr: &Expr) -> bool {
        if let Expr::BinOp(binary) = expr {
            return binary.op == ruff_python_ast::Operator::BitOr
                && self.builtin_union_operand(scope, &binary.left)
                && self.builtin_union_operand(scope, &binary.right);
        }
        if matches!(expr, Expr::NoneLiteral(_)) {
            return true;
        }
        if !matches!(expr, Expr::Name(_) | Expr::Attribute(_)) {
            return false;
        }
        let raw = self.source.slice(span(expr));
        if scope
            .shadowed
            .contains(raw.split('.').next().unwrap_or(raw))
        {
            return false;
        }
        let name = self.runtime.canonical_name(self.source, raw);
        let name = name.strip_prefix("builtins.").unwrap_or(&name);
        !self
            .analysis
            .classes
            .contains_key(&format!("{}.{}", self.source.module, name))
            && matches!(
                name,
                "bool"
                    | "int"
                    | "float"
                    | "complex"
                    | "str"
                    | "bytes"
                    | "bytearray"
                    | "list"
                    | "tuple"
                    | "dict"
                    | "set"
                    | "frozenset"
                    | "object"
                    | "type"
                    | "memoryview"
                    | "range"
                    | "slice"
                    | "property"
                    | "staticmethod"
                    | "classmethod"
                    | "BaseException"
                    | "Exception"
            )
    }

    fn comprehension(
        &mut self,
        scope: &mut Scope,
        generators: &[ruff_python_ast::Comprehension],
        elements: &[&Expr],
    ) -> Result<()> {
        let Some((first, rest)) = generators.split_first() else {
            return Ok(());
        };
        self.expr(scope, &first.iter)?;
        // Like functions and class bodies, the checker models a comprehension
        // in its own object-state scope. Target bindings never rebind an outer
        // variable with the same spelling, even when Python inlines the scope.
        let mut inner = Scope {
            shadowed: scope.shadowed.clone(),
            ..Scope::default()
        };
        for (name, identity) in &scope.names {
            let captured = self.fresh(&mut inner, &scope.types[identity], false);
            inner.names.insert(name.clone(), captured);
        }
        inner.protocol_names = scope.protocol_names.clone();
        self.assignment(&mut inner, &first.target, None, None)?;
        for condition in &first.ifs {
            self.expr(&mut inner, condition)?;
        }
        for generator in rest {
            self.expr(&mut inner, &generator.iter)?;
            self.assignment(&mut inner, &generator.target, None, None)?;
            for condition in &generator.ifs {
                self.expr(&mut inner, condition)?;
            }
        }
        for element in elements {
            self.expr(&mut inner, element)?;
        }
        Ok(())
    }
}

fn extend_members(facts: &mut Facts, members: &Value) {
    if let Some(members) = members.as_object() {
        for (name, value) in members {
            Arc::make_mut(facts).insert(name.clone(), value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    struct Fixture {
        directory: PathBuf,
        source: Source,
        suite: Vec<Stmt>,
        runtime: Runtime,
        analysis: Analysis,
    }
    impl Fixture {
        fn new(reassign: bool) -> Self {
            let manifest = json!({
                "id":"native.flow-contract", "name":"Native flow contract", "version":"0.0.0",
                "protocol-version":{"major":0,"minor":7},
                "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
                "runtime":{"kind":"monty","artifact":"plugin.py"},
                "capabilities":{"call-state":true,"call-return":true},
                "claims":{"methods":[
                    {"kind":"exact","class-qualified-name":"demo.Factory","method-name":"get"},
                    {"kind":"exact","class-qualified-name":"demo.Item","method-name":"touch"}
                ]}
            });
            let plugin = "@on_call_state\ndef state(request):\n    if request['callee']['expression'] == 'demo.Factory.get':\n        return call_state_patch(result_members={'id': type_annotation('int')}, fresh_result=True)\n    return None\n";
            let reassignment = if reassign {
                "    alias = object()\n"
            } else {
                ""
            };
            let text = format!(
                "class Item:\n    id: int | None\n    def touch(self) -> None: pass\nclass Factory:\n    def get(self) -> Item: raise NotImplementedError\ndef example(factory: Factory) -> None:\n    loaded = factory.get()\n    alias = loaded\n    before = alias.id\n{reassignment}    alias.touch()\n"
            );
            Self::from_parts(text, manifest, plugin)
        }

        fn from_parts(text: String, manifest: Value, plugin: &str) -> Self {
            static ID: AtomicUsize = AtomicUsize::new(0);
            let directory = std::env::temp_dir().join(format!(
                "native-flow-contract-{}-{}",
                std::process::id(),
                ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("demo.py");
            std::fs::write(&path, &text).unwrap();
            std::fs::write(directory.join("plugin.py"), plugin).unwrap();
            std::fs::write(
                directory.join("plugin.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let project = crate::project::Project::load(
                &directory,
                std::slice::from_ref(&path),
                &manifest,
                json!({}),
            )
            .unwrap();
            let mut runtime =
                Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py")).unwrap();
            runtime.attach_project(Arc::new(project), json!({}));
            let source = Source::new(text, path.to_string_lossy().into_owned(), "demo".into());
            let suite = ruff_python_parser::parse_module(&source.text)
                .unwrap()
                .syntax()
                .body
                .to_vec();
            let analysis = crate::lower::analyze(&source, &suite, &mut runtime).unwrap();
            Self {
                directory,
                source,
                suite,
                runtime,
                analysis,
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    #[test]
    fn failed_state_hooks_report_and_discard_receiver_alias_and_other_facts() {
        let manifest = json!({
            "id":"native.failed-state","name":"Failed state","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},
            "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},
            "capabilities":{"call-state":true},
            "claims":{"constructors":[{"kind":"exact","qualified-name":"demo.Item"}],"methods":[
                {"kind":"exact","class-qualified-name":"demo.Item","method-name":"touch"},
                {"kind":"exact","class-qualified-name":"demo.Item","method-name":"restore"}
            ]}
        });
        let text = "class Item:\n    id: int | None\n    def touch(self) -> None: pass\n    def restore(self) -> None: pass\ndef example() -> None:\n    first = Item()\n    second = Item()\n    alias = first\n    before_alias = alias.id\n    before_other = second.id\n    alias.touch()\n    after_first = first.id\n    after_alias = alias.id\n    after_other = second.id\n    first.restore()\n    restored = first.id\n";
        for failure in [
            "return {'kind': 'invalid-response-kind'}",
            "raise ValueError('caught by the SDK into a protocol Error')",
        ] {
            let plugin = format!(
                "@on_call_state\ndef state(request):\n    name = request['callee']['expression']\n    if name == 'demo.Item':\n        return call_state_patch(result_members={{'id':type_annotation('int')}}, fresh_result=True, preserves_other_objects=True)\n    if name.endswith('.touch'):\n        {failure}\n    return call_state_patch(receiver_members={{'id':type_annotation('int')}}, preserves_other_objects=True)\n"
            );
            let mut fixture = Fixture::from_parts(text.into(), manifest.clone(), &plugin);
            let flow = project(
                &fixture.source,
                &fixture.suite,
                &mut fixture.analysis,
                &mut fixture.runtime,
            )
            .unwrap();
            let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
            for statement in [
                "before_alias = alias.id",
                "before_other = second.id",
                "restored = first.id",
            ] {
                assert!(
                    !rendered.text.contains(statement),
                    "successful facts must project: {}",
                    rendered.text
                );
            }
            for statement in [
                "after_first = first.id",
                "after_alias = alias.id",
                "after_other = second.id",
            ] {
                assert!(
                    rendered.text.contains(statement),
                    "failed state retained stale facts: {}",
                    rendered.text
                );
            }
            let is_host_failure = failure.starts_with("return");
            assert_eq!(flow.diagnostics.len(), usize::from(is_host_failure));
            if is_host_failure {
                let diagnostic = &flow.diagnostics[0];
                assert_eq!(diagnostic["diagnostic"]["severity"], "warning");
                assert_eq!(
                    diagnostic["diagnostic"]["runtime-error"]["plugin-id"],
                    "native.failed-state"
                );
                let start = text.find("alias.touch()").unwrap();
                assert_eq!(
                    diagnostic["source"],
                    fixture.source.location(Span {
                        start,
                        end: start + "alias.touch()".len()
                    })
                );
            }
        }
    }

    fn alias_fixture(body: &str) -> Fixture {
        let manifest = json!({
            "id":"native.alias-state","name":"Alias state","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},
            "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-state":true},
            "claims":{"constructors":[{"kind":"exact","qualified-name":"demo.Item"}],"methods":[
                {"kind":"exact","class-qualified-name":"demo.Item","method-name":"set_key"},
                {"kind":"exact","class-qualified-name":"demo.Item","method-name":"clear_key"}
            ]}
        });
        Fixture::from_parts(
            format!(
                "class Item:\n    key: int | None\n    def set_key(self, *args: object) -> None: pass\n    def clear_key(self) -> None: pass\ndef unknown() -> Item: raise NotImplementedError\n{body}"
            ),
            manifest,
            "@on_call_state\ndef state(request):\n    name = request['callee']['expression']\n    if name == 'demo.Item':\n        return call_state_patch(result_members={'key':type_annotation('None')}, fresh_result=True, preserves_other_objects=True)\n    annotation = 'int' if name.endswith('.set_key') else 'None'\n    return call_state_patch(receiver_members={'key':type_annotation(annotation)}, preserves_other_objects=True)\n",
        )
    }

    #[test]
    fn branch_allocations_keep_shared_aliases_and_member_facts() {
        let mut fixture = alias_fixture(
            "def example(condition: bool) -> None:\n    if condition:\n        record = Item()\n        alias = record\n        record.set_key()\n    else:\n        record = Item()\n        alias = record\n        record.set_key()\n    before = record.key\n    alias.clear_key()\n    after = record.key\n",
        );
        let flow = project(
            &fixture.source,
            &fixture.suite,
            &mut fixture.analysis,
            &mut fixture.runtime,
        )
        .unwrap();
        let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
        assert!(
            !rendered.text.contains("before = record.key"),
            "{}",
            rendered.text
        );
        assert!(
            rendered.text.contains("None, record.key)"),
            "{}",
            rendered.text
        );
    }

    #[test]
    fn call_arguments_rebind_names_after_the_receiver_was_bound() {
        let mut fixture = alias_fixture(
            "record = Item()\nalias = record\nother = Item()\nrecord.set_key(record := other)\nafter_alias = alias.key\nafter_record = record.key\n",
        );
        let flow = project(
            &fixture.source,
            &fixture.suite,
            &mut fixture.analysis,
            &mut fixture.runtime,
        )
        .unwrap();
        let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
        assert!(
            rendered.text.contains(".int, alias.key)"),
            "{}",
            rendered.text
        );
        assert!(
            rendered.text.contains("None, record.key)"),
            "{}",
            rendered.text
        );
    }

    #[test]
    fn unknown_call_results_keep_multi_assignment_identity() {
        let mut fixture = alias_fixture(
            "first = second = unknown()\nfirst.set_key()\nsecond.clear_key()\nafter = first.key\n",
        );
        let start = fixture.source.text.find("unknown()\nfirst").unwrap();
        let range = Span {
            start,
            end: start + "unknown()".len(),
        };
        assert_eq!(
            fixture.analysis.expression_probes.get(&range),
            Some(&expression_key(range))
        );
        let oracle = Oracle::from([(expression_key(range), "demo.Item".into())]);
        let flow = project_with_oracle(
            &fixture.source,
            &fixture.suite,
            &mut fixture.analysis,
            &mut fixture.runtime,
            Some(&oracle),
            &ReturnPatches::new(),
        )
        .unwrap();
        let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
        assert!(
            rendered.text.contains("None, first.key)"),
            "{}",
            rendered.text
        );
    }

    #[test]
    fn nested_nonlocal_writes_invalidate_only_the_referenced_outer_cells() {
        let parsed = ruff_python_parser::parse_module("def replace():\n    nonlocal record\n    record = None\n    local = None\n    def inner():\n        nonlocal local, shared\n        local = shared = None\n").unwrap();
        let Stmt::FunctionDef(function) = &parsed.syntax().body[0] else {
            panic!("function")
        };
        assert_eq!(
            captured_nonlocals(
                function,
                &BTreeSet::from(["record".into(), "local".into(), "shared".into()])
            ),
            BTreeSet::from(["record".into(), "shared".into()])
        );
    }

    #[test]
    fn legacy_member_hooks_preserve_existing_declarations_unless_replaced() {
        for mode in ["fill-on-miss", "replace-existing"] {
            let manifest = json!({
                "id":"native.member-contract","name":"Member contract","version":"0.0.0",
                "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
                "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"instance-member":true},
                "claims":{"attributes":[{"kind":"exact","owner-qualified-name":"demo.Resource","attribute-name":"token","scope":"instance"}]}
            });
            let plugin = format!(
                "@on_instance_member\ndef token(request):\n    assert request['existing-member']['access']['type-expr']['expression'] == 'int'\n    return member_response(member('token', type_annotation('bytes'), mode='{mode}'))\n"
            );
            let mut fixture = Fixture::from_parts(
                "class Resource:\n    token: int\ndef example(resource: Resource) -> bytes:\n    return resource.token\n".into(),
                manifest,
                &plugin,
            );
            // The retained single-file research command has no project index.
            fixture.runtime = Runtime::new(
                &fixture.directory.join("plugin.json"),
                &fixture.directory.join("plugin.py"),
            )
            .unwrap();
            fixture.analysis = crate::static_project::analyze(
                &fixture.source,
                &fixture.suite,
                &mut fixture.runtime,
            )
            .unwrap();
            let flow = project(
                &fixture.source,
                &fixture.suite,
                &mut fixture.analysis,
                &mut fixture.runtime,
            )
            .unwrap();
            assert_eq!(fixture.runtime.executions, 1, "member hook was skipped");
            let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
            assert_eq!(
                rendered.text.contains("    return resource.token\n"),
                mode == "fill-on-miss",
                "incorrect {mode} behavior: {}",
                rendered.text
            );
        }
    }

    #[test]
    fn fresh_result_defers_until_the_first_oracle_response() {
        let mut fixture = Fixture::new(false);
        let flow = project_with_return_types(
            &fixture.source,
            &fixture.suite,
            &mut fixture.analysis,
            &mut fixture.runtime,
            Some(&Oracle::new()),
            &ReturnPatches::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        let projected = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
        assert_eq!(projected.text, fixture.source.text);
        assert!(
            fixture.runtime.executions > 0,
            "state hook must actually execute in Monty"
        );
    }

    #[test]
    fn raw_return_identity_survives_aliasing_and_is_cleared_by_reassignment() {
        for reassign in [false, true] {
            let mut fixture = Fixture::new(reassign);
            let site = fixture
                .analysis
                .calls
                .values()
                .find(|call| call.callee.ends_with(".get"))
                .unwrap()
                .clone();
            let oracle = Oracle::from([(site.return_key(), "_generated_display_alias".into())]);
            let rendered = ReturnPatches::from([(site.span, "_generated_display_alias".into())]);
            let raw = BTreeMap::from([(site.span, typ("demo.Item"))]);
            let flow = project_with_return_types(
                &fixture.source,
                &fixture.suite,
                &mut fixture.analysis,
                &mut fixture.runtime,
                Some(&oracle),
                &rendered,
                &raw,
            )
            .unwrap();
            let projected = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
            assert!(
                projected.text.contains(".int, alias.id)"),
                "{}",
                projected.text
            );
            assert!(flow.type_prelude.contains("import builtins as "));
            let call = fixture
                .analysis
                .calls
                .values()
                .find(|call| call.callee.ends_with(".touch"))
                .unwrap();
            assert_eq!(
                call.flow_receiver_type.as_deref(),
                if reassign { None } else { Some("demo.Item") }
            );
        }
    }

    #[test]
    fn chained_self_results_keep_the_receiver_bound() {
        let text = "from typing import Self\nclass Selection:\n    def select(self) -> Self: return self\n    def again(self) -> Self: return self\n    def example(self) -> Self:\n        return self.select().again()\n";
        let manifest = json!({
            "id":"native.self-contract","name":"Self contract","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-return":true},
            "claims":{"methods":[
                {"kind":"exact","class-qualified-name":"demo.Selection","method-name":"select"},
                {"kind":"exact","class-qualified-name":"demo.Selection","method-name":"again"}
            ]}
        });
        let plugin = "@on_call_return\ndef returns(request):\n    return call_return_patch(request['receiver']['type-expr'])\n";
        let mut fixture = Fixture::from_parts(text.into(), manifest, plugin);
        let first = fixture
            .analysis
            .calls
            .values()
            .find(|site| fixture.source.slice(site.function).ends_with(".select"))
            .unwrap()
            .clone();
        let second = fixture
            .analysis
            .calls
            .values()
            .find(|site| fixture.source.slice(site.function).ends_with(".again"))
            .unwrap()
            .clone();
        let oracle = Oracle::from([
            (first.receiver_key(), "Self@example".into()),
            (second.receiver_key(), "Self@example".into()),
        ]);
        let first_response = call_request(
            &mut fixture.runtime,
            &fixture.source,
            "adjust-call-return",
            &first,
            Some(&oracle),
            Some("demo.Selection"),
        )
        .unwrap();
        let raw = BTreeMap::from([(first.span, first_response["return-type"].clone())]);
        project_with_return_types(
            &fixture.source,
            &fixture.suite,
            &mut fixture.analysis,
            &mut fixture.runtime,
            Some(&oracle),
            &ReturnPatches::new(),
            &raw,
        )
        .unwrap();
        let second = &fixture.analysis.calls[&second.span];
        assert_eq!(second.flow_receiver_type.as_deref(), Some("demo.Selection"));
        let response = call_request(
            &mut fixture.runtime,
            &fixture.source,
            "adjust-call-return",
            second,
            Some(&oracle),
            Runtime::receiver_identity(&raw[&first.span]).as_deref(),
        )
        .unwrap();
        assert_eq!(response["return-type"]["expression"], "Self");
        assert_eq!(
            Runtime::receiver_identity(&response["return-type"]).as_deref(),
            Some("demo.Selection")
        );
    }

    #[test]
    fn assignment_receivers_use_constructor_return_refinements() {
        let text = "class ReadOnly: pass\nclass Mutable: pass\nclass Request:\n    data: ReadOnly\nclass FreshRequest(Request):\n    data: Mutable\ndef example(request: Request) -> None:\n    request.data['key'] = 1\n    Request().data['key'] = 1\n";
        let manifest = json!({
            "id":"native.mutation-contract","name":"Mutation contract","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},
            "capabilities":{"call-return":true,"mutation-validation":true},
            "claims":{"functions":[{"qualified-name":"demo.Request"}],"mutations":[{"kind":"exact","qualified-name":"demo.ReadOnly"}]}
        });
        let plugin = "@on_mutation\ndef mutation(request):\n    return mutation_diagnostics([diagnostic('read-only-write', 'Read-only object')])\n";
        let mut fixture = Fixture::from_parts(text.into(), manifest, plugin);
        let constructor = fixture
            .analysis
            .calls
            .values()
            .find(|site| site.callee == "demo.Request")
            .unwrap();
        let raw = BTreeMap::from([(constructor.span, typ("demo.FreshRequest"))]);
        let flow = project_with_return_types(
            &fixture.source,
            &fixture.suite,
            &mut fixture.analysis,
            &mut fixture.runtime,
            Some(&Oracle::new()),
            &ReturnPatches::new(),
            &raw,
        )
        .unwrap();
        assert_eq!(
            flow.diagnostics.len(),
            1,
            "only the original request is read-only: {:?}",
            flow.diagnostics
        );
        assert_eq!(flow.diagnostics[0]["diagnostic"]["id"], "read-only-write");
    }

    #[test]
    fn loop_else_postconditions_do_not_escape_the_loop() {
        let loops = [
            ("empty-for", "    for _ in ():\n        pass\n"),
            ("for-break", "    for _ in values:\n        break\n"),
            (
                "conditional-break",
                "    for _ in values:\n        if condition:\n            break\n",
            ),
            ("while-break", "    while condition:\n        break\n"),
            ("empty-while", "    while False:\n        pass\n"),
            (
                "nested-break",
                "    for _ in values:\n        for _ in values:\n            break\n",
            ),
            (
                "nested-else-break",
                "    for _ in values:\n        for _ in ():\n            pass\n        else:\n            break\n",
            ),
            (
                "nested-function-break",
                "    for _ in values:\n        def nested() -> None:\n            for _ in values:\n                break\n",
            ),
        ];
        for (name, loop_body) in loops {
            let text = format!(
                "class Item:\n    id: int | None\n    def touch(self) -> None: pass\ndef example(item: Item, values: list[int], condition: bool) -> None:\n{loop_body}    else:\n        item.touch()\n        inside = item.id\n    after = item.id\n"
            );
            let manifest = json!({
                "id":"native.loop-contract","name":"Loop contract","version":"0.0.0",
                "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
                "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-state":true},
                "claims":{"methods":[{"kind":"exact","class-qualified-name":"demo.Item","method-name":"touch"}]}
            });
            let plugin = "@on_call_state\ndef state(request):\n    return call_state_patch(receiver_members={'id': type_annotation('int')})\n";
            let mut fixture = Fixture::from_parts(text, manifest, plugin);
            let flow = project(
                &fixture.source,
                &fixture.suite,
                &mut fixture.analysis,
                &mut fixture.runtime,
            )
            .unwrap();
            let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
            let inside = rendered
                .text
                .lines()
                .find(|line| line.contains("inside ="))
                .unwrap();
            assert!(inside.contains(".int, item.id)"), "{name}: {inside}");
            assert!(
                rendered.text.contains("    after = item.id\n"),
                "loop-else fact escaped {name}: {}",
                rendered.text
            );
        }
    }

    #[test]
    fn loop_break_can_bypass_terminating_else() {
        for (loop_header, termination) in [
            ("for _ in values", "return"),
            ("for _ in values", "raise RuntimeError"),
            ("while condition", "return"),
            ("while condition", "raise RuntimeError"),
        ] {
            let text = format!(
                "class Item:\n    id: int | None\n    def touch(self) -> None: pass\ndef example(item: Item, values: list[int], condition: bool) -> None:\n    {loop_header}:\n        break\n    else:\n        {termination}\n    item.touch()\n    after = item.id\n"
            );
            let manifest = json!({
                "id":"native.loop-exit-contract","name":"Loop exit contract","version":"0.0.0",
                "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
                "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-state":true},
                "claims":{"methods":[{"kind":"exact","class-qualified-name":"demo.Item","method-name":"touch"}]}
            });
            let plugin = "@on_call_state\ndef state(request):\n    return call_state_patch(receiver_members={'id': type_annotation('int')})\n";
            let mut fixture = Fixture::from_parts(text, manifest, plugin);
            let flow = project(
                &fixture.source,
                &fixture.suite,
                &mut fixture.analysis,
                &mut fixture.runtime,
            )
            .unwrap();
            let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
            let after = rendered
                .text
                .lines()
                .find(|line| line.contains("after ="))
                .unwrap();
            assert!(
                after.contains(".int, item.id)"),
                "lost break continuation for {loop_header}/else {termination}: {after}"
            );
        }
    }

    #[test]
    fn loop_break_can_bypass_else_alias_assignment() {
        for body in [
            "        break\n",
            "        if condition:\n            break\n",
        ] {
            let text = format!(
                "class Item:\n    id: int | None\n    def touch(self) -> None: pass\ndef example(first: Item, second: Item, values: list[int], condition: bool) -> None:\n    for _ in values:\n{body}    else:\n        first = second\n    first.touch()\n    changed = first.id\n    other = second.id\n"
            );
            let manifest = json!({
                "id":"native.loop-alias-contract","name":"Loop alias contract","version":"0.0.0",
                "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
                "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"call-state":true},
                "claims":{"methods":[{"kind":"exact","class-qualified-name":"demo.Item","method-name":"touch"}]}
            });
            let plugin = "@on_call_state\ndef state(request):\n    return call_state_patch(receiver_members={'id': type_annotation('int')})\n";
            let mut fixture = Fixture::from_parts(text, manifest, plugin);
            let call = fixture.analysis.calls.values().next().unwrap();
            let oracle = Oracle::from([(call.receiver_key(), "Item".to_owned())]);
            let flow = project_with_oracle(
                &fixture.source,
                &fixture.suite,
                &mut fixture.analysis,
                &mut fixture.runtime,
                Some(&oracle),
                &ReturnPatches::new(),
            )
            .unwrap();
            let rendered = crate::mapping::render(&fixture.source, &flow.edits).unwrap();
            let changed = rendered
                .text
                .lines()
                .find(|line| line.contains("changed ="))
                .unwrap();
            assert!(changed.contains(".int, first.id)"), "{changed}");
            assert!(
                rendered.text.contains("    other = second.id\n"),
                "conditional alias was treated as certain: {}",
                rendered.text
            );
        }
    }
}
