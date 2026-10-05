"""Authoring surface for `ty` semantic plugins written in Python.

Importable at authoring time so editors and type checkers resolve hook names and
signatures; inside the sandbox `ty` prepends this file so its names are ambient.
Builders emit plain dicts in the kebab-case protocol wire format; hooks receive a
request dict and return a response dict (or `None` for `no-change`).
"""

from __future__ import annotations

import json
import typing

if typing.TYPE_CHECKING:
    from typing import Any, Callable, Literal, Optional

    JsonObject = dict[str, Any]
    Request = JsonObject
    Response = Optional[JsonObject]
    Handler = Callable[[Request], Response]
    Decorator = Callable[[Handler], Handler]
else:
    # Lets `request: Request` annotations evaluate in the sandbox without `from __future__`.
    Request = dict
    Response = dict

# The wire protocol this SDK was built against. Bumped with the host's protocol version.
PROTOCOL_VERSION: dict[str, int] = {"major": 0, "minor": 7}

_hooks: dict[str, list[Handler]] = {}
_MANIFEST: Optional[JsonObject] = None


# Hook registration. Each kind keeps a list of handlers; dispatch runs them in
# registration order and returns the first non-`None` result.


def _register(kind: str) -> Decorator:
    def decorator(fn: Handler) -> Handler:
        _hooks.setdefault(kind, []).append(fn)
        return fn

    return decorator


def _filtered(kind: str, key: str, names: tuple[str, ...]) -> Decorator:

    def decorator(fn: Handler) -> Handler:
        def check(request: Request) -> Response:
            if request.get(key) in names:
                return fn(request)
            return None

        _hooks.setdefault(kind, []).append(check)
        return fn

    return decorator


def _filtered_callee(kind: str, names: tuple[str, ...]) -> Decorator:
    def decorator(fn: Handler) -> Handler:
        def check(request: Request) -> Response:
            callee = request.get("callee") or {}
            if callee.get("expression") in names:
                return fn(request)
            return None

        _hooks.setdefault(kind, []).append(check)
        return fn

    return decorator


def _filtered_class(names: tuple[str, ...]) -> Decorator:
    def decorator(fn: Handler) -> Handler:
        def check(request: Request) -> Response:
            summary = request.get("class") or {}
            if summary.get("qualified-name") in names:
                return fn(request)
            return None

        _hooks.setdefault("analyze-class", []).append(check)
        return fn

    return decorator


on_class_transform: Decorator = _register("analyze-class")
on_class_member: Decorator = _register("resolve-class-member")
on_instance_member: Decorator = _register("resolve-instance-member")
on_call_signature: Decorator = _register("adjust-call-signature")
on_call_return: Decorator = _register("adjust-call-return")
on_call_state: Decorator = _register("adjust-call-state")
on_project_index: Decorator = _register("build-project-index")
on_dependencies: Decorator = _register("additional-dependencies")
on_mutation: Decorator = _register("validate-mutation")
on_manifest: Decorator = _register("manifest")


def on_class_transform_of(*qualified_names: str) -> Decorator:
    return _filtered_class(qualified_names)


def on_class_member_of(*member_names: str) -> Decorator:
    return _filtered("resolve-class-member", "member-name", member_names)


def on_instance_member_of(*member_names: str) -> Decorator:
    return _filtered("resolve-instance-member", "member-name", member_names)


def on_call_signature_of(*qualified_names: str) -> Decorator:
    return _filtered_callee("adjust-call-signature", qualified_names)


def on_call_state_of(*qualified_names: str) -> Decorator:
    return _filtered_callee("adjust-call-state", qualified_names)


def on_call_return_of(*qualified_names: str) -> Decorator:
    return _filtered_callee("adjust-call-return", qualified_names)


def on_mutation_of(
    *operations: Literal["attribute-set", "item-set", "attribute-delete", "item-delete"],
) -> Decorator:
    return _filtered("validate-mutation", "operation", operations)


