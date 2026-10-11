//! Additional frontend contracts beyond django-ty's unchanged published corpus.
//! Both checkers consume the same sources and unchanged Monty plugin.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn required(name: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var(name).with_context(|| format!("set {name}"))?,
    ))
}

fn run(command: &mut Command, root: &Path, output: &Path, name: &str) -> Result<Output> {
    let result = command
        .current_dir(root)
        .env("TY_MAX_PARALLELISM", "1")
        .env("TY_UV", "0")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()?;
    fs::write(output.join(format!("{name}.stdout")), &result.stdout)?;
    fs::write(output.join(format!("{name}.stderr")), &result.stderr)?;
    ensure!(
        matches!(result.status.code(), Some(0 | 1)),
        "{name} failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(result)
}

fn normalize(value: &Value, root: &Path) -> Result<Vec<Value>> {
    let mut records = Vec::new();
    for diagnostic in value.as_array().context("diagnostics must be an array")? {
        let mut location = diagnostic["location"].clone();
        let path = Path::new(location["path"].as_str().context("diagnostic path")?);
        location["path"] = json!(path.strip_prefix(root).unwrap_or(path));
        records.push(json!({
            "check_name":diagnostic["check_name"], "description":diagnostic["description"],
            "severity":diagnostic["severity"], "location":location,
        }));
    }
    records.sort_by_key(Value::to_string);
    Ok(records)
}

#[test]
#[ignore = "requires DJANGO_TY_SOURCE and TY_COMPANION_TEST_{OFFICIAL,FORK,PYTHON}"]
fn model_identity_self_and_async_control_flow_match_the_fork() -> Result<()> {
    let django = required("DJANGO_TY_SOURCE")?;
    let official = required("TY_COMPANION_TEST_OFFICIAL")?;
    let fork = required("TY_COMPANION_TEST_FORK")?;
    let python = required("TY_COMPANION_TEST_PYTHON")?;
    let package = django.join("python/django_ty");
    let output = std::env::var_os("TY_COMPANION_EDGE_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!(
                "native-django-frontend-edges-{}",
                std::process::id()
            ))
        });
    let root = output.join("project");
    fs::create_dir_all(&root)?;
    for (binary, version) in [(&fork, "0.84.4"), (&official, "0.0.84")] {
        let result = Command::new(binary).arg("--version").output()?;
        ensure!(
            result.status.success() && String::from_utf8_lossy(&result.stdout).contains(version),
            "unexpected checker version: {}",
            String::from_utf8_lossy(&result.stdout)
        );
    }
    // The plugin validates nullable relation arguments using their nominal
    // snapshots; its current nonnullable relation matcher is permissive.
    let fixtures = [
        (
            "settings.py",
            "INSTALLED_APPS = ['left', 'right']\nSECRET_KEY = 'fixture'\n",
        ),
        ("left/__init__.py", ""),
        ("right/__init__.py", ""),
        (
            "left/models.py",
            r#"from __future__ import annotations
from typing import Self
from django.db import models

class Selection(models.QuerySet["Record"]):
    def chained(self) -> Self:
        return self.filter(label="ok").all()

    def invalid_chained(self) -> Self:
        return self.all().filter(does_not_exist="no")

class Record(models.Model):
    label = models.CharField(max_length=30)
    objects = Selection.as_manager()

class Link(models.Model):
    target = models.ForeignKey(Record, on_delete=models.CASCADE, null=True)
"#,
        ),
        (
            "right/models.py",
            "from django.db import models\n\nclass Record(models.Model):\n    count = models.IntegerField()\n",
        ),
        (
            "case.py",
            r#"from typing import assert_type
from left.models import Record as LeftRecord, Link
from right.models import Record as RightRecord

def verify(left: LeftRecord, right: RightRecord) -> None:
    assert_type(LeftRecord.objects.get(pk=1), LeftRecord)
    assert_type(RightRecord.objects.get(pk=1), RightRecord)
    assert_type(LeftRecord.objects.get(pk=1).label, str)
    assert_type(RightRecord.objects.get(pk=1).count, int)
    Link.objects.filter(target=left)
    Link.objects.filter(target=right)
    Link.objects.filter(target=LeftRecord.objects.get(pk=1))
    Link.objects.filter(target=RightRecord.objects.get(pk=1))

def construct() -> None:
    left = LeftRecord(label="ok")
    assert_type(left.id, None)
    left.save()
    assert_type(left.id, int)
    right = RightRecord(count=1)
    assert_type(right.id, None)
    right.save()
    assert_type(right.id, int)
"#,
        ),
        (
            "syntax.py",
            r#"from typing import assert_type
from left.models import Record as LeftRecord

async def asynchronous() -> None:
    loaded = await LeftRecord.objects.aget(pk=1)
    assert_type(loaded, LeftRecord)
    assert_type(loaded.id, int | None)
    await loaded.asave()
    assert_type(loaded.id, int | None)
    await loaded.adelete()
    assert_type(loaded.id, int | None)
    loaded.save()
    assert_type(loaded.id, int)
    await LeftRecord.objects.aget(does_not_exist=1)

def loop_join(condition: bool) -> None:
    item = LeftRecord(label="loop")
    while condition:
        item.save()
        break
    assert_type(item.id, int | None)
    for _ in range(2):
        item.save()
    assert_type(item.id, int | None)

def exception_join() -> None:
    item = LeftRecord(label="exception")
    try:
        item.save()
    except Exception:
        pass
    assert_type(item.id, int | None)

def successful_exception_path() -> None:
    item = LeftRecord(label="success")
    try:
        item.save()
    except Exception:
        raise
    assert_type(item.id, int)
"#,
        ),
        (
            "loops.py",
            r#"from typing import assert_type
from left.models import Record

# Checking the model source roots is required for the fork project index.
def state_hook_control() -> None:
    item = Record(label="control")
    assert_type(item.id, None)
    item.save()
    assert_type(item.id, int)

def zero_iterations() -> None:
    item = Record(label="zero")
    for _ in ():
        pass
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def unconditional_break(values: list[int]) -> None:
    item = Record(label="break")
    for _ in values:
        break
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def conditional_break(values: list[int], condition: bool) -> None:
    item = Record(label="conditional")
    for _ in values:
        if condition:
            break
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def while_break(condition: bool) -> None:
    item = Record(label="while")
    while condition:
        break
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def while_zero() -> None:
    item = Record(label="while-zero")
    while False:
        pass
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def nested_break(values: list[int]) -> None:
    item = Record(label="nested")
    for _ in values:
        for _ in values:
            break
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def nested_else_break(values: list[int]) -> None:
    item = Record(label="nested-else")
    for _ in values:
        for _ in ():
            pass
        else:
            break
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def nested_function_break(values: list[int]) -> None:
    item = Record(label="nested-function")
    for _ in values:
        def nested() -> None:
            for _ in values:
                break
    else:
        item.save()
        assert_type(item.id, int)
    assert_type(item.id, int | None)

def for_else_return(values: list[int]) -> None:
    item = Record(label="for-return")
    for _ in values:
        break
    else:
        return
    item.save()
    assert_type(item.id, int)

def while_else_return(condition: bool) -> None:
    item = Record(label="while-return")
    while condition:
        break
    else:
        return
    item.save()
    assert_type(item.id, int)

def for_else_raise(values: list[int]) -> None:
    item = Record(label="for-raise")
    for _ in values:
        break
    else:
        raise RuntimeError
    item.save()
    assert_type(item.id, int)

def while_else_raise(condition: bool) -> None:
    item = Record(label="while-raise")
    while condition:
        break
    else:
        raise RuntimeError
    item.save()
    assert_type(item.id, int)

def break_skips_else_alias(values: list[int]) -> None:
    first = Record(label="first")
    second = Record(label="second")
    for _ in values:
        break
    else:
        first = second
    first.save()
    assert_type(first.id, int)
    assert_type(second.id, int | None)

def conditional_break_skips_else_alias(values: list[int], condition: bool) -> None:
    first = Record(label="first")
    second = Record(label="second")
    for _ in values:
        if condition:
            break
    else:
        first = second
    first.save()
    assert_type(first.id, int)
    assert_type(second.id, int | None)
"#,
        ),
    ];
    for (path, content) in fixtures {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap())?;
        fs::write(path, content)?;
    }
    let config = output.join("config.json");
    let files = output.join("files.json");
    fs::write(
        &config,
        serde_json::to_vec(&json!({"django-settings-module":"settings"}))?,
    )?;
    fs::write(
        &files,
        serde_json::to_vec(&json!([
            "left",
            "right",
            "case.py",
            "syntax.py",
            "loops.py"
        ]))?,
    )?;
    let entry = format!(
        "plugins.plugin = [{{id = \"django-ty\", runtime = \"monty\", trusted = true, path = {}, manifest-path = {}, stub-overlay-path = {}, config = {{django-settings-module = \"settings\"}}}}]",
        serde_json::to_string(&package.join("monty.py"))?,
        serde_json::to_string(&package.join("ty-plugin-monty.json"))?,
        serde_json::to_string(&package.join("stubs"))?,
    );
    let baseline = run(
        Command::new(&fork)
            .args([
                "check",
                "left",
                "right",
                "case.py",
                "syntax.py",
                "loops.py",
                "--project",
            ])
            .arg(&root)
            .arg("--python")
            .arg(&python)
            .args([
                "--python-version",
                "3.13",
                "--output-format",
                "gitlab",
                "--color",
                "never",
                "--no-progress",
                "--config",
                "plugins.enabled = true",
                "--config",
                "plugins.auto-discover = false",
                "--config",
            ])
            .arg(entry)
            .args(["--config", "terminal.error-on-warning = true"]),
        &root,
        &output,
        "fork",
    )?;
    let expected = normalize(&serde_json::from_slice::<Value>(&baseline.stdout)?, &root)?;
    // An empty or silently broken baseline must never make this differential gate pass.
    ensure!(
        expected.len() == 3,
        "expected two invalid lookups and one wrong-model call argument, got {}: {}",
        expected.len(),
        serde_json::to_string_pretty(&expected)?
    );
    ensure!(
        expected
            .iter()
            .all(|diagnostic| diagnostic["check_name"] == "plugin-configuration"),
        "unexpected fork type errors: {}",
        serde_json::to_string_pretty(&expected)?
    );
    for mode in ["cli", "push"] {
        let projection = output.join(format!("projection-{mode}"));
        let candidate = run(
            Command::new(env!("CARGO_BIN_EXE_ty-companion-rust"))
                .arg("project")
                .arg(&root)
                .arg("--plugin")
                .arg(package.join("monty.py"))
                .arg("--manifest")
                .arg(package.join("ty-plugin-monty.json"))
                .arg("--ty")
                .arg(&official)
                .arg("--out")
                .arg(&projection)
                .arg("--config")
                .arg(&config)
                .arg("--stubs")
                .arg(package.join("stubs"))
                .args(["--python-version", "3.13", "--python"])
                .arg(&python)
                .arg("--files")
                .arg(&files)
                .args(["--checker-mode", mode]),
            &root,
            &output,
            mode,
        )?;
        let candidate: Value = serde_json::from_slice(&candidate.stdout)?;
        let actual = normalize(&candidate["diagnostics"], &root)?;
        fs::write(
            output.join(format!("normalized-{mode}.json")),
            serde_json::to_vec_pretty(&actual)?,
        )?;
        ensure!(
            actual == expected,
            "{mode} frontend edge mismatch; evidence at {}\nexpected={}\nactual={}",
            output.display(),
            serde_json::to_string_pretty(&expected)?,
            serde_json::to_string_pretty(&actual)?
        );
    }
    println!(
        "Model identity, Self chains and async/control-flow/loop-else checks match in CLI and push modes: {}",
        output.display()
    );
    Ok(())
}
