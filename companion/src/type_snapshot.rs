//! Structural protocol types reconstructed from checker annotations and retained
//! generated-declaration provenance. Never interpret a diagnostic display as a
//! record schema: generated records use their registered field definitions.
use crate::model::span;
use ruff_python_ast::{Expr, Operator};
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
            "nominal" | "tuple" | "union" | "typed-dict" => {
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

pub fn from_annotation(text: &str, lookup: &impl Fn(&str) -> Option<Value>) -> Value {
    let Ok(parsed) = ruff_python_parser::parse_expression(text) else {
        return expression(text);
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
