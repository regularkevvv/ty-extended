#!/usr/bin/env python3
"""Check the unchanged Django corpus through the editor-facing Rust server."""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import time
from pathlib import Path

from lsp_acceptance import Client


def signature(items):
    return sorted(
        (
            d["code"],
            d["range"]["start"]["line"],
            d["range"]["start"]["character"],
            d["range"]["end"]["line"],
            d["range"]["end"]["character"],
            d["severity"],
        )
        for d in items
        if "code" in d
    )


def hints(items):
    return sorted(json.dumps(d, sort_keys=True) for d in items if "code" not in d)


def reference_edits(args, root, paths, reports):
    """Measure the same verified edits in a persistent reference-fork server."""
    package = args.django / "python/django_ty"
    config = args.output / "reference.toml"
    config.write_text(
        f"[environment]\npython={json.dumps(str(args.python))}\n"
        f"python-version={json.dumps(args.python_version)}\n"
        "[plugins]\nenabled=true\nauto-discover=false\n"
        "[[plugins.plugin]]\nid='django-ty'\nruntime='monty'\ntrusted=true\n"
        f"path={json.dumps(str(package / 'monty.py'))}\n"
        f"manifest-path={json.dumps(str(package / 'ty-plugin-monty.json'))}\n"
        f"stub-overlay-path={json.dumps(str(package / 'stubs'))}\n"
        "[plugins.plugin.config]\ndjango-settings-module='conformance_project.settings'\n"
    )
    client = Client([str(args.fork), "server"], root, args.output / "reference.stderr")
    try:
        client.request(
            "initialize",
            {
                "rootUri": root.as_uri(),
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {"diagnostic": {}},
                },
                "initializationOptions": {
                    "configurationFile": str(config),
                    "diagnosticMode": "openFilesOnly",
                },
            },
        )
        client.notify("initialized")
        for path in paths:
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": path.as_uri(),
                        "version": 1,
                        "languageId": "python",
                        "text": path.read_text(),
                    }
                },
            )
        for path in paths:
            actual = client.request(
                "textDocument/diagnostic", {"textDocument": {"uri": path.as_uri()}}
            )
            wanted = reports[str(path.relative_to(root))]["items"]
            assert signature(actual["items"]) == signature(wanted), actual
            assert hints(actual["items"]) == hints(wanted), (path, actual, wanted)
        path = root / "cases/model_state.py"
        uri = path.as_uri()
        text = path.read_text()
        original = set(signature(reports[str(path.relative_to(root))]["items"]))
        samples = {"comment": [], "semantic": []}
        for version in range(2, 22):
            semantic = version >= 12
            invalid = semantic and version % 2 == 0
            value = '"wrong"' if invalid else "42"
            suffix = (
                f"\ncompanion_negative_control: int = {value}\n"
                if semantic
                else f"\n# editor revision {version}\n"
            )
            started = time.perf_counter()
            client.notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [{"text": text + suffix}],
                },
            )
            try:
                report = client.request(
                    "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                )
            except RuntimeError as error:
                raise RuntimeError(
                    f"Reference fork failed at revision {version} ({'semantic' if semantic else 'comment'}): {error}"
                ) from error
            seconds = time.perf_counter() - started
            actual = set(signature(report["items"]))
            extra = actual - original
            assert original <= actual
            assert (
                (len(extra) == 1 and next(iter(extra))[0] == "invalid-assignment")
                if invalid
                else not extra
            )
            if version >= (15 if semantic else 5):
                samples["semantic" if semantic else "comment"].append(seconds)
        return {
            "passed": True,
            "binary_sha256": hashlib.sha256(args.fork.read_bytes()).hexdigest(),
            **{f"warm_{key}_seconds": values for key, values in samples.items()},
            **{
                f"warm_{key}_median_seconds": statistics.median(values)
                for key, values in samples.items()
            },
        }
    finally:
        client.close()


def main():
    parser = argparse.ArgumentParser()
    for name in ("companion", "official", "django", "python", "acceptance", "output"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    parser.add_argument("--python-version", default="3.13")
    parser.add_argument("--fork", type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True)
    root = args.acceptance / "inputs/conformance"
    config = args.acceptance / "runs/conformance/native/config.json"
    files = args.acceptance / "runs/conformance/native/files.json"
    expected = json.loads((args.acceptance / "acceptance.json").read_text())["checks"][
        "conformance"
    ]["fork"]["diagnostics"]
    package = args.django / "python/django_ty"
    source_hashes = {
        str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
        for p in root.rglob("*.py")
    }
    client = Client(
        [
            str(args.companion),
            "server",
            str(root),
            "--ty",
            str(args.official),
            "--plugin",
            str(package / "monty.py"),
            "--manifest",
            str(package / "ty-plugin-monty.json"),
            "--stubs",
            str(package / "stubs"),
            "--config",
            str(config),
            "--files",
            str(files),
            "--python",
            str(args.python),
            "--python-version",
            args.python_version,
            "--out",
            str(args.output / "analysis"),
        ],
        root,
        args.output / "stderr.log",
    )
    evidence = {
        "companion_sha256": hashlib.sha256(args.companion.read_bytes()).hexdigest(),
        "official_sha256": hashlib.sha256(args.official.read_bytes()).hexdigest(),
        "source_sha256": source_hashes,
        "method": "Persistent servers, three discarded edits then seven measured edits per kind; reference runs after companion; diagnostics verified after every edit.",
    }
    try:
        client.request(
            "initialize",
            {
                "rootUri": root.as_uri(),
                "capabilities": {
                    "general": {"positionEncodings": ["utf-16"]},
                    "textDocument": {
                        "diagnostic": {},
                        "hover": {"contentFormat": ["markdown"]},
                    },
                },
            },
        )
        client.notify("initialized")
        checked = set(json.loads(files.read_text()))
        paths = []
        for name in checked:
            path = root / name
            paths.extend(path.rglob("*.py") if path.is_dir() else [path])
        for path in paths:
            client.notify(
                "textDocument/didOpen",
                {
                    "textDocument": {
                        "uri": path.as_uri(),
                        "version": 1,
                        "languageId": "python",
                        "text": path.read_text(),
                    }
                },
            )
        actual = []
        reports = {}
        for path in paths:
            report = client.request(
                "textDocument/diagnostic", {"textDocument": {"uri": path.as_uri()}}
            )
            name = str(path.relative_to(root))
            reports[name] = report
            for d in report["items"]:
                if "code" not in d:
                    assert d.get("severity") == 4 and 1 in d.get("tags", []), d
                    continue
                actual.append(
                    (
                        name,
                        d["code"],
                        d["range"]["start"]["line"] + 1,
                        d["range"]["start"]["character"] + 1,
                        d["range"]["end"]["line"] + 1,
                        d["range"]["end"]["character"] + 1,
                        d["severity"],
                    )
                )
        wanted = [
            (
                d["path"],
                d["code"],
                d["begin"]["line"],
                d["begin"]["column"],
                d["end"]["line"],
                d["end"]["column"],
                {"major": 1, "minor": 2, "info": 3}[d["severity"]],
            )
            for d in expected
        ]
        assert sorted(actual) == sorted(wanted), (
            set(actual) - set(wanted),
            set(wanted) - set(actual),
        )
        evidence["diagnostics"] = reports
        evidence["baseline_diagnostic_count"] = len(wanted)
        path = root / "cases/model_state.py"
        uri = path.as_uri()
        text = path.read_text()
        baseline = reports[str(path.relative_to(root))]["items"]
        original_signature = signature(baseline)
        pid = client.request("tyCompanion/statistics")["checker_pids"]
        samples = []
        for version in range(2, 12):
            modified = text + f"\n# editor revision {version}\n"
            started = time.perf_counter()
            client.notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [{"text": modified}],
                },
            )
            result = client.request(
                "textDocument/diagnostic", {"textDocument": {"uri": uri}}
            )
            elapsed = time.perf_counter() - started
            assert signature(result["items"]) == original_signature
            if version >= 5:
                samples.append(elapsed)
        # Real semantic changes require re-analysis; measure them separately
        # from trailing trivia so the benchmark cannot hide normal edit cost.
        semantic_samples = []
        for version in range(12, 22):
            invalid = version % 2 == 0
            value = '"wrong"' if invalid else "42"
            modified = text + f"\ncompanion_negative_control: int = {value}\n"
            started = time.perf_counter()
            client.notify(
                "textDocument/didChange",
                {
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": [{"text": modified}],
                },
            )
            report = client.request(
                "textDocument/diagnostic", {"textDocument": {"uri": uri}}
            )
            elapsed = time.perf_counter() - started
            extra = set(signature(report["items"])) - set(original_signature)
            assert (
                (len(extra) == 1 and next(iter(extra))[0] == "invalid-assignment")
                if invalid
                else not extra
            ), extra
            assert set(original_signature) <= set(signature(report["items"]))
            if version >= 15:
                semantic_samples.append(elapsed)
        client.notify("textDocument/didClose", {"textDocument": {"uri": uri}})
        assert (
            signature(
                client.request(
                    "textDocument/diagnostic", {"textDocument": {"uri": uri}}
                )["items"]
            )
            == original_signature
        )
        stats = client.request("tyCompanion/statistics")
        assert stats["checker_pids"] == pid
        assert source_hashes == {
            str(p.relative_to(root)): hashlib.sha256(p.read_bytes()).hexdigest()
            for p in root.rglob("*.py")
        }
        evidence.update(
            passed=True,
            warm_comment_seconds=samples,
            warm_comment_median_seconds=statistics.median(samples),
            warm_semantic_seconds=semantic_samples,
            warm_semantic_median_seconds=statistics.median(semantic_samples),
            statistics=stats,
            timings=client.timings,
        )
    finally:
        client.close()
        try:
            if evidence.get("passed") and args.fork:
                evidence["passed"] = False
                evidence["reference"] = reference_edits(args, root, paths, reports)
                evidence["passed"] = True
        finally:
            (args.output / "evidence.json").write_text(
                json.dumps(evidence, indent=2) + "\n"
            )
    print(
        f"Django editor corpus: PASS; comment median {statistics.median(samples):.3f}s; semantic edit median {statistics.median(semantic_samples):.3f}s"
    )


if __name__ == "__main__":
    main()
