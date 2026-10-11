//! Differential coverage for SDK virtual descriptors and inherited generics.
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
fn virtual_descriptors_and_inherited_generics_match_the_fork() -> Result<()> {
    let fork = std::env::var("TY_COMPANION_TEST_FORK").context("TY_COMPANION_TEST_FORK")?;
    let official =
        std::env::var("TY_COMPANION_TEST_OFFICIAL").context("TY_COMPANION_TEST_OFFICIAL")?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sdk");
    let output = std::env::var_os("TY_COMPANION_SDK_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("native-sdk-semantics-{}", std::process::id()))
        });
    let root = output.join("project");
    let stubs = output.join("stubs");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&stubs)?;
    fs::copy(fixture.join("demo.py"), root.join("demo.py"))?;
    let plugin = fixture.join("plugin.py");
    let manifest = fixture.join("plugin.plugin.json");
    let entry = format!(
        "plugins.plugin = [{{id=\"research.sdk\",runtime=\"monty\",trusted=true,path={},manifest-path={}}}]",
        serde_json::to_string(&plugin)?,
        serde_json::to_string(&manifest)?
    );
    let baseline = Command::new(&fork)
        .args(["check", "demo.py", "--project"])
        .arg(&root)
        .args([
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
    fs::write(output.join("fork.stdout"), &baseline.stdout)?;
    fs::write(output.join("fork.stderr"), &baseline.stderr)?;
    ensure!(
        baseline.status.code() == Some(1),
        "fork must report the five deliberate descriptor diagnostics: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let expected = normalize(&serde_json::from_slice(&baseline.stdout)?, &root)?;
    ensure!(expected.len() == 5, "unexpected baseline: {expected:#?}");
    let lines: Vec<_> = expected
        .iter()
        .map(|d| {
            d["location"]["positions"]["begin"]["line"]
                .as_u64()
                .unwrap()
        })
        .collect();
    ensure!(
        lines.iter().all(|line| [9, 12, 14, 15].contains(line)),
        "generic assertions must pass: {expected:#?}"
    );
    for mode in ["cli", "push"] {
        let result = Command::new(env!("CARGO_BIN_EXE_ty-companion-rust"))
            .arg("project")
            .arg(&root)
            .arg("--plugin")
            .arg(&plugin)
            .arg("--manifest")
            .arg(&manifest)
            .arg("--stubs")
            .arg(&stubs)
            .arg("--ty")
            .arg(&official)
            .arg("--out")
            .arg(output.join(format!("projection-{mode}")))
            .args(["--python-version", "3.13", "--checker-mode", mode])
            .current_dir(&root)
            .env("TY_UV", "0")
            .env("TY_MAX_PARALLELISM", "1")
            .output()?;
        fs::write(output.join(format!("{mode}.stdout")), &result.stdout)?;
        fs::write(output.join(format!("{mode}.stderr")), &result.stderr)?;
        ensure!(
            result.status.code() == baseline.status.code(),
            "{mode} status mismatch: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let response: Value = serde_json::from_slice(&result.stdout)?;
        let actual = normalize(&response["diagnostics"], &root)?;
        ensure!(
            actual == expected,
            "{mode} SDK mismatch; evidence {}\nexpected={expected:#?}\nactual={actual:#?}",
            output.display()
        );
    }
    Ok(())
}
