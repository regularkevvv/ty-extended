//! Structural protocol types reconstructed from checker annotations and retained
//! generated-declaration provenance. Never interpret a diagnostic display as a
//! record schema: generated records use their registered field definitions.
use crate::model::span;
use ruff_python_ast::{Expr, Operator, Stmt};
use serde_json::{Value, json};

pub fn expression(text: &str) -> Value {
    json!({"kind":"expression","expression":text,"mode":"annotation"})
}

/// Snapshot of a retained virtual field after the context-free protocol decode.
/// Unlike an outgoing Annotated snapshot, receiving Annotated discards metadata.
pub fn decoded_annotation(value: &Value) -> Value {
    fn decoded(snapshot: &Value) -> Option<Value> {
        match snapshot["kind"].as_str()? {
            "expression" => Some(decoded_annotation(&json!({
                "expression":snapshot["expression"], "imports":snapshot["imports"]
            }))),
            "annotated" => decoded(&snapshot["base"]),
            "self-type" => snapshot.get("bound").and_then(decoded),
            "plugin-class" => None,
            // Parameter and type-param objects carry their own `kind` tags;
            // only the nested snapshot fields decode, never the list items.
            "callable" => {
                let mut value = snapshot.clone();
                if let Some(returns) = value.get_mut("return-type") {
                    *returns = decoded(returns).unwrap_or_else(|| expression("Unknown"));
                }
                for parameter in value["parameters"].as_array_mut().into_iter().flatten() {
                    if let Some(ty) = parameter.get_mut("type-snapshot") {
                        *ty = decoded(ty).unwrap_or_else(|| expression("Unknown"));
                    }
                }
                for param in value["type-params"].as_array_mut().into_iter().flatten() {
                    for key in ["bound", "default"] {
                        if let Some(bound) = param.get_mut(key) {
                            *bound = decoded(bound).unwrap_or_else(|| expression("Unknown"));
                        }
                    }
                    for constraint in param["constraints"].as_array_mut().into_iter().flatten() {
                        *constraint = decoded(constraint).unwrap_or_else(|| expression("Unknown"));
                    }
                }
                Some(value)
            }
            "nominal" | "tuple" | "union" | "typed-dict" | "overloaded" => {
                let mut value = snapshot.clone();
                fn children(value: &mut Value) {
                    match value {
                        Value::Object(object) => {
                            for value in object.values_mut() {
                                if value.get("kind").is_some() {
                                    *value =
                                        decoded(value).unwrap_or_else(|| expression("Unknown"));
                                } else {
                                    children(value);
                                }
                            }
                        }
                        Value::Array(values) => values.iter_mut().for_each(|value| {
                            if value.get("kind").is_some() {
                                *value = decoded(value).unwrap_or_else(|| expression("Unknown"));
                            } else {
                                children(value);
                            }
                        }),
                        _ => {}
                    }
                }
                children(&mut value);
                Some(value)
            }
            _ => None,
        }
    }
    if let Some(snapshot) = value.get("snapshot").and_then(decoded) {
        return snapshot;
    }
    let bindings = value["imports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|binding| {
            let module = binding["module"].as_str()?;
            let name = binding["name"].as_str()?;
            Some((
                binding["alias"].as_str().unwrap_or(name),
                format!("{module}.{name}"),
            ))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let text =
        crate::project::rewrite_names(value["expression"].as_str().unwrap_or("Unknown"), |name| {
            let (head, tail) = name.split_once('.').unwrap_or((name, ""));
            bindings.get(head).map_or_else(
                || name.to_owned(),
                |qualified| {
                    if tail.is_empty() {
                        qualified.clone()
                    } else {
                        format!("{qualified}.{tail}")
                    }
                },
            )
        });
    let snapshot = from_annotation(&text, &|_| None);
    if snapshot["kind"] == "annotated" {
        decoded(&snapshot["base"]).unwrap_or_else(|| expression("Unknown"))
    } else {
        snapshot
    }
}

/// Structural snapshot for the checker's callable display forms:
/// `def name[...](params) -> ret`, `(params) -> ret` and `(...) -> ret`.
/// `def` displays are re-parsed through a synthesized definition so parameter
/// kinds, defaults and PEP 695 type parameters come from the grammar rather
/// than from ad-hoc string scanning.
fn callable_display(text: &str, lookup: &impl Fn(&str) -> Option<Value>) -> Option<Value> {
    fn bracket_end(text: &str, open_index: usize) -> Option<usize> {
        let mut depth = 0usize;
        for (offset, ch) in text[open_index..].char_indices() {
            match ch {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(open_index + offset);
                    }
                }
                _ => {}
            }
        }
        None
    }
    let text = text.trim();
    // Locate the suffix that starts a signature: `[` or `(` after an optional
    // `def qualified.name` / `async def qualified.name` prefix.
    let suffix = if let Some(rest) = text
        .strip_prefix("def ")
        .or_else(|| text.strip_prefix("async def "))
    {
        let name_len = rest
            .find(|ch: char| !(ch.is_alphanumeric() || matches!(ch, '_' | '.')))
            .unwrap_or(rest.len());
        let rest = &rest[name_len..];
        if rest.starts_with('(') || rest.starts_with('[') {
            rest
        } else {
            return None;
        }
    } else if text.starts_with('(') {
        text
    } else {
        return None;
    };
    if !suffix.contains("->") {
        return None;
    }
    // The parameter list opens at `(` — after the closing `]` of a PEP 695
    // type-parameter list, whose own bounds may legitimately contain parens.
    let open = if suffix.starts_with('[') {
        let end = bracket_end(suffix, 0)?;
        if !suffix[end + 1..].starts_with('(') {
            return None;
        }
        end + 1
    } else {
        0
    };
    let close = bracket_end(suffix, open)?;
    // `(...) -> ret` is the checker spelling of Callable[..., ret]; the
    // synthesized definition cannot express it, so mark the ellipsis first.
    if suffix[open + 1..close].trim() == "..." {
        let returns = suffix[close + 1..].trim_start().strip_prefix("->")?.trim();
        return Some(json!({
            "kind": "callable", "display": text, "ellipsis": true,
            "return-type": from_annotation(returns, lookup),
        }));
    }
    let definition = format!("def _tyx_{suffix}: ...");
    let Ok(parsed) = ruff_python_parser::parse_module(&definition) else {
        return None;
    };
    let Stmt::FunctionDef(function) = parsed.suite().first()? else {
        return None;
    };
    let parameters = &function.parameters;
    let annotation = |parameter: &ruff_python_ast::Parameter| {
        parameter.annotation.as_ref().map(|annotation| {
            from_annotation(
                &definition[span(annotation.as_ref()).start..span(annotation.as_ref()).end],
                lookup,
            )
        })
    };
    let mut items = Vec::new();
    let mut ordinary = |kind: &str, list: &[ruff_python_ast::ParameterWithDefault]| {
        for parameter in list {
            let mut item = json!({
                "name": parameter.parameter.name.as_str(),
                "kind": kind,
                "required": parameter.default.is_none(),
            });
            if let Some(snapshot) = annotation(&parameter.parameter) {
                item["type-snapshot"] = snapshot;
            }
            items.push(item);
        }
    };
    ordinary("positional-only", &parameters.posonlyargs);
    ordinary("positional-or-keyword", &parameters.args);
    ordinary("keyword-only", &parameters.kwonlyargs);
    for (kind, parameter) in [
        ("var-args", parameters.vararg.as_deref()),
        ("kwargs", parameters.kwarg.as_deref()),
    ] {
        if let Some(parameter) = parameter {
            let mut item = json!({"name": parameter.name.as_str(), "kind": kind});
            if let Some(snapshot) = annotation(parameter) {
                item["type-snapshot"] = snapshot;
            }
            items.push(item);
        }
    }
    let type_params: Vec<Value> = function
        .type_params
        .iter()
        .flat_map(|parameters| parameters.iter())
        .map(|parameter| {
            let (name, kind, bound, default) = match parameter {
                ruff_python_ast::TypeParam::TypeVar(parameter) => (
                    parameter.name.as_str(),
                    "type-var",
                    parameter.bound.as_deref(),
                    parameter.default.as_deref(),
                ),
                ruff_python_ast::TypeParam::ParamSpec(parameter) => (
                    parameter.name.as_str(),
                    "param-spec",
                    None,
                    parameter.default.as_deref(),
                ),
                ruff_python_ast::TypeParam::TypeVarTuple(parameter) => (
                    parameter.name.as_str(),
                    "type-var-tuple",
                    None,
                    parameter.default.as_deref(),
                ),
            };
            let mut item = json!({"name": name, "kind": kind});
            if let Some(bound) = bound {
                let snapshot =
                    from_annotation(&definition[span(bound).start..span(bound).end], lookup);
                // `T: (A, B)` declares constraints rather than a bound.
                if snapshot["kind"] == "tuple" {
                    item["constraints"] = json!(snapshot["prefix"]);
                } else {
                    item["bound"] = snapshot;
                }
            }
            if let Some(default) = default {
                item["default"] =
                    from_annotation(&definition[span(default).start..span(default).end], lookup);
            }
            item
        })
        .collect();
    let returns = function
        .returns
        .as_ref()
        .map(|returns| {
            from_annotation(
                &definition[span(returns.as_ref()).start..span(returns.as_ref()).end],
                lookup,
            )
        })
        .unwrap_or_else(|| expression("Unknown"));
    let mut snapshot = json!({"kind": "callable", "display": text, "return-type": returns});
    if !type_params.is_empty() {
        snapshot["type-params"] = json!(type_params);
    }
    if !items.is_empty() {
        snapshot["parameters"] = json!(items);
    }
    Some(snapshot)
}

