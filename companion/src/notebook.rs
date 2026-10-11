//! Native notebook transport around the combined plugin projection.
//!
//! The plugin observes the same concatenated Python source as ty. Final source
//! maps are split back into cells and ty retains responsibility for cross-cell
//! name resolution, navigation, and notebook-specific parsing.
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::{
    checker::{ProjectChecker, file_uri},
    lsp_mapping::{DocumentMap, PositionEncoding, offset_to_position, position_to_offset},
    model::Span,
    project_check::ProjectDocument,
};

#[derive(Clone)]
pub struct Cell {
    pub uri: String,
    pub kind: u64,
    pub text: String,
    pub version: i64,
    pub language: String,
    descriptor: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Result<Notebook> {
        Notebook::open(
            &json!({"notebookDocument":{"uri":"file:///src/demo.ipynb","notebookType":"jupyter-notebook","version":1,"cells":[{"kind":1,"document":"cell:markdown"},{"kind":2,"document":"cell:one"},{"kind":2,"document":"cell:magic"},{"kind":2,"document":"cell:two"}]},"cellTextDocuments":[{"uri":"cell:one","languageId":"python","version":1,"text":"emoji = '😀'\r\nvalue = 1"},{"uri":"cell:magic","languageId":"python","version":1,"text":"%%bash\necho hi"},{"uri":"cell:two","languageId":"python","version":1,"text":"print(value)\n"}]}),
            Path::new("/src/demo.py"),
        )
    }

    #[test]
    fn native_cell_filtering_and_unicode_ranges() -> Result<()> {
        let book = fixture()?;
        assert_eq!(
            book.combined(),
            "emoji = '😀'\r\nvalue = 1\nprint(value)\n\n"
        );
        assert_eq!(book.included, vec![1, 3]);
        let (uri, range) = book
            .combined_range(
                &json!({"start":{"line":2,"character":6},"end":{"line":2,"character":11}}),
                PositionEncoding::Utf16,
            )?
            .unwrap();
        assert_eq!(uri, "cell:two");
        assert_eq!(
            range,
            json!({"start":{"line":0,"character":6},"end":{"line":0,"character":11}})
        );
        Ok(())
    }

    #[test]
    fn notebook_changes_are_atomic_and_support_cell_reordering() -> Result<()> {
        let mut book = fixture()?;
        let original = book.combined();
        let bad = json!({"notebookDocument":{"uri":book.uri(),"version":2},"change":{"cells":{"textContent":[{"document":{"uri":"cell:one","version":2},"changes":[{"text":"different"}]},{"document":{"uri":"cell:two","version":2},"changes":[{"range":{"start":{"line":99,"character":0},"end":{"line":99,"character":0}},"text":"bad"}]}]}}});
        assert!(book.apply_change(&bad, PositionEncoding::Utf16).is_err());
        assert_eq!(book.combined(), original);
        assert_eq!(book.version(), 1);
        book.apply_change(&json!({"notebookDocument":{"uri":book.uri(),"version":2},"change":{"cells":{"structure":{"array":{"start":0,"deleteCount":4,"cells":[{"kind":2,"document":"cell:two"},{"kind":2,"document":"cell:one"}]}},"textContent":[{"document":{"uri":"cell:one","version":2},"changes":[{"range":{"start":{"line":0,"character":9},"end":{"line":0,"character":11}},"text":"雪"}]}]}}}),PositionEncoding::Utf16)?;
        assert_eq!(
            book.combined(),
            "print(value)\n\nemoji = '雪'\r\nvalue = 1\n"
        );
        Ok(())
    }

    #[test]
    fn empty_notebook_matches_native_source() -> Result<()> {
        let book = Notebook::open(
            &json!({"notebookDocument":{"uri":"untitled:notebook","notebookType":"jupyter","version":0,"cells":[]},"cellTextDocuments":[]}),
            Path::new("/src/empty.py"),
        )?;
        assert_eq!(book.combined(), "\n");
        assert!(book.spans().is_empty());
        Ok(())
    }

