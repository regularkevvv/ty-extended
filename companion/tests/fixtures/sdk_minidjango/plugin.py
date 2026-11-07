# ruff: noqa: F821, I002
"""Monty adapter for ty_plugin_examples::minidjango, used only by SDK proof tests.

Keep the original Rust sample's rules and fixture source unchanged. The SDK prelude
provides protocol constructors and callback registration in both tested runtimes.
"""


def observed(fn):
    def run(request):
        try:
            return fn(request)
        except Exception as error:
            print("MINIDJANGO_ERROR", str(error))
            raise

    return run


def nullable(expression, flag):
    return expression + " | None" if flag else expression


def assigned_call(field):
    value = field.get("assigned-value") or {}
    return value if value.get("kind") == "call" else None


def callee(call):
    return call["callee"]["qualified-name"].split(".")[-1]


def keyword(call, name):
    for arg in call.get("arguments", []):
        if arg.get("name") == name:
            return arg.get("value", {})
    return {}


def is_null(call):
    return keyword(call, "null") == {"kind": "bool", "value": True}


def manager(call):
    name = callee(call)
    return name == "Manager" or name.endswith("Manager")


def target_type(module, model, call, settings):
    for arg in call.get("arguments", []):
        if arg["kind"] != "positional":
            continue
        value = arg["value"]
        kind = value["kind"]
        qualified = value.get("qualified-name", "")
        if kind in ["enum-ref", "symbol-ref"] and qualified in settings:
            return settings[qualified]
        if kind in ["class-ref", "symbol-ref", "enum-ref"]:
            return (arg.get("type-expr") or {}).get("expression", qualified)
        if kind == "str":
            text = value["value"]
            if text == "self":
                return model
            if text in settings:
                return settings[text]
            return text if "." in text else module + "." + text
        return (arg.get("type-expr") or {}).get("expression")
    return None


def field_type(module, model, call, settings):
    name = callee(call)
    if name == "CharField":
        result = "str"
    elif name == "IntegerField":
        result = "int"
    elif name == "ForeignKey":
        result = target_type(module, model, call, settings)
    else:
        result = None
    return nullable(result, is_null(call)) if result else None


def is_model(cls):
    return any(
        base["expression"] == "minidjango.Model" for base in cls.get("bases", [])
    )


def virtual_name(model, shape):
    return "minidjango.virtual." + model + "." + shape


def diagnostic_at(ident, message, source):
    result = {"id": ident, "message": message, "severity": "error"}
    if source and all(key in source for key in ["file-path", "start", "end"]):
        result["location"] = {key: source[key] for key in ["file-path", "start", "end"]}
    return result


@on_project_index
@observed
def build_index(request):
    print("MINIDJANGO_INDEX")
    settings = {}
    for summary in request.get("settings", []):
        for value in summary.get("values", []):
            if value["value"]["kind"] == "str":
                settings[summary["module"] + "." + value["name"]] = value["value"][
                    "value"
                ]
    classes = [cls for cls in request.get("classes", []) if is_model(cls)]
    names = [cls["qualified-name"] for cls in classes]
    models, reverse, contributions, diagnostics, virtual = {}, {}, [], [], []
    for cls in classes:
        model = cls["qualified-name"]
        module = ".".join(model.split(".")[:-1])
        fields = {"id": "int", "pk": "int"}
        for field in cls.get("fields", []):
            call = assigned_call(field)
            if not call or manager(call):
                continue
            expression = field_type(module, model, call, settings)
            if expression:
                fields[field["name"]] = expression
                if callee(call) == "ForeignKey":
                    fields[field["name"] + "_id"] = nullable("int", is_null(call))
        fields = {name: fields[name] for name in sorted(fields)}
        models[model] = {"fields": fields}
        vf = [virtual_field(name, type_annotation(fields[name])) for name in fields]
        virtual += [
            virtual_type(
                virtual_name(model, "Manager"),
                virtual_class([type_expr("minidjango.Manager[" + model + "]")]),
            ),
            virtual_type(virtual_name(model, "ValuesRow"), virtual_typed_dict(vf)),
            virtual_type(virtual_name(model, "ValuesListRow"), virtual_named_tuple(vf)),
        ]
        for field in cls.get("fields", []):
            call = assigned_call(field)
            if not call or callee(call) != "ForeignKey":
                continue
            target = target_type(module, model, call, settings)
            if not target:
                continue
            source = field["source"]
            if target not in names:
                diagnostics.append(
                    diagnostic_at(
                        "minidjango.unknown-relation-target",
                        "Unknown Mini-Django relation target `"
                        + target
                        + "` for field `"
                        + model
                        + "."
                        + field["name"]
                        + "`",
                        source,
                    )
                )
                continue
            related = keyword(call, "related_name")
            name = (
                related["value"]
                if related.get("kind") == "str"
                else model.split(".")[-1].lower() + "_set"
            )
            if name == "+":
                continue
            conflict = target + "." + name
            first = reverse.get(conflict)
            reverse[conflict] = source
            if first:
                diag = diagnostic_at(
                    "minidjango.reverse-relation-conflict",
                    "Conflicting Mini-Django reverse relation `" + conflict + "`",
                    source,
                )
                if first.get("file-path"):
                    diag["metadata"] = {"first-file-path": first["file-path"]}
                diagnostics.append(diag)
                continue
            patch = field_patch(
                name, type_annotation(virtual_name(model, "Manager")), has_default=True
            )
            contributions.append(
                contribution(
                    source, instance_target(target), field_contribution(patch), conflict
                )
            )
    return project_index(
        plugin_index={"models": models, "settings": settings},
        contributions=contributions,
        virtual_types=virtual,
        diagnostics=diagnostics,
    )


