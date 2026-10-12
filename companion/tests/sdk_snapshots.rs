//! Structural snapshot precedence, nested bindings, Self, and record shapes.
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
fn structural_snapshot_contract_matches_the_fork() -> Result<()> {
    let fork = std::env::var("TY_COMPANION_TEST_FORK").context("TY_COMPANION_TEST_FORK")?;
    let official =
        std::env::var("TY_COMPANION_TEST_OFFICIAL").context("TY_COMPANION_TEST_OFFICIAL")?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sdk_snapshots");
    let output = std::env::var_os("TY_COMPANION_SNAPSHOTS_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("native-sdk-snapshots-{}", std::process::id()))
        });
    let root = output.join("project");
    let stubs = output.join("stubs");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&stubs)?;
    fs::copy(fixture.join("demo.py"), root.join("demo.py"))?;
    let plugin = fixture.join("plugin.py");
    let manifest = fixture.join("plugin.plugin.json");
    let entry = format!(
        "plugins.plugin = [{{id=\"audit.snapshots\",runtime=\"monty\",trusted=true,path={},manifest-path={}}}]",
        serde_json::to_string(&plugin)?,
        serde_json::to_string(&manifest)?
    );
    let baseline = Command::new(&fork)
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
        "fork must report the three deliberate record errors and one revealed type: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let expected = normalize(&serde_json::from_slice(&baseline.stdout)?, &root)?;
    ensure!(expected.len() == 7, "unexpected baseline: {expected:#?}");
    let lines: Vec<_> = expected
        .iter()
        .map(|d| {
            d["location"]["positions"]["begin"]["line"]
                .as_u64()
                .unwrap()
        })
        .collect();
    let source = fs::read_to_string(fixture.join("demo.py"))?;
    let diagnostic_lines = [
        "bad: str = row",
        "row[\"readonly\"] =",
        "row[\"unknown\"] =",
        "reveal_type(closed_record())",
        "reveal_type(codec_literal_bytes())",
        "reveal_type(codec_class())",
        "reveal_type(codec_class_queryset())",
    ]
    .into_iter()
    .map(|marker| {
        source
            .lines()
            .position(|line| line.contains(marker))
            .map(|line| line as u64 + 1)
            .context("diagnostic fixture marker")
    })
    .collect::<Result<Vec<_>>>()?;
    ensure!(
        lines.iter().all(|line| diagnostic_lines.contains(line)),
        "snapshot assertions must pass: {expected:#?}"
    );
    ensure!(
        expected
            .iter()
            .filter(|diagnostic| diagnostic["code"] == "invalid-assignment")
            .count()
            == 2
            && expected
                .iter()
                .filter(|diagnostic| diagnostic["code"] == "invalid-key")
                .count()
                == 1
            && expected
                .iter()
                .filter(|diagnostic| diagnostic["code"] == "revealed-type")
                .count()
                == 4,
        "snapshot contract diagnostics: {expected:#?}"
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
