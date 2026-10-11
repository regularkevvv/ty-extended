//! User-facing checking command over the persistent project frontend.
//! Flag names and precedence follow the pinned upstream `ty/src/args.rs`.
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

pub const HELP: &str = "Check Python with official ty and configured Monty plugins\n\nUsage: ty-companion-rust check [OPTIONS] [PATH]...\n\n  --fix                          Apply safe fixes to original source files\n  --add-ignore                   Add ty: ignore comments to original source files\n  -W, --watch                    Recheck when project or dependency files change\n  --project PATH                 Select the project directory\n  --python PATH                  Python interpreter or environment\n  --python-version VERSION       Target Python version\n  --python-platform PLATFORM     Target platform\n  --typeshed PATH                Custom standard-library stubs\n  --extra-search-path PATH       Additional import path (repeatable)\n  -c, --config KEY=VALUE          Override TOML configuration (repeatable)\n  --config-file PATH             Use a ty.toml file\n  --error RULE                   Set a rule to error (repeatable)\n  --warn RULE                    Set a rule to warning (repeatable)\n  --ignore RULE                  Disable a rule (repeatable)\n  --exclude PATTERN              Exclude source paths (repeatable)\n  --respect-ignore-files         Respect ignore files (default)\n  --no-respect-ignore-files      Ignore ignore files\n  --force-exclude                Exclude explicitly selected paths too\n  --no-force-exclude             Explicit paths bypass exclusions (default)\n  --exclude-scripts              Exclude PEP 723 scripts\n  --include-scripts              Include PEP 723 scripts\n  --output-format FORMAT         full, concise, gitlab, github, junit\n  --error-on-warning[=BOOL]       Fail on warnings (default: true)\n  --exit-zero-on-warning         Fail only on errors\n  --exit-zero                    Succeed even when diagnostics occur\n  --color WHEN                   auto, always, never\n  -q, --quiet                    Hide the success summary; -qq hides diagnostics\n  -v, --verbose                  Print timing information\n  --no-progress                 Suppress progress output\n  --ty PATH                      Official ty executable (default: TY_COMPANION_TY or ty)\n  -h, --help                     Show this help\n\nPlugin configuration is discovered from ty.toml or [tool.ty] in pyproject.toml.\nTemporary analysis files are removed when checking finishes.\n";

#[derive(Default)]
struct Arguments {
    project: Option<PathBuf>,
    paths: Vec<PathBuf>,
    options: BTreeMap<String, String>,
    configs: Vec<String>,
    explicit: Vec<String>,
    extras: Vec<String>,
    excludes: Vec<String>,
    format: Option<String>,
    warning_exit: Option<bool>,
    exit_zero: bool,
    fix: bool,
    add_ignore: bool,
    watch: bool,
    force_exclude: bool,
    quiet: usize,
    verbose: usize,
    color: String,
    help: bool,
}

fn quoted(value: &str) -> String {
    toml::Value::String(value.to_owned()).to_string()
}

