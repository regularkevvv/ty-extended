#!/usr/bin/env python3
"""Compare real native notebook editor behavior with the reference fork."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from lsp_acceptance import position
from lsp_differential import (
    DifferentialClient,
    canonical,
    edit,
    hover,
    locations,
    tokens,
)

DEFINITIONS = '''class Resource:
    """A resource shared between notebook cells."""
    status: str | None = None
    def activate(self) -> None: pass
    def release(self) -> None: pass

def decode(value: object) -> object:
    """Decode a value."""
    return value
'''
FIRST = """from demo import Resource, decode
item = Resource()
item.activate()
local = 1
emoji = "😀"
"""
SECOND = """item.label
item.status
item.token
value = decode("value")
wrong: str = value
other = local
"""


def run(args, encoding):
    case = args.output / encoding
    root = case / "project"
    root.mkdir(parents=True)
    stubs = case / "stubs"
    stubs.mkdir()
    (root / "demo.py").write_text(DEFINITIONS, encoding="utf-8")
    uri = (root / "analysis.ipynb").as_uri()
    cell_uris = [f"vscode-notebook-cell:{uri}#cell-{index}" for index in range(4)]
    texts = ["Notebook documentation", FIRST, "%%bash\necho ignored", SECOND]
    notebook_path = root / "analysis.ipynb"
    notebook_text = json.dumps(
        {
            "nbformat": 4,
            "nbformat_minor": 5,
            "metadata": {},
            "cells": [
                {
                    "cell_type": "markdown" if index == 0 else "code",
                    "id": f"cell-{index}",
                    "metadata": {},
                    "source": [text],
                    **({} if index == 0 else {"outputs": [], "execution_count": None}),
                }
                for index, text in enumerate(texts)
            ],
        },
        indent=2,
    )
    notebook_path.write_text(notebook_text, encoding="utf-8")
    cells = [
        {"kind": 1 if index == 0 else 2, "document": cell_uri}
        for index, cell_uri in enumerate(cell_uris)
    ]
    opened = {
        "notebookDocument": {
            "uri": uri,
            "notebookType": "jupyter-notebook",
            "version": 1,
            "cells": cells,
        },
        "cellTextDocuments": [
            {
                "uri": cell_uri,
                "languageId": "markdown" if index == 0 else "python",
                "version": 1,
                "text": texts[index],
            }
            for index, cell_uri in enumerate(cell_uris)
        ],
    }
    fixture = Path(__file__).resolve().parents[1] / "tests/fixtures/generic"
    config = case / "fork.toml"
    config.write_text(
        '[environment]\npython-version = "3.13"\n[plugins]\nenabled = true\nauto-discover = false\n[[plugins.plugin]]\n'
        f'id = "research.generic"\nruntime = "monty"\ntrusted = true\npath = {json.dumps(str(fixture / "plugin.py"))}\nmanifest-path = {json.dumps(str(fixture / "plugin.plugin.json"))}\n',
        encoding="utf-8",
    )
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
    evidence = {"encoding": encoding, "checks": [], "differences": [], "raw": {}}
    initialized = {}
    caps = {
        "general": {"positionEncodings": [encoding]},
        "notebookDocument": {"synchronization": {"dynamicRegistration": False}},
        "textDocument": {
            "diagnostic": {},
            "hover": {"contentFormat": ["markdown"]},
            "definition": {"linkSupport": True},
            "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
            "signatureHelp": {
                "signatureInformation": {
                    "documentationFormat": ["markdown"],
                    "parameterInformation": {"labelOffsetSupport": True},
                }
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
        },
    }
    try:
        for backend, command in commands.items():
            client = DifferentialClient(command, root, case / f"{backend}.stderr")
            clients[backend] = client
            options = {"configurationFile": str(config)} if backend == "fork" else {}
            initialized[backend] = client.request(
                "initialize",
                {
                    "rootUri": root.as_uri(),
                    "capabilities": caps,
                    "initializationOptions": options,
                },
            )
            assert initialized[backend]["capabilities"].get("notebookDocumentSync"), (
                backend
            )
            client.notify("initialized")
            if backend == "companion":
                closed = client.request(
                    "workspace/diagnostic", {"previousResultIds": []}
                )
                assert all(
                    item["uri"] != uri and item["uri"] not in cell_uris
                    for item in closed["items"]
                ), closed
                evidence["closed_notebook_has_no_cell_diagnostics"] = True
            # ty 0.0.86 uses a raw cell index as a Python-only source index.
            # Its unfiltered notebook gives hovers for the wrong cell when
            # markdown/magic cells precede it. Compare identical Python source
            # against a filtered reference, while the companion receives the
            # actual mixed notebook and must retain the original cell URIs.
            reference = {
                **opened,
                "notebookDocument": {
                    **opened["notebookDocument"],
                    "cells": [cells[1], cells[3]],
                },
                "cellTextDocuments": [
                    opened["cellTextDocuments"][1],
                    opened["cellTextDocuments"][3],
                ],
            }
            client.notify(
                "notebookDocument/didOpen", reference if backend == "fork" else opened
            )

        def check(name, method, params, transform=canonical):
            values = {}
            raw = {}
            for backend, client in clients.items():
                try:
                    raw[backend] = client.request(method, params)
                    values[backend] = transform(raw[backend])
                except (RuntimeError, AssertionError, KeyError) as error:
                    raw[backend] = {"error": str(error)}
                    values[backend] = raw[backend]
            equal = values["fork"] == values["companion"]
            evidence["checks"].append({"name": name, "equal": equal})
            evidence["raw"][name] = raw
            if not equal:
                evidence["differences"].append({"name": name, "values": values})
            return raw

        document = {"textDocument": {"uri": cell_uris[3]}}

        def diagnostic(result):
            return [
                {
                    key: item[key]
                    for key in ("range", "message", "severity", "code")
                    if key in item
                }
                for item in result.get("items", [])
            ]

        check("diagnostics", "textDocument/diagnostic", document, diagnostic)
        for name, needle, offset in [
            ("injected", "item.label", 7),
            ("state", "item.status", 7),
            ("dynamic", "item.token", 7),
            ("return", "value =", 2),
            ("callable", "decode(", 3),
        ]:
            check(
                f"hover_{name}",
                "textDocument/hover",
                {
                    **document,
                    "position": position(SECOND, needle, offset, encoding=encoding),
                },
                hover,
            )
        local = {
            **document,
            "position": position(SECOND, "other = local", 10, encoding=encoding),
        }
        check("cross_cell_definition", "textDocument/definition", local, locations)
        check(
            "cross_cell_references",
            "textDocument/references",
            {**local, "context": {"includeDeclaration": True}},
            locations,
        )
        check(
            "cross_cell_rename",
            "textDocument/rename",
            {**local, "newName": "renamed"},
            edit,
        )
        # Like symbols, fork highlights include other cells without their URI.
        local_range = {
            "start": position(SECOND, "other = local", 8, encoding=encoding),
            "end": position(SECOND, "other = local", 13, encoding=encoding),
        }
        highlights = check(
            "highlights",
            "textDocument/documentHighlight",
            local,
            lambda result: [item for item in result if item["range"] == local_range],
        )
        assert len(highlights["companion"]) == 1
        check("prepare_rename", "textDocument/prepareRename", local)
        check(
            "selection_range",
            "textDocument/selectionRange",
            {**document, "positions": [local["position"]]},
        )
        check("folding_range", "textDocument/foldingRange", document)
        whole_range = {
            "start": {"line": 0, "character": 0},
            "end": {"line": len(SECOND.splitlines()), "character": 0},
        }
        check(
            "inlay_hints", "textDocument/inlayHint", {**document, "range": whole_range}
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
        # The reference also returns symbols from every cell without a URI.
        # Retain only this cell's known names for the differential, and assert
        # the companion itself returns exactly these unambiguous symbols.
        symbol_names = {"value", "wrong", "other"}
        symbols = check(
            "symbols",
            "textDocument/documentSymbol",
            document,
            lambda result: [
                canonical(item) for item in result if item["name"] in symbol_names
            ],
        )
        assert {item["name"] for item in symbols["companion"]} == symbol_names
        check(
            "signature",
            "textDocument/signatureHelp",
            {
                **document,
                "position": position(SECOND, 'decode("value")', 10, encoding=encoding),
            },
        )
        first_changed = FIRST.replace("activate", "release")
        change = {
            "notebookDocument": {"uri": uri, "version": 2},
            "change": {
                "cells": {
                    "textContent": [
                        {
                            "document": {"uri": cell_uris[1], "version": 2},
                            "changes": [
                                {
                                    "range": {
                                        "start": position(
                                            FIRST, "activate", encoding=encoding
                                        ),
                                        "end": position(
                                            FIRST, "activate", 8, encoding=encoding
                                        ),
                                    },
                                    "text": "release",
                                }
                            ],
                        }
                    ]
                }
            },
        }
        for client in clients.values():
            client.notify("notebookDocument/didChange", change)
        check(
            "state_after_incremental_change",
            "textDocument/hover",
            {
                **document,
                "position": position(SECOND, "item.status", 7, encoding=encoding),
            },
            hover,
        )
        check(
            "diagnostics_after_change", "textDocument/diagnostic", document, diagnostic
        )
        # Reordering must update shared notebook scope and generated-cell maps.
        reordered = [cells[0], cells[3], cells[2], cells[1]]
        for backend, client in clients.items():
            client.notify(
                "notebookDocument/didChange",
                {
                    "notebookDocument": {"uri": uri, "version": 3},
                    "change": {
                        "cells": {
                            "structure": {
                                "array": {
                                    "start": 0,
                                    "deleteCount": 2 if backend == "fork" else 4,
                                    "cells": [cells[3], cells[1]]
                                    if backend == "fork"
                                    else reordered,
                                }
                            }
                        }
                    },
                },
            )
        check(
            "diagnostics_after_reorder", "textDocument/diagnostic", document, diagnostic
        )
        check("definition_after_reorder", "textDocument/definition", local, locations)
        evidence["final_first_cell"] = first_changed
        for client in clients.values():
            client.notify(
                "notebookDocument/didClose",
                {
                    "notebookDocument": {"uri": uri},
                    "cellTextDocuments": [{"uri": cell_uri} for cell_uri in cell_uris],
                },
            )
        closed = clients["companion"].request(
            "workspace/diagnostic", {"previousResultIds": []}
        )
        assert all(
            item["uri"] != uri and item["uri"] not in cell_uris
            for item in closed["items"]
        ), closed
        assert notebook_path.read_text(encoding="utf-8") == notebook_text, (
            "editor changes rewrote notebook JSON"
        )
        evidence["closed_notebook_diagnostics_cleared"] = True
        evidence["passed"] = not evidence["differences"]
    finally:
        for client in clients.values():
            client.close()
        (case / "evidence.json").write_text(
            json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
        )
    return evidence


def main():
    parser = argparse.ArgumentParser()
    for name in ["companion", "official", "fork", "output"]:
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--encodings", nargs="+", default=["utf-16", "utf-8", "utf-32"])
    args = parser.parse_args()
    reports = [run(args, encoding) for encoding in args.encodings]
    summary = {
        "passed": all(report["passed"] for report in reports),
        "reports": reports,
    }
    (args.output / "summary.json").write_text(
        json.dumps(summary, indent=2) + "\n", encoding="utf-8"
    )
    print(
        json.dumps(
            {
                "passed": summary["passed"],
                "differences": {
                    report["encoding"]: [item["name"] for item in report["differences"]]
                    for report in reports
                },
            }
        )
    )
    raise SystemExit(0 if summary["passed"] else 1)


if __name__ == "__main__":
    main()
