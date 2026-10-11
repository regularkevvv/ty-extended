mod checker;
mod cli;
mod cli_edits;
mod editor_options;
mod flow;
mod lower;
mod lsp_mapping;
mod lsp_server;
mod mapping;
mod model;
mod notebook;
mod project;
mod project_check;
mod runtime;
mod static_project;
mod type_render;
mod verify;

use anyhow::{Context, Result, bail};
use mapping::{Edits, map_diagnostics, render};
use model::{Analysis, Oracle, ReturnPatches, Source};
use runtime::{Runtime, call_request};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Instant,
};

fn class_edits(source: &Source, analysis: &Analysis, edits: &mut Edits) {
    for info in analysis.classes.values() {
        let mut additions = String::new();
        for (name, expression) in &info.fields {
            if !info.declared.contains(name) {
                additions.push_str(&format!("{name}: {expression}\n{}", info.indent));
            }
        }
        if !additions.is_empty() {
            edits.insert_with_origin(info.insertion, additions, info.origin);
        }
    }
    let _ = source;
}

fn build_edits(
    source: &Source,
    analysis: &Analysis,
    flow_edits: Edits,
    patches: &ReturnPatches,
    probes: bool,
) -> Edits {
    let mut edits = analysis.edits.clone();
    edits.extend(flow_edits);
    class_edits(source, analysis, &mut edits);
    // Probe identities come from generated coordinates. One hygienic binding
    // avoids asking ty to resolve thousands of identical imported aliases.
    let probe_name = format!("{}probe", source.prefix);
    if probes {
        for call in analysis.calls.values() {
            if let Some(receiver) = call.receiver_span {
                edits.wrap(
                    receiver,
                    format!("{probe_name}("),
                    ")".into(),
                    Some(call.receiver_key()),
                );
            }
            edits.wrap(
                call.span,
                format!("{probe_name}("),
                ")".to_owned(),
                Some(call.return_key()),
            );
        }
        for (&span, key) in &analysis.expression_probes {
            edits.wrap(
                span,
                format!("{probe_name}("),
                ")".into(),
                Some(key.clone()),
            );
        }
    }
    for (&s, expression) in patches {
        edits.annotate_type(s, expression.clone());
        edits.wrap(
            s,
            format!("{}({expression}, ", source.cast_name()),
            ")".to_owned(),
            None,
        );
    }
    if probes {
        for call in analysis.calls.values() {
            for (index, arg) in call.arguments.iter().enumerate() {
                edits.wrap(
                    arg.span,
                    format!("{probe_name}("),
                    ")".to_owned(),
                    Some(call.argument_key(index)),
                );
            }
        }
        if !analysis.calls.is_empty() || !analysis.expression_probes.is_empty() {
            edits.insert(
                analysis.prelude_offset,
                format!(
                    "from typing_extensions import reveal_type as {probe_name}\n{probe_name}(None)\n"
                ),
            );
        }
    }
    edits
}

