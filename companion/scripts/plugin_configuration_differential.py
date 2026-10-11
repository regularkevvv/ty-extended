#!/usr/bin/env python3
"""Compare recoverable Monty configuration/load errors with the pinned fork."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
from pathlib import Path

from lsp_acceptance import Client


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(
        json.dumps(value, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )


def manifest(name, function):
    return {
        "id": name,
        "name": name,
        "version": "0.0.1",
        "protocol-version": {"major": 0, "minor": 7},
        "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
        "runtime": {"kind": "monty", "artifact": "plugin.py"},
        "capabilities": {"call-return": True},
        "claims": {"functions": [{"qualified-name": function}]},
    }


def diagnostics(values, root):
    out = []
    for value in values:
        item = {
            key: value[key]
            for key in ("check_name", "description", "severity", "location")
        }
        item["location"] = dict(item["location"])
        raw = item["location"]["path"]
        if raw:
            path = Path(raw)
            if not path.is_absolute():
                path = root / path
            item["location"]["path"] = os.path.relpath(path.resolve(), root).replace(
                "\\", "/"
            )
        out.append(item)
    return sorted(out, key=lambda x: json.dumps(x, sort_keys=True))


def lsp_diagnostics(values):
    return sorted(
        [
            {k: v[k] for k in ("range", "message", "code", "severity") if k in v}
            for v in values
            if v.get("code")
        ],
        key=lambda v: json.dumps(v, sort_keys=True),
    )


class Gate:
    def __init__(self, args):
        self.args = args
        args.output.mkdir(parents=True)
        self.report = {
            "passed": False,
            "artifact": str(args.companion),
            "sha256": digest(args.companion),
            "official": {"path": str(args.official), "sha256": digest(args.official)},
            "fork": {"path": str(args.fork), "sha256": digest(args.fork)},
            "source_sha256": {
                name: digest(Path(__file__).with_name(name))
                for name in (Path(__file__).name, "lsp_acceptance.py")
            },
            "checks": [],
            "cases": {},
        }

    def equal(self, name, actual, expected):
        passed = actual == expected
        self.report["checks"].append({"name": name, "equal": passed})
        assert passed, (
            f"{name}: {json.dumps({'actual': actual, 'expected': expected}, ensure_ascii=False)}"
        )

    def fixture(self, name):
        case = self.args.output / name
        root = case / "project"
        root.mkdir(parents=True)
        bad = manifest("research.invalid", "demo.bad")
        good = manifest("research.valid", "demo.answer")
        for prefix, value in (("plugin", bad), ("good", good)):
            save(case / (prefix + ".json"), value)
            (case / (prefix + ".py")).write_text(
                "set_manifest("
                + repr(value)
                + ")\n@on_call_return_of("
                + repr(value["claims"]["functions"][0]["qualified-name"])
                + ")\ndef callback(request):\n    return call_return_patch(type_annotation('int'))\n",
                encoding="utf-8",
            )
        # Unicode before value spans ensures byte offsets are converted, not
        # accidentally reported as Unicode/LSP character offsets.
        config = '# café 😀\n[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid="research.invalid"\npath="../plugin.py"\nruntime="monty"\ntrusted=true\nmanifest-path="../plugin.json"\n'
        if name == "disabled":
            config = config.replace("enabled=true", "enabled=false")
        elif name in ("missing-artifact", "pyproject-origin"):
            config = config.replace('path="../plugin.py"', 'path="../missing.py"')
        elif name == "missing-manifest":
            config = config.replace(
                'manifest-path="../plugin.json"', 'manifest-path="../missing.json"'
            )
        elif name == "untrusted":
            config = config.replace("trusted=true", "trusted=false")
        elif name == "bad-manifest":
            (case / "plugin.json").write_text("{", encoding="utf-8")
        elif name == "id-mismatch":
            bad["id"] = "different"
            save(case / "plugin.json", bad)
        elif name == "protocol":
            bad["protocol-version"] = {"major": 99, "minor": 1}
            save(case / "plugin.json", bad)
        elif name in ("missing-stubs", "stub-capability"):
            config += 'stub-overlay-path="../stubs"\n'
            if name == "stub-capability":
                (case / "stubs").mkdir()
        elif name == "invalid-artifact":
            (case / "plugin.py").write_text("this is ( invalid", encoding="utf-8")
        elif name == "crashing-artifact":
            with (case / "plugin.py").open("a", encoding="utf-8") as handle:
                handle.write('raise ValueError("crash before request")\n')
        elif name == "invalid-utf8":
            (case / "plugin.py").write_bytes(b"\xff")
        config += '[[plugins.plugin]]\nid="research.valid"\npath="../good.py"\nruntime="monty"\ntrusted=true\nmanifest-path="../good.json"\n'
        expected = "str" if name in ("disabled", "protocol") else "int"
        source = (
            'from typing import assert_type\ndef answer() -> str:\n    return "original"\ndef bad() -> str:\n    return "original"\nassert_type(answer(), '
            + expected
            + ')\nbad()\nlater: int = "checking continues"\n'
        )
        (root / "demo.py").write_text(source, encoding="utf-8")
        if name == "pyproject-origin":
            config = (
                config.replace("[environment]", "[tool.ty.environment]")
                .replace("[plugins]", "[tool.ty.plugins]")
                .replace("[[plugins.plugin]]", "[[tool.ty.plugins.plugin]]")
            )
            (root / "pyproject.toml").write_text(config, encoding="utf-8")
        else:
            (root / "ty.toml").write_text(config, encoding="utf-8")
        self.report["cases"][name] = {
            "root": str(root),
            "files": {
                str(path.relative_to(case)): digest(path)
                for path in case.rglob("*")
                if path.is_file()
            },
        }
        return case, root

    def command(self, command, root, path):
        result = subprocess.run(
            command,
            cwd=root,
            capture_output=True,
            encoding="utf-8",
            timeout=120,
            env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
        )
        save(path.with_suffix(".command.json"), {"argv": command, "cwd": str(root)})
        path.with_suffix(".stdout").write_text(result.stdout, encoding="utf-8")
        path.with_suffix(".stderr").write_text(result.stderr, encoding="utf-8")
        assert result.returncode == 1, (
            command,
            result.returncode,
            result.stdout,
            result.stderr,
        )
        return result

    def cli(self, name, case, root):
        common = ["check", "--project", ".", "--output-format", "gitlab", "demo.py"]
        native = self.command([str(self.args.fork), *common], root, case / "fork")
        expected = diagnostics(json.loads(native.stdout), root)
        assert not any(x["check_name"] == "type-assertion-failure" for x in expected), (
            expected
        )
        assert any(x["check_name"] == "invalid-assignment" for x in expected), expected
        assert any(x["check_name"] == "plugin-configuration" for x in expected), (
            expected
        )
        actual = self.command(
            [str(self.args.companion), *common, "--ty", str(self.args.official)],
            root,
            case / "companion",
        )
        self.equal(
            name + "/public-cli", diagnostics(json.loads(actual.stdout), root), expected
        )
        self.report["cases"][name]["native_diagnostics"] = expected

    def editor(self, name, case, root):
        clients = {}
        raw = {}
        try:
            for backend, binary in (
                ("fork", self.args.fork),
                ("companion", self.args.companion),
            ):
                command = [str(binary), "server"]
                if backend == "companion":
                    command += [
                        "--ty",
                        str(self.args.official),
                        "--out",
                        str(case / "analysis"),
                    ]
                client = Client(command, root, case / (backend + ".lsp.stderr"))
                clients[backend] = client
                client.request(
                    "initialize",
                    {
                        "rootUri": root.as_uri(),
                        "capabilities": {
                            "general": {"positionEncodings": ["utf-16"]},
                            "textDocument": {"diagnostic": {}},
                        },
                    },
                )
                client.notify("initialized")
                source = root / "demo.py"
                client.notify(
                    "textDocument/didOpen",
                    {
                        "textDocument": {
                            "uri": source.as_uri(),
                            "version": 1,
                            "languageId": "python",
                            "text": source.read_text(encoding="utf-8"),
                        }
                    },
                )
                current = client.request(
                    "textDocument/diagnostic",
                    {"textDocument": {"uri": source.as_uri()}},
                )
                # A second request is a barrier for publications queued after the
                # first response in the wrapper's event loop.
                config = client.request(
                    "textDocument/diagnostic",
                    {"textDocument": {"uri": (root / "ty.toml").as_uri()}},
                )
                published = {}
                for value in client.notifications:
                    if value["method"] == "textDocument/publishDiagnostics":
                        params = value["params"]
                        published[params["uri"]] = lsp_diagnostics(
                            params["diagnostics"]
                        )
                raw[backend] = {
                    "source": lsp_diagnostics(current["items"]),
                    "config_pull": lsp_diagnostics(config["items"]),
                    "published": {
                        uri: values for uri, values in published.items() if values
                    },
                }
            self.equal(name + "/editor", raw["companion"], raw["fork"])
            if name in ("missing-artifact", "bad-manifest", "invalid-artifact"):
                target = case / (
                    "missing.py"
                    if name == "missing-artifact"
                    else "plugin.json"
                    if name == "bad-manifest"
                    else "plugin.py"
                )
                old = target.read_bytes() if target.exists() else None
                valid = manifest("research.invalid", "demo.bad")
                contents = (
                    json.dumps(valid)
                    if target.suffix == ".json"
                    else "set_manifest("
                    + repr(valid)
                    + ")\n@on_call_return_of('demo.bad')\ndef callback(request):\n    return call_return_patch(type_annotation('int'))\n"
                )
                target.write_text(contents, encoding="utf-8")
                source = root / "demo.py"
                original = source.read_text(encoding="utf-8")
                for stage, text, event in (
                    (
                        "recovered",
                        original.replace(
                            "bad()\nlater", "assert_type(bad(), int)\nlater"
                        ),
                        1 if old is None else 2,
                    ),
                    ("failed-again", original, 3 if old is None else 2),
                ):
                    if stage == "failed-again":
                        if old is None:
                            target.unlink()
                        else:
                            target.write_bytes(old)
                    snapshots = {}
                    for backend, client in clients.items():
                        client.notify(
                            "workspace/didChangeWatchedFiles",
                            {"changes": [{"uri": target.as_uri(), "type": event}]},
                        )
                        client.notify(
                            "textDocument/didChange",
                            {
                                "textDocument": {
                                    "uri": source.as_uri(),
                                    "version": 2 if stage == "recovered" else 3,
                                },
                                "contentChanges": [{"text": text}],
                            },
                        )
                        result = client.request(
                            "textDocument/diagnostic",
                            {"textDocument": {"uri": source.as_uri()}},
                        )
                        client.request(
                            "textDocument/diagnostic",
                            {"textDocument": {"uri": source.as_uri()}},
                        )
                        published = {}
                        for notification in client.notifications:
                            if (
                                notification["method"]
                                == "textDocument/publishDiagnostics"
                            ):
                                params = notification["params"]
                                published[params["uri"]] = lsp_diagnostics(
                                    params["diagnostics"]
                                )
                        snapshots[backend] = {
                            "source": lsp_diagnostics(result["items"]),
                            "published": {
                                uri: items for uri, items in published.items() if items
                            },
                        }
                    self.equal(
                        name + "/" + stage, snapshots["companion"], snapshots["fork"]
                    )
                    assert not any(
                        item.get("code") == "type-assertion-failure"
                        for item in snapshots["fork"]["source"]
                    ), snapshots
                    if stage == "recovered":
                        assert not snapshots["fork"]["published"], snapshots
                    raw[stage] = snapshots
            save(case / "lsp.json", raw)
        finally:
            for client in clients.values():
                client.close()

    def warnings_only(self, case, root):
        source = root / "demo.py"
        original = source.read_bytes()
        try:
            source.write_text("pass\n", encoding="utf-8")
            for strict in (False, True):
                values = {}
                for backend, binary in (
                    ("fork", self.args.fork),
                    ("companion", self.args.companion),
                ):
                    command = [
                        str(binary),
                        "check",
                        "--project",
                        ".",
                        "--output-format",
                        "gitlab",
                        "demo.py",
                    ]
                    command.append(
                        "--error-on-warning" if strict else "--exit-zero-on-warning"
                    )
                    if backend == "companion":
                        command += ["--ty", str(self.args.official)]
                    result = subprocess.run(
                        command,
                        cwd=root,
                        capture_output=True,
                        encoding="utf-8",
                        timeout=120,
                        env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
                    )
                    prefix = case / (backend + "-warnings-" + str(strict))
                    save(
                        prefix.with_suffix(".command.json"),
                        {"argv": command, "cwd": str(root)},
                    )
                    prefix.with_suffix(".stdout").write_text(
                        result.stdout, encoding="utf-8"
                    )
                    prefix.with_suffix(".stderr").write_text(
                        result.stderr, encoding="utf-8"
                    )
                    items = diagnostics(json.loads(result.stdout), root)
                    assert len(items) == 2 and all(
                        item["severity"] == "minor" for item in items
                    ), items
                    assert result.returncode == int(strict), (
                        command,
                        result.returncode,
                        result.stderr,
                    )
                    values[backend] = {
                        "exit_code": result.returncode,
                        "diagnostics": items,
                    }
                self.equal(
                    "disabled/warning-exit-" + str(strict),
                    values["companion"],
                    values["fork"],
                )
        finally:
            source.write_bytes(original)

    def inline(self):
        case = self.args.output / "inline"
        root = case / "project"
        root.mkdir(parents=True)
        (root / "demo.py").write_text('value: int = "bad"\n', encoding="utf-8")
        (root / "ty.toml").write_text(
            '[environment]\npython-version="3.13"\n', encoding="utf-8"
        )
        (root / "plugin.py").write_text("pass\n", encoding="utf-8")
        overrides = [
            "--config",
            "plugins.enabled=false",
            "--config",
            'plugins.plugin=[{id="inline",path="plugin.py",runtime="monty",trusted=true}]',
        ]
        for format in ("gitlab", "full", "concise", "github", "junit"):
            values = {}
            for backend, binary in (
                ("fork", self.args.fork),
                ("companion", self.args.companion),
            ):
                command = [
                    str(binary),
                    "check",
                    "--project",
                    ".",
                    "--output-format",
                    format,
                    "demo.py",
                    *overrides,
                ]
                if backend == "companion":
                    command += ["--ty", str(self.args.official)]
                result = self.command(command, root, case / (backend + "-" + format))
                if format == "gitlab":
                    values[backend] = diagnostics(json.loads(result.stdout), root)
                elif format == "junit":
                    import xml.etree.ElementTree as ET

                    node = next(
                        node
                        for node in ET.fromstring(result.stdout).iter("testcase")
                        if node.attrib["name"] == "org.ty.plugin-configuration"
                    )
                    failure = node.find("failure")
                    values[backend] = [node.attrib, failure.attrib, failure.text]
                else:
                    lines = result.stdout.splitlines()
                    values[backend] = [
                        line.strip()
                        for line in lines
                        if "plugin-configuration" in line
                        or line.startswith("info:")
                        or line.lstrip().startswith("info:")
                    ]
                    assert not any(line.startswith(" --> :") for line in lines), (
                        result.stdout
                    )
            self.equal("inline/" + format, values["companion"], values["fork"])
        values = {}
        for backend, binary in (
            ("fork", self.args.fork),
            ("companion", self.args.companion),
        ):
            command = [str(binary), "server"]
            if backend == "companion":
                command += [
                    "--ty",
                    str(self.args.official),
                    "--out",
                    str(case / "analysis"),
                ]
            client = Client(command, root, case / (backend + ".lsp.stderr"))
            try:
                client.request(
                    "initialize",
                    {
                        "rootUri": root.as_uri(),
                        "capabilities": {"textDocument": {"diagnostic": {}}},
                        "initializationOptions": {
                            "configuration": {
                                "plugins": {
                                    "enabled": False,
                                    "plugin": [
                                        {
                                            "id": "inline",
                                            "path": "plugin.py",
                                            "runtime": "monty",
                                            "trusted": True,
                                        }
                                    ],
                                }
                            }
                        },
                    },
                )
                client.notify("initialized")
                uri = (root / "demo.py").as_uri()
                client.notify(
                    "textDocument/didOpen",
                    {
                        "textDocument": {
                            "uri": uri,
                            "version": 1,
                            "languageId": "python",
                            "text": (root / "demo.py").read_text(encoding="utf-8"),
                        }
                    },
                )
                result = client.request(
                    "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                )
                client.request(
                    "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                )
                values[backend] = {
                    "source": lsp_diagnostics(result["items"]),
                    "messages": [
                        notification
                        for notification in client.notifications
                        if notification["method"]
                        in ("window/showMessage", "textDocument/publishDiagnostics")
                    ],
                }
            finally:
                client.close()
        self.equal("inline/editor", values["companion"], values["fork"])

    def run(self):
        try:
            for name in (
                "disabled",
                "missing-artifact",
                "pyproject-origin",
                "missing-manifest",
                "untrusted",
                "bad-manifest",
                "id-mismatch",
                "protocol",
                "missing-stubs",
                "stub-capability",
                "invalid-artifact",
                "crashing-artifact",
                "invalid-utf8",
            ):
                case, root = self.fixture(name)
                self.cli(name, case, root)
                self.editor(name, case, root)
                if name == "disabled":
                    self.warnings_only(case, root)
                for path, expected in self.report["cases"][name]["files"].items():
                    assert digest(case / path) == expected, (
                        f"fixture changed: {case / path}"
                    )
            self.inline()
            self.equal(
                "artifact-unchanged", digest(self.args.companion), self.report["sha256"]
            )
            self.report["passed"] = True
        finally:
            self.report["check_count"] = len(self.report["checks"])
            save(self.args.output / "summary.json", self.report)
        print(
            json.dumps(
                {
                    "passed": True,
                    "checks": self.report["check_count"],
                    "summary": str(self.args.output / "summary.json"),
                }
            )
        )


def main():
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    for name in ("companion", "official", "fork", "output"):
        setattr(args, name, getattr(args, name).resolve())
    Gate(args).run()


if __name__ == "__main__":
    main()
