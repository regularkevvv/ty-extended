#!/usr/bin/env python3
"""Exercise the fork's recoverable semantic-hook failure contracts with real Monty.

Python SDK exceptions become valid protocol Error responses (native no-change).
Malformed hook responses instead fail in the host, report warnings for call and
mutation hooks, and conservatively discard object-state facts. Static class and
member hook failures remain native no-change without additional warnings.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path

from lsp_acceptance import position
from lsp_differential import DifferentialClient, hover

SOURCE = 'from typing import assert_type\n\nclass Record:\n    key: int | None = None\n    def set_key(self) -> None: pass\n    def fail(self) -> None: pass\n\nclass Static:\n    value: str = "native"\n\nclass Bag:\n    def __setitem__(self, key: str, value: str) -> None: pass\n\ndef returns() -> str:\n    return "native"\n\ndef signature(value: int) -> int:\n    return value\n\nfirst = Record()\nfirst.set_key()\nsecond = Record()\nsecond.set_key()\nalias = first\nassert_type(first.key, int)\nassert_type(second.key, int)\nalias.fail()\nassert_type(first.key, int | None)\nassert_type(alias.key, int | None)\nassert_type(second.key, int | None)\nresult = returns()\nassert_type(result, str)\nwrong_return: int = result\nsignature("wrong")\nbag = Bag()\nbag["key"] = "value"\nstatic = Static()\nassert_type(static.value, str)\nlater: int = "checking continued"\n'
PLUGIN = '@on_call_state_of("demo.Record")\ndef construct(request):\n    return call_state_patch(result_members={"key": type_annotation("None")}, fresh_result=True, preserves_other_objects=True)\n@on_call_state_of("demo.Record.set_key")\ndef set_key(request):\n    return call_state_patch(receiver_members={"key": type_annotation("int")}, preserves_other_objects=True)\n@on_call_state_of("demo.Record.fail")\ndef fail_state(request):\n    raise ValueError("state failure")\n@on_call_return_of("demo.returns")\ndef fail_return(request):\n    raise ValueError("return failure")\n@on_call_signature_of("demo.signature")\ndef fail_signature(request):\n    raise ValueError("signature failure")\n@on_mutation\ndef fail_mutation(request):\n    raise ValueError("mutation failure")\n@on_class_transform\ndef fail_class(request):\n    raise ValueError("class failure")\n@on_instance_member\ndef fail_member(request):\n    raise ValueError("member failure")\n'
MANIFEST = json.loads(
    '{"id": "research.failures", "name": "Runtime failure parity", "version": "0.0.1", "protocol-version": {"major": 0, "minor": 7}, "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"}, "runtime": {"kind": "monty", "artifact": "plugin.py"}, "capabilities": {"call-state": true, "call-signature": true, "call-return": true, "mutation-validation": true, "class-transform": true, "instance-member": true}, "claims": {"modules": [{"name": "demo", "recursive": false}], "constructors": [{"kind": "exact", "qualified-name": "demo.Record"}], "classes": [{"kind": "exact", "qualified-name": "demo.Static"}], "functions": [{"qualified-name": "demo.returns"}, {"qualified-name": "demo.signature"}], "methods": [{"kind": "exact", "class-qualified-name": "demo.Record", "method-name": "set_key"}, {"kind": "exact", "class-qualified-name": "demo.Record", "method-name": "fail"}], "mutations": [{"kind": "exact", "qualified-name": "demo.Bag"}], "attributes": [{"kind": "exact", "owner-qualified-name": "demo.Static", "attribute-name": "value", "scope": "instance"}]}}'
)


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def cli_diagnostics(value, root):
    result = []
    for item in value:
        path = Path(item["location"]["path"])
        if not path.is_absolute():
            path = root / path
        assert path.resolve() == (root / "demo.py").resolve(), item
        result.append(
            {key: item[key] for key in ("check_name", "description", "severity")}
            | {
                "location": {
                    "path": "demo.py",
                    "positions": item["location"]["positions"],
                }
            }
        )
    return sorted(result, key=lambda item: json.dumps(item, sort_keys=True))


def lsp_diagnostics(value):
    return sorted(
        [
            {
                key: item[key]
                for key in ("range", "message", "code", "severity")
                if key in item
            }
            for item in value["items"]
            if item.get("code")
        ],
        key=lambda item: json.dumps(item, sort_keys=True),
    )


class Gate:
    def __init__(self, args):
        self.args = args
        self.output = args.output
        self.output.mkdir(parents=True)
        self.evidence = {
            "passed": False,
            "artifact": str(args.companion),
            "sha256": digest(args.companion),
            "official": {"path": str(args.official), "sha256": digest(args.official)},
            "fork": {"path": str(args.fork), "sha256": digest(args.fork)},
            "source_sha256": {
                name: digest(Path(__file__).with_name(name))
                for name in (
                    "plugin_error_differential.py",
                    "lsp_acceptance.py",
                    "lsp_differential.py",
                )
            },
            "contract": "state_hook_failure_reports_and_discards_facts; native call/signature/return/mutation fallback and static-hook no-change",
            "checks": [],
            "cases": {},
        }

    def equal(self, name, actual, expected):
        passed = actual == expected
        self.evidence["checks"].append({"name": name, "equal": passed})
        assert passed, (
            f"{name}: {json.dumps({'actual': actual, 'expected': expected}, ensure_ascii=False)}"
        )

    def fixture(self, kind):
        case = self.output / kind
        root = case / "project"
        root.mkdir(parents=True)
        plugin = case / "plugin.py"
        body = PLUGIN
        if kind == "runtime-error":
            body = re.sub(
                r'raise ValueError\("[a-z ]+"\)',
                'return {"kind": "not-a-real-response"}',
                body,
            )
        plugin.write_text(
            "set_manifest(" + repr(MANIFEST) + ")\n" + body, encoding="utf-8"
        )
        manifest = case / "plugin.plugin.json"
        save(manifest, MANIFEST)
        (root / "demo.py").write_text(SOURCE, encoding="utf-8")
        (root / "ty.toml").write_text(
            '[environment]\npython-version="3.13"\n[plugins]\nenabled=true\nauto-discover=false\n'
            '[[plugins.plugin]]\nid="research.failures"\nruntime="monty"\ntrusted=true\n'
            'path="../plugin.py"\nmanifest-path="../plugin.plugin.json"\n',
            encoding="utf-8",
        )
        self.evidence["cases"][kind] = {
            "root": str(root),
            "files": {
                str(path): digest(path)
                for path in (plugin, manifest, root / "demo.py", root / "ty.toml")
            },
        }
        return case, root

    def command(self, command, root, output):
        result = subprocess.run(
            command,
            cwd=root,
            capture_output=True,
            encoding="utf-8",
            timeout=120,
            env={
                **os.environ,
                "TY_UV": "0",
                "TY_MAX_PARALLELISM": "1",
                "NO_COLOR": "1",
            },
        )
        save(output.with_suffix(".command.json"), {"argv": command, "cwd": str(root)})
        output.with_suffix(".stdout").write_text(result.stdout, encoding="utf-8")
        output.with_suffix(".stderr").write_text(result.stderr, encoding="utf-8")
        assert result.returncode == 1, (
            command,
            result.returncode,
            result.stdout,
            result.stderr,
        )
        return result

    def cli(self, kind, case, root):
        expected = self.command(
            [
                str(self.args.fork),
                "check",
                "--project",
                ".",
                "--output-format",
                "gitlab",
                "demo.py",
            ],
            root,
            case / "fork",
        )
        expected = cli_diagnostics(json.loads(expected.stdout), root)
        warnings = [d for d in expected if d["check_name"] == "plugin-configuration"]
        assert len(warnings) == (4 if kind == "runtime-error" else 0), expected
        assert len(expected) == (7 if kind == "runtime-error" else 3), expected
        assert not any(d["check_name"] == "type-assertion-failure" for d in expected), (
            expected
        )
        if warnings:
            assert {d["location"]["positions"]["begin"]["line"] for d in warnings} == {
                27,
                31,
                34,
                36,
            }, warnings
            assert all(d["severity"] == "minor" for d in warnings), warnings
        self.evidence["cases"][kind]["native_diagnostics"] = expected
        normal = self.command(
            [
                str(self.args.companion),
                "check",
                "--ty",
                str(self.args.official),
                "--project",
                ".",
                "--output-format",
                "gitlab",
                "demo.py",
            ],
            root,
            case / "companion",
        )
        self.equal(
            kind + "/public-cli",
            cli_diagnostics(json.loads(normal.stdout), root),
            expected,
        )
        for mode in ("cli", "push"):
            command = [
                str(self.args.companion),
                "project",
                str(root),
                "--plugin",
                str(case / "plugin.py"),
                "--manifest",
                str(case / "plugin.plugin.json"),
                "--ty",
                str(self.args.official),
                "--out",
                str(case / ("analysis-" + mode)),
                "--python-version",
                "3.13",
                "--checker-mode",
                mode,
            ]
            result = subprocess.run(
                command,
                cwd=root,
                capture_output=True,
                encoding="utf-8",
                timeout=120,
                env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
            )
            save(case / (mode + ".command.json"), {"argv": command, "cwd": str(root)})
            (case / (mode + ".stdout")).write_text(result.stdout, encoding="utf-8")
            (case / (mode + ".stderr")).write_text(result.stderr, encoding="utf-8")
            assert result.returncode == 1, (command, result.stdout, result.stderr)
            value = json.loads(result.stdout)
            self.equal(
                kind + "/project-" + mode,
                cli_diagnostics(value["diagnostics"], root),
                expected,
            )
        full = self.command(
            [
                str(self.args.companion),
                "check",
                "--ty",
                str(self.args.official),
                "--project",
                ".",
                "--output-format",
                "full",
                "demo.py",
            ],
            root,
            case / "companion-full",
        )
        if kind == "runtime-error":
            assert (
                "The plugin returned a response `ty` could not understand."
                in full.stdout
            ), full.stdout
        self.equal(
            kind + "/source-intact-after-cli",
            (root / "demo.py").read_text(encoding="utf-8"),
            SOURCE,
        )

    def editor(self, kind, case, root, encoding):
        out = case / encoding
        out.mkdir()
        source = root / "demo.py"
        uri = source.as_uri()
        document = {"textDocument": {"uri": uri}}
        caps = {
            "general": {"positionEncodings": [encoding]},
            "textDocument": {
                "diagnostic": {},
                "publishDiagnostics": {"relatedInformation": True},
                "hover": {"contentFormat": ["markdown"]},
            },
        }
        commands = {
            "fork": [str(self.args.fork), "server"],
            "companion": [
                str(self.args.companion),
                "server",
                "--ty",
                str(self.args.official),
                "--out",
                str(out / "analysis"),
            ],
        }
        clients = {}
        raw = {}
        try:
            for backend, command in commands.items():
                client = DifferentialClient(command, root, out / (backend + ".stderr"))
                clients[backend] = client
                client.request(
                    "initialize", {"rootUri": root.as_uri(), "capabilities": caps}
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
            for stage, text in (
                ("initial", SOURCE),
                (
                    "repaired",
                    SOURCE.replace("alias.fail()", "alias.set_key()")
                    .replace(
                        "assert_type(first.key, int | None)",
                        "assert_type(first.key, int)",
                    )
                    .replace(
                        "assert_type(alias.key, int | None)",
                        "assert_type(alias.key, int)",
                    )
                    .replace(
                        "assert_type(second.key, int | None)",
                        "assert_type(second.key, int)",
                    ),
                ),
            ):
                if stage == "repaired":
                    for client in clients.values():
                        client.notify(
                            "textDocument/didChange",
                            {
                                "textDocument": {"uri": uri, "version": 2},
                                "contentChanges": [{"text": text}],
                            },
                        )
                results = {
                    backend: client.request("textDocument/diagnostic", document)
                    for backend, client in clients.items()
                }
                raw[stage + "/diagnostics"] = results
                expected = lsp_diagnostics(results["fork"])
                assert not any(
                    d.get("code") == "type-assertion-failure" for d in expected
                ), expected
                assert sum(
                    d.get("code") == "plugin-configuration" for d in expected
                ) == (
                    (4 if stage == "initial" else 3) if kind == "runtime-error" else 0
                ), expected
                self.equal(
                    f"{kind}/{encoding}/{stage}/diagnostics",
                    lsp_diagnostics(results["companion"]),
                    expected,
                )
                # This is the first read AFTER the failed/successful mutation;
                # the earlier assert_type reads remain refined in both stages.
                line = text.index(
                    "assert_type(first.key",
                    text.index(
                        "alias." + ("fail()" if stage == "initial" else "set_key()")
                    ),
                )
                offset = text.index("key", line)
                params = {
                    **document,
                    "position": position(text, text[offset:], encoding=encoding),
                }
                values = {
                    backend: client.request("textDocument/hover", params)
                    for backend, client in clients.items()
                }
                raw[stage + "/hover"] = values
                self.equal(
                    f"{kind}/{encoding}/{stage}/hover",
                    hover(values["companion"]),
                    hover(values["fork"]),
                )
                assert ("None" in json.dumps(values["fork"])) == (stage == "initial"), (
                    values
                )
            # A semantic change in another module must not discard cached
            # warnings belonging to the unchanged failing module.
            unrelated_uri = (root / "unrelated.py").as_uri()
            for client in clients.values():
                client.notify(
                    "textDocument/didOpen",
                    {
                        "textDocument": {
                            "uri": unrelated_uri,
                            "languageId": "python",
                            "version": 1,
                            "text": "value: int = 1\n",
                        }
                    },
                )
                client.notify(
                    "textDocument/didChange",
                    {
                        "textDocument": {"uri": unrelated_uri, "version": 2},
                        "contentChanges": [{"text": "value: int = 2\n"}],
                    },
                )
            results = {
                backend: client.request("textDocument/diagnostic", document)
                for backend, client in clients.items()
            }
            raw["unrelated-edit/diagnostics"] = results
            self.equal(
                f"{kind}/{encoding}/unrelated-edit/diagnostics",
                lsp_diagnostics(results["companion"]),
                lsp_diagnostics(raw["repaired/diagnostics"]["fork"]),
            )
            self.equal(
                f"{kind}/{encoding}/unrelated-edit/reference",
                lsp_diagnostics(results["fork"]),
                lsp_diagnostics(raw["repaired/diagnostics"]["fork"]),
            )
            if kind == "runtime-error":
                plugin_path = case / "plugin.py"
                original_plugin = plugin_path.read_text(encoding="utf-8")
                recovered = original_plugin.replace(
                    'def fail_state(request):\n    return {"kind": "not-a-real-response"}',
                    'def fail_state(request):\n    return call_state_patch(receiver_members={"key": type_annotation("int")}, preserves_other_objects=True)',
                )
                assert recovered != original_plugin
                try:
                    for version, label, body, buffer, expected_warnings in (
                        (
                            3,
                            "recovered",
                            recovered,
                            SOURCE.replace(
                                "assert_type(first.key, int | None)",
                                "assert_type(first.key, int)",
                            )
                            .replace(
                                "assert_type(alias.key, int | None)",
                                "assert_type(alias.key, int)",
                            )
                            .replace(
                                "assert_type(second.key, int | None)",
                                "assert_type(second.key, int)",
                            ),
                            3,
                        ),
                        (4, "failed-again", original_plugin, SOURCE, 4),
                    ):
                        plugin_path.write_text(body, encoding="utf-8")
                        for client in clients.values():
                            client.notify(
                                "workspace/didChangeWatchedFiles",
                                {"changes": [{"uri": plugin_path.as_uri(), "type": 2}]},
                            )
                            client.notify(
                                "textDocument/didChange",
                                {
                                    "textDocument": {"uri": uri, "version": version},
                                    "contentChanges": [{"text": buffer}],
                                },
                            )
                        results = {
                            backend: client.request("textDocument/diagnostic", document)
                            for backend, client in clients.items()
                        }
                        raw["artifact-" + label] = results
                        expected = lsp_diagnostics(results["fork"])
                        assert (
                            sum(
                                d.get("code") == "plugin-configuration"
                                for d in expected
                            )
                            == expected_warnings
                        ), results
                        assert not any(
                            d.get("code") == "type-assertion-failure" for d in expected
                        ), results
                        self.equal(
                            f"{kind}/{encoding}/artifact-{label}",
                            lsp_diagnostics(results["companion"]),
                            expected,
                        )
                finally:
                    plugin_path.write_text(original_plugin, encoding="utf-8")
            self.equal(
                f"{kind}/{encoding}/unsaved-buffer-did-not-change-disk",
                source.read_text(encoding="utf-8"),
                SOURCE,
            )
        finally:
            save(out / "results.json", raw)
            for client in clients.values():
                client.close()

    def run(self):
        try:
            for kind in ("protocol-error", "runtime-error"):
                case, root = self.fixture(kind)
                self.cli(kind, case, root)
                for encoding in ("utf-8", "utf-16", "utf-32"):
                    self.editor(kind, case, root, encoding)
            for case in self.evidence["cases"].values():
                assert all(
                    digest(Path(path)) == expected
                    for path, expected in case["files"].items()
                )
            assert digest(self.args.companion) == self.evidence["sha256"]
            self.evidence["passed"] = True
        finally:
            self.evidence["check_count"] = len(self.evidence["checks"])
            save(self.output / "summary.json", self.evidence)
        print(
            json.dumps(
                {
                    "passed": True,
                    "checks": self.evidence["check_count"],
                    "output": str(self.output),
                }
            )
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("companion", "official", "fork", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    for name in ("companion", "official", "fork", "output"):
        setattr(args, name, getattr(args, name).resolve())
    Gate(args).run()


if __name__ == "__main__":
    main()
