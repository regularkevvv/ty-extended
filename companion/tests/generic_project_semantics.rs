//! Run every existing generic acceptance fixture through production project mode.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn normalize(diagnostics: &Value, root: &Path) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    for diagnostic in diagnostics.as_array().context("diagnostic array")? {
        let mut location = diagnostic["location"].clone();
        let path = Path::new(location["path"].as_str().context("diagnostic path")?);
        location["path"] = json!(path.strip_prefix(root).unwrap_or(path));
        values.push(json!({"code":diagnostic["check_name"],"description":diagnostic["description"],"severity":diagnostic["severity"],"location":location}));
    }
    values.sort_by_key(Value::to_string);
    Ok(values)
}

#[test]
#[ignore = "requires TY_COMPANION_TEST_FORK and TY_COMPANION_TEST_OFFICIAL"]
fn existing_generic_corpus_passes_through_project_frontend() -> Result<()> {
    let fork = std::env::var("TY_COMPANION_TEST_FORK")?;
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
    let output = std::env::var_os("TY_COMPANION_GENERIC_PROJECT_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("native-generic-project-{}", std::process::id()))
        });
    let mut cases: Vec<_> = fs::read_dir(fixtures.join("audit-final"))?
        .map(|entry| Ok(entry?.path().join("fork")))
        .collect::<Result<_>>()?;
    cases.extend([
        fixtures.join("benchmark/fork"),
        fixtures.join("scaling/10/fork"),
        fixtures.join("scaling/100/fork"),
    ]);
    cases.sort();
    ensure!(
        cases.len() == 15,
        "fixture corpus changed; audit expected cases"
    );
    for (index, fixture) in cases.iter().enumerate() {
        let case = output.join(index.to_string());
        let root = case.join("project");
        let stubs = case.join("stubs");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(&stubs)?;
        fs::copy(fixture.join("demo.py"), root.join("demo.py"))?;
        let plugin = fixture.join("plugin.py");
        let manifest = fixture.join("plugin.plugin.json");
        let entry = format!(
            "plugins.plugin = [{{id=\"research.generic\",runtime=\"monty\",trusted=true,path={},manifest-path={}}}]",
            serde_json::to_string(&plugin)?,
            serde_json::to_string(&manifest)?
        );
        let baseline = Command::new(&fork)
            .args([
                "check",
                "demo.py",
                "--python-version",
                "3.13",
                "--output-format",
                "gitlab",
                "--config",
                "plugins.enabled=true",
                "--config",
                "plugins.auto-discover=false",
                "--config",
            ])
            .arg(entry)
            .current_dir(&root)
            .env("TY_UV", "0")
            .env("TY_MAX_PARALLELISM", "1")
            .output()?;
        fs::write(case.join("fork.stdout"), &baseline.stdout)?;
        fs::write(case.join("fork.stderr"), &baseline.stderr)?;
        ensure!(
            matches!(baseline.status.code(), Some(0 | 1)),
            "fork failed {}: {}",
            fixture.display(),
            String::from_utf8_lossy(&baseline.stderr)
        );
        let expected = normalize(&serde_json::from_slice(&baseline.stdout)?, &root)?;
        for mode in ["cli", "push"] {
            let result = Command::new(env!("CARGO_BIN_EXE_ty-companion-rust"))
                .arg("project")
                .arg(&root)
                .arg("--manifest")
                .arg(&manifest)
                .arg("--plugin")
                .arg(&plugin)
                .arg("--stubs")
                .arg(&stubs)
                .arg("--ty")
                .arg(&official)
                .arg("--out")
                .arg(case.join(format!("projection-{mode}")))
                .args(["--python-version", "3.13", "--checker-mode", mode])
                .current_dir(&root)
                .env("TY_UV", "0")
                .env("TY_MAX_PARALLELISM", "1")
                .output()?;
            fs::write(case.join(format!("{mode}.stdout")), &result.stdout)?;
            fs::write(case.join(format!("{mode}.stderr")), &result.stderr)?;
            ensure!(
                result.status.code() == baseline.status.code(),
                "{} {mode} exit mismatch: {}",
                fixture.display(),
                String::from_utf8_lossy(&result.stderr)
            );
            let response: Value = serde_json::from_slice(&result.stdout)?;
            let actual = normalize(&response["diagnostics"], &root)?;
            ensure!(
                actual == expected,
                "{} {mode} mismatch; evidence {}\nexpected={expected:#?}\nactual={actual:#?}",
                fixture.display(),
                case.display()
            );
        }
    }
    Ok(())
}