def non_init(name, expression):
    ty = type_annotation(expression)
    return field_patch(name, ty, instance_set_type=ty, has_default=True)


@on_class_transform
@observed
def analyze(request):
    cls = request["class"]
    if not is_model(cls):
        return no_change()
    model = cls["qualified-name"]
    settings = (request.get("project-index") or {}).get("settings", {})
    manager_type = type_annotation(virtual_name(model, "Manager"))
    fields = [non_init("id", "int"), non_init("pk", "int")]
    members = [
        member("objects", manager_type),
        member("_default_manager", manager_type),
    ]
    for field in cls.get("fields", []):
        call = assigned_call(field)
        if not call:
            continue
        if manager(call):
            members.append(member(field["name"], manager_type))
            continue
        expression = field_type(request["context"]["module"], model, call, settings)
        if not expression:
            continue
        ty = type_annotation(expression)
        has_default = field.get("has-default", False) or is_null(call)
        parameter = {
            "name": field["name"],
            "kind": "keyword-only",
            "type-expr": ty,
            "required": not has_default,
        }
        if callee(call) == "ForeignKey":
            fields.append(
                non_init(field["name"] + "_id", nullable("int", is_null(call)))
            )
        fields.append(
            field_patch(
                field["name"],
                ty,
                mode="replace-existing",
                descriptor=member_descriptor(ty, instance_set_type=ty),
                instance_set_type=ty,
                constructor_parameter=parameter,
                has_default=has_default,
            )
        )
    return class_patch(fields=fields, class_members=members)


TERMINALS = [
    "exact",
    "iexact",
    "contains",
    "icontains",
    "startswith",
    "istartswith",
    "endswith",
    "iendswith",
    "regex",
    "iregex",
    "gt",
    "gte",
    "lt",
    "lte",
    "in",
    "range",
    "isnull",
]
TEXT_LOOKUPS = [
    "contains",
    "icontains",
    "startswith",
    "istartswith",
    "endswith",
    "iendswith",
    "regex",
    "iregex",
]


def lookup_field(models, model, lookup):
    parts = lookup.split("__")
    if not parts or any(not part for part in parts):
        return None
    terminal = parts[-1] if parts[-1] in TERMINALS else "exact"
    if parts[-1] in TERMINALS:
        parts = parts[:-1]
    if not parts:
        return None
    for part in parts[:-1]:
        expression = models.get(model, {}).get("fields", {}).get(part)
        if not expression:
            return None
        candidates = [
            name.strip()
            for name in expression.split("|")
            if name.strip() != "None" and name.strip() in models
        ]
        if not candidates:
            return None
        model = candidates[0]
    expression = models.get(model, {}).get("fields", {}).get(parts[-1])
    return [parts[-1], expression, terminal] if expression else None


def matches(expression, value):
    kind = value["kind"]
    if kind in ["unknown", "class-ref", "enum-ref", "symbol-ref"]:
        return True
    expected = {"none": "None", "bool": "bool", "int": "int", "str": "str"}.get(kind)
    return expected is not None and expected in [
        name.strip() for name in expression.split("|")
    ]


def compatible(expression, terminal, value):
    kind = value["kind"]
    if terminal == "isnull":
        return kind == "bool"
    if terminal in ["in", "range"]:
        if kind == "unknown":
            return True
        if kind not in ["list", "tuple"]:
            return False
        items = value["items"]
        return (terminal != "range" or len(items) == 2) and all(
            matches(expression, item) for item in items
        )
    if terminal in TEXT_LOOKUPS:
        return "str" in [name.strip() for name in expression.split("|")] and kind in [
            "str",
            "unknown",
        ]
    return matches(expression, value)


