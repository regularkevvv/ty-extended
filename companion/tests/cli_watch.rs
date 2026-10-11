//! Native-event watch mode must reuse analysis and clean up after interruption.
#![cfg(unix)]
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
fn watch_rechecks_source_plugin_and_config_then_cleans_on_interrupt() -> Result<()> {
    let temporary = std::env::temp_dir().canonicalize()?;
    let root = temporary.join(format!("ty-cli-watch-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let dependencies = temporary.join(format!("ty-cli-watch-dependencies-{}", std::process::id()));
    fs::create_dir_all(&dependencies)?;
    let configuration = "[environment]\npython-version='3.13'\n[src]\nexclude=['plugin.py']\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid='watch.test'\nruntime='monty'\ntrusted=true\npath='plugin.py'\nmanifest-path='plugin.json'\n";
    let configuration = configuration.replace(
        "[environment]\n",
        &format!(
            "[environment]\nextra-paths=[{}]\n",
            serde_json::to_string(&dependencies)?
        ),
    );
    fs::write(root.join("ty.toml"), &configuration)?;
    fs::write(
        root.join("plugin.json"),
        r#"{"id":"watch.test","name":"Watch test","version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":"plugin.py"},"capabilities":{"class-transform":true},"claims":{"classes":[{"kind":"exact","qualified-name":"demo.Resource"}]}}"#,
    )?;
    let plugin = "@on_class_transform\ndef transform(request):\n    return class_patch(fields=[field('extra', type_annotation('str'))])\n";
    fs::write(root.join("plugin.py"), plugin)?;
    let source = "class Resource: pass\nvalue: str = Resource().extra\n";
    fs::write(root.join("demo.py"), source)?;
    let mut child = Running(
        Command::new(env!("CARGO_BIN_EXE_ty-companion-rust"))
            .args([
                "check",
                "--watch",
                "-vv",
                "--output-format",
                "concise",
                "--ty",
                &std::env::var("TY_COMPANION_TEST_OFFICIAL")?,
            ])
            .current_dir(&root)
            .env("TY_UV", "0")
            .env("TY_MAX_PARALLELISM", "1")
            .env_remove("TY_CONFIG_FILE")
            .env_remove("TY_OUTPUT_FORMAT")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let (sender, receiver) = mpsc::channel();
    for stream in [
        Box::new(child.0.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>,
        Box::new(child.0.stderr.take().unwrap()),
    ] {
        let sender = sender.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                if sender.send(line.unwrap_or_default()).is_err() {
                    break;
                }
            }
        });
    }
    drop(sender);
    let wait = |needle: &str| -> Result<Vec<String>> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut lines = Vec::new();
        while Instant::now() < deadline {
            let line = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .with_context(|| format!("watch did not emit {needle:?}; received {lines:?}"))?;
            let matched = line.contains(needle);
            lines.push(line);
            if matched {
                return Ok(lines);
            }
        }
        anyhow::bail!("watch timed out waiting for {needle}: {lines:?}")
    };
    wait("Watching for file changes")?;
    // A comment-only edit has identical semantic requests and reuses Monty state.
    fs::write(root.join("demo.py"), format!("{source}# comment\n"))?;
    let trivia = wait("Checked in")?;
    ensure!(
        trivia
            .iter()
            .any(|line| line.contains("0 Monty executions")),
        "trivia reran Monty: {trivia:?}"
    );
    // Plugin source changes are configuration dependencies even when excluded
    // from checking as ordinary Python input.
    fs::write(root.join("plugin.py"), plugin.replace("'str'", "'int'"))?;
    wait("error[invalid-assignment]")?;
    wait("Checked in")?;
    fs::write(
        root.join("ty.toml"),
        format!("{configuration}\n[rules]\ninvalid-assignment='warn'\n"),
    )?;
    wait("warning[invalid-assignment]")?;
    wait("Checked in")?;
    fs::write(
        root.join("demo.py"),
        source.replace("value: str", "value: int"),
    )?;
    wait("All checks passed!")?;
    wait("Checked in")?;
    // Atomic editor saves must remain observed after replacing the watched file.
    fs::write(root.join("next.py.tmp"), "new_error: int = 'bad'\n")?;
    fs::rename(root.join("next.py.tmp"), root.join("next.py"))?;
    wait("next.py:1:")?;
    wait("Checked in")?;
    fs::remove_file(root.join("next.py"))?;
    wait("All checks passed!")?;
    wait("Checked in")?;
    fs::write(
        root.join("dependency.py"),
        "from installed_later import value\nother: int = value\n",
    )?;
    wait("error[unresolved-import]")?;
    wait("Checked in")?;
    fs::write(dependencies.join("installed_later.py"), "value = 1\n")?;
    wait("All checks passed!")?;
    wait("Checked in")?;
    ensure!(
        receiver.recv_timeout(Duration::from_millis(400)).is_err(),
        "idle watcher reran analysis without a file change"
    );
    let process_id = child.0.id();
    ensure!(
        Command::new("kill")
            .args(["-INT", &process_id.to_string()])
            .status()?
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        ensure!(Instant::now() < deadline, "watch did not stop after Ctrl+C");
        std::thread::sleep(Duration::from_millis(10));
    };
    ensure!(status.code() == Some(130), "watch exit status: {status}");
    let prefix = format!("ty-companion-check-{process_id}-");
    ensure!(
        !fs::read_dir(&temporary)?
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix)),
        "watch leaked its projection on Ctrl+C"
    );
    fs::remove_dir_all(root)?;
    fs::remove_dir_all(dependencies)?;
    Ok(())
}