    #[test]
    fn disk_display_coordinates_keep_raw_cell_indices_and_related_locations() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "ty-notebook-locations-{}.ipynb",
            std::process::id()
        ));
        let code = |text| json!({"cell_type":"code","metadata":{},"source":[text],"outputs":[],"execution_count":null});
        std::fs::write(
            &path,
            serde_json::to_vec(
                &json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"markdown","metadata":{},"source":["heading"]},code("first = 1\r\n"),code("%%bash\necho skipped\n"),code("second = missing\n")]}),
            )?,
        )?;
        let mut diagnostics = vec![
            json!({"location":{"path":path,"positions":{"begin":{"line":3,"column":10},"end":{"line":3,"column":17}}}}),
            json!({"location":{"path":"/src/demo.py","positions":{"begin":{"line":1,"column":1},"end":{"line":1,"column":2}}},"related":[{"message":"declared here","location":{"uri":file_uri(&path)?,"range":{"start":{"line":2,"character":9},"end":{"line":2,"character":16}}}}]}),
        ];
        cli_locations(&mut diagnostics)?;
        assert_eq!(diagnostics[0]["location"]["cell"], 4);
        assert_eq!(
            diagnostics[0]["location"]["cell_positions"]["begin"],
            json!({"line":1,"column":10})
        );
        assert_eq!(diagnostics[0]["location"]["positions"]["begin"]["line"], 3);
        assert_eq!(
            diagnostics[0]["location"]["source_line"],
            "second = missing"
        );
        assert_eq!(diagnostics[1]["related"][0]["location"]["cell"], 4);
        assert_eq!(
            diagnostics[1]["related"][0]["location"]["cell_positions"]["begin"],
            json!({"line":1,"column":10})
        );
        std::fs::remove_file(path)?;
        Ok(())
    }
}

#[derive(Clone)]
struct Backend {
    process: u32,
    uri: String,
    version: i64,
    cells: Vec<Value>,
    texts: BTreeMap<String, String>,
    metadata: Value,
}

#[derive(Clone)]
pub struct Notebook {
    uri: String,
    path: PathBuf,
    version: i64,
    notebook_type: String,
    metadata: Value,
    cells: Vec<Cell>,
    combined: String,
    included: Vec<usize>,
    spans: Vec<Span>,
    backend: Option<Backend>,
    generated_cells: Vec<(String, String)>,
}

fn cell(descriptor: &Value, texts: &BTreeMap<String, Value>) -> Result<Cell> {
    let uri = descriptor["document"]
        .as_str()
        .context("notebook cell has no document URI")?
        .to_owned();
    let kind = descriptor["kind"]
        .as_u64()
        .context("notebook cell has no kind")?;
    ensure!(
        matches!(kind, 1 | 2),
        "unsupported notebook cell kind {kind}"
    );
    let document = texts.get(&uri);
    Ok(Cell {
        uri,
        kind,
        text: document
            .and_then(|v| v["text"].as_str())
            .unwrap_or("")
            .into(),
        version: document.and_then(|v| v["version"].as_i64()).unwrap_or(0),
        language: document
            .and_then(|v| v["languageId"].as_str())
            .unwrap_or(if kind == 2 { "python" } else { "markdown" })
            .into(),
        descriptor: descriptor.clone(),
    })
}

fn text_documents(value: &Value) -> Result<BTreeMap<String, Value>> {
    let mut output = BTreeMap::new();
    for document in value.as_array().into_iter().flatten() {
        let uri = document["uri"]
            .as_str()
            .context("cell text document has no URI")?;
        ensure!(
            document["text"].is_string() && document["version"].is_i64(),
            "invalid cell text document"
        );
        ensure!(
            output.insert(uri.into(), document.clone()).is_none(),
            "duplicate cell text document"
        );
    }
    Ok(output)
}