def set_manifest(manifest_dict: JsonObject) -> None:
    global _MANIFEST
    _MANIFEST = manifest_dict


# Request accessors — thin reads over the kebab-case wire keys.


def context(request: Request) -> JsonObject:
    return request.get("context") or {}


def plugin_config(request: Request) -> JsonObject:
    """Resolved plugin config: `context.config`, or top-level `config` on `additional-dependencies`."""
    ctx = context(request)
    if "config" in ctx:
        return ctx.get("config") or {}
    return request.get("config") or {}


def strict_settings(request: Request) -> bool:
    return bool(plugin_config(request).get("strict_settings"))


def speculative(request: Request) -> bool:
    """Whether the request is a speculative query whose answers must not be cached."""
    return bool(context(request).get("speculative"))


def callee(request: Request) -> Optional[str]:
    return (request.get("callee") or {}).get("expression")


def receiver(request: Request) -> Optional[JsonObject]:
    return request.get("receiver")


def owner(request: Request) -> Optional[JsonObject]:
    return request.get("owner")


def member_name(request: Request) -> Optional[str]:
    return request.get("member-name")


def class_summary(request: Request) -> JsonObject:
    return request.get("class") or {}


def project_index_of(request: Request) -> JsonObject:
    return request.get("project-index") or {}


def call_arguments(request: Request) -> list[JsonObject]:
    return request.get("arguments") or []


def positional_arguments(request: Request) -> list[JsonObject]:
    return [arg for arg in call_arguments(request) if arg.get("kind") == "positional"]


def keyword_arguments(request: Request) -> dict[str, JsonObject]:
    result = {}
    for arg in call_arguments(request):
        name = arg.get("name")
        if arg.get("kind") == "keyword" and name is not None:
            result[name] = arg
    return result


def argument_type(argument: JsonObject) -> Optional[JsonObject]:
    return argument.get("type-expr")


#: Sentinel returned by `literal_value` for values the host could not summarize.
#: Compare with `is`: `if literal_value(arg) is UNKNOWN: ...`
UNKNOWN: JsonObject = {}


def literal_value(argument_or_value: Any) -> Any:
    """Unwrap an `ArgumentSummary`/`LiteralValue` into Python data; `unknown` yields `UNKNOWN`."""
    if not isinstance(argument_or_value, dict):
        return UNKNOWN
    value = argument_or_value
    inner = value.get("value")
    if isinstance(inner, dict):
        value = inner
    if not value:
        return UNKNOWN
    kind = value.get("kind")
    if kind == "bool" or kind == "int" or kind == "str":
        return value.get("value")
    if kind == "none":
        return None
    if kind == "tuple":
        return tuple(literal_value(item) for item in value.get("items") or [])
    if kind == "list":
        return [literal_value(item) for item in value.get("items") or []]
    if kind == "dict":
        result = {}
        for entry in value.get("entries") or []:
            result[literal_value(entry.get("key"))] = literal_value(entry.get("value"))
        return result
    if kind == "enum-ref" or kind == "symbol-ref" or kind == "class-ref":
        return value.get("qualified-name")
    return UNKNOWN


# Shared value builders.


def type_expr(
    expression: str,
    mode: Literal["expression", "annotation", "stub"] = "expression",
    imports: Optional[list[JsonObject]] = None,
    snapshot: Optional[JsonObject] = None,
) -> JsonObject:
    value = {"expression": expression, "mode": mode}
    if imports:
        value["imports"] = imports
    if snapshot is not None:
        value["snapshot"] = snapshot
    return value


def type_annotation(
    expression: str,
    imports: Optional[list[JsonObject]] = None,
    snapshot: Optional[JsonObject] = None,
) -> JsonObject:
    return type_expr(expression, mode="annotation", imports=imports, snapshot=snapshot)


def type_stub(
    expression: str,
    imports: Optional[list[JsonObject]] = None,
    snapshot: Optional[JsonObject] = None,
) -> JsonObject:
    return type_expr(expression, mode="stub", imports=imports, snapshot=snapshot)


