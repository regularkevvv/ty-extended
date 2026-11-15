//! Class-transform fields preserve native class access and plugin instance access.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{fs, path::Path, process::Command};

mod support;
use support::normalize;

#[test]
#[ignore = "requires TY_COMPANION_TEST_FORK and TY_COMPANION_TEST_OFFICIAL"]
fn field_access_matches_historical_sdk_in_both_transports() -> Result<()> {
    let historical = std::env::var("TY_COMPANION_TEST_FORK").context("historical checker")?;
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL").context("official checker")?;
    let candidate = std::env::var("TY_COMPANION_TEST_CANDIDATE")
        .unwrap_or_else(|_| env!("CARGO_BIN_EXE_ty-extended").to_owned());
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sdk_fields");
    let output = std::env::temp_dir()
        .canonicalize()?
        .join(format!("ty-sdk-field-semantics-{}", std::process::id()));
    let root = output.join("project");
    fs::create_dir_all(&root)?;
    let source = root.join("demo.py");
    fs::copy(fixture.join("demo.py"), &source)?;
    let original = fs::read(&source)?;
    let modified = fs::metadata(&source)?.modified()?;
    let plugin = fixture.join("plugin.py");
    let manifest = fixture.join("plugin.plugin.json");
    fs::write(
        root.join("ty.toml"),
        format!(
            "[environment]\npython-version=\"3.13\"\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid=\"research.fields\"\nruntime=\"monty\"\ntrusted=true\npath={}\nmanifest-path={}\n",
            serde_json::to_string(&plugin)?,
            serde_json::to_string(&manifest)?
        ),
    )?;
    let check = |binary: &str, memory: Option<&str>| -> Result<_> {
        let mut command = Command::new(binary);
        command
            .args([
                "check",
                "demo.py",
                "--output-format",
                "gitlab",
                "--no-progress",
                "--color",
                "never",
            ])
            .current_dir(&root)
            .env("TY_UV", "0")
            .env("TY_MAX_PARALLELISM", "1");
        if let Some(memory) = memory {
            command
                .args(["--ty", &official])
                .env("TY_COMPANION_MEMORY_PROJECTION", memory);
        }
        Ok(command.output()?)
    };
    let baseline = check(&historical, None)?;
    fs::write(output.join("historical.stdout"), &baseline.stdout)?;
    fs::write(output.join("historical.stderr"), &baseline.stderr)?;
    ensure!(
        baseline.status.code() == Some(1),
        "historical controls: {}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let expected = normalize(&serde_json::from_slice(&baseline.stdout)?, &root)?;
    let text = std::str::from_utf8(&original)?;
    let controls: Vec<_> = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| line.contains("# control:").then_some(index as u64 + 1))
        .collect();
    ensure!(
        expected.len() == controls.len(),
        "historical contract assertions: {expected:#?}"
    );
    for diagnostic in &expected {
        ensure!(
            controls.contains(
                &diagnostic["location"]["positions"]["begin"]["line"]
                    .as_u64()
                    .unwrap()
            ),
            "historical positive assertion failed: {diagnostic:#?}"
        );
    }
    for memory in ["0", "1"] {
        let result = check(&candidate, Some(memory))?;
        fs::write(
            output.join(format!("memory-{memory}.stdout")),
            &result.stdout,
        )?;
        fs::write(
            output.join(format!("memory-{memory}.stderr")),
            &result.stderr,
        )?;
        ensure!(
            result.status.code() == Some(1),
            "memory={memory}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let diagnostics: Value = serde_json::from_slice(&result.stdout)?;
        let actual = normalize(&diagnostics, &root)?;
        ensure!(
            actual == expected,
            "memory={memory}; evidence={}\nexpected={expected:#?}\nactual={actual:#?}",
            output.display()
        );
        ensure!(fs::read(&source)? == original, "source bytes changed");
        ensure!(
            fs::metadata(&source)?.modified()? == modified,
            "source timestamp changed"
        );
    }
    fs::remove_dir_all(output)?;
    Ok(())
}
