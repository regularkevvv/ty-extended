use crate::model::{CallSite, Oracle, Source, typ};
use crate::project::Project;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use ty_plugin_host::{MontyLimits, MontyRunner, PluginEnvironment, PluginHost};
use ty_plugin_protocol::{PluginManifest, PluginRequest, method_name_pattern_matches};

pub struct Runtime {
    host: PluginHost<MontyRunner>,
    id: String,
    pub manifest: Value,
    pub project: Option<Arc<Project>>,
    pub project_index: Option<Value>,
    pub config: Value,
    pub python_version: String,
    pub project_patches: BTreeMap<String, Value>,
    pub project_contributions: Vec<Value>,
    cache: BTreeMap<String, Value>,
    pub executions: usize,
    pub hits: usize,
    pub execution_time: Duration,
}
impl Runtime {
    pub fn new(manifest_path: &Path, plugin_path: &Path) -> Result<Self> {
        let data = std::fs::read(manifest_path)?;
        let manifest_value: Value = serde_json::from_slice(&data)?;
        let manifest: PluginManifest = serde_json::from_slice(&data)?;
        let id = manifest.id.clone();
        let environment = PluginEnvironment::from_manifests(vec![manifest])?;
        let mut runner = MontyRunner::new(MontyLimits::default())?;
        runner.add_plugin(&id, std::fs::read(plugin_path)?)?;
        Ok(Self {
            host: PluginHost::new(environment, runner),
            id,
            config: manifest_value
                .get("default-config")
                .cloned()
                .unwrap_or(json!({})),
            python_version: "3.13".into(),
            manifest: manifest_value,
            project: None,
            project_index: None,
            project_patches: BTreeMap::new(),
            project_contributions: Vec::new(),
            cache: BTreeMap::new(),
            executions: 0,
            hits: 0,
            execution_time: Duration::ZERO,
        })
    }

    pub fn attach_project(&mut self, project: Arc<Project>, config: Value) {
        self.project = Some(project);
        if let (Some(defaults), Some(overrides)) = (self.config.as_object_mut(), config.as_object())
        {
            defaults.extend(overrides.clone());
        } else if !config.is_null() {
            self.config = config;
        }
        self.cache.clear();
    }

    pub fn canonical_name(&self, source: &Source, name: &str) -> String {
        self.project.as_ref().map_or_else(
            || name.to_string(),
            |project| {
                let qualified = project.canonical_name(&source.module, name);
                let (nominal, _) = generic_parts(&qualified);
                // Display forms such as Literal[1] are checker/typing syntax,
                // not evidence for an unrelated user class called Literal.
                if matches!(
                    nominal,
                    "Literal"
                        | "Self"
                        | "Any"
                        | "Unknown"
                        | "Never"
                        | "NoReturn"
                        | "Callable"
                        | "ClassVar"
                        | "Final"
                        | "Optional"
                        | "Union"
                        | "Type"
                        | "Tuple"
                        | "List"
                        | "Dict"
                        | "Set"
                        | "FrozenSet"
                        | "Annotated"
                        | "TypeGuard"
                        | "TypeIs"
                        | "Unpack"
                        | "Required"
                        | "NotRequired"
                        | "ReadOnly"
                        | "Concatenate"
                        | "TypeVar"
                        | "ParamSpec"
                        | "TypeVarTuple"
                        | "Generic"
                        | "Protocol"
                        | "TypeAlias"
                        | "TypeAliasType"
                        | "LiteralString"
                        | "object"
                        | "type"
                        | "str"
                        | "int"
                        | "bool"
                        | "float"
                        | "complex"
                        | "bytes"
                        | "bytearray"
                        | "tuple"
                        | "list"
                        | "dict"
                        | "set"
                        | "frozenset"
                        | "None"
                ) {
                    return qualified;
                }
                if !nominal.contains('.')
                    && nominal
                        .chars()
                        .all(|character| character.is_alphanumeric() || character == '_')
                {
                    let mut matches = project
                        .classes
                        .keys()
                        .filter(|candidate| candidate.rsplit('.').next() == Some(nominal));
                    if let Some(candidate) = matches.next()
                        && matches.next().is_none()
                    {
                        return format!("{candidate}{}", &qualified[nominal.len()..]);
                    }
                }
                qualified
            },
        )
    }

