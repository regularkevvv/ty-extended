#!/usr/bin/env python3
"""Exercise the real Rust editor wrapper and unchanged ty over framed stdio."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import queue
import subprocess
import threading
import time
from pathlib import Path


class Client:
    def __init__(self, command: list[str], root: Path, stderr: Path):
        self.log = stderr.open("wb")
        self.process = subprocess.Popen(
            command,
            cwd=root,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.log,
            env={**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"},
        )
        self.messages: queue.Queue = queue.Queue()
        self.notifications: list[dict] = []
        self.server_requests: list[dict] = []
        self.next_id = 1
        self.timings: list[dict] = []
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        try:
            while True:
                headers = {}
                while (line := self.process.stdout.readline()) not in (b"\r\n", b"\n"):
                    if not line:
                        raise EOFError("server closed stdout")
                    key, value = line.decode().split(":", 1)
                    headers[key.lower()] = value.strip()
                self.messages.put(
                    json.loads(self.process.stdout.read(int(headers["content-length"])))
                )
        except Exception as error:
            self.messages.put(error)

    def send(self, message: dict):
        encoded = json.dumps({"jsonrpc": "2.0", **message}).encode()
        self.process.stdin.write(
            f"Content-Length: {len(encoded)}\r\n\r\n".encode() + encoded
        )
        self.process.stdin.flush()

    def notify(self, method: str, params: dict | None = None):
        self.send({"method": method, "params": params or {}})

    def request(self, method: str, params: dict | None = None, *, retries: int = 2):
        started = time.perf_counter()
        ident = self.next_id
        self.next_id += 1
        self.send({"id": ident, "method": method, "params": params})
        while True:
            message = self.messages.get(timeout=120)
            if isinstance(message, Exception):
                raise message
            if "method" in message:
                if "id" in message:
                    self.server_requests.append(message)
                    result = None
                    if message["method"] == "workspace/configuration":
                        result = [None] * len(message["params"]["items"])
                    self.send({"id": message["id"], "result": result})
                else:
                    self.notifications.append(message)
                continue
            assert message["id"] == ident, message
            self.timings.append(
                {"method": method, "seconds": time.perf_counter() - started}
            )
            if "error" in message:
                if (
                    retries
                    and method == "textDocument/diagnostic"
                    and message["error"]["code"] in {-32801, -32802}
                ):
                    return self.request(method, params, retries=retries - 1)
                raise RuntimeError(f"{method}: {message['error']}")
            return message["result"]

    def close(self):
        try:
            self.request("shutdown")
            self.notify("exit")
            assert self.process.wait(timeout=10) == 0
        finally:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait()
            self.log.close()


SOURCE = """from typing import assert_type
class Resource:
    status: str | None = None
    def activate(self) -> None: pass
    def release(self) -> None: pass

def decode(value: object) -> object: return value