/// Check an on-disk notebook with real cell boundaries, then rebase native
/// diagnostics and safe-fix ranges to the combined generated source used by
/// the transport-neutral project mapper.
pub fn check_disk(document: &ProjectDocument, checker: &mut ProjectChecker) -> Result<Vec<Value>> {
    let path = Path::new(&document.source.path);
    if !ruff_notebook::Notebook::from_path(path)?.is_python_notebook() {
        return Ok(vec![]);
    }
    let raw: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    let uri = file_uri(path)?;
    let mut cells = vec![];
    let mut texts = vec![];
    for (index, cell) in raw["cells"]
        .as_array()
        .context("notebook cells")?
        .iter()
        .enumerate()
    {
        let cell_uri = format!("{uri}#cell-{index}");
        let code = cell["cell_type"] == "code";
        let text = match &cell["source"] {
            Value::String(text) => text.clone(),
            Value::Array(lines) => lines.iter().filter_map(Value::as_str).collect(),
            _ => String::new(),
        };
        cells.push(
            json!({"kind":if code {2} else {1},"document":cell_uri,"metadata":cell["metadata"]}),
        );
        texts.push(json!({"uri":cell_uri,"languageId":if code {"python"} else {"markdown"},"version":1,"text":text}));
    }
    let mut notebook = Notebook::open(
        &json!({"notebookDocument":{"uri":uri,"notebookType":"jupyter-notebook","version":1,"metadata":raw["metadata"],"cells":cells},"cellTextDocuments":texts}),
        path,
    )?;
    notebook.project(document, checker)?;
    let result = (|| -> Result<Vec<Value>> {
        let backend = notebook.backend.as_ref().context("disk notebook backend")?;
        let mut cell_sources = BTreeMap::new();
        let mut offset = 0;
        for (_, generated) in notebook.generated_cells() {
            let text = &backend.texts[generated];
            cell_sources.insert(generated.clone(), (offset, text.as_str()));
            offset += text.len() + 1;
        }
        let aggregate_uri = file_uri(&document.path)?;
        let shift = |uri: &str, range: &Value| -> Result<Value> {
            let Some((offset, text)) = cell_sources.get(uri) else {
                return Ok(range.clone());
            };
            let point = |value: &Value| -> Result<Value> {
                let local = position_to_offset(text, value, PositionEncoding::Utf32)?;
                offset_to_position(
                    &document.rendered.text,
                    offset + local,
                    PositionEncoding::Utf32,
                )
            };
            Ok(json!({"start":point(&range["start"])? ,"end":point(&range["end"])?}))
        };
        let mut result = vec![];
        for (_, generated) in notebook.generated_cells() {
            let report = checker.request(
                "textDocument/diagnostic",
                json!({"textDocument":{"uri":generated}}),
            )?;
            for item in report["items"]
                .as_array()
                .context("disk notebook diagnostic report")?
            {
                let mut item = item.clone();
                item["range"] = shift(generated, &item["range"])?;
                for related in item["relatedInformation"]
                    .as_array_mut()
                    .into_iter()
                    .flatten()
                {
                    let location = &mut related["location"];
                    if let Some(uri) = location["uri"]
                        .as_str()
                        .filter(|uri| cell_sources.contains_key(*uri))
                    {
                        location["range"] = shift(uri, &location["range"])?;
                        location["uri"] = json!(aggregate_uri);
                    }
                }
                let data = &mut item["data"];
                let fix = if data.get("fix").is_some() {
                    &mut data["fix"]
                } else {
                    data
                };
                if let Some(changes) = fix.get_mut("edits").and_then(Value::as_object_mut) {
                    let mut rewritten = serde_json::Map::<String, Value>::new();
                    for (uri, edits) in std::mem::take(changes) {
                        let target = if cell_sources.contains_key(&uri) {
                            aggregate_uri.clone()
                        } else {
                            uri.clone()
                        };
                        let destination = rewritten
                            .entry(target)
                            .or_insert_with(|| json!([]))
                            .as_array_mut()
                            .unwrap();
                        for mut edit in edits.as_array().context("notebook safe edit list")?.clone()
                        {
                            edit["range"] = shift(&uri, &edit["range"])?;
                            destination.push(edit);
                        }
                    }
                    *changes = rewritten;
                }
                if let Some(diagnostic) = crate::checker::normalize_diagnostic(
                    &document.path,
                    &document.rendered.text,
                    &item,
                )? {
                    result.push(diagnostic);
                }
            }
        }
        Ok(result)
    })();
    let close = notebook.close(checker);
    close?;
    result
}

