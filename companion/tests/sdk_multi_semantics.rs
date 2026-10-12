//! Multiple real Monty plugins compared with the integrated checker.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{fs, path::PathBuf, process::Command};

mod support;
use support::normalize;

#[test]
#[ignore = "requires TY_COMPANION_TEST_FORK and TY_COMPANION_TEST_OFFICIAL"]
fn multiple_plugins_keep_precedence_config_and_virtual_types() -> Result<()> {
    let official = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
    let fork = std::env::var("TY_COMPANION_TEST_FORK")?;
    let output = std::env::var_os("TY_COMPANION_MULTI_OUTPUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("native-sdk-multi-{}", std::process::id()))
        });
    let root = output.join("project");
    let stubs = output.join("stubs");
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&stubs)?;
    fs::write(
        root.join("demo.py"),
        "from typing import assert_type\nclass Item: pass\ndef load() -> object: return object()\ndef shape_a() -> object: return object()\ndef shape_z() -> object: return object()\nassert_type(Item().shared, int)\nassert_type(Item().first, bool)\nassert_type(Item().second, bool)\nassert_type(Item().dynamic, str)\nassert_type(load(), object)\nassert_type(shape_a().value, int)\nassert_type(shape_z().value, str)\n",
    )?;
    let mut registrations = Vec::new();
    // Deliberately register z first: the fork's public precedence is lexical id.
    for (id, annotation, unique) in [("z", "str", "second"), ("a", "int", "first")] {
        let manifest = json!({"id":id,"name":id,"version":"0.0.0","protocol-version":{"major":0,"minor":7},"ty-compatibility":{"requirement":">=0.86.0,<0.87.0"},"runtime":{"kind":"monty","artifact":format!("{id}.py")},"capabilities":{"project-index":true,"class-transform":true,"instance-member":true,"call-return":true},"claims":{"modules":[{"name":"demo"}],"classes":[{"kind":"exact","qualified-name":"demo.Item"}],"functions":[{"qualified-name":"demo.load"},{"qualified-name":format!("demo.shape_{id}")}],"attributes":[{"kind":"exact","scope":"instance","owner-qualified-name":"demo.Item","attribute-name":"dynamic"}]}});
        let manifest_path = output.join(format!("{id}.json"));
        let plugin = output.join(format!("{id}.py"));
        fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
        let manifest_python = manifest
            .to_string()
            .replace(":true", ":True")
            .replace(":false", ":False")
            .replace(":null", ":None");
        fs::write(
            &plugin,
            format!(
                "set_manifest({manifest_python})\n@on_project_index\ndef index(request):\n    return project_index(plugin_index={{'annotation':request['context']['config']['annotation']}}, virtual_types=[virtual_type('virtual.Record',virtual_class(members=[member('value',type_annotation(request['context']['config']['annotation']))]))])\n@on_class_transform\ndef transform(request):\n    config=request['context']['config']\n    return class_patch(fields=[field('shared',type_annotation(config['annotation'])),field(config['unique'],type_annotation('bool'))])\n@on_instance_member\ndef dynamic(request):\n    if request['context']['config']['annotation']=='int': return None\n    return member_response(member('dynamic',type_annotation(request['project-index']['annotation'])))\n@on_call_return\ndef returns(request):\n    if request['callee']['expression']=='demo.load':\n        if request['context']['config']['annotation']=='int': return None\n        return call_return_patch(type_annotation('str'))\n    return call_return_patch(type_annotation('virtual.Record'))\n"
            ),
        )?;
        registrations.push(format!("{{id={id:?},runtime=\"monty\",trusted=true,path={},manifest-path={},config={{annotation={annotation:?},unique={unique:?}}}}}",serde_json::to_string(&plugin)?,serde_json::to_string(&manifest_path)?));
    }
    fs::write(
        root.join("ty.toml"),
        format!(
            "[plugins]\nenabled=true\nauto-discover=false\nplugin=[{}]\n",
            registrations.join(",")
        ),
    )?;
    let baseline = Command::new(&fork)
        .args([
            "check",
            "demo.py",
            "--output-format",
            "gitlab",
            "--python-version",
            "3.13",
        ])
        .current_dir(&root)
        .env("TY_UV", "0")
        .env("TY_MAX_PARALLELISM", "1")
        .output()?;
    fs::write(output.join("fork.stdout"), &baseline.stdout)?;
    fs::write(output.join("fork.stderr"), &baseline.stderr)?;
    ensure!(
        baseline.status.success(),
        "fork setup/contract failed: {} {}",
        String::from_utf8_lossy(&baseline.stdout),
        String::from_utf8_lossy(&baseline.stderr)
    );
    let expected = normalize(&serde_json::from_slice(&baseline.stdout)?, &root)?;
    ensure!(
        expected.is_empty(),
        "fork should accept every multi-plugin assertion"
    );
    for mode in ["cli", "push"] {
        let result = Command::new(env!("CARGO_BIN_EXE_ty-companion-rust"))
            .arg("project")
            .arg(&root)
            .arg("--ty")
            .arg(&official)
            .arg("--stubs")
            .arg(&stubs)
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
            result.status.success(),
            "{mode} failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let response: Value = serde_json::from_slice(&result.stdout)?;
        ensure!(
            normalize(&response["diagnostics"], &root)? == expected,
            "{mode} differs from fork"
        );
    }
    Ok(())
}
