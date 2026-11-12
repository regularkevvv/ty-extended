//! Exercise the public CLI without research-only output directories or flags.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
fn automatic_memory_selection_preserves_explicit_hidden_notebooks() -> Result<()> {
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
    let root = std::env::temp_dir().join(format!("ty-explicit-notebook-{}", std::process::id()));
    fs::create_dir_all(root.join(".hidden"))?;
    let root = root.canonicalize()?;
    let notebook = root.join(".hidden/check.ipynb");
    let bytes = serde_json::to_vec(
        &json!({"cells":[{"cell_type":"code","execution_count":null,"metadata":{},"outputs":[],"source":["wrong: int = 'text'\n"]}],"metadata":{"language_info":{"name":"python","version":"3.13"}},"nbformat":4,"nbformat_minor":2}),
    )?;
    fs::write(&notebook, &bytes)?;
    let modified = fs::metadata(&notebook)?.modified()?;
    fs::write(
        root.join("ty.toml"),
        "[environment]\npython-version='3.13'\n[plugins]\nenabled=false\n",
    )?;
    for path in [&notebook, &root.join(".hidden")] {
        for memory in [None, Some("0")] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_ty-extended"));
            command
                .args(["check", "--ty", &official, "--output-format", "gitlab"])
                .arg(path)
                .current_dir(&root)
                .env("TY_UV", "0")
                .env_remove("TY_CONFIG_FILE")
                .env_remove("TY_OUTPUT_FORMAT")
                .env_remove("TY_COMPANION_MEMORY_PROJECTION");
            if let Some(memory) = memory {
                command.env("TY_COMPANION_MEMORY_PROJECTION", memory);
            }
            let output = command.output()?;
            let stderr = String::from_utf8(output.stderr)?;
            ensure!(output.status.code() == Some(1), "{stderr}");
            let diagnostics: Value = serde_json::from_slice(&output.stdout).context(stderr)?;
            ensure!(
                diagnostics.as_array().is_some_and(
                    |items| items.len() == 1 && items[0]["check_name"] == "invalid-assignment"
                ),
                "{diagnostics:#}"
            );
            ensure!(
                Path::new(
                    diagnostics[0]["location"]["path"]
                        .as_str()
                        .context("diagnostic path")?
                )
                .canonicalize()?
                    == notebook,
                "{diagnostics:#}"
            );
            ensure!(
                fs::read(&notebook)? == bytes && fs::metadata(&notebook)?.modified()? == modified,
                "original notebook changed"
            );
        }
    }
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
#[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
fn manifest_native_strategy_works_without_experimental_environment_flags() -> Result<()> {
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
    let temporary = std::env::temp_dir().canonicalize()?;
    let fixture = temporary.join(format!("ty-native-defaults-{}", std::process::id()));
    let root = fixture.join("project");
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(fixture.clone());
    fs::create_dir_all(&root)?;
    fs::create_dir_all(fixture.join("provider"))?;
    let provider = fixture.join("provider/api.pyi");
    fs::write(&provider, "class External: pass\n")?;
    let provider_bytes = fs::read(&provider)?;
    let provider_modified = fs::metadata(&provider)?.modified()?;
    fs::write(
        fixture.join("plugin.py"),
        "@on_instance_member\ndef resolve(request):\n    return member_response(member('extra', type_annotation('str')))\n",
    )?;
    fs::write(
        root.join("demo.py"),
        "from typing import assert_type\nclass Row: pass\nassert_type(Row().extra, str)\nwrong: int = 'ordinary native error'\n",
    )?;
    fs::write(
        root.join("ty.toml"),
        "[environment]\npython-version='3.13'\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid='test.defaults'\nruntime='monty'\ntrusted=true\npath='../plugin.py'\nmanifest-path='../plugin.json'\nstub-overlay-path='../provider'\n",
    )?;
    let mut manifest = json!({"id":"test.defaults","name":"Defaults","version":"0","protocol-version":{"major":0,"minor":9},"ty-compatibility":{"requirement":">=0.89.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"instance-member":true,"stub-overlays":true},"claims":{"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"demo.Row","attribute-name":"extra"}]},"stub-overlays":[{"module":"api","path":"provider/api.pyi"}],"default-config":{"companion-native-fields":true}});
    for (profile, fields, memory, borrow, native, warm, borrowed) in [
        (true, None, None, None, true, !cfg!(windows), !cfg!(windows)),
        (true, Some("0"), None, None, false, false, false),
        (false, None, None, None, false, false, false),
        (
            false,
            Some("1"),
            None,
            None,
            true,
            !cfg!(windows),
            !cfg!(windows),
        ),
        (true, None, Some("0"), None, true, false, false),
        (true, None, None, Some("0"), true, !cfg!(windows), false),
    ] {
        manifest["default-config"]["companion-native-fields"] = json!(profile);
        fs::write(fixture.join("plugin.json"), serde_json::to_vec(&manifest)?)?;
        let mut command = Command::new(env!("CARGO_BIN_EXE_ty-extended"));
        command
            .args(["check", "--ty", &official, "--output-format", "gitlab"])
            .env("TY_UV", "0")
            .env("TY_MAX_PARALLELISM", "1")
            .env("TY_COMPANION_TRACE_PHASES", "1")
            .env_remove("TY_CONFIG_FILE")
            .env_remove("TY_OUTPUT_FORMAT")
            .env_remove("TY_COMPANION_NATIVE_WARMUP")
            .env_remove("TY_COMPANION_NATIVE_FIELDS")
            .env_remove("TY_COMPANION_MEMORY_PROJECTION")
            .env_remove("TY_COMPANION_BORROW_OVERLAYS")
            .current_dir(&root);
        for (key, value) in [
            ("TY_COMPANION_NATIVE_FIELDS", fields),
            ("TY_COMPANION_MEMORY_PROJECTION", memory),
            ("TY_COMPANION_BORROW_OVERLAYS", borrow),
        ] {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        let child = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let prefix = format!("ty-companion-check-{}-", child.id());
        let output = child.wait_with_output()?;
        let stderr = String::from_utf8(output.stderr)?;
        ensure!(output.status.code() == Some(1), "{stderr}");
        let diagnostics: Value = serde_json::from_slice(&output.stdout).context(stderr.clone())?;
        ensure!(
            diagnostics.as_array().is_some_and(
                |items| items.len() == 1 && items[0]["check_name"] == "invalid-assignment"
            ),
            "{diagnostics:#}: {stderr}"
        );
        ensure!(
            stderr.contains(&format!("companion native fields: {native}")),
            "{stderr}"
        );
        ensure!(
            stderr.contains("companion native warmup initialize:") == warm,
            "{stderr}"
        );
        ensure!(
            stderr.contains(&format!("companion borrowed overlay: {borrowed}")),
            "{stderr}"
        );
        ensure!(
            fs::read(&provider)? == provider_bytes
                && fs::metadata(&provider)?.modified()? == provider_modified,
            "provider changed"
        );
        for entry in fs::read_dir(&temporary)? {
            ensure!(
                !entry?.file_name().to_string_lossy().starts_with(&prefix),
                "temporary analysis directory leaked"
            );
        }
    }
    Ok(())
}

