//! Apply opt-in CLI edits to original files, never to the projected project.
//!
//! Upstream `ty_python_semantic::fixes` applies safe fixes and rechecks for at
//! most ten rounds. Its LSP diagnostic metadata exposes the same safe flag.
//! Suppressions are computed by ty against original source so multiline strings,
//! continuations, existing comments, and rule names use its actual parser.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{
    checker::{ProjectChecker, file_uri},
    lsp_mapping::{
        DocumentMap, LspSourceMaps, PositionEncoding, decode_file_uri, offset_to_position,
        position_to_offset,
    },
    project_check::{ProjectDocument, Session},
};

#[derive(Clone, Copy)]
pub enum Mode {
    Fix,
    AddIgnore,
}
pub struct Outcome {
    pub result: Value,
    pub count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
struct Edit {
    start: usize,
    end: usize,
    text: String,
}

fn parse_edits(text: &str, edits: &[Value]) -> Result<Vec<Edit>> {
    edits
        .iter()
        .map(|edit| {
            let start = position_to_offset(text, &edit["range"]["start"], PositionEncoding::Utf32)?;
            let end = position_to_offset(text, &edit["range"]["end"], PositionEncoding::Utf32)?;
            ensure!(start <= end, "reversed CLI edit");
            Ok(Edit {
                start,
                end,
                text: edit["newText"].as_str().context("CLI edit text")?.into(),
            })
        })
        .collect()
}
fn overlaps(left: &Edit, right: &Edit) -> bool {
    if left == right {
        return false;
    }
    if left.start == left.end && right.start == right.end {
        return left.start == right.start;
    }
    left.start < right.end && right.start < left.end
        || (left.start == left.end && right.start <= left.start && left.start < right.end)
        || (right.start == right.end && left.start <= right.start && right.start < left.end)
}
fn edited(source: &str, edits: &[Edit]) -> Result<String> {
    let sorted: BTreeSet<_> = edits.iter().cloned().collect();
    let sorted: Vec<_> = sorted.into_iter().collect();
    ensure!(
        sorted.windows(2).all(|pair| !overlaps(&pair[0], &pair[1])),
        "overlapping CLI edit transaction"
    );
    let mut output = source.to_owned();
    for edit in sorted.iter().rev() {
        ensure!(
            source.get(edit.start..edit.end).is_some(),
            "CLI edit splits original source"
        );
        output.replace_range(edit.start..edit.end, &edit.text);
    }
    Ok(output)
}
#[derive(Clone)]
struct Snapshot {
    raw: String,
    notebook: Option<ruff_notebook::Notebook>,
}
impl Snapshot {
    fn capture(document: &ProjectDocument) -> Result<Self> {
        let path = Path::new(&document.source.path);
        let raw = std::fs::read_to_string(path)?;
        let notebook = if path.extension().is_some_and(|ext| ext == "ipynb") {
            Some(ruff_notebook::Notebook::from_source_code(&raw)?)
        } else {
            None
        };
        let source = notebook.as_ref().map_or(raw.as_str(), |notebook| {
            if notebook.is_python_notebook() {
                notebook.source_code()
            } else {
                ""
            }
        });
        ensure!(
            source == document.source.text,
            "{} changed while computing fixes; no files were changed",
            path.display()
        );
        Ok(Self { raw, notebook })
    }
    fn valid(&self, source: &str) -> bool {
        self.notebook.as_ref().map_or_else(
            || ruff_python_parser::parse_module(source).is_ok(),
            |notebook| {
                ruff_python_parser::parse_cells_unchecked(
                    source,
                    notebook.cell_offsets().content_ranges(),
                    &ruff_python_parser::ParseOptions::from(ruff_python_ast::PySourceType::Ipynb),
                )
                .has_valid_syntax()
            },
        )
    }
    fn apply(&mut self, source: &str, edits: &[Edit]) -> Result<String> {
        let transformed = edited(source, edits)?;
        let Some(notebook) = &mut self.notebook else {
            return Ok(transformed);
        };
        let mut map = ruff_diagnostics::SourceMap::default();
        let mut delta = 0_i128;
        for edit in edits.iter().cloned().collect::<BTreeSet<_>>() {
            let destination = usize::try_from(edit.start as i128 + delta)?;
            map.push_marker(
                ruff_text_size::TextSize::try_from(edit.start)?,
                ruff_text_size::TextSize::try_from(destination)?,
            );
            delta += edit.text.len() as i128 - (edit.end - edit.start) as i128;
            let destination = usize::try_from(edit.end as i128 + delta)?;
            map.push_marker(
                ruff_text_size::TextSize::try_from(edit.end)?,
                ruff_text_size::TextSize::try_from(destination)?,
            );
        }
        notebook.update(&map, transformed);
        Ok(notebook.source_code().to_owned())
    }
    fn serialize(&self, source: String) -> Result<String> {
        if let Some(notebook) = &self.notebook {
            let mut bytes = vec![];
            notebook.write(&mut bytes)?;
            Ok(String::from_utf8(bytes)?)
        } else {
            Ok(source)
        }
    }
}

fn source_maps(documents: &[ProjectDocument], session: &Session) -> Result<LspSourceMaps> {
    let mut maps = LspSourceMaps::new(PositionEncoding::Utf32, PositionEncoding::Utf32);
    for (generated, original) in session.names() {
        maps.add_name(generated, original);
    }
    for document in documents {
        maps.add_hidden_prefix(document.source.prefix.clone());
        maps.add_generated_root(file_uri(
            document.path.parent().context("projection parent")?,
        )?);
        maps.insert(
            DocumentMap::new(
                file_uri(Path::new(&document.source.path))?,
                file_uri(&document.path)?,
                document.source.text.clone(),
                document.rendered.clone(),
            )
            .with_names(document.names.clone())
            .with_module(document.source.module.clone()),
        );
    }
    Ok(maps)
}

fn fix_edits(
    documents: &[ProjectDocument],
    snapshots: &BTreeMap<PathBuf, Snapshot>,
    session: &Session,
    result: &Value,
) -> Result<(BTreeMap<PathBuf, String>, usize)> {
    let maps = source_maps(documents, session)?;
    let sources: BTreeMap<_, _> = documents
        .iter()
        .map(|document| {
            (
                PathBuf::from(&document.source.path),
                document.source.text.as_str(),
            )
        })
        .collect();
    let mut pending = BTreeMap::<PathBuf, Vec<Edit>>::new();
    let mut count = 0;
    for diagnostic in result["diagnostics"]
        .as_array()
        .context("CLI diagnostics")?
    {
        let data = &diagnostic["lsp_diagnostic"]["data"];
        let fix = data.get("fix").unwrap_or(data);
        if fix["preferred"] != true {
            continue;
        }
        let Some(changes) = fix["edits"].as_object() else {
            continue;
        };
        // A safe upstream fix may still reference a synthetic declaration.
        // The complete fix is then inapplicable; leave its diagnostic visible.
        let Ok(mapped) = maps.workspace_edit(&json!({"changes":changes})) else {
            continue;
        };
        let mut transaction = BTreeMap::new();
        let mut applicable = true;
        for (uri, edits) in mapped["changes"].as_object().context("mapped fixes")? {
            let path = decode_file_uri(uri)?.context("fix target is not a local file")?;
            let Some(source) = sources.get(&path) else {
                applicable = false;
                break;
            };
            if !snapshots[&path].valid(source) {
                applicable = false;
                break;
            }
            let edits = parse_edits(source, edits.as_array().context("fix edit list")?)?;
            if pending.get(&path).is_some_and(|prior| {
                prior
                    .iter()
                    .any(|left| edits.iter().any(|right| overlaps(left, right)))
            }) {
                applicable = false;
                break;
            }
            transaction.insert(path, edits);
        }
        if applicable && !transaction.is_empty() {
            for (path, edits) in transaction {
                pending.entry(path).or_default().extend(edits);
            }
            count += 1;
        }
    }
    let mut output = BTreeMap::new();
    for (path, edits) in pending {
        let mut snapshot = snapshots[&path].clone();
        let text = snapshot.apply(sources[&path], &edits)?;
        ensure!(
            snapshot.valid(&text),
            "mapped safe fixes introduce invalid syntax in {}; no files were changed",
            path.display()
        );
        if text != sources[&path] {
            output.insert(path, snapshot.serialize(text)?);
        }
    }
    Ok((output, count))
}

fn diagnostic_span(source: &str, diagnostic: &Value) -> Result<(usize, usize)> {
    let position = |value: &Value| -> Result<usize> {
        let line = value["line"]
            .as_u64()
            .context("diagnostic line")?
            .checked_sub(1)
            .context("zero diagnostic line")?;
        let mut character = value["column"]
            .as_u64()
            .context("diagnostic column")?
            .checked_sub(1)
            .context("zero diagnostic column")?;
        if line == 0 && source.starts_with('\u{feff}') {
            character += 1;
        }
        position_to_offset(
            source,
            &json!({"line":line,"character":character}),
            PositionEncoding::Utf32,
        )
    };
    Ok((
        position(&diagnostic["location"]["positions"]["begin"])?,
        position(&diagnostic["location"]["positions"]["end"])?,
    ))
}

fn shifted(point: usize, edits: &[Edit]) -> Result<usize> {
    let mut point = point as i128;
    for edit in edits {
        if edit.end as i128 <= point {
            point += edit.text.len() as i128 - (edit.end - edit.start) as i128;
        } else {
            ensure!(
                edit.start as i128 >= point,
                "suppression unexpectedly changed diagnostic source"
            );
        }
    }
    usize::try_from(point).context("suppression offset overflow")
}

fn ignore_file(
    checker: &mut ProjectChecker,
    document: &ProjectDocument,
    snapshot: &Snapshot,
    diagnostics: &[&Value],
) -> Result<(String, usize)> {
    let source = &document.source.text;
    let path = document.path.with_file_name(format!(
        "__ty_ignore_{}.py",
        document
            .path
            .file_stem()
            .context("ignore source filename")?
            .to_string_lossy()
    ));
    let uri = file_uri(&path)?;
    let notebook_uri = file_uri(&path.with_extension("ipynb"))?;
    if snapshot.notebook.is_some() {
        checker.notify("notebookDocument/didOpen", json!({"notebookDocument":{"uri":notebook_uri,"notebookType":"jupyter-notebook","version":1,"cells":[{"kind":2,"document":uri}]},"cellTextDocuments":[{"uri":uri,"languageId":"python","version":1,"text":source}]}))?;
    } else {
        checker.notify(
            "textDocument/didOpen",
            json!({"textDocument":{"uri":uri,"languageId":"python","version":1,"text":source}}),
        )?;
    }
    let mut run = || -> Result<(String, usize)> {
        let mut text = source.clone();
        let mut snapshot = snapshot.clone();
        let mut previous = Vec::<Vec<Edit>>::new();
        let mut count = 0;
        let mut version = 1;
        let mut seen = BTreeSet::new();
        for diagnostic in diagnostics {
            let code = diagnostic["check_name"].as_str().unwrap_or("");
            if code == "unused-ignore-comment" {
                continue;
            }
            let (mut start, mut end) = diagnostic_span(source, diagnostic)?;
            if !seen.insert((start, end, code)) {
                continue;
            }
            for changes in &previous {
                start = shifted(start, changes)?;
                end = shifted(end, changes)?;
            }
            let range = json!({"start":offset_to_position(&text,start,PositionEncoding::Utf32)?,"end":offset_to_position(&text,end,PositionEncoding::Utf32)?});
            let raw = json!({"range":range,"code":code,"source":"ty","message":diagnostic["description"].as_str().unwrap_or("")});
            let actions=checker.request("textDocument/codeAction",json!({"textDocument":{"uri":uri},"range":range,"context":{"diagnostics":[raw],"only":["quickfix"]}}))?;
            let expected = format!("Ignore '{code}' for this line");
            let Some(action) = actions
                .as_array()
                .into_iter()
                .flatten()
                .find(|action| action["title"] == expected)
            else {
                continue;
            };
            let Some(changes) = action["edit"]["changes"].as_object() else {
                continue;
            };
            ensure!(
                changes.len() == 1 && changes.contains_key(&uri),
                "suppression edits another file"
            );
            let mut edits = parse_edits(&text, changes[&uri].as_array().context("ignore edits")?)?;
            if edits.len() == 1
                && edits[0].start == edits[0].end
                && let Some((_, codes)) = text[..edits[0].start].rsplit_once('[')
                && !codes.contains([']', '\n', '\r'])
                && codes
                    .split(',')
                    .any(|entry| entry.trim().strip_prefix("ty:").unwrap_or(entry.trim()) == code)
            {
                count += 1;
                continue;
            }
            edits.sort_by_key(|edit| std::cmp::Reverse(edit.start));
            let changed = snapshot.apply(&text, &edits)?;
            if changed == text {
                continue;
            }
            ensure!(
                snapshot.valid(&changed),
                "suppression creates invalid Python; no files were changed"
            );
            text = changed;
            previous.push(edits);
            count += 1;
            version += 1;
            if snapshot.notebook.is_some() {
                checker.notify("notebookDocument/didChange",json!({"notebookDocument":{"uri":notebook_uri,"version":version},"change":{"cells":{"textContent":[{"document":{"uri":uri,"version":version},"changes":[{"text":text}]}]}}}))?;
            } else {
                checker.notify("textDocument/didChange",json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]}))?;
            }
        }
        Ok((snapshot.serialize(text)?, count))
    };
    let result = run();
    let close = if snapshot.notebook.is_some() {
        checker.notify(
            "notebookDocument/didClose",
            json!({"notebookDocument":{"uri":notebook_uri},"cellTextDocuments":[{"uri":uri}]}),
        )
    } else {
        checker.notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))
    };
    close?;
    result
}

