"""returns plugin for ty-extended — a Monty port of the upstream mypy plugin.

Mirrors `returns.contrib.mypy.returns_plugin` hooks:

- ``curry``         — all contiguous argument groupings as an overloaded callable
- ``partial``       — applied-argument subtraction with keyword-hole handling
- ``flow``          — pipeline step chaining with structural mismatch checks
- ``pipe``          — pipeline composition into a plain callable type
- ``dekind``/kinded — ``KindN[I, a, b]`` → ``I[a, b]`` recursive translation
- ``*.do``          — container-family validation and error-union inference

Where mypy's plugin re-runs the expression checker for subtype checks and
constraint solving, this port works structurally on protocol 0.10 `callable`
snapshots; genuinely unanswerable comparisons abstain instead of guessing.
"""

import typing

if typing.TYPE_CHECKING:
    from ty_extended.plugin_sdk import (
        call_return_patch,
        diagnostic,
        on_call_return,
        snapshot_callable,
        snapshot_overloaded,
        snapshot_parameter,
        snapshot_to_expression,
        type_annotation,
        type_expr_from_snapshot,
    )

_POSITIONAL = ("positional-only", "positional-or-keyword")
_PARAM_VARIADIC = ("var-args", "kwargs")
_ARG_VARIADIC = ("star-args", "star-kwargs")


def _snapshot(argument):
    return ((argument or {}).get("type-expr") or {}).get("snapshot") or {}


def _copy(value):
    """Monty-safe shallow dict copy."""
    result = {}
    for key in value:
        result[key] = value[key]
    return result


def _callable(parameters, return_type, type_params=None):
    return snapshot_callable(
        parameters=parameters,
        return_type=return_type,
        type_params=type_params,
    )


def _nominal(qualified_name, arguments=None):
    snap = {"kind": "nominal", "qualified-name": qualified_name}
    if arguments:
        snap["arguments"] = arguments
    return snap


def _first_param_type(snapshot):
    for parameter in snapshot.get("parameters") or []:
        if parameter.get("kind") in _POSITIONAL:
            return parameter.get("type-snapshot") or {
                "kind": "expression",
                "expression": "Unknown",
                "mode": "annotation",
            }
    return {"kind": "expression", "expression": "Unknown", "mode": "annotation"}


def _same_nominal(left, right):
    """Structural approximation of `is_subtype` for pipeline validation.

    Only *proves* mismatch when both sides are nominal with different
    qualified names; everything else is inconclusive (skipped).
    """
    if left.get("kind") == "nominal" and right.get("kind") == "nominal":
        return left.get("qualified-name") == right.get("qualified-name")
    return True


def _default_return(request):
    return request.get("default-return-type") or type_annotation("object")


def _default_patch(request, diagnostics=None):
    return call_return_patch(_default_return(request), diagnostics=diagnostics)


# ---------------------------------------------------------------------------
# curry — returns.curry.curry (function calls and bare @curry decorators)


def _slices(params):
    """All contiguous groupings, upstream itertools.product order."""
    count = len(params)
    result = []
    for mask in range((1 << (count - 1)) - 1, -1, -1) if count > 1 else range(1):
        groups = []
        start = 0
        for index in range(1, count):
            if mask & (1 << (count - 1 - index)):
                groups.append(params[start:index])
                start = index
        groups.append(params[start:])
        result.append(groups)
    return result


def _curry(request):
    arguments = request.get("arguments") or []
    if not arguments:
        return None
    snapshot = _snapshot(arguments[0])
    if snapshot.get("kind") != "callable":
        return None
    params = snapshot.get("parameters") or []
    if not params:
        return None
    for parameter in params:
        if parameter.get("kind") in _PARAM_VARIADIC:
            # Upstream refuses to curry *args/**kwargs and returns Any.
            return None
    returns = snapshot.get("return-type") or {
        "kind": "expression",
        "expression": "Unknown",
        "mode": "annotation",
    }
    type_params = snapshot.get("type-params") or None
    cases = []
    for groups in _slices(params):
        case = returns
        depth = len(groups)
        for index in range(depth - 1, -1, -1):
            # Type variables belong on the outermost level only: inner
            # `__call__` generics would rebind per application.
            case = _callable(
                groups[index],
                case,
                type_params=type_params if index == 0 else None,
            )
        cases.append(case)
    result = cases[0] if len(cases) == 1 else snapshot_overloaded(cases)
    return call_return_patch(type_expr_from_snapshot(result))


# ---------------------------------------------------------------------------
# partial — returns.curry.partial


