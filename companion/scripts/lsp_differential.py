#!/usr/bin/env python3
"""Compare the Rust editor proxy with the reference fork over real LSP.

Opaque protocol data is used for follow-up requests but excluded from display
comparisons. All source positions and edits are retained in comparisons.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
from pathlib import Path

from lsp_acceptance import Client, position


class DifferentialClient(Client):
    def send(self, message):
        if message.get("method") in {"shutdown", "exit"}:
            message = {**message, "params": None}
        super().send(message)


SOURCE = '''from typing import assert_type
from helpers import helper

class Resource:
    """Resource documentation."""
    status: str | None = None
    def activate(self) -> None:
        """Activate this resource."""
        pass
    def release(self) -> None: pass

def decode(value: object) -> object:
    """Decode the value, preserving useful documentation."""
    return value

item = Resource()
item.activate()
item.label
item.status
item.token
state = item.status
value = decode("value")
wrong: str = value
local = 1
other = local
emoji = "😀"; last = local
answer = helper(local)

def on_change(sender: object, **kwargs: object) -> None:
    pass

assert_type(1, str)
'''


def canonical(value):
    if isinstance(value, list):
        return [canonical(item) for item in value]
    if isinstance(value, dict):
        return {
            key: canonical(item)
            for key, item in value.items()
            if key not in {"data", "resultId", "version"}
        }
    return value


def hover(value):
    if not value:
        return value
    contents = value["contents"]
    if isinstance(contents, dict):
        contents = contents.get("value", "")
    if isinstance(contents, list):
        contents = "\n\n".join(
            item if isinstance(item, str) else item.get("value", "")
            for item in contents
        )
    # Normalise only insignificant signature whitespace. Documentation and
    # source range are kept, including the exact typed return annotation.
    contents = re.sub(r"[ \t]+\n", "\n", contents).strip()
    return {"contents": contents, "range": value.get("range")}


def tokens(result, legend):
    if result is None:
        return None
    raw = result["data"]
    line = column = 0
    decoded = []
    for index in range(0, len(raw), 5):
        dl, dc, length, kind, modifiers = raw[index : index + 5]
        line += dl
        column = column + dc if dl == 0 else dc
        decoded.append(
            [
                line,
                column,
                length,
                legend["tokenTypes"][kind],
                [
                    name
                    for bit, name in enumerate(legend["tokenModifiers"])
                    if modifiers & (1 << bit)
                ],
            ]
        )
    return decoded


def edit(value):
    if not value:
        return value
    changes = {}
    for uri, edits in value.get("changes", {}).items():
        changes.setdefault(uri, []).extend(canonical(edits))
    for change in value.get("documentChanges", []):
        if "textDocument" not in change:
            raise AssertionError(f"unexpected resource operation: {change}")
        changes.setdefault(change["textDocument"]["uri"], []).extend(
            canonical(change["edits"])
        )
    for edits in changes.values():
        edits.sort(key=lambda item: json.dumps(item, sort_keys=True))
    return changes


def locations(value):
    # LSP explicitly permits null and an empty result list for no target.
    return canonical(value) if value else None


def run(args, encoding):
    case = args.output / encoding
    root = case / "project"
    stubs = case / "stubs"
    root.mkdir(parents=True)
    stubs.mkdir()
    source = root / "demo.py"
    source.write_text(SOURCE)
    (root / "helpers.py").write_text(
        'def helper(value: int) -> int:\n    """Return a helper value."""\n    return value\n'
    )
    before = {
        path.name: hashlib.sha256(path.read_bytes()).hexdigest()
        for path in root.glob("*.py")
    }
    fixture = Path(__file__).resolve().parents[1] / "tests/fixtures/generic"
    fork_config = case / "fork.toml"
    fork_config.write_text(
        '[environment]\npython-version = "3.13"\n[plugins]\nenabled = true\nauto-discover = false\n[[plugins.plugin]]\n'
        f'id = "research.generic"\nruntime = "monty"\ntrusted = true\npath = {json.dumps(str(fixture / "plugin.py"))}\n'
        f"manifest-path = {json.dumps(str(fixture / 'plugin.plugin.json'))}\n"
    )
    caps = {
        "general": {"positionEncodings": [encoding]},
        "workspace": {"configuration": False, "workspaceFolders": True},
        "textDocument": {
            "diagnostic": {},
            "hover": {"contentFormat": ["markdown"]},
            "completion": {
                "completionItem": {
                    "snippetSupport": True,
                    "resolveSupport": {
                        "properties": ["documentation", "detail", "additionalTextEdits"]
                    },
                }
            },
            "signatureHelp": {
                "signatureInformation": {
                    "documentationFormat": ["markdown"],
                    "parameterInformation": {"labelOffsetSupport": True},
                }
            },
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
            "definition": {"linkSupport": True},
            "codeAction": {
                "resolveSupport": {"properties": ["edit"]},
                "codeActionLiteralSupport": {
                    "codeActionKind": {"valueSet": ["quickfix"]}
                },
            },
            "semanticTokens": {
                "requests": {"full": True},
                "tokenTypes": [
                    "namespace",
                    "type",
                    "class",
                    "enum",
                    "interface",
                    "struct",
                    "typeParameter",
                    "parameter",
                    "variable",
                    "property",
                    "enumMember",
                    "event",
                    "function",
                    "method",
                    "macro",
                    "keyword",
                    "modifier",
                    "comment",
                    "string",
                    "number",
                    "regexp",
                    "operator",
                    "decorator",
                ],
                "tokenModifiers": [
                    "declaration",
                    "definition",
                    "readonly",
                    "static",
                    "deprecated",
                    "abstract",
                    "async",
                    "modification",
                    "documentation",
                    "defaultLibrary",
                ],
                "formats": ["relative"],
            },
            "inlayHint": {
                "resolveSupport": {
                    "properties": [
                        "tooltip",
                        "textEdits",
                        "label.tooltip",
                        "label.location",
                        "label.command",
                    ]
                }
            },
            "callHierarchy": {},
            "typeHierarchy": {},
        },
    }
    commands = {
        "fork": [str(args.fork), "server"],
        "companion": [
            str(args.companion),
            "server",
            str(root),
            "--plugin",
            str(fixture / "plugin.py"),
            "--manifest",
            str(fixture / "plugin.plugin.json"),
            "--stubs",
            str(stubs),
            "--ty",
            str(args.official),
            "--out",
            str(case / "analysis"),
        ],
    }
    clients = {}
    evidence = {"encoding": encoding, "checks": [], "raw": {}, "differences": []}
    initialized = {}
    uri = source.as_uri()
    document = {"textDocument": {"uri": uri}}
    try:
        for backend, command in commands.items():
            client = DifferentialClient(command, root, case / f"{backend}.stderr")
            clients[backend] = client
            options = (
                {"configurationFile": str(fork_config)} if backend == "fork" else {}
            )
            initialized[backend] = client.request(
                "initialize",
                {
                    "rootUri": root.as_uri(),
                    "capabilities": caps,
                    "initializationOptions": options,
                },
            )
            client.notify("initialized")
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "python",
                        "version": 1,
                        "text": SOURCE,
                    }
                },
            )
        evidence["capabilities"] = initialized

        def check(name, method, parameters, transform=canonical):
            values = {}
            raw = {}
            for backend, client in clients.items():
                try:
                    raw[backend] = client.request(
                        method,
                        parameters[backend]
                        if set(parameters) == set(clients)
                        else parameters,
                    )
                    values[backend] = transform(raw[backend])
                except (RuntimeError, AssertionError, KeyError) as error:
                    raw[backend] = {"error": str(error)}
                    values[backend] = raw[backend]
            equal = values["fork"] == values["companion"]
            evidence["checks"].append({"name": name, "method": method, "equal": equal})
            evidence["raw"][name] = raw
            if not equal:
                evidence["differences"].append(
                    {"name": name, "method": method, "values": values}
                )
            return raw

        diagnostics = check(
            "diagnostics",
            "textDocument/diagnostic",
            document,
            lambda result: [
                {
                    key: item[key]
                    for key in ("range", "message", "severity", "code", "tags")
                    if key in item
                }
                for item in result.get("items", [])
            ],
        )
        assert any(
            item.get("severity") == 4 and item.get("tags") == [1] and "code" not in item
            for item in diagnostics["companion"].get("items", [])
        ), "native unused-parameter editor hints must survive CLI normalization"
        for name, needle, offset in [
            ("class", "class Resource", 8),
            ("constructor", "item = Resource()", 10),
            ("method", "item.activate()", 8),
            ("injected", "item.label", 7),
            ("state", "item.status", 7),
            ("dynamic", "item.token", 7),
            ("state_variable", "state =", 2),
            ("return_variable", "value = decode", 2),
            ("signature_hook", 'decode("value")', 3),
            ("unicode", "last = local", 9),
        ]:
            check(
                f"hover_{name}",
                "textDocument/hover",
                {
                    **document,
                    "position": position(SOURCE, needle, offset, encoding=encoding),
                },
                hover,
            )
        for name, needle, offset in [
            ("local", "other = local", 10),
            ("cross_file", "answer = helper", 11),
            ("injected", "item.label", 7),
            ("method", "item.activate()", 8),
        ]:
            params = {
                **document,
                "position": position(SOURCE, needle, offset, encoding=encoding),
            }
            check(f"definition_{name}", "textDocument/definition", params, locations)
            check(
                f"references_{name}",
                "textDocument/references",
                {**params, "context": {"includeDeclaration": True}},
                locations,
            )
        params = {
            **document,
            "position": position(SOURCE, "other = local", 10, encoding=encoding),
        }
        check("rename", "textDocument/rename", {**params, "newName": "renamed"}, edit)
        check("document_symbols", "textDocument/documentSymbol", document)
        check(
            "signature_help",
            "textDocument/signatureHelp",
            {
                **document,
                "position": position(SOURCE, 'decode("value")', 10, encoding=encoding),
            },
        )
        semantic = check(
            "semantic_tokens_raw",
            "textDocument/semanticTokens/full",
            document,
            lambda _: None,
        )
        semantic_values = {
            backend: tokens(
                value,
                initialized[backend]["capabilities"]["semanticTokensProvider"][
                    "legend"
                ],
            )
            for backend, value in semantic.items()
        }
        equal = semantic_values["fork"] == semantic_values["companion"]
        evidence["checks"].append({"name": "semantic_tokens", "equal": equal})
        if not equal:
            evidence["differences"].append(
                {"name": "semantic_tokens", "values": semantic_values}
            )
        whole_range = {
            "start": {"line": 0, "character": 0},
            "end": {"line": len(SOURCE.splitlines()), "character": 0},
        }
        check(
            "inlay_hints", "textDocument/inlayHint", {**document, "range": whole_range}
        )
        contexts = {
            backend: {
                **document,
                "range": whole_range,
                "context": {"diagnostics": value.get("items", [])},
            }
            for backend, value in diagnostics.items()
        }
        check("code_actions", "textDocument/codeAction", contexts)
        for name, method, needle, offset in [
            (
                "call_hierarchy",
                "textDocument/prepareCallHierarchy",
                "answer = helper",
                11,
            ),
            (
                "type_hierarchy",
                "textDocument/prepareTypeHierarchy",
                "class Resource",
                8,
            ),
        ]:
            items = check(
                name,
                method,
                {
                    **document,
                    "position": position(SOURCE, needle, offset, encoding=encoding),
                },
            )
            if all(isinstance(value, list) and value for value in items.values()):
                continuation = (
                    "callHierarchy/incomingCalls"
                    if name == "call_hierarchy"
                    else "typeHierarchy/subtypes"
                )
                check(
                    name + "_resolved",
                    continuation,
                    {backend: {"item": value[0]} for backend, value in items.items()},
                )
        # Rebuilding source maps for a different workspace must neither append
        # explanatory notes again nor duplicate uncoded unused-parameter hints.
        other_root = case / "other-workspace"
        other_root.mkdir()
        other_source = other_root / "other.py"
        other_source.write_text("value = 1\n")
        for client in clients.values():
            client.notify(
                "workspace/didChangeWorkspaceFolders",
                {
                    "event": {
                        "added": [{"uri": other_root.as_uri(), "name": "other"}],
                        "removed": [],
                    }
                },
            )
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": other_source.as_uri(),
                        "languageId": "python",
                        "version": 1,
                        "text": "value = 1\n",
                    }
                },
            )
            client.request(
                "textDocument/diagnostic",
                {"textDocument": {"uri": other_source.as_uri()}},
            )
        for revision in range(2, 4):
            for client in clients.values():
                client.notify(
                    "textDocument/didChange",
                    {
                        "textDocument": {
                            "uri": other_source.as_uri(),
                            "version": revision,
                        },
                        "contentChanges": [{"text": f"value = {revision}\n"}],
                    },
                )
                client.request(
                    "textDocument/diagnostic",
                    {"textDocument": {"uri": other_source.as_uri()}},
                )
            refreshed = check(
                f"unchanged_workspace_notes_{revision}",
                "textDocument/diagnostic",
                document,
            )
            messages = [item["message"] for item in refreshed["companion"]["items"]]
            assert (
                sum("info: `str` and `Literal[1]`" in message for message in messages)
                == 1
            )
            assert all(
                message.count("info: `str` and `Literal[1]`") <= 1
                for message in messages
            )
        after = {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in root.glob("*.py")
        }
        assert before == after, "editor modified original project files"
        evidence["unchanged_source_hashes"] = after
        evidence["passed"] = not evidence["differences"]
    finally:
        for client in clients.values():
            client.close()
        (case / "evidence.json").write_text(json.dumps(evidence, indent=2) + "\n")
    return evidence


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--companion", type=Path, required=True)
    parser.add_argument("--official", type=Path, required=True)
    parser.add_argument("--fork", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--encodings", nargs="+", default=["utf-16", "utf-8", "utf-32"])
    args = parser.parse_args()
    reports = [run(args, encoding) for encoding in args.encodings]
    summary = {
        "passed": all(report["passed"] for report in reports),
        "reports": reports,
    }
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(
        json.dumps(
            {
                "passed": summary["passed"],
                "differences": {
                    r["encoding"]: [item["name"] for item in r["differences"]]
                    for r in reports
                },
            }
        )
    )
    raise SystemExit(0 if summary["passed"] else 1)


if __name__ == "__main__":
    main()