fn ignore_edits(
    documents: &[ProjectDocument],
    snapshots: &BTreeMap<PathBuf, Snapshot>,
    checker: &mut ProjectChecker,
    result: &Value,
) -> Result<(BTreeMap<PathBuf, String>, usize)> {
    let mut output = BTreeMap::new();
    let mut count = 0;
    for document in documents {
        let snapshot = &snapshots[Path::new(&document.source.path)];
        if !snapshot.valid(&document.source.text) {
            continue;
        }
        let mut diagnostics: Vec<_> = result["diagnostics"]
            .as_array()
            .context("CLI diagnostics")?
            .iter()
            .filter(|diagnostic| {
                diagnostic["location"]["path"].as_str() == Some(&document.source.path)
            })
            .collect();
        diagnostics.sort_by_key(|diagnostic| {
            (
                diagnostic["check_name"].as_str(),
                diagnostic["location"]["positions"]["begin"]["line"].as_u64(),
                diagnostic["location"]["positions"]["begin"]["column"].as_u64(),
                diagnostic["location"]["positions"]["end"]["line"].as_u64(),
                diagnostic["location"]["positions"]["end"]["column"].as_u64(),
            )
        });
        if diagnostics.is_empty() {
            continue;
        }
        let (text, changes) = ignore_file(checker, document, snapshot, &diagnostics)?;
        if changes > 0 && text != snapshot.raw {
            output.insert(document.source.path.clone().into(), text);
            count += changes;
        }
    }
    Ok((output, count))
}