fn parse(arguments: &[String]) -> Result<Arguments> {
    let mut result = Arguments {
        color: "auto".into(),
        ..Arguments::default()
    };
    let mut index = 0;
    let mut positional = false;
    let mut warning_flag = false;
    let mut zero_warning_flag = false;
    while index < arguments.len() {
        let argument = &arguments[index];
        index += 1;
        if positional || !argument.starts_with('-') || argument == "-" {
            result.paths.push(PathBuf::from(argument));
            continue;
        }
        if argument == "--" {
            positional = true;
            continue;
        }
        if matches!(argument.as_str(), "--help" | "-h") {
            result.help = true;
            continue;
        }
        if matches!(argument.as_str(), "--verbose" | "--quiet") {
            if argument == "--verbose" {
                result.verbose += 1;
            } else {
                result.quiet += 1;
            }
            continue;
        }
        if !argument.starts_with("--") && argument[1..].chars().all(|c| matches!(c, 'v' | 'q')) {
            result.verbose += argument[1..].chars().filter(|c| *c == 'v').count();
            result.quiet += argument[1..].chars().filter(|c| *c == 'q').count();
            continue;
        }
        let (flag, inline) = argument
            .split_once('=')
            .map_or((argument.as_str(), None), |(k, v)| (k, Some(v)));
        let inline = if inline.is_none()
            && matches!(
                flag,
                "--error-on-warning" | "--respect-ignore-files" | "--exclude-scripts"
            )
            && arguments
                .get(index)
                .is_some_and(|value| matches!(value.as_str(), "true" | "false"))
        {
            let value = arguments[index].as_str();
            index += 1;
            Some(value)
        } else {
            inline
        };
        let mut value = || -> Result<String> {
            if let Some(value) = inline {
                return Ok(value.to_owned());
            }
            let value = arguments
                .get(index)
                .with_context(|| format!("{flag} requires a value"))?;
            index += 1;
            Ok(value.clone())
        };
        match flag {
            "--project" => result.project = Some(PathBuf::from(value()?)),
            "--ty" | "--plugin" | "--manifest" | "--stubs" | "--checker-mode" => {
                result.options.insert(flag[2..].into(), value()?);
            }
            "--config-file" => {
                let path = PathBuf::from(value()?);
                ensure!(
                    path.file_name().is_none_or(|name| name != "pyproject.toml"),
                    "--config-file requires a ty.toml file, not pyproject.toml"
                );
                result.options.insert(
                    "ty-config".into(),
                    path.canonicalize()?.to_string_lossy().into_owned(),
                );
            }
            "--config" | "-c" => result.configs.push(value()?),
            "--python" | "--typeshed" | "--python-version" | "--python-platform" => {
                let value = value()?;
                result
                    .explicit
                    .push(format!("environment.{}={}", &flag[2..], quoted(&value)));
            }
            "--extra-search-path" => result.extras.push(value()?),
            "--exclude" => result.excludes.push(value()?),
            "--error" | "--warn" | "--ignore" => result.explicit.push(format!(
                "rules.{}={}",
                quoted(&value()?),
                quoted(&flag[2..])
            )),
            "--output-format" => result.format = Some(value()?),
            "--color" => result.color = value()?,
            "--error-on-warning" => {
                warning_flag = true;
                result.warning_exit = Some(match inline {
                    Some("false") => false,
                    Some("true") | None => true,
                    Some(value) => {
                        bail!("invalid --error-on-warning value {value:?}; expected true or false")
                    }
                });
            }
            "--exit-zero" => {
                ensure!(inline.is_none(), "{flag} does not take a value");
                result.exit_zero = true;
            }
            "--exit-zero-on-warning" => {
                ensure!(inline.is_none(), "{flag} does not take a value");
                zero_warning_flag = true;
                result.warning_exit = Some(false);
            }
            "--force-exclude" | "--no-force-exclude" => {
                ensure!(inline.is_none(), "{flag} does not take a value");
                result.force_exclude = flag == "--force-exclude";
            }
            "--respect-ignore-files" | "--no-respect-ignore-files" => {
                let enabled = optional_bool(flag, inline, flag == "--respect-ignore-files")?;
                result
                    .explicit
                    .push(format!("src.respect-ignore-files={}", enabled))
            }
            "--exclude-scripts" | "--include-scripts" => result.explicit.push(format!(
                "src.exclude-scripts={}",
                optional_bool(flag, inline, flag == "--exclude-scripts")?
            )),
            "--no-progress" => ensure!(inline.is_none(), "{flag} does not take a value"),
            "--fix" | "--add-ignore" | "--watch" | "-W" => {
                ensure!(inline.is_none(), "{flag} does not take a value");
                match flag {
                    "--fix" => result.fix = true,
                    "--add-ignore" => result.add_ignore = true,
                    _ => result.watch = true,
                }
            }
            _ => bail!("unknown check option {flag:?}; use --help"),
        }
    }
    ensure!(
        !(result.fix && result.add_ignore),
        "--fix conflicts with --add-ignore"
    );
    ensure!(
        !(warning_flag && (result.exit_zero || zero_warning_flag)),
        "--error-on-warning conflicts with --exit-zero and --exit-zero-on-warning"
    );
    ensure!(
        matches!(result.color.as_str(), "auto" | "always" | "never"),
        "--color expects auto, always or never"
    );
    if !result.extras.is_empty() {
        result.explicit.push(format!(
            "environment.extra-paths={}",
            serde_json::to_string(&result.extras)?
        ));
    }
    if !result.excludes.is_empty() {
        result.explicit.push(format!(
            "src.exclude={}",
            serde_json::to_string(&result.excludes)?
        ));
    }
    // Upstream named CLI options take precedence over every --config value,
    // independently of their relative positions on the command line.
    result.configs.append(&mut result.explicit);
    if let Some(value) = &result.format {
        ensure!(valid_format(value), "unsupported output format {value:?}");
    }
    Ok(result)
}

fn valid_format(value: &str) -> bool {
    matches!(value, "full" | "concise" | "gitlab" | "github" | "junit")
}

fn optional_bool(flag: &str, value: Option<&str>, enabled: bool) -> Result<bool> {
    match value {
        None => Ok(enabled),
        Some("true") if enabled => Ok(true),
        Some("false") if enabled => Ok(false),
        _ => bail!("invalid boolean value for {flag}"),
    }
}