/// Render a callable snapshot's canonical `(...) -> ret` display when the
/// checker's own spelling is unavailable (plugin-authored snapshots).
pub fn callable_display_text(snapshot: &Value) -> String {
    fn render(snapshot: &Value) -> String {
        match snapshot["kind"].as_str() {
            Some("callable") => {
                if let Some(display) = snapshot["display"].as_str()
                    && !display.is_empty()
                {
                    return display.to_owned();
                }
            }
            Some("expression") => {
                return snapshot["expression"]
                    .as_str()
                    .unwrap_or("Unknown")
                    .to_owned();
            }
            _ => {}
        }
        qualified_expression(snapshot, "Unknown", None)
    }
    let parameters = snapshot["parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|parameter| {
            let ty = parameter
                .get("type-snapshot")
                .map(render)
                .unwrap_or_else(|| "Unknown".into());
            let name = parameter["name"].as_str().unwrap_or("");
            match parameter["kind"].as_str().unwrap_or("") {
                "var-args" => format!("*{name}: {ty}"),
                "kwargs" => format!("**{name}: {ty}"),
                "param-spec" => ty,
                _ if name.is_empty() => ty,
                _ => format!("{name}: {ty}"),
            }
        })
        .collect::<Vec<_>>();
    let inner = if snapshot["ellipsis"] == true {
        "...".into()
    } else {
        parameters.join(", ")
    };
    format!("({}) -> {}", inner, render(&snapshot["return-type"]))
}