def import_binding(module: str, name: str, alias: Optional[str] = None) -> JsonObject:
    binding = {"module": module, "name": name}
    if alias is not None:
        binding["alias"] = alias
    return binding


def parameter(
    name: Optional[str] = None,
    kind: Literal[
        "positional-only", "positional-or-keyword", "var-args", "keyword-only", "kwargs"
    ] = "positional-or-keyword",
    type: Optional[JsonObject] = None,
    required: bool = False,
) -> JsonObject:
    param = {"kind": kind, "required": required}
    if name is not None:
        param["name"] = name
    if type is not None:
        param["type-expr"] = type
    return param


def positional_or_keyword(name: str, type: JsonObject, required: bool = True) -> JsonObject:
    return parameter(name=name, type=type, required=required)


def keyword_only(name: str, type: JsonObject, required: bool = True) -> JsonObject:
    return parameter(name=name, kind="keyword-only", type=type, required=required)


def optional(
    name: str,
    type: JsonObject,
    kind: Literal["positional-only", "positional-or-keyword", "keyword-only"] = "positional-or-keyword",
) -> JsonObject:
    """A parameter the caller may omit (`required` unset on the wire)."""
    return parameter(name=name, kind=kind, type=type)


def callable_signature(
    parameters: Optional[list[JsonObject]] = None,
    return_type: Optional[JsonObject] = None,
) -> JsonObject:
    signature = {"return-type": return_type or type_expr("None")}
    if parameters:
        signature["parameters"] = parameters
    return signature


def member_value(type: JsonObject) -> JsonObject:
    return {"kind": "value", "type-expr": type}


def member_descriptor(
    instance_get_type: JsonObject,
    class_type: Optional[JsonObject] = None,
    instance_set_type: Optional[JsonObject] = None,
) -> JsonObject:
    patch = {"kind": "descriptor", "instance-get-type": instance_get_type}
    if class_type is not None:
        patch["class-type"] = class_type
    if instance_set_type is not None:
        patch["instance-set-type"] = instance_set_type
    return patch


def member_callable(signature: JsonObject, fallback_type: JsonObject) -> JsonObject:
    return {"kind": "callable", "signature": signature, "fallback-type": fallback_type}


