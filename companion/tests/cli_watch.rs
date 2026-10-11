//! Native-event watch mode must reuse analysis on every supported platform.
//! Unix additionally verifies Ctrl-C exit status and automatic cleanup.
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

/// stdout and stderr are independent pipes. A completed check's stderr timing
/// can reach its reader before that check's stdout diagnostic, despite the CLI
/// flushing stdout first. Await the whole check without discarding either half.
fn wait_for(
    receiver: &mpsc::Receiver<String>,
    stage: &str,
    expected: &[&str],
) -> Result<Vec<String>> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut pending = expected.to_vec();
    let mut lines = Vec::new();
    while !pending.is_empty() {
        let line = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .with_context(|| {
                format!("watch stage {stage:?} missing {pending:?}; received {lines:?}")
            })?;
        pending.retain(|needle| !line.contains(needle));
        lines.push(line);
    }
    Ok(lines)
}

#[test]
fn check_batches_accept_both_stdout_stderr_orders() -> Result<()> {
    for timing_first in [false, true] {
        let (sender, receiver) = mpsc::channel();
        let timing = "Checked in 0.001s (0 Monty executions, 1 cache hits)";
        if timing_first {
            sender.send(timing.to_owned())?;
        }
        sender.send("demo.py:2:1: error[invalid-assignment] incompatible value".to_owned())?;
        if !timing_first {
            sender.send(timing.to_owned())?;
        }
        sender.send("Found 1 diagnostic".to_owned())?;
        sender.send("next check".to_owned())?;
        drop(sender);
        let lines = wait_for(
            &receiver,
            "controlled stream reordering",
            &[
                "error[invalid-assignment]",
                "Found 1 diagnostic",
                "Checked in",
            ],
        )?;
        assert_eq!(lines.len(), 3);
        assert!(lines.iter().any(|line| line.contains("0 Monty executions")));
        assert_eq!(receiver.try_recv()?, "next check");
    }
    Ok(())
}

#[test]
#[ignore = "requires TY_COMPANION_TEST_OFFICIAL"]
fn watch_rechecks_source_plugin_config_and_dependencies() -> Result<()> {
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
    let wait = |stage, expected: &[&str]| wait_for(&receiver, stage, expected);
    wait(
        "initial analysis and watcher ready",
        &[
            "All checks passed!",
            "Checked in",
            "Watching for file changes",
        ],
    )?;
    // A comment-only edit has identical semantic requests and reuses Monty state.
    fs::write(root.join("demo.py"), format!("{source}# comment\n"))?;
    let trivia = wait("comment-only edit", &["All checks passed!", "Checked in"])?;
    ensure!(
        trivia
            .iter()
            .any(|line| line.contains("0 Monty executions")),
        "trivia reran Monty: {trivia:?}"
    );
    // Plugin source changes are configuration dependencies even when excluded
    // from checking as ordinary Python input.
    fs::write(root.join("plugin.py"), plugin.replace("'str'", "'int'"))?;
    wait(
        "plugin implementation changed",
        &[
            "error[invalid-assignment]",
            "Found 1 diagnostic",
            "Checked in",
        ],
    )?;
    fs::write(
        root.join("ty.toml"),
        format!("{configuration}\n[rules]\ninvalid-assignment='warn'\n"),
    )?;
    wait(
        "configuration severity changed",
        &[
            "warning[invalid-assignment]",
            "Found 1 diagnostic",
            "Checked in",
        ],
    )?;
    fs::write(
        root.join("demo.py"),
        source.replace("value: str", "value: int"),
    )?;
    wait(
        "source annotation fixed",
        &["All checks passed!", "Checked in"],
    )?;
    // Atomic editor saves must remain observed after replacing the watched file.
    fs::write(root.join("next.py.tmp"), "new_error: int = 'bad'\n")?;
    fs::rename(root.join("next.py.tmp"), root.join("next.py"))?;
    wait(
        "new source installed by atomic rename",
        &["next.py:1:", "Found 1 diagnostic", "Checked in"],
    )?;
    fs::remove_file(root.join("next.py"))?;
    wait("source file removed", &["All checks passed!", "Checked in"])?;
    fs::write(
        root.join("dependency.py"),
        "from installed_later import value\nother: int = value\n",
    )?;
    wait(
        "missing dependency imported",
        &[
            "error[unresolved-import]",
            "Found 1 diagnostic",
            "Checked in",
        ],
    )?;
    fs::write(dependencies.join("installed_later.py"), "value = 1\n")?;
    wait(
        "dependency installed after negative lookup",
        &["All checks passed!", "Checked in"],
    )?;
    let idle_until = Instant::now() + Duration::from_millis(400);
    while let Ok(line) = receiver.recv_timeout(idle_until.saturating_duration_since(Instant::now()))
    {
        // Linux inotify reports our own reads as Access events. Debug logging
        // such an event is expected; a new check without a mutation is not.
        ensure!(
            !line.contains("Checked in")
                && !line.contains("All checks passed!")
                && !line.starts_with("Found "),
            "idle watcher reran analysis without a file change: {line}"
        );
    }
    let process_id = child.0.id();
    let prefix = format!("ty-companion-check-{process_id}-");
    #[cfg(unix)]
    {
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
        ensure!(
            !fs::read_dir(&temporary)?
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with(&prefix)),
            "watch leaked its projection on Ctrl+C"
        );
    }
    #[cfg(not(unix))]
    {
        // Process termination is teardown, not evidence of Windows Ctrl-C
        // handling. Every preceding native watch assertion still runs here.
        child.0.kill()?;
        child.0.wait()?;
        for entry in fs::read_dir(&temporary)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    match fs::remove_dir_all(entry.path()) {
                        Ok(()) => break,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                        Err(error) if Instant::now() >= deadline => {
                            return Err(error).context("remove terminated watch test projection");
                        }
                        // Allow the checker child to observe closed stdio and
                        // release Windows file handles after its parent exits.
                        Err(_) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            }
        }
    }
    fs::remove_dir_all(root)?;
    fs::remove_dir_all(dependencies)?;
    Ok(())
}