    pub fn member_type(
        &self,
        source: &Source,
        owner: &str,
        member: &str,
        class_access: bool,
    ) -> Option<Value> {
        fn access_type(access: &Value, class_access: bool) -> Option<Value> {
            match access["kind"].as_str().unwrap_or("value") {
                "value" => access.get("type-expr").cloned(),
                "descriptor" => access
                    .get(if class_access {
                        "class-type"
                    } else {
                        "instance-get-type"
                    })
                    .cloned(),
                _ => None,
            }
        }
        fn patch_type(patch: &Value, class_access: bool) -> Option<Value> {
            if patch.get("instance-get-type").is_some() {
                if class_access {
                    patch
                        .get("descriptor")
                        .and_then(|descriptor| access_type(descriptor, true))
                } else {
                    patch.get("instance-get-type").cloned()
                }
            } else {
                access_type(&patch["access"], class_access)
            }
        }
        let _ = source;
        let (nominal, arguments) = generic_parts(owner);
        let nominal = if matches!(nominal, "type" | "builtins.type") {
            arguments.first().copied().unwrap_or(nominal)
        } else {
            nominal
        };
        let (nominal, _) = generic_parts(nominal);
        let mro = self
            .project
            .as_ref()
            .and_then(|project| project.mro(nominal))
            .unwrap_or_else(|| vec![nominal.to_string()]);
        for owner in mro {
            if let Some(patch) = self.project_patches.get(&owner) {
                for field in patch["fields"].as_array().into_iter().flatten() {
                    if field["name"] == member
                        && let Some(value) = patch_type(field, class_access)
                    {
                        return Some(value);
                    }
                }
                for field in patch[if class_access {
                    "class-members"
                } else {
                    "instance-members"
                }]
                .as_array()
                .into_iter()
                .flatten()
                {
                    if field["name"] == member
                        && let Some(value) = access_type(&field["access"], class_access)
                    {
                        return Some(value);
                    }
                }
            }
            for contribution in self.project_contributions.iter().rev() {
                if contribution["target"]["qualified-name"] != owner
                    || contribution["patch"]["name"] != member
                {
                    continue;
                }
                if contribution["target"]["kind"] == "class" && !class_access {
                    continue;
                }
                if let Some(value) = patch_type(&contribution["patch"], class_access) {
                    return Some(value);
                }
            }
            if let Some(mut declared) = self
                .project
                .as_ref()
                .and_then(|project| project.declared_member_type(&owner, member))
                && let Some(expression) = declared["expression"].as_str()
            {
                let (outer, arguments) = generic_parts(expression);
                let expression = if matches!(
                    outer,
                    "ClassVar" | "typing.ClassVar" | "typing_extensions.ClassVar"
                ) {
                    arguments.first().copied().unwrap_or(expression)
                } else {
                    expression
                };
                declared["expression"] =
                    json!(crate::project::substitute_self(expression, nominal));
                return Some(declared);
            }
        }
        None
    }

    /// Recover a protocol type from a known class/member patch before consulting
    /// the checker's deliberately abbreviated display. This is generic patch
    /// provenance, not an inference rule for a particular plugin or framework.
    pub fn expression_type(
        &self,
        source: &Source,
        expression: &ruff_python_ast::Expr,
    ) -> Option<Value> {
        use crate::model::span;
        use ruff_python_ast::Expr;
        match expression {
            Expr::Name(_) | Expr::Attribute(_) => {
                let symbol = self.canonical_name(source, source.slice(span(expression)));
                if self
                    .project
                    .as_ref()
                    .is_some_and(|project| project.class_summary(&symbol).is_some())
                {
                    return Some(typ(format!("type[{symbol}]")));
                }
                if let Expr::Attribute(attribute) = expression {
                    let parent = self.expression_type(source, &attribute.value)?;
                    let parent = parent["expression"].as_str()?;
                    let (nominal, _) = generic_parts(parent);
                    return self.member_type(
                        source,
                        parent,
                        attribute.attr.as_str(),
                        matches!(nominal, "type" | "builtins.type"),
                    );
                }
                None
            }
            Expr::Call(call) => {
                let symbol = self.canonical_name(source, source.slice(span(call.func.as_ref())));
                self.project
                    .as_ref()
                    .and_then(|project| project.default_constructor_result(&symbol))
                    .map(typ)
            }
            _ => None,
        }
    }

