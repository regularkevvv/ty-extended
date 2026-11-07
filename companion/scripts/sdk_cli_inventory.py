#!/usr/bin/env python3
"""Run original CLI plugin fixtures with equivalent Monty hook responses."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
from pathlib import Path

from plugin_configuration_differential import diagnostics

PLUGIN = """
@on_class_transform
def transform(request):
    if not any(base["expression"] == "toy.Model" for base in request["class"].get("bases", [])):
        return None
    fields = []
    parameters = []
    for item in request["class"].get("fields", []):
        annotation = item.get("annotation")
        if annotation is None:
            continue
        argument = keyword_only(item["name"], annotation, required=not item.get("has-default", False))
        parameters.append(argument)
        fields.append(field(item["name"], annotation, instance_set_type=annotation, constructor_parameter=argument, has_default=item.get("has-default", False)))
    return class_patch(fields=fields, constructor=callable_signature(parameters, type_expr("Self")))

@on_class_member
def class_member(request):
    return member_response(member(request["member-name"], type_annotation("str")))

@on_instance_member
def instance_member(request):
    return member_response(member(request["member-name"], type_annotation("str")))

@on_call_return
def result(request):
    return call_return_patch(type_annotation("int"))

@on_call_signature
def signature(request):
    return call_signature_patch(callable_signature([parameter("value", type=type_annotation("int"), required=True)], type_annotation("toy.Widget")))
"""

CASES = {
    "config_file_plugin_stub_overlay_affects_type_checking": ["invalid-assignment"],
    "config_file_mock_plugin_class_transform_affects_type_checking": [
        "invalid-argument-type",
        "invalid-assignment",
    ],
    "config_file_mock_plugin_member_hooks_affect_type_checking": ["invalid-assignment"]
    * 3,
    "config_file_mock_plugin_call_hooks_affect_type_checking": [
        "invalid-assignment",
        "invalid-argument-type",
        "missing-argument",
    ],
}


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


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
        / "ruff/crates/ty/tests/cli/config_option.rs"
    )
    raw = original.read_text(encoding="utf-8")
    environment = {**os.environ, "TY_UV": "0", "TY_MAX_PARALLELISM": "1"}
    report = {
        "passed": False,
        "original_source": str(original),
        "original_source_sha256": digest(original),
        "adapter": "Original CLI Python source and manifests; mock runtime replaced by equivalent Monty hook responses.",
        "binaries": {
            name: {
                "path": str(getattr(args, name)),
                "sha256": digest(getattr(args, name)),
            }
            for name in ("companion", "official", "fork")
        },
        "cases": [],
    }
    for name, expected_codes in CASES.items():
        block = raw.split("fn " + name + "()", 1)[1].split("assert_cmd_snapshot!", 1)[0]
        files = {}
        for match in re.finditer(
            r'\(\s*"([^"\n]+)",\s*(r#"(.*?)"#|"(?:\\.|[^"\\])*")\s*,?\s*\)', block, re.S
        ):
            files[match[1]] = (
                match[3] if match[2].startswith('r#"') else json.loads(match[2])
            )
        assert "test.py" in files, name
        root = args.output / name
        root.mkdir()
        for path, text in files.items():
            if path.endswith((".py", ".pyi")):
                destination = root / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(text, encoding="utf-8")
        config = '[environment]\npython-version="3.13"\n'
        if "stub_overlay" in name:
            site = (
                root
                / ".venv"
                / (
                    "Lib/site-packages"
                    if os.name == "nt"
                    else "lib/python3.13/site-packages"
                )
                / "foo"
            )
            site.mkdir(parents=True)
            (site / "__init__.py").write_text(
                "def make():\n    return 1\n", encoding="utf-8"
            )
            (root / ".venv/pyvenv.cfg").write_text(
                "home = ./\nversion_info = 3.13.0\n", encoding="utf-8"
            )
            config += 'python=".venv"\n'
        config += "[plugins]\nenabled=true\nauto-discover=false\n"
        for path, text in files.items():
            if not path.endswith(".plugin.json"):
                continue
            manifest = json.loads(text)
            manifest["protocol-version"] = {"major": 0, "minor": 7}
            manifest["runtime"] = {"kind": "monty", "artifact": manifest["id"] + ".py"}
            manifest_path = root / path
            manifest_path.parent.mkdir(parents=True, exist_ok=True)
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            artifact = manifest_path.parent / (manifest["id"] + ".py")
            artifact.write_text(
                "set_manifest(" + repr(manifest) + ")\n" + PLUGIN, encoding="utf-8"
            )
            config += (
                "\n[[plugins.plugin]]\nid="
                + json.dumps(manifest["id"])
                + '\nruntime="monty"\ntrusted=true\npath='
                + json.dumps(artifact.relative_to(root).as_posix())
                + "\nmanifest-path="
                + json.dumps(path)
                + "\n"
            )
            if manifest["capabilities"].get("stub-overlays"):
                config += 'stub-overlay-path=".ty/plugins/stubs"\n'
        (root / "ty.toml").write_text(config, encoding="utf-8")
        row = {
            "name": name,
            "source_sha256": {
                path: digest(root / path)
                for path in files
                if path.endswith((".py", ".pyi"))
            },
            "runs": [],
        }
        outputs = {}
        for backend, binary in (("fork", args.fork), ("companion", args.companion)):
            for format_ in ("gitlab", "full"):
                command = [
                    str(binary),
                    "check",
                    "test.py",
                    "--project",
                    ".",
                    "--output-format",
                    format_,
                ]
                if backend == "companion":
                    command += ["--ty", str(args.official)]
                process = subprocess.run(
                    command, cwd=root, env=environment, capture_output=True
                )
                (root / f"{backend}-{format_}.stdout").write_bytes(process.stdout)
                (root / f"{backend}-{format_}.stderr").write_bytes(process.stderr)
                value = (
                    diagnostics(json.loads(process.stdout), root)
                    if format_ == "gitlab"
                    else process.stdout.decode()
                )
                outputs[(backend, format_)] = value
                row["runs"].append(
                    {
                        "backend": backend,
                        "format": format_,
                        "exit": process.returncode,
                        "passed": process.returncode == 1,
                    }
                )
        expected = outputs[("fork", "gitlab")]
        # The companion renders its own text layout; require the native
        # diagnostic contract and every original primary source location.
        full_locations = []
        for backend in ("fork", "companion"):
            text = outputs[(backend, "full")]
            full_locations.append(
                all(
                    f"--> {value['location']['path']}:{value['location']['positions']['begin']['line']}:{value['location']['positions']['begin']['column']}"
                    in text
                    for value in expected
                )
                and f"Found {len(expected)} diagnostic" in text
            )
        row["passed"] = (
            all(run["passed"] for run in row["runs"])
            and sorted(value["check_name"] for value in expected)
            == sorted(expected_codes)
            and expected == outputs[("companion", "gitlab")]
            and all(full_locations)
        )
        report["cases"].append(row)
        (args.output / "summary.json").write_text(
            json.dumps(report, indent=2) + "\n", encoding="utf-8"
        )
    report["passed"] = all(row["passed"] for row in report["cases"])
    (args.output / "summary.json").write_text(
        json.dumps(report, indent=2) + "\n", encoding="utf-8"
    )
    assert report["passed"], report["cases"]
    print(json.dumps({"passed": True, "contracts": len(report["cases"])}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
