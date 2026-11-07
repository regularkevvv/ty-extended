#!/usr/bin/env python3
"""Replay original Mini-Django SDK sources using one equivalent Monty adapter."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import textwrap
from pathlib import Path

from lsp_acceptance import Client, position
from plugin_configuration_differential import diagnostics, lsp_diagnostics


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(
        json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )


def manifest():
    return {
        "id": "example.minidjango",
        "name": "Mini-Django original SDK adapter",
        "version": "0.1.0",
        "protocol-version": {"major": 0, "minor": 7},
        "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
        "runtime": {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": {
            "class-transform": True,
            "project-index": True,
            "cross-symbol-contributions": True,
            "virtual-types": True,
            "settings-data": True,
            "call-return": True,
        },
        "claims": {
            "classes": [
                {"kind": "subclass-of", "base-qualified-name": "minidjango.Model"}
            ],
            "attributes": [
                {
                    "kind": "contribution-target",
                    "owner-base-qualified-name": "minidjango.Model",
                    "scope": "instance",
                }
            ],
            "methods": [
                {
                    "kind": "on-subclass-of",
                    "base-qualified-name": owner,
                    "method-name": method,
                }
                for owner in ["minidjango.Manager", "minidjango.QuerySet"]
                for method in [
                    "filter",
                    "get",
                    "get_or_create",
                    "first",
                    "count",
                    "exists",
                    "values",
                    "values_list",
                    "annotate",
                ]
            ],
            "settings": [{"module": "minidjango_settings"}],
        },
    }


def editor_contract(args, report, number, sources, replacement=None):
    case = args.output / str(number)
    root = case / "project"
    root.mkdir(parents=True)
    for name, text in sources.items():
        (root / name).write_text(text, encoding="utf-8")
    (root / "ty.toml").write_text(
        '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="example.minidjango"\nruntime="monty"\ntrusted=true\npath="../../plugin.py"\nmanifest-path="../../plugin.json"\n',
        encoding="utf-8",
    )
    source = sources["models.py"]
    uri = (root / "models.py").as_uri()
    row = {
        "inventory_id": number,
        "sources": {name: digest(root / name) for name in sources},
        "runs": [],
    }
    report["cases"].append(row)
    baseline = {}
    for backend in ("fork", "companion"):
        command = (
            [str(args.fork), "server"]
            if backend == "fork"
            else [str(args.companion), "server", "--ty", str(args.official)]
        )
        save(case / (backend + ".command.json"), command)
        stderr = case / (backend + ".stderr")
        client = Client(command, root, stderr)
        snapshots = {}
        try:
            initialized = client.request(
                "initialize",
                {
                    "initializationOptions": {
                        "logLevel": "debug",
                        "diagnosticMode": "workspace",
                    },
                    "processId": None,
                    "rootUri": root.as_uri(),
                    "workspaceFolders": [{"uri": root.as_uri(), "name": "fixture"}],
                    "capabilities": {
                        "general": {"positionEncodings": ["utf-16"]},
                        "textDocument": {
                            "diagnostic": {},
                            "hover": {"contentFormat": ["plaintext"]},
                        },
                    },
                },
            )
            assert (
                initialized["capabilities"].get("positionEncoding", "utf-16")
                == "utf-16"
            )
            client.notify("initialized")
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "python",
                        "version": 1,
                        "text": source,
                    }
                },
            )
            snapshots["initial"] = lsp_diagnostics(
                client.request(
                    "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                )["items"]
            )
            if number == 152:
                messages = [item["message"] for item in snapshots["initial"]]
                assert any(
                    "Conflicting Mini-Django reverse relation `library.Author.books`"
                    in message
                    for message in messages
                ), messages
                assert any(
                    "Invalid Mini-Django lookup value for `pages__isnull` on `models.Book.pages`; expected `int | None`"
                    in message
                    for message in messages
                ), messages
            if backend == "companion":
                before = client.request("tyCompanion/statistics")
            else:
                before = stderr.read_text(encoding="utf-8").count("MINIDJANGO_INDEX")
                assert before > 0, (number, "native project index did not execute")
            for _ in range(3):
                client.request(
                    "textDocument/hover",
                    {
                        "textDocument": {"uri": uri},
                        "position": position(source, "Book("),
                    },
                )
                assert (
                    lsp_diagnostics(
                        client.request(
                            "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                        )["items"]
                    )
                    == snapshots["initial"]
                )
            if backend == "companion":
                after = client.request("tyCompanion/statistics")
                assert before == after, (
                    number,
                    "unchanged semantic query rebuilt projection",
                    before,
                    after,
                )
            else:
                after = stderr.read_text(encoding="utf-8").count("MINIDJANGO_INDEX")
                assert before == after, (
                    number,
                    "native unchanged query rebuilt project index",
                    before,
                    after,
                )
            if replacement is not None:
                assert not any(
                    "Unknown Mini-Django lookup `title__contains`" in item["message"]
                    for item in snapshots["initial"]
                )
                client.notify(
                    "textDocument/didChange",
                    {
                        "textDocument": {"uri": uri, "version": 2},
                        "contentChanges": [{"text": replacement}],
                    },
                )
                snapshots["edited"] = lsp_diagnostics(
                    client.request(
                        "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                    )["items"]
                )
                assert any(
                    "Unknown Mini-Django lookup `title__contains`" in item["message"]
                    for item in snapshots["edited"]
                ), (backend, snapshots)
                if backend == "fork":
                    assert (
                        stderr.read_text(encoding="utf-8").count("MINIDJANGO_INDEX")
                        > after
                    )
                else:
                    assert client.request("tyCompanion/statistics") != after
                client.notify(
                    "textDocument/didChange",
                    {
                        "textDocument": {"uri": uri, "version": 3},
                        "contentChanges": [{"text": source}],
                    },
                )
                snapshots["restored"] = lsp_diagnostics(
                    client.request(
                        "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                    )["items"]
                )
                assert snapshots["restored"] == snapshots["initial"]
            if backend == "fork":
                baseline = snapshots
            equal = snapshots == baseline
            save(case / (backend + ".diagnostics.json"), snapshots)
            row["runs"].append(
                {
                    "backend": backend,
                    "transport": "lsp",
                    "equal": equal,
                    "unchanged_queries_reuse_index": True,
                    "original_private_query_warmup_bound": [1, 2],
                    "public_lsp_warmup_count": before if backend == "fork" else None,
                    "warmup_basis": "Public workspace initialization plus document open; private DB warmup bound is not asserted for this different entry point",
                    "companion_cache_evidence": "unchanged projection and Monty execution counters"
                    if backend == "companion"
                    else None,
                    "before": before,
                    "after": after,
                }
            )
            assert equal, (number, backend, "editor diagnostic mismatch", case)
        finally:
            client.close()
        assert all(digest(root / name) == sha for name, sha in row["sources"].items())


def type_contract(args, row, root, number):
    """Exercise original direct field/member type assertions through public LSP."""
    fields = {
        "id": "int",
        "pk": "int",
        "title": "str",
        "pages": "int | None",
        "author": "Author",
        "author_id": "int",
        "parent": "Book | None",
        "parent_id": "int | None",
        "owner": "User | None",
        "owner_id": "int | None",
    }
    path = root / "models.py"
    source = (
        path.read_text(encoding="utf-8")
        + "\ndef __sdk_original_contract(book: Book):\n"
    )
    expected = {}
    for name, value in fields.items():
        source += f"    probe_{name} = book.{name}\n"
        expected["probe_" + name] = value
    for name in ("objects", "_default_manager", "published"):
        source += f"    probe_{name} = Book.{name}\n"
        expected["probe_" + name] = "Manager"
    if number == 151:
        source += "probe_method = annotate_method_probe\nprobe_row = annotated_probe\nprobe_score = annotated_score_probe\n"
    # Preserve the original declaring scope of the private DB queries. Only
    # the editor buffer receives appended probes; all fixture files stay exact.
    probe_path = root.parent / "type-probes.py"
    probe_path.write_text(source, encoding="utf-8")
    row["type_probe_source_sha256"] = digest(probe_path)
    baseline = None
    for backend in ("fork", "companion"):
        command = (
            [str(args.fork), "server"]
            if backend == "fork"
            else [str(args.companion), "server", "--ty", str(args.official)]
        )
        client = Client(command, root, root.parent / (backend + ".types.stderr"))
        observed = {}
        try:
            client.request(
                "initialize",
                {
                    "rootUri": root.as_uri(),
                    "capabilities": {
                        "textDocument": {"hover": {"contentFormat": ["plaintext"]}}
                    },
                },
            )
            client.notify("initialized")
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": path.as_uri(),
                        "languageId": "python",
                        "version": 1,
                        "text": source,
                    }
                },
            )
            for name in list(expected) + (
                ["probe_method", "probe_row", "probe_score"] if number == 151 else []
            ):
                hover = client.request(
                    "textDocument/hover",
                    {
                        "textDocument": {"uri": path.as_uri()},
                        "position": position(source, name),
                    },
                )
                content = hover["contents"]
                text = content["value"] if isinstance(content, dict) else content
                observed[name] = text
                first_line = text.split("\n", 1)[0]
                if name in expected:
                    assert first_line == expected[name] or first_line.endswith(
                        ": " + expected[name]
                    ), (
                        number,
                        backend,
                        name,
                        expected[name],
                        hover,
                    )
            if number == 151:
                assert observed["probe_method"].split("\n", 1)[0] != "Unknown", observed
                assert (
                    observed["probe_row"].split("\n", 1)[0].removeprefix("probe_row: ")
                    == "MiniDjangoAnnotatedRow"
                ), observed
                assert observed["probe_score"].split("\n", 1)[0].removeprefix(
                    "probe_score: "
                ) in {"int", "Literal[1]"}, observed
            if baseline is None:
                baseline = observed
            # A generated callable's public signature is exercised by the
            # dedicated signature suite; this original query only asserts it
            # resolves, while all field/row/member type values compare exactly.
            comparable = {
                key: value for key, value in observed.items() if key != "probe_method"
            }
            assert comparable == {
                key: value for key, value in baseline.items() if key != "probe_method"
            }, (number, backend, observed, baseline)
            row["runs"].append(
                {
                    "backend": backend,
                    "transport": "lsp-types",
                    "equal": True,
                    "queries": len(observed),
                }
            )
        finally:
            save(root.parent / (backend + ".types.json"), observed)
            client.close()


def editor_scope_contract(args, report, sources):
    """Default open-file indexing, scope changes, and live diagnostic mode."""
    case = args.output / "lsp-open-file-scope"
    root = case / "project"
    root.mkdir(parents=True)
    for name, source in sources.items():
        (root / name).write_text(source, encoding="utf-8")
    (root / "ty.toml").write_text(
        '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="example.minidjango"\nruntime="monty"\ntrusted=true\npath="../../plugin.py"\nmanifest-path="../../plugin.json"\n',
        encoding="utf-8",
    )
    row = {
        "inventory_id": "lsp-open-file-scope",
        "sources": {name: digest(root / name) for name in sources},
        "runs": [],
        "basis": "Live companion notifications compared with freshly initialized native reference for each mode/open-file set; native 0.86 ignores live didChangeConfiguration.",
    }
    report["cases"].append(row)
    configuration = {"diagnosticMode": "openFilesOnly"}
    opened = {"models.py"}
    uri = (root / "models.py").as_uri()

    def handler(client, request):
        if request["method"] != "workspace/configuration":
            return False
        assert all(
            item == {"scopeUri": root.as_uri(), "section": "ty"}
            for item in request["params"]["items"]
        ), request
        client.send(
            {
                "id": request["id"],
                "result": [dict(configuration) for _ in request["params"]["items"]],
            }
        )
        return True

    def start(backend, phase):
        command = (
            [str(args.fork), "server"]
            if backend == "fork"
            else [str(args.companion), "server", "--ty", str(args.official)]
        )
        save(case / (backend + "-" + phase + ".command.json"), command)
        client = Client(
            command,
            root,
            case / (backend + "-" + phase + ".stderr"),
            server_request_handler=handler,
        )
        client.request(
            "initialize",
            {
                "rootUri": root.as_uri(),
                "workspaceFolders": [{"uri": root.as_uri(), "name": "fixture"}],
                "capabilities": {
                    "workspace": {"configuration": True},
                    "textDocument": {"diagnostic": {}},
                },
            },
        )
        client.notify("initialized")
        for name in sorted(opened):
            open_document(client, name)
        return client

    def open_document(client, name):
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": (root / name).as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": sources[name],
                }
            },
        )

    def pull(client):
        return lsp_diagnostics(
            client.request("textDocument/diagnostic", {"textDocument": {"uri": uri}})[
                "items"
            ]
        )

    client = start("companion", "live")
    initial_pids = None
    try:
        for phase in (
            "models-only",
            "open-library",
            "open-accounts",
            "close-library",
            "workspace-mode",
            "open-files-mode",
        ):
            if phase in {"open-library", "open-accounts"}:
                name = "library.py" if phase == "open-library" else "accounts.py"
                opened.add(name)
                open_document(client, name)
            elif phase == "close-library":
                opened.remove("library.py")
                client.notify(
                    "textDocument/didClose",
                    {"textDocument": {"uri": (root / "library.py").as_uri()}},
                )
            elif phase in {"workspace-mode", "open-files-mode"}:
                configuration["diagnosticMode"] = (
                    "workspace" if phase == "workspace-mode" else "openFilesOnly"
                )
                client.notify("workspace/didChangeConfiguration", {"settings": {}})
            observed = pull(client)
            save(case / ("companion-" + phase + ".diagnostics.json"), observed)
            reference = start("fork", phase)
            try:
                expected = pull(reference)
                save(case / ("fork-" + phase + ".diagnostics.json"), expected)
            finally:
                reference.close()
            assert observed == expected, (
                phase,
                "project-index scope differs",
                str(case),
            )
            messages = [item["message"] for item in expected]
            for owner, file in (
                ("library.Author", "library.py"),
                ("accounts.User", "accounts.py"),
            ):
                missing = any(
                    "Unknown Mini-Django relation target `" + owner + "`" in message
                    for message in messages
                )
                assert missing == (
                    file not in opened
                    and configuration["diagnosticMode"] != "workspace"
                ), (phase, owner, messages)
            before = client.request("tyCompanion/statistics")
            if initial_pids is None:
                initial_pids = before["checker_pids"]
            assert before["checker_pids"] == initial_pids, (
                phase,
                "scope update restarted checker",
            )
            assert pull(client) == observed
            assert client.request("tyCompanion/statistics") == before, (
                phase,
                "unchanged query rebuilt projection",
            )
            row["runs"].append(
                {
                    "phase": phase,
                    "equal": True,
                    "open_files": sorted(opened),
                    "diagnostic_mode": configuration["diagnosticMode"],
                    "diagnostics": len(observed),
                    "unchanged_queries_reuse_projection": True,
                    "checker_pids": before["checker_pids"],
                }
            )
    finally:
        save(case / "companion.server-requests.json", client.server_requests)
        client.close()
    assert all(digest(root / name) == sha for name, sha in row["sources"].items())


def main():
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument(
        "--inventory-id", type=int, action="append", choices=[150, 151, 152, 153]
    )
    args = parser.parse_args()
    for name in ("companion", "official", "fork", "output"):
        setattr(args, name, getattr(args, name).resolve())
    args.output.mkdir(parents=True)
    repo = Path(__file__).resolve().parents[2]
    original = repo / "ruff/crates/ty_python_semantic/src/types/class/static_literal.rs"
    rust_plugin = repo / "ruff/crates/ty_plugin_examples/src/minidjango.rs"
    adapter = repo / "companion/tests/fixtures/sdk_minidjango/plugin.py"
    raw = original.read_text(encoding="utf-8")
    original150 = raw.split(
        "fn minidjango_plugin_transforms_real_class_summary_through_semantic_hooks()", 1
    )[1].split("fn minidjango_fixture_harness_runs_through_semantic_hooks()", 1)[0]
    source150 = {
        name: textwrap.dedent(text).strip() + "\n"
        for name, text in re.findall(r'"/src/([^"]+)",\s*r#"(.*?)"#', original150, re.S)
    }
    resources = (
        repo / "ruff/crates/ty_python_semantic/resources/plugin_fixtures/minidjango"
    )
    source151 = {p.name: p.read_text(encoding="utf-8") for p in resources.glob("*.py")}
    assert (
        set(source150)
        == set(source151)
        == {
            "minidjango.py",
            "models.py",
            "accounts.py",
            "library.py",
            "minidjango_settings.py",
        }
    )
    value = manifest()
    save(args.output / "plugin.json", value)
    (args.output / "plugin.py").write_text(
        "set_manifest(" + repr(value) + ")\n" + adapter.read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    report = {
        "passed": False,
        "artifact": str(args.companion),
        "sha256": digest(args.companion),
        "fork_sha256": digest(args.fork),
        "official_sha256": digest(args.official),
        "source_sha256": {
            str(p.relative_to(repo)): digest(p)
            for p in [Path(__file__), adapter, original, rust_plugin]
        },
        "cases": [],
    }
    try:
        for number, sources in [(150, source150), (151, source151)]:
            if args.inventory_id and number not in args.inventory_id:
                continue
            case = args.output / str(number)
            root = case / "project"
            root.mkdir(parents=True)
            for name, text in sources.items():
                (root / name).write_text(text, encoding="utf-8")
            (root / "ty.toml").write_text(
                '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="example.minidjango"\nruntime="monty"\ntrusted=true\npath="../../plugin.py"\nmanifest-path="../../plugin.json"\n',
                encoding="utf-8",
            )
            save(case / "selected.json", [str(root / "models.py")])
            row = {
                "inventory_id": number,
                "sources": {name: digest(root / name) for name in sources},
                "runs": [],
            }
            report["cases"].append(row)
            expected = None
            for backend in ["fork", "companion", "project-cli", "project-push"]:
                if backend.startswith("project-"):
                    command = [
                        str(args.companion),
                        "project",
                        str(root),
                        "--ty",
                        str(args.official),
                        "--out",
                        str(case / backend),
                        "--checker-mode",
                        backend.removeprefix("project-"),
                        "--python-version",
                        "3.13",
                    ]
                else:
                    command = [
                        str(args.fork if backend == "fork" else args.companion),
                        "check",
                        "--project",
                        ".",
                        "--output-format",
                        "gitlab",
                    ]
                    if backend == "companion":
                        command += ["--ty", str(args.official)]
                    else:
                        command += ["-vv"]
                save(case / (backend + ".command.json"), command)
                run = subprocess.run(
                    command,
                    cwd=root,
                    env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
                    text=True,
                    encoding="utf-8",
                    capture_output=True,
                    timeout=120,
                )
                (case / (backend + ".stdout")).write_text(run.stdout, encoding="utf-8")
                (case / (backend + ".stderr")).write_text(run.stderr, encoding="utf-8")
                assert run.returncode in [0, 1], (number, backend, run.stderr)
                result = json.loads(run.stdout)
                observed = diagnostics(
                    result["diagnostics"] if isinstance(result, dict) else result, root
                )
                save(case / (backend + ".diagnostics.json"), observed)
                descriptions = [item["description"] for item in observed]
                if backend == "fork":
                    assert sum(
                        "not assignable" in message for message in descriptions
                    ) >= (14 if number == 150 else 24), descriptions
                    assert sum(
                        "Unknown Mini-Django lookup" in message
                        for message in descriptions
                    ) >= (3 if number == 150 else 5), descriptions
                    assert any(
                        "Conflicting Mini-Django reverse relation `library.Author.books`"
                        in message
                        for message in descriptions
                    ), descriptions
                    assert any(
                        "Unknown Mini-Django relation target `library.Missing`"
                        in message
                        for message in descriptions
                    ), descriptions
                    assert not any(
                        "No parameter named" in message for message in descriptions
                    ), descriptions
                    if number == 151:
                        assert (
                            sum(
                                "Invalid Mini-Django lookup value" in message
                                for message in descriptions
                            )
                            >= 4
                        ), descriptions
                    expected = observed
                equal = observed == expected
                row["runs"].append(
                    {
                        "backend": backend,
                        "equal": equal,
                        "exit_code": run.returncode,
                        "diagnostics": len(observed),
                    }
                )
                assert equal, (number, backend, "diagnostic mismatch", str(case))
            type_contract(args, row, root, number)
            assert all(
                digest(root / name) == sha for name, sha in row["sources"].items()
            )
        if not args.inventory_id or 152 in args.inventory_id:
            editor_contract(args, report, 152, source151)
            editor_scope_contract(args, report, source151)
        if not args.inventory_id or 153 in args.inventory_id:
            block = raw.split(
                "fn minidjango_project_index_invalidates_when_model_source_changes()", 1
            )[1].split(
                "fn plugin_class_summary_includes_django_like_assignment_calls()", 1
            )[0]
            states = [
                textwrap.dedent(text).strip() + "\n"
                for text in re.findall(r'r#"(.*?)"#', block, re.S)
            ]
            assert len(states) == 2
            editor_contract(
                args, report, 153, {**source151, "models.py": states[0]}, states[1]
            )
        assert digest(args.companion) == report["sha256"]
        report["passed"] = True
    finally:
        save(args.output / "summary.json", report)
    print(
        json.dumps(
            {
                "passed": True,
                "cases": len(report["cases"]),
                "summary": str(args.output / "summary.json"),
            }
        )
    )


if __name__ == "__main__":
    main()