    pub fn type_expr(&self, source: &Source, expression: &str) -> Value {
        // Display spelling and nominal identity are independent SDK fields.
        // Plugins may inspect either: keep the checker spelling while resolving
        // imported classes for the structural snapshot.
        if expression.starts_with("Self@") || expression == "Self" {
            return json!({"expression":"Self","mode":"annotation","snapshot":{"kind":"self-type"}});
        }
        let display = self.project.as_ref().map_or_else(
            || expression.to_owned(),
            |project| project.rewrite_virtual_identities(expression),
        );
        let qualified = self.canonical_name(source, expression);
        self.type_expr_with_identity(display, &qualified)
    }

    /// Types obtained from protocol metadata and established flow identities
    /// are already qualified. Source aliases must not be applied a second time.
    pub fn qualified_type_expr(&self, expression: &str) -> Value {
        self.type_expr_with_identity(expression.to_owned(), expression)
    }

    fn type_expr_with_identity(&self, display: String, qualified: &str) -> Value {
        let mut value = typ(display);
        let (nominal, arguments) = generic_parts(qualified);
        let qualified_nominal = match nominal {
            "str" | "int" | "bool" | "float" | "complex" | "bytes" | "bytearray" | "list"
            | "dict" | "set" | "frozenset" | "object" => Some(format!("builtins.{nominal}")),
            _ if nominal.contains('.')
                && !nominal.starts_with("typing.")
                && !nominal.starts_with("typing_extensions.")
                && nominal.chars().all(|character| {
                    character.is_alphanumeric() || matches!(character, '_' | '.')
                }) =>
            {
                Some(nominal.to_owned())
            }
            _ => None,
        };
        if let Some(nominal) = qualified_nominal {
            let snapshots = arguments
                .into_iter()
                .map(|argument| {
                    let argument = self.qualified_type_expr(argument);
                    argument.get("snapshot").cloned().unwrap_or_else(|| {
                        json!({"kind":"expression","expression":argument["expression"],"mode":"annotation"})
                    })
                })
                .collect::<Vec<_>>();
            value["snapshot"] =
                json!({"kind":"nominal","qualified-name":nominal,"arguments":snapshots});
        }
        value
    }

    /// A Self result must retain its bound when it becomes the next receiver.
    /// The display expression is still Self and must not be used as a class key.
    pub fn receiver_identity(value: &Value) -> Option<String> {
        let snapshot = &value["snapshot"];
        if snapshot["kind"] == "self-type" {
            let bound = &snapshot["bound"];
            if let Some(expression) = crate::project::type_expression(bound) {
                return Some(expression);
            }
            if let Some(nominal) = bound["qualified-name"].as_str() {
                return Some(nominal.to_owned());
            }
        }
        crate::project::type_expression(value)
    }

    fn nominal_name(&self, source: &Source, expression: &str) -> String {
        let (nominal, arguments) = generic_parts(expression);
        let nominal = if matches!(nominal, "type" | "builtins.type") {
            arguments.first().map_or_else(
                || nominal.to_owned(),
                |inner| generic_parts(inner).0.to_owned(),
            )
        } else {
            nominal.to_owned()
        };
        if self.project.is_none() && !nominal.contains('.') {
            format!("{}.{}", source.module, nominal)
        } else {
            nominal
        }
    }

    fn subclass(&self, class: &str, base: &str) -> bool {
        class == base
            || self
                .project
                .as_ref()
                .is_some_and(|p| p.is_subclass(class, base))
    }

    fn class_claim(&self, claim: &Value, qualified: &str) -> bool {
        match claim["kind"].as_str().unwrap_or("exact") {
            "exact" => claim["qualified-name"] == qualified,
            "subclass-of" => claim["base-qualified-name"]
                .as_str()
                .is_some_and(|base| self.subclass(qualified, base)),
            _ => false,
        }
    }

    /// Resolve the function actually defined in the receiver's MRO. State
    /// hooks must not refine an overriding method merely because its receiver
    /// is a subclass of a claimed class.
    pub fn method_callee(&self, owner: &str, method: &str) -> String {
        let (owner, _) = generic_parts(owner);
        self.project
            .as_ref()
            .and_then(|project| project.method_owner(owner, method))
            .map(|owner| format!("{owner}.{method}"))
            .unwrap_or_else(|| format!("{owner}.{method}"))
    }

