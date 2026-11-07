//! Shared diagnostic comparison for real-checker integration harnesses.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::path::Path;

pub fn normalize(diagnostics: &Value, root: &Path) -> Result<Vec<Value>> {
    let root = root.canonicalize().context("canonical fixture root")?;
    let mut values = Vec::new();
    for diagnostic in diagnostics.as_array().context("diagnostic array")? {
        let mut location = diagnostic["location"].clone();
        let path = Path::new(location["path"].as_str().context("diagnostic path")?);
        // The reference can emit a relative path or an ordinary Windows drive
        // path; the companion stores canonical (verbatim on Windows) origins.
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            ensure!(
                !path.components().any(|part| matches!(
                    part,
                    std::path::Component::Prefix(_) | std::path::Component::RootDir
                )),
                "ambiguous diagnostic path: {}",
                path.display()
            );
            root.join(path)
        };
        let canonical = absolute
            .canonicalize()
            .with_context(|| format!("diagnostic source {}", path.display()))?;
        let relative = canonical.strip_prefix(&root).with_context(|| {
            format!(
                "diagnostic source outside fixture root: {}",
                canonical.display()
            )
        })?;
        location["path"] = json!(relative);
        values.push(json!({"code":diagnostic["check_name"],"description":diagnostic["description"],"severity":diagnostic["severity"],"location":location}));
    }
    values.sort_by_key(Value::to_string);
    Ok(values)
}

#[test]
fn diagnostic_paths_compare_canonical_origins_and_reject_external_files() -> Result<()> {
    use std::fs;

    let directory = std::env::temp_dir().join(format!(
        "ty-harness-diagnostic-paths-{}",
        std::process::id()
    ));
    let root = directory.join("project");
    fs::create_dir_all(root.join("nested"))?;
    let relative = Path::new("nested").join("demo.py");
    let source = root.join(&relative);
    fs::write(&source, "value: int = 'wrong'\n")?;
    let outside = directory.join("outside.py");
    fs::write(&outside, "value: int = 'outside'\n")?;
    let diagnostic = |path: &Path| json!([{"check_name":"invalid-assignment","description":"exact diagnostic text", "severity":"major", "location":{"path":path,"positions":{"begin":{"line":1,"column":14},"end":{"line":1,"column":21}}}}]);
    let expected = normalize(&diagnostic(&relative), &root)?;
    ensure!(expected[0]["location"]["path"] == json!(relative));
    ensure!(expected[0]["description"] == "exact diagnostic text");
    ensure!(expected[0]["code"] == "invalid-assignment");
    ensure!(expected[0]["severity"] == "major");
    ensure!(
        expected[0]["location"]["positions"] == diagnostic(&relative)[0]["location"]["positions"]
    );
    for spelling in [&source, &source.canonicalize()?] {
        for root_spelling in [&root, &root.canonicalize()?] {
            ensure!(normalize(&diagnostic(spelling), root_spelling)? == expected);
        }
    }
    for spelling in [
        &outside,
        &outside.canonicalize()?,
        Path::new("../outside.py"),
    ] {
        ensure!(
            normalize(&diagnostic(spelling), &root).is_err(),
            "external diagnostic was accepted: {}",
            spelling.display()
        );
    }
    #[cfg(windows)]
    for ambiguous in [r"C:outside.py", r"\outside.py"] {
        ensure!(normalize(&diagnostic(Path::new(ambiguous)), &root).is_err());
    }
    fs::remove_dir_all(directory)?;
    Ok(())
}