/// Public presentation coordinates are separate from aggregate positions,
/// which CLI edits still need until all source mutations are complete.
pub fn cli_locations(diagnostics: &mut [Value]) -> Result<()> {
    let mut notebooks = BTreeMap::<PathBuf, ruff_notebook::Notebook>::new();
    let mut annotate = |location: &mut Value, path: PathBuf, mut positions: Value| -> Result<()> {
        if path
            .extension()
            .is_none_or(|extension| extension != "ipynb")
        {
            return Ok(());
        }
        let notebook = match notebooks.entry(path.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(ruff_notebook::Notebook::from_path(&path)?)
            }
        };
        let line = positions["begin"]["line"].as_u64().unwrap_or(1);
        if let Some(cell) = notebook
            .index()
            .iter()
            .take_while(|cell| cell.start_row().get() as u64 <= line)
            .last()
        {
            location["cell"] = json!(cell.cell_index().get());
            for key in ["begin", "end"] {
                let row = positions[key]["line"].as_u64().unwrap_or(1);
                positions[key]["line"] =
                    json!(row.saturating_sub(cell.start_row().get() as u64) + 1);
            }
            location["cell_positions"] = positions;
            if let Some(text) = notebook
                .source_code()
                .lines()
                .nth(line.saturating_sub(1) as usize)
            {
                location["source_line"] = json!(text);
            }
        }
        Ok(())
    };
    for diagnostic in diagnostics {
        let location = &mut diagnostic["location"];
        if let Some(path) = location["path"].as_str().map(PathBuf::from) {
            annotate(location, path, location["positions"].clone())?;
        }
        for related in diagnostic["related"].as_array_mut().into_iter().flatten() {
            let location = &mut related["location"];
            if let Some(path) = location["uri"]
                .as_str()
                .map(crate::lsp_mapping::decode_file_uri)
                .transpose()?
                .flatten()
            {
                let point = |value: &Value| json!({"line":value["line"].as_u64().unwrap_or(0)+1,"column":value["character"].as_u64().unwrap_or(0)+1});
                let positions = json!({"begin":point(&location["range"]["start"]),"end":point(&location["range"]["end"])});
                annotate(location, path, positions)?;
            }
        }
    }
    Ok(())
}

/// Apply a text-document change transaction without leaving a partial edit on
/// invalid input. LSP ranges use the encoding negotiated with this client.
pub fn apply_changes(
    text: &mut String,
    changes: &[Value],
    encoding: PositionEncoding,
) -> Result<()> {
    let mut updated = text.clone();
    for change in changes {
        let replacement = change["text"].as_str().context("change has no text")?;
        if let Some(range) = change.get("range") {
            let start = position_to_offset(&updated, &range["start"], encoding)?;
            let end = position_to_offset(&updated, &range["end"], encoding)?;
            ensure!(start <= end, "reversed edit range");
            updated.replace_range(start..end, replacement);
        } else {
            updated = replacement.into();
        }
    }
    *text = updated;
    Ok(())
}