def quote(text):
    return '"' + text.replace("\\", "\\\\").replace('"', '\\"') + '"'


@on_call_return
@observed
def call_return(request):
    receiver = request.get("receiver") or {}
    nominal = receiver.get("nominal-class")
    generic = receiver.get("generic-arguments", [])
    if nominal not in ["minidjango.Manager", "minidjango.QuerySet"] or not generic:
        return no_change()
    method = request["callee"]["expression"].split(".")[-1]
    model = generic[0]["expression"]
    row = (
        generic[1]["expression"]
        if nominal == "minidjango.QuerySet" and len(generic) > 1
        else model
    )
    models = (request.get("project-index") or {}).get("models", {})
    fields = models.get(model, {}).get("fields")
    diagnostics = []
    if fields is not None:
        for arg in request.get("arguments", []):
            lookup, resolved = None, None
            if (
                method in ["filter", "get", "get_or_create"]
                and arg["kind"] == "keyword"
            ):
                lookup = arg.get("name")
                if lookup:
                    resolved = lookup_field(models, model, lookup)
            elif (
                method in ["values", "values_list"]
                and arg["kind"] == "positional"
                and arg["value"]["kind"] == "str"
            ):
                lookup = arg["value"]["value"]
                if lookup in fields:
                    continue
            if lookup is None:
                continue
            if resolved is None:
                diagnostics.append(
                    diagnostic_at(
                        "minidjango.unknown-lookup",
                        "Unknown Mini-Django lookup `"
                        + lookup
                        + "` for model `"
                        + model
                        + "`",
                        arg.get("source"),
                    )
                )
            elif not compatible(resolved[1], resolved[2], arg["value"]):
                diagnostics.append(
                    diagnostic_at(
                        "minidjango.invalid-lookup-value",
                        "Invalid Mini-Django lookup value for `"
                        + lookup
                        + "` on `"
                        + model
                        + "."
                        + resolved[0]
                        + "`; expected `"
                        + resolved[1]
                        + "`",
                        arg.get("source"),
                    )
                )
    if method == "filter":
        result = "minidjango.QuerySet[" + model + ", " + row + "]"
    elif method == "get":
        result = row
    elif method == "get_or_create":
        result = "tuple[" + model + ", bool]"
    elif method == "first":
        result = row + " | None"
    elif method in ["count", "exists"]:
        result = "int" if method == "count" else "bool"
    elif method in ["values", "values_list"]:
        names = [
            arg["value"]["value"]
            for arg in request.get("arguments", [])
            if arg["kind"] == "positional" and arg["value"]["kind"] == "str"
        ]
        named = keyword(request, "named") == {"kind": "bool", "value": True}
        flat = keyword(request, "flat") == {"kind": "bool", "value": True}
        if method == "values":
            if fields is None:
                row = "dict[str, object]"
            elif not names:
                row = virtual_name(model, "ValuesRow")
            else:
                row = (
                    "TypedDict({"
                    + ", ".join(
                        quote(name) + ": " + fields.get(name, "object")
                        for name in names
                    )
                    + "})"
                )
        elif named:
            if fields is None:
                return no_change()
            row = (
                'NamedTuple("MiniDjangoValuesListRow", {'
                + ", ".join(
                    quote(name) + ": " + fields.get(name, "object") for name in names
                )
                + "})"
                if names
                else virtual_name(model, "ValuesListRow")
            )
        elif flat:
            if not names:
                return no_change()
            row = (fields or {}).get(names[0], "str")
        elif fields is None:
            return no_change()
        else:
            row = (
                "tuple["
                + ", ".join(
                    fields.get(name, "Any") for name in (names or sorted(fields))
                )
                + "]"
            )
        result = "minidjango.QuerySet[" + model + ", " + row + "]"
    elif method == "annotate":
        entries = []
        for arg in request.get("arguments", []):
            if arg["kind"] == "keyword" and arg.get("name"):
                expression = (arg.get("type-expr") or {}).get("expression")
                if expression is None:
                    expression = {
                        "bool": "bool",
                        "int": "int",
                        "str": "str",
                        "none": "None",
                    }.get(arg["value"]["kind"], "object")
                entries.append(quote(arg["name"]) + ": " + expression)
        if entries:
            row = (
                'Class("MiniDjangoAnnotatedRow", {'
                + ", ".join(entries)
                + "}, "
                + row
                + ")"
            )
        result = "minidjango.QuerySet[" + model + ", " + row + "]"
    else:
        return no_change()
    return call_return_patch(type_annotation(result), diagnostics=diagnostics)