fn project_root(start: &Path) -> Result<PathBuf> {
    let start = start.canonicalize()?;
    ensure!(
        start.is_dir(),
        "project path must be a directory: {}",
        start.display()
    );
    for directory in start.ancestors() {
        if directory.join("ty.toml").is_file() || directory.join("pyproject.toml").is_file() {
            return Ok(directory.to_owned());
        }
    }
    Ok(start)
}

fn executable(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    let resolved = if path.components().count() > 1 || path.is_absolute() {
        path.canonicalize()
            .with_context(|| format!("official ty executable {value:?}"))?
    } else {
        let candidates = std::env::var_os("PATH").context("PATH is unset; pass --ty")?;
        std::env::split_paths(&candidates)
            .map(|root| {
                root.join(if cfg!(windows) && !value.ends_with(".exe") {
                    format!("{value}.exe")
                } else {
                    value.to_owned()
                })
            })
            .find(|path| path.is_file())
            .with_context(|| {
                format!("official ty executable {value:?} not found; install ty or pass --ty")
            })?
            .canonicalize()?
    };
    ensure!(
        resolved != std::env::current_exe()?.canonicalize()?,
        "--ty must point to official ty, not this companion executable"
    );
    Ok(resolved)
}

struct Temporary(PathBuf);
impl Temporary {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().canonicalize()?;
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let path = root.join(format!("ty-companion-check-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn severity(diagnostic: &Value) -> &'static str {
    match diagnostic["severity"].as_str() {
        Some("minor") => "warning",
        Some("info") => "info",
        _ => "error",
    }
}
fn message(diagnostic: &Value) -> &str {
    let message = diagnostic["description"].as_str().unwrap_or("");
    let code = diagnostic["check_name"].as_str().unwrap_or("");
    message
        .strip_prefix(code)
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(message)
}
fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
fn annotation(value: &str, property: bool) -> String {
    let value = value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A");
    if property {
        value.replace(':', "%3A").replace(',', "%2C")
    } else {
        value
    }
}

fn display_source(path: &str, cell: Option<u64>) -> Option<String> {
    let source = std::fs::read_to_string(path).ok()?;
    let Some(cell) = cell else {
        return Some(source);
    };
    let notebook: Value = serde_json::from_str(&source).ok()?;
    let source = &notebook["cells"].get(cell.checked_sub(1)? as usize)?["source"];
    match source {
        Value::String(source) => Some(source.clone()),
        Value::Array(lines) => lines
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .map(|lines| lines.concat()),
        _ => None,
    }
}

fn related_description(related: &Value, cwd: &Path) -> String {
    let message = related["message"].as_str().unwrap_or("");
    let Some(path) = related["location"]["uri"]
        .as_str()
        .and_then(|uri| crate::lsp_server::uri_path(uri).ok())
    else {
        return message.into();
    };
    let display = path.strip_prefix(cwd).unwrap_or(&path).display();
    let line = related["location"]["range"]["start"]["line"]
        .as_u64()
        .unwrap_or(0)
        + 1;
    let column = related["location"]["range"]["start"]["character"]
        .as_u64()
        .unwrap_or(0)
        + 1;
    format!("{display}:{line}:{column}: {message}")
}

fn render(
    diagnostics: &[Value],
    format: &str,
    quiet: usize,
    color: bool,
    cwd: &Path,
) -> Result<String> {
    if quiet >= 2 {
        return Ok(String::new());
    }
    if format == "gitlab" {
        use sha2::{Digest, Sha256};
        let mut seen = BTreeMap::<String, usize>::new();
        let public = diagnostics
            .iter()
            .map(|diagnostic| {
                let mut diagnostic = diagnostic.clone();
                if let Some(fields) = diagnostic.as_object_mut() {
                    fields.retain(|name, _| {
                        matches!(
                            name.as_str(),
                            "check_name" | "description" | "severity" | "fingerprint" | "location"
                        )
                    });
                }
                if diagnostic.get("fingerprint").is_none() {
                    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&diagnostic)?));
                    let ordinal = seen.entry(digest.clone()).or_default();
                    let fingerprint = if *ordinal == 0 {
                        digest
                    } else {
                        format!("{:x}", Sha256::digest(format!("{digest}:{ordinal}")))
                    };
                    *ordinal += 1;
                    diagnostic["fingerprint"] = Value::String(fingerprint);
                }
                if diagnostic["location"].get("cell").is_some() {
                    diagnostic["location"]["positions"] = serde_json::json!({"begin":{"line":1,"column":1},"end":{"line":1,"column":1}});
                    if let Some(location) = diagnostic["location"].as_object_mut() { location.retain(|name, _| matches!(name.as_str(), "path" | "positions")); }
                }
                Ok(diagnostic)
            })
            .collect::<Result<Vec<_>>>()?;
        return Ok(format!("{}\n", serde_json::to_string_pretty(&public)?));
    }
    if format == "junit" {
        // The upstream renderer groups cases by source file and includes rule,
        // severity, and original start coordinates as structured XML fields.
        let mut output = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites name=\"ty\" tests=\"{}\" failures=\"{}\" errors=\"0\">\n",
            diagnostics.len().max(1),
            diagnostics.len()
        );
        if diagnostics.is_empty() {
            output.push_str("  <testsuite name=\"ty\" package=\"org.ty\" tests=\"1\" failures=\"0\" errors=\"0\"><testcase name=\"No errors found\" classname=\"ty\"/></testsuite>\n");
        } else {
            let mut files = BTreeMap::<&str, Vec<&Value>>::new();
            for diagnostic in diagnostics {
                files
                    .entry(diagnostic["location"]["path"].as_str().unwrap_or(""))
                    .or_default()
                    .push(diagnostic);
            }
            for (path, diagnostics) in files {
                output.push_str(&format!("  <testsuite name=\"{}\" package=\"org.ty\" tests=\"{}\" failures=\"{}\" errors=\"0\">\n", xml(path), diagnostics.len(), diagnostics.len()));
                for diagnostic in diagnostics {
                    let code = diagnostic["check_name"].as_str().unwrap_or("");
                    let line = diagnostic["location"]["positions"]["begin"]["line"]
                        .as_u64()
                        .unwrap_or(1);
                    let column = diagnostic["location"]["positions"]["begin"]["column"]
                        .as_u64()
                        .unwrap_or(1);
                    let (coordinates, description) =
                        if path.is_empty() || diagnostic["location"].get("cell").is_some() {
                            (String::new(), message(diagnostic).to_owned())
                        } else {
                            (
                                format!(" line=\"{line}\" column=\"{column}\""),
                                format!("line {line}, col {column}, {}", message(diagnostic)),
                            )
                        };
                    output.push_str(&format!("    <testcase name=\"org.ty.{}\" classname=\"{}\"{coordinates}><properties><property name=\"severity\" value=\"{}\"/></properties><failure message=\"{}\">{}</failure></testcase>\n", xml(code), xml(&Path::new(path).with_extension("").to_string_lossy()), severity(diagnostic), xml(message(diagnostic)), xml(&description)));
                }
                output.push_str("  </testsuite>\n");
            }
        }
        output.push_str("</testsuites>\n");
        return Ok(output);
    }
    let mut output = String::new();
    for diagnostic in diagnostics {
        let location = &diagnostic["location"];
        let path = location["path"].as_str().unwrap_or("");
        let display = Path::new(path)
            .strip_prefix(cwd)
            .unwrap_or(Path::new(path))
            .display();
        let cell = location["cell"].as_u64();
        let fallback =
            serde_json::json!({"begin":{"line":1,"column":1},"end":{"line":1,"column":1}});
        let positions = if cell.is_some() {
            if format == "github" {
                &fallback
            } else {
                &location["cell_positions"]
            }
        } else {
            &location["positions"]
        };
        let line = positions["begin"]["line"].as_u64().unwrap_or(1);
        let column = positions["begin"]["column"].as_u64().unwrap_or(1);
        let displayed_location = match cell.filter(|_| format != "github") {
            Some(cell) => format!("{display}:cell {cell}:{line}:{column}"),
            None => format!("{display}:{line}:{column}"),
        };
        let code = diagnostic["check_name"].as_str().unwrap_or("");
        let level = severity(diagnostic);
        if path.is_empty() {
            // Option diagnostics supplied by CLI/editor settings have no
            // primary source. Preserve that absence in every public format.
            if format == "github" {
                let level = if level == "info" { "notice" } else { level };
                let mut body = format!("{code}: {}", message(diagnostic));
                if let Some(details) = diagnostic["details"].as_str() {
                    for line in details.lines() {
                        body.push_str(&format!("\n  {line}"));
                    }
                }
                output.push_str(&format!(
                    "::{level} title=ty ({})::{}\n",
                    annotation(code, true),
                    annotation(&body, false)
                ));
            } else if format == "concise" {
                output.push_str(&format!("{level}[{code}] {}\n", message(diagnostic)));
            } else {
                output.push_str(&format!("{level}[{code}]: {}\n", message(diagnostic)));
                if let Some(details) = diagnostic["details"].as_str().filter(|s| !s.is_empty()) {
                    output.push_str(details);
                    output.push('\n');
                }
                output.push('\n');
            }
            continue;
        }
        if format == "github" {
            let level = if level == "info" { "notice" } else { level };
            let end_line = positions["end"]["line"].as_u64().unwrap_or(line);
            let end_column = positions["end"]["column"].as_u64().unwrap_or(column);
            let coordinates = if line == end_line {
                format!("line={line},col={column},endLine={end_line},endColumn={end_column}")
            } else {
                format!("line={line},endLine={end_line}")
            };
            let mut body = format!("{display}:{line}:{column}: {code}: {}", message(diagnostic));
            if let Some(details) = diagnostic["details"]
                .as_str()
                .filter(|details| !details.is_empty())
            {
                body.push_str(&format!("\n{details}"));
            }
            for related in diagnostic["related"].as_array().into_iter().flatten() {
                body.push_str(&format!("\n  {}", related_description(related, cwd)));
            }
            output.push_str(&format!(
                "::{level} title=ty ({}),file={},{coordinates}::{}\n",
                annotation(code, true),
                annotation(path, true),
                annotation(&body, false)
            ));
        } else if format == "concise" {
            output.push_str(&format!(
                "{displayed_location}: {level}[{code}] {}\n",
                message(diagnostic)
            ));
        } else {
            let label = format!("{level}[{code}]");
            let label = if color {
                format!(
                    "\x1b[1;{}m{label}\x1b[0m",
                    if level == "warning" { 33 } else { 31 }
                )
            } else {
                label
            };
            output.push_str(&format!(
                "{label}: {}\n --> {displayed_location}\n",
                message(diagnostic)
            ));
            let source_line = location["source_line"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    display_source(path, cell).and_then(|source| {
                        source
                            .lines()
                            .nth(line.saturating_sub(1) as usize)
                            .map(str::to_owned)
                    })
                });
            if let Some(text) = source_line {
                let prefix = format!("{line} | ");
                output.push_str(&format!(
                    "{prefix}{text}\n{}{}^\n",
                    " ".repeat(prefix.len()),
                    " ".repeat(column.saturating_sub(1) as usize)
                ));
            }
            for related in diagnostic["related"].as_array().into_iter().flatten() {
                output.push_str(&format!("  {}\n", related_description(related, cwd)));
            }
            if let Some(details) = diagnostic["details"].as_str() {
                for line in details.lines() {
                    output.push_str(&format!("  {line}\n"));
                }
            }
            output.push('\n');
        }
    }
    if matches!(format, "full" | "concise") && quiet == 0 {
        if diagnostics.is_empty() {
            output.push_str("All checks passed!\n");
        } else {
            output.push_str(&format!(
                "Found {} diagnostic{}\n",
                diagnostics.len(),
                if diagnostics.len() == 1 { "" } else { "s" }
            ));
        }
    }
    Ok(output)
}