impl Notebook {
    pub fn open(params: &Value, path: &Path) -> Result<Self> {
        let document = &params["notebookDocument"];
        let texts = text_documents(&params["cellTextDocuments"])?;
        let mut notebook = Self {
            uri: document["uri"].as_str().context("notebook URI")?.into(),
            path: path.into(),
            version: document["version"].as_i64().context("notebook version")?,
            notebook_type: document["notebookType"]
                .as_str()
                .context("notebook type")?
                .into(),
            metadata: document
                .get("metadata")
                .cloned()
                .unwrap_or_else(|| json!({})),
            cells: document["cells"]
                .as_array()
                .context("notebook cells")?
                .iter()
                .map(|value| cell(value, &texts))
                .collect::<Result<_>>()?,
            combined: String::new(),
            included: vec![],
            spans: vec![],
            backend: None,
            generated_cells: vec![],
        };
        notebook.refresh()?;
        Ok(notebook)
    }
    pub fn uri(&self) -> &str {
        &self.uri
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn version(&self) -> i64 {
        self.version
    }
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }
    pub fn contains_cell(&self, uri: &str) -> bool {
        self.cells.iter().any(|cell| cell.uri == uri)
    }
    pub fn is_code_cell(&self, uri: &str) -> bool {
        self.included
            .iter()
            .any(|index| self.cells[*index].uri == uri)
    }
    pub fn combined(&self) -> String {
        self.combined.clone()
    }
    pub fn spans(&self) -> &[Span] {
        &self.spans
    }
    pub fn generated_cells(&self) -> &[(String, String)] {
        &self.generated_cells
    }

    fn refresh(&mut self) -> Result<()> {
        let mut seen = std::collections::BTreeSet::new();
        ensure!(
            self.cells.iter().all(|cell| seen.insert(&cell.uri)),
            "duplicate notebook cell URI"
        );
        // Match ty_server::NotebookDocument::to_ruff_notebook: per-cell metadata
        // is empty there, including vscode.languageId; cell kind determines code.
        let cells:Vec<Value>=self.cells.iter().map(|cell|if cell.kind==2 {
            json!({"cell_type":"code","metadata":{},"source":cell.text,"outputs":[],"execution_count":null})
        } else {json!({"cell_type":"markdown","metadata":{},"source":cell.text})}).collect();
        let raw = json!({"nbformat":4,"nbformat_minor":5,"metadata":self.metadata,"cells":cells});
        let native = ruff_notebook::Notebook::from_source_code(&serde_json::to_string(&raw)?)?;
        self.combined = native.source_code().into();
        self.included = native
            .index()
            .iter()
            .map(|start| start.cell_index().to_zero_indexed())
            .collect();
        let mut offset = 0;
        self.spans = self
            .included
            .iter()
            .map(|index| {
                let span = Span {
                    start: offset,
                    end: offset + self.cells[*index].text.len(),
                };
                offset = span.end + 1;
                span
            })
            .collect();
        ensure!(
            self.included.is_empty() || offset == self.combined.len(),
            "native notebook source spans differ"
        );
        Ok(())
    }