fn check(input: &Path, options: &BTreeMap<String, String>) -> Result<Value> {
    let started = Instant::now();
    let value = |key: &str| options.get(key).with_context(|| format!("missing --{key}"));
    let output_dir = PathBuf::from(value("out")?);
    std::fs::create_dir_all(&output_dir)?;
    let input = std::fs::canonicalize(input)?;
    let source = Source::new(
        std::fs::read_to_string(&input)?,
        input.to_string_lossy().into_owned(),
        options
            .get("module")
            .cloned()
            .unwrap_or_else(|| "demo".into()),
    );
    let checker_path = PathBuf::from(value("ty")?);
    let checker_mode = options
        .get("checker-mode")
        .map(String::as_str)
        .unwrap_or("cli");
    if !matches!(checker_mode, "cli" | "lsp" | "push") {
        bail!("--checker-mode must be cli, lsp or push");
    }
    let trace_queries = options
        .get("trace-queries")
        .is_some_and(|value| value == "true");
    let mut stages = BTreeMap::<String, f64>::new();
    let mut tick = Instant::now();
    let mut runtime = Runtime::new(Path::new(value("manifest")?), Path::new(value("plugin")?))?;
    stages.insert("runtime_init".into(), tick.elapsed().as_secs_f64());
    tick = Instant::now();
    let parsed = ruff_python_parser::parse_module(&source.text).context("parse source")?;
    let mut analysis = static_project::analyze(&source, &parsed.syntax().body, &mut runtime)?;
    stages.insert("parse_and_static".into(), tick.elapsed().as_secs_f64());
    let mut checker = if checker_mode == "push" {
        checker::Checker::new_push(&checker_path, &output_dir)?
    } else {
        checker::Checker::new(&checker_path, &output_dir, checker_mode == "lsp")?
    };
    stages.insert("checker_init".into(), checker.startup_seconds);
    let mut patches = ReturnPatches::new();
    // Flow may add member-hook fields containing per-pass generated imports.
    // Rebuild those fields with each renderer instead of retaining stale aliases.
    let static_classes = analysis.classes.clone();
    let mut previous = None;
    let mut oracle_rounds = Vec::new();
    let mut oracle_seconds = 0.0;
    let mut frontend_seconds = 0.0;
    let mut converged = false;
    for iteration in 0..8 {
        tick = Instant::now();
        analysis.classes = static_classes.clone();
        let flow = flow::project(&source, &parsed.syntax().body, &mut analysis, &mut runtime)?;
        let mut edits = build_edits(&source, &analysis, flow.edits, &patches, true);
        edits.insert(analysis.prelude_offset, flow.type_prelude);
        let generated = render(&source, &edits)?;
        let mut oracle = Oracle::new();
        let mut non_oracle = Vec::new();
        frontend_seconds += tick.elapsed().as_secs_f64();
        if !analysis.calls.is_empty() {
            let path = output_dir.join(format!("oracle-{iteration}.py"));
            // CLI transport materializes its own input. The persistent server
            // receives source in memory; disk snapshots are optional evidence.
            if checker_mode != "cli" && trace_queries {
                std::fs::write(&path, &generated.text)?;
            }
            tick = Instant::now();
            let diagnostics = checker.check(&path, &generated.text)?;
            mapping::validate_probe_health(&source, &generated, &diagnostics)?;
            oracle_seconds += tick.elapsed().as_secs_f64();
            for diagnostic in diagnostics {
                if diagnostic["check_name"] == "revealed-type" {
                    let pos = &diagnostic["location"]["positions"]["begin"];
                    let coordinate = (
                        pos["line"].as_u64().unwrap_or(0) as usize,
                        pos["column"].as_u64().unwrap_or(0) as usize,
                    );
                    if let Some(key) = generated.probes.get(&coordinate) {
                        let description = diagnostic["description"]
                            .as_str()
                            .context("reveal description")?;
                        let start = description
                            .find("Revealed type: `")
                            .context("reveal type format")?
                            + "Revealed type: `".len();
                        let end = description.rfind('`').context("reveal type end")?;
                        oracle.insert(key.clone(), description[start..end].to_owned());
                        continue;
                    }
                }
                non_oracle.push(diagnostic);
            }
        }
        tick = Instant::now();
        let mut next = ReturnPatches::new();
        for call in analysis.calls.values() {
            let patch = call_request(
                &mut runtime,
                &source,
                "adjust-call-return",
                call,
                Some(&oracle),
                None,
            )?;
            if patch["kind"] == "call-return-patch" {
                let expression = patch["return-type"]["expression"]
                    .as_str()
                    .context("return type expression")?;
                ruff_python_parser::parse_expression(expression)
                    .context("unsupported inferred return expression")?;
                next.insert(call.span, expression.to_owned());
            }
        }
        oracle_rounds
            .push(json!({"round":iteration,"types":oracle,"non_oracle_diagnostics":non_oracle}));
        let stable = previous.as_ref() == Some(&next);
        previous = Some(next.clone());
        patches = next;
        frontend_seconds += tick.elapsed().as_secs_f64();
        if stable {
            converged = true;
            break;
        }
    }
    if !converged {
        bail!("projection did not converge within 8 rounds");
    }
    tick = Instant::now();
    analysis.classes = static_classes;
    let flow = flow::project(&source, &parsed.syntax().body, &mut analysis, &mut runtime)?;
    let mut edits = build_edits(&source, &analysis, flow.edits, &patches, false);
    edits.insert(analysis.prelude_offset, flow.type_prelude);
    let generated = render(&source, &edits)?;
    let final_path = output_dir.join("projected.py");
    std::fs::write(&final_path, &generated.text)?;
    stages.insert("final_render".into(), tick.elapsed().as_secs_f64());
    tick = Instant::now();
    let raw = checker.check(&final_path, &generated.text)?;
    stages.insert("final_ty_check".into(), tick.elapsed().as_secs_f64());
    tick = Instant::now();
    let (diagnostics, suppressed) = map_diagnostics(
        &source,
        &generated,
        &raw,
        &analysis.helpers,
        &flow.diagnostics,
    )?;
    stages.insert("diagnostic_mapping".into(), tick.elapsed().as_secs_f64());
    stages.insert("frontend_iterations".into(), frontend_seconds);
    stages.insert("oracle_checks".into(), oracle_seconds);
    // Runtime execution is a nested counter, not additive to the stages above.
    let result = json!({"diagnostics":diagnostics,"raw_diagnostics":raw,"suppressed_generated":suppressed,
        "oracle_rounds":oracle_rounds.len(),"monty_executions":runtime.executions,"monty_cache_hits":runtime.hits,
        "monty_execution_seconds":runtime.execution_time.as_secs_f64(),"stage_seconds":stages,
        "total_seconds":started.elapsed().as_secs_f64(),"implementation":"rust","plugin_runtime":"embedded-monty",
        "checker":checker_path,"checker_mode":checker_mode,"checker_queries":checker.queries,"original_source":input});
    std::fs::write(
        output_dir.join("rounds.json"),
        serde_json::to_vec_pretty(&oracle_rounds)?,
    )?;
    std::fs::write(
        output_dir.join("results.json"),
        serde_json::to_vec_pretty(&result)?,
    )?;
    Ok(result)
}

