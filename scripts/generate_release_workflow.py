"""Regenerate cargo-dist CI while retaining reviewed release customizations.

Run with the docs environment (PyYAML) and the cargo-dist version pinned in
dist-workspace.toml. Custom steps and gates live in release-customizations.json.
"""

from __future__ import annotations

import argparse
import copy
import json
import re
import subprocess
import tomllib
from pathlib import Path

import yaml

ROOT = Path(__file__).resolve().parents[1]


class WorkflowLoader(yaml.SafeLoader):
    pass


# GitHub uses YAML 1.2: the event name `on` is a string, not a boolean.
WorkflowLoader.yaml_implicit_resolvers = copy.deepcopy(
    yaml.SafeLoader.yaml_implicit_resolvers
)
for key, resolvers in WorkflowLoader.yaml_implicit_resolvers.items():
    WorkflowLoader.yaml_implicit_resolvers[key] = [
        (tag, pattern)
        for tag, pattern in resolvers
        if tag not in {"tag:yaml.org,2002:bool", "tag:yaml.org,2002:int"}
    ]
WorkflowLoader.add_implicit_resolver(
    "tag:yaml.org,2002:bool",
    re.compile(r"^(?:true|false|True|False|TRUE|FALSE)$"),
    list("tTfF"),
)
WorkflowLoader.add_implicit_resolver(
    "tag:yaml.org,2002:int",
    re.compile(r"^[-+]?(?:0o[0-7]+|0x[0-9a-fA-F]+|[0-9]+)$"),
    list("-+0123456789"),
)


def integer(loader, node):
    value = loader.construct_scalar(node)
    return int(value, 0 if value.lstrip("-+").startswith(("0o", "0x")) else 10)


WorkflowLoader.add_constructor("tag:yaml.org,2002:int", integer)


class WorkflowDumper(yaml.SafeDumper):
    pass


def string(dumper, value):
    return dumper.represent_scalar(
        "tag:yaml.org,2002:str", value, style="|" if "\n" in value else None
    )


WorkflowDumper.add_representer(str, string)


def load(text):
    return yaml.load(text, Loader=WorkflowLoader)


def replace_step(workflow, job, key, value, replacement):
    steps = workflow["jobs"][job]["steps"]
    matches = [index for index, step in enumerate(steps) if step.get(key) == value]
    if len(matches) != 1:
        raise RuntimeError(f"generated {job} step changed: {key}={value}")
    steps[matches[0]] = copy.deepcopy(replacement)


def customize(workflow, custom):
    workflow["permissions"] = {}
    workflow["env"] = custom["env"]
    jobs = {"release-gate": custom["release-gate"], **workflow["jobs"]}
    jobs["generate-checksum-manifest"] = custom["generate-checksum-manifest"]
    workflow["jobs"] = jobs
    replacements = [
        ("plan", "name", "Install dist", "plan/install-dist"),
        ("plan", "id", "plan", "plan/plan"),
        (
            "build-global-artifacts",
            "id",
            "cargo-dist",
            "build-global-artifacts/cargo-dist",
        ),
        ("host", "id", "host", "host/host"),
        ("announce", "name", "Create GitHub Release", "announce/release"),
    ]
    for job, key, value, fragment in replacements:
        replace_step(workflow, job, key, value, custom["steps"][fragment])
    # Planning creates the draft release; hosting uploads its artifacts. Grant
    # write access only to those jobs and the final announcement, not globally.
    jobs["plan"]["permissions"] = {"contents": "write"}
    for name in ["custom-build-binaries", "build-global-artifacts"]:
        jobs[name]["permissions"] = {"contents": "read"}
    jobs["build-global-artifacts"]["needs"].append("generate-checksum-manifest")
    jobs["host"]["needs"].extend(["generate-checksum-manifest", "release-gate"])
    jobs["host"]["permissions"] = {"contents": "write"}
    jobs["host"]["environment"] = {"name": "release"}
    jobs["host"]["if"] = custom["host-condition"]
    jobs["custom-publish-pypi"]["needs"].append("release-gate")
    jobs["custom-publish-pypi"]["permissions"] = {
        "contents": "read",
        "id-token": "write",
    }
    jobs["announce"]["needs"].append("release-gate")
    jobs["announce"]["environment"] = {"name": "release"}
    jobs["announce"]["if"] = custom["announce-condition"]
    jobs["announce"].pop("env", None)
    return workflow


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist", default="dist")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    config = ROOT / "dist-workspace.toml"
    workflow = ROOT / ".github/workflows/release.yml"
    original_config = config.read_text()
    original_workflow = workflow.read_text()
    version = tomllib.loads(original_config)["dist"]["cargo-dist-version"]
    actual = subprocess.check_output([args.dist, "--version"], text=True).strip()
    if actual != f"cargo-dist {version}":
        raise RuntimeError(f"expected cargo-dist {version}, got {actual}")
    custom = json.loads((ROOT / ".github/release-customizations.json").read_text())
    if custom["env"]["CARGO_DIST_VERSION"] != version:
        raise RuntimeError(
            "update the pinned archive/checksum in release-customizations.json"
        )
    success = False
    try:
        # The repository explicitly permits reviewed CI customizations. Clear
        # that switch only for template generation, then restore it unchanged.
        config.write_text(
            re.sub(r"(?m)^allow-dirty = .*", "allow-dirty = []", original_config)
        )
        subprocess.run([args.dist, "generate", "--mode", "ci"], cwd=ROOT, check=True)
        expected = customize(load(workflow.read_text()), custom)
        if args.check:
            if load(original_workflow) != expected:
                raise RuntimeError(
                    "release.yml differs; run scripts/generate_release_workflow.py"
                )
        else:
            workflow.write_text(
                "# Generated by scripts/generate_release_workflow.py using pinned cargo-dist.\n"
                "# Edit dist-workspace.toml or .github/release-customizations.json and regenerate.\n"
                + yaml.dump(
                    expected, Dumper=WorkflowDumper, sort_keys=False, width=1000
                )
            )
        success = True
    finally:
        config.write_text(original_config)
        if args.check or not success:
            workflow.write_text(original_workflow)


if __name__ == "__main__":
    main()