    pub fn apply_change(&mut self, params: &Value, encoding: PositionEncoding) -> Result<()> {
        ensure!(
            params["notebookDocument"]["uri"] == self.uri,
            "notebook change URI differs"
        );
        let version = params["notebookDocument"]["version"]
            .as_i64()
            .context("notebook change version")?;
        ensure!(version > self.version, "notebook version did not advance");
        let mut updated = self.clone();
        updated.version = version;
        let change = &params["change"];
        if let Some(metadata) = change.get("metadata") {
            updated.metadata = metadata.clone();
        }
        if let Some(structure) = change.pointer("/cells/structure") {
            let array = &structure["array"];
            let start = usize::try_from(array["start"].as_u64().context("cell splice start")?)?;
            let count =
                usize::try_from(array["deleteCount"].as_u64().context("cell splice count")?)?;
            ensure!(
                start <= updated.cells.len() && count <= updated.cells.len() - start,
                "cell splice exceeds notebook"
            );
            let mut texts:BTreeMap<_,_>=updated.cells.iter().map(|cell|(cell.uri.clone(),json!({"uri":cell.uri,"text":cell.text,"version":cell.version,"languageId":cell.language}))).collect();
            texts.extend(text_documents(&structure["didOpen"])?);
            let added = array["cells"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|value| cell(value, &texts))
                .collect::<Result<Vec<_>>>()?;
            updated.cells.splice(start..start + count, added);
        }
        for descriptor in change
            .pointer("/cells/data")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let uri = descriptor["document"]
                .as_str()
                .context("updated cell URI")?;
            let target = updated
                .cells
                .iter_mut()
                .find(|cell| cell.uri == uri)
                .context("updated cell is not in notebook")?;
            let kind = descriptor["kind"].as_u64().context("updated cell kind")?;
            ensure!(matches!(kind, 1 | 2), "unsupported notebook cell kind");
            target.kind = kind;
            target.descriptor = descriptor.clone();
        }
        for content in change
            .pointer("/cells/textContent")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let uri = content["document"]["uri"]
                .as_str()
                .context("changed cell URI")?;
            let target = updated
                .cells
                .iter_mut()
                .find(|cell| cell.uri == uri)
                .context("changed cell is not in notebook")?;
            let version = content["document"]["version"]
                .as_i64()
                .context("changed cell version")?;
            ensure!(version > target.version, "cell version did not advance");
            apply_changes(
                &mut target.text,
                content["changes"].as_array().context("cell text changes")?,
                encoding,
            )?;
            target.version = version;
        }
        updated.refresh()?;
        *self = updated;
        Ok(())
    }

    #[cfg(test)]
    pub fn combined_range(
        &self,
        range: &Value,
        encoding: PositionEncoding,
    ) -> Result<Option<(String, Value)>> {
        let start = position_to_offset(&self.combined, &range["start"], encoding)?;
        let end = position_to_offset(&self.combined, &range["end"], encoding)?;
        ensure!(start <= end, "reversed notebook source range");
        for (&index, span) in self.included.iter().zip(&self.spans) {
            if span.start <= start && start <= span.end {
                let cell = &self.cells[index];
                // Notebook diagnostics spanning a separator are displayed to the
                // end of the originating cell, as native ty locations are.
                let range = json!({"start":offset_to_position(&cell.text,start-span.start,encoding)?,"end":offset_to_position(&cell.text,end.min(span.end)-span.start,encoding)?});
                return Ok(Some((cell.uri.clone(), range)));
            }
        }
        Ok(None)
    }

    pub fn project(
        &mut self,
        document: &ProjectDocument,
        checker: &mut ProjectChecker,
    ) -> Result<Vec<DocumentMap>> {
        ensure!(
            document.source.text == self.combined,
            "notebook projection belongs to outdated source"
        );
        let pieces = document.rendered.split_cells(&self.combined, &self.spans)?;
        let mut pieces: BTreeMap<_, _> = self.included.iter().copied().zip(pieces).collect();
        let uri = file_uri(&document.path.with_extension("editor.ipynb"))?;
        let mut maps = Vec::with_capacity(self.cells.len());
        let mut cells = Vec::with_capacity(self.cells.len());
        let mut texts = BTreeMap::new();
        let mut generated_cells = vec![];
        for &index in &self.included {
            let cell = &self.cells[index];
            // ty 0.0.86 indexes notebook positions by raw cell index while its
            // source offsets include only valid Python cells. Keeping the
            // backend notebook Python-only avoids that upstream mismatch.
            let digest = format!("{:x}", Sha256::digest(cell.uri.as_bytes()));
            let path = document
                .path
                .with_extension(format!("cell-{}.py", &digest[..24]));
            let generated = file_uri(&path)?;
            let rendered = pieces
                .remove(&index)
                .context("missing projected notebook cell")?;
            let mut descriptor = cell.descriptor.clone();
            descriptor["document"] = json!(generated);
            cells.push(descriptor);
            texts.insert(generated.clone(), rendered.text.clone());
            maps.push(
                DocumentMap::new(
                    cell.uri.clone(),
                    generated.clone(),
                    cell.text.clone(),
                    rendered,
                )
                .with_names(document.names.clone())
                .with_namespace(&document.source.text)
                .with_module(document.source.module.clone()),
            );
            if cell.kind == 2 {
                generated_cells.push((cell.uri.clone(), generated));
            }
        }
        let process = checker.process_id();
        let previous = self
            .backend
            .as_ref()
            .filter(|backend| backend.process == process);
        let version = previous.map_or(1, |backend| backend.version + 1);
        let same_structure =
            previous.is_some_and(|backend| backend.uri == uri && backend.cells == cells);
        if same_structure {
            let previous = previous.unwrap();
            let changed:Vec<_>=texts.iter().filter(|(uri,text)|previous.texts.get(*uri)!=Some(text)).map(|(uri,text)|json!({"document":{"uri":uri,"version":version},"changes":[{"text":text}]})).collect();
            if !changed.is_empty() || previous.metadata != self.metadata {
                checker.notify("notebookDocument/didChange",json!({"notebookDocument":{"uri":uri,"version":version},"change":{"metadata":self.metadata,"cells":{"textContent":changed}}}))?;
            }
        } else {
            self.close(checker)?;
            let documents:Vec<_>=texts.iter().map(|(uri,text)|json!({"uri":uri,"version":version,"languageId":"python","text":text})).collect();
            checker.notify("notebookDocument/didOpen",json!({"notebookDocument":{"uri":uri,"notebookType":self.notebook_type,"version":version,"metadata":self.metadata,"cells":cells},"cellTextDocuments":documents}))?;
        }
        self.backend = Some(Backend {
            process,
            uri,
            version,
            cells,
            texts,
            metadata: self.metadata.clone(),
        });
        self.generated_cells = generated_cells;
        Ok(maps)
    }

    pub fn close(&mut self, checker: &mut ProjectChecker) -> Result<()> {
        if let Some(backend) = &self.backend
            && backend.process == checker.process_id()
        {
            let documents: Vec<_> = backend.texts.keys().map(|uri| json!({"uri":uri})).collect();
            checker.notify(
                "notebookDocument/didClose",
                json!({"notebookDocument":{"uri":backend.uri},"cellTextDocuments":documents}),
            )?;
        }
        self.backend = None;
        self.generated_cells.clear();
        Ok(())
    }

    /// Query declared signatures in a temporary native notebook, retaining all
    /// preceding cells. A plain auxiliary Python file would lose their imports
    /// and names. URIs differ from the live notebook and are always closed.
    pub fn signature_help(
        &self,
        checker: &mut ProjectChecker,
        target: &str,
        text: &str,
        mut params: Value,
    ) -> Result<Value> {
        let backend = self
            .backend
            .as_ref()
            .filter(|backend| backend.process == checker.process_id())
            .context("notebook projection is not open")?;
        ensure!(
            backend.texts.contains_key(target),
            "signature target is not a notebook cell"
        );
        let uri = format!("{}.signature.ipynb", backend.uri);
        let rename = |uri: &str| format!("{uri}.signature.py");
        let cells: Vec<_> = backend
            .cells
            .iter()
            .map(|cell| {
                let mut cell = cell.clone();
                cell["document"] = json!(rename(cell["document"].as_str().unwrap()));
                cell
            })
            .collect();
        let documents:Vec<_>=backend.texts.iter().map(|(uri,original)|json!({"uri":rename(uri),"languageId":"python","version":1,"text":if uri==target {text} else {original}})).collect();
        checker.notify("notebookDocument/didOpen",json!({"notebookDocument":{"uri":uri,"notebookType":self.notebook_type,"version":1,"metadata":self.metadata,"cells":cells},"cellTextDocuments":documents}))?;
        params["textDocument"]["uri"] = json!(rename(target));
        let result = checker.request("textDocument/signatureHelp", params);
        let close=checker.notify("notebookDocument/didClose",json!({"notebookDocument":{"uri":uri},"cellTextDocuments":backend.texts.keys().map(|uri|json!({"uri":rename(uri)})).collect::<Vec<_>>()}));
        close?;
        result
    }

    /// ty 0.0.86 emits every notebook symbol with cell-local ranges and no URI.
    /// Query a native one-cell view to obtain an unambiguous response for the
    /// requested cell. Symbol extraction is syntactic and needs no prior cells.
    pub fn document_symbols(
        &self,
        checker: &mut ProjectChecker,
        target: &str,
        mut params: Value,
    ) -> Result<Value> {
        let backend = self
            .backend
            .as_ref()
            .filter(|backend| backend.process == checker.process_id())
            .context("notebook projection is not open")?;
        let text = backend
            .texts
            .get(target)
            .context("symbol target is not a notebook cell")?;
        let uri = format!("{}.symbols.ipynb", backend.uri);
        let cell = format!("{target}.symbols.py");
        checker.notify("notebookDocument/didOpen",json!({"notebookDocument":{"uri":uri,"notebookType":self.notebook_type,"version":1,"metadata":self.metadata,"cells":[{"kind":2,"document":cell}]},"cellTextDocuments":[{"uri":cell,"languageId":"python","version":1,"text":text}]}))?;
        params["textDocument"]["uri"] = json!(cell);
        let result = checker.request("textDocument/documentSymbol", params);
        let close = checker.notify(
            "notebookDocument/didClose",
            json!({"notebookDocument":{"uri":uri},"cellTextDocuments":[{"uri":cell}]}),
        );
        close?;
        result
    }

    /// Notebook highlights have no URI; upstream currently includes other
    /// cells' highlights in the result. A one-cell aggregate view retains the
    /// shared semantic context while making every returned range unambiguous.
    pub fn document_highlights(
        &self,
        checker: &mut ProjectChecker,
        target: &str,
        mut params: Value,
    ) -> Result<Value> {
        let backend = self
            .backend
            .as_ref()
            .filter(|backend| backend.process == checker.process_id())
            .context("notebook projection is not open")?;
        let mut text = String::new();
        let mut target_span = None;
        for cell in &backend.cells {
            let uri = cell["document"].as_str().context("backend cell URI")?;
            let content = &backend.texts[uri];
            let start = text.len();
            text.push_str(content);
            if uri == target {
                target_span = Some(Span {
                    start,
                    end: text.len(),
                });
            }
            text.push('\n');
        }
        let span = target_span.context("highlight target is not a notebook cell")?;
        let encoding = PositionEncoding::Utf32;
        let local = position_to_offset(&backend.texts[target], &params["position"], encoding)?;
        let uri = format!("{}.highlights.ipynb", backend.uri);
        let cell = format!("{target}.highlights.py");
        checker.notify("notebookDocument/didOpen",json!({"notebookDocument":{"uri":uri,"notebookType":self.notebook_type,"version":1,"metadata":self.metadata,"cells":[{"kind":2,"document":cell}]},"cellTextDocuments":[{"uri":cell,"languageId":"python","version":1,"text":text}]}))?;
        params["textDocument"]["uri"] = json!(cell);
        params["position"] = offset_to_position(&text, span.start + local, encoding)?;
        let result = checker.request("textDocument/documentHighlight", params);
        let close = checker.notify(
            "notebookDocument/didClose",
            json!({"notebookDocument":{"uri":uri},"cellTextDocuments":[{"uri":cell}]}),
        );
        close?;
        let result = result?;
        if result.is_null() {
            return Ok(result);
        }
        let mut output = vec![];
        for value in result.as_array().context("notebook highlights response")? {
            let start = position_to_offset(&text, &value["range"]["start"], encoding)?;
            let end = position_to_offset(&text, &value["range"]["end"], encoding)?;
            if span.start <= start && end <= span.end {
                let mut value = value.clone();
                value["range"] = json!({"start":offset_to_position(&backend.texts[target],start-span.start,encoding)?,"end":offset_to_position(&backend.texts[target],end-span.start,encoding)?});
                output.push(value);
            }
        }
        Ok(json!(output))
    }
}