fn main() -> std::process::ExitCode {
    let arguments: Vec<_> = std::env::args().skip(1).collect();
    if arguments.is_empty()
        || arguments
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print!("{}", cli::HELP);
        return std::process::ExitCode::SUCCESS;
    }
    if arguments
        .first()
        .is_some_and(|arg| arg == "--version" || arg == "-V")
    {
        println!("ty-companion-rust {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }
    if arguments.first().is_some_and(|arg| arg == "check")
        && !arguments.iter().any(|arg| arg == "--out")
    {
        return match cli::check(&arguments[1..]) {
            Ok(code) => std::process::ExitCode::from(code as u8),
            Err(error) => {
                eprintln!("{error:#}");
                std::process::ExitCode::from(2)
            }
        };
    }
    if arguments.first().is_some_and(|arg| arg == "server") {
        let mut index = 1;
        let root = arguments
            .get(index)
            .filter(|arg| !arg.starts_with("--"))
            .map(PathBuf::from);
        if root.is_some() {
            index += 1;
        }
        let mut options = BTreeMap::new();
        while index < arguments.len() {
            if arguments[index] == "--stdio" {
                index += 1;
                continue;
            }
            if !arguments[index].starts_with("--") || index + 1 >= arguments.len() {
                eprintln!("server options require --NAME VALUE");
                return std::process::ExitCode::from(2);
            }
            options.insert(arguments[index][2..].into(), arguments[index + 1].clone());
            index += 2;
        }
        return match lsp_server::run(root.as_deref(), options) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error:#}");
                std::process::ExitCode::from(2)
            }
        };
    }
    let run = || -> Result<(Value, bool)> {
        let arguments: Vec<_> = std::env::args().skip(1).collect();
        if arguments.first().map(String::as_str) == Some("suite") {
            let usage = "suite FIXTURES OUTPUT_DIR MODE OFFICIAL_TY REFERENCE_TY";
            let root = arguments.get(1).context(usage)?;
            let out = arguments.get(2).context(usage)?;
            let mode = arguments.get(3).context(usage)?;
            let official = arguments.get(4).context(usage)?;
            let reference = arguments.get(5).context(usage)?;
            return Ok((
                verify::run(
                    &std::env::current_exe()?,
                    Path::new(root),
                    Path::new(out),
                    mode,
                    Path::new(official),
                    Path::new(reference),
                )?,
                false,
            ));
        }
        if !matches!(
            arguments.first().map(String::as_str),
            Some("check" | "project")
        ) || arguments.len() < 2
        {
            bail!(
                "Usage: ty-companion-rust check SOURCE --plugin FILE --manifest FILE --ty BINARY --out DIRECTORY [--module NAME] | project ROOT --plugin FILE --manifest FILE --stubs DIRECTORY --ty BINARY --out DIRECTORY | suite FIXTURES OUTPUT_DIR MODE OFFICIAL_TY REFERENCE_TY"
            );
        }
        let mut options = BTreeMap::new();
        for pair in arguments[2..].chunks(2) {
            if pair.len() != 2 || !pair[0].starts_with("--") {
                bail!("invalid option");
            }
            options.insert(pair[0][2..].to_owned(), pair[1].clone());
        }
        let result = if arguments[0] == "project" {
            project_check::run(Path::new(&arguments[1]), &options)?
        } else {
            check(Path::new(&arguments[1]), &options)?
        };
        let has_errors = result["diagnostics"].as_array().is_some_and(|d| {
            d.iter()
                .any(|d| matches!(d["severity"].as_str(), Some("minor" | "major" | "critical")))
        });
        Ok((result, has_errors))
    };
    match run() {
        Ok((result, error)) => {
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
            std::process::ExitCode::from(u8::from(error))
        }
        Err(error) => {
            eprintln!("{error:#}");
            std::process::ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::ensure;

    #[test]
    fn legacy_member_and_state_imports_survive_repeated_flow_passes() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")
            .context("TY_COMPANION_TEST_OFFICIAL must name official ty")?;
        let directory =
            std::env::temp_dir().join(format!("ty-native-legacy-imports-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let source = directory.join("demo.py");
        std::fs::write(
            &source,
            "from typing import assert_type\nclass Resource:\n    status: str | None = None\n    def activate(self) -> None: pass\ndef example() -> None:\n    item = Resource()\n    assert_type(item.token, bytes)\n    item.activate()\n    assert_type(item.status, str)\n",
        )?;
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
        let options = BTreeMap::from([
            ("ty".into(), binary),
            (
                "out".into(),
                directory.join("projection").display().to_string(),
            ),
            (
                "plugin".into(),
                fixture.join("plugin.py").display().to_string(),
            ),
            (
                "manifest".into(),
                fixture.join("plugin.plugin.json").display().to_string(),
            ),
            ("checker-mode".into(), "cli".into()),
        ]);
        let result = check(&source, &options)?;
        assert_eq!(result["diagnostics"], json!([]), "{result:#}");
        assert!(result["oracle_rounds"].as_u64().unwrap() >= 2);
        assert!(result["monty_executions"].as_u64().unwrap() > 0);
        let rendered = std::fs::read_to_string(directory.join("projection/projected.py"))?;
        assert!(rendered.contains("import builtins as "));
        assert!(rendered.contains(".bytes"));
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }

    #[test]
    fn generated_type_probes_work_on_oldest_and_newest_python_targets() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")
            .context("TY_COMPANION_TEST_OFFICIAL must name official ty")?;
        let directory =
            std::env::temp_dir().join(format!("ty-native-probe-targets-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let source = Source::new(
            "value = 42\n".into(),
            directory.join("original.py").to_string_lossy().into_owned(),
            "original".into(),
        );
        let start = source.text.find("42").unwrap();
        let analysis = Analysis {
            expression_probes: BTreeMap::from([(
                model::Span {
                    start,
                    end: start + 2,
                },
                "value".into(),
            )]),
            ..Analysis::default()
        };
        let edits = build_edits(
            &source,
            &analysis,
            Edits::default(),
            &ReturnPatches::new(),
            true,
        );
        let generated = render(&source, &edits)?;
        let path = directory.join("generated.py");
        std::fs::write(&path, &generated.text)?;
        for version in ["3.10", "3.14"] {
            let output = std::process::Command::new(&binary)
                .args([
                    "check",
                    "--python-version",
                    version,
                    "--output-format",
                    "gitlab",
                ])
                .arg(&path)
                .current_dir(&directory)
                .env("TY_UV", "0")
                .output()?;
            ensure!(
                matches!(output.status.code(), Some(0 | 1)),
                "checker failed: {output:?}"
            );
            let diagnostics: Vec<Value> = serde_json::from_slice(&output.stdout)?;
            mapping::validate_probe_health(&source, &generated, &diagnostics)?;
            assert_eq!(diagnostics.len(), 2, "Python {version}: {diagnostics:?}");
            assert!(
                diagnostics
                    .iter()
                    .all(|item| item["check_name"] == "revealed-type")
            );
            assert!(diagnostics.iter().any(|item| {
                item["description"] == "revealed-type: Revealed type: `Literal[42]`"
            }));
        }
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
