//! Lower protocol type constructors into ordinary checker-visible declarations.
use crate::model::span;
use crate::project::Project;
use anyhow::{Context, Result, bail};
use ruff_python_ast::{
    Expr,
    visitor::{self, Visitor},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

/// Immutable project inputs shared by all renderers in the same revision.
/// Aliases, generated declarations and candidate discoveries remain local to
/// each renderer, so sharing this context cannot change declaration ordering.
#[derive(Clone, Default)]
struct RenderContext {
    modules: Arc<BTreeSet<String>>,
    virtual_classes: Arc<BTreeSet<String>>,
    virtual_modules: Arc<BTreeMap<String, String>>,
    unavailable_modules: Arc<BTreeSet<String>>,
    nominal_resolution: Arc<BTreeMap<String, Option<usize>>>,
    annotation_resolution: Arc<BTreeMap<String, bool>>,
}

impl RenderContext {
    fn from_project(project: &Project) -> Self {
        Self {
            modules: project.renderer_module_names(),
            virtual_classes: project.renderer_virtual_class_names(),
            virtual_modules: project.virtual_modules.clone(),
            unavailable_modules: project.unavailable_modules.clone(),
            nominal_resolution: project.snapshot_nominals.clone(),
            annotation_resolution: project.snapshot_symbols.clone(),
        }
    }

    fn virtual_module(&self, module: &str) -> bool {
        // An unqualified virtual class uses a fallback generated module, but
        // does not declare that module as a public namespace. Preserve the
        // exact dotted-class rule without scanning every project class.
        let prefix = format!("{module}.");
        self.virtual_classes
            .range::<str, _>((
                std::ops::Bound::Included(prefix.as_str()),
                std::ops::Bound::Unbounded,
            ))
            .take_while(|name| name.starts_with(&prefix))
            .any(|name| {
                name.rsplit_once('.')
                    .is_some_and(|(owner, _)| owner == module)
            })
    }

    fn virtual_class(&self, qualified: &str) -> bool {
        qualified.contains('.') && self.virtual_classes.contains(qualified)
    }
}

#[derive(Clone)]
pub struct Types {
    pub imports: BTreeSet<String>,
    pub definitions: String,
    prefix: String,
    module: String,
    unimportable_module: bool,
    aliases: BTreeMap<String, String>,
    modules: BTreeSet<String>,
    project_context: RenderContext,
    import_bindings: BTreeMap<String, String>,
    imported_symbols: BTreeMap<(String, String), String>,
    imported_modules: BTreeMap<String, String>,
    imported_virtual_modules: BTreeMap<String, String>,
    unresolved_bindings: BTreeSet<String>,
    self_context: Option<Value>,
    snapshots: BTreeMap<String, String>,
    protocol_types: BTreeMap<String, Value>,
    nominal_candidates: BTreeSet<String>,
    annotation_candidates: BTreeSet<String>,
}
impl Types {
    pub fn new(prefix: &str, module: &str) -> Self {
        Self::with_context(prefix, module, RenderContext::default())
    }
    fn with_context(prefix: &str, module: &str, project_context: RenderContext) -> Self {
        Self {
            imports: BTreeSet::new(),
            definitions: String::new(),
            prefix: prefix.into(),
            module: module.into(),
            unimportable_module: false,
            aliases: BTreeMap::new(),
            modules: BTreeSet::from([module.to_owned()]),
            project_context,
            import_bindings: BTreeMap::new(),
            imported_symbols: BTreeMap::new(),
            imported_modules: BTreeMap::new(),
            imported_virtual_modules: BTreeMap::new(),
            unresolved_bindings: BTreeSet::new(),
            self_context: None,
            snapshots: BTreeMap::new(),
            protocol_types: BTreeMap::new(),
            nominal_candidates: BTreeSet::new(),
            annotation_candidates: BTreeSet::new(),
        }
    }
    pub fn with_project(prefix: &str, module: &str, project: &Project) -> Self {
        let mut renderer = Self::with_context(prefix, module, RenderContext::from_project(project));
        renderer.unimportable_module = project
            .modules
            .get(module)
            .is_some_and(|module| module.source.protocol_module.is_some());
        renderer
    }
    pub fn add_module(&mut self, module: &str) {
        self.modules.insert(module.to_owned());
    }
    fn known_module<'a>(&self, name: &'a str) -> Option<&'a str> {
        let mut remaining = name;
        while let Some((module, _)) = remaining.rsplit_once('.') {
            if self.modules.contains(module)
                || self.project_context.modules.contains(module)
                || self.project_context.virtual_module(module)
            {
                return Some(module);
            }
            remaining = module;
        }
        None
    }
    pub fn display_names(&self) -> BTreeMap<String, String> {
        self.imported_modules
            .iter()
            .chain(self.imported_virtual_modules.iter())
            .map(|(module, alias)| (alias.clone(), module.clone()))
            .chain(
                self.imported_symbols
                    .iter()
                    .map(|((module, symbol), alias)| (alias.clone(), format!("{module}.{symbol}"))),
            )
            .chain(
                self.aliases
                    .iter()
                    .map(|(expression, alias)| (alias.clone(), expression.clone())),
            )
            .chain(self.protocol_types.iter().filter_map(|(alias, value)| {
                Some((alias.clone(), value["expression"].as_str()?.to_owned()))
            }))
            .collect()
    }
    /// Preserve structural declarations through the checker's short hover names.
    pub fn register_protocol_types(&self, runtime: &mut crate::runtime::Runtime) {
        runtime.register_protocol_types(&self.module, &self.prefix, self.protocol_types.clone());
        runtime
            .nominal_candidates
            .extend(self.nominal_candidates.iter().cloned());
        runtime
            .annotation_candidates
            .extend(self.annotation_candidates.iter().cloned());
    }
    fn protocol_annotation(&self, rendered: &str) -> String {
        let names: BTreeMap<_, _> = self
            .imported_modules
            .iter()
            .chain(self.imported_virtual_modules.iter())
            .map(|(module, alias)| (alias.clone(), module.clone()))
            .chain(
                self.imported_symbols
                    .iter()
                    .map(|((module, symbol), alias)| (alias.clone(), format!("{module}.{symbol}"))),
            )
            .collect();
        crate::project::rewrite_names(rendered, |name| {
            let (head, tail) = name.split_once('.').unwrap_or((name, ""));
            names.get(head).map_or_else(
                || name.to_owned(),
                |origin| {
                    if tail.is_empty() {
                        origin.clone()
                    } else {
                        format!("{origin}.{tail}")
                    }
                },
            )
        })
    }
    fn protocol_snapshot(&self, rendered: &str) -> Value {
        let qualified = self.protocol_annotation(rendered);
        crate::type_snapshot::from_annotation(&qualified, &|name| {
            self.protocol_types
                .get(name)
                .and_then(|value| value.get("snapshot"))
                .cloned()
        })
    }
    pub fn typing_alias(&mut self) -> String {
        self.module_alias("typing")
    }
    pub fn typing_extensions_alias(&mut self) -> String {
        self.module_alias("typing_extensions")
    }
    pub fn builtins_alias(&mut self) -> String {
        self.module_alias("builtins")
    }
    pub fn set_self_context(&mut self, context: Option<Value>) -> Option<Value> {
        std::mem::replace(&mut self.self_context, context)
    }
    /// Direct member hooks use the instance-get type even for a class lookup.
    /// This follows plugin_member_access_to_type_with_virtual_types in ty; the
    /// descriptor class/set types belong to class-transform field patches.
    pub fn member_access(&mut self, access: &Value, helper: &str) -> Result<String> {
        match access["kind"].as_str().unwrap_or("value") {
            "descriptor" => self.render(&access["instance-get-type"]),
            "callable" => {
                let signature = &access["signature"];
                let parameters = self.parameter_list(
                    signature["parameters"]
                        .as_array()
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    true,
                )?;
                let result = self.render(&signature["return-type"])?;
                let typing = self.typing_alias();
                self.definitions.push_str(&format!(
                    "class {helper}({typing}.Protocol):\n    def __call__({parameters}) -> {result}: ...\n"
                ));
                Ok(helper.to_owned())
            }
            _ => self.render(&access["type-expr"]),
        }
    }
    pub fn render(&mut self, value: &Value) -> Result<String> {
        // The protocol's structural representation takes precedence over its
        // display spelling. The latter is a fallback only when ty cannot resolve
        // a nominal/plugin/Self snapshot in the current context.
        if let Some(snapshot) = value.get("snapshot").filter(|value| !value.is_null())
            && let Some(rendered) = self.snapshot(snapshot)?
        {
            return Ok(rendered);
        }
        let previous_bindings = std::mem::take(&mut self.import_bindings);
        let previous_unresolved = std::mem::take(&mut self.unresolved_bindings);
        for import in value["imports"].as_array().into_iter().flatten() {
            let module = import["module"].as_str().context("type import module")?;
            let name = import["name"].as_str().context("type import name")?;
            let alias = import["alias"].as_str().unwrap_or(name);
            if self
                .project_context
                .unavailable_modules
                .iter()
                .any(|unresolved| {
                    module == unresolved || module.starts_with(&format!("{unresolved}."))
                })
            {
                self.unresolved_bindings.insert(alias.to_owned());
                continue;
            }
            if let Some(generated) = self.virtual_symbol(&format!("{module}.{name}")) {
                self.import_bindings.insert(alias.to_owned(), generated);
                continue;
            }
            if module == self.module {
                if self.unimportable_module {
                    self.import_bindings
                        .insert(alias.to_owned(), name.to_owned());
                    continue;
                }
                // Importing a symbol from this file before its declaration
                // creates a circular binding. A module alias supports forward
                // references while remaining hygienic inside local scopes.
                let module_alias = self.module_alias(module);
                self.import_bindings
                    .insert(alias.to_owned(), format!("{module_alias}.{name}"));
                continue;
            }
            let key = (module.to_owned(), name.to_owned());
            let generated = if let Some(generated) = self.imported_symbols.get(&key) {
                generated.clone()
            } else {
                let generated = format!("{}symbol_{}", self.prefix, self.imported_symbols.len());
                self.imports
                    .insert(format!("from {module} import {name} as {generated}"));
                self.imported_symbols.insert(key, generated.clone());
                generated
            };
            self.import_bindings.insert(alias.to_owned(), generated);
        }
        let rendered = self.text(value["expression"].as_str().context("type expression")?);
        self.import_bindings = previous_bindings;
        self.unresolved_bindings = previous_unresolved;
        rendered
    }
    fn snapshot_or_unknown(&mut self, value: &Value) -> Result<String> {
        Ok(self.snapshot(value)?.unwrap_or_else(|| self.unknown()))
    }
    fn snapshot(&mut self, value: &Value) -> Result<Option<String>> {
        let result = match value["kind"].as_str().context("type snapshot kind")? {
            "expression" => self.render(value)?,
            "nominal" => {
                let name = value["qualified-name"]
                    .as_str()
                    .context("nominal snapshot name")?;
                if self.unresolved(name) || !name.split('.').all(identifier) {
                    return Ok(None);
                }
                let arguments = value["arguments"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                if let Some(resolution) = self.project_context.nominal_resolution.get(name) {
                    if resolution
                        .is_none_or(|arity| !arguments.is_empty() && arity != arguments.len())
                    {
                        return Ok(None);
                    }
                } else {
                    self.nominal_candidates.insert(name.to_owned());
                }
                let base = self.text(name)?;
                if arguments.is_empty() {
                    base
                } else {
                    let arguments = arguments
                        .iter()
                        .map(|argument| self.snapshot_or_unknown(argument))
                        .collect::<Result<Vec<_>>>()?;
                    if name == "builtins.tuple" && arguments.len() == 1 {
                        // The pinned decoder's nominal tuple constructor
                        // yields its default tuple shape; structural element
                        // types are represented by the dedicated tuple variant.
                        base
                    } else {
                        format!("{base}[{}]", arguments.join(", "))
                    }
                }
            }
            "tuple" => {
                let mut elements = value["prefix"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|element| self.snapshot_or_unknown(element))
                    .collect::<Result<Vec<_>>>()?;
                let tuple = format!("{}.tuple", self.builtins_alias());
                if let Some(variadic) = value.get("variadic").filter(|value| !value.is_null()) {
                    let element = self.snapshot_or_unknown(variadic)?;
                    elements.push(format!("*{tuple}[{element}, ...]"));
                }
                elements.extend(
                    value["suffix"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|element| self.snapshot_or_unknown(element))
                        .collect::<Result<Vec<_>>>()?,
                );
                format!(
                    "{tuple}[{}]",
                    if elements.is_empty() {
                        "()".into()
                    } else {
                        elements.join(", ")
                    }
                )
            }
            "union" => {
                let elements = value["elements"]
                    .as_array()
                    .context("union snapshot elements")?
                    .iter()
                    .map(|element| self.snapshot_or_unknown(element))
                    .collect::<Result<Vec<_>>>()?;
                if elements.is_empty() {
                    format!("{}.Never", self.typing_extensions_alias())
                } else {
                    format!("({})", elements.join(" | "))
                }
            }
            "typed-dict" => {
                let key = serde_json::to_string(value)?;
                if let Some(alias) = self.snapshots.get(&key) {
                    return Ok(Some(alias.clone()));
                }
                let alias = format!("{}snapshot_{}", self.prefix, self.snapshots.len());
                self.snapshots.insert(key, alias.clone());
                let typing = self.typing_extensions_alias();
                let mut fields = Vec::new();
                let mut wire_fields = Vec::new();
                for field in value["fields"].as_array().into_iter().flatten() {
                    let name = field["name"].as_str().context("snapshot field name")?;
                    let mut annotation = self.snapshot_or_unknown(&field["type-snapshot"])?;
                    wire_fields.push(serde_json::json!({"name":name,
                        "type-snapshot":self.protocol_snapshot(&annotation),
                        "required":field["required"] != false,"read-only":field["read-only"] == true}));
                    if field["read-only"] == true {
                        annotation = format!("{typing}.ReadOnly[{annotation}]");
                    }
                    if field["required"] == false {
                        annotation = format!("{typing}.NotRequired[{annotation}]");
                    }
                    fields.push(format!("{}: {annotation}", serde_json::to_string(name)?));
                }
                let mut wire_snapshot = serde_json::json!({"kind":"typed-dict","fields":wire_fields,"closed":value["closed"] == true});
                let openness = if let Some(extra) =
                    value.get("extra-items").filter(|value| !value.is_null())
                {
                    let mut annotation = self.snapshot_or_unknown(&extra["type-snapshot"])?;
                    wire_snapshot["closed"] = Value::Bool(false);
                    wire_snapshot["extra-items"] = serde_json::json!({"name":"","type-snapshot":self.protocol_snapshot(&annotation),"required":false,"read-only":extra["read-only"] == true});
                    if extra["read-only"] == true {
                        annotation = format!("{typing}.ReadOnly[{annotation}]");
                    }
                    format!(", extra_items={annotation}")
                } else if value["closed"] == true {
                    ", closed=True".into()
                } else {
                    String::new()
                };
                self.definitions.push_str(&format!(
                    "{alias} = {typing}.TypedDict(\"{alias}\", {{{}}}{openness})\n",
                    fields.join(", ")
                ));
                let names = wire_fields_names(&wire_snapshot);
                self.protocol_types.insert(
                    alias.clone(),
                    serde_json::json!({
                        "expression":format!("<TypedDict with items {}>",names.join(", ")),
                        "mode":"annotation","snapshot":wire_snapshot
                    }),
                );
                alias
            }
            "plugin-class" => {
                let identity = value["identity"]
                    .as_str()
                    .context("plugin class snapshot identity")?;
                if self.unresolved(identity) {
                    return Ok(None);
                }
                if identity.split('.').all(identifier) {
                    return self.snapshot(&serde_json::json!({"kind":"nominal","qualified-name":identity,"arguments":[]}));
                }
                self.text(identity)?
            }
            "self-type" => {
                if let Some(context) = self.self_context.clone() {
                    let previous = self.set_self_context(None);
                    let rendered = self.render(&context);
                    self.set_self_context(previous);
                    rendered?
                } else if let Some(bound) = value.get("bound").filter(|value| !value.is_null()) {
                    return self.snapshot(bound);
                } else {
                    return Ok(None);
                }
            }
            // The pinned checker deliberately lowers snapshot Annotated to its
            // base. Result metadata travels through the separate SDK field.
            "annotated" => return self.snapshot(&value["base"]),
            kind => bail!("unsupported type snapshot {kind}"),
        };
        Ok(Some(result))
    }
    pub fn text(&mut self, text: &str) -> Result<String> {
        // Native decoding falls back to Unknown when a display spelling is not
        // a valid annotation expression (structural display strings included).
        let Ok(parsed) = ruff_python_parser::parse_expression(text) else {
            let parts = split_top_level(text, '|');
            if parts.len() > 1 {
                return Ok(parts
                    .into_iter()
                    .map(|part| self.text(part))
                    .collect::<Result<Vec<_>>>()?
                    .join(" | "));
            }
            let (origin, arguments) = crate::runtime::generic_parts(text);
            if !arguments.is_empty() && ruff_python_parser::parse_expression(origin).is_ok() {
                let origin = self.text(origin)?;
                let arguments = arguments
                    .into_iter()
                    .map(|arg| self.text(arg))
                    .collect::<Result<Vec<_>>>()?;
                return Ok(format!("{origin}[{}]", arguments.join(", ")));
            }
            return Ok(self.unknown());
        };
        self.node(text, &parsed.syntax().body)
    }
    fn node(&mut self, source: &str, node: &Expr) -> Result<String> {
        if let Expr::Subscript(subscript) = node
            && qualified_name(&subscript.value).is_some_and(|name| self.unresolved(&name))
        {
            return Ok(self.unknown());
        }
        if let Expr::Call(call) = node
            && let Some(origin) = qualified_name(&call.func)
            && matches!(
                origin.as_str(),
                "TypedDict"
                    | "typing.TypedDict"
                    | "typing_extensions.TypedDict"
                    | "NamedTuple"
                    | "typing.NamedTuple"
                    | "typing_extensions.NamedTuple"
                    | "Class"
                    | "PluginClass"
            )
        {
            let name = origin.rsplit('.').next().unwrap();
            let raw = &source[span(node).start..span(node).end];
            if let Some(alias) = self.aliases.get(raw) {
                return Ok(alias.clone());
            }
            let alias = format!("{}type_{}", self.prefix, self.aliases.len());
            self.aliases.insert(raw.into(), alias.clone());
            let dict = call
                .arguments
                .args
                .iter()
                .find_map(|arg| {
                    if let Expr::Dict(dict) = arg {
                        Some(dict)
                    } else {
                        None
                    }
                })
                .context("structural type constructor needs a field dictionary")?;
            let mut members = Vec::new();
            for item in &dict.items {
                let Some(Expr::StringLiteral(key)) = &item.key else {
                    bail!("structural type fields need string names")
                };
                let value = self.node(source, &item.value)?;
                members.push((key.value.to_str().to_owned(), value));
            }
            let base = match name {
                "TypedDict" => {
                    format!("{}.TypedDict, closed=True", self.typing_extensions_alias())
                }
                "NamedTuple" => format!("{}.NamedTuple", self.typing_alias()),
                _ => {
                    if let Some(base) = call.arguments.args.get(2) {
                        self.node(source, base)?
                    } else {
                        format!("{}.object", self.builtins_alias())
                    }
                }
            };
            let structural = if name == "TypedDict" {
                let fields = members.iter().map(|(name, annotation)| serde_json::json!({
                    "name":name,"type-snapshot":self.protocol_snapshot(annotation),"required":true,"read-only":false
                })).collect::<Vec<_>>();
                let snapshot =
                    serde_json::json!({"kind":"typed-dict","fields":fields,"closed":true});
                Some(
                    serde_json::json!({"expression":format!("<TypedDict with items {}>",wire_fields_names(&snapshot).join(", ")),"mode":"annotation","snapshot":snapshot}),
                )
            } else {
                let original_name = call
                    .arguments
                    .args
                    .first()
                    .and_then(|arg| {
                        if let Expr::StringLiteral(name) = arg {
                            Some(name.value.to_str())
                        } else {
                            None
                        }
                    })
                    .unwrap_or("Row");
                let fields = members
                    .iter()
                    .map(|(name, annotation)| {
                        Ok(format!(
                            "{}: {}",
                            serde_json::to_string(name)?,
                            self.protocol_annotation(annotation)
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .join(", ");
                let expression = if name == "NamedTuple" {
                    format!(
                        "NamedTuple({}, {{{fields}}})",
                        serde_json::to_string(original_name)?
                    )
                } else {
                    format!(
                        "Class({}, {{{fields}}}, {})",
                        serde_json::to_string(original_name)?,
                        self.protocol_annotation(&base)
                    )
                };
                let snapshot = if name == "NamedTuple" {
                    serde_json::json!({"kind":"plugin-class","identity":expression})
                } else {
                    crate::type_snapshot::expression(&expression)
                };
                Some(
                    serde_json::json!({"expression":original_name,"mode":"annotation","snapshot":snapshot}),
                )
            };
            if name == "TypedDict" && members.iter().any(|(name, _)| !identifier(name)) {
                let fields = members
                    .iter()
                    .map(|(name, ty)| Ok(format!("{}: {ty}", serde_json::to_string(name)?)))
                    .collect::<Result<Vec<_>>>()?
                    .join(", ");
                let typing_extensions = self.typing_extensions_alias();
                self.definitions.push_str(&format!("{alias} = {typing_extensions}.TypedDict(\"{alias}\", {{{fields}}}, closed=True)\n"));
            } else {
                self.definitions
                    .push_str(&format!("class {alias}({base}):\n"));
                if members.is_empty() {
                    self.definitions.push_str("    pass\n");
                }
                for (name, ty) in members {
                    if !identifier(&name) {
                        bail!("structural class field is not a Python identifier: {name}");
                    }
                    self.definitions.push_str(&format!("    {name}: {ty}\n"));
                }
            }
            if let Some(value) = structural {
                self.protocol_types.insert(alias.clone(), value);
            }
            return Ok(alias);
        }
        // Resolve the complete qualified name before traversing Attribute's
        // shorter children. Strings are never edited, including Literal values.
        if let Some(name) = qualified_name(node) {
            if matches!(
                name.as_str(),
                "Self" | "typing.Self" | "typing_extensions.Self"
            ) && let Some(context) = self.self_context.clone()
            {
                // Resolve against the modeled class/receiver, never against a
                // generated descriptor or Protocol helper's own Self.
                let previous = self.set_self_context(None);
                let result = self.render(&context);
                self.set_self_context(previous);
                return result;
            }
            if self.unimportable_module
                && let Some(local) = name.strip_prefix(&format!("{}.", self.module))
            {
                return Ok(local.to_owned());
            }
            if self.unresolved(&name) {
                return Ok(self.unknown());
            }
            let head = name.split('.').next().unwrap();
            if let Some(generated) = self.import_bindings.get(head).cloned() {
                let qualified =
                    self.protocol_annotation(&format!("{generated}{}", &name[head.len()..]));
                if self.annotation_missing(&qualified) {
                    return Ok(self.unknown());
                }
                return Ok(format!("{generated}{}", &name[head.len()..]));
            }
            if matches!(
                name.as_str(),
                "bool"
                    | "bytes"
                    | "bytearray"
                    | "memoryview"
                    | "complex"
                    | "float"
                    | "int"
                    | "str"
                    | "object"
                    | "type"
                    | "list"
                    | "dict"
                    | "tuple"
                    | "set"
                    | "frozenset"
            ) {
                return Ok(format!("{}.{name}", self.builtins_alias()));
            }
            if matches!(
                name.as_str(),
                "Any"
                    | "Literal"
                    | "NoReturn"
                    | "Callable"
                    | "ClassVar"
                    | "Final"
                    | "Optional"
                    | "Union"
                    | "Type"
                    | "overload"
            ) {
                return Ok(format!("{}.{name}", self.typing_alias()));
            }
            if matches!(
                name.as_str(),
                "Self" | "Never" | "ReadOnly" | "Required" | "NotRequired"
            ) {
                return Ok(format!("{}.{name}", self.typing_extensions_alias()));
            }
            if let Some(generated) = self.virtual_symbol(&name) {
                return Ok(generated);
            }
            if name.contains('.') {
                if self.annotation_missing(&name) {
                    return Ok(self.unknown());
                }
                if let Some(module) = self.known_module(&name) {
                    let alias = self.module_alias(module);
                    return Ok(format!("{alias}{}", &name[module.len()..]));
                } else {
                    // Dependencies outside the project may only be known from
                    // TypeExpr's qualified fallback. Explicit import bindings
                    // and project modules always take precedence over this.
                    let pieces: Vec<_> = name.split('.').collect();
                    let end = pieces
                        .iter()
                        .position(|part| part.chars().next().is_some_and(char::is_uppercase))
                        .unwrap_or(pieces.len() - 1);
                    if end > 0 {
                        let module = pieces[..end].join(".");
                        let alias = self.module_alias(&module);
                        return Ok(format!("{alias}{}", &name[module.len()..]));
                    }
                }
            }
            return Ok(name);
        }
        struct Children<'a> {
            root: &'a Expr,
            nodes: Vec<&'a Expr>,
        }
        impl<'a> Visitor<'a> for Children<'a> {
            fn visit_expr(&mut self, expression: &'a Expr) {
                if std::ptr::eq(self.root, expression) {
                    visitor::walk_expr(self, expression);
                } else {
                    self.nodes.push(expression);
                }
            }
        }
        let mut children = Children {
            root: node,
            nodes: Vec::new(),
        };
        children.visit_expr(node);
        let range = span(node);
        let mut result = source[range.start..range.end].to_string();
        for child in children.nodes.into_iter().rev() {
            let child_range = span(child);
            let rendered = self.node(source, child)?;
            result.replace_range(
                child_range.start - range.start..child_range.end - range.start,
                &rendered,
            );
        }
        Ok(result)
    }

    fn annotation_missing(&mut self, name: &str) -> bool {
        if let Some(available) = self.project_context.annotation_resolution.get(name) {
            return !available;
        }
        if name.contains('.')
            && !name.starts_with("typing.")
            && !name.starts_with("typing_extensions.")
        {
            self.annotation_candidates.insert(name.to_owned());
        }
        false
    }
    pub fn parameter_list(&mut self, parameters: &[Value], receiver: bool) -> Result<String> {
        let mut output = if receiver {
            vec!["self".to_owned()]
        } else {
            Vec::new()
        };
        let mut starred = false;
        for (index, p) in parameters.iter().enumerate() {
            if receiver && p["name"] == "self" {
                continue;
            }
            let kind = p["kind"].as_str().unwrap_or("positional-or-keyword");
            if kind == "keyword-only" && !starred {
                output.push("*".into());
                starred = true;
            }
            let name = p["name"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| format!("arg{index}"));
            let ty = if p.get("type-expr").is_some() {
                self.render(&p["type-expr"])?
            } else {
                format!("{}.object", self.builtins_alias())
            };
            let prefix = match kind {
                "var-args" => {
                    starred = true;
                    "*"
                }
                "kwargs" => "**",
                _ => "",
            };
            let default = if !p["required"].as_bool().unwrap_or(false)
                && !matches!(kind, "var-args" | "kwargs")
            {
                let typing = self.typing_alias();
                format!(" = {typing}.cast({typing}.Any, ...)")
            } else {
                String::new()
            };
            output.push(format!("{prefix}{name}: {ty}{default}"));
            if kind == "positional-only"
                && parameters
                    .get(index + 1)
                    .is_none_or(|next| next["kind"] != "positional-only")
            {
                output.push("/".into());
            }
        }
        Ok(output.join(", "))
    }
    pub fn prelude(&self) -> String {
        format!(
            "{}\n{}",
            self.imports.iter().cloned().collect::<Vec<_>>().join("\n"),
            self.definitions
        )
    }
    fn unresolved(&self, name: &str) -> bool {
        let head = name.split('.').next().unwrap_or(name);
        let qualified = self.import_bindings.get(head).map(|generated| {
            self.protocol_annotation(&format!("{generated}{}", &name[head.len()..]))
        });
        self.project_context
            .annotation_resolution
            .get(qualified.as_deref().unwrap_or(name))
            == Some(&false)
            || self
                .unresolved_bindings
                .contains(name.split('.').next().unwrap_or(name))
            || self
                .project_context
                .unavailable_modules
                .iter()
                .any(|module| name == module || name.starts_with(&format!("{module}.")))
    }
    /// Only a registered virtual class is redirected. Ordinary classes can
    /// share its logical module and must still resolve through the real module.
    fn virtual_symbol(&mut self, name: &str) -> Option<String> {
        let mut qualified = name;
        while !self.project_context.virtual_class(qualified) {
            qualified = qualified.rsplit_once('.')?.0;
        }
        let (module, symbol) = qualified.rsplit_once('.')?;
        let physical = self.project_context.virtual_modules.get(module)?;
        let alias = if let Some(alias) = self.imported_virtual_modules.get(module) {
            alias.clone()
        } else {
            let alias = format!(
                "{}virtual_module_{}",
                self.prefix,
                self.imported_virtual_modules.len()
            );
            self.imports.insert(format!("import {physical} as {alias}"));
            self.imported_virtual_modules
                .insert(module.to_owned(), alias.clone());
            alias
        };
        Some(format!("{alias}.{symbol}{}", &name[qualified.len()..]))
    }
    fn module_alias(&mut self, module: &str) -> String {
        if let Some(alias) = self.imported_modules.get(module) {
            return alias.clone();
        }
        let alias = format!("{}module_{}", self.prefix, self.imported_modules.len());
        self.imports.insert(format!("import {module} as {alias}"));
        self.imported_modules
            .insert(module.to_owned(), alias.clone());
        alias
    }
    fn unknown(&mut self) -> String {
        format!("{}.Unknown", self.module_alias("ty_extensions._internal"))
    }
}

fn wire_fields_names(value: &Value) -> Vec<String> {
    value["fields"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|field| field["name"].as_str())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|name| format!("'{name}'"))
        .collect()
}

// Same quote/bracket boundaries as the pinned protocol expression decoder.
fn split_top_level(expression: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (offset, character) in expression.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == active_quote {
                quote = None;
            }
            continue;
        }

        match character {
            '\'' | '"' => quote = Some(character),
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth = depth.saturating_sub(1),
            character if character == delimiter && depth == 0 => {
                parts.push(expression[start..offset].trim());
                start = offset + character.len_utf8();
            }
            _ => {}
        }
    }

    parts.push(expression[start..].trim());
    parts
}