def _partial(request):
    arguments = request.get("arguments") or []
    if len(arguments) < 2:
        return None  # `partial(func)` — the default return is already right.
    snapshot = _snapshot(arguments[0])
    if snapshot.get("kind") != "callable":
        return None
    params = snapshot.get("parameters") or []
    applied = arguments[1:]
    for argument in applied:
        if argument.get("kind") in _ARG_VARIADIC:
            return None  # Literal *args/**kwargs partials are unknowable.
    positional = 0
    names = []
    for argument in applied:
        if argument.get("name"):
            names.append(argument["name"])
        else:
            positional += 1

    # `Functions.diff`: a positional parameter applied by keyword opens a
    # keyword hole — every later positional parameter turns keyword-only and
    # *args becomes unreachable.
    hole_at = len(params)
    seen = 0
    for index in range(len(params)):
        parameter = params[index]
        if parameter.get("kind") not in _POSITIONAL:
            continue
        applied_here = index < positional or parameter.get("name") in names
        if applied_here and seen >= positional:
            hole_at = index
            break
        seen += 1

    remaining = []
    for index in range(len(params)):
        parameter = params[index]
        kind = parameter.get("kind")
        if kind == "var-args" and index > hole_at:
            continue
        if kind in _PARAM_VARIADIC:
            remaining.append(parameter)
            continue
        applied_here = index < positional or parameter.get("name") in names
        if applied_here:
            continue
        if index > hole_at and kind in _POSITIONAL:
            parameter = _copy(parameter)
            parameter["kind"] = "keyword-only"
        remaining.append(parameter)

    result = _callable(
        remaining,
        snapshot.get("return-type")
        or {"kind": "expression", "expression": "Unknown", "mode": "annotation"},
        type_params=snapshot.get("type-params") or None,
    )
    return call_return_patch(type_expr_from_snapshot(result))


# ---------------------------------------------------------------------------
# flow / pipe — returns._internal.pipeline


def _chain_diagnostics(leading, steps):
    diagnostics = []
    previous = leading
    for index in range(len(steps)):
        expected = _first_param_type(steps[index])
        if not _same_nominal(expected, previous):
            diagnostics.append(
                diagnostic(
                    "returns.pipeline-step",
                    "Pipeline step {0} expects {1}, got {2}".format(
                        index + 1,
                        snapshot_to_expression(expected),
                        snapshot_to_expression(previous),
                    ),
                    severity="error",
                )
            )
        previous = steps[index].get("return-type") or {
            "kind": "expression",
            "expression": "Unknown",
            "mode": "annotation",
        }
    return diagnostics, previous


def _flow(request):
    arguments = request.get("arguments") or []
    if len(arguments) < 2:
        return _default_patch(
            request,
            diagnostics=[diagnostic("returns.flow", 'Too few arguments for "flow"')],
        )
    steps = [_snapshot(argument) for argument in arguments[1:]]
    for step in steps:
        if step.get("kind") != "callable":
            return None
    diagnostics, final = _chain_diagnostics(_snapshot(arguments[0]), steps)
    return call_return_patch(
        type_expr_from_snapshot(final), diagnostics=diagnostics or None
    )


def _pipe(request):
    arguments = request.get("arguments") or []
    if not arguments:
        return _default_patch(
            request,
            diagnostics=[diagnostic("returns.pipe", 'Too few arguments for "pipe"')],
        )
    steps = [_snapshot(argument) for argument in arguments]
    for step in steps:
        if step.get("kind") != "callable":
            return None
    first = _first_param_type(steps[0])
    last = steps[-1].get("return-type") or {
        "kind": "expression",
        "expression": "Unknown",
        "mode": "annotation",
    }
    diagnostics, _unused = _chain_diagnostics(
        steps[0].get("return-type") or first, steps[1:]
    )
    # The piped callable must redeclare the type parameters it references —
    # otherwise the rendered `__call__` mentions unbound type variables.
    type_params = []
    seen = set()
    for step in steps:
        for type_param in step.get("type-params") or []:
            name = type_param.get("name")
            if name not in seen:
                seen.add(name)
                type_params.append(type_param)
    result = _callable(
        [
            snapshot_parameter(
                name="instance", type_snapshot=first, kind="positional-only"
            )
        ],
        last,
        type_params=type_params or None,
    )
    return call_return_patch(
        type_expr_from_snapshot(result), diagnostics=diagnostics or None
    )


# ---------------------------------------------------------------------------
# do notation — *.do methods


