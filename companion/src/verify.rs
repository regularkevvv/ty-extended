//! Native validation and benchmark driver. It starts only the Rust companion
//! executable and the two checker binaries; Python is fixture input, never a
//! driver or transformation dependency.
use anyhow::{Context, Result, bail, ensure};
use ruff_python_ast::{
    Expr,
    visitor::{self, Visitor},
};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
    time::Instant,
};

struct Run {
    value: Value,
    wall: f64,
    exit: i32,
}

fn read_output(output: Output, wall: f64, label: &str) -> Result<Run> {
    let exit = output.status.code().context("checker killed by signal")?;
    ensure!(
        [0, 1].contains(&exit),
        "{label} failed with exit {exit}: {}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    let value = serde_json::from_slice(&output.stdout).with_context(|| {
        format!(
            "{label} returned invalid JSON: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    Ok(Run { value, wall, exit })
}

fn fork_run(binary: &Path, source: &Path) -> Result<Run> {
    let start = Instant::now();
    let output = Command::new(binary)
        .args([
            "check",
            "--python-version",
            "3.13",
            "--output-format",
            "gitlab",
        ])
        .arg(source)
        .current_dir(source.parent().context("source has no parent")?)
        .env("TY_MAX_PARALLELISM", "1")
        .output()
        .context("start reference fork")?;
    read_output(output, start.elapsed().as_secs_f64(), "reference fork")
}

fn companion_command(
    binary: &Path,
    source: &Path,
    plugin: &Path,
    manifest: &Path,
    official: &Path,
    out: &Path,
    checker_mode: &str,
) -> Command {
    let mut command = Command::new(binary);
    command
        .arg("check")
        .arg(source)
        .arg("--plugin")
        .arg(plugin)
        .arg("--manifest")
        .arg(manifest)
        .arg("--ty")
        .arg(official)
        .arg("--out")
        .arg(out)
        .args(["--module", "demo"])
        .args(["--checker-mode", checker_mode])
        .current_dir(source.parent().unwrap())
        .env("TY_MAX_PARALLELISM", "1");
    command
}

fn companion_run(
    binary: &Path,
    source: &Path,
    plugin: &Path,
    manifest: &Path,
    official: &Path,
    out: &Path,
    checker_mode: &str,
) -> Result<Run> {
    fs::create_dir_all(out)?;
    let start = Instant::now();
    let output = companion_command(
        binary,
        source,
        plugin,
        manifest,
        official,
        out,
        checker_mode,
    )
    .output()
    .context("start Rust companion")?;
    read_output(output, start.elapsed().as_secs_f64(), "Rust companion")
}

/// Paths and fingerprint hashes differ between processes. Require diagnostic
/// codes and original line/scalar-column locations to agree, plus exact inferred type text
/// for reveal_type diagnostics. Non-reveal wording can name a generated helper.
fn signature(diagnostics: &Value) -> Result<Vec<String>> {
    let mut signature = Vec::new();
    for diagnostic in diagnostics
        .as_array()
        .context("diagnostics is not an array")?
    {
        let code = diagnostic["check_name"]
            .as_str()
            .context("diagnostic lacks check_name")?;
        let positions = diagnostic
            .pointer("/location/positions")
            .context("diagnostic lacks positions")?;
        let reveal = if code == "revealed-type" {
            diagnostic["description"]
                .as_str()
                .context("reveal diagnostic lacks description")?
                .split_once("Revealed type: ")
                .map(|(_, suffix)| suffix)
                .context("unrecognized reveal diagnostic")?
        } else {
            ""
        };
        signature.push(serde_json::to_string(
            &json!({"code":code,"positions":positions,"revealed_type":reveal}),
        )?);
    }
    signature.sort();
    Ok(signature)
}

fn validate(reference: &Run, companion: &Run) -> Result<bool> {
    Ok(
        signature(&reference.value)? == signature(&companion.value["diagnostics"])?
            && reference.exit == companion.exit,
    )
}

fn save(out: &Path, report: &Value) -> Result<()> {
    fs::write(
        out.join("results.json"),
        serde_json::to_string_pretty(report)? + "\n",
    )?;
    Ok(())
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let n = values.len();
    if n.is_multiple_of(2) {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    } else {
        values[n / 2]
    }
}

fn assertion_count(text: &str) -> Result<usize> {
    struct Count(usize);
    impl<'a> Visitor<'a> for Count {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if matches!(expr, Expr::Call(call) if matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "assert_type"))
            {
                self.0 += 1;
            }
            visitor::walk_expr(self, expr);
        }
    }
    let parsed = ruff_python_parser::parse_module(text)?;
    let mut count = Count(0);
    count.visit_body(&parsed.syntax().body);
    Ok(count.0)
}