fn qualified_name(node: &Expr) -> Option<String> {
    match node {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attr) => Some(format!("{}.{}", qualified_name(&attr.value)?, attr.attr)),
        _ => None,
    }
}

/// Rewrite only TypeExpr nodes in a protocol response, using module absence
/// established by the official checker. This is also used for generated .pyi
/// definitions, before rendering their class/field/member structure.
pub fn replace_unavailable_types(value: &mut Value, modules: &BTreeSet<String>) -> Result<()> {
    match value {
        Value::Object(object)
            if object.get("expression").is_some_and(Value::is_string)
                && object.get("mode").is_some_and(Value::is_string) =>
        {
            let mut unavailable = modules.clone();
            if let Some(Value::Array(bindings)) = object.get_mut("imports") {
                bindings.retain(|binding| {
                    let module = binding["module"].as_str().unwrap_or("");
                    let absent = modules.iter().any(|prefix| {
                        module == prefix || module.starts_with(&format!("{prefix}."))
                    });
                    if absent
                        && let Some(alias) = binding["alias"]
                            .as_str()
                            .or_else(|| binding["name"].as_str())
                    {
                        unavailable.insert(alias.to_owned());
                    }
                    !absent
                });
            }
            let text = object["expression"].as_str().unwrap();
            let parsed = ruff_python_parser::parse_expression(text)
                .context("plugin type expression for availability")?;
            struct Replace<'a> {
                unavailable: &'a BTreeSet<String>,
                ranges: Vec<crate::model::Span>,
            }
            impl Replace<'_> {
                fn unavailable(&self, name: &str) -> bool {
                    self.unavailable
                        .iter()
                        .any(|prefix| name == prefix || name.starts_with(&format!("{prefix}.")))
                }
            }
            impl<'a> Visitor<'a> for Replace<'_> {
                fn visit_expr(&mut self, node: &'a Expr) {
                    if let Expr::Subscript(subscript) = node
                        && qualified_name(&subscript.value)
                            .is_some_and(|name| self.unavailable(&name))
                    {
                        self.ranges.push(span(node));
                        return;
                    }
                    if qualified_name(node).is_some_and(|name| self.unavailable(&name)) {
                        self.ranges.push(span(node));
                        return;
                    }
                    visitor::walk_expr(self, node);
                }
            }
            let mut replace = Replace {
                unavailable: &unavailable,
                ranges: vec![],
            };
            replace.visit_expr(&parsed.syntax().body);
            let mut text = text.to_owned();
            replace.ranges.sort_by_key(|range| range.start);
            for range in replace.ranges.into_iter().rev() {
                text.replace_range(range.start..range.end, "ty_extensions._internal.Unknown");
            }
            object.insert("expression".to_owned(), Value::String(text));
        }
        Value::Object(object) => {
            for child in object.values_mut() {
                replace_unavailable_types(child, modules)?;
            }
        }
        Value::Array(values) => {
            for child in values {
                replace_unavailable_types(child, modules)?;
            }
        }
        _ => {}
    }
    Ok(())
}
pub(crate) fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
        && !matches!(
            name,
            "class"
                | "def"
                | "return"
                | "if"
                | "else"
                | "for"
                | "in"
                | "is"
                | "None"
                | "True"
                | "False"
                | "import"
                | "from"
                | "lambda"
                | "async"
                | "await"
                | "with"
                | "try"
                | "except"
                | "finally"
                | "while"
                | "break"
                | "continue"
                | "pass"
                | "raise"
                | "yield"
                | "del"
                | "global"
                | "nonlocal"
                | "assert"
                | "and"
                | "or"
                | "not"
                | "as"
                | "elif"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn qualification_does_not_rewrite_literals_or_name_substrings() {
        let mut types = Types::new("_generated_", "app.models");
        let rendered = types
            .text(
                "tuple[app.models.Book, typing.Literal['app.models.Book'], other.app.models.Book]",
            )
            .unwrap();
        assert_eq!(
            rendered,
            format!(
                "{}.tuple[{}.Book, {}.Literal['app.models.Book'], {}.Book]",
                types.imported_modules["builtins"],
                types.imported_modules["app.models"],
                types.imported_modules["typing"],
                types.imported_modules["other.app.models"]
            )
        );
        assert!(
            types
                .imports
                .iter()
                .any(|import| import.starts_with("import other.app.models as _generated_module_"))
        );
    }
    #[test]
    fn structural_types_are_recursive_closed_and_support_nonidentifier_keys() {
        let mut types = Types::new("_generated_", "models");
        let value = types
            .text("list[TypedDict({'x-y': NamedTuple('Pair', {'value': models.Book})})]")
            .unwrap();
        assert_eq!(
            value,
            format!(
                "{}.list[_generated_type_0]",
                types.imported_modules["builtins"]
            )
        );
        assert!(types.definitions.contains(&format!(
            "class _generated_type_1({}.NamedTuple):",
            types.imported_modules["typing"]
        )));
        assert!(
            types
                .definitions
                .contains(&format!("value: {}.Book", types.imported_modules["models"]))
        );
        assert!(types.definitions.contains("\"x-y\": _generated_type_1"));
        assert!(types.definitions.contains("closed=True"));
        ruff_python_parser::parse_module(&types.prelude()).unwrap();
    }
    #[test]
    fn known_module_paths_can_contain_uppercase_segments() {
        let mut types = Types::new("_generated_", "models");
        types
            .modules
            .insert("package.generated.models.Book".to_owned());
        assert_eq!(
            types.text("package.generated.models.Book.Manager").unwrap(),
            "_generated_module_0.Manager"
        );
        assert!(
            types
                .imports
                .contains("import package.generated.models.Book as _generated_module_0")
        );
        assert!(!types.imports.contains("import package.generated.models"));
    }

    #[test]
    fn renderer_project_context_shares_storage_and_preserves_old_revisions() -> Result<()> {
        let directory =
            std::env::temp_dir().join(format!("native-renderer-context-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("models.py");
        std::fs::write(&path, "class Model: pass\n")?;
        let mut project = Project::load(
            &directory,
            std::slice::from_ref(&path),
            &serde_json::json!({}),
            serde_json::json!({}),
        )?;
        let virtual_types = vec![serde_json::json!({
            "name":"Package.generated.Row", "shape":{"kind":"class","bases":[]}
        })];
        project.register_virtual_types(&virtual_types)?;
        Arc::make_mut(&mut project.snapshot_nominals).insert("models.Model".into(), Some(0));
        Arc::make_mut(&mut project.snapshot_symbols).insert("models.Model".into(), true);
        let module_owners = Arc::strong_count(&project.modules);
        let class_owners = Arc::strong_count(&project.classes);
        let mut original = Types::with_project("_generated_", "models", &project);
        assert!(Arc::ptr_eq(
            &original.project_context.modules,
            &project.renderer_module_names()
        ));
        assert!(Arc::ptr_eq(
            &original.project_context.virtual_classes,
            &project.renderer_virtual_class_names()
        ));
        // Cached unchanged units can retain renderers from older revisions.
        // Neither creating nor cloning one may retain the full syntax/summary
        // maps, which would accumulate one project map per changed file.
        let retained = original.clone();
        assert_eq!(Arc::strong_count(&project.modules), module_owners);
        assert_eq!(Arc::strong_count(&project.classes), class_owners);
        assert!(Arc::ptr_eq(
            &retained.project_context.modules,
            &original.project_context.modules
        ));
        drop(retained);
        assert!(Arc::ptr_eq(
            &original.project_context.nominal_resolution,
            &project.snapshot_nominals
        ));
        assert_eq!(
            original.known_module("Package.generated.Other"),
            Some("Package.generated")
        );
        let original_virtual = original.text("Package.generated.Row")?;

        Arc::make_mut(&mut project.unavailable_modules).insert("absent".into());
        Arc::make_mut(&mut project.snapshot_nominals).insert("models.Model".into(), None);
        Arc::make_mut(&mut project.snapshot_symbols).insert("models.Model".into(), false);
        let nested = directory.join("Nested.pyi");
        std::fs::write(&nested, "class Item: pass\n")?;
        project.add_resolved_modules(&[("Package.generated.Nested".into(), nested)])?;
        project.register_virtual_types(&virtual_types)?;
        Arc::make_mut(&mut project.virtual_modules)
            .insert("Package.generated".into(), "_replacement".into());
        let mut current = Types::with_project("_generated_", "models", &project);

        assert!(
            !original
                .project_context
                .unavailable_modules
                .contains("absent")
        );
        assert!(
            current
                .project_context
                .unavailable_modules
                .contains("absent")
        );
        assert_eq!(
            original.project_context.nominal_resolution["models.Model"],
            Some(0)
        );
        assert_eq!(
            current.project_context.nominal_resolution["models.Model"],
            None
        );
        assert!(!original.annotation_missing("models.Model"));
        assert!(current.annotation_missing("models.Model"));
        assert_eq!(
            original.known_module("Package.generated.Nested.Item"),
            Some("Package.generated")
        );
        assert_eq!(
            current.known_module("Package.generated.Nested.Item"),
            Some("Package.generated.Nested")
        );
        assert_eq!(original.text("Package.generated.Row")?, original_virtual);
        current.text("Package.generated.Row")?;
        assert!(
            current
                .imports
                .iter()
                .any(|import| import.starts_with("import _replacement as "))
        );
        assert!(
            original
                .imports
                .iter()
                .all(|import| !import.starts_with("import _replacement as "))
        );

        // Local explicit modules are independent of the shared project map.
        current.add_module("Package.generated.Nested.Item");
        assert_eq!(
            current.known_module("Package.generated.Nested.Item.Child"),
            Some("Package.generated.Nested.Item")
        );
        assert_eq!(
            original.known_module("Package.generated.Nested.Item.Child"),
            Some("Package.generated")
        );
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn shared_virtual_namespaces_preserve_dotted_class_rules() {
        let mut types = Types::new("_generated_", "models");
        Arc::make_mut(&mut types.project_context.virtual_classes)
            .extend(["Bare".into(), "Package.Inner.Row".into()]);
        Arc::make_mut(&mut types.project_context.virtual_modules)
            .insert("_ty_ext_virtual".into(), "_generated_bare".into());
        assert!(!types.project_context.virtual_class("Bare"));
        assert!(!types.project_context.virtual_module("_ty_ext_virtual"));
        assert!(!types.project_context.virtual_module("ordinary"));
        assert!(!types.project_context.virtual_module("Package"));
        assert!(types.project_context.virtual_module("Package.Inner"));
        assert_eq!(
            types.known_module("Package.Inner.Other"),
            Some("Package.Inner")
        );
    }
    #[test]
    fn explicit_import_aliases_are_not_mistaken_for_module_names() {
        let mut types = Types::new("_generated_", "models");
        let rendered=types.render(&serde_json::json!({"expression":"m.Row","mode":"annotation","imports":[{"module":"vendor","name":"models","alias":"m"}]})).unwrap();
        assert_eq!(rendered, "_generated_symbol_0.Row");
        assert!(
            types
                .imports
                .contains("from vendor import models as _generated_symbol_0")
        );
        assert!(!types.imports.contains("import m"));
        let other = types.render(&serde_json::json!({"expression":"m.Row","mode":"annotation","imports":[{"module":"different","name":"models","alias":"m"}]})).unwrap();
        assert_eq!(other, "_generated_symbol_1.Row");
        assert_eq!(
            types.text("m").unwrap(),
            "m",
            "TypeExpr bindings must not leak to later expressions"
        );
    }

    #[test]
    fn unavailable_type_fallback_requires_checker_evidence_and_preserves_literals() {
        let mut types = Types::new("_generated_", "models");
        assert_eq!(
            types.text("external.Widget").unwrap(),
            "_generated_module_0.Widget"
        );
        Arc::make_mut(&mut types.project_context.unavailable_modules).insert("absent".to_owned());
        let rendered = types
            .text("tuple[absent.Row, typing.Literal['absent.Row'], absent.Container[int]]")
            .unwrap();
        assert_eq!(
            rendered,
            format!(
                "{2}.tuple[{0}.Unknown, {1}.Literal['absent.Row'], {0}.Unknown]",
                types.imported_modules["ty_extensions._internal"],
                types.imported_modules["typing"],
                types.imported_modules["builtins"]
            )
        );
        assert!(!types.imports.contains("import absent"));
        assert!(
            types
                .imports
                .iter()
                .any(|import| import.starts_with("import ty_extensions._internal as "))
        );
        let mut definition = serde_json::json!({"shape":{"kind":"class","bases":[{"expression":"available.Base[absent.Row]","mode":"annotation"}],"members":[{"name":"item","access":{"kind":"value","type-expr":{"expression":"Alias","mode":"annotation","imports":[{"module":"absent","name":"Item","alias":"Alias"}]}}}]}});
        replace_unavailable_types(&mut definition, &BTreeSet::from(["absent".to_owned()])).unwrap();
        assert_eq!(
            definition["shape"]["bases"][0]["expression"],
            "available.Base[ty_extensions._internal.Unknown]"
        );
        assert_eq!(
            definition["shape"]["members"][0]["access"]["type-expr"]["expression"],
            "ty_extensions._internal.Unknown"
        );
    }
}