    #[cfg(test)]
    pub fn receiver_summary(&self, source: &Source, expression: &str) -> Value {
        let qualified = self.canonical_name(source, expression);
        self.qualified_receiver_summary(source, &qualified)
    }

    fn qualified_receiver_summary(&self, source: &Source, qualified: &str) -> Value {
        let (outer, outer_arguments) = generic_parts(qualified);
        let summary_type = if matches!(outer, "type" | "builtins.type") {
            outer_arguments.first().copied().unwrap_or(qualified)
        } else {
            qualified
        };
        let (nominal, arguments) = generic_parts(summary_type);
        // A nongeneric subclass can inherit a concrete generic specialization.
        // This mirrors the SDK receiver-summary contract; the original receiver
        // expression remains available independently of the summary class.
        if arguments.is_empty()
            && let Some(project) = &self.project
            && let Some((base, args)) = generic_base(project, nominal, &mut BTreeSet::new())
        {
            return json!({"type-expr":typ(qualified),"nominal-class":base,
                        "generic-arguments":args.into_iter().map(typ).collect::<Vec<_>>(),"plugin-metadata":null});
        }
        let nominal = if self.project.is_none() && !nominal.contains('.') {
            format!("{}.{}", source.module, nominal)
        } else {
            nominal.to_string()
        };
        json!({"type-expr":typ(qualified),"nominal-class":nominal,
            "generic-arguments":arguments.iter().map(|arg| typ(*arg)).collect::<Vec<_>>(),"plugin-metadata":null})
    }