pub fn run(
    binary: &Path,
    fixtures: &Path,
    out: &Path,
    checker_mode: &str,
    official: &Path,
    fork: &Path,
) -> Result<Value> {
    ensure!(
        matches!(checker_mode, "cli" | "lsp" | "push"),
        "unknown checker mode: {checker_mode}"
    );
    fs::create_dir_all(out)?;
    let start = Instant::now();
    let semantic = fixtures;
    let plugin = semantic.join("plugin.py");
    let manifest = semantic.join("plugin.plugin.json");
    for artifact in [binary, plugin.as_path(), manifest.as_path(), official, fork] {
        ensure!(
            artifact.is_file(),
            "missing required executable/fixture: {}",
            artifact.display()
        );
    }
    let mut report = json!({
        "status":"running", "driver":"Rust std::process::Command; no Python driver or frontend",
        "parallelism":1, "warmup_runs":1, "measured_repetitions":5,
        "checker_mode":checker_mode,
        "companion_binary":binary,"official_binary":official,"fork_binary":fork,
        "companion_binary_bytes":fs::metadata(binary)?.len(),
        "validation":"Exact diagnostic codes and original spans, inferred reveal_type text, and exit codes on every run",
        "differentials":[], "unsupported_controls":[], "benchmarks":[]
    });
    save(out, &report)?;

    let mut cases = vec![(
        "original".to_owned(),
        semantic.join("benchmark/fork/demo.py"),
    )];
    let mut audit_paths: Vec<_> = fs::read_dir(semantic.join("audit-final"))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.join("fork/demo.py").is_file())
        .collect();
    audit_paths.sort();
    ensure!(
        audit_paths.len() == 12,
        "expected 12 adversarial fixtures, found {}",
        audit_paths.len()
    );
    for path in audit_paths {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        cases.push((name, path.join("fork/demo.py")));
    }
    let original = fs::read_to_string(semantic.join("benchmark/fork/demo.py"))?;
    let unicode = original
        .replace("    decode(42)", "    café = \"🌍\"; decode(42)")
        .replace(
            "    result: str = decode(\"42\")",
            "    café = \"🌍\"; result: str = decode(\"42\")",
        );
    for (name, text) in [
        ("unicode_crlf", unicode.replace('\n', "\r\n")),
        ("unicode_cr", unicode.replace('\n', "\r")),
        (
            "comment_suppression",
            original.replace(
                "    decode(42)",
                "    decode(42)  # ty: ignore[invalid-argument-type]",
            ),
        ),
        (
            "unused_awaitable_final_check",
            "from types import CoroutineType\nfrom typing import Any\n\ndef pending() -> CoroutineType[Any, Any, int]:\n    raise NotImplementedError\n\npending()\n".to_owned(),
        ),
    ] {
        let folder = out.join("extra-fixtures").join(name).join("fork");
        fs::create_dir_all(&folder)?;
        let source = folder.join("demo.py");
        fs::write(&source, text)?;
        for file in ["plugin.py", "plugin.plugin.json", "ty.toml"] {
            fs::copy(
                semantic.join("benchmark/fork").join(file),
                folder.join(file),
            )?;
        }
        cases.push((name.to_owned(), source));
    }
    for (name, source) in cases {
        let reference =
            fork_run(fork, &source).with_context(|| format!("reference case {name}"))?;
        let projected = companion_run(
            binary,
            &source,
            &plugin,
            &manifest,
            official,
            &out.join("differential").join(&name),
            checker_mode,
        )
        .with_context(|| format!("companion case {name}"))?;
        let matches = validate(&reference, &projected)?;
        if name == "comment_suppression" {
            ensure!(
                reference
                    .value
                    .as_array()
                    .is_some_and(|values| values.len() == 1
                        && values[0]["check_name"] == "invalid-assignment"),
                "comment suppression reference must retain exactly the other negative control"
            );
        }
        if name == "unused_awaitable_final_check" {
            ensure!(
                reference.value.as_array().is_some_and(
                    |values| values.len() == 1 && values[0]["check_name"] == "unused-awaitable"
                ),
                "final-check control must retain unused-awaitable; generated type probes suppress this warning"
            );
        }
        report["differentials"].as_array_mut().unwrap().push(json!({
            "case":name,"source":source,"same_codes_original_spans_and_revealed_types":matches,
            "fork_diagnostics":reference.value,"companion":projected.value,
            "fork_wall_seconds":reference.wall,"companion_wall_seconds":projected.wall
        }));
        save(out, &report)?;
        ensure!(
            matches,
            "differential mismatch in {name}; saved evidence in {}",
            out.join("results.json").display()
        );
        eprintln!("verified {name}");
    }

    for (name, text) in [
        (
            "shadowed_assert_type",
            "def f(assert_type: object):\n    pass\n",
        ),
        (
            "shadowed_reveal_type",
            "def f(reveal_type: object):\n    pass\n",
        ),
        (
            "foreign_intrinsic_import",
            "from somewhere import assert_type\n",
        ),
        ("loop_graph", "for item in []:\n    pass\n"),
    ] {
        let folder = out.join("unsupported").join(name);
        fs::create_dir_all(&folder)?;
        let source = folder.join("demo.py");
        fs::write(&source, text)?;
        let output = companion_command(
            binary,
            &source,
            &plugin,
            &manifest,
            official,
            &folder.join("output"),
            checker_mode,
        )
        .output()?;
        let exit = output.status.code();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let rejected = exit == Some(2) && stderr.contains("prototype");
        report["unsupported_controls"]
            .as_array_mut()
            .unwrap()
            .push(json!({"case":name,"rejected":rejected,"exit":exit,"stderr":stderr}));
        save(out, &report)?;
        ensure!(
            rejected,
            "unsupported fixture {name} did not fail explicitly"
        );
    }

    for (name, source, expected_assertions) in [
        ("original", semantic.join("benchmark/fork/demo.py"), 14),
        ("10_copies", semantic.join("scaling/10/fork/demo.py"), 140),
        (
            "100_copies",
            semantic.join("scaling/100/fork/demo.py"),
            1400,
        ),
    ] {
        let text = fs::read_to_string(&source)?;
        let assertions = assertion_count(&text)?;
        ensure!(
            assertions == expected_assertions,
            "assertion count mismatch in {name}"
        );
        let mut fixture = json!({"fixture":name,"source":source,"source_lines":text.lines().count(),"assert_type_count":assertions,"runs":[]});
        let mut companion_times = Vec::new();
        let mut fork_times = Vec::new();
        for repetition in 0..6 {
            let folder = out
                .join("benchmark")
                .join(name)
                .join(format!("run-{repetition}"));
            let (reference, projected) = if repetition % 2 == 0 {
                let reference = fork_run(fork, &source)?;
                let projected = companion_run(
                    binary,
                    &source,
                    &plugin,
                    &manifest,
                    official,
                    &folder,
                    checker_mode,
                )?;
                (reference, projected)
            } else {
                let projected = companion_run(
                    binary,
                    &source,
                    &plugin,
                    &manifest,
                    official,
                    &folder,
                    checker_mode,
                )?;
                let reference = fork_run(fork, &source)?;
                (reference, projected)
            };
            let matches = validate(&reference, &projected)?;
            let codes: Vec<_> = reference
                .value
                .as_array()
                .context("reference diagnostics not array")?
                .iter()
                .map(|d| d["check_name"].as_str().unwrap_or(""))
                .collect();
            let expected_negatives = codes == ["invalid-argument-type", "invalid-assignment"];
            fixture["runs"].as_array_mut().unwrap().push(json!({
                "repetition":repetition,"warmup":repetition == 0,
                "execution_order":if repetition % 2 == 0 {"fork then companion"} else {"companion then fork"},
                "fork_wall_seconds":reference.wall,"companion_wall_seconds":projected.wall,
                "matching_code_spans_and_reveals":matches,"two_expected_negative_controls":expected_negatives,
                "fork_diagnostics":reference.value,"companion":projected.value
            }));
            if repetition > 0 {
                companion_times.push(projected.wall);
                fork_times.push(reference.wall);
            }
            if !matches || !expected_negatives {
                report["benchmarks"].as_array_mut().unwrap().push(fixture);
                save(out, &report)?;
                bail!("benchmark {name} repetition {repetition} failed correctness check");
            }
        }
        let companion_median = median(companion_times);
        let fork_median = median(fork_times);
        fixture["companion_median_wall_seconds"] = json!(companion_median);
        fixture["fork_median_wall_seconds"] = json!(fork_median);
        fixture["companion_to_fork_ratio"] = json!(companion_median / fork_median);
        report["benchmarks"].as_array_mut().unwrap().push(fixture);
        save(out, &report)?;
        eprintln!(
            "benchmarked {name}: native companion {companion_median:.4}s; fork {fork_median:.4}s"
        );
    }
    report["status"] = json!("passed");
    report["elapsed_seconds"] = json!(start.elapsed().as_secs_f64());
    report["scope"] = json!(
        "Bounded prototype fixtures, including 12 adversarial cases, Unicode with CRLF and CR newlines, comment suppression, and 4 explicit rejection controls; not the complete 194-test SDK compatibility gate. Scaling copies repeat the same 14 assertions rather than distinct semantic coverage. Complete process wall time includes Rust startup, source projection, all official ty queries, Monty execution, final type checking, and mapping."
    );
    save(out, &report)?;
    Ok(
        json!({"status":"passed","checker_mode":checker_mode,"differential_cases":report["differentials"].as_array().unwrap().len(),"unsupported_controls":4,"results":out.join("results.json"),"benchmarks":report["benchmarks"].as_array().unwrap().iter().map(|v|json!({"fixture":v["fixture"],"assert_type_count":v["assert_type_count"],"companion_median_wall_seconds":v["companion_median_wall_seconds"],"fork_median_wall_seconds":v["fork_median_wall_seconds"],"ratio":v["companion_to_fork_ratio"]})).collect::<Vec<_>>()}),
    )
}