#[test]
#[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
fn normal_check_discovers_plugins_respects_options_and_cleans_temporary_files() -> Result<()> {
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
    let temporary = std::env::temp_dir().canonicalize()?;
    let root = temporary.join(format!("ty-cli-acceptance-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let manifest = json!({"id":"test.cli","name":"CLI test","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Resource"}]}});
    fs::write(root.join("plugin.json"), serde_json::to_vec(&manifest)?)?;
    // The native host receives its manifest separately; no Python environment
    // or plugin package installation is needed for explicit registrations.
    fs::write(
        root.join("plugin.py"),
        "@on_class_transform\ndef transform(request):\n    return class_patch(fields=[field('extra', type_annotation('str'))])\n",
    )?;
    fs::write(
        root.join("demo.py"),
        "from typing import assert_type\nclass Resource: pass\nvalue = Resource()\nassert_type(value.extra, str)\n",
    )?;
    fs::write(root.join("bad.py"), "wrong: int = 'bad'\n")?;
    fs::write(root.join("excluded.py"), "wrong: int = 'excluded'\n")?;
    fs::write(
        root.join("ty.toml"),
        "[environment]\npython-version='3.13'\n[src]\nexclude=['excluded.py', 'plugin.py']\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid='test.cli'\nruntime='monty'\ntrusted=true\npath='plugin.py'\nmanifest-path='plugin.json'\n",
    )?;
    let run = |arguments: &[&str]| -> Result<(i32, String)> {
        let child = Command::new(env!("CARGO_BIN_EXE_ty-extended"))
            .args(["check", "--ty", &official])
            .args(arguments)
            .env("TY_UV", "0")
            .env("TY_MAX_PARALLELISM", "1")
            .env_remove("TY_CONFIG_FILE")
            .env_remove("TY_OUTPUT_FORMAT")
            .current_dir(&root)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        let prefix = format!("ty-companion-check-{}-", child.id());
        let output = child.wait_with_output()?;
        for entry in fs::read_dir(&temporary)? {
            ensure!(
                !entry?.file_name().to_string_lossy().starts_with(&prefix),
                "CLI leaked its generated analysis directory"
            );
        }
        ensure!(
            matches!(output.status.code(), Some(0 | 1)),
            "CLI failed {:?}: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
        Ok((
            output.status.code().unwrap(),
            String::from_utf8(output.stdout)?,
        ))
    };
    let check = |arguments: &[&str], status: i32, count: usize| -> Result<Value> {
        let mut values = vec!["--output-format", "gitlab"];
        values.extend_from_slice(arguments);
        let (actual_status, stdout) = run(&values)?;
        ensure!(
            actual_status == status,
            "exit mismatch {arguments:?}: {actual_status} != {status}: {stdout}"
        );
        let diagnostics: Value = serde_json::from_str(&stdout)?;
        ensure!(
            diagnostics.as_array().context("diagnostic array")?.len() == count,
            "diagnostic mismatch {arguments:?}: {diagnostics:#}"
        );
        Ok(diagnostics)
    };
    let baseline = check(&[], 1, 1)?;
    ensure!(
        Path::new(baseline[0]["location"]["path"].as_str().unwrap())
            .file_name()
            .unwrap()
            == "bad.py"
    );
    check(&["demo.py"], 0, 0)?;
    check(&["excluded.py"], 1, 1)?;
    check(&["excluded.py", "--force-exclude"], 0, 0)?;
    check(
        &[
            ".",
            "--exclude",
            "bad.py",
            "--exclude",
            "excluded.py",
            "--exclude",
            "plugin.py",
        ],
        0,
        0,
    )?;
    check(&["bad.py", "--warn", "invalid-assignment"], 1, 1)?;
    check(
        &[
            "bad.py",
            "--warn",
            "invalid-assignment",
            "--exit-zero-on-warning",
        ],
        0,
        1,
    )?;
    check(&["bad.py", "--exit-zero"], 0, 1)?;
    check(
        &[
            "bad.py",
            "--config",
            "rules.invalid-assignment='warn'",
            "--config",
            "terminal.error-on-warning=false",
        ],
        0,
        1,
    )?;
    check(&["bad.py", "--ignore", "invalid-assignment"], 0, 0)?;
    for (format, marker) in [
        ("full", "error[invalid-assignment]"),
        ("concise", "bad.py:1:14:"),
        ("github", "::error title=ty (invalid-assignment)"),
        ("junit", "<testsuite"),
    ] {
        let (status, text) = run(&["bad.py", "--output-format", format])?;
        ensure!(
            status == 1 && text.contains(marker),
            "wrong {format} output: {text}"
        );
        ensure!(
            !text.contains("ty-companion-check-"),
            "generated path escaped into {format} output"
        );
    }
    let (status, stdout) = run(&["bad.py", "-qq"])?;
    ensure!(
        status == 1 && stdout.is_empty(),
        "silent output must preserve exit code"
    );
    fs::write(root.join("malformed.py"), "x =\n")?;
    let syntax = check(&["malformed.py"], 1, 1)?;
    ensure!(
        syntax[0]["check_name"] == "invalid-syntax",
        "syntax diagnostics must come from official ty: {syntax}"
    );
    fs::write(
        root.join("notes.py"),
        "def consume(value: str) -> None: pass\nconsume(1)\n",
    )?;
    for format in ["full", "github"] {
        let (status, output) = run(&["notes.py", "--output-format", format])?;
        ensure!(
            status == 1
                && output.contains("Function defined here")
                && output.contains("Parameter declared here"),
            "missing original related locations in {format}: {output}"
        );
        ensure!(
            output.contains("notes.py:1:") && !output.contains("ty-companion-check-"),
            "generated related location in {format}: {output}"
        );
    }
    fs::remove_dir_all(root)?;
    Ok(())
}