    pub fn nominal_receiver(&self, source: &Source, expression: &str) -> bool {
        let _ = source;
        let (nominal, arguments) = generic_parts(expression);
        if matches!(nominal, "type" | "builtins.type") {
            return arguments
                .first()
                .is_some_and(|inner| self.nominal_receiver(source, inner));
        }
        if matches!(
            nominal,
            "Unknown"
                | "Any"
                | "typing.Any"
                | "@Todo"
                | "None"
                | "Never"
                | "Module"
                | "ModuleType"
                | "types.ModuleType"
                | "Literal"
                | "typing.Literal"
        ) {
            return false;
        }
        if !nominal
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '.'))
        {
            return false;
        }
        true
    }

    pub fn default_member_type(&self, owner: &str, member: &str) -> Option<String> {
        self.default_member_expr(owner, member)
            .and_then(|value| crate::project::type_expression(&value))
    }

    pub fn default_member_expr(&self, owner: &str, member: &str) -> Option<Value> {
        let (owner, _) = generic_parts(owner);
        if let Some(patch) = self.project_patches.get(owner) {
            for field in patch["fields"].as_array().into_iter().flatten() {
                if field["name"] == member {
                    return field["instance-get-type"]["expression"]
                        .as_str()
                        .map(|_| field["instance-get-type"].clone());
                }
            }
            for field in patch["instance-members"].as_array().into_iter().flatten() {
                if field["name"] == member {
                    return field["access"]["type-expr"]["expression"]
                        .as_str()
                        .map(|_| field["access"]["type-expr"].clone());
                }
            }
        }
        for contribution in self.project_contributions.iter().rev() {
            if contribution["target"]["qualified-name"] == owner
                && contribution["patch"]["name"] == member
            {
                let patch = &contribution["patch"];
                if let Some(value) = patch
                    .get("instance-get-type")
                    .or_else(|| patch["access"].get("type-expr"))
                {
                    return Some(value.clone());
                }
            }
        }
        self.project
            .as_ref()
            .and_then(|project| project.class_summary(owner))
            .and_then(|class| class["fields"].as_array())
            .and_then(|fields| fields.iter().find(|field| field["name"] == member))
            .and_then(|field| field.get("annotation"))
            .cloned()
    }

    fn allowed(&self, source: &Source, kind: &str, payload: &Value) -> bool {
        let capability = match kind {
            "build-project-index" => "project-index",
            "analyze-class" => "class-transform",
            "resolve-class-member" => "class-member",
            "resolve-instance-member" => "instance-member",
            "adjust-call-signature" => "call-signature",
            "adjust-call-return" => "call-return",
            "adjust-call-state" => "call-state",
            "validate-mutation" => "mutation-validation",
            "additional-dependencies" => "additional-dependencies",
            _ => return false,
        };
        if self.manifest["capabilities"][capability] != true {
            return false;
        }
        let claims = &self.manifest["claims"];
        let list = |key: &str| claims[key].as_array().map(Vec::as_slice).unwrap_or(&[]);
        match kind {
            "build-project-index" | "additional-dependencies" => true,
            "analyze-class" => payload["class"]["qualified-name"]
                .as_str()
                .is_some_and(|name| {
                    list("classes")
                        .iter()
                        .any(|claim| self.class_claim(claim, name))
                }),
            "adjust-call-signature" | "adjust-call-return" | "adjust-call-state" => {
                let callee = payload["callee"]["expression"].as_str().unwrap_or("");
                if list("functions")
                    .iter()
                    .any(|claim| claim["qualified-name"] == callee)
                {
                    return true;
                }
                // Constructors are state claims in the existing SDK. Ordinary
                // constructor signatures are returned by class transforms.
                if kind == "adjust-call-state"
                    && payload.get("receiver").is_none()
                    && list("constructors")
                        .iter()
                        .any(|claim| self.class_claim(claim, callee))
                {
                    return true;
                }
                let method = callee.rsplit('.').next().unwrap_or(callee);
                let receiver = payload["receiver"]["nominal-class"].as_str();
                list("methods").iter().any(|claim| {
                    match claim["kind"].as_str().unwrap_or("exact") {
                        "exact" => {
                            format!(
                                "{}.{}",
                                claim["class-qualified-name"].as_str().unwrap_or(""),
                                claim["method-name"].as_str().unwrap_or("")
                            ) == callee
                        }
                        "on-subclass-of" | "on-subclass-of-matching" => {
                            let Some(base) = claim["base-qualified-name"].as_str() else {
                                return false;
                            };
                            let matches = if claim["kind"] == "on-subclass-of-matching" {
                                claim["method-name-pattern"]
                                    .as_str()
                                    .is_some_and(|pattern| {
                                        method_name_pattern_matches(pattern, method)
                                    })
                            } else {
                                claim["method-name"] == method
                            };
                            matches
                                && receiver.is_some_and(|owner| self.subclass(owner, base))
                                && (kind != "adjust-call-state"
                                    || callee == format!("{base}.{method}"))
                        }
                        _ => false,
                    }
                })
            }
            "resolve-instance-member" | "resolve-class-member" => {
                let expression = payload["owner"]["expression"].as_str().unwrap_or("");
                let owner = self.nominal_name(source, expression);
                let scope = if kind == "resolve-class-member" {
                    "class"
                } else {
                    "instance"
                };
                list("attributes").iter().any(|claim| {
                    claim["scope"] == scope
                        && match claim["kind"].as_str().unwrap_or("exact") {
                            "exact" => {
                                claim["owner-qualified-name"] == owner
                                    && claim["attribute-name"] == payload["member-name"]
                            }
                            "on-subclass-of" => claim["owner-base-qualified-name"]
                                .as_str()
                                .is_some_and(|base| self.subclass(&owner, base)),
                            // Contribution targets authorize project-index patches,
                            // not a direct member hook invocation.
                            _ => false,
                        }
                })
            }
            "validate-mutation" => {
                let expression = payload["receiver"]["expression"].as_str().unwrap_or("");
                let owner = self.nominal_name(source, expression);
                list("mutations")
                    .iter()
                    .any(|claim| self.class_claim(claim, &owner))
            }
            _ => false,
        }
    }

    fn dispatch(&mut self, payload: Value) -> Result<Value> {
        let key = serde_json::to_string(&payload)?;
        if let Some(value) = self.cache.get(&key) {
            self.hits += 1;
            return Ok(value.clone());
        }
        let request: PluginRequest =
            serde_json::from_value(payload).context("invalid plugin request")?;
        let start = Instant::now();
        let response = serde_json::to_value(self.host.execute(&self.id, &request)?)?;
        self.execution_time += start.elapsed();
        self.executions += 1;
        if response["kind"] == "error" {
            bail!("plugin error: {response}");
        }
        self.cache.insert(key, response.clone());
        Ok(response)
    }

    pub fn execute_project(&mut self, kind: &str, mut payload: Value) -> Result<Value> {
        let capability = match kind {
            "build-project-index" => "project-index",
            "additional-dependencies" => "additional-dependencies",
            _ => bail!("not a project hook: {kind}"),
        };
        if self.manifest["capabilities"][capability] != true {
            return Ok(json!({"kind":"no-change"}));
        }
        payload["kind"] = json!(kind);
        if kind == "build-project-index" {
            payload["context"]["config"] = self.config.clone();
        } else {
            payload["config"] = self.config.clone();
        }
        self.dispatch(payload)
    }

    pub fn execute(&mut self, source: &Source, kind: &str, mut payload: Value) -> Result<Value> {
        if !self.allowed(source, kind, &payload) {
            return Ok(json!({"kind":"no-change"}));
        }
        let object = payload
            .as_object_mut()
            .context("hook payload must be object")?;
        object.insert("kind".into(), json!(kind));
        object.insert(
            "context".into(),
            json!({"module":source.module,"file-path":source.path,
            "python-version":self.python_version,"platform":"darwin","config":self.config,"speculative":false}),
        );
        if let Some(index) = &self.project_index {
            object.insert("project-index".into(), index.clone());
        }
        self.dispatch(payload)
    }
}