pub fn check(arguments: &[String]) -> Result<i32> {
    let mut arguments = parse(arguments)?;
    if arguments.help {
        print!("{HELP}");
        return Ok(0);
    }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let root = project_root(arguments.project.as_deref().unwrap_or(&cwd))?;
    let temporary = Temporary::new()?;
    let checker = executable(
        arguments
            .options
            .get("ty")
            .map(String::as_str)
            .unwrap_or(&std::env::var("TY_COMPANION_TY").unwrap_or_else(|_| "ty".into())),
    )?;
    arguments
        .options
        .insert("ty".into(), checker.to_string_lossy().into_owned());
    arguments
        .options
        .insert("out".into(), temporary.0.to_string_lossy().into_owned());
    arguments.options.insert("public-cli".into(), "true".into());
    arguments
        .options
        .entry("checker-mode".into())
        .or_insert_with(|| "push".into());
    if !arguments.options.contains_key("ty-config")
        && let Some(path) = std::env::var_os("TY_CONFIG_FILE")
    {
        arguments.options.insert(
            "ty-config".into(),
            PathBuf::from(path)
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
        );
    }
    arguments.options.insert(
        "cli-config".into(),
        serde_json::to_string(&arguments.configs)?,
    );
    if !arguments.paths.is_empty() {
        let paths = arguments
            .paths
            .iter()
            .map(|path| {
                let absolute = cwd
                    .join(path)
                    .canonicalize()
                    .with_context(|| format!("check path {}", path.display()))?;
                Ok(absolute)
            })
            .collect::<Result<Vec<_>>>()?;
        let names = serde_json::to_string(&paths)?;
        let selected = temporary.0.join("selected.json");
        std::fs::write(&selected, &names)?;
        arguments
            .options
            .insert("files".into(), selected.to_string_lossy().into_owned());
        arguments.options.insert("explicit-files".into(), names);
    }
    arguments
        .options
        .insert("force-exclude".into(), arguments.force_exclude.to_string());
    let mut session = crate::project_check::Session::new(&root, &arguments.options)?;
    if arguments.watch {
        watch(&mut session, &arguments, &cwd)
    } else {
        check_once(&mut session, &arguments, &cwd)
    }
}

