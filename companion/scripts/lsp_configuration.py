#!/usr/bin/env python3
"""Compare real client-only editor settings with unmodified official ty.

Initial settings use the same workspace/configuration responses for both servers.
Live changes are compared with a freshly initialized official server: the pinned
upstream server does not implement workspace/didChangeConfiguration updates.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
import queue
import subprocess
import sys
import time
from pathlib import Path

from lsp_acceptance import Client, position
from lsp_differential import canonical, hover

SOURCE = """from selected_environment import value
from typing import TypeIs

answer: int = value
bad: int = "wrong"

def choose(first: int) -> int:
    return first

inferred = choose(7)
"""
PLUGIN_SOURCE = """def target() -> object:
    return object()

value: int = target()
"""


def save(path, value):
    path.write_text(
        json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


class RpcError(RuntimeError):
    def __init__(self, method, error):
        self.code = error["code"]
        self.error = error
        super().__init__(f"{method}: {error}")


class InitializationExit(RuntimeError):
    def __init__(self, returncode, stderr):
        self.returncode = returncode
        self.stderr = stderr
        super().__init__(f"server exited during initialize ({returncode}): {stderr}")


class SettingsClient(Client):
    def __init__(self, command, root, stderr, settings):
        self.settings = settings
        self.client_initialized = False
        self.configuration = []
        self.held = []
        self.hold = False
        self.responses = {}
        self.inflight = {}
        super().__init__(
            command, root, stderr, server_request_handler=self._configuration
        )

    def _configuration(self, client, message):
        if message["method"] != "workspace/configuration":
            return False
        assert self.client_initialized, "configuration requested before initialized"
        items = message["params"]["items"]
        values = []
        for item in items:
            assert item["section"] == "ty", item
            scope = item["scopeUri"]
            assert scope in self.settings, (
                f"configuration scope is not original: {item}"
            )
            values.append(copy.deepcopy(self.settings[scope]))
        record = {"request": message, "settings": values}
        self.configuration.append(record)
        if self.hold:
            self.held.append(record)
        else:
            client.send({"id": message["id"], "result": values})
        return True

    def notify(self, method, params=None):
        if method == "initialized":
            self.client_initialized = True
        if method == "exit":
            self.send({"method": method, "params": None})
        else:
            super().notify(method, params)

    def begin(self, method, params=None):
        ident = self.next_id
        self.next_id += 1
        self.inflight[ident] = (method, time.perf_counter())
        self.send({"id": ident, "method": method, "params": params})
        return ident

    def pump(self, timeout=120):
        message = self.messages.get(timeout=timeout)
        if isinstance(message, Exception):
            raise message
        if "method" in message:
            if "id" in message:
                self._handle_server_request(message)
            else:
                self.notifications.append(message)
        else:
            assert message["id"] in self.inflight, message
            self.responses[message["id"]] = message

    def until(self, predicate, timeout=120):
        deadline = time.monotonic() + timeout
        while not predicate():
            self.pump(max(0.001, deadline - time.monotonic()))
            if time.monotonic() >= deadline and not predicate():
                raise TimeoutError("waiting for configuration protocol event")

    def finish(self, ident):
        self.until(lambda: ident in self.responses)
        message = self.responses.pop(ident)
        method, started = self.inflight.pop(ident)
        self.timings.append(
            {"method": method, "seconds": time.perf_counter() - started}
        )
        if "error" in message:
            raise RpcError(method, message["error"])
        return message["result"]

    def request(self, method, params=None, *, retries=2):
        try:
            return self.finish(self.begin(method, params))
        except RpcError as error:
            if (
                retries
                and method == "textDocument/diagnostic"
                and error.code in {-32801, -32802}
            ):
                return self.request(method, params, retries=retries - 1)
            raise

    def release(self, record, values=None):
        self.send(
            {
                "id": record["request"]["id"],
                "result": record["settings"] if values is None else values,
            }
        )
        self.held.remove(record)

    def assert_blocked(self, ident):
        deadline = time.monotonic() + 0.2
        while time.monotonic() < deadline:
            try:
                self.pump(max(0.001, deadline - time.monotonic()))
            except queue.Empty:
                break
        assert ident not in self.responses, (
            "request used defaults before client settings arrived"
        )

    def close(self):
        self.hold = False
        for record in self.held[:]:
            self.release(record)
        super().close()


def capabilities(configuration=True):
    return {
        "general": {"positionEncodings": ["utf-16"]},
        "workspace": {
            "workspaceFolders": True,
            "configuration": configuration,
            "didChangeConfiguration": {"dynamicRegistration": configuration},
            "diagnostics": {"refreshSupport": True},
            "inlayHint": {"refreshSupport": True},
        },
        "textDocument": {
            "diagnostic": {},
            "hover": {"contentFormat": ["markdown"]},
            "inlayHint": {},
            "publishDiagnostics": {"relatedInformation": True},
        },
    }


def settings(environment, minor, *, ignore=False, disabled=False, hints=True):
    return {
        "configuration": {
            "rules": {"invalid-assignment": "ignore" if ignore else "error"},
        },
        "pythonExtension": {
            "activeEnvironment": {
                "executable": {"sysPrefix": str(environment)},
                "version": {"major": 3, "minor": minor},
            }
        },
        "inlayHints": {"variableTypes": hints, "callArgumentNames": hints},
        "disableLanguageServices": disabled,
    }


def diagnostics(result):
    return sorted(
        [
            {
                key: item[key]
                for key in ("range", "message", "severity", "code", "tags")
                if key in item
            }
            for item in result["items"]
        ],
        key=lambda item: json.dumps(item, sort_keys=True),
    )


def inlays(result):
    # Document-version/data tokens are opaque. Positions, labels and edits must
    # be identical; label-part location links retain their original URI.
    return canonical(result) if result else []


class Gate:
    def __init__(self, args):
        self.args = args
        self.output = args.output
        self.output.mkdir(parents=True)
        self.roots = []
        self.settings = {}
        self.originals = {}
        self.count = 0
        self.evidence = {
            "passed": False,
            "artifact": str(args.companion),
            "sha256": digest(args.companion),
            "official": {"path": str(args.official), "sha256": digest(args.official)},
            "official_version": subprocess.check_output(
                [str(args.official), "--version"], encoding="utf-8"
            ).strip(),
            "source_sha256": {
                name: digest(Path(__file__).with_name(name))
                for name in (
                    "lsp_configuration.py",
                    "lsp_acceptance.py",
                    "lsp_differential.py",
                )
            },
            "checks": [],
            "raw": {},
            "configuration_exchanges": {},
            "live_update_basis": "updated companion versus fresh official server with the same effective settings; upstream ignores didChangeConfiguration",
        }
        for index, annotation in enumerate(("int", "str", "int")):
            root = self.output / f"workspace {index} % café"
            root.mkdir()
            (root / "pyproject.toml").write_text(
                f'[project]\nname="lsp-settings-{index}"\nversion="0.0.0"\n',
                encoding="utf-8",
            )
            (root / "main.py").write_text(SOURCE, encoding="utf-8")
            environment = self.output / f"environment-{index}"
            environment.mkdir()
            (environment / "pyvenv.cfg").write_text(
                f"home = {sys.base_prefix}\ninclude-system-site-packages = false\nversion = 3.13.0\n",
                encoding="utf-8",
            )
            for relative in (
                "lib/python3.12/site-packages",
                "lib/python3.13/site-packages",
                "Lib/site-packages",
            ):
                packages = environment / relative
                packages.mkdir(parents=True)
                (packages / "selected_environment.py").write_text(
                    f"value: {annotation} = {1 if annotation == 'int' else repr('dependency')}\n",
                    encoding="utf-8",
                )
            self.roots.append(root)
            self.settings[root.as_uri()] = settings(
                environment,
                13 if index != 1 else 12,
                ignore=index == 0,
                disabled=index == 1,
                hints=index != 1,
            )
        for root in self.roots:
            for path in root.iterdir():
                self.originals[path] = digest(path)

    def start(
        self,
        backend,
        roots,
        *,
        options=None,
        configuration=True,
        hold=False,
        extra=None,
        mappings=None,
    ):
        self.count += 1
        case = self.output / f"session-{self.count}-{backend}"
        case.mkdir()
        if backend == "official":
            command = [str(self.args.official), "server"]
        else:
            command = [
                str(self.args.companion),
                "server",
                "--ty",
                str(self.args.official),
                "--out",
                str(case / "analysis"),
            ]
            command += extra or []
        client = SettingsClient(
            command, self.output, case / "stderr.log", mappings or self.settings
        )
        client.hold = hold
        try:
            client.request(
                "initialize",
                {
                    "workspaceFolders": [
                        {"uri": root.as_uri(), "name": root.name} for root in roots
                    ],
                    "capabilities": capabilities(configuration),
                    "initializationOptions": options or {},
                },
            )
            client.notify("initialized")
        except EOFError as error:
            # The native server rejects malformed initialization by exiting.
            # Its asynchronous transport may close before flushing the optional
            # JSON-RPC error frame; retain the actual process result as evidence.
            try:
                returncode = client.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                client.process.kill()
                client.process.wait()
                raise
            finally:
                client.log.close()
            raise InitializationExit(
                returncode, (case / "stderr.log").read_text(encoding="utf-8")
            ) from error
        except BaseException:
            if client.process.poll() is None:
                client.process.kill()
                client.process.wait()
            client.log.close()
            raise
        self.evidence["configuration_exchanges"][case.name] = client.configuration
        return client

    @staticmethod
    def open(client, root, text=SOURCE, name="main.py"):
        uri = (root / name).as_uri()
        client.notify(
            "textDocument/didOpen",
            {
                "textDocument": {
                    "uri": uri,
                    "languageId": "python",
                    "version": 1,
                    "text": text,
                }
            },
        )
        return {"textDocument": {"uri": uri}}

    def compare(
        self, name, method, parameters, companion, official, transform=canonical
    ):
        raw = {
            "companion": companion.request(method, parameters),
            "official": official.request(method, parameters),
        }
        values = {backend: transform(value) for backend, value in raw.items()}
        self.evidence["raw"][name] = raw
        equal = values["companion"] == values["official"]
        self.evidence["checks"].append({"name": name, "method": method, "equal": equal})
        assert equal, f"{name}: {json.dumps(values, ensure_ascii=False)}"
        return raw["companion"]

    def inspect(self, label, root, companion, official, text=SOURCE):
        document = {"textDocument": {"uri": (root / "main.py").as_uri()}}
        report = self.compare(
            label + "/diagnostics",
            "textDocument/diagnostic",
            document,
            companion,
            official,
            diagnostics,
        )
        hovered = self.compare(
            label + "/hover",
            "textDocument/hover",
            {**document, "position": position(text, "choose(7)", 2)},
            companion,
            official,
            hover,
        )
        hints = self.compare(
            label + "/inlay",
            "textDocument/inlayHint",
            {
                **document,
                "range": {
                    "start": {"line": 0, "character": 0},
                    "end": {"line": len(text.splitlines()), "character": 0},
                },
            },
            companion,
            official,
            inlays,
        )
        return report, hovered, hints

    def fresh_reference(self, roots, texts=None):
        client = self.start("official", roots)
        for root in roots:
            self.open(client, root, (texts or {}).get(root, SOURCE))
        return client

    def regular(self):
        roots = self.roots[:2]
        companion = self.start("companion", roots)
        official = self.fresh_reference(roots)
        try:
            for root in roots:
                self.open(companion, root)
            first = self.inspect("initial/first", roots[0], companion, official)
            second = self.inspect("initial/second", roots[1], companion, official)
            assert not [d for d in first[0]["items"] if "code" in d], first
            assert first[1] and first[2], first
            second_codes = [d.get("code") for d in second[0]["items"]]
            assert second_codes.count("invalid-assignment") == 2, second
            assert any(
                d.get("range", {}).get("start", {}).get("line") == 1 and d.get("code")
                for d in second[0]["items"]
            ), second
            assert second[1] is None and not second[2], second
            assert companion.configuration and official.configuration
            registered = [
                r
                for message in companion.server_requests
                if message["method"] == "client/registerCapability"
                for r in message["params"]["registrations"]
            ]
            assert any(
                r["method"] == "workspace/didChangeConfiguration" for r in registered
            ), registered
            pids = companion.request("tyCompanion/statistics")["checker_pids"]
            companion.notify("workspace/didChangeConfiguration", {"settings": {}})
            assert companion.request("tyCompanion/statistics")["checker_pids"] == pids
            self.evidence["checks"].append(
                {"name": "unchanged-settings-preserve-checker-processes", "equal": True}
            )
            # Preserve a live buffer while rebuilding settings/backend state.
            changed = SOURCE.replace('bad: int = "wrong"', "bad: int = 42")
            companion.notify(
                "textDocument/didChange",
                {
                    "textDocument": {
                        "uri": (roots[0] / "main.py").as_uri(),
                        "version": 2,
                    },
                    "contentChanges": [{"text": changed}],
                },
            )
            self.settings[roots[0].as_uri()] = settings(
                self.output / "environment-1", 12, hints=False
            )
            self.settings[roots[1].as_uri()] = settings(
                self.output / "environment-0", 13
            )
            companion.notify("workspace/didChangeConfiguration", {"settings": {}})
            official.close()
            official = self.fresh_reference(roots, {roots[0]: changed})
            first = self.inspect(
                "changed/first-unsaved", roots[0], companion, official, changed
            )
            second = self.inspect(
                "changed/second-enabled", roots[1], companion, official
            )
            assert [d.get("code") for d in first[0]["items"]].count(
                "invalid-assignment"
            ) == 1, first
            assert first[1] and not first[2], first
            assert second[1] and second[2], second
            assert [d.get("code") for d in second[0]["items"]].count(
                "invalid-assignment"
            ) == 1, second
            third = self.roots[2]
            companion.notify(
                "workspace/didChangeWorkspaceFolders",
                {
                    "event": {
                        "added": [{"uri": third.as_uri(), "name": third.name}],
                        "removed": [],
                    }
                },
            )
            self.open(companion, third)
            official.close()
            official = self.fresh_reference([*roots, third], {roots[0]: changed})
            self.inspect("added/third", third, companion, official)
            companion.notify(
                "workspace/didChangeWorkspaceFolders",
                {
                    "event": {
                        "added": [],
                        "removed": [{"uri": roots[1].as_uri(), "name": roots[1].name}],
                    }
                },
            )
            assert companion.request("tyCompanion/statistics")["workspaces"] == 2
            self.inspect(
                "removed/first-preserved", roots[0], companion, official, changed
            )
        finally:
            companion.close()
            official.close()

    def fallback(self):
        root = self.roots[0]
        options = {
            "configuration": {
                "environment": {
                    "python-version": "3.12",
                    "extra-paths": [
                        str(self.output / "environment-0/Lib/site-packages")
                    ],
                },
                "rules": {"invalid-assignment": "ignore"},
            },
            "inlayHints": {"variableTypes": False, "callArgumentNames": False},
        }
        companion = self.start(
            "companion", [root], configuration=False, options=options
        )
        official = self.start("official", [root], configuration=False, options=options)
        try:
            for client in (companion, official):
                self.open(client, root)
            result = self.inspect(
                "no-capability/initialization-options", root, companion, official
            )
            assert not companion.configuration and not official.configuration
            assert not result[2] and result[1]
            assert not any(
                d.get("code") == "invalid-assignment" for d in result[0]["items"]
            )
            assert any(d.get("code") for d in result[0]["items"]), result
        finally:
            companion.close()
            official.close()

    def reordered(self):
        root = self.roots[0]
        self.settings[root.as_uri()] = settings(
            self.output / "environment-0", 13, ignore=True
        )
        companion = self.start("companion", [root], hold=True)
        official = None
        try:
            document = self.open(companion, root)
            request = companion.begin("textDocument/diagnostic", document)
            companion.until(lambda: len(companion.held) == 1)
            obsolete = companion.held[0]
            companion.assert_blocked(request)
            cancelled = companion.begin(
                "textDocument/hover",
                {**document, "position": position(SOURCE, "choose(7)", 2)},
            )
            companion.notify("$/cancelRequest", {"id": cancelled})
            try:
                companion.finish(cancelled)
            except RpcError as error:
                assert error.code == -32800, error
            else:
                raise AssertionError("deferred request ignored client cancellation")
            assert not any(
                message["method"] == "textDocument/publishDiagnostics"
                and message["params"]["diagnostics"]
                for message in companion.notifications
            ), companion.notifications
            self.evidence["checks"].append(
                {"name": "deferred-request-cancellation", "equal": True}
            )
            self.settings[root.as_uri()] = settings(self.output / "environment-1", 12)
            companion.notify("workspace/didChangeConfiguration", {"settings": {}})
            companion.until(lambda: len(companion.held) == 2)
            current = companion.held[1]
            companion.release(current)
            companion.hold = False
            actual = companion.finish(request)
            official = self.fresh_reference([root])
            expected = official.request("textDocument/diagnostic", document)
            assert diagnostics(actual) == diagnostics(expected), (actual, expected)
            companion.release(obsolete)
            self.inspect("reordered/stale-reply-ignored", root, companion, official)
            self.evidence["checks"].append(
                {"name": "reordered/queued-request-used-latest-settings", "equal": True}
            )
            # A removed workspace's delayed settings must not resurrect it.
            companion.hold = True
            third = self.roots[2]
            companion.notify(
                "workspace/didChangeWorkspaceFolders",
                {
                    "event": {
                        "added": [{"uri": third.as_uri(), "name": third.name}],
                        "removed": [],
                    }
                },
            )
            companion.until(lambda: bool(companion.held))
            removed = companion.held[-1]
            companion.notify(
                "workspace/didChangeWorkspaceFolders",
                {
                    "event": {
                        "added": [],
                        "removed": [{"uri": third.as_uri(), "name": third.name}],
                    }
                },
            )
            companion.hold = False
            companion.release(removed)
            assert companion.request("tyCompanion/statistics")["workspaces"] == 1
            self.inspect("reordered/removed-workspace-reply", root, companion, official)
        finally:
            companion.close()
            if official:
                official.close()

    def original_scope(self):
        if os.name == "nt":
            self.evidence["symlink_scope"] = (
                "Unix-only symlink identity check; percent-encoded original scopes are checked on every platform"
            )
            return
        root = self.roots[0]
        alias = self.output / "workspace alias % café"
        alias.symlink_to(root, target_is_directory=True)
        mappings = {
            alias.as_uri(): settings(self.output / "environment-0", 13, ignore=True)
        }
        companion = self.start("companion", [alias], mappings=mappings)
        official = self.start("official", [alias], mappings=mappings)
        try:
            for client in (companion, official):
                self.open(client, alias)
            self.inspect("original-symlink-scope", alias, companion, official)
            assert {
                item["scopeUri"]
                for r in companion.configuration
                for item in r["request"]["params"]["items"]
            } == {alias.as_uri()}
            self.evidence["symlink_scope"] = "passed"
        finally:
            companion.close()
            official.close()

    def configuration_file(self):
        root = self.roots[0]
        config = root / "editor overrides.toml"
        config.write_text(
            '[environment]\npython-version="3.13"\n'
            'extra-paths=["../environment-0/Lib/site-packages"]\n'
            '[rules]\ninvalid-assignment="ignore"\n',
            encoding="utf-8",
        )
        self.originals[config] = digest(config)
        mappings = {
            root.as_uri(): {
                "configurationFile": config.name,
                "inlayHints": {"variableTypes": False, "callArgumentNames": False},
            }
        }
        companion = self.start("companion", [root], mappings=mappings)
        official = self.start("official", [root], mappings=mappings)
        try:
            for client in (companion, official):
                self.open(client, root)
            result = self.inspect(
                "client-configuration-file", root, companion, official
            )
            assert not any(d.get("code") for d in result[0]["items"]), result
            assert result[1] and not result[2], result
        finally:
            companion.close()
            official.close()

    def unavailable_settings(self):
        root = self.roots[0]
        mappings = {root.as_uri(): None}
        options = {
            "configuration": {
                "environment": {"python-version": "3.13"},
                "rules": {"invalid-assignment": "ignore"},
            }
        }
        companion = self.start("companion", [root], options=options, mappings=mappings)
        official = self.start("official", [root], options=options, mappings=mappings)
        try:
            text = 'value: int = "wrong"\n'
            for client in (companion, official):
                document = self.open(client, root, text)
            result = self.compare(
                "null-settings/initialization-fallback",
                "textDocument/diagnostic",
                document,
                companion,
                official,
                diagnostics,
            )
            assert not result["items"], result
            assert companion.configuration and official.configuration
        finally:
            companion.close()
            official.close()

    def malformed_settings(self):
        root = self.roots[0]
        text = 'value: int = "wrong"\n'
        malformed = {
            "configuration": {"rules": {"invalid-assignment": "ignore"}},
            "inlayHints": {"variableTypes": "no"},
        }
        for mode in ("workspace", "initialization"):
            options = (
                {**malformed, "untrustedWorkspace": True}
                if mode == "initialization"
                else {}
            )
            mappings = {root.as_uri(): malformed if mode == "workspace" else {}}
            companion = self.start(
                "companion", [root], options=options, mappings=mappings
            )
            official = self.start(
                "official", [root], options=options, mappings=mappings
            )
            try:
                for client in (companion, official):
                    document = self.open(client, root, text)
                result = self.compare(
                    f"malformed-{mode}/whole-object-defaults",
                    "textDocument/diagnostic",
                    document,
                    companion,
                    official,
                    diagnostics,
                )
                assert [d.get("code") for d in result["items"]] == [
                    "invalid-assignment"
                ], result
            finally:
                companion.close()
                official.close()
        errors = {}
        for backend in ("companion", "official"):
            try:
                client = self.start(
                    backend, [root], options={"untrustedWorkspace": "false"}
                )
            except RpcError as error:
                assert error.code == -32602, error.error
                errors[backend] = error.error
            except InitializationExit as error:
                assert backend == "official", (backend, error)
                assert error.returncode == 2, error
                assert "Invalid `untrustedWorkspace` setting" in error.stderr, error
                assert "expected a boolean" in error.stderr, error
                errors[backend] = {
                    "transport": "process-exit",
                    "exit": error.returncode,
                    "message": error.stderr,
                }
            else:
                client.close()
                raise AssertionError(f"{backend} accepted malformed workspace trust")
        self.evidence["raw"]["malformed-trust"] = errors
        assert errors["companion"]["code"] == -32602, errors
        assert (
            errors["official"].get("code") == -32602
            or errors["official"].get("transport") == "process-exit"
        ), errors
        assert all(
            "untrustedWorkspace" in error["message"] for error in errors.values()
        ), errors
        self.evidence["checks"].append(
            {"name": "malformed-trust/initialize-rejected", "equal": True}
        )

    def invalid_inline_configuration(self):
        root = self.roots[0]
        options = settings(self.output / "environment-0", 13, hints=False)
        options["configuration"]["rules"]["invalid-assignment"] = "not-a-rule-level"
        mappings = {root.as_uri(): options}
        companion = self.start("companion", [root], mappings=mappings)
        official = self.start("official", [root], mappings=mappings)
        try:
            for client in (companion, official):
                self.open(client, root)
            result = self.inspect(
                "invalid-inline/native-fallback-retains-other-client-fields",
                root,
                companion,
                official,
            )
            assert [d.get("code") for d in result[0]["items"]] == [
                "invalid-assignment"
            ], result
            assert result[1] and not result[2], result
            messages = {
                backend: [
                    message["params"]
                    for message in client.notifications
                    if message["method"] == "window/showMessage"
                ]
                for backend, client in (
                    ("companion", companion),
                    ("official", official),
                )
            }
            self.evidence["raw"]["invalid-inline/editor-messages"] = messages
            assert messages["companion"] and messages["official"], messages
            assert all(
                "not-a-rule-level" in json.dumps(items) for items in messages.values()
            ), messages
            assert "/analysis/" not in json.dumps(messages["companion"]).replace(
                "\\", "/"
            ), messages
            self.evidence["checks"].append(
                {
                    "name": "invalid-inline/native-error-forwarded-to-editor",
                    "equal": True,
                }
            )
        finally:
            companion.close()
            official.close()

    def unresolved_import_paths(self):
        root = self.roots[0]
        mappings = {root.as_uri(): settings(self.output / "environment-0", 13)}
        companion = self.start("companion", [root], mappings=mappings)
        official = self.start("official", [root], mappings=mappings)
        text = "import missing_configuration_probe\n"
        try:
            for client in (companion, official):
                document = self.open(client, root, text)
            raw = {
                backend: client.request("textDocument/diagnostic", document)
                for backend, client in (
                    ("companion", companion),
                    ("official", official),
                )
            }
            self.evidence["raw"]["unresolved-import-paths"] = raw
            errors = {
                backend: [
                    d for d in result["items"] if d.get("code") == "unresolved-import"
                ]
                for backend, result in raw.items()
            }
            assert all(len(items) == 1 for items in errors.values()), raw
            companion_error, official_error = (
                errors["companion"][0],
                errors["official"][0],
            )
            assert companion_error["range"] == official_error["range"], raw
            assert (
                companion_error["message"].split("\n", 1)[0]
                == official_error["message"].split("\n", 1)[0]
            ), raw
            shown = companion_error["message"].replace("\\", "/")
            expected_root = str(root).replace("\\", "/")
            expected_environment = str(self.output / "environment-0").replace("\\", "/")
            assert expected_root in shown and expected_environment in shown, raw
            assert "<plugin stub overlay>" in shown, raw
            assert "/analysis/" not in shown and "file:///analysis/" not in shown, raw
            self.evidence["checks"].append(
                {
                    "name": "unresolved-import/original-search-paths-and-honest-overlay",
                    "equal": True,
                }
            )
        finally:
            companion.close()
            official.close()

    def cli_unresolved_import_paths(self):
        root = self.output / "cli diagnostic paths"
        root.mkdir()
        source = root / "missing.py"
        source.write_text("import missing_configuration_probe\n", encoding="utf-8")
        config = root / "pyproject.toml"
        config.write_text(
            '[project]\nname="cli-search-paths"\nversion="0.0.0"\n', encoding="utf-8"
        )
        for path in (source, config):
            self.originals[path] = digest(path)
        raw = {}
        for backend, binary in (
            ("companion", self.args.companion),
            ("official", self.args.official),
        ):
            command = [str(binary), "check"]
            if backend == "companion":
                command += ["--ty", str(self.args.official)]
            command += [
                "--project",
                ".",
                "--python",
                str(self.output / "environment-0"),
                "--python-version",
                "3.13",
                "--output-format",
                "full",
                "missing.py",
            ]
            completed = subprocess.run(
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
            raw[backend] = {
                "command": command,
                "cwd": str(root),
                "status": completed.returncode,
                "stdout": completed.stdout,
                "stderr": completed.stderr,
            }
            assert (
                completed.returncode == 1 and "unresolved-import" in completed.stdout
            ), raw
        self.evidence["raw"]["cli-unresolved-import-paths"] = raw
        shown = raw["companion"]["stdout"].replace("\\", "/")
        assert str(root).replace("\\", "/") in shown, raw
        assert str(self.output / "environment-0").replace("\\", "/") in shown, raw
        assert (
            "<plugin stub overlay>" in shown and "ty-companion-check-" not in shown
        ), raw
        self.evidence["checks"].append(
            {
                "name": "cli-unresolved-import/original-search-paths-and-honest-overlay",
                "equal": True,
            }
        )

    def global_diagnostics(self):
        root = self.roots[0]
        mappings = {
            root.as_uri(): {
                "configuration": {"environment": {"python-version": "3.13"}},
                "diagnosticMode": "off",
            }
        }
        companion = self.start("companion", [root], mappings=mappings)
        official = self.start("official", [root], mappings=mappings)
        try:
            for client in (companion, official):
                document = self.open(client, root, 'value: int = "wrong"\n')
            result = self.compare(
                "global/diagnostics-off",
                "textDocument/diagnostic",
                document,
                companion,
                official,
                diagnostics,
            )
            assert not result["items"], result
            broken = "value = (\n"
            companion.notify(
                "textDocument/didChange",
                {
                    "textDocument": {**document["textDocument"], "version": 2},
                    "contentChanges": [{"text": broken}],
                },
            )
            for show in (False, True):
                mappings[root.as_uri()]["diagnosticMode"] = "openFilesOnly"
                mappings[root.as_uri()]["showSyntaxErrors"] = show
                companion.notify("workspace/didChangeConfiguration", {"settings": {}})
                official.close()
                official = self.start("official", [root], mappings=mappings)
                self.open(official, root, broken)
                result = self.compare(
                    f"global/show-syntax-errors-{show}",
                    "textDocument/diagnostic",
                    document,
                    companion,
                    official,
                    diagnostics,
                )
                assert bool(result["items"]) == show, result
        finally:
            companion.close()
            official.close()

    def plugin_context(self):
        case = self.output / "plugin-context"
        root = case / "project"
        root.mkdir(parents=True)
        (root / "pyproject.toml").write_text(
            '[project]\nname="plugin-context"\nversion="0.0.0"\n', encoding="utf-8"
        )
        source = root / "demo.py"
        source.write_text(PLUGIN_SOURCE, encoding="utf-8")
        self.originals[source] = digest(source)
        manifest = {
            "id": "research.client-settings",
            "name": "Client target context",
            "version": "0.0.1",
            "protocol-version": {"major": 0, "minor": 7},
            "ty-compatibility": {"requirement": ">=0.86.0,<0.87.0"},
            "runtime": {"kind": "monty", "artifact": "plugin.py"},
            "capabilities": {"call-return": True},
            "claims": {"functions": [{"qualified-name": "demo.target"}]},
        }
        save(case / "plugin.plugin.json", manifest)
        (case / "plugin.py").write_text(
            f"set_manifest({manifest!r})\n@on_call_return_of('demo.target')\ndef target(request):\n    return call_return_patch(type_annotation('int' if request['context']['python-version'] == '3.13' else 'str'))\n",
            encoding="utf-8",
        )
        mappings = {
            root.as_uri(): {
                "configuration": {"environment": {"python-version": "3.13"}}
            }
        }
        companion = self.start(
            "companion",
            [root],
            extra=[
                "--plugin",
                str(case / "plugin.py"),
                "--manifest",
                str(case / "plugin.plugin.json"),
            ],
            mappings=mappings,
        )
        try:
            document = self.open(companion, root, PLUGIN_SOURCE, "demo.py")
            initial = companion.request("textDocument/diagnostic", document)
            assert not any(d.get("code") for d in initial["items"]), initial
            mappings[root.as_uri()]["configuration"]["environment"][
                "python-version"
            ] = "3.12"
            companion.notify("workspace/didChangeConfiguration", {"settings": {}})
            changed = companion.request("textDocument/diagnostic", document)
            assert [d.get("code") for d in changed["items"]].count(
                "invalid-assignment"
            ) == 1, changed
            self.evidence["raw"]["monty-python-context"] = {
                "3.13": initial,
                "3.12": changed,
            }
            self.evidence["checks"].append(
                {"name": "monty-python-context-follows-client-target", "equal": True}
            )
        finally:
            companion.close()

    def run(self):
        try:
            self.regular()
            self.fallback()
            self.reordered()
            self.original_scope()
            self.configuration_file()
            self.unavailable_settings()
            self.malformed_settings()
            self.invalid_inline_configuration()
            self.unresolved_import_paths()
            self.cli_unresolved_import_paths()
            self.global_diagnostics()
            self.plugin_context()
            assert all(
                digest(path) == before for path, before in self.originals.items()
            )
            self.evidence["source_files_unchanged"] = True
            assert digest(self.args.companion) == self.evidence["sha256"]
            self.evidence["passed"] = True
        finally:
            save(self.output / "evidence.json", self.evidence)
            save(
                self.output / "summary.json",
                {
                    key: value
                    for key, value in {
                        **self.evidence,
                        "check_count": len(self.evidence["checks"]),
                        "session_count": self.count,
                        "details": str(self.output / "evidence.json"),
                        "initial_comparison_basis": "same original workspace URIs and identical workspace/configuration responses",
                    }.items()
                    if key not in {"raw", "configuration_exchanges"}
                },
            )
        print(
            json.dumps(
                {
                    "passed": True,
                    "checks": len(self.evidence["checks"]),
                    "output": str(self.output),
                }
            )
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--companion", type=Path, required=True)
    parser.add_argument("--official", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.companion = args.companion.resolve()
    args.official = args.official.resolve()
    args.output = args.output.resolve()
    Gate(args).run()


if __name__ == "__main__":
    main()