def member_patch(
    name: str,
    access: JsonObject,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    read_only: bool = False,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    patch = {"name": name, "mode": mode, "access": access, "read-only": read_only}
    if diagnostics:
        patch["diagnostics"] = diagnostics
    return patch


def member(
    name: str,
    type: JsonObject,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    read_only: bool = False,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    return member_patch(
        name, member_value(type), mode=mode, read_only=read_only, diagnostics=diagnostics
    )


def callable_member(
    name: str,
    signature: JsonObject,
    fallback_type: JsonObject,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    read_only: bool = False,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    return member_patch(
        name,
        member_callable(signature, fallback_type),
        mode=mode,
        read_only=read_only,
        diagnostics=diagnostics,
    )


def descriptor_member(
    name: str,
    instance_get_type: JsonObject,
    class_type: Optional[JsonObject] = None,
    instance_set_type: Optional[JsonObject] = None,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    read_only: bool = False,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    return member_patch(
        name,
        member_descriptor(instance_get_type, class_type, instance_set_type),
        mode=mode,
        read_only=read_only,
        diagnostics=diagnostics,
    )


def field_patch(
    name: str,
    instance_get_type: JsonObject,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    descriptor: Optional[JsonObject] = None,
    instance_set_type: Optional[JsonObject] = None,
    constructor_parameter: Optional[JsonObject] = None,
    has_default: bool = False,
) -> JsonObject:
    patch = {
        "name": name,
        "mode": mode,
        "instance-get-type": instance_get_type,
        "has-default": has_default,
    }
    if descriptor is not None:
        patch["descriptor"] = descriptor
    if instance_set_type is not None:
        patch["instance-set-type"] = instance_set_type
    if constructor_parameter is not None:
        patch["constructor-parameter"] = constructor_parameter
    return patch


def field(
    name: str,
    type: JsonObject,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    instance_set_type: Optional[JsonObject] = None,
    constructor_parameter: Optional[JsonObject] = None,
    has_default: bool = False,
    descriptor: Optional[JsonObject] = None,
) -> JsonObject:
    return field_patch(
        name,
        type,
        mode=mode,
        descriptor=descriptor,
        instance_set_type=instance_set_type,
        constructor_parameter=constructor_parameter,
        has_default=has_default,
    )


def init_field(
    name: str,
    type: JsonObject,
    has_default: bool = False,
    mode: Literal["fill-on-miss", "replace-existing"] = "fill-on-miss",
    descriptor: Optional[JsonObject] = None,
) -> JsonObject:
    return field_patch(
        name,
        instance_get_type=type,
        mode=mode,
        descriptor=descriptor,
        constructor_parameter=parameter(name=name, type=type, required=not has_default),
        has_default=has_default,
    )


def text_position(line: int, column: int) -> JsonObject:
    """`line` and `column` are 1-based."""
    return {"line": line, "column": column}


def location(file_path: str, start: JsonObject, end: JsonObject) -> JsonObject:
    return {"file-path": file_path, "start": start, "end": end}


def diagnostic(
    id: str,
    message: str,
    severity: Literal["error", "warning", "info"] = "error",
    location: Optional[JsonObject] = None,
    metadata: Optional[JsonObject] = None,
) -> JsonObject:
    diag = {"id": id, "message": message, "severity": severity}
    if location is not None:
        diag["location"] = location
    if metadata:
        diag["metadata"] = metadata
    return diag


def dependency(path: str, sha256: Optional[str] = None) -> JsonObject:
    dep = {"path": path}
    if sha256 is not None:
        dep["sha256"] = sha256
    return dep


def symbol_source(
    module: Optional[str] = None,
    qualified_name: Optional[str] = None,
    file_path: Optional[str] = None,
    start: Optional[JsonObject] = None,
    end: Optional[JsonObject] = None,
) -> JsonObject:
    """An empty `symbol_source()` is the protocol's "unknown origin" marker."""
    source = {}
    if module is not None:
        source["module"] = module
    if qualified_name is not None:
        source["qualified-name"] = qualified_name
    if file_path is not None:
        source["file-path"] = file_path
    if start is not None:
        source["start"] = start
    if end is not None:
        source["end"] = end
    return source


# TypeSnapshot builders — the structural form carried inside TypeExpr.


def snapshot_expression(
    expression: str,
    mode: Literal["expression", "annotation", "stub"] = "expression",
    imports: Optional[list[JsonObject]] = None,
) -> JsonObject:
    snap = {"kind": "expression", "expression": expression, "mode": mode}
    if imports:
        snap["imports"] = imports
    return snap


def snapshot_nominal(
    qualified_name: str, arguments: Optional[list[JsonObject]] = None
) -> JsonObject:
    snap = {"kind": "nominal", "qualified-name": qualified_name}
    if arguments:
        snap["arguments"] = arguments
    return snap


def snapshot_tuple(
    prefix: Optional[list[JsonObject]] = None,
    variadic: Optional[JsonObject] = None,
    suffix: Optional[list[JsonObject]] = None,
) -> JsonObject:
    snap = {"kind": "tuple"}
    if prefix:
        snap["prefix"] = prefix
    if variadic is not None:
        snap["variadic"] = variadic
    if suffix:
        snap["suffix"] = suffix
    return snap


def snapshot_field(
    name: str, type_snapshot: JsonObject, required: bool = True, read_only: bool = False
) -> JsonObject:
    return {
        "name": name,
        "type-snapshot": type_snapshot,
        "required": required,
        "read-only": read_only,
    }


def snapshot_typed_dict(
    fields: Optional[list[JsonObject]] = None,
    extra_items: Optional[JsonObject] = None,
    closed: bool = False,
) -> JsonObject:
    snap = {"kind": "typed-dict", "closed": closed}
    if fields:
        snap["fields"] = fields
    if extra_items is not None:
        snap["extra-items"] = extra_items
    return snap


def snapshot_union(elements: list[JsonObject]) -> JsonObject:
    return {"kind": "union", "elements": elements}


def snapshot_plugin_class(identity: str) -> JsonObject:
    return {"kind": "plugin-class", "identity": identity}


def snapshot_self(bound: Optional[JsonObject] = None) -> JsonObject:
    snap = {"kind": "self-type"}
    if bound is not None:
        snap["bound"] = bound
    return snap


def snapshot_metadata(
    qualified_name: str, arguments: Optional[list[JsonObject]] = None
) -> JsonObject:
    meta = {"qualified-name": qualified_name}
    if arguments:
        meta["arguments"] = arguments
    return meta


def snapshot_annotated(
    base: JsonObject, metadata: Optional[list[JsonObject]] = None
) -> JsonObject:
    snap = {"kind": "annotated", "base": base}
    if metadata:
        snap["metadata"] = metadata
    return snap


def snapshot_name(snapshot: JsonObject) -> Optional[str]:
    kind = snapshot.get("kind")
    if kind == "nominal":
        return snapshot.get("qualified-name")
    if kind == "plugin-class":
        return snapshot.get("identity")
    if kind == "annotated":
        return snapshot_name(snapshot.get("base") or {})
    if kind == "self-type":
        bound = snapshot.get("bound")
        return snapshot_name(bound) if bound else "Self"
    return None


def snapshot_to_expression(snapshot: JsonObject) -> str:
    """Render a snapshot as a display expression. `typed-dict` has no expression form and raises."""
    kind = (snapshot or {}).get("kind")
    if kind == "expression":
        return snapshot.get("expression") or "object"
    if kind == "nominal":
        name = snapshot.get("qualified-name") or "object"
        arguments = snapshot.get("arguments") or []
        if arguments:
            return name + "[" + ", ".join(snapshot_to_expression(arg) for arg in arguments) + "]"
        return name
    if kind == "union":
        return " | ".join(snapshot_to_expression(el) for el in snapshot.get("elements") or [])
    if kind == "tuple":
        parts = []
        for item in snapshot.get("prefix") or []:
            parts.append(snapshot_to_expression(item))
        variadic = snapshot.get("variadic")
        if variadic is not None:
            parts.append("*" + snapshot_to_expression(variadic))
        for item in snapshot.get("suffix") or []:
            parts.append(snapshot_to_expression(item))
        return "tuple[" + ", ".join(parts) + "]" if parts else "tuple"
    if kind == "plugin-class":
        return snapshot.get("identity") or "object"
    if kind == "self-type":
        bound = snapshot.get("bound")
        return snapshot_to_expression(bound) if bound else "Self"
    if kind == "annotated":
        return snapshot_to_expression(snapshot.get("base") or {})
    if kind == "typed-dict":
        raise ValueError("typed-dict snapshots have no expression form")
    raise ValueError("unknown snapshot kind: " + str(kind))


def type_expr_from_snapshot(
    snapshot: JsonObject,
    mode: Literal["expression", "annotation", "stub"] = "expression",
) -> JsonObject:
    return type_expr(snapshot_to_expression(snapshot), mode=mode, snapshot=snapshot)


def type_expr_expression(expr: Optional[JsonObject]) -> Optional[str]:
    return (expr or {}).get("expression")


def type_expr_snapshot(expr: Optional[JsonObject]) -> Optional[JsonObject]:
    return (expr or {}).get("snapshot")


# Manifest claims — each builder returns a dict for the matching `claims()` list.


def module_claim(name: str, recursive: bool = False) -> JsonObject:
    return {"name": name, "recursive": recursive}


def class_claim_exact(qualified_name: str) -> JsonObject:
    return {"kind": "exact", "qualified-name": qualified_name}


def class_claim_subclass_of(base_qualified_name: str) -> JsonObject:
    return {"kind": "subclass-of", "base-qualified-name": base_qualified_name}


def symbol_claim(qualified_name: str) -> JsonObject:
    return {"qualified-name": qualified_name}


def method_claim_exact(class_qualified_name: str, method_name: str) -> JsonObject:
    return {
        "kind": "exact",
        "class-qualified-name": class_qualified_name,
        "method-name": method_name,
    }


def method_claim_on_subclass_of(base_qualified_name: str, method_name: str) -> JsonObject:
    return {
        "kind": "on-subclass-of",
        "base-qualified-name": base_qualified_name,
        "method-name": method_name,
    }


def method_claim_on_subclass_of_matching(
    base_qualified_name: str, method_name_pattern: str
) -> JsonObject:
    """`method_name_pattern` uses `*` as a wildcard (`"*"` claims every method)."""
    return {
        "kind": "on-subclass-of-matching",
        "base-qualified-name": base_qualified_name,
        "method-name-pattern": method_name_pattern,
    }


def attribute_claim_exact(
    owner_qualified_name: str,
    attribute_name: str,
    scope: Literal["class", "instance"],
) -> JsonObject:
    return {
        "kind": "exact",
        "owner-qualified-name": owner_qualified_name,
        "attribute-name": attribute_name,
        "scope": scope,
    }


def attribute_claim_on_subclass_of(
    owner_base_qualified_name: str,
    scope: Literal["class", "instance"],
) -> JsonObject:
    return {
        "kind": "on-subclass-of",
        "owner-base-qualified-name": owner_base_qualified_name,
        "scope": scope,
    }


def attribute_claim_contribution_target(
    owner_base_qualified_name: str,
    scope: Literal["class", "instance"],
) -> JsonObject:
    return {
        "kind": "contribution-target",
        "owner-base-qualified-name": owner_base_qualified_name,
        "scope": scope,
    }


def settings_claim(
    module: Optional[str] = None, config_key: Optional[str] = None
) -> JsonObject:
    claim = {}
    if module is not None:
        claim["module"] = module
    if config_key is not None:
        claim["config-key"] = config_key
    return claim


def claims(
    modules: Optional[list[JsonObject]] = None,
    classes: Optional[list[JsonObject]] = None,
    decorators: Optional[list[JsonObject]] = None,
    functions: Optional[list[JsonObject]] = None,
    methods: Optional[list[JsonObject]] = None,
    attributes: Optional[list[JsonObject]] = None,
    settings: Optional[list[JsonObject]] = None,
    mutations: Optional[list[JsonObject]] = None,
    constructors: Optional[list[JsonObject]] = None,
) -> JsonObject:
    """`mutations` takes `class_claim_*` entries; `decorators`/`functions` take `symbol_claim`s."""
    result = {}
    groups = (
        ("modules", modules),
        ("classes", classes),
        ("constructors", constructors),
        ("decorators", decorators),
        ("functions", functions),
        ("methods", methods),
        ("attributes", attributes),
        ("settings", settings),
        ("mutations", mutations),
    )
    for key, items in groups:
        if items:
            result[key] = items
    return result


def stub_overlay(module: str, path: str, sha256: Optional[str] = None) -> JsonObject:
    overlay = {"module": module, "path": path}
    if sha256 is not None:
        overlay["sha256"] = sha256
    return overlay


# Contributions — entries in a `project_index(contributions=[...])` response.


def class_target(qualified_name: str) -> JsonObject:
    return {"kind": "class", "qualified-name": qualified_name}


def instance_target(qualified_name: str) -> JsonObject:
    return {"kind": "instance", "qualified-name": qualified_name}


def constructor_target(qualified_name: str) -> JsonObject:
    return {"kind": "constructor", "qualified-name": qualified_name}


def member_contribution(patch: JsonObject) -> JsonObject:
    result = {"kind": "member"}
    result.update(patch)
    return result


def field_contribution(patch: JsonObject) -> JsonObject:
    result = {"kind": "field"}
    result.update(patch)
    return result


def constructor_contribution(signature: JsonObject) -> JsonObject:
    result = {"kind": "constructor"}
    result.update(signature)
    return result


def diagnostic_contribution(diag: JsonObject) -> JsonObject:
    result = {"kind": "diagnostic"}
    result.update(diag)
    return result


def contribution(
    source: JsonObject,
    target: JsonObject,
    patch: JsonObject,
    conflict_key: str,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    """`conflict_key` deduplicates overlapping contributions."""
    result = {
        "source": source,
        "target": target,
        "patch": patch,
        "conflict-key": conflict_key,
    }
    if diagnostics:
        result["diagnostics"] = diagnostics
    return result


# Virtual types — plugin-declared types in `project_index(virtual_types=[...])`.


def virtual_field(
    name: str, type: JsonObject, required: bool = True, read_only: bool = False
) -> JsonObject:
    return {"name": name, "type-expr": type, "required": required, "read-only": read_only}


def virtual_class(
    bases: Optional[list[JsonObject]] = None,
    members: Optional[list[JsonObject]] = None,
) -> JsonObject:
    shape = {"kind": "class"}
    if bases:
        shape["bases"] = bases
    if members:
        shape["members"] = members
    return shape


def virtual_typed_dict(
    fields: Optional[list[JsonObject]] = None, total: bool = True
) -> JsonObject:
    shape = {"kind": "typed-dict", "total": total}
    if fields:
        shape["fields"] = fields
    return shape


def virtual_named_tuple(fields: Optional[list[JsonObject]] = None) -> JsonObject:
    shape = {"kind": "named-tuple"}
    if fields:
        shape["fields"] = fields
    return shape


def virtual_type(
    name: str, shape: JsonObject, metadata: Optional[JsonObject] = None
) -> JsonObject:
    definition = {"name": name, "shape": shape}
    if metadata is not None:
        definition["metadata"] = metadata
    return definition


# Response builders — each returns the tagged dict the host expects.


def no_change() -> JsonObject:
    return {"kind": "no-change"}


def error(message: str, diagnostic: Optional[JsonObject] = None) -> JsonObject:
    err = {"kind": "error", "message": message}
    if diagnostic is not None:
        err["diagnostic"] = diagnostic
    return err


def class_patch(
    fields: Optional[list[JsonObject]] = None,
    class_members: Optional[list[JsonObject]] = None,
    instance_members: Optional[list[JsonObject]] = None,
    constructor: Optional[JsonObject] = None,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    patch = {"kind": "class-patch"}
    if fields:
        patch["fields"] = fields
    if class_members:
        patch["class-members"] = class_members
    if instance_members:
        patch["instance-members"] = instance_members
    if constructor is not None:
        patch["constructor"] = constructor
    if diagnostics:
        patch["diagnostics"] = diagnostics
    return patch


def member_response(patch: JsonObject) -> JsonObject:
    response = {"kind": "member-patch"}
    response.update(patch)
    return response


def call_signature_patch(
    signature: JsonObject, diagnostics: Optional[list[JsonObject]] = None
) -> JsonObject:
    patch = {"kind": "call-signature-patch", "signature": signature}
    if diagnostics:
        patch["diagnostics"] = diagnostics
    return patch


def call_return_patch(
    return_type: JsonObject,
    diagnostics: Optional[list[JsonObject]] = None,
    result_metadata: Optional[JsonObject] = None,
) -> JsonObject:
    patch = {"kind": "call-return-patch", "return-type": return_type}
    if diagnostics:
        patch["diagnostics"] = diagnostics
    if result_metadata is not None:
        patch["result-metadata"] = result_metadata
    return patch


def call_state_patch(
    receiver_members: Optional[JsonObject] = None,
    result_members: Optional[JsonObject] = None,
    *,
    fresh_result: bool = False,
    preserves_other_objects: bool = False,
) -> JsonObject:
    """Member facts after successful completion; result facts require a fresh object."""
    return {
        "kind": "call-state-patch",
        "receiver-members": receiver_members or {},
        "result-members": result_members or {},
        "fresh-result": fresh_result,
        "preserves-other-objects": preserves_other_objects,
    }


def project_index(
    plugin_index: Optional[JsonObject] = None,
    contributions: Optional[list[JsonObject]] = None,
    virtual_types: Optional[list[JsonObject]] = None,
    dependencies: Optional[list[JsonObject]] = None,
    diagnostics: Optional[list[JsonObject]] = None,
) -> JsonObject:
    index = {"kind": "project-index", "plugin-index": plugin_index if plugin_index is not None else {}}
    if contributions:
        index["contributions"] = contributions
    if virtual_types:
        index["virtual-types"] = virtual_types
    if dependencies:
        index["dependencies"] = dependencies
    if diagnostics:
        index["diagnostics"] = diagnostics
    return index


def dependencies(items: list[JsonObject]) -> JsonObject:
    return {"kind": "dependencies", "dependencies": items}


def mutation_diagnostics(items: list[JsonObject]) -> JsonObject:
    return {"kind": "mutation-diagnostics", "diagnostics": items}


def manifest_response(manifest_dict: JsonObject) -> JsonObject:
    response = {"kind": "manifest"}
    response.update(manifest_dict)
    return response


# Manifest builder.


def manifest(
    id: str,
    name: str,
    version: str,
    ty_compatibility: str = ">=0.0.0",
    capabilities: Optional[JsonObject] = None,
    claims: Optional[JsonObject] = None,
    runtime: Optional[JsonObject] = None,
    config_schema: Optional[JsonObject] = None,
    default_config: Optional[JsonObject] = None,
    stub_overlays: Optional[list[JsonObject]] = None,
    protocol_version: Optional[JsonObject] = None,
) -> JsonObject:
    manifest = {
        "id": id,
        "name": name,
        "version": version,
        "protocol-version": protocol_version or dict(PROTOCOL_VERSION),
        "ty-compatibility": {"requirement": ty_compatibility},
        "runtime": runtime or {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": capabilities or {},
        "claims": claims or {},
    }
    if config_schema is not None:
        manifest["config-schema"] = config_schema
    if default_config is not None:
        manifest["default-config"] = default_config
    if stub_overlays:
        manifest["stub-overlays"] = stub_overlays
    return manifest


def capabilities(**flags: bool) -> JsonObject:
    """e.g. `capabilities(call_return=True)`; underscores serialize as kebab-case."""
    return {key.replace("_", "-"): value for key, value in flags.items()}


# Dispatch — invoked by the host as `__ty_handle__(request_json) -> str`.


def __ty_handle__(request_json: str) -> str:
    try:
        request: Request = json.loads(request_json)
    except ValueError as exc:
        return json.dumps(error(f"invalid request JSON: {exc}"))

    kind = request.get("kind")
    if kind == "manifest":
        try:
            produced = None
            for hook in _hooks.get("manifest") or ():
                produced = hook(request)
                if produced is not None:
                    break
            if produced is None:
                produced = _MANIFEST
            return json.dumps(manifest_response(produced) if produced else no_change())
        except Exception as exc:  # noqa: BLE001 — plugin errors surface as protocol errors
            return json.dumps(error(str(exc)))

    handlers = _hooks.get(kind) or ()
    if not handlers:
        return json.dumps(no_change())

    for hook in handlers:
        try:
            result = hook(request)
        except Exception as exc:  # noqa: BLE001
            return json.dumps(error(str(exc)))
        if result is not None:
            return json.dumps(result)

    return json.dumps(no_change())