/// Native events wake a single persistent analysis session. Watching containing
/// directories also observes editor atomic-save renames and new dependencies.
fn watch(
    session: &mut crate::project_check::Session,
    arguments: &Arguments,
    cwd: &Path,
) -> Result<i32> {
    use notify::{EventKind, RecursiveMode, Watcher};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::{Duration, Instant};
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = interrupted.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))
        .context("install watch interruption handler")?;
    let (sender, receiver) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = sender.send(event);
    })
    .context("start native filesystem watcher")?;
    let mut watched = BTreeMap::<PathBuf, RecursiveMode>::new();
    let sync = |watcher: &mut notify::RecommendedWatcher,
                watched: &mut BTreeMap<PathBuf, RecursiveMode>,
                session: &crate::project_check::Session|
     -> Result<()> {
        let mut requested = BTreeMap::new();
        for mut path in session.watch_paths() {
            let recursive = if path.is_dir() {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
            };
            if !path.is_dir() {
                path.pop();
            }
            while !path.is_dir() && path.pop() {}
            requested
                .entry(path)
                .and_modify(|old| {
                    if recursive == RecursiveMode::Recursive {
                        *old = recursive;
                    }
                })
                .or_insert(recursive);
        }
        // A recursive ancestor already covers a more specific registration.
        let roots = requested.clone();
        requested.retain(|path, _| {
            !roots.iter().any(|(parent, mode)| {
                parent != path && *mode == RecursiveMode::Recursive && path.starts_with(parent)
            })
        });
        for (path, mode) in &requested {
            if watched.get(path) != Some(mode) {
                if watched.contains_key(path) {
                    watcher.unwatch(path)?;
                }
                watcher
                    .watch(path, *mode)
                    .with_context(|| format!("watch {}", path.display()))?;
            }
        }
        for path in watched.keys().filter(|path| !requested.contains_key(*path)) {
            watcher.unwatch(path)?;
        }
        *watched = requested;
        Ok(())
    };
    sync(&mut watcher, &mut watched, session)?;
    check_once(session, arguments, cwd)?;
    sync(&mut watcher, &mut watched, session)?;
    if arguments.quiet == 0 {
        eprintln!("Watching for file changes. Press Ctrl+C to stop.");
    }
    loop {
        if interrupted.load(Ordering::Relaxed) {
            return Ok(130);
        }
        let first = match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(event) => event.context("filesystem watch event")?,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("filesystem watcher stopped unexpectedly")
            }
        };
        if arguments.verbose > 1 {
            eprintln!("Filesystem event: {first:?}");
        }
        let mut events = vec![first];
        let start = Instant::now();
        // Coalesce a save's rename/write events without indefinitely postponing
        // checking during continuous changes. No filesystem polling occurs idle.
        while start.elapsed() < Duration::from_millis(500) && !interrupted.load(Ordering::Relaxed) {
            match receiver.recv_timeout(Duration::from_millis(75)) {
                Ok(event) => events.push(event.context("filesystem watch event")?),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("filesystem watcher stopped unexpectedly")
                }
            }
        }
        if interrupted.load(Ordering::Relaxed) {
            return Ok(130);
        }
        let mut relevant = false;
        let mut reload = false;
        let mut dependency_events = Vec::new();
        for event in events {
            if matches!(event.kind, EventKind::Access(_)) {
                continue;
            }
            if event.need_rescan() || event.paths.is_empty() {
                relevant = true;
                reload = true;
            }
            for path in event.paths {
                let dependency = session.path_requires_reload(&path);
                if dependency {
                    let change = match event.kind {
                        EventKind::Create(_) => 1,
                        EventKind::Remove(_) => 3,
                        _ => 2,
                    };
                    dependency_events.push(
                        serde_json::json!({"uri":crate::checker::file_uri(&path)?,"type":change}),
                    );
                }
                reload |= dependency;
                relevant |= dependency
                    || path.is_dir()
                    || matches!(
                        path.extension().and_then(|e| e.to_str()),
                        Some("py" | "pyi" | "ipynb")
                    )
                    || matches!(
                        event.kind,
                        EventKind::Remove(notify::event::RemoveKind::Folder)
                    );
            }
        }
        if relevant {
            if !dependency_events.is_empty()
                && let Some(checker) = session.checker_mut()
            {
                checker.notify(
                    "workspace/didChangeWatchedFiles",
                    serde_json::json!({"changes":dependency_events}),
                )?;
            }
            if reload {
                session.invalidate();
            }
            check_once(session, arguments, cwd)?;
            sync(&mut watcher, &mut watched, session)?;
        }
    }
}

