#!/usr/bin/env python3
"""Prepare the unchanged django-ty stress generator at fixed model counts."""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import shutil
import sys
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--django", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--models", type=int, nargs="+", default=[0, 25, 100])
parser.add_argument("--fields", type=int, default=8)
parser.add_argument("--queries", type=int, default=3)
parser.add_argument(
    "--relations", choices=["shallow", "chain", "none"], default="shallow"
)
args = parser.parse_args()
repo = args.django.resolve()
out = args.output.resolve()
assert not out.exists()
assert all(models >= 0 for models in args.models)
assert args.fields > 0 and args.queries >= 0
out.mkdir(parents=True)
sys.path.insert(0, str(repo / "scripts"))
source = repo / "scripts/benchmark_runtimes.py"
spec = importlib.util.spec_from_file_location("django_stress_generator", source)
assert spec and spec.loader
generator = importlib.util.module_from_spec(spec)
spec.loader.exec_module(generator)


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


original = repo / "e2e/django5_fixture"
records = []
for count in args.models:
    root = out / f"models-{count}"
    shutil.copytree(
        original,
        root,
        ignore=shutil.ignore_patterns("__pycache__", ".mypy_cache", ".venv", "*.pyc"),
    )
    settings = root / "config/settings.py"
    settings.write_text(
        settings.read_text() + '\nINSTALLED_APPS = [*INSTALLED_APPS, "benchmark_app"]\n'
    )
    app = root / "benchmark_app"
    app.mkdir()
    (app / "__init__.py").write_text("")
    (app / "models.py").write_text(
        generator.stress_source(count, args.fields, args.queries, args.relations)
    )
    paths = generator.PATHS + (["benchmark_app"] if count else [])
    source_hashes = {
        str(path.relative_to(root)): digest(path)
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }
    records.append(
        {
            "extra_models": count,
            "fields_per_extra_model": args.fields,
            "queries_per_extra_model": args.queries,
            "relations": args.relations,
            "project": str(root),
            "paths": paths,
            "settings": "config.settings",
            "source_hashes": source_hashes,
        }
    )
report = {
    "generator": str(source),
    "generator_sha256": digest(source),
    "preparation_sha256": digest(Path(__file__)),
    "purpose": "Original unchanged e2e-positive checks plus existing django-ty generated model stress. Only settings app registration appended; original model/typecheck source bytes preserved.",
    "workloads": records,
}
(out / "plan.json").write_text(json.dumps(report, indent=2) + "\n")
print(out / "plan.json")
