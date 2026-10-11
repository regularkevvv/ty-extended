//! Native flow projection for the bounded companion compatibility fixtures.
//!
//! The real source AST is never rewritten. Each inferred member view becomes a
//! source-mapped wrapper, so synthetic casts cannot change object identities or
//! be mistaken for user calls. Unsupported control-flow constructs are rejected.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use ruff_python_ast::{ElifElseClause, Expr, ExprContext, Stmt, StmtFunctionDef};
use serde_json::{Value, json};

use crate::mapping::Edits;
use crate::model::{
    Analysis, Oracle, ReturnPatches, Source, Span, call_site, expression_key, literal, span, typ,
};
use crate::runtime::{Runtime, call_request};
use crate::type_render::Types;

pub struct FlowResult {
    pub edits: Edits,
    pub diagnostics: Vec<Value>,
    pub type_prelude: String,
}

type Object = usize;
type Facts = BTreeMap<String, Value>;

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
}

impl Scope {
    fn identity(&self, expr: &Expr) -> Option<Object> {
        if let Expr::Name(name) = expr {
            self.names.get(name.id.as_str()).copied()
        } else {
            self.objects.get(&span(expr)).copied()
        }
    }

    fn invalidate(&mut self) {
        for facts in self.facts.values_mut() {
            facts.clear();
        }
        // At module level an unknown call can also rebind globals. Local names
        // retain identity while member facts are invalidated.
        if self.module {
            self.names.clear();
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
    Ok(project.result)
}

impl Project<'_> {
    fn fresh(&mut self, scope: &mut Scope, owner: &str, guaranteed: bool) -> Object {
        self.next_object += 1;
        let id = self.next_object;
        scope.types.insert(id, owner.to_string());
        scope.facts.insert(id, Facts::new());
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
        if let Some(owner) = owner {
            let id = self.fresh(&mut sub, owner, false);
            sub.names.insert("self".to_string(), id);
        }
        for parameter in function.parameters.iter_non_variadic_params() {
            if parameter.name() != "self"
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
                // Class method bodies execute in their own flow scope. In
                // particular, their mutations do not execute at declaration.
                for member in &class.body {
                    if let Stmt::FunctionDef(function) = member {
                        let owner = self
                            .runtime
                            .canonical_name(self.source, class.name.as_str());
                        self.function(function, Some(&owner))?;
                    }
                }
            }
            Stmt::FunctionDef(function) => self.function(function, None)?,
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
                let initial = scope.clone();
                self.body(scope, &branch.body)?;
                let left = scope.clone();
                *scope = initial;
                self.else_clauses(scope, &branch.elif_else_clauses)?;
                let right = scope.clone();
                self.join(scope, left, right);
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
                    self.expr(scope, value)?;
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
                scope.invalidate();
            }
            Stmt::Delete(delete) if self.runtime.project.is_some() => {
                for target in &delete.targets {
                    if let Expr::Name(name) = target {
                        scope.names.remove(name.id.as_str());
                    } else {
                        self.expr(scope, target)?;
                    }
                }
                scope.invalidate();
            }
            Stmt::Break(_) | Stmt::Continue(_) if self.runtime.project.is_some() => {
                scope.terminated = true
            }
            Stmt::Global(_) | Stmt::Nonlocal(_) if self.runtime.project.is_some() => {
                scope.invalidate()
            }
            Stmt::TypeAlias(_) if self.runtime.project.is_some() => {}
            Stmt::Import(_) | Stmt::ImportFrom(_) | Stmt::Pass(_) => {}
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

    fn assignment(
        &mut self,
        scope: &mut Scope,
        target: &Expr,
        value: Option<&Expr>,
        identity: Option<Object>,
    ) -> Result<()> {
        match target {
            Expr::Name(name) => {
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
                if let Some(owner) = self.identity(scope, &target.value) {
                    let patch = self.runtime.execute(self.source, "validate-mutation", json!({
                        "operation":"item-set", "receiver":typ(&scope.types[&owner]),
                        "key":{"kind":"positional", "value":literal(&target.slice)},
                        "value":{"kind":"positional", "value":value.map_or(json!({"kind":"unknown"}), literal)},
                        "source":self.source.location(span(target)),
                    }))?;
                    if let Some(diagnostics) = patch["diagnostics"].as_array() {
                        for diagnostic in diagnostics {
                            self.result.diagnostics.push(json!({
                                "diagnostic":diagnostic,
                                "source":self.source.location(span(target)),
                            }));
                        }
                    }
                }
                scope.invalidate();
            }
            Expr::Attribute(target) => {
                self.expr(scope, &target.value)?;
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

    fn join(&self, scope: &mut Scope, left: Scope, right: Scope) {
        if left.terminated {
            *scope = right;
            return;
        }
        if right.terminated {
            *scope = left;
            return;
        }
        scope.shadowed = left.shadowed.union(&right.shadowed).cloned().collect();
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
        for identity in left.facts.keys().chain(right.facts.keys()) {
            let first = left.facts.get(identity);
            let second = right.facts.get(identity);
            let defaults = self
                .analysis
                .classes
                .get(&scope.types[identity])
                .map(|class| &class.fields);
            let mut facts = Facts::new();
            for member in first
                .into_iter()
                .flat_map(|facts| facts.keys())
                .chain(second.into_iter().flat_map(|facts| facts.keys()))
            {
                let default = self
                    .runtime
                    .default_member_expr(&scope.types[identity], member)
                    .or_else(|| defaults.and_then(|fields| fields.get(member)).map(typ))
                    .unwrap_or_else(|| typ("object"));
                let types = [
                    first
                        .and_then(|facts| facts.get(member))
                        .unwrap_or(&default),
                    second
                        .and_then(|facts| facts.get(member))
                        .unwrap_or(&default),
                ];
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
            scope.facts.insert(*identity, facts);
        }
    }

    fn identity(&mut self, scope: &mut Scope, expr: &Expr) -> Option<Object> {
        if let Some(identity) = scope.identity(expr) {
            return Some(identity);
        }
        let known = if let Expr::Attribute(attribute) = expr {
            self.identity(scope, &attribute.value).and_then(|identity| {
                let owner = &scope.types[&identity];
                let (nominal, _) = crate::runtime::generic_parts(owner);
                self.runtime.member_type(
                    self.source,
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
        if let Expr::Name(name) = call.func.as_ref()
            && self.analysis.intrinsics.contains(name.id.as_str())
        {
            if let Some(first) = call.arguments.args.first() {
                self.expr(scope, first)?;
            }
            return Ok(());
        }

        self.expr(scope, &call.func)?;
        for argument in &call.arguments.args {
            self.expr(scope, argument)?;
        }
        for keyword in &call.arguments.keywords {
            self.expr(scope, &keyword.value)?;
        }

        // A display such as `Record` cannot distinguish two imported models
        // with that name. Keep the precise type already established for a
        // parameter, local alias, or returned object alongside its display.
        let argument_types: BTreeMap<_, _> = call
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
            self.identity(scope, &attribute.value).or_else(|| {
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

        if patch["kind"] != "call-state-patch"
            || !patch["preserves-other-objects"].as_bool().unwrap_or(false)
        {
            scope.invalidate();
        } else if let Some(receiver) = receiver {
            for (identity, facts) in &mut scope.facts {
                if *identity == receiver
                    || !scope.fresh.contains(&receiver)
                    || !scope.fresh.contains(identity)
                {
                    facts.clear();
                }
            }
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
                let mut expression = scope
                    .facts
                    .get(&identity)
                    .and_then(|facts| facts.get(member))
                    .cloned();
                if expression.is_none() {
                    let response = self.runtime.execute(
                        self.source,
                        "resolve-instance-member",
                        json!({
                            "owner":typ(&owner), "member-name":member,
                        }),
                    )?;
                    if response["kind"] == "member-patch" {
                        let class = self.analysis.classes.get_mut(&owner);
                        if response["mode"].as_str().unwrap_or("fill-on-miss") == "fill-on-miss"
                            && (class
                                .as_ref()
                                .is_some_and(|class| class.fields.contains_key(member))
                                || self.runtime.default_member_type(&owner, member).is_some())
                        {
                            return Ok(());
                        }
                        if response["access"]["kind"] != "value" {
                            bail!(
                                "Rust companion unsupported member access shape: {}",
                                response["access"]
                            );
                        }
                        let resolved = response["access"]["type-expr"].clone();
                        if let Some(class) = class {
                            class
                                .fields
                                .insert(member.to_string(), self.types.render(&resolved)?);
                        }
                        expression = Some(resolved);
                    }
                }
                if let Some(expression) = expression {
                    let expression = self.types.render(&expression)?;
                    self.result.edits.wrap(
                        span(attribute),
                        format!("{}({}, ", self.source.cast_name(), expression),
                        ")".into(),
                        None,
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
                if let Some((first, rest)) = boolean.values.split_first() {
                    self.expr(scope, first)?;
                    for value in rest {
                        let skipped = scope.clone();
                        self.expr(scope, value)?;
                        let executed = scope.clone();
                        self.join(scope, skipped, executed);
                    }
                }
            }
            Expr::If(branch) => {
                self.expr(scope, &branch.test)?;
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
                scope.invalidate();
            }
            Expr::UnaryOp(operator) => {
                self.expr(scope, &operator.operand)?;
                scope.invalidate();
            }
            Expr::Compare(compare) => {
                for operand in &compare.operands {
                    self.expr(scope, operand)?;
                }
                scope.invalidate();
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
            _ if self.runtime.project.is_some() => {
                // Comprehensions, formatting, and yield can invoke user code.
                // Preserve checks within their expressions, and discard member
                // postconditions across the operation until its control-flow
                // semantics have a dedicated transfer function.
                struct Children<'a>(Vec<&'a Expr>);
                impl<'a> ruff_python_ast::visitor::Visitor<'a> for Children<'a> {
                    fn visit_expr(&mut self, expression: &'a Expr) {
                        self.0.push(expression);
                    }
                }
                let mut children = Children(Vec::new());
                ruff_python_ast::visitor::walk_expr(&mut children, expr);
                scope.invalidate();
                for child in children.0 {
                    self.expr(scope, child)?;
                }
                scope.invalidate();
            }
            _ => bail!(
                "Rust companion unsupported expression flow at {:?}",
                span(expr)
            ),
        }
        Ok(())
    }
}

fn extend_members(facts: &mut Facts, members: &Value) {
    if let Some(members) = members.as_object() {
        for (name, value) in members {
            facts.insert(name.clone(), value.clone());
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
