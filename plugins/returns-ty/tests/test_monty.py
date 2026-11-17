"""Unit tests for the Monty plugin helpers.

monty.py executes under Monty with the ty plugin SDK injected into its
globals; the tests stub the handful of SDK names the module calls.
"""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

MONTY = Path(__file__).resolve().parent.parent / "python" / "returns_ty" / "monty.py"


def load_module():
    spec = importlib.util.spec_from_file_location("returns_ty_monty", MONTY)
    module = importlib.util.module_from_spec(spec)
    module.call_return_patch = lambda *args, **kwargs: ("patch", args, kwargs)
    module.diagnostic = lambda *args, **kwargs: ("diagnostic", args, kwargs)
    module.on_call_return = lambda *args, **kwargs: None
    module.snapshot_callable = lambda **kwargs: {"kind": "callable", **kwargs}
    module.snapshot_overloaded = lambda elements: {
        "kind": "overloaded",
        "elements": elements,
    }
    module.snapshot_parameter = lambda **kwargs: dict(kwargs)
    module.snapshot_to_expression = lambda snapshot: snapshot.get(
        "qualified-name", "expr"
    )
    module.type_annotation = lambda snapshot: {"snapshot": snapshot}
    module.type_expr_from_snapshot = lambda snapshot: {"snapshot": snapshot}
    spec.loader.exec_module(module)
    return module


def nominal(name):
    return {"kind": "nominal", "qualified-name": name, "arguments": []}


def callable_snapshot(*params, return_type="builtins.str", type_params=None):
    return {
        "kind": "callable",
        "parameters": [
            {
                "name": name,
                "kind": kind,
                "required": required,
                "type-snapshot": nominal(typ),
            }
            for name, kind, required, typ in params
        ],
        "return-type": nominal(return_type),
        "type-params": type_params or [],
        "ellipsis": False,
    }


def argument(typ=None, name=None):
    snapshot = typ if isinstance(typ, dict) else nominal(typ or "builtins.int")
    return {"type-expr": {"snapshot": snapshot}, "name": name}


def request(*arguments, default=None):
    return {
        "arguments": list(arguments),
        "default-return-type": default or type_annotation_like(),
    }


def type_annotation_like():
    return {"snapshot": nominal("builtins.object")}


class CurryTests(unittest.TestCase):
    def setUp(self):
        self.monty = load_module()

    def test_slices_partition_groupings(self):
        result = self.monty._slices([1, 2, 3])
        self.assertEqual(
            result,
            [
                [[1], [2], [3]],
                [[1], [2, 3]],
                [[1, 2], [3]],
                [[1, 2, 3]],
            ],
        )

    def test_slices_single(self):
        self.assertEqual(self.monty._slices([1]), [[[1]]])

    def test_curry_produces_overloads(self):
        signature = callable_snapshot(
            ("a", "positional-or-keyword", True, "builtins.int"),
            ("b", "positional-or-keyword", True, "builtins.str"),
        )
        patch = self.monty._curry(request(argument(signature)))
        self.assertEqual(patch[0], "patch")
        snapshot = patch[1][0]["snapshot"]
        self.assertEqual(snapshot["kind"], "overloaded")
        self.assertEqual(len(snapshot["elements"]), 2)

    def test_curry_zero_params_abstains(self):
        self.assertIsNone(self.monty._curry(request(argument(callable_snapshot()))))


class PartialTests(unittest.TestCase):
    def setUp(self):
        self.monty = load_module()

    def test_partial_removes_bound_param(self):
        signature = callable_snapshot(
            ("a", "positional-or-keyword", True, "builtins.int"),
            ("b", "positional-or-keyword", True, "builtins.str"),
        )
        patch = self.monty._partial(
            request(argument(signature), argument("builtins.int"))
        )
        parameters = patch[1][0]["snapshot"]["parameters"]
        self.assertEqual([p["name"] for p in parameters], ["b"])

    def test_partial_keyword_hole_makes_rest_keyword_only(self):
        signature = callable_snapshot(
            ("a", "positional-or-keyword", True, "builtins.int"),
            ("b", "positional-or-keyword", True, "builtins.str"),
            ("c", "positional-or-keyword", True, "builtins.float"),
        )
        patch = self.monty._partial(
            request(argument(signature), argument("builtins.str", name="b"))
        )
        parameters = patch[1][0]["snapshot"]["parameters"]
        self.assertEqual([p["name"] for p in parameters], ["a", "c"])
        self.assertEqual(parameters[1]["kind"], "keyword-only")


class FlowTests(unittest.TestCase):
    def setUp(self):
        self.monty = load_module()

    def test_chain_diagnostics_match(self):
        steps = [
            callable_snapshot(
                ("a", "positional-only", True, "builtins.int"),
                return_type="builtins.str",
            ),
            callable_snapshot(
                ("b", "positional-only", True, "builtins.str"),
                return_type="builtins.float",
            ),
        ]
        diagnostics, previous = self.monty._chain_diagnostics(
            nominal("builtins.int"), steps
        )
        self.assertEqual(diagnostics, [])
        self.assertEqual(previous, nominal("builtins.float"))

    def test_chain_diagnostics_mismatch(self):
        steps = [
            callable_snapshot(
                ("a", "positional-only", True, "builtins.int"),
                return_type="builtins.str",
            ),
            callable_snapshot(
                ("b", "positional-only", True, "builtins.float"),
                return_type="builtins.float",
            ),
        ]
        diagnostics, _ = self.monty._chain_diagnostics(nominal("builtins.int"), steps)
        self.assertEqual(len(diagnostics), 1)

    def test_flow_returns_last_return_type(self):
        steps = [
            callable_snapshot(
                ("a", "positional-only", True, "builtins.int"),
                return_type="builtins.str",
            ),
            callable_snapshot(
                ("b", "positional-only", True, "builtins.str"),
                return_type="builtins.float",
            ),
        ]
        patch = self.monty._flow(
            request(
                argument(nominal("builtins.int")), *[argument(step) for step in steps]
            )
        )
        snapshot = patch[1][0]["snapshot"]
        self.assertEqual(snapshot, nominal("builtins.float"))


if __name__ == "__main__":
    unittest.main()