_DO_FAMILIES = {
    "returns.result.Result": (
        "returns.result.Result",
        "returns.result.ResultE",
        "returns.result.Success",
        "returns.result.Failure",
    ),
    "returns.maybe.Maybe": (
        "returns.maybe.Maybe",
        "returns.maybe.Some",
        "returns.maybe._Nothing",
    ),
    "returns.io.IO": ("returns.io.IO",),
    "returns.io.IOResult": (
        "returns.io.IOResult",
        "returns.io.IOResultE",
        "returns.io.IOSuccess",
        "returns.io.IOFailure",
    ),
    "returns.future.Future": ("returns.future.Future",),
    "returns.future.FutureResult": (
        "returns.future.FutureResult",
        "returns.future.FutureResultE",
        "returns.future.FutureSuccess",
        "returns.future.FutureFailure",
    ),
}

# Owners with two type parameters: ``Owner[T, E]`` lets ``do`` infer the error
# union; single-parameter owners only re-yield the value type.
_DO_ARITY = {
    "returns.result.Result": 2,
    "returns.maybe.Maybe": 1,
    "returns.io.IO": 1,
    "returns.io.IOResult": 2,
    "returns.future.Future": 1,
    "returns.future.FutureResult": 2,
}

# Constructors whose parameters hold the VALUE only (`Success[T]`, `Some[T]`,
# `IOSuccess[T]`, `FutureSuccess[T]`, `_Nothing`) carry no extractable error.
# Everything else with one argument (`Failure[E]`, `ResultE[E]`, `IOFailure[E]`,
# `IOResultE[E]`, `FutureFailure[E]`, `FutureResultE[E]`) holds the error at
# position 0; two-argument containers hold it at position 1.
_DO_VALUE_ONLY = {
    "returns.result.Success",
    "returns.maybe.Some",
    "returns.maybe._Nothing",
    "returns.io.IOSuccess",
    "returns.future.FutureSuccess",
}


def _dedupe(snapshots):
    seen = set()
    unique = []
    for snapshot in snapshots:
        key = snapshot_to_expression(snapshot)
        if key not in seen:
            seen.add(key)
            unique.append(snapshot)
    return unique


def _do_in_family(snapshot, family):
    """Structural `is_subtype(seq, Owner)` approximation for `do` clauses."""
    kind = snapshot.get("kind")
    if kind == "union":
        return all(
            _do_in_family(element, family) for element in snapshot.get("elements") or []
        )
    if kind == "nominal":
        return (snapshot.get("qualified-name") or "") in family
    # Expression/unknown snapshots are inconclusive — like `Any`, they pass.
    return True


def _do_error_type(snapshot):
    """Upstream `_extract_error_type`: the error param of a family member."""
    if snapshot.get("kind") == "union":
        parts = []
        for element in snapshot.get("elements") or []:
            error = _do_error_type(element)
            if error is not None:
                parts.append(error)
        if not parts:
            return None
        unique = _dedupe(parts)
        if len(unique) == 1:
            return unique[0]
        return {"kind": "union", "elements": unique}
    name = snapshot.get("qualified-name") or ""
    if name in _DO_VALUE_ONLY or snapshot.get("kind") != "nominal":
        return None
    args = snapshot.get("arguments") or []
    if not args:
        return None
    return args[1] if len(args) >= 2 else args[0]


def _do(request):
    callee = ((request.get("callee") or {}).get("expression")) or ""
    if not callee.endswith(".do"):
        return None
    owner = callee[: -len(".do")]
    family = _DO_FAMILIES.get(owner)
    if family is None:
        return None
    arguments = request.get("arguments") or []
    if not arguments:
        return None
    generator = arguments[0].get("generator")
    if generator is None:
        return _default_patch(
            request,
            diagnostics=[
                diagnostic(
                    "returns.do",
                    "Literal generator expression is required, not a variable or function call",
                    severity="error",
                    location=arguments[0].get("source"),
                )
            ],
        )
    diagnostics = []
    errors = []
    for clause in generator.get("clauses") or []:
        iterated = ((clause.get("iter-type") or {}).get("snapshot")) or {}
        if not _do_in_family(iterated, family):
            arity = _DO_ARITY.get(owner) or 1
            diagnostics.append(
                diagnostic(
                    "returns.do",
                    'Invalid type supplied in do-notation: expected "{0}[{1}]", got "{2}"'.format(
                        owner,
                        ", ".join(["Any"] * arity),
                        snapshot_to_expression(iterated) or "Unknown",
                    ),
                    severity="error",
                    location=clause.get("iter-source"),
                )
            )
            continue
        error = _do_error_type(iterated)
        if error is not None:
            errors.append(error)
        if clause.get("conditions"):
            diagnostics.append(
                diagnostic(
                    "returns.do",
                    'Using "if" conditions inside a generator is not allowed',
                    severity="error",
                    location=generator.get("yield-source"),
                )
            )
    if diagnostics:
        return _default_patch(request, diagnostics=diagnostics)
    result = (_default_return(request) or {}).get("snapshot")
    if (result or {}).get("kind") != "nominal":
        return None
    errors = _dedupe(errors)
    args = list(result.get("arguments") or [])
    if len(args) < 2:
        return None
    if not errors:
        # Upstream leaves the unsolved error variable to mypy, which reports
        # `Never`; ty reports `Unknown`. Normalize to `Never`.
        errors = [{"kind": "union", "elements": []}]
    args[1] = errors[0] if len(errors) == 1 else {"kind": "union", "elements": errors}
    patched = _nominal(result.get("qualified-name"), args)
    return call_return_patch(type_expr_from_snapshot(patched))


