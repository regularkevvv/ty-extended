//! Differential coverage for direct hooks and Python object-state scopes.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

mod support;
use support::normalize;

#[test]
#[ignore = "requires TY_COMPANION_TEST_FORK and TY_COMPANION_TEST_OFFICIAL"]
fn dynamic_hooks_and_python_scopes_match_the_fork() -> Result<()> {
    let fork = std::env::var("TY_COMPANION_TEST_FORK").context("TY_COMPANION_TEST_FORK")?;
    let official =
        std::env::var("TY_COMPANION_TEST_OFFICIAL").context("TY_COMPANION_TEST_OFFICIAL")?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sdk_extended");
    let output = std::env::var_os("TY_COMPANION_EXTENDED_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("native-sdk-extended-{}", std::process::id()))
        });
    let root = output.join("project");
    let stubs = output.join("stubs");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&stubs)?;
    fs::copy(fixture.join("demo.py"), root.join("demo.py"))?;
    let plugin = fixture.join("plugin.py");
    let manifest = fixture.join("plugin.plugin.json");
    let entry = format!(
        "plugins.plugin = [{{id=\"research.sdk_extended\",runtime=\"monty\",trusted=true,path={},manifest-path={}}}]",
        serde_json::to_string(&plugin)?,
        serde_json::to_string(&manifest)?
    );
    let baseline = Command::new(&fork)
        // Resolve both paths against the same OS working directory. On Windows,
        // `canonicalize` uses a verbatim prefix, while `demo.py` is resolved by
        // the reference checker against its ordinary `current_dir` spelling.
        .args(["check", "demo.py", "--project", "."])
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
        "fork must report the two deliberate mutation diagnostics: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let expected = normalize(&serde_json::from_slice(&baseline.stdout)?, &root)?;
    ensure!(
        expected.len() == 2,
        "unexpected baseline: {expected:#?}\nstderr: {}\nevidence: {}",
        String::from_utf8_lossy(&baseline.stderr),
        output.display()
    );
    let lines: Vec<_> = expected
        .iter()
        .map(|d| {
            d["location"]["positions"]["begin"]["line"]
                .as_u64()
                .unwrap()
        })
        .collect();
    ensure!(
        lines.iter().all(|line| [52, 53].contains(line)),
        "all hook and Python-scope assertions must pass: {expected:#?}"
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
