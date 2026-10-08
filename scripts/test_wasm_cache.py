"""Verify installed-checker compiled WASM cache hits, invalidation and fallback."""

# /// script
# requires-python = ">=3.10"
# dependencies = []
# ///

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
from pathlib import Path

from ty import find_ty_bin
from ty.plugin_sdk import (
    capabilities,
    claims,
    class_claim_exact,
    class_patch,
    manifest,
    member,
    type_annotation,
)


def write_plugin(project: Path, value_type: str) -> None:
    response = json.dumps(
        class_patch(instance_members=[member("value", type_annotation(value_type))])
    ).encode()
    escaped = "".join(f"\\{byte:02x}" for byte in response)
    (project / "plugin.wat").write_text(
        '(module (memory (export "memory") 1)\n'
        f'  (data (i32.const 0) "{escaped}")\n'
        '  (func (export "ty_plugin_alloc") (param i32) (result i32) i32.const 4096)\n'
        '  (func (export "ty_plugin_handle") (param i32 i32) (result i64)\n'
        f"    i64.const {len(response)}))\n"
    )


def check(binary: str, project: Path, cache: Path, expected_type: str) -> str:
    result = subprocess.run(
        [binary, "check", "--color", "never", "--no-progress"],
        cwd=project,
        env={
            **os.environ,
            "XDG_CACHE_HOME": str(cache),
            "TY_LOG": "ty_plugin_host::wasm=debug",
        },
        capture_output=True,
        text=True,
        timeout=60,
    )
    output = result.stdout + result.stderr
    if result.returncode != 0 or f"`{expected_type}`" not in output:
        raise AssertionError(f"WASM plugin did not infer {expected_type}:\n{output}")
    return output


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ty-bin", help="Test a locally built checker")
    args = parser.parse_args()
    binary = args.ty_bin or find_ty_bin()
    with tempfile.TemporaryDirectory(prefix="ty-wasm-cache-") as directory:
        root = Path(directory)
        project = root / "project"
        project.mkdir()
        cache = root / "cache"
        (project / "app.py").write_text(
            "from typing import reveal_type\nclass Token: pass\nreveal_type(Token().value)\n"
        )
        (project / "plugin.json").write_text(
            json.dumps(
                manifest(
                    "example.cache",
                    "cache-fixture",
                    "0.0.0",
                    capabilities=capabilities(class_transform=True),
                    claims=claims(classes=[class_claim_exact("app.Token")]),
                    runtime={"kind": "wasm", "artifact": "plugin.wat"},
                )
            )
        )
        (project / "ty.toml").write_text(
            "[plugins]\nenabled = true\nauto-discover = false\n[[plugins.plugin]]\n"
            'id = "example.cache"\npath = "plugin.wat"\nruntime = "wasm"\n'
            'manifest-path = "plugin.json"\ntrusted = true\n'
        )
        write_plugin(project, "int")
        cold = check(binary, project, cache, "int")
        if "cache_misses=1" not in cold:
            raise AssertionError(f"No compilation miss on empty cache:\n{cold}")
        warm = check(binary, project, cache, "int")
        if "cache_hits=1" not in warm or "cache_misses=1" in warm:
            raise AssertionError(f"No compiled-code hit after restart:\n{warm}")
        write_plugin(project, "str")
        changed = check(binary, project, cache, "str")
        if "cache_misses=1" not in changed:
            raise AssertionError(
                f"Changed artifact did not invalidate cache:\n{changed}"
            )
        blocked = root / "blocked-cache"
        blocked.write_text("not a directory")
        fallback = check(binary, project, blocked, "str")
        if "cache_enabled=false" not in fallback:
            raise AssertionError(f"Unusable cache did not fall back:\n{fallback}")
    print("Installed checker: WASM cache miss, hit, invalidation and fallback passed")


if __name__ == "__main__":
    main()