fn write_transaction(
    snapshots: &BTreeMap<PathBuf, Snapshot>,
    changes: &BTreeMap<PathBuf, String>,
) -> Result<()> {
    let sources: BTreeMap<_, _> = snapshots
        .iter()
        .map(|(path, snapshot)| (path.clone(), snapshot.raw.as_str()))
        .collect();
    for path in changes.keys() {
        ensure!(
            std::fs::read_to_string(path)? == sources[path],
            "{} changed while computing fixes; no files were changed",
            path.display()
        );
    }
    // Stage both versions in the destination directory. A failed write cannot
    // truncate a user's file, and a later rename failure can restore preceding
    // files. Canonical destinations preserve a user's symbolic links.
    let mut temporary = TemporaryWrites::default();
    let mut staged = vec![];
    let mut destinations = BTreeMap::new();
    for (path, text) in changes {
        let destination = path.canonicalize()?;
        if let Some(previous) = destinations.insert(destination.clone(), text) {
            ensure!(
                previous == text,
                "two aliases produce conflicting fixes for {}",
                destination.display()
            );
            continue;
        }
        let permissions = std::fs::metadata(&destination)?.permissions();
        ensure!(
            !permissions.readonly(),
            "fix target is read-only: {}",
            path.display()
        );
        let new = temporary.stage(&destination, text, &permissions)?;
        let backup = temporary.stage(&destination, sources[path], &permissions)?;
        staged.push((destination, new, backup));
    }
    for path in changes.keys() {
        ensure!(
            std::fs::read_to_string(path)? == sources[path],
            "{} changed while staging fixes; no files were changed",
            path.display()
        );
    }
    for (index, (path, new, _)) in staged.iter().enumerate() {
        if let Err(error) = std::fs::rename(new, path) {
            let mut failures = vec![];
            for (previous, _, backup) in staged[..index].iter().rev() {
                if let Err(rollback) = std::fs::rename(backup, previous) {
                    temporary.0.retain(|path| path != backup);
                    failures.push(format!(
                        "{}: {rollback}; original content retained at {}",
                        previous.display(),
                        backup.display()
                    ));
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "writing {}; rollback failures: {failures:?}",
                    path.display()
                )
            });
        }
    }
    Ok(())
}