# ---------------------------------------------------------------------------
# kind — returns.primitives.hkt


def _translate_kind(snapshot):
    """Recursive `KindN[I, a, b]` → `I[a, b]` (upstream `translate_kind_instance`)."""
    kind = snapshot.get("kind")
    if kind == "nominal":
        name = snapshot.get("qualified-name") or ""
        args = []
        for arg in snapshot.get("arguments") or []:
            args.append(_translate_kind(arg))
        if name == "returns.primitives.hkt.KindN" and args:
            inner = args[0]
            if inner.get("kind") == "nominal":
                inner_args = inner.get("arguments") or []
                result = _copy(inner)
                result["arguments"] = args[1 : len(inner_args) + 1]
                return result
            return inner
        result = _copy(snapshot)
        result["arguments"] = args
        return result
    if kind == "union":
        result = _copy(snapshot)
        result["elements"] = []
        for element in snapshot.get("elements") or []:
            result["elements"].append(_translate_kind(element))
        return result
    if kind == "tuple":
        result = _copy(snapshot)
        for key in ("prefix", "suffix"):
            result[key] = []
            for element in snapshot.get(key) or []:
                result[key].append(_translate_kind(element))
        if snapshot.get("variadic") is not None:
            result["variadic"] = _translate_kind(snapshot["variadic"])
        return result
    if kind == "callable":
        result = _copy(snapshot)
        result["parameters"] = []
        for parameter in snapshot.get("parameters") or []:
            parameter = _copy(parameter)
            if parameter.get("type-snapshot"):
                parameter["type-snapshot"] = _translate_kind(parameter["type-snapshot"])
            result["parameters"].append(parameter)
        if snapshot.get("return-type"):
            result["return-type"] = _translate_kind(snapshot["return-type"])
        return result
    if kind == "overloaded":
        result = _copy(snapshot)
        result["elements"] = []
        for element in snapshot.get("elements") or []:
            result["elements"].append(_translate_kind(element))
        return result
    return snapshot


def _dekind(request):
    arguments = request.get("arguments") or []
    if not arguments:
        return None
    kind = _snapshot(arguments[0])
    if kind.get("kind") != "nominal" or not kind.get("arguments"):
        return None
    result = _translate_kind(kind)
    if result is kind:
        return None
    return call_return_patch(type_expr_from_snapshot(result))


def _kinded_call(request):
    """`Kinded[F]` call: `F`'s return type with `KindN` translated."""
    default = (_default_return(request) or {}).get("snapshot")
    if not default:
        return None
    return call_return_patch(type_expr_from_snapshot(_translate_kind(default)))


# ---------------------------------------------------------------------------
# dispatch


@on_call_return
def adjust_call_return(request):
    callee = ((request.get("callee") or {}).get("expression")) or ""
    if callee == "returns.curry.curry":
        return _curry(request)
    if callee == "returns.curry.partial":
        return _partial(request)
    if callee in ("returns._internal.pipeline.flow.flow", "returns.pipeline.flow"):
        return _flow(request)
    if callee in ("returns._internal.pipeline.pipe.pipe", "returns.pipeline.pipe"):
        return _pipe(request)
    if callee == "returns.primitives.hkt.dekind":
        return _dekind(request)
    if callee == "returns.primitives.hkt.Kinded.__call__":
        return _kinded_call(request)
    if callee.endswith(".do"):
        return _do(request)
    return None