fn generic_base(
    project: &Project,
    owner: &str,
    seen: &mut BTreeSet<String>,
) -> Option<(String, Vec<String>)> {
    if !seen.insert(owner.to_owned()) {
        return None;
    }
    let class = project.class_summary(owner)?;
    for base in class["bases"].as_array().into_iter().flatten() {
        let expression = base["expression"].as_str()?;
        let (nominal, args) = generic_parts(expression);
        if !args.is_empty()
            && !matches!(
                nominal,
                "typing.Generic" | "typing.Protocol" | "Generic" | "Protocol"
            )
        {
            let module = class["source"]["module"]
                .as_str()
                .unwrap_or_else(|| owner.rsplit_once('.').map_or("", |(module, _)| module));
            return Some((
                nominal.to_string(),
                args.iter()
                    .map(|arg| {
                        if let Ok(parsed) = ruff_python_parser::parse_expression(arg)
                            && let ruff_python_ast::Expr::StringLiteral(literal) =
                                parsed.syntax().body.as_ref()
                        {
                            return project.canonical_name(module, literal.value.to_str());
                        }
                        (*arg).to_owned()
                    })
                    .collect(),
            ));
        }
        if let Some(found) = generic_base(project, nominal, seen) {
            return Some(found);
        }
    }
    None
}

/// Split only outer generic arguments, preserving nested tuples, unions, and
/// quoted Literal values. A display expression that is not nominal stays whole.
pub fn generic_parts(expression: &str) -> (&str, Vec<&str>) {
    let expression = expression.trim();
    let Some(start) = expression.find('[') else {
        return (expression, Vec::new());
    };
    if !expression.ends_with(']') {
        return (expression, Vec::new());
    }
    let owner = expression[..start].trim();
    if !owner
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '.')
    {
        return (expression, Vec::new());
    }
    let body = &expression[start + 1..expression.len() - 1];
    let mut args = Vec::new();
    let mut depth = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut from = 0;
    for (index, character) in body.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == delimiter {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '[' | '(' | '{' => depth += 1,
            ']' | ')' | '}' => depth -= 1,
            ',' if depth == 0 => {
                args.push(body[from..index].trim());
                from = index + 1;
            }
            _ => {}
        }
    }
    if from < body.len() {
        args.push(body[from..].trim());
    }
    (owner, args)
}