/// Rewrite `callable` snapshots to the protocol-1 `expression` fallback for
/// plugins declaring a minor version before 0.10. `display` preserves the
/// checker's exact spelling so older plugins observe identical payloads.
pub fn downgrade_callable_snapshots(value: &mut Value) {
    match value {
        Value::Object(object) => {
            let kind = object.get("kind").and_then(Value::as_str);
            if kind == Some("overloaded") {
                // Only emitted by plugins back to the host; if it ever reaches
                // an old-protocol request, degrade to the first callable.
                for element in object
                    .get_mut("elements")
                    .and_then(Value::as_array_mut)
                    .into_iter()
                    .flatten()
                {
                    downgrade_callable_snapshots(element);
                }
                if let Some(first) = object["elements"].as_array().and_then(|e| e.first()) {
                    *value = first.clone();
                }
                return;
            }
            if kind == Some("callable") {
                let display = object["display"]
                    .as_str()
                    .filter(|display| !display.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| callable_display_text(&json!(object.clone())));
                *value = json!({"kind":"expression","expression":display,"mode":"annotation"});
                return;
            }
            for value in object.values_mut() {
                downgrade_callable_snapshots(value);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(downgrade_callable_snapshots),
        _ => {}
    }
}

pub fn from_annotation(text: &str, lookup: &impl Fn(&str) -> Option<Value>) -> Value {
    let Ok(parsed) = ruff_python_parser::parse_expression(text) else {
        return callable_display(text, lookup).unwrap_or_else(|| expression(text));
    };
    fn node(text: &str, value: &Expr, lookup: &impl Fn(&str) -> Option<Value>) -> Value {
        let raw = &text[span(value).start..span(value).end];
        if let Some(found) = lookup(raw) {
            return found;
        }
        let child = |value: &Expr| node(text, value, lookup);
        match value {
            Expr::BinOp(union) if union.op == Operator::BitOr => {
                let mut elements = Vec::new();
                for value in [&union.left, &union.right] {
                    let snapshot = child(value);
                    if snapshot["kind"] == "union" {
                        elements.extend(snapshot["elements"].as_array().unwrap().iter().cloned());
                    } else {
                        elements.push(snapshot);
                    }
                }
                json!({"kind":"union","elements":elements})
            }
            Expr::Subscript(generic) => {
                let origin =
                    &text[span(generic.value.as_ref()).start..span(generic.value.as_ref()).end];
                let args = match generic.slice.as_ref() {
                    Expr::Tuple(tuple) => tuple.elts.iter().collect::<Vec<_>>(),
                    value => vec![value],
                };
                match origin {
                    "tuple" | "builtins.tuple" | "typing.Tuple" => {
                        let mut prefix = Vec::new();
                        let mut suffix = Vec::new();
                        let mut variadic = None;
                        if args.len() == 2 && matches!(args[1], Expr::EllipsisLiteral(_)) {
                            variadic = Some(child(args[0]));
                        } else {
                            for arg in args {
                                if let Expr::Starred(starred) = arg {
                                    let nested = child(&starred.value);
                                    if nested["kind"] == "tuple" {
                                        prefix.extend(
                                            nested["prefix"]
                                                .as_array()
                                                .into_iter()
                                                .flatten()
                                                .cloned(),
                                        );
                                        variadic = nested
                                            .get("variadic")
                                            .filter(|value| !value.is_null())
                                            .cloned();
                                        suffix.extend(
                                            nested["suffix"]
                                                .as_array()
                                                .into_iter()
                                                .flatten()
                                                .cloned(),
                                        );
                                    } else {
                                        variadic = Some(expression("object"));
                                    }
                                } else if variadic.is_some() {
                                    suffix.push(child(arg));
                                } else {
                                    prefix.push(child(arg));
                                }
                            }
                        }
                        let mut result = json!({"kind":"tuple","prefix":prefix,"suffix":suffix});
                        if let Some(variadic) = variadic {
                            result["variadic"] = variadic;
                        }
                        result
                    }
                    "typing.Union" | "Union" => {
                        json!({"kind":"union","elements":args.into_iter().map(child).collect::<Vec<_>>()})
                    }
                    "typing.Optional" | "Optional" if args.len() == 1 => {
                        json!({"kind":"union","elements":[child(args[0]),expression("None")]})
                    }
                    "typing.Annotated" | "typing_extensions.Annotated" | "Annotated"
                        if !args.is_empty() =>
                    {
                        let metadata = args[1..].iter().map(|arg| {
                            let raw = &text[span(*arg).start..span(*arg).end];
                            let (name, arguments) = crate::runtime::generic_parts(raw);
                            json!({"qualified-name":name,"arguments":arguments.into_iter().map(|arg| from_annotation(arg,lookup)).collect::<Vec<_>>()})
                        }).collect::<Vec<_>>();
                        json!({"kind":"annotated","base":child(args[0]),"metadata":metadata})
                    }
                    "Callable" | "typing.Callable" | "collections.abc.Callable"
                        if !args.is_empty() =>
                    {
                        let returns = args
                            .last()
                            .map(|returns| child(returns))
                            .unwrap_or_else(|| expression("Unknown"));
                        let positional = |parameter: &&Expr| json!({"kind":"positional-only","type-snapshot":child(parameter),"required":true});
                        let mut parameters = Vec::new();
                        let mut ellipsis = false;
                        match args[0] {
                            Expr::EllipsisLiteral(_) => ellipsis = true,
                            Expr::List(list) => {
                                parameters.extend(
                                    list.elts.iter().collect::<Vec<_>>().iter().map(positional),
                                );
                            }
                            Expr::Subscript(concatenate)
                                if matches!(
                                    &text[span(concatenate.value.as_ref()).start
                                        ..span(concatenate.value.as_ref()).end],
                                    "Concatenate"
                                        | "typing.Concatenate"
                                        | "typing_extensions.Concatenate"
                                ) =>
                            {
                                let mut items = match concatenate.slice.as_ref() {
                                    Expr::Tuple(tuple) => tuple.elts.iter().collect::<Vec<_>>(),
                                    value => vec![value],
                                };
                                if let Some(spec) = items.pop() {
                                    for parameter in &items {
                                        parameters.push(positional(parameter));
                                    }
                                    parameters.push(
                                        json!({"kind":"param-spec","type-snapshot":child(spec)}),
                                    );
                                }
                            }
                            Expr::Name(_) => {
                                parameters.push(
                                    json!({"kind":"param-spec","type-snapshot":child(args[0])}),
                                );
                            }
                            _ => ellipsis = true,
                        }
                        let mut snapshot =
                            json!({"kind":"callable","display":raw,"return-type":returns});
                        if !parameters.is_empty() {
                            snapshot["parameters"] = json!(parameters);
                        }
                        if ellipsis {
                            snapshot["ellipsis"] = json!(true);
                        }
                        snapshot
                    }
                    _ => nominal(origin, args.into_iter().map(child).collect())
                        .unwrap_or_else(|| expression(raw)),
                }
            }
            Expr::Call(call) => {
                let name = &text[span(call.func.as_ref()).start..span(call.func.as_ref()).end];
                if matches!(
                    name,
                    "NamedTuple" | "typing.NamedTuple" | "typing_extensions.NamedTuple"
                ) {
                    return json!({"kind":"plugin-class","identity":raw});
                }
                if matches!(
                    name,
                    "TypedDict" | "typing.TypedDict" | "typing_extensions.TypedDict"
                ) && let Some(Expr::Dict(fields)) = call
                    .arguments
                    .args
                    .iter()
                    .find(|arg| matches!(arg, Expr::Dict(_)))
                {
                    let mut items = Vec::new();
                    for item in &fields.items {
                        let Some(Expr::StringLiteral(name)) = &item.key else {
                            return expression(raw);
                        };
                        let mut annotation = &item.value;
                        let mut required = true;
                        let mut readonly = false;
                        while let Expr::Subscript(wrapper) = annotation {
                            let origin = &text[span(wrapper.value.as_ref()).start
                                ..span(wrapper.value.as_ref()).end];
                            match origin.rsplit('.').next().unwrap_or(origin) {
                                "Required" => required = true,
                                "NotRequired" => required = false,
                                "ReadOnly" => readonly = true,
                                _ => break,
                            }
                            annotation = &wrapper.slice;
                        }
                        items.push(json!({"name":name.value.to_str(),"type-snapshot":child(annotation),"required":required,"read-only":readonly}));
                    }
                    return json!({"kind":"typed-dict","fields":items,"closed":true});
                }
                expression(raw)
            }
            Expr::NoneLiteral(_) => json!({"kind":"nominal","qualified-name":"types.NoneType"}),
            Expr::Name(_) | Expr::Attribute(_) => {
                if matches!(raw, "Self" | "typing.Self" | "typing_extensions.Self") {
                    json!({"kind":"self-type"})
                } else {
                    nominal(raw, Vec::new()).unwrap_or_else(|| expression(raw))
                }
            }
            _ => expression(raw),
        }
    }
    node(text, &parsed.syntax().body, lookup)
}

/// The native SDK keeps a lossless snapshot beside a deliberately nominal
/// qualified expression. In particular tuple's nominal generic argument is
/// its element union, while its snapshot retains the fixed positions.
pub fn qualified_expression(snapshot: &Value, fallback: &str, module: Option<&str>) -> String {
    fn ordinary(snapshot: &Value) -> String {
        match snapshot["kind"].as_str().unwrap_or("") {
            "nominal" if snapshot["qualified-name"] == "types.NoneType" => "None".into(),
            "nominal" => {
                let name = snapshot["qualified-name"].as_str().unwrap_or("Unknown");
                let name = name.rsplit('.').next().unwrap_or(name);
                let args: Vec<_> = snapshot["arguments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(ordinary)
                    .collect();
                if args.is_empty() {
                    name.to_owned()
                } else {
                    format!("{name}[{}]", args.join(", "))
                }
            }
            "union" => snapshot["elements"]
                .as_array()
                .into_iter()
                .flatten()
                .map(ordinary)
                .collect::<Vec<_>>()
                .join(" | "),
            "overloaded" => snapshot["elements"]
                .as_array()
                .and_then(|elements| elements.first())
                .map(ordinary)
                .unwrap_or_else(|| "Unknown".to_owned()),
            "expression" => snapshot["expression"]
                .as_str()
                .unwrap_or("Unknown")
                .to_owned(),
            "annotated" => ordinary(&snapshot["base"]),
            _ => qualified_expression(snapshot, "Unknown", None),
        }
    }
    match snapshot["kind"].as_str().unwrap_or("") {
        "nominal" => {
            let name = snapshot["qualified-name"].as_str().unwrap_or(fallback);
            let args: Vec<_> = snapshot["arguments"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|arg| qualified_expression(arg, &ordinary(arg), module))
                .collect();
            if args.is_empty() {
                name.to_owned()
            } else {
                format!("{name}[{}]", args.join(", "))
            }
        }
        "tuple" => {
            fn elements(snapshot: &Value, values: &mut Vec<String>) {
                if snapshot["kind"] == "union" {
                    for element in snapshot["elements"].as_array().into_iter().flatten() {
                        elements(element, values)
                    }
                } else {
                    let value = ordinary(snapshot);
                    if !values.contains(&value) {
                        values.push(value);
                    }
                }
            }
            let mut values = Vec::new();
            for element in snapshot["prefix"]
                .as_array()
                .into_iter()
                .flatten()
                .chain(snapshot.get("variadic").filter(|v| !v.is_null()))
                .chain(snapshot["suffix"].as_array().into_iter().flatten())
            {
                elements(element, &mut values);
            }
            let inner = if values.is_empty() {
                "Never".into()
            } else if values.len() == 1 {
                let child = snapshot["prefix"]
                    .as_array()
                    .and_then(|v| v.first())
                    .or_else(|| snapshot.get("variadic"))
                    .or_else(|| snapshot["suffix"].as_array().and_then(|v| v.first()));
                child
                    .map(|value| qualified_expression(value, &values[0], module))
                    .unwrap_or_else(|| values[0].clone())
            } else {
                values.join(" | ")
            };
            format!("builtins.tuple[{inner}]")
        }
        "typed-dict" => {
            let mut names: Vec<_> = snapshot["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|field| field["name"].as_str())
                .collect();
            names.sort();
            format!(
                "<TypedDict with items {}>",
                names
                    .into_iter()
                    .map(|name| format!("'{name}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        "union" => ordinary(snapshot),
        "callable" => {
            let display = snapshot["display"].as_str().unwrap_or("");
            if display.is_empty() {
                callable_display_text(snapshot)
            } else {
                display.to_owned()
            }
        }
        "annotated" => qualified_expression(&snapshot["base"], fallback, module),
        "plugin-class" => {
            let identity = snapshot["identity"].as_str().unwrap_or(fallback);
            if let Ok(parsed) = ruff_python_parser::parse_expression(identity)
                && let Expr::Call(call) = parsed.syntax().body.as_ref()
                && let Some(Expr::StringLiteral(name)) = call.arguments.args.first()
            {
                return module.filter(|module| !module.is_empty()).map_or_else(
                    || name.value.to_str().to_owned(),
                    |module| format!("{module}.{}", name.value.to_str()),
                );
            }
            identity.to_owned()
        }
        _ => fallback.to_owned(),
    }
}

fn nominal(name: &str, arguments: Vec<Value>) -> Option<Value> {
    let name = match name {
        "str" | "int" | "bool" | "float" | "complex" | "bytes" | "bytearray" | "memoryview"
        | "list" | "dict" | "set" | "frozenset" | "object" => format!("builtins.{name}"),
        name if name.contains('.')
            && !name.starts_with("typing.")
            && !name.starts_with("typing_extensions.")
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.')) =>
        {
            name.to_owned()
        }
        _ => return None,
    };
    Some(json!({"kind":"nominal","qualified-name":name,"arguments":arguments}))
}

/// Nominal identity used for receiver routing; structural records are carried by
/// their declaration registry and must not be guessed from their display.
pub fn receiver_annotation(snapshot: &Value) -> Option<String> {
    match snapshot["kind"].as_str()? {
        "expression" => crate::project::type_expression(snapshot),
        "nominal" => {
            let name = snapshot["qualified-name"].as_str()?;
            let arguments = snapshot["arguments"]
                .as_array()
                .into_iter()
                .flatten()
                .map(receiver_annotation)
                .collect::<Option<Vec<_>>>()?;
            Some(if arguments.is_empty() {
                name.to_owned()
            } else {
                format!("{name}[{}]", arguments.join(", "))
            })
        }
        "plugin-class" => snapshot["identity"].as_str().map(str::to_owned),
        "self-type" => receiver_annotation(&snapshot["bound"]),
        "annotated" => receiver_annotation(&snapshot["base"]),
        _ => None,
    }
}

/// Named TypedDict annotations are not nominal wire types. Preserve their
/// declared field schema, including inherited fields and generic substitution.
pub fn project_record(
    project: &crate::project::Project,
    qualified: &str,
    active: &mut std::collections::BTreeSet<String>,
) -> Option<Value> {
    use ruff_python_ast::{Stmt, StmtClassDef};
    use std::collections::BTreeMap;
    let (name, arguments) = crate::runtime::generic_parts(qualified);
    if !["typing.TypedDict", "typing_extensions.TypedDict"]
        .iter()
        .any(|base| project.is_subclass(name, base))
        || !active.insert(qualified.to_owned())
    {
        return None;
    }
    let result = (|| {
        let summary = project.class_summary(name)?;
        let module_name = summary["source"]["module"].as_str()?;
        let module = project.modules.get(module_name)?;
        fn find<'a>(suite: &'a [Stmt], prefix: &str, name: &str) -> Option<&'a StmtClassDef> {
            for statement in suite {
                match statement {
                    Stmt::ClassDef(class) => {
                        let qualified = format!("{prefix}.{}", class.name);
                        if qualified == name {
                            return Some(class);
                        }
                        if let Some(found) = find(&class.body, &qualified, name) {
                            return Some(found);
                        }
                    }
                    Stmt::If(branch) => {
                        if let Some(found) = find(&branch.body, prefix, name) {
                            return Some(found);
                        }
                        for clause in &branch.elif_else_clauses {
                            if let Some(found) = find(&clause.body, prefix, name) {
                                return Some(found);
                            }
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        let class = find(&module.suite, module_name, name)?;
        let parameters = class
            .type_params
            .as_ref()
            .map(|parameters| {
                parameters
                    .iter()
                    .map(|parameter| parameter.name().as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let substitute = |expression: &str| {
            crate::project::rewrite_names(expression, |name| {
                let short = name
                    .strip_prefix(&format!("{module_name}."))
                    .unwrap_or(name);
                parameters
                    .iter()
                    .position(|parameter| *parameter == short)
                    .map_or_else(
                        || name.to_owned(),
                        |index| {
                            arguments
                                .get(index)
                                .copied()
                                .unwrap_or("ty_extensions._internal.Unknown")
                                .to_owned()
                        },
                    )
            })
        };
        let snapshot = |annotation: &str, active: &mut std::collections::BTreeSet<String>| {
            from_annotation(annotation, &|name| {
                project_record(project, name, &mut active.clone())
            })
        };
        let mut fields = BTreeMap::new();
        let mut closed = false;
        let mut extra = None;
        for base in summary["bases"].as_array().into_iter().flatten() {
            if let Some(expression) = base["expression"].as_str()
                && let Some(parent) = project_record(project, &substitute(expression), active)
            {
                for field in parent["fields"].as_array().into_iter().flatten() {
                    fields.insert(field["name"].as_str()?.to_owned(), field.clone());
                }
                closed = parent["closed"] == true;
                extra = parent.get("extra-items").cloned();
            }
        }
        let mut total = true;
        for keyword in class
            .arguments
            .as_ref()
            .into_iter()
            .flat_map(|arguments| &arguments.keywords)
        {
            let raw = module.source.slice(span(&keyword.value));
            match keyword.arg.as_ref().map(|name| name.as_str()) {
                Some("total") => total = raw != "False",
                Some("closed") => {
                    closed = raw == "True";
                    extra = None;
                }
                Some("extra_items") => {
                    let annotation = substitute(&project.canonical_name(module_name, raw));
                    let (origin, arguments) = crate::runtime::generic_parts(&annotation);
                    let readonly =
                        matches!(origin, "typing.ReadOnly" | "typing_extensions.ReadOnly");
                    let annotation = if readonly {
                        *arguments.first()?
                    } else {
                        &annotation
                    };
                    extra = Some(
                        json!({"name":"","type-snapshot":snapshot(annotation,active),"required":false,"read-only":readonly}),
                    );
                    closed = false;
                }
                _ => {}
            }
        }
        for field in summary["fields"].as_array().into_iter().flatten() {
            let Some(annotation) = field["annotation"]["expression"].as_str() else {
                continue;
            };
            let annotation = substitute(annotation);
            let mut annotation = annotation.as_str();
            let mut required = total;
            let mut readonly = false;
            loop {
                let (origin, arguments) = crate::runtime::generic_parts(annotation);
                if arguments.len() != 1 {
                    break;
                }
                match origin {
                    "typing.Required" | "typing_extensions.Required" => required = true,
                    "typing.NotRequired" | "typing_extensions.NotRequired" => required = false,
                    "typing.ReadOnly" | "typing_extensions.ReadOnly" => readonly = true,
                    _ => break,
                }
                annotation = arguments[0];
            }
            let name = field["name"].as_str()?;
            fields.insert(name.to_owned(),json!({"name":name,"type-snapshot":snapshot(annotation,active),"required":required,"read-only":readonly}));
        }
        let mut result = json!({"kind":"typed-dict","fields":fields.into_values().collect::<Vec<_>>(),"closed":closed});
        if let Some(extra) = extra {
            result["extra-items"] = extra;
        }
        Some(result)
    })();
    active.remove(qualified);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_receiver_display_keeps_native_lossy_spelling_and_lossless_snapshot() {
        let snapshot = from_annotation(
            "query.QuerySet[models.Book, tuple[str, int | None]]",
            &|_| None,
        );
        assert_eq!(
            qualified_expression(&snapshot, "", Some("models")),
            "query.QuerySet[models.Book, builtins.tuple[str | int | None]]"
        );
        assert_eq!(
            snapshot["arguments"][1]["prefix"].as_array().unwrap().len(),
            2
        );
        let snapshot = from_annotation(
            "query.QuerySet[models.Book, NamedTuple(\"Row\", {\"title\": str})]",
            &|_| None,
        );
        assert_eq!(
            qualified_expression(&snapshot, "", Some("models")),
            "query.QuerySet[models.Book, models.Row]"
        );
        assert_eq!(snapshot["arguments"][1]["kind"], "plugin-class");
    }

    #[test]
    fn context_free_field_decode_respects_imports_and_received_metadata() {
        let value = json!({"expression":"bytes","snapshot":{"kind":"nominal","qualified-name":"builtins.list","arguments":[{"kind":"annotated","base":{"kind":"expression","expression":"Alias","imports":[{"module":"builtins","name":"str","alias":"Alias"}]},"metadata":[{"qualified-name":"metadata.Marker"}]}]}});
        let result = decoded_annotation(&value);
        assert_eq!(result["qualified-name"], "builtins.list");
        assert_eq!(result["arguments"][0]["qualified-name"], "builtins.str");
        assert_eq!(result["arguments"][0]["kind"], "nominal");
        assert_eq!(
            decoded_annotation(
                &json!({"expression":"int","snapshot":{"kind":"plugin-class","identity":"absent.Virtual"}})
            )["qualified-name"],
            "builtins.int"
        );
    }

    #[test]
    fn tuple_segments_unions_and_metadata_preserve_structure() {
        let snapshot = from_annotation("tuple[int, *tuple[str, ...], bool]", &|_| None);
        assert_eq!(snapshot["kind"], "tuple");
        assert_eq!(snapshot["prefix"][0]["qualified-name"], "builtins.int");
        assert_eq!(snapshot["variadic"]["qualified-name"], "builtins.str");
        assert_eq!(snapshot["suffix"][0]["qualified-name"], "builtins.bool");
        assert_eq!(from_annotation("tuple[()]", &|_| None)["prefix"], json!([]));
        let snapshot = from_annotation(
            "typing.Annotated[int | str, package.Metadata[list[int]]]",
            &|_| None,
        );
        assert_eq!(snapshot["base"]["kind"], "union");
        assert_eq!(
            snapshot["metadata"][0]["qualified-name"],
            "package.Metadata"
        );
        assert_eq!(
            snapshot["metadata"][0]["arguments"][0]["arguments"][0]["qualified-name"],
            "builtins.int"
        );
        serde_json::from_value::<ty_plugin_protocol::TypeSnapshot>(snapshot).unwrap();
    }

    #[test]
    fn callable_displays_preserve_parameter_kinds_defaults_and_type_params() {
        let snapshot = from_annotation(
            "def module.add_three(a: int, b: int, c: int) -> int",
            &|_| None,
        );
        assert_eq!(snapshot["kind"], "callable");
        assert_eq!(
            snapshot["display"],
            "def module.add_three(a: int, b: int, c: int) -> int"
        );
        let parameters = snapshot["parameters"].as_array().unwrap();
        assert_eq!(parameters.len(), 3);
        assert_eq!(parameters[0]["name"], "a");
        assert_eq!(parameters[0]["kind"], "positional-or-keyword");
        assert_eq!(parameters[0]["required"], true);
        assert_eq!(
            parameters[0]["type-snapshot"]["qualified-name"],
            "builtins.int"
        );
        assert_eq!(snapshot["return-type"]["qualified-name"], "builtins.int");

        let snapshot = from_annotation(
            "def f(x: int, /, y: str, *, z: bool = ..., **rest: bytes) -> None",
            &|_| None,
        );
        let kinds: Vec<_> = snapshot["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|parameter| parameter["kind"].as_str().unwrap())
            .collect();
        assert_eq!(
            kinds,
            [
                "positional-only",
                "positional-or-keyword",
                "keyword-only",
                "kwargs"
            ]
        );
        assert_eq!(snapshot["parameters"][2]["required"], false);

        let snapshot = from_annotation(
            "def f[T: Base, *Ts, **P](x: T, *args: *Ts, **kwargs: P) -> T",
            &|_| None,
        );
        let type_params = snapshot["type-params"].as_array().unwrap();
        assert_eq!(type_params[0]["name"], "T");
        assert_eq!(type_params[0]["kind"], "type-var");
        assert_eq!(type_params[0]["bound"]["expression"], "Base");
        assert_eq!(type_params[1]["kind"], "type-var-tuple");
        assert_eq!(type_params[2]["kind"], "param-spec");
        assert_eq!(snapshot["parameters"][1]["kind"], "var-args");
        serde_json::from_value::<ty_plugin_protocol::TypeSnapshot>(snapshot).unwrap();
    }

    #[test]
    fn callable_arrow_and_callable_forms_produce_structural_snapshots() {
        let snapshot = from_annotation("(x: int) -> str", &|_| None);
        assert_eq!(snapshot["kind"], "callable");
        assert_eq!(snapshot["parameters"][0]["name"], "x");
        assert_eq!(snapshot["return-type"]["qualified-name"], "builtins.str");

        let snapshot = from_annotation("(...) -> int", &|_| None);
        assert_eq!(snapshot["kind"], "callable");
        assert_eq!(snapshot["ellipsis"], true);

        let snapshot = from_annotation("typing.Callable[[int, str], bool]", &|_| None);
        assert_eq!(snapshot["kind"], "callable");
        let parameters = snapshot["parameters"].as_array().unwrap();
        assert_eq!(parameters.len(), 2);
        assert_eq!(parameters[0]["kind"], "positional-only");
        assert_eq!(
            parameters[0]["type-snapshot"]["qualified-name"],
            "builtins.int"
        );
        assert_eq!(snapshot["return-type"]["qualified-name"], "builtins.bool");

        let snapshot = from_annotation("Callable[..., int]", &|_| None);
        assert_eq!(snapshot["ellipsis"], true);

        let snapshot = from_annotation("Callable[P, int]", &|_| None);
        assert_eq!(snapshot["parameters"][0]["kind"], "param-spec");
        assert_eq!(
            snapshot["parameters"][0]["type-snapshot"]["expression"],
            "P"
        );

        let snapshot =
            from_annotation("typing.Callable[typing.Concatenate[int, P], bool]", &|_| {
                None
            });
        let parameters = snapshot["parameters"].as_array().unwrap();
        assert_eq!(parameters[0]["kind"], "positional-only");
        assert_eq!(parameters[1]["kind"], "param-spec");
        serde_json::from_value::<ty_plugin_protocol::TypeSnapshot>(snapshot).unwrap();
    }

    #[test]
    fn callable_downgrade_restores_the_checker_display_spelling() {
        let snapshot = from_annotation("def f(a: int) -> str", &|_| None);
        assert_eq!(snapshot["kind"], "callable");
        let mut value = json!({"nested": [snapshot.clone()], "type-expr": {"expression": "def f(a: int) -> str", "snapshot": snapshot}});
        downgrade_callable_snapshots(&mut value);
        assert_eq!(value["nested"][0]["kind"], "expression");
        assert_eq!(value["nested"][0]["expression"], "def f(a: int) -> str");
        assert_eq!(value["type-expr"]["snapshot"]["kind"], "expression");
    }

    #[test]
    fn callable_display_renders_plugin_authored_snapshots() {
        let snapshot = json!({"kind":"callable","parameters":[{"name":"a","kind":"positional-or-keyword","type-snapshot":{"kind":"nominal","qualified-name":"builtins.int"},"required":true},{"kind":"param-spec","type-snapshot":{"kind":"expression","expression":"P","mode":"annotation"}}],"return-type":{"kind":"nominal","qualified-name":"builtins.str"}});
        assert_eq!(
            callable_display_text(&snapshot),
            "(a: builtins.int, P) -> builtins.str"
        );
        assert_eq!(
            qualified_expression(
                &from_annotation("def f(a: int) -> str", &|_| None),
                "",
                None
            ),
            "def f(a: int) -> str"
        );
    }

    #[test]
    fn nested_record_registry_is_authoritative_and_does_not_rewrite_literals() {
        let record = json!({"kind":"typed-dict","fields":[{"name":"x","type-snapshot":expression("str"),"required":false,"read-only":true}],"closed":true});
        let snapshot = from_annotation("package.QuerySet[package.Model, _record]", &|name| {
            (name == "_record").then(|| record.clone())
        });
        assert_eq!(snapshot["arguments"][1], record);
        assert_eq!(
            from_annotation("typing.Literal['_record']", &|name| (name == "_record")
                .then(|| record.clone())),
            expression("typing.Literal['_record']")
        );
        assert_eq!(receiver_annotation(&json!({"kind":"nominal","qualified-name":"package.QuerySet","arguments":[expression("str")]})).as_deref(),Some("package.QuerySet[str]"));
    }
}