#[derive(Default)]
struct TemporaryWrites(Vec<PathBuf>);
impl TemporaryWrites {
    fn stage(
        &mut self,
        destination: &Path,
        text: &str,
        permissions: &std::fs::Permissions,
    ) -> Result<PathBuf> {
        use std::io::Write;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = destination.with_file_name(format!(
                ".ty-companion-edit-{}-{suffix}.tmp",
                std::process::id()
            ));
            let mut file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            };
            self.0.push(path.clone());
            file.write_all(text.as_bytes())?;
            file.set_permissions(permissions.clone())?;
            file.sync_all()?;
            return Ok(path);
        }
    }
}
impl Drop for TemporaryWrites {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

pub fn apply(session: &mut Session, mut result: Value, mode: Mode) -> Result<Outcome> {
    let mut count = 0;
    for _ in 0..10 {
        let documents = session.documents();
        let snapshots: BTreeMap<_, _> = documents
            .iter()
            .map(|document| {
                Ok((
                    PathBuf::from(&document.source.path),
                    Snapshot::capture(document)?,
                ))
            })
            .collect::<Result<_>>()?;
        let (changes, applied) = match mode {
            Mode::Fix => fix_edits(&documents, &snapshots, session, &result)?,
            Mode::AddIgnore => ignore_edits(
                &documents,
                &snapshots,
                session
                    .checker_mut()
                    .context("checker missing for suppressions")?,
                &result,
            )?,
        };
        if changes.is_empty() {
            break;
        }
        write_transaction(&snapshots, &changes)?;
        count += applied;
        session.invalidate();
        result = session.check(&BTreeMap::new())?;
    }
    Ok(Outcome { result, count })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_transactions_reject_overlaps_and_preserve_unicode_crlf() -> Result<()> {
        let text = "é = 1\r\n";
        let edits = parse_edits(
            text,
            &[
                json!({"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}},"newText":"2"}),
            ],
        )?;
        assert_eq!(edited(text, &edits)?, "é = 2\r\n");
        let mut conflicts = edits.clone();
        conflicts.push(Edit {
            start: 5,
            end: 6,
            text: "3".into(),
        });
        assert!(edited(text, &conflicts).is_err());
        assert_eq!(
            edited(text, &[edits[0].clone(), edits[0].clone()])?,
            "é = 2\r\n"
        );
        Ok(())
    }
    #[test]
    fn suppression_offsets_follow_prior_insertions() -> Result<()> {
        let edits = [
            Edit {
                start: 10,
                end: 10,
                text: " # ty: ignore[a]".into(),
            },
            Edit {
                start: 3,
                end: 3,
                text: "abc".into(),
            },
        ];
        assert_eq!(shifted(2, &edits)?, 2);
        assert_eq!(shifted(7, &edits)?, 10);
        assert_eq!(shifted(20, &edits)?, 39);
        Ok(())
    }

    #[test]
    fn source_transaction_rejects_stale_snapshots_before_writing() -> Result<()> {
        let root = std::env::temp_dir().join(format!("ty-cli-transaction-{}", std::process::id()));
        std::fs::create_dir_all(&root)?;
        let first = root.join("first.py");
        let second = root.join("second.py");
        std::fs::write(&first, "first = 1\n")?;
        std::fs::write(&second, "second = 1\n")?;
        let documents: Vec<_> = [(&first, "first = 1\n"), (&second, "second = 1\n")]
            .into_iter()
            .map(|(path, text)| {
                let source = std::sync::Arc::new(crate::model::Source::new(
                    text.into(),
                    path.to_string_lossy().into(),
                    "test".into(),
                ));
                Ok(ProjectDocument {
                    rendered: crate::mapping::render(&source, &crate::mapping::Edits::default())?,
                    source,
                    path: root.join("generated.py"),
                    names: BTreeMap::new(),
                })
            })
            .collect::<Result<_>>()?;
        let changes = BTreeMap::from([
            (first.clone(), "first = 2\n".into()),
            (second.clone(), "second = 2\n".into()),
        ]);
        let snapshots = documents
            .iter()
            .map(|document| {
                Ok((
                    PathBuf::from(&document.source.path),
                    Snapshot::capture(document)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        std::fs::write(&second, "user_edit = True\n")?;
        assert!(write_transaction(&snapshots, &changes).is_err());
        assert_eq!(std::fs::read_to_string(&first)?, "first = 1\n");
        assert_eq!(std::fs::read_to_string(&second)?, "user_edit = True\n");
        std::fs::write(&second, "second = 1\n")?;
        write_transaction(&snapshots, &changes)?;
        assert_eq!(std::fs::read_to_string(&first)?, "first = 2\n");
        assert_eq!(std::fs::read_dir(&root)?.count(), 2);
        std::fs::remove_dir_all(root)?;
        Ok(())
    }
}