pub fn call_request(
    runtime: &mut Runtime,
    source: &Source,
    kind: &str,
    call: &CallSite,
    oracle: Option<&Oracle>,
    receiver: Option<&str>,
) -> Result<Value> {
    let arguments: Vec<_> = call
        .arguments
        .iter()
        .enumerate()
        .map(|(index, arg)| {
            let mut value =
                json!({"kind":arg.kind,"value":arg.literal,"source":source.location(arg.span)});
            if let Some(name) = &arg.name {
                value["name"] = json!(name);
            }
            if let Some(expression) = oracle.and_then(|o| o.get(&call.argument_key(index))) {
                value["type-expr"] = runtime.type_expr(source, expression);
            }
            if let Some(inferred) = &arg.inferred_type {
                if value.get("type-expr").is_none() {
                    value["type-expr"] = inferred.clone();
                } else if let Some(snapshot) = inferred.get("snapshot") {
                    value["type-expr"]["snapshot"] = snapshot.clone();
                }
            }
            value
        })
        .collect();
    let mut callee = call.callee.clone();
    let receiver = receiver
        .or(call.flow_receiver_type.as_deref())
        .or(call.receiver_type.as_deref())
        .map(str::to_owned)
        .or_else(|| {
            oracle
                .and_then(|oracle| oracle.get(&call.receiver_key()))
                .map(|expression| runtime.canonical_name(source, expression))
        });
    let is_constructor = runtime
        .project
        .as_ref()
        .is_some_and(|project| project.class_summary(&callee).is_some());
    let mut summary = receiver
        .as_deref()
        .filter(|_| !is_constructor)
        .filter(|owner| runtime.nominal_receiver(source, owner))
        .map(|owner| runtime.qualified_receiver_summary(source, owner));
    let oracle_receiver = oracle.and_then(|oracle| {
        oracle.get(&call.receiver_key()).or_else(|| {
            call.receiver_span
                .and_then(|span| oracle.get(&crate::model::expression_key(span)))
        })
    });
    if oracle_receiver
        .is_some_and(|expression| expression.starts_with("Self@") || expression == "Self")
        && let Some(summary) = &mut summary
    {
        let bound = summary["type-expr"].clone();
        summary["type-expr"] = json!({"expression":"Self","mode":"annotation","snapshot":{"kind":"self-type","bound":{"kind":"expression","expression":bound["expression"],"mode":"annotation"}}});
    }
    if let Some(summary) = &summary
        && runtime.project.is_some()
    {
        let method = callee.rsplit('.').next().unwrap_or(&callee);
        // The summary may intentionally describe a generic base. Routing
        // still needs the actual receiver class so overridden methods are
        // never mistaken for an inherited base implementation.
        let actual = summary["type-expr"]["snapshot"]["bound"]["expression"]
            .as_str()
            .or_else(|| summary["type-expr"]["expression"].as_str())
            .unwrap_or("");
        let (outer, arguments) = generic_parts(actual);
        let owner = if matches!(outer, "type" | "builtins.type") {
            arguments.first().copied().unwrap_or(outer)
        } else {
            outer
        };
        if !owner.is_empty() {
            callee = runtime.method_callee(owner, method);
        }
    }
    let mut value =
        json!({"callee":{"expression":callee,"mode":"expression"},"arguments":arguments});
    if let Some(expression) = oracle.and_then(|o| o.get(&call.return_key())) {
        value["default-return-type"] = runtime.type_expr(source, expression);
    }
    if let Some(summary) = summary {
        value["receiver"] = summary;
    }
    runtime.execute(source, kind, value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generic_arguments_preserve_nested_and_quoted_commas() {
        assert_eq!(
            generic_parts("QuerySet[Book, tuple[int, Literal['a,b']]]"),
            ("QuerySet", vec!["Book", "tuple[int, Literal['a,b']]"])
        );
        assert_eq!(
            generic_parts("tuple[int, str] | None"),
            ("tuple[int, str] | None", vec![])
        );
    }

    #[test]
    fn inherited_state_claim_requires_original_method_implementation() {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-routing-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let manifest = json!({
            "id":"native.routing", "name":"Native routing test", "version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},
            "ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},
            "capabilities":{"call-state":true,"call-return":true},
            "claims":{
                "constructors":[{"kind":"subclass-of","base-qualified-name":"models.Base"}],
                "methods":[{"kind":"on-subclass-of","base-qualified-name":"models.Base","method-name":"save"}]
            }
        });
        let source = "class Base:\n    def save(self) -> None: pass\nclass Child(Base): pass\nclass Override(Base):\n    def save(self) -> None: pass\n";
        std::fs::write(directory.join("models.py"), source).unwrap();
        std::fs::write(directory.join("plugin.py"), "\n").unwrap();
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let project = Project::load(
            &directory,
            &[directory.join("models.py")],
            &manifest,
            json!({}),
        )
        .unwrap();
        let mut runtime =
            Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py")).unwrap();
        runtime.attach_project(Arc::new(project), json!({}));
        let source = Source::new(
            source.into(),
            directory.join("models.py").to_string_lossy().into_owned(),
            "models".into(),
        );
        let inherited = json!({"callee":{"expression":runtime.method_callee("models.Child","save")},"receiver":runtime.receiver_summary(&source,"Child")});
        let overridden = json!({"callee":{"expression":runtime.method_callee("models.Override","save")},"receiver":runtime.receiver_summary(&source,"Override")});
        assert!(runtime.allowed(&source, "adjust-call-state", &inherited));
        assert!(!runtime.allowed(&source, "adjust-call-state", &overridden));
        assert!(runtime.allowed(&source, "adjust-call-return", &overridden));
        let constructor = json!({"callee":{"expression":"models.Child"}});
        assert!(runtime.allowed(&source, "adjust-call-state", &constructor));
        assert!(!runtime.allowed(&source, "adjust-call-return", &constructor));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn oracle_display_forms_do_not_capture_unrelated_project_classes() {
        let directory =
            std::env::temp_dir().join(format!("native-runtime-display-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let manifest = json!({"id":"native.display","name":"Display contract","version":"0.0.0",
            "protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},
            "runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{},"claims":{}});
        std::fs::write(
            directory.join("library.py"),
            "class Literal: pass\nclass Prefetch: pass\n",
        )
        .unwrap();
        std::fs::write(
            directory.join("consumer.py"),
            "from datetime import date, datetime\nfrom library import Prefetch as Imported\n",
        )
        .unwrap();
        std::fs::write(directory.join("plugin.py"), "\n").unwrap();
        std::fs::write(
            directory.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let project = Project::load(
            &directory,
            &[directory.join("library.py"), directory.join("consumer.py")],
            &manifest,
            json!({}),
        )
        .unwrap();
        let mut runtime =
            Runtime::new(&directory.join("plugin.json"), &directory.join("plugin.py")).unwrap();
        runtime.attach_project(Arc::new(project), json!({}));
        let consumer = Source::new("\n".into(), "consumer.py".into(), "consumer".into());
        assert_eq!(
            runtime.canonical_name(&consumer, "Literal[1]"),
            "Literal[1]"
        );
        assert_eq!(
            runtime.type_expr(&consumer, "Literal[1]")["expression"],
            "Literal[1]"
        );
        let inferred = "Prefetch[Book, QuerySet[Book, Book], Literal[\"cached\"]]";
        assert_eq!(
            runtime.type_expr(&consumer, inferred)["expression"],
            inferred
        );
        let date = runtime.type_expr(&consumer, "date");
        assert_eq!(date["expression"], "date");
        assert_eq!(date["snapshot"]["qualified-name"], "datetime.date");
        let imported = runtime.type_expr(&consumer, "Imported[int]");
        assert_eq!(imported["expression"], "Imported[int]");
        assert_eq!(imported["snapshot"]["qualified-name"], "library.Prefetch");
        assert_eq!(
            imported["snapshot"]["arguments"][0]["qualified-name"],
            "builtins.int"
        );
        assert!(
            runtime
                .type_expr(&consumer, "Literal[1]")
                .get("snapshot")
                .is_none()
        );
        let protocol_type = runtime.qualified_type_expr("datetime.datetime");
        assert_eq!(
            protocol_type["snapshot"]["qualified-name"],
            "datetime.datetime"
        );
        let receiver =
            runtime.qualified_receiver_summary(&consumer, "library.Prefetch[datetime.datetime]");
        assert_eq!(
            receiver["generic-arguments"][0]["expression"],
            "datetime.datetime"
        );
        assert_eq!(
            runtime.type_expr(&consumer, "Self@select")["snapshot"]["kind"],
            "self-type"
        );
        let library = Source::new("\n".into(), "library.py".into(), "library".into());
        assert_eq!(
            runtime.canonical_name(&library, "Literal"),
            "library.Literal"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn chained_self_receiver_uses_its_bound_without_changing_display() {
        let value = json!({"expression":"Self","mode":"annotation","snapshot":{
            "kind":"self-type","bound":{"kind":"expression","expression":"demo.Selection[int]","mode":"annotation"}
        }});
        assert_eq!(
            Runtime::receiver_identity(&value).as_deref(),
            Some("demo.Selection[int]")
        );
        assert_eq!(value["expression"], "Self");
    }
}
