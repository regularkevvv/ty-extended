#!/usr/bin/env python3
"""Export completed, verified gates without copying build or projection trees."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path
from typing import Any

from django_acceptance import Failure, digest, fixture_files, normalize, run, write_json
from django_benchmark import diagnostic_hash

MODES = ("cli", "push")
GENERIC_MODES = ("cli", "lsp", "push")
SUITES = {"conformance", "e2e-positive", "e2e-negative"}
PROTOCOL_TESTS = {
    "existing_django_responses_obey_production_eight_mib_limit",
    "all_django_responses_match_with_explicit_sixteen_mib_limit",
}


def require(condition: Any, message: str) -> None:
    if not condition:
        raise Failure(message)


def read(path: Path) -> dict:
    return json.loads(path.read_text())


def checker_release(version: str) -> str:
    match = re.match(r"^ty (\d+\.\d+\.\d+)(?:\+[A-Za-z0-9.-]+)?(?:\s|$)", version)
    require(match is not None, f"unrecognized checker version: {version}")
    return match.group(1)


def source_identity(root: Path) -> dict:
    paths = [root / "Cargo.toml", root / "Cargo.lock"]
    for directory, suffix in (("src", ".rs"), ("tests", ".rs"), ("scripts", ".py")):
        paths.extend((root / directory).rglob(f"*{suffix}"))
    paths.extend((root / "tests").rglob("*.py"))
    paths.extend((root / "tests").rglob("*.json"))
    commits = {}
    for name, repository in (("repository", root), ("ruff", root.parent / "ruff")):
        commits[name] = subprocess.check_output(
            ["git", "-C", str(repository), "rev-parse", "HEAD"], text=True
        ).strip()
    return {
        "commits": commits,
        "source_sha256": {
            str(path.relative_to(root)): digest(path) for path in sorted(paths)
        },
    }


def generic_identity(args: Any) -> dict:
    return {
        "binaries": {
            name: {
                "path": str(getattr(args, name)),
                "sha256": digest(getattr(args, name)),
            }
            for name in ("companion", "official", "fork")
        },
        "fixture_root": str(args.fixtures),
        "fixture_sha256": {
            str(relative): digest(path)
            for relative, path in sorted(fixture_files(args.fixtures).items())
        },
        "source": source_identity(args.source_root),
    }


def run_generic(args: Any) -> int:
    require(
        not args.output.exists() or not any(args.output.iterdir()),
        "generic output must be new or empty",
    )
    args.output.mkdir(parents=True, exist_ok=True)
    before = generic_identity(args)
    require(
        len(before["fixture_sha256"]) == 62,
        "generic gate requires all 62 fixture files",
    )
    versions = {}
    for name, expected in (("official", "0.0.86"), ("fork", "0.86.0")):
        version = subprocess.check_output(
            [str(getattr(args, name)), "--version"], text=True
        ).strip()
        require(
            checker_release(version) == expected,
            f"generic {name} must be {expected}: {version}",
        )
        versions[name] = version
    write_json(args.output / "provenance-before.json", before)
    command = [
        str(args.companion),
        "suite",
        str(args.fixtures),
        str(args.output),
        args.checker_mode,
        str(args.official),
        str(args.fork),
    ]
    execution = run(command, args.source_root, args.output / "driver", args.timeout)
    after = generic_identity(args)
    require(
        before == after, "generic binaries, fixtures or source changed during execution"
    )
    require(
        execution["exit"] == 0,
        "native generic suite failed; raw driver output retained",
    )
    result = read(args.output / "results.json")
    require(
        result.get("status") == "passed"
        and result.get("checker_mode") == args.checker_mode,
        "generic suite did not report success in requested mode",
    )
    write_json(
        args.output / "provenance.json",
        {
            **before,
            "versions": versions,
            "verified_unchanged_before_after": True,
            "driver_exit": execution["exit"],
            "results_sha256": digest(args.output / "results.json"),
        },
    )
    return 0


class Diagnostics:
    def __init__(self):
        self.arrays: dict[str, list[dict]] = {}

    def add(self, values: list[dict]) -> str:
        key = diagnostic_hash(values)
        self.arrays[key] = values
        return key


def validate_matrix(summary: dict, mode: str) -> list[dict]:
    require(
        summary.get("mode") == "compare" and summary.get("checker_mode") == mode,
        "wrong matrix mode",
    )
    require(
        summary.get("complete_matrix_selected") and summary.get("compatibility_passed"),
        f"{mode} matrix is partial or non-green",
    )
    rows = summary["results"]
    require(
        summary.get("required_pairs") == 18
        and summary.get("selected_pairs") == 18
        and len(rows) == 18,
        "matrix requires exactly 18 pairs",
    )
    require(
        len({(row["django"], row["python"]) for row in rows}) == 18,
        "matrix contains duplicate pairs",
    )
    require(
        all(row.get("passed") and row.get("exit") == 0 for row in rows),
        "matrix contains a non-green pair",
    )
    return rows


def validate_binary_hashes(actual: dict, expected: dict) -> None:
    require(set(actual) == set(expected), "binary provenance set differs")
    for name in expected:
        require(
            actual[name]["sha256"] == expected[name]["sha256"],
            f"mismatched {name} binary",
        )


def accepted_check(row: dict, arrays: Diagnostics) -> dict:
    require(row.get("passed"), "non-green acceptance check")
    fork, native = row["fork"], row["native"]
    require(fork.get("passed") and native.get("passed"), "checker execution failed")
    require(
        fork["exit"] == native["exit"] and fork["diagnostics"] == native["diagnostics"],
        "acceptance diagnostics or exits differ",
    )
    return {
        "passed": True,
        "exit": fork["exit"],
        "diagnostics": len(fork["diagnostics"]),
        "diagnostic_sha256": arrays.add(fork["diagnostics"]),
        "native_metrics": native.get("metrics", {}),
    }


def accepted_report(
    report: dict, inputs: dict, arrays: Diagnostics, mode: str, row: dict
) -> dict:
    require(
        report.get("compatibility_passed")
        and report.get("passed")
        and report.get("runtime_gates_requested")
        and report.get("complete_suites_selected"),
        "acceptance report is partial or non-green",
    )
    require(
        report.get("checker_mode") == mode
        and report.get("python_version") == row["python"],
        "acceptance target differs from matrix",
    )
    require(
        report["environment"]["django"] == row["django"],
        "Django version differs from matrix",
    )
    require(
        "installed_files" in report["environment"]
        and "python_binary_sha256" in report["environment"],
        "missing installed dependency fingerprints",
    )
    require(set(report["checks"]) == SUITES, "missing project acceptance suite")
    checks = {}
    for name, check in report["checks"].items():
        require(
            check.get("exact_diagnostics_equal") and check.get("exit_equal"),
            "project parity flags are not green",
        )
        checks[name] = accepted_check(check, arrays)
        if name == "conformance":
            for side in ("fork_contract", "native_contract"):
                contract = check[side]
                require(
                    contract.get("passed")
                    and contract["matched"]
                    == contract["total"]
                    == inputs["assertions"]
                    == 166,
                    "conformance does not pass all 166 assertions",
                )
                require(
                    len(contract["cases"]) == 166
                    and all(case.get("passed") for case in contract["cases"])
                    and not contract["unowned_diagnostics"],
                    "incomplete conformance case evidence",
                )
            checks[name]["cases"] = [
                {key: value for key, value in case.items() if key != "diagnostics"}
                for case in check["native_contract"]["cases"]
            ]
        elif name == "e2e-positive":
            require(
                checks[name]["exit"] == 0 and checks[name]["diagnostics"] == 0,
                "positive E2E rejected",
            )
        else:
            require(
                checks[name]["exit"] == 1
                and check.get("missing_negative_controls") == [],
                "negative E2E controls missing",
            )
    runtime = report["runtime"]
    require(
        runtime.get("passed")
        and len(runtime["cases"]) == 83
        and all(case.get("passed") for case in runtime["cases"].values()),
        "Django runtime gate incomplete or failed",
    )
    querydict = report["querydict"]
    require(
        querydict.get("passed") and len(querydict["cases"]) == 9,
        "QueryDict gate incomplete",
    )
    return {
        "django": row["django"],
        "python": row["python"],
        "passed": True,
        "environment": report["environment"],
        "checks": checks,
        "runtime": runtime,
        "querydict": {
            name: {**accepted_check(case, arrays), "runtime": case["runtime"]}
            for name, case in querydict["cases"].items()
        },
    }


def generic_signature(values: list[dict]) -> list[str]:
    return sorted(
        json.dumps(
            {
                "code": value["code"],
                "begin": value["begin"],
                "end": value["end"],
                "reveal": value["message"].split("Revealed type: ", 1)[1]
                if value["code"] == "revealed-type"
                else "",
            },
            sort_keys=True,
        )
        for value in values
    )


def generic_check(row: dict, source: Path, arrays: Diagnostics) -> dict:
    fork = normalize(row["fork_diagnostics"], source.parent)
    native = normalize(row["companion"]["diagnostics"], source.parent)
    require(
        generic_signature(fork) == generic_signature(native),
        "generic diagnostic codes/spans/reveals differ",
    )
    result = {
        key: value
        for key, value in row.items()
        if key not in {"fork_diagnostics", "companion"}
    }
    result["fork_diagnostic_sha256"] = arrays.add(fork)
    result["native_diagnostic_sha256"] = arrays.add(native)
    result["native_metrics"] = {
        key: value
        for key, value in row["companion"].items()
        if key not in {"diagnostics", "raw_diagnostics"}
    }
    return result


def validate_generic(
    result: dict, provenance: dict, mode: str, companion_hash: str, arrays: Diagnostics
) -> dict:
    require(
        result.get("status") == "passed" and result.get("checker_mode") == mode,
        f"generic {mode} gate non-green",
    )
    require(
        provenance.get("verified_unchanged_before_after")
        and provenance.get("driver_exit") == 0,
        "generic run lacks before/after provenance",
    )
    require(
        provenance["binaries"]["companion"]["sha256"] == companion_hash,
        "generic gate used mismatched companion binary",
    )
    require(len(provenance["fixture_sha256"]) == 62, "generic fixture set incomplete")
    require(
        len(result["differentials"]) == 17
        and len({row["case"] for row in result["differentials"]}) == 17,
        "generic differential cases incomplete",
    )
    differentials = []
    for row in result["differentials"]:
        require(
            row.get("same_codes_original_spans_and_revealed_types"),
            "generic differential failed",
        )
        differentials.append(generic_check(row, Path(row["source"]), arrays))
    controls = result["unsupported_controls"]
    require(
        len(controls) == 4
        and all(row.get("rejected") and row.get("exit") == 2 for row in controls),
        "generic unsupported controls incomplete or failed",
    )
    require(
        result.get("warmup_runs") == 1 and result.get("measured_repetitions") == 5,
        "generic repetitions incomplete",
    )
    expected = {"original": 14, "10_copies": 140, "100_copies": 1400}
    require(
        {row["fixture"]: row["assert_type_count"] for row in result["benchmarks"]}
        == expected,
        "generic scaling cases incomplete",
    )
    benchmarks = []
    for row in result["benchmarks"]:
        require(
            len(row["runs"]) == 6
            and {sample["repetition"] for sample in row["runs"]} == set(range(6)),
            "generic benchmark repetitions missing",
        )
        samples = []
        for sample in row["runs"]:
            require(
                sample.get("matching_code_spans_and_reveals")
                and sample.get("two_expected_negative_controls"),
                "generic benchmark sample not verified",
            )
            require(
                sample["warmup"] == (sample["repetition"] == 0),
                "generic warmup mislabeled",
            )
            samples.append(generic_check(sample, Path(row["source"]), arrays))
        benchmarks.append({**row, "runs": samples})
    return {
        **{
            key: value
            for key, value in result.items()
            if key not in {"differentials", "unsupported_controls", "benchmarks"}
        },
        "provenance": provenance,
        "differentials": differentials,
        "unsupported_controls": controls,
        "benchmarks": benchmarks,
    }


def validate_benchmark(
    report: dict, binaries: dict, matrix: dict, arrays: Diagnostics
) -> None:
    require(report.get("verified"), "Django benchmark is non-green")
    validate_binary_hashes(report["binaries"], binaries)
    method = report["method"]
    require(
        set(method["checker_modes"]) == set(MODES)
        and method["repetitions"] >= 3
        and method["warmups"] >= 0,
        "benchmark modes/repetitions incomplete",
    )
    matching = [
        row for row in matrix["cli"] if row["environment"] == report["environment"]
    ]
    require(
        len(matching) == 1,
        "benchmark environment does not match an accepted matrix pair",
    )
    pair = matching[0]
    for suite, expected in report["expected"].items():
        accepted = pair["checks"][suite]
        require(
            expected["diagnostic_sha256"] == accepted["diagnostic_sha256"]
            and expected["exit"] == accepted["exit"],
            "benchmark expected diagnostics differ from acceptance",
        )
    require(set(report["expected"]) == SUITES, "benchmark workload missing")
    required = {
        (phase, repetition, suite, backend)
        for phase, count in (
            ("first-observed", 1),
            ("warmup", method["warmups"]),
            ("measured", method["repetitions"]),
        )
        for repetition in range(count)
        for suite in SUITES
        for backend in ("fork", *MODES)
    }
    seen = set()
    counters = {}
    for sample in report["records"]:
        key = (
            sample["phase"],
            sample["repetition"],
            sample["suite"],
            sample["backend"],
        )
        require(key not in seen, "duplicate benchmark sample")
        seen.add(key)
        expected = report["expected"][sample["suite"]]
        require(
            sample.get("verified")
            and sample["exit"] == expected["exit"]
            and sample["diagnostic_sha256"] == expected["diagnostic_sha256"],
            "benchmark sample non-green or diagnostics changed",
        )
        require(
            sample["diagnostic_sha256"] in arrays.arrays,
            "benchmark diagnostic hash has no retained array",
        )
        if sample["backend"] != "fork":
            require(
                sample["native_metrics"].get("checker_mode") == sample["backend"],
                "benchmark checker mode mislabeled",
            )
            for name in ("projection_rounds", "oracle_rounds", "monty_executions"):
                require(
                    sample["counters"].get(name, 0) > 0,
                    "benchmark native work counters missing",
                )
            counter_key = (sample["suite"], sample["backend"])
            require(
                counter_key not in counters
                or counters[counter_key] == sample["counters"],
                "benchmark work counters changed",
            )
            counters[counter_key] = sample["counters"]
    require(seen == required, "benchmark sample set incomplete")


def rust_log(path: Path, expected: int, protocol: bool = False) -> dict:
    text = path.read_text()
    footers = re.findall(
        r"test result: ok\. (\d+) passed; (\d+) failed; (\d+) ignored;", text
    )
    require(
        footers
        and not re.search(r"test result: FAILED|^error(?:\[|:)", text, re.MULTILINE),
        f"failed or missing Rust test result: {path}",
    )
    require(
        sum(int(row[0]) for row in footers) == expected
        and all(int(row[1]) == 0 for row in footers),
        f"incorrect Rust test count: {path}",
    )
    if protocol:
        require(
            expected == 2 and all(int(row[2]) == 0 for row in footers),
            "protocol replay tests were ignored",
        )
        require(
            all(
                re.search(rf"^test {name} \.\.\.", text, re.MULTILINE)
                for name in PROTOCOL_TESTS
            ),
            "missing required protocol replay test",
        )
    result = {
        "artifact": str(path),
        "sha256": digest(path),
        "passed": expected,
        "ignored": sum(int(row[2]) for row in footers),
    }
    if protocol:
        counts = re.findall(
            r"(\d+) Django regressions checked with (\d+) byte response limit \((\d+) expected oversized rejections\), plus matching packaged manifest: (\{[^\n]+\})",
            text,
        )
        require(
            len(counts) == 2,
            "protocol counts missing; run replay tests with --nocapture",
        )
        require(
            {(int(limit), int(rejected)) for _, limit, rejected, _ in counts}
            == {(8388608, 1), (16777216, 0)}
            and all(int(total) >= 150 for total, _, _, _ in counts),
            "protocol replay bounds/corpus incomplete",
        )
        result["replays"] = [
            {
                "requests": int(total),
                "max_response_bytes": int(limit),
                "oversized_rejections": int(rejected),
                "matched_requests_by_kind": json.loads(kinds),
            }
            for total, limit, rejected, kinds in counts
        ]
    return result


def supplemental_json(path: Path, companion_sha256: str | None = None) -> dict:
    value = read(path)
    require(
        value.get("passed") is True
        or value.get("status") == "passed"
        or value.get("verified") is True,
        f"supplemental JSON has no explicit green outcome: {path}",
    )
    require(
        not any(
            value.get(name) is False
            for name in ("passed", "verified", "compatibility_passed")
        )
        and value.get("status") not in {"failed", "running"},
        f"supplemental JSON has a failed/incomplete outcome: {path}",
    )
    if companion_sha256 is not None:
        identities = {
            key: value[key]
            for key in ("companion_sha256", "binary_sha256")
            if key in value
        }
        if "artifact" in value and "sha256" in value:
            identities["artifact.sha256"] = value["sha256"]
        for key in ("companion", "companion_binary"):
            record = value.get(key)
            if isinstance(record, dict) and "sha256" in record:
                identities[f"{key}.sha256"] = record["sha256"]
        records = value.get("binaries")
        if isinstance(records, dict):
            record = records.get("companion")
            if isinstance(record, dict) and "sha256" in record:
                identities["binaries.companion.sha256"] = record["sha256"]
        require(
            identities,
            f"supplemental JSON has no companion binary fingerprint: {path}",
        )
        for key, fingerprint in identities.items():
            require(
                fingerprint == companion_sha256,
                f"supplemental {key} differs from accepted companion: {path}",
            )
    return value


def path_label(labels: dict[str, str], specification: str) -> None:
    name, separator, value = specification.partition("=")
    require(
        separator and value and re.fullmatch(r"[a-z0-9_-]+", name),
        "path labels require lowercase LABEL=PATH",
    )
    path = Path(value).resolve()
    require(path.exists(), f"path label does not exist: {path}")
    label = f"artifact:{name}"
    require(
        str(path) not in labels or labels[str(path)] == label,
        f"path already has another evidence label: {path}",
    )
    require(
        all(
            existing != label or original == str(path)
            for original, existing in labels.items()
        ),
        f"evidence label already identifies another path: {label}",
    )
    labels[str(path)] = label


def portable(value: Any, labels: dict[str, str]) -> Any:
    if isinstance(value, dict):
        return {
            portable(key, labels): portable(item, labels) for key, item in value.items()
        }
    if isinstance(value, list):
        return [portable(item, labels) for item in value]
    if isinstance(value, str):
        for path, label in sorted(
            labels.items(), key=lambda item: len(item[0]), reverse=True
        ):
            # A labeled directory must not also label an unrelated sibling
            # whose name merely starts with the same bytes.
            value = re.sub(
                re.escape(path) + r"(?=$|[/\\\s\"'<>),;:\]?#])",
                lambda _match, replacement=label: replacement,
                value,
            )
        require(
            not value.startswith("/")
            and "/private/tmp/" not in value
            and "/Users/" not in value,
            f"unlabeled local path in portable evidence: {value[:160]}",
        )
    return value


def collect(args: Any) -> None:
    arrays = Diagnostics()
    labels = {
        str(args.acceptance_root): "artifact:django-acceptance",
        str(args.source_root): "source:companion",
        str(args.companion): "binary:companion",
    }
    originals = {}

    def artifact(path: Path, label: str) -> dict:
        labels[str(path)] = label
        record = {"artifact": label, "sha256": digest(path)}
        originals[label] = {"path": str(path), "sha256": record["sha256"]}
        return record

    matrix = {}
    binaries = inputs = None
    evidence = {}
    for mode in MODES:
        summary_path = args.acceptance_root / "matrix" / f"compare-{mode}.json"
        summary = read(summary_path)
        rows = validate_matrix(summary, mode)
        evidence[f"matrix-{mode}"] = artifact(summary_path, f"artifact:matrix-{mode}")
        matrix[mode] = []
        for row in rows:
            report_path = Path(row["evidence"])
            report = read(report_path)
            current_inputs = read(report_path.with_name("inputs.json"))
            if inputs is None:
                inputs, binaries = current_inputs, report["binaries"]
                labels[inputs["repository"]] = "source:django-ty"
                require(
                    binaries["companion"]["sha256"] == digest(args.companion),
                    "accepted companion differs from supplied binary",
                )
                for name, record in binaries.items():
                    labels[record["path"]] = (
                        "binary:companion"
                        if name == "companion"
                        else f"binary:django-{name}"
                    )
            require(current_inputs == inputs, "matrix input source hashes differ")
            validate_binary_hashes(report["binaries"], binaries)
            portable_row = accepted_report(report, inputs, arrays, mode, row)
            portable_row["original_report"] = artifact(
                report_path,
                f"artifact:{mode}-django-{row['django']}-python-{row['python']}",
            )
            matrix[mode].append(portable_row)
        require(
            {(row["django"], row["python"]) for row in rows}
            == {(row["django"], row["python"]) for row in inputs["matrix"]},
            "matrix pairs differ from original declared contract",
        )
    assert binaries is not None and inputs is not None
    for left, right in zip(
        sorted(matrix["cli"], key=lambda row: (row["django"], row["python"])),
        sorted(matrix["push"], key=lambda row: (row["django"], row["python"])),
    ):
        require(
            left["environment"] == right["environment"],
            "matrix mode environments differ",
        )
        for suite in SUITES:
            require(
                left["checks"][suite]["diagnostic_sha256"]
                == right["checks"][suite]["diagnostic_sha256"]
                and left["checks"][suite]["exit"] == right["checks"][suite]["exit"],
                "CLI/push matrix diagnostics differ",
            )
    generic = {}
    current_source = source_identity(args.source_root)
    fixtures = None
    generic_binaries = None
    for mode in GENERIC_MODES:
        directory = getattr(args, f"generic_{mode}")
        labels[str(directory)] = f"artifact:generic-{mode}"
        provenance = read(directory / "provenance.json")
        require(
            provenance["results_sha256"] == digest(directory / "results.json"),
            "generic results changed after verification",
        )
        require(
            provenance["source"] == current_source,
            "generic gate source differs from current source",
        )
        if fixtures is None:
            fixtures = provenance["fixture_sha256"]
            generic_binaries = provenance["binaries"]
        require(
            provenance["fixture_sha256"] == fixtures, "generic mode fixtures differ"
        )
        validate_binary_hashes(provenance["binaries"], generic_binaries)
        require(
            {
                str(relative): digest(path)
                for relative, path in fixture_files(
                    Path(provenance["fixture_root"])
                ).items()
            }
            == fixtures,
            "generic source fixtures changed since their run",
        )
        labels[provenance["fixture_root"]] = "source:generic-fixtures"
        for name, record in provenance["binaries"].items():
            labels[record["path"]] = (
                "binary:companion" if name == "companion" else f"binary:generic-{name}"
            )
        generic[mode] = validate_generic(
            read(directory / "results.json"),
            provenance,
            mode,
            binaries["companion"]["sha256"],
            arrays,
        )
        evidence[f"generic-{mode}"] = artifact(
            directory / "results.json", f"artifact:generic-{mode}-results"
        )
    benchmark = read(args.benchmark)
    require(
        read(args.benchmark.with_name("inputs.json")) == inputs,
        "benchmark inputs differ from accepted matrix source",
    )
    for mode in MODES:
        accepted = benchmark["acceptance"][mode]
        path = Path(accepted["path"])
        require(
            digest(path) == accepted["sha256"],
            "benchmark acceptance report changed after verification",
        )
        require(
            accepted["sha256"]
            in {row["original_report"]["sha256"] for row in matrix[mode]},
            "benchmark acceptance is not a final matrix report",
        )
    validate_benchmark(benchmark, binaries, matrix, arrays)
    benchmark_path = artifact(args.benchmark, "artifact:django-benchmark")
    labels[str(args.benchmark.parent)] = "artifact:django-benchmark-runs"
    gates = {
        "unit": rust_log(args.unit_log, args.expected_unit_tests),
        "protocol": rust_log(args.protocol_log, 2, protocol=True),
    }
    artifact(args.unit_log, "artifact:unit-test-log")
    artifact(args.protocol_log, "artifact:protocol-test-log")
    supplemental = {}
    for kind, specifications in (
        ("json", args.supplemental_json),
        ("log", args.supplemental_log),
    ):
        for specification in specifications:
            label, separator, value = specification.partition("=")
            require(
                separator
                and re.fullmatch(r"[a-z0-9_-]+", label)
                and label not in supplemental,
                "supplemental gates require unique lowercase LABEL=PATH",
            )
            path = Path(value).resolve()
            if kind == "json":
                value = supplemental_json(path, binaries["companion"]["sha256"])
            else:
                counts = re.findall(
                    r"test result: ok\. (\d+) passed;", path.read_text()
                )
                count = sum(map(int, counts))
                require(
                    count > 0,
                    "supplemental Rust test log has no executed passing tests",
                )
                value = rust_log(path, count)
            artifact_record = artifact(path, f"artifact:supplemental-{label}")
            supplemental[label] = {"original_report": artifact_record, "result": value}
    for specification in args.path_label:
        path_label(labels, specification)
    documents = {
        "provenance.json": {
            "schema": 1,
            "binaries": binaries,
            "source": current_source,
            "django_inputs": inputs,
            "generic_fixture_sha256": fixtures,
            "gates": gates,
            "original_artifacts": evidence,
        },
        "matrix.json": {"passed": True, "pairs_per_mode": 18, "modes": matrix},
        "generic.json": {"passed": True, "modes": generic},
        "benchmark.json": {"original_report": benchmark_path, **benchmark},
        "diagnostics.json": {
            "hash_algorithm": "sha256 of sorted-key compact JSON of normalized diagnostic array",
            "arrays": arrays.arrays,
        },
        "supplemental.json": supplemental,
    }
    # Diagnostic payloads are already original-source-relative. Never rewrite a
    # message or location while preserving its old hash.
    require(
        portable(arrays.arrays, labels) == arrays.arrays,
        "diagnostic payload would require path rewriting; cannot preserve its hash",
    )
    documents = {name: portable(value, labels) for name, value in documents.items()}
    require(
        not args.output.exists() or not any(args.output.iterdir()),
        "portable output must be new or empty",
    )
    for name, value in documents.items():
        write_json(args.output / name, value)
    write_json(
        args.acceptance_root / "portable-evidence-originals.json",
        {
            "artifacts": originals,
            "path_labels": labels,
            "portable_files": {name: digest(args.output / name) for name in documents},
        },
    )
    (args.output / "README.md").write_text(
        "# Verified experiment evidence\n\n"
        "These reports were exported only after both 18-pair Django matrices, all "
        "three generic checker modes, protocol replay, unit tests, and verified "
        "Django benchmarks passed. This proves the recorded corpus; it does not "
        "claim every plugin or editor workflow is supported.\n\n"
        "`provenance.json` pins sources, checker binaries and raw gate logs. "
        "`matrix.json` retains installed dependency fingerprints and every "
        "contract case. `diagnostics.json` deduplicates complete normalized "
        "diagnostic arrays by hash. Generic non-reveal wording is not its parity "
        "contract; both actual message arrays are retained.\n\n"
        "`benchmark.json` retains every first-observed, warmup and measured "
        "sample plus native counters. Each invocation starts fresh processes; "
        "push mode reuses its official checker within an invocation only.\n\n"
        "`source:` and `binary:` labels identify inputs; `artifact:` labels "
        "identify original local reports. The unmodified originals and the "
        "label/path mapping remain under the acceptance root. Reproduction "
        "commands are documented in ../tests/DJANGO_ACCEPTANCE.md and the "
        "exporter accepts `--help`. Generated projections and build caches "
        "are intentionally absent.\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_subparsers(dest="action", required=True)
    generic = modes.add_parser(
        "run-generic",
        help="Run the unchanged Rust suite with before/after fingerprints",
    )
    for name in ("companion", "official", "fork", "fixtures", "output"):
        generic.add_argument(f"--{name}", type=Path, required=True)
    generic.add_argument("--checker-mode", choices=GENERIC_MODES, required=True)
    generic.add_argument("--timeout", type=int, default=1800)
    exporter = modes.add_parser(
        "export", help="Refuse incomplete gates and write portable evidence"
    )
    for name in (
        "acceptance-root",
        "companion",
        "generic-cli",
        "generic-lsp",
        "generic-push",
        "unit-log",
        "protocol-log",
        "benchmark",
    ):
        exporter.add_argument(f"--{name}", type=Path, required=True)
    exporter.add_argument("--expected-unit-tests", type=int, required=True)
    exporter.add_argument(
        "--supplemental-json", action="append", default=[], metavar="LABEL=PATH"
    )
    exporter.add_argument(
        "--supplemental-log", action="append", default=[], metavar="LABEL=PATH"
    )
    exporter.add_argument(
        "--path-label",
        action="append",
        default=[],
        metavar="LABEL=PATH",
        help="Label an existing artifact path explicitly; supplemental reports never label their parents",
    )
    exporter.add_argument(
        "--output", type=Path, default=Path(__file__).resolve().parents[1] / "evidence"
    )
    for subparser in (generic, exporter):
        subparser.add_argument(
            "--source-root", type=Path, default=Path(__file__).resolve().parents[1]
        )
    args = parser.parse_args()
    for name, value in vars(args).items():
        if isinstance(value, Path):
            setattr(args, name, value.resolve())
    try:
        if args.action == "run-generic":
            return run_generic(args)
        require(args.expected_unit_tests > 0, "unit test count must be positive")
        collect(args)
    except (Failure, OSError, ValueError, KeyError, TypeError) as error:
        print(str(error), file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
