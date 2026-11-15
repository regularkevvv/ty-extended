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
_VARIADIC = ("var-args", "kwargs")


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
        if parameter.get("kind") in _VARIADIC:
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
        if argument.get("kind") in _VARIADIC:
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
        if kind in _VARIADIC:
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
    result = _callable(
        [
            snapshot_parameter(
                name="instance", type_snapshot=first, kind="positional-only"
            )
        ],
        last,
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
    ),
}


def _generator_yield(snapshot):
    """`Generator[X, ...]`/`Iterator[X]`/callable returning them → `X`."""
    kind = snapshot.get("kind")
    if kind == "nominal":
        tail = (snapshot.get("qualified-name") or "").rsplit(".", 1)[-1]
        if tail in ("Generator", "Iterator", "Iterable"):
            arguments = snapshot.get("arguments") or []
            if arguments:
                return arguments[0]
    if kind == "callable":
        return _generator_yield(snapshot.get("return-type") or {})
    return None


def _do(request):
    receiver = (request.get("receiver") or {}).get("nominal-class") or ""
    owner = receiver.split("[")[0]
    family = _DO_FAMILIES.get(owner)
    if family is None:
        return None
    arguments = request.get("arguments") or []
    if not arguments:
        return None
    yielded = _generator_yield(_snapshot(arguments[0]))
    if yielded is None:
        return None  # Unannotated generators give the host nothing to check.
    diagnostics = []
    errors = []
    if yielded.get("kind") == "union":
        items = yielded.get("elements") or []
    else:
        items = [yielded]
    for item in items:
        if item.get("kind") != "nominal":
            continue
        name = item.get("qualified-name") or ""
        if name not in family:
            diagnostics.append(
                diagnostic(
                    "returns.do",
                    'Invalid type supplied in do-notation: expected "{0}", got "{1}"'.format(
                        owner, name
                    ),
                    severity="error",
                )
            )
            continue
        args = item.get("arguments") or []
        if len(args) >= 2:
            errors.append(args[1])
    if diagnostics:
        return _default_patch(request, diagnostics=diagnostics)
    result = (_default_return(request) or {}).get("snapshot")
    if errors and (result or {}).get("kind") == "nominal":
        args = list(result.get("arguments") or [])
        if len(args) >= 2:
            args[1] = (
                errors[0] if len(errors) == 1 else {"kind": "union", "elements": errors}
            )
            patched = _nominal(result.get("qualified-name"), args)
            return call_return_patch(type_expr_from_snapshot(patched))
    return None


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
