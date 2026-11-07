"""Exercise the installed companion, SDK, adjacent official ty, and editor server."""

from __future__ import annotations

import importlib.metadata
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from ty import find_ty_bin
from ty_extended import find_ty_extended_bin, plugin_sdk

ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    sys.path.insert(0, str(ROOT / "companion/scripts"))
    from lsp_acceptance import Client

    binary = Path(find_ty_extended_bin()).resolve()
    official = Path(find_ty_bin()).resolve()
    assert binary != official, (binary, official)
    version = importlib.metadata.version("ty-extended")
    assert plugin_sdk.PROTOCOL_VERSION == {"major": 0, "minor": 9}
    assert (
        subprocess.check_output([str(binary), "--version"], text=True).strip()
        == f"ty-extended {version}"
    )
    assert subprocess.check_output([str(official), "--version"], text=True).startswith(
        "ty 0.0."
    )
    subprocess.run(
        [sys.executable, "-m", "ty_extended", "--help"], check=True, capture_output=True
    )

    with tempfile.TemporaryDirectory(prefix="companion-wheel-") as directory:
        root = Path(directory)
        project = root / "project"
        shutil.copytree(ROOT / "examples/monty", project)
        # No official ty on PATH: isolated tools must find their adjacent dependency.
        env = {**os.environ, "PATH": str(root / "empty-path"), "TY_UV": "0"}
        env.pop("TY_COMPANION_TY", None)
        worker = root / "monty"
        receipt = root / "worker-started"
        worker.write_text(
            f"#!{sys.executable}\nopen({str(receipt)!r}, 'w').close()\nraise SystemExit(3)\n"
        )
        worker.chmod(0o700)
        env["TY_MONTY_BIN"] = str(worker)
        command = [str(binary), "check", "--color", "never"]
        result = subprocess.run(
            command, cwd=project, env=env, capture_output=True, text=True, timeout=60
        )
        output = result.stdout + result.stderr
        assert result.returncode == 0 and "Token" in output, output
        assert not receipt.exists(), "Embedded Monty attempted to execute a worker"
        source = project / "app.py"
        original = source.read_text()
        source.write_text(original + "\nbad: int = token\n")
        result = subprocess.run(
            command, cwd=project, env=env, capture_output=True, text=True, timeout=60
        )
        output = result.stdout + result.stderr
        assert result.returncode == 1 and "invalid-assignment" in output, output
        assert source.read_text() == original + "\nbad: int = token\n"
        source.write_text(original)
        client = Client(
            [str(binary), "server", str(project)], project, root / "server.stderr"
        )
        try:
            client.request(
                "initialize",
                {
                    "rootUri": project.as_uri(),
                    "capabilities": {
                        "textDocument": {
                            "diagnostic": {},
                            "hover": {"contentFormat": ["markdown"]},
                        }
                    },
                },
            )
            client.notify("initialized")
            uri = source.as_uri()
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": uri,
                        "languageId": "python",
                        "version": 1,
                        "text": original,
                    }
                },
            )
            diagnostic = client.request(
                "textDocument/diagnostic", {"textDocument": {"uri": uri}}
            )
            assert not [d for d in diagnostic["items"] if d.get("severity", 1) == 1], (
                diagnostic
            )
            line = next(
                i
                for i, text in enumerate(original.splitlines())
                if text.startswith("token =")
            )
            hover = client.request(
                "textDocument/hover",
                {
                    "textDocument": {"uri": uri},
                    "position": {"line": line, "character": 2},
                },
            )
            assert "Token" in json.dumps(hover), hover
        finally:
            client.close()
    print(
        "Installed companion: SDK, official ty coexistence, CLI controls, embedded Monty, and LSP passed"
    )


if __name__ == "__main__":
    main()
