#!/usr/bin/env python3
"""Real editor lifecycle: configuration discovery, workspaces and unsaved files."""

from __future__ import annotations

import argparse
import json
from pathlib import Path

from lsp_acceptance import SOURCE, Client, position


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--companion", type=Path, required=True)
    parser.add_argument("--official", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    first = args.output / "first"
    second = args.output / "second"
    first.mkdir()
    second.mkdir()
    fixture = Path(__file__).resolve().parents[1] / "tests/fixtures/generic"
    (first / "demo.py").write_text(SOURCE, encoding="utf-8")
    config = first / "ty.toml"
    config.write_text(
        '[environment]\npython-version="3.13"\n'
        "[plugins]\nenabled=true\nauto-discover=false\n"
        '[[plugins.plugin]]\nid="research.generic"\nruntime="monty"\ntrusted=true\n'
        f"path={json.dumps(str(fixture / 'plugin.py'))}\n"
        f"manifest-path={json.dumps(str(fixture / 'plugin.plugin.json'))}\n",
        encoding="utf-8",
    )
    other_text = 'number: int = "wrong"\n'
    dependencies = args.output / "dependencies"
    dependencies.mkdir()
    (second / "other.py").write_text(other_text, encoding="utf-8")
    (second / "pyproject.toml").write_text(
        '[tool.ty.environment]\npython-version="3.13"\n'
        f"extra-paths=[{json.dumps(str(dependencies))}]\n",
        encoding="utf-8",
    )
    client = Client(
        [
            str(args.companion),
            "server",
            "--ty",
            str(args.official),
            "--out",
            str(args.output / "analysis"),
        ],
        first,
        args.output / "stderr.log",
    )
    evidence = {}
    try:
        client.request(
            "initialize",
            {
                "workspaceFolders": [
                    {"uri": first.as_uri(), "name": "first"},
                    {"uri": second.as_uri(), "name": "second"},
                ],
                "capabilities": {
                    "textDocument": {"diagnostic": {}},
                    "workspace": {
                        "didChangeWatchedFiles": {"dynamicRegistration": True}
                    },
                },
            },
        )
        client.notify("initialized")
        for path, text in (
            (first / "demo.py", SOURCE),
            (second / "other.py", other_text),
        ):
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": path.as_uri(),
                        "version": 1,
                        "languageId": "python",
                        "text": text,
                    }
                },
            )
        one = {"textDocument": {"uri": (first / "demo.py").as_uri()}}
        two = {"textDocument": {"uri": (second / "other.py").as_uri()}}
        initial = client.request("textDocument/diagnostic", one)
        assert (
            len(initial["items"]) == 1
            and initial["items"][0]["code"] == "invalid-assignment"
        ), initial
        other = client.request("textDocument/diagnostic", two)
        assert len(other["items"]) == 1, other
        hover = client.request(
            "textDocument/hover",
            {**one, "position": position(SOURCE, "item.status", 6)},
        )
        assert "str" in json.dumps(hover) and "None" not in json.dumps(hover), hover
        evidence["configured_plugin"] = initial
        initial_pids = client.request("tyCompanion/statistics")["checker_pids"]
        assert len(initial_pids) == 2

        # A file which exists only in the editor must participate in analysis.
        new = {"textDocument": {"uri": (second / "unsaved.py").as_uri()}}
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    **new["textDocument"],
                    "version": 1,
                    "languageId": "python",
                    "text": "value: int = 'bad'\n",
                }
            },
        )
        new_report = client.request("textDocument/diagnostic", new)
        assert len(new_report["items"]) == 1, new_report
        assert not (second / "unsaved.py").exists()
        client.notify(
            "textDocument/didChange",
            {
                **new,
                "textDocument": {**new["textDocument"], "version": 2},
                "contentChanges": [{"text": "value: int = 1\n"}],
            },
        )
        assert client.request("textDocument/diagnostic", new)["items"] == []
        client.notify("textDocument/didClose", new)
        assert client.request("textDocument/diagnostic", new)["items"] == []
        evidence["unsaved_file"] = new_report
        assert initial_pids == client.request("tyCompanion/statistics")["checker_pids"]

        # Installing a previously unresolved dependency must invalidate both
        # the projection and the native checker's negative module lookup.
        dependency_document = {
            "textDocument": {"uri": (second / "dependency_use.py").as_uri()}
        }
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    **dependency_document["textDocument"],
                    "version": 1,
                    "languageId": "python",
                    "text": "import installed_later\nvalue: int = installed_later.value\n",
                }
            },
        )
        missing = client.request("textDocument/diagnostic", dependency_document)
        assert any(
            item.get("code") == "unresolved-import" for item in missing["items"]
        ), missing
        installed = dependencies / "installed_later.py"
        installed.write_text("value = 42\n", encoding="utf-8")
        client.notify(
            "workspace/didChangeWatchedFiles",
            {"changes": [{"uri": installed.as_uri(), "type": 1}]},
        )
        available = client.request("textDocument/diagnostic", dependency_document)
        assert available["items"] == [], available
        client.notify("textDocument/didClose", dependency_document)
        evidence["new_dependency"] = {"before": missing, "after": available}

        # Opaque editor documents retain their original URI and never appear
        # as synthetic files in the user's project.
        untitled = {"textDocument": {"uri": "untitled:Untitled-1"}}
        original_files = set(first.iterdir())
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    **untitled["textDocument"],
                    "version": 1,
                    "languageId": "python",
                    "text": "value: int = 'bad'\n",
                }
            },
        )
        untitled_report = client.request("textDocument/diagnostic", untitled)
        assert len(untitled_report["items"]) == 1, untitled_report
        assert set(first.iterdir()) == original_files
        client.notify("textDocument/didClose", untitled)
        evidence["untitled"] = untitled_report

        # A real config change must invalidate the projection and checker rules.
        config.write_text(
            config.read_text(encoding="utf-8")
            + '\n[rules]\ninvalid-assignment="ignore"\n',
            encoding="utf-8",
        )
        client.notify(
            "workspace/didChangeWatchedFiles",
            {"changes": [{"uri": config.as_uri(), "type": 2}]},
        )
        assert client.request("textDocument/diagnostic", one)["items"] == []
        assert len(client.request("textDocument/diagnostic", two)["items"]) == 1
        evidence["independent_rules"] = True

        client.notify(
            "workspace/didChangeWorkspaceFolders",
            {
                "event": {
                    "removed": [{"uri": first.as_uri(), "name": "first"}],
                    "added": [],
                }
            },
        )
        assert client.request("tyCompanion/statistics")["workspaces"] == 1
        assert len(client.request("textDocument/diagnostic", two)["items"]) == 1
        client.notify(
            "workspace/didChangeWorkspaceFolders",
            {
                "event": {
                    "removed": [],
                    "added": [{"uri": first.as_uri(), "name": "first"}],
                }
            },
        )
        assert client.request("tyCompanion/statistics")["workspaces"] == 2
        assert client.request("textDocument/diagnostic", one)["items"] == []
        assert (first / "demo.py").read_text(encoding="utf-8") == SOURCE
        assert (second / "other.py").read_text(encoding="utf-8") == other_text
        evidence["statistics"] = client.request("tyCompanion/statistics")
        evidence["timings"] = client.timings
        client.notify(
            "workspace/didChangeWorkspaceFolders",
            {
                "event": {
                    "removed": [
                        {"uri": first.as_uri(), "name": "first"},
                        {"uri": second.as_uri(), "name": "second"},
                    ],
                    "added": [],
                }
            },
        )
        assert client.request("tyCompanion/statistics")["workspaces"] == 0
        registrations = [
            message["params"]["registrations"][0]
            for message in client.server_requests
            if message["method"] == "client/registerCapability"
        ]
        unregistrations = [
            message["params"]["unregisterations"][0]
            for message in client.server_requests
            if message["method"] == "client/unregisterCapability"
        ]
        assert registrations and unregistrations
        assert len({item["id"] for item in registrations}) == len(registrations)
        assert unregistrations[-1]["id"] == registrations[-1]["id"]
        assert all(
            watcher["kind"] == 7 and "analysis" not in watcher["globPattern"]
            for item in registrations
            for watcher in item["registerOptions"]["watchers"]
        )
        evidence["watcher_registrations"] = registrations
        evidence["watcher_unregistrations"] = unregistrations
        assert client.request("workspace/symbol", {"query": "value"}) == []
        # A standalone file outside all folders is still a valid LSP document.
        outside = args.output / "outside"
        outside.mkdir()
        outside_file = outside / "standalone.py"
        outside_file.write_text("value: int = 'bad'\n", encoding="utf-8")
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": outside_file.as_uri(),
                    "version": 1,
                    "languageId": "python",
                    "text": outside_file.read_text(encoding="utf-8"),
                }
            },
        )
        assert (
            len(
                client.request(
                    "textDocument/diagnostic",
                    {"textDocument": {"uri": outside_file.as_uri()}},
                )["items"]
            )
            == 1
        )
        evidence["standalone_after_remove_all"] = True
        evidence["passed"] = True
    finally:
        client.close()
        (args.output / "evidence.json").write_text(
            json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
        )
    print(
        "Configured plugin, multiple workspaces, unsaved files and config reload: PASS"
    )


if __name__ == "__main__":
    main()
