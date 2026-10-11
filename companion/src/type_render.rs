//! Lower protocol type constructors into ordinary checker-visible declarations.
use crate::model::span;
use crate::project::Project;
use anyhow::{Context, Result, bail};
use ruff_python_ast::{
    Expr,
    visitor::{self, Visitor},
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub struct Types {
    pub imports: BTreeSet<String>,
    pub definitions: String,
    prefix: String,
    module: String,
    unimportable_module: bool,
    aliases: BTreeMap<String, String>,
    modules: BTreeSet<String>,
    import_bindings: BTreeMap<String, String>,
    imported_symbols: BTreeMap<(String, String), String>,
    imported_modules: BTreeMap<String, String>,
    unavailable_modules: BTreeSet<String>,
    unresolved_bindings: BTreeSet<String>,
}
impl Types {
    pub fn new(prefix: &str, module: &str) -> Self {
        Self {
            imports: BTreeSet::new(),
            definitions: String::new(),
            prefix: prefix.into(),
            module: module.into(),
            unimportable_module: false,
            aliases: BTreeMap::new(),
            modules: BTreeSet::from([module.to_owned()]),
            import_bindings: BTreeMap::new(),
            imported_symbols: BTreeMap::new(),
            imported_modules: BTreeMap::new(),
            unavailable_modules: BTreeSet::new(),
            unresolved_bindings: BTreeSet::new(),
        }
    }
    pub fn with_project(prefix: &str, module: &str, project: &Project) -> Self {
        let mut renderer = Self::new(prefix, module);
        renderer.unimportable_module = project
            .modules
            .get(module)
            .is_some_and(|module| module.source.protocol_module.is_some());
        renderer.modules.extend(project.modules.keys().cloned());
        renderer.unavailable_modules = project.unavailable_modules.clone();
        for (qualified, summary) in &project.classes {
            if summary["virtual"] == true
                && let Some((module, _)) = qualified.rsplit_once('.')
            {
                renderer.modules.insert(module.to_owned());
            }
        }
        renderer
    }
    pub fn add_module(&mut self, module: &str) {
        self.modules.insert(module.to_owned());
    }
    pub fn display_names(&self) -> BTreeMap<String, String> {
        self.imported_modules
            .iter()
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
            .collect()
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
        let previous_bindings = std::mem::take(&mut self.import_bindings);
        let previous_unresolved = std::mem::take(&mut self.unresolved_bindings);
        for import in value["imports"].as_array().into_iter().flatten() {
            let module = import["module"].as_str().context("type import module")?;
            let name = import["name"].as_str().context("type import name")?;
            let alias = import["alias"].as_str().unwrap_or(name);
            if self.unavailable_modules.iter().any(|unresolved| {
                module == unresolved || module.starts_with(&format!("{unresolved}."))
            }) {
                self.unresolved_bindings.insert(alias.to_owned());
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
    pub fn text(&mut self, text: &str) -> Result<String> {
        let parsed = ruff_python_parser::parse_expression(text)
            .with_context(|| format!("plugin type expression {text}"))?;
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
            return Ok(alias);
        }
        // Resolve the complete qualified name before traversing Attribute's
        // shorter children. Strings are never edited, including Literal values.
        if let Some(name) = qualified_name(node) {
            if self.unimportable_module
                && let Some(local) = name.strip_prefix(&format!("{}.", self.module))
            {
                return Ok(local.to_owned());
            }
            if self.unresolved(&name) {
                return Ok(self.unknown());
            }
            let head = name.split('.').next().unwrap();
            if let Some(generated) = self.import_bindings.get(head) {
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
            if name.contains('.') {
                if let Some(module) = self
                    .modules
                    .iter()
                    .filter(|module| name.starts_with(&format!("{module}.")))
                    .max_by_key(|module| module.len())
                    .cloned()
                {
                    let alias = self.module_alias(&module);
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
        self.unresolved_bindings
            .contains(name.split('.').next().unwrap_or(name))
            || self
                .unavailable_modules
                .iter()
                .any(|module| name == module || name.starts_with(&format!("{module}.")))
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
        types.unavailable_modules.insert("absent".to_owned());
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
