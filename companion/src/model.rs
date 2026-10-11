use crate::mapping::Edits;
use ruff_text_size::Ranged;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}
pub fn span(node: &impl Ranged) -> Span {
    Span {
        start: node.start().to_usize(),
        end: node.end().to_usize(),
    }
}

pub struct Source {
    pub text: String,
    pub path: String,
    pub module: String,
    /// Notebook files have no importable Python module. Internal indexing still
    /// uses `module`; only semantic protocol requests use this override.
    pub protocol_module: Option<String>,
    pub prefix: String,
    pub lines: Vec<usize>,
}
impl Source {
    pub fn new(text: String, path: String, module: String) -> Self {
        let mut prefix = "_ty_ext_".to_string();
        while text.contains(&prefix) {
            prefix.push('_');
        }
        let mut lines = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' || (b == b'\r' && text.as_bytes().get(i + 1) != Some(&b'\n')) {
                lines.push(i + 1);
            }
        }
        Self {
            text,
            path,
            module,
            protocol_module: None,
            prefix,
            lines,
        }
    }
    pub fn slice(&self, s: Span) -> &str {
        &self.text[s.start..s.end]
    }
    pub fn position(&self, offset: usize) -> Value {
        let line = self.lines.partition_point(|p| *p <= offset) - 1;
        let column = self.text[self.lines[line]..offset].chars().count() + 1;
        let column = if line == 0 && self.text.starts_with('\u{feff}') {
            column.saturating_sub(1).max(1)
        } else {
            column
        };
        json!({"line":line + 1,"column":column})
    }
    pub fn location(&self, s: Span) -> Value {
        json!({"module":self.module,"file-path":self.path,
            "start":self.position(s.start),"end":self.position(s.end)})
    }
    pub fn cast_name(&self) -> String {
        format!("{}cast", self.prefix)
    }
}

#[derive(Clone)]
pub struct Argument {
    pub span: Span,
    pub name: Option<String>,
    pub literal: Value,
    pub kind: &'static str,
    pub inferred_type: Option<Value>,
}
#[derive(Clone)]
pub struct CallSite {
    pub span: Span,
    pub function: Span,
    pub callee: String,
    pub arguments: Vec<Argument>,
    pub receiver_span: Option<Span>,
    pub receiver_type: Option<String>,
    pub flow_receiver_type: Option<String>,
}
impl CallSite {
    pub fn argument_key(&self, index: usize) -> String {
        format!("{}:{}:arg:{index}", self.span.start, self.span.end)
    }
    pub fn return_key(&self) -> String {
        format!("{}:{}:default", self.span.start, self.span.end)
    }
    pub fn receiver_key(&self) -> String {
        format!("{}:{}:receiver", self.span.start, self.span.end)
    }
}
#[derive(Clone)]
pub struct ClassInfo {
    pub origin: Span,
    pub fields: BTreeMap<String, String>,
    pub methods: BTreeSet<String>,
    pub insertion: usize,
    pub indent: String,
    pub declared: BTreeSet<String>,
}
#[derive(Default)]
pub struct Analysis {
    pub prelude_offset: usize,
    pub classes: BTreeMap<String, ClassInfo>,
    pub intrinsics: BTreeSet<String>,
    pub calls: BTreeMap<Span, CallSite>,
    pub constructor_calls: BTreeMap<Span, String>,
    pub unbound_calls: BTreeSet<Span>,
    pub annotated_casts: BTreeMap<Span, Value>,
    pub edits: Edits,
    pub helpers: BTreeMap<String, String>,
    pub expression_probes: BTreeMap<Span, String>,
    pub plugin_diagnostics: Vec<Value>,
    pub generated_declarations: crate::mapping::GeneratedDeclarations,
}
pub fn typ(expression: impl Into<String>) -> Value {
    json!({"expression":expression.into(),"mode":"annotation"})
}
pub fn literal(expr: &ruff_python_ast::Expr) -> Value {
    use ruff_python_ast::Expr;
    fn symbol(expr: &Expr) -> Option<String> {
        match expr {
            Expr::Name(name) => Some(name.id.to_string()),
            Expr::Attribute(attribute) => {
                Some(format!("{}.{}", symbol(&attribute.value)?, attribute.attr))
            }
            _ => None,
        }
    }
    match expr {
        Expr::StringLiteral(s) => json!({"kind":"str","value":s.value.to_str()}),
        Expr::BooleanLiteral(b) => json!({"kind":"bool","value":b.value}),
        Expr::NoneLiteral(_) => json!({"kind":"none"}),
        Expr::Name(_) => json!({"kind":"symbol-ref","qualified-name":symbol(expr).unwrap()}),
        Expr::Attribute(_) => symbol(expr).map_or_else(
            || json!({"kind":"unknown"}),
            |name| json!({"kind":"enum-ref","qualified-name":name}),
        ),
        Expr::NumberLiteral(n) => match &n.value {
            ruff_python_ast::Number::Int(value) => value
                .as_i64()
                .map(|v| json!({"kind":"int","value":v}))
                .unwrap_or_else(|| json!({"kind":"unknown"})),
            _ => json!({"kind":"unknown"}),
        },
        Expr::List(value) => {
            json!({"kind":"list","items":value.elts.iter().map(literal).collect::<Vec<_>>()})
        }
        Expr::Tuple(value) => {
            json!({"kind":"tuple","items":value.elts.iter().map(literal).collect::<Vec<_>>()})
        }
        Expr::Dict(value) => json!({
            "kind":"dict","entries":value.items.iter().filter_map(|item| Some(json!({
                "key":literal(item.key.as_ref()?),"value":literal(&item.value)
            }))).collect::<Vec<_>>()
        }),
        _ => json!({"kind":"unknown"}),
    }
}
pub fn call_site(call: &ruff_python_ast::ExprCall, callee: String) -> CallSite {
    let mut arguments: Vec<_> = call
        .arguments
        .args
        .iter()
        .map(|a| Argument {
            span: span(a),
            name: None,
            literal: literal(a),
            inferred_type: None,
            kind: if matches!(a, ruff_python_ast::Expr::Starred(_)) {
                "star-args"
            } else {
                "positional"
            },
        })
        .collect();
    arguments.extend(call.arguments.keywords.iter().map(|k| Argument {
        span: span(&k.value),
        name: k.arg.as_ref().map(ToString::to_string),
        literal: literal(&k.value),
        inferred_type: None,
        kind: if k.arg.is_some() {
            "keyword"
        } else {
            "star-kwargs"
        },
    }));
    CallSite {
        span: span(call),
        function: span(call.func.as_ref()),
        callee,
        arguments,
        receiver_span: match call.func.as_ref() {
            ruff_python_ast::Expr::Attribute(attribute) => Some(span(attribute.value.as_ref())),
            _ => None,
        },
        receiver_type: None,
        flow_receiver_type: None,
    }
}