fn check_once(
    session: &mut crate::project_check::Session,
    arguments: &Arguments,
    cwd: &Path,
) -> Result<i32> {
    let result = session.check(&BTreeMap::new())?;
    let (mut result, fixes) = if arguments.fix || arguments.add_ignore {
        let mode = if arguments.fix {
            crate::cli_edits::Mode::Fix
        } else {
            crate::cli_edits::Mode::AddIgnore
        };
        let outcome = crate::cli_edits::apply(session, result, mode)?;
        (outcome.result, outcome.count)
    } else {
        (result, 0)
    };
    let terminal = session.terminal_options();
    let format = arguments
        .format
        .clone()
        .or_else(|| std::env::var("TY_OUTPUT_FORMAT").ok())
        .or_else(|| terminal["output-format"].as_str().map(str::to_owned))
        .unwrap_or_else(|| "full".into());
    ensure!(
        valid_format(&format),
        "unsupported output format {format:?}"
    );
    let warning_exit = arguments
        .warning_exit
        .or_else(|| terminal["error-on-warning"].as_bool())
        .unwrap_or(true);
    if matches!(format.as_str(), "full" | "github")
        && result["diagnostics"]
            .as_array()
            .is_some_and(|diagnostics| !diagnostics.is_empty())
    {
        let rich = session.rich_diagnostics()?;
        crate::lsp_mapping::merge_cli_diagnostic_details(
            result["diagnostics"]
                .as_array_mut()
                .context("checker diagnostics missing")?,
            &rich,
            &session.documents(),
            session.names(),
            crate::lsp_mapping::ProjectDisplayPaths {
                projected_root: session.projected_root(),
                original_root: session.original_root(),
                projected_stubs: session.projected_stubs(),
                generated_root: session.generated_root(),
            },
        )?;
    }
    crate::notebook::cli_locations(
        result["diagnostics"]
            .as_array_mut()
            .context("checker diagnostics missing")?,
    )?;
    let diagnostics = result["diagnostics"]
        .as_array()
        .context("checker diagnostics missing")?;
    let color = arguments.color == "always"
        || (arguments.color == "auto" && std::io::stdout().is_terminal());
    let output = render(diagnostics, &format, arguments.quiet, color, cwd)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(output.as_bytes())?;
    stdout.flush()?;
    if fixes > 0 && arguments.quiet == 0 {
        eprintln!(
            "Applied {fixes} source edit{}",
            if fixes == 1 { "" } else { "s" }
        );
    }
    if arguments.verbose > 0 && arguments.quiet == 0 {
        eprintln!(
            "Checked in {:.3}s ({} Monty executions, {} cache hits)",
            result["seconds"].as_f64().unwrap_or(0.0),
            result
                .get("monty_executions_delta")
                .unwrap_or(&result["monty_executions"]),
            result
                .get("monty_cache_hits_delta")
                .unwrap_or(&result["monty_cache_hits"])
        );
    }
    Ok(if arguments.exit_zero {
        0
    } else {
        exit_code(diagnostics, warning_exit)
    })
}