item = Resource()
item.activate()
item.label
item.status
wrong: str = decode("value")
local = 1
value = local
emoji = "😀"; value2 = local
"""


def position(
    text: str, needle: str, offset: int = 0, occurrence: int = 0, encoding="utf-16"
):
    start = -1
    for _ in range(occurrence + 1):
        start = text.index(needle, start + 1)
    start += offset
    before = text[:start]
    line = before.count("\n")
    column = before.rsplit("\n", 1)[-1]
    character = (
        len(column.encode("utf-16-le")) // 2
        if encoding == "utf-16"
        else len(column.encode())
        if encoding == "utf-8"
        else len(column)
    )
    return {"line": line, "character": character}


def gate(args, encoding: str, pull: bool):
    case = args.output / f"{encoding}-{'pull' if pull else 'push'}"
    root = case / "project"
    stubs = case / "stubs"
    root.mkdir(parents=True)
    stubs.mkdir()
    source = root / "demo.py"
    source.write_text(SOURCE, encoding="utf-8")
    original_hash = hashlib.sha256(source.read_bytes()).hexdigest()
    uri = source.as_uri()
    fixture = Path(__file__).resolve().parents[1] / "tests/fixtures/generic"
    client = Client(
        [
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
            "--python-version",
            "3.13",
            "--out",
            str(case / "analysis"),
        ],
        root,
        case / "stderr.log",
    )
    evidence = {}
    try:
        caps = {
            "general": {"positionEncodings": [encoding]},
            "workspace": {
                name: {"refreshSupport": True}
                for name in ("diagnostics", "semanticTokens", "inlayHint")
            },
            "textDocument": {
                "hover": {"contentFormat": ["markdown"]},
                "completion": {"completionItem": {"snippetSupport": True}},
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                "definition": {"linkSupport": True},
                "semanticTokens": {
                    "requests": {"full": True},
                    "tokenTypes": [],
                    "tokenModifiers": [],
                    "formats": ["relative"],
                },
            },
        }
        if pull:
            caps["textDocument"]["diagnostic"] = {}
        initialized = client.request(
            "initialize", {"rootUri": root.as_uri(), "capabilities": caps}
        )
        assert initialized["capabilities"]["positionEncoding"] == encoding
        evidence["capabilities"] = initialized["capabilities"]
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
        document = {"textDocument": {"uri": uri}}
        diagnostic = client.request("textDocument/diagnostic", document)
        errors = [d for d in diagnostic["items"] if d.get("severity", 1) == 1]
        assert len(errors) == 1 and errors[0]["range"]["start"]["line"] == 12, (
            diagnostic
        )
        evidence["initial_diagnostics"] = diagnostic
        unchanged = client.request(
            "textDocument/diagnostic",
            {**document, "previousResultId": diagnostic["resultId"]},
        )
        assert unchanged["kind"] == "unchanged"

        for name, needle, offset in (
            ("injected", "item.label", 6),
            ("state", "item.status", 6),
            ("unicode", "value2 = local", 10),
        ):
            hover = client.request(
                "textDocument/hover",
                {
                    **document,
                    "position": position(SOURCE, needle, offset, encoding=encoding),
                },
            )
            assert hover and "_ty_ext_" not in json.dumps(hover), hover
            if name != "unicode":
                assert "str" in json.dumps(hover), hover
            if name == "state":
                assert "None" not in json.dumps(hover), hover
            evidence[f"hover_{name}"] = hover

        cursor = position(SOURCE, "value = local", 10, encoding=encoding)
        location = client.request(
            "textDocument/definition", {**document, "position": cursor}
        )
        assert (
            location
            and source.as_uri() in json.dumps(location)
            and "/analysis/" not in json.dumps(location)
        ), location
        evidence["definition"] = location
        references = client.request(
            "textDocument/references",
            {**document, "position": cursor, "context": {"includeDeclaration": True}},
        )
        assert len(references) == 3 and all(r["uri"] == uri for r in references), (
            references
        )
        rename = client.request(
            "textDocument/rename",
            {**document, "position": cursor, "newName": "renamed"},
        )
        assert "renamed" in json.dumps(rename) and "/analysis/" not in json.dumps(
            rename
        ), rename
        evidence["rename"] = rename
        symbols = client.request("textDocument/documentSymbol", document)
        assert symbols and "_ty_ext_" not in json.dumps(symbols), symbols
        evidence["symbols"] = symbols
        tokens = client.request("textDocument/semanticTokens/full", document)
        assert tokens and len(tokens["data"]) % 5 == 0, tokens
        evidence["tokens"] = tokens

        fixed = SOURCE.replace("wrong: str", "wrong: int")
        client.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": uri, "version": 2},
                "contentChanges": [
                    {
                        "range": {
                            "start": {"line": 12, "character": 7},
                            "end": {"line": 12, "character": 10},
                        },
                        "text": "int",
                    }
                ],
            },
        )
        diagnostic = client.request("textDocument/diagnostic", document)
        assert not [d for d in diagnostic["items"] if d.get("severity", 1) == 1], (
            diagnostic
        )
        evidence["edited_diagnostics"] = diagnostic

        incomplete = fixed + "item.\n"
        client.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": uri, "version": 3},
                "contentChanges": [{"text": incomplete}],
            },
        )
        completions = client.request(
            "textDocument/completion",
            {
                **document,
                "position": {"line": 16, "character": 5},
                "context": {"triggerKind": 2, "triggerCharacter": "."},
            },
        )
        items = (
            completions.get("items", [])
            if isinstance(completions, dict)
            else completions or []
        )
        assert any(item["label"] == "label" for item in items), completions
        assert not any("_ty_ext_" in item["label"] for item in items), completions
        evidence["completion_labels"] = sorted(item["label"] for item in items)

        client.notify(
            "textDocument/didChange",
            {
                "textDocument": {"uri": uri, "version": 4},
                "contentChanges": [{"text": fixed}],
            },
        )
        assert not [
            d
            for d in client.request("textDocument/diagnostic", document)["items"]
            if d.get("severity", 1) == 1
        ]
        client.notify("textDocument/didClose", document)
        reverted = client.request("textDocument/diagnostic", document)
        assert len([d for d in reverted["items"] if d.get("severity", 1) == 1]) == 1, (
            reverted
        )
        assert hashlib.sha256(source.read_bytes()).hexdigest() == original_hash
        evidence["statistics"] = client.request("tyCompanion/statistics")
        refreshes = [
            message
            for message in client.server_requests
            if message["method"].endswith("/refresh")
        ]
        expected_refreshes = {
            "workspace/semanticTokens/refresh",
            "workspace/inlayHint/refresh",
        }
        if pull:
            expected_refreshes.add("workspace/diagnostic/refresh")
        assert {message["method"] for message in refreshes} == expected_refreshes
        assert len({message["id"] for message in refreshes}) == len(refreshes)
        evidence["refresh_requests"] = refreshes
        if not pull:
            publications = [
                m["params"]
                for m in client.notifications
                if m["method"] == "textDocument/publishDiagnostics"
            ]
            assert any(
                p.get("version") == 1 and p["diagnostics"] for p in publications
            ), publications
            assert any(
                p.get("version") == 2 and not p["diagnostics"] for p in publications
            ), publications
            assert any(
                "version" not in p and not p["diagnostics"] for p in publications
            ), publications
            evidence["push_publications"] = publications
        evidence["timings"] = client.timings
        evidence["passed"] = True
        return evidence
    finally:
        client.close()
        (case / "evidence.json").write_text(
            json.dumps(evidence, indent=2) + "\n", encoding="utf-8"
        )


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--companion", type=Path, required=True)
    parser.add_argument("--official", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    results = []
    for encoding in ("utf-16", "utf-8", "utf-32"):
        for pull in (True, False):
            results.append(gate(args, encoding, pull))
    (args.output / "summary.json").write_text(
        json.dumps(
            {"passed": True, "sessions": len(results), "results": results}, indent=2
        )
        + "\n",
        encoding="utf-8",
    )
    print(f"Passed {len(results)} real editor sessions")


if __name__ == "__main__":
    main()
