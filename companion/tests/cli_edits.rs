//! Differential checks for explicit source-mutating CLI flags. All writes are
//! confined to this test's temporary project and compared byte-for-byte.
use anyhow::{Context, Result, ensure};
use serde_json::json;
use std::{fs, path::Path, process::Command};

#[test]
#[ignore = "requires TY_COMPANION_TEST_OFFICIAL and TY_COMPANION_TEST_FORK"]
fn safe_fixes_and_suppressions_match_reference_source_edits() -> Result<()> {
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
    let fork = std::env::var("TY_COMPANION_TEST_FORK")?;
    let root = std::env::temp_dir()
        .canonicalize()?
        .join(format!("ty-cli-edits-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/generic");
    let config = format!(
        "[environment]\npython-version='3.13'\n[plugins]\nenabled=true\nauto-discover=false\n[[plugins.plugin]]\nid='research.generic'\nruntime='monty'\ntrusted=true\npath={}\nmanifest-path={}\n",
        json!(fixture.join("plugin.py")),
        json!(fixture.join("plugin.plugin.json"))
    );
    fs::write(root.join("ty.toml"), config)?;
    let cases = [
        (
            "--fix",
            "safe",
            "value = 1  # ty: ignore[invalid-assignment]\nreveal_type(value)\n",
        ),
        (
            "--fix",
            "overlap",
            "value: int = 'wrong'  # ty: ignore[invalid-assignment, unresolved-reference]\n",
        ),
        (
            "--add-ignore",
            "unicode",
            "é: int = 'wrong'\r\nvalue = unknown + other\r\n",
        ),
        (
            "--add-ignore",
            "multiple_codes",
            "value: str = 1; other = missing\n",
        ),
        (
            "--add-ignore",
            "existing_reason",
            "value: str = 1  # ty: ignore[unresolved-reference] historical reason\n",
        ),
        (
            "--add-ignore",
            "multiline",
            "value = f'''hello {missing}\nworld'''\ncontinued = unknown \\\n    + 1\n",
        ),
        (
            "--add-ignore",
            "plugin",
            "from typing import assert_type\nclass Resource:\n    status: str | None = None\n    def activate(self) -> None: pass\nitem = Resource()\nitem.activate()\nassert_type(item.label, str)\nwrong: int = item.status\n",
        ),
        (
            "--fix",
            "invalid_syntax",
            "if:\nvalue = 1  # ty: ignore[invalid-assignment]\n",
        ),
    ];
    let notebook = |first: &str, second: &str| -> String {
        serde_json::to_string_pretty(&json!({"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"display_name":"Python 3","language":"python","name":"python3"},"custom":{"keep":true}},"cells":[
            {"cell_type":"markdown","id":"intro","metadata":{},"source":["# Keep this heading\n"]},
            {"cell_type":"code","id":"first","execution_count":7,"metadata":{"tag":"preserve"},"outputs":[{"name":"stdout","output_type":"stream","text":["keep output\n"]}],"source":[first]},
            {"cell_type":"code","id":"magic","execution_count":null,"metadata":{},"outputs":[],"source":["%%bash\necho keep\n"]},
            {"cell_type":"code","id":"second","execution_count":null,"metadata":{},"outputs":[],"source":[second]}
        ]})).unwrap() + "\n"
    };
    let cases = cases
        .into_iter()
        .map(|(flag, name, source)| (flag, name, source.to_owned()))
        .chain([
            (
                "--fix",
                "notebook_safe",
                notebook(
                    "# ty: ignore[invalid-assignment]\n",
                    "value = 1  # ty: ignore[invalid-assignment]\n",
                ),
            ),
            (
                "--add-ignore",
                "notebook_suppress",
                notebook("é: int = 'wrong'\r\n", "value: str = 1; other = missing\n"),
            ),
            (
                "--fix",
                "notebook_invalid_cells",
                notebook(
                    "if True:\n",
                    "    value = 1 # ty: ignore[invalid-assignment]\n",
                ),
            ),
        (
            "--add-ignore",
            "notebook_plugin",
            notebook(
                "from demo import Resource\nitem = Resource()\nitem.activate()\n",
                "from typing import assert_type\nassert_type(item.status, str)\nassert_type(item.label, str)\nwrong: int = item.status\n",
            ),
        ),
        ]);
    let mut evidence = vec![];
    for (flag, name, source) in cases {
        let filename = if name.starts_with("notebook_") {
            "demo.ipynb"
        } else {
            "demo.py"
        };
        let path = root.join(filename);
        if name == "notebook_plugin" {
            fs::write(
                root.join("demo.py"),
                "class Resource:\n    status: str | None = None\n    def activate(self) -> None: pass\n",
            )?;
        }
        let mut results = vec![];
        for (backend, binary) in [
            ("fork", fork.as_str()),
            ("companion", env!("CARGO_BIN_EXE_ty-companion-rust")),
        ] {
            fs::write(&path, &source)?;
            let mut command = Command::new(binary);
            command.args(["check", filename, flag, "--output-format", "gitlab"]);
            if backend == "companion" {
                command.args(["--ty", &official]);
            }
            let output = command
                .current_dir(&root)
                .env("TY_UV", "0")
                .env("TY_MAX_PARALLELISM", "1")
                .env_remove("TY_CONFIG_FILE")
                .env_remove("TY_OUTPUT_FORMAT")
                .output()?;
            fs::write(
                root.join(format!("{name}-{backend}.stdout")),
                &output.stdout,
            )?;
            fs::write(
                root.join(format!("{name}-{backend}.stderr")),
                &output.stderr,
            )?;
            let text = fs::read_to_string(&path)?;
            fs::write(root.join(format!("{name}-{backend}.txt")), &text)?;
            ensure!(
                matches!(output.status.code(), Some(0 | 1)),
                "{name}/{backend}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let diagnostics: serde_json::Value = serde_json::from_slice(&output.stdout)
                .with_context(|| {
                    format!(
                        "{name}/{backend} JSON: {}",
                        String::from_utf8_lossy(&output.stdout)
                    )
                })?;
            let mut codes: Vec<_> = diagnostics
                .as_array()
                .context("diagnostics")?
                .iter()
                .map(|diagnostic| diagnostic["check_name"].clone())
                .collect();
            codes.sort_by_key(serde_json::Value::to_string);
            results.push((text, output.status.code(), codes));
        }
        evidence.push(json!({"case":name,"flag":flag,"equal":results[0]==results[1],"fork":results[0],"companion":results[1]}));
        fs::write(
            root.join("evidence.json"),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        ensure!(
            results[0] == results[1],
            "{name} source/status/rules differ: {}",
            root.display()
        );
    }
    Ok(())
}
