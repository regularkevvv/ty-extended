#!/usr/bin/env python3
"""Exercise the pinned resolver's plugin-overlay contracts through real binaries."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path

from harness_paths import file_uri_matches
from lsp_acceptance import Client, position

CASES = [
    (
        "first_party_precedes_plugin_stub_overlay",
        {"foo.py": "value: int = 1\n"},
        {"foo.pyi": "value: str\n"},
        {"foo.py": 'value: bytes = b""\n'},
        "from typing import assert_type\nimport foo\nassert_type(foo.value, int)\n",
    ),
    (
        "plugin_stub_overlay_precedes_site_packages",
        {},
        {"foo.pyi": "def value() -> str: ...\n"},
        {"foo.py": "def value() -> int:\n    return 1\n"},
        "from typing import assert_type\nimport foo\nassert_type(foo.value(), str)\n",
    ),
    (
        "stdlib_precedes_plugin_stub_overlay",
        {},
        {"json/__init__.pyi": "def dumps(value: object) -> int: ...\n"},
        {},
        "from typing import assert_type\nimport json\nassert_type(json.dumps(1), str)\n",
    ),
    (
        "partial_plugin_stub_overlay_falls_back_to_site_packages_submodule",
        {},
        {
            "foo/__init__.pyi": "",
            "foo/py.typed": "partial\n",
            "foo/overlay_only.pyi": "value: str\n",
        },
        {"foo/__init__.py": "", "foo/runtime_only.py": 'value: bytes = b""\n'},
        "from typing import assert_type\nfrom foo import overlay_only, runtime_only\n"
        "assert_type(overlay_only.value, str)\nassert_type(runtime_only.value, bytes)\n",
    ),
]


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def write(path: Path, value: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, indent=2) + "\n", encoding="utf-8")


def put(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def setup(case: Path, sources: dict, stubs: dict, installed: dict, main: str) -> Path:
    root = case / "project"
    overlay = case / "stubs"
    environment = case / "environment"
    for base, files in [
        (root, sources),
        (overlay, stubs),
        (environment / "lib/python3.13/site-packages", installed),
        (environment / "Lib/site-packages", installed),
    ]:
        base.mkdir(parents=True, exist_ok=True)
        for name, text in files.items():
            put(base / name, text)
    put(
        environment / "pyvenv.cfg",
        "home = .\nversion_info = 3.13.0\ninclude-system-site-packages = false\n",
    )
    manifest = {
        "id": "overlay",
        "name": "Overlay resolver contracts",
        "version": "0.0.0",
        "protocol-version": {"major": 0, "minor": 7},
        "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
        "runtime": {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": {"stub-overlays": True},
    }
    write(case / "plugin.json", manifest)
    put(case / "plugin.py", "set_manifest(" + repr(manifest) + ")\n")
    put(root / "main.py", main)
    put(
        root / "ty.toml",
        '[environment]\npython-version="3.13"\npython='
        + json.dumps(str(environment))
        + "\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\n"
        'id="overlay"\nruntime="monty"\ntrusted=true\npath="../plugin.py"\n'
        'manifest-path="../plugin.json"\nstub-overlay-path="../stubs"\n',
    )
    return root


def diagnostics(client: Client, path: Path) -> list[dict]:
    report = client.request(
        "textDocument/diagnostic", {"textDocument": {"uri": path.as_uri()}}
    )
    # LSP optional fields may be omitted or null with identical semantics.
    items = [
        {key: value for key, value in item.items() if value is not None}
        for item in report["items"]
    ]
    return sorted(
        items, key=lambda item: (json.dumps(item["range"]), item.get("code", ""))
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    for name in ("companion", "official", "fork", "output"):
        setattr(args, name, getattr(args, name).resolve())
    args.output.mkdir(parents=True)
    original = (
        Path(__file__).resolve().parents[2]
        / "ruff/crates/ty_module_resolver/src/resolve.rs"
    )
    source = original.read_text(encoding="utf-8")
    original_names = [row[0] for row in CASES] + [
        "real_module_resolution_ignores_plugin_stub_overlay"
    ]
    for name in original_names:
        assert f"fn {name}()" in source, (name, "pinned resolver fixture moved")
    result = {
        "passed": False,
        "original_source": str(original),
        "original_source_sha256": digest(original),
        "runner_sha256": digest(Path(__file__)),
        "path_helper_sha256": digest(Path(__file__).with_name("harness_paths.py")),
        "original_contracts": original_names,
        "fixture_kind": "Original resolver file topologies with executable type assertions",
        "binaries": {
            name: {
                "path": str(getattr(args, name)),
                "sha256": digest(getattr(args, name)),
            }
            for name in ("companion", "official", "fork")
        },
        "cases": [],
        "checks": [],
    }
    env = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}

    def command(backend: str, mode: str) -> list[str]:
        return [str(getattr(args, backend)), mode] + (
            ["--ty", str(args.official)] if backend == "companion" else []
        )

    def start(
        backend: str, root: Path, name: str, inline: dict | None = None
    ) -> Client:
        client = Client(
            command(backend, "server"), root, root.parent / f"{backend}-{name}.stderr"
        )
        params = {
            "rootUri": root.as_uri(),
            "workspaceFolders": [{"uri": root.as_uri(), "name": "project"}],
            "capabilities": {
                "textDocument": {"diagnostic": {}},
                "workspace": {"didChangeWatchedFiles": {"dynamicRegistration": True}},
            },
        }
        if inline:
            params["initializationOptions"] = {"configuration": inline}
        client.request("initialize", params)
        client.notify("initialized")
        text = (root / "main.py").read_text(
            encoding="utf-8"
        ) + "# unsaved editor buffer\n"
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": (root / "main.py").as_uri(),
                    "languageId": "python",
                    "version": 1,
                    "text": text,
                }
            },
        )
        return client

    try:
        for name, sources, stubs, installed, main_source in CASES:
            case = args.output / name
            root = setup(case, sources, stubs, installed, main_source)
            row = {"name": name, "runs": []}
            result["cases"].append(row)
            for backend in ("fork", "companion"):
                cmd = [
                    *command(backend, "check"),
                    "main.py",
                    "--project",
                    ".",
                    "--output-format",
                    "gitlab",
                ]
                process = subprocess.run(
                    cmd, cwd=root, env=env, capture_output=True, timeout=60
                )
                (case / f"{backend}.stdout").write_bytes(process.stdout)
                (case / f"{backend}.stderr").write_bytes(process.stderr)
                row["runs"].append(
                    {"backend": backend, "command": cmd, "exit": process.returncode}
                )
                assert process.returncode == 0 and json.loads(process.stdout) == [], row
            if name == "plugin_stub_overlay_precedes_site_packages":
                reports = {}
                for backend in ("fork", "companion"):
                    client = start(backend, root, "definition-and-reload")
                    try:
                        path = root / "main.py"
                        assert diagnostics(client, path) == []
                        declaration = client.request(
                            "textDocument/definition",
                            {
                                "textDocument": {"uri": path.as_uri()},
                                "position": position(main_source, "foo.value", 5),
                            },
                        )
                        reports[backend] = {"definition": declaration, "revisions": []}
                        expected_site = (
                            case
                            / "environment"
                            / ("Lib" if os.name == "nt" else "lib/python3.13")
                            / "site-packages/foo.py"
                        )
                        assert file_uri_matches(declaration[0]["uri"], expected_site), (
                            declaration
                        )
                        for annotation, expected in [("int", 1), ("str", 0)]:
                            changed = case / "stubs/foo.pyi"
                            put(changed, f"def value() -> {annotation}: ...\n")
                            client.notify(
                                "workspace/didChangeWatchedFiles",
                                {"changes": [{"uri": changed.as_uri(), "type": 2}]},
                            )
                            items = diagnostics(client, path)
                            reports[backend]["revisions"].append(items)
                            assert len(items) == expected, (backend, annotation, items)
                    finally:
                        client.close()
                assert reports["fork"] == reports["companion"], reports
                row["editor"] = reports
                result["checks"].extend(
                    [
                        "installed-source-definition",
                        "overlay-live-change",
                        "overlay-live-restore",
                    ]
                )
            if (
                name
                == "partial_plugin_stub_overlay_falls_back_to_site_packages_submodule"
            ):
                reports = {}
                for backend in ("fork", "companion"):
                    client = start(backend, root, "partial-marker-reload")
                    try:
                        path = root / "main.py"
                        assert diagnostics(client, path) == []
                        marker = case / "stubs/foo/py.typed"
                        marker.unlink()
                        client.notify(
                            "workspace/didChangeWatchedFiles",
                            {"changes": [{"uri": marker.as_uri(), "type": 3}]},
                        )
                        missing = diagnostics(client, path)
                        assert any(
                            d.get("code") == "unresolved-import" for d in missing
                        ), missing
                        put(marker, "partial\n")
                        client.notify(
                            "workspace/didChangeWatchedFiles",
                            {"changes": [{"uri": marker.as_uri(), "type": 1}]},
                        )
                        restored = diagnostics(client, path)
                        assert restored == [], restored
                        reports[backend] = {"removed": missing, "restored": restored}
                    finally:
                        client.close()
                assert reports["fork"] == reports["companion"], reports
                row["editor"] = reports
                result["checks"].extend(
                    ["partial-marker-live-remove", "partial-marker-live-restore"]
                )
            assert (root / "main.py").read_text(encoding="utf-8") == main_source
        # Literal original #12 fixture, transported through the public LSP.
        source12 = original.parents[2] / "ty_ide/src/goto_definition.rs"
        source12_text = source12.read_text(encoding="utf-8")
        assert (
            "fn goto_definition_plugin_stub_overlay_maps_to_site_packages_source()"
            in source12_text
        )
        main12 = "from foo import make\nmake()\n"
        overlay12 = "def make() -> int: ...\n"
        installed12 = "def make():\n    return 1\n"
        case12 = (
            args.output
            / "goto_definition_plugin_stub_overlay_maps_to_site_packages_source"
        )
        root12 = setup(
            case12,
            {},
            {"foo/__init__.pyi": overlay12},
            {"foo/__init__.py": installed12},
            main12,
        )
        reports12 = {}
        for backend in ("fork", "companion"):
            client = start(backend, root12, "original12")
            try:
                path = root12 / "main.py"
                assert diagnostics(client, path) == []
                location = client.request(
                    "textDocument/definition",
                    {
                        "textDocument": {"uri": path.as_uri()},
                        "position": {"line": 1, "character": 4},
                    },
                )
                expected_path = (
                    case12
                    / "environment"
                    / ("Lib" if os.name == "nt" else "lib/python3.13")
                    / "site-packages/foo/__init__.py"
                )
                expected = [
                    {
                        "uri": expected_path.as_uri(),
                        "range": {
                            "start": {"line": 0, "character": 4},
                            "end": {"line": 0, "character": 8},
                        },
                    }
                ]
                assert len(location) == 1 and file_uri_matches(
                    location[0]["uri"], expected_path
                ), (backend, location, expected)
                assert location == [expected[0] | {"uri": location[0]["uri"]}], (
                    backend,
                    location,
                    expected,
                )
                reports12[backend] = location
            finally:
                client.close()
        result["original12"] = {
            "source": str(source12),
            "source_sha256": digest(source12),
            "fixture": "exact original Python and overlay/runtime package topology, cursor marker removed",
            "reports": reports12,
        }
        result["checks"].append(
            "original12-package-function-definition-to-installed-source"
        )
        main194 = "from foo import make\nvalue: int = make()\n"
        case194 = (
            args.output
            / "plugin_stub_overlay_diagnostics_update_on_watched_file_change"
        )
        root194 = setup(
            case194, {}, {"foo/__init__.pyi": "def make() -> int: ...\n"}, {}, main194
        )
        reports194 = {}
        overlay194 = case194 / "stubs/foo/__init__.pyi"
        for backend in ("fork", "companion"):
            put(overlay194, "def make() -> int: ...\n")
            client = start(backend, root194, "original194")
            try:
                path = root194 / "main.py"
                before = diagnostics(client, path)
                assert before == [], (backend, before)
                put(overlay194, "def make() -> str: ...\n")
                client.notify(
                    "workspace/didChangeWatchedFiles",
                    {"changes": [{"uri": overlay194.as_uri(), "type": 2}]},
                )
                after = diagnostics(client, path)
                assert len(after) == 1 and after[0]["code"] == "invalid-assignment", (
                    backend,
                    after,
                )
                reports194[backend] = {"before": before, "after": after}
            finally:
                client.close()
        assert reports194["companion"] == reports194["fork"], reports194
        result["original194"] = {
            "fixture": "Exact original Python and overlay edit, using a supported Monty registration in place of the native internal mock runtime",
            "reports": reports194,
        }
        result["checks"].append("original194-overlay-document-diagnostic-watch-update")
        case = args.output / "inline_extra_precedes_first_party_and_overlay"
        root = setup(
            case,
            {"foo.py": "value: int = 1\n"},
            {"foo.pyi": "value: str\n"},
            {},
            "from typing import assert_type\nimport foo\nassert_type(foo.value, bytes)\n",
        )
        extra = case / "user-extra"
        put(extra / "foo.pyi", "value: bytes\n")
        for backend in ("fork", "companion"):
            client = start(
                backend,
                root,
                "inline-extra",
                {"environment": {"extra-paths": [str(extra)]}},
            )
            try:
                items = diagnostics(client, root / "main.py")
                assert items == [], (backend, items)
            finally:
                client.close()
        result["checks"].append("inline-extra-before-synthetic-roots")
        result["passed"] = True
    finally:
        write(args.output / "summary.json", result)
    print(
        f"Passed {len(original_names)} original overlay contracts and {len(result['checks'])} editor checks"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