fn exit_code(diagnostics: &[Value], warning_exit: bool) -> i32 {
    // Same ordering as upstream exit_status_from_diagnostics: fatal outranks IO,
    // which outranks ordinary errors and the configurable warning policy.
    if diagnostics
        .iter()
        .any(|d| matches!(d["severity"].as_str(), Some("critical" | "blocker")))
    {
        101
    } else if diagnostics.iter().any(|d| d["check_name"] == "io") {
        2
    } else {
        i32::from(
            diagnostics
                .iter()
                .any(|d| d["severity"] == "major" || (warning_exit && d["severity"] == "minor")),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn arguments(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn command_line_preserves_option_precedence_and_rejects_unknown_semantics() -> Result<()> {
        let parsed = parse(&arguments(&[
            "--warn",
            "invalid-assignment",
            "--config",
            "rules.invalid-assignment='ignore'",
            "--extra-search-path",
            "a",
            "--extra-search-path=b",
            "--",
            "--source.py",
        ]))?;
        assert_eq!(parsed.configs[0], "rules.invalid-assignment='ignore'");
        assert_eq!(parsed.configs[1], "rules.\"invalid-assignment\"=\"warn\"");
        assert_eq!(parsed.paths, [PathBuf::from("--source.py")]);
        assert!(parse(&arguments(&["--exit-zero", "--error-on-warning"])).is_err());
        assert!(parse(&arguments(&["--fix"]))?.fix);
        assert!(parse(&arguments(&["--fix", "--add-ignore"])).is_err());
        assert!(parse(&arguments(&["-W"]))?.watch);
        assert!(parse(&arguments(&["--unknown"])).is_err());
        assert_eq!(
            parse(&arguments(&["--error-on-warning=false"]))?.warning_exit,
            Some(false)
        );
        Ok(())
    }

    #[test]
    fn diagnostic_exit_policy_preserves_fatal_io_and_warning_priority() {
        let io = json!({"check_name":"io","severity":"major"});
        let fatal = json!({"check_name":"internal-error","severity":"critical"});
        assert_eq!(exit_code(&[io.clone(), fatal], false), 101);
        assert_eq!(exit_code(&[io], false), 2);
        let warning = json!({"check_name":"unused-ignore-comment","severity":"minor"});
        assert_eq!(exit_code(std::slice::from_ref(&warning), false), 0);
        assert_eq!(exit_code(&[warning], true), 1);
    }

    #[test]
    fn notebook_formats_use_cells_for_people_and_upstream_machine_coordinates() -> Result<()> {
        let diagnostic = json!({"check_name":"invalid-assignment","description":"invalid-assignment: bad type","severity":"major","location":{"path":"/project/book.ipynb","cell":3,"cell_positions":{"begin":{"line":2,"column":14},"end":{"line":2,"column":19}},"source_line":"value: int = 'bad'","positions":{"begin":{"line":5,"column":14},"end":{"line":5,"column":19}}}});
        for format in ["full", "concise"] {
            let rendered = render(
                std::slice::from_ref(&diagnostic),
                format,
                0,
                false,
                Path::new("/project"),
            )?;
            assert!(rendered.contains("book.ipynb:cell 3:2:14"), "{rendered}");
            if format == "full" {
                assert!(rendered.contains("2 | value: int = 'bad'"));
            }
        }
        let gitlab: Value = serde_json::from_str(&render(
            std::slice::from_ref(&diagnostic),
            "gitlab",
            0,
            false,
            Path::new("/project"),
        )?)?;
        assert_eq!(
            gitlab[0]["location"]["positions"]["begin"],
            json!({"line":1,"column":1})
        );
        assert!(gitlab[0]["location"].get("cell").is_none());
        assert!(gitlab[0]["location"].get("source_line").is_none());
        let github = render(
            std::slice::from_ref(&diagnostic),
            "github",
            0,
            false,
            Path::new("/project"),
        )?;
        assert!(github.contains("line=1,col=1,endLine=1,endColumn=1"));
        let junit = render(&[diagnostic], "junit", 0, false, Path::new("/project"))?;
        assert!(!junit.contains(" line=\"") && !junit.contains("line 5, col"));
        Ok(())
    }

    #[test]
    fn machine_formats_escape_original_diagnostics() -> Result<()> {
        let diagnostics = vec![
            json!({"check_name":"invalid-assignment","description":"invalid-assignment: expected <int> & got \"str\"\nnext","severity":"major","location":{"path":"/project/a,b.py","positions":{"begin":{"line":2,"column":3}}}}),
        ];
        let junit = render(&diagnostics, "junit", 0, false, Path::new("/project"))?;
        assert!(junit.contains("&lt;int&gt; &amp; got &quot;str&quot;"));
        let github = render(&diagnostics, "github", 0, false, Path::new("/project"))?;
        assert!(github.contains("file=/project/a%2Cb.py"));
        assert!(github.contains("%0Anext"));
        let mut gitlab: Value = serde_json::from_str(&render(
            &diagnostics,
            "gitlab",
            0,
            false,
            Path::new("/project"),
        )?)?;
        assert!(
            gitlab[0]["fingerprint"]
                .as_str()
                .is_some_and(|value| value.len() == 64)
        );
        gitlab[0].as_object_mut().unwrap().remove("fingerprint");
        assert_eq!(gitlab, json!(diagnostics));
        let mut internal = diagnostics[0].clone();
        internal["lsp_diagnostic"] =
            json!({"message":"_ty_ext_call_0","uri":"file:///generated/project.py"});
        internal["lsp_message"] = json!("internal transport state");
        let public = render(&[internal], "gitlab", 0, false, Path::new("/project"))?;
        assert!(
            !public.contains("lsp_")
                && !public.contains("generated")
                && !public.contains("_ty_ext_")
        );
        Ok(())
    }
}