pub type Oracle = BTreeMap<String, String>;
pub type ReturnPatches = BTreeMap<Span, String>;

pub fn expression_key(span: Span) -> String {
    format!("expr:{}:{}", span.start, span.end)
}

pub fn comprehension(
    expr: &ruff_python_ast::Expr,
) -> Option<(
    &[ruff_python_ast::Comprehension],
    Vec<&ruff_python_ast::Expr>,
)> {
    use ruff_python_ast::Expr;
    match expr {
        Expr::ListComp(comp) => Some((&comp.generators, vec![&comp.elt])),
        Expr::SetComp(comp) => Some((&comp.generators, vec![&comp.elt])),
        Expr::DictComp(comp) => Some((
            &comp.generators,
            comp.key
                .iter()
                .map(|key| key.as_ref())
                .chain(std::iter::once(comp.value.as_ref()))
                .collect(),
        )),
        Expr::Generator(comp) => Some((&comp.generators, vec![&comp.elt])),
        _ => None,
    }
}

/// Assignment expressions bind outside comprehensions; iteration targets do not.
pub fn comprehension_outer_bindings(expr: &ruff_python_ast::Expr) -> BTreeSet<String> {
    use ruff_python_ast::{
        Expr,
        visitor::{self, Visitor},
    };
    #[derive(Default)]
    struct Named(BTreeSet<String>);
    impl<'a> Visitor<'a> for Named {
        fn visit_expr(&mut self, expression: &'a Expr) {
            match expression {
                Expr::Named(named) => {
                    if let Expr::Name(name) = named.target.as_ref() {
                        self.0.insert(name.id.to_string());
                    }
                    self.visit_expr(&named.value);
                }
                Expr::Lambda(_) => {}
                _ => visitor::walk_expr(self, expression),
            }
        }
    }
    let mut names = Named::default();
    names.visit_expr(expr);
    names.0
}

pub fn pattern_bindings(pattern: &ruff_python_ast::Pattern) -> BTreeSet<String> {
    use ruff_python_ast::{
        Pattern,
        visitor::{self, Visitor},
    };
    struct Bindings(BTreeSet<String>);
    impl<'a> Visitor<'a> for Bindings {
        fn visit_pattern(&mut self, pattern: &'a Pattern) {
            let name = match pattern {
                Pattern::MatchAs(pattern) => pattern.name.as_ref(),
                Pattern::MatchStar(pattern) => pattern.name.as_ref(),
                Pattern::MatchMapping(pattern) => pattern.rest.as_ref(),
                _ => None,
            };
            if let Some(name) = name {
                self.0.insert(name.to_string());
            }
            visitor::walk_pattern(self, pattern);
        }
    }
    let mut bindings = Bindings(BTreeSet::new());
    bindings.visit_pattern(pattern);
    bindings.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fluent_calls_have_distinct_oracle_keys() {
        let parsed = ruff_python_parser::parse_expression("factory().method()").unwrap();
        let ruff_python_ast::Expr::Call(outer) = parsed.syntax().body.as_ref() else {
            panic!()
        };
        let ruff_python_ast::Expr::Attribute(method) = outer.func.as_ref() else {
            panic!()
        };
        let ruff_python_ast::Expr::Call(inner) = method.value.as_ref() else {
            panic!()
        };
        let outer = call_site(outer, "factory().method".into());
        let inner = call_site(inner, "factory".into());
        assert_eq!(outer.span.start, inner.span.start);
        assert_ne!(outer.return_key(), inner.return_key());
        assert_ne!(outer.receiver_key(), inner.receiver_key());
        assert_ne!(outer.argument_key(0), inner.argument_key(0));
    }

    #[test]
    fn argument_symbols_match_the_sdk_literal_contract() {
        for (source, expected) in [
            (
                "parameter",
                json!({"kind":"symbol-ref","qualified-name":"parameter"}),
            ),
            (
                "State.READY",
                json!({"kind":"enum-ref","qualified-name":"State.READY"}),
            ),
            ("factory().field", json!({"kind":"unknown"})),
            (
                "{'known': parameter, **extras}",
                json!({"kind":"dict","entries":[{"key":{"kind":"str","value":"known"},"value":{"kind":"symbol-ref","qualified-name":"parameter"}}]}),
            ),
        ] {
            let parsed = ruff_python_parser::parse_expression(source).unwrap();
            let actual = literal(&parsed.syntax().body);
            assert_eq!(actual, expected);
            serde_json::from_value::<ty_plugin_protocol::LiteralValue>(actual).unwrap();
        }
    }
}
