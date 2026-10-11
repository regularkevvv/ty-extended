//! Editor-facing stdio LSP. All analysis and source projection remains in Rust.
//! One project session owns Monty and the official checker for each workspace.
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io::{self, BufReader, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

use crate::{
    checker::{ProtocolError, file_uri, read_frame},
    lsp_mapping::{
        DocumentMap, LspSourceMaps, PositionEncoding, offset_to_position, position_to_offset,
    },
    notebook::Notebook,
    project_check::Session,
};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn scratch() -> Result<Scratch> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    for attempt in 0..100 {
        let path = std::env::temp_dir().join(format!(
            "ty-companion-editor-{}-{seed}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(Scratch(path.canonicalize()?)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("could not allocate editor analysis directory")
}

pub(crate) fn default_checker(options: &mut BTreeMap<String, String>) -> Result<()> {
    let requested = options
        .get("ty")
        .cloned()
        .or_else(|| std::env::var("TY_COMPANION_TY").ok())
        .unwrap_or_else(|| "ty".into());
    let requested_path = Path::new(&requested);
    let resolved = if requested_path.components().count() > 1 || requested_path.is_absolute() {
        requested_path.canonicalize()?
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .flat_map(|directory| {
                let names = if cfg!(windows) {
                    vec![requested.clone(), format!("{requested}.exe")]
                } else {
                    vec![requested.clone()]
                };
                names.into_iter().map(move |name| directory.join(name))
            })
            .find(|candidate| candidate.is_file())
            .context("official ty not found; install ty separately or set TY_COMPANION_TY")?
            .canonicalize()?
    };
    ensure!(
        resolved != std::env::current_exe()?.canonicalize()?,
        "--ty resolves to the companion itself; supply the official ty executable"
    );
    options.insert("ty".into(), resolved.display().to_string());
    Ok(())
}

struct Buffer {
    text: String,
    version: i64,
}
struct Workspace {
    root: PathBuf,
    session: Session,
    dirty: bool,
    revision: u64,
    diagnostics: BTreeMap<String, Vec<Value>>,
    raw_diagnostics: Vec<(String, Value, Value)>,
    error: Option<String>,
    statistics: Value,
}

struct Editor {
    options: BTreeMap<String, String>,
    workspaces: Vec<Workspace>,
    buffers: BTreeMap<String, Buffer>,
    // Preserve editor URIs while resolving symlinks and non-file overlays to
    // canonical analysis paths. Synthetic modules never modify the project.
    aliases: BTreeMap<String, PathBuf>,
    next_document: usize,
    notebooks: BTreeMap<String, Notebook>,
    maps: LspSourceMaps,
    encoding: PositionEncoding,
    capabilities: Value,
    initialized: bool,
    client_ready: bool,
    shutdown: bool,
    pull: bool,
    next_workspace: usize,
    next_client_request: u64,
    watched_patterns: BTreeSet<String>,
    watcher_registration: Option<String>,
    cancelled: Arc<Mutex<BTreeSet<String>>>,
    timings: Vec<Value>,
    // Hints depend on semantic reachability, not only this file's text. Entries
    // survive only a globally unchanged/trivia-only refresh, never type edits.
    hint_cache: BTreeMap<(u32, String), Vec<Value>>,
    publications_pending: bool,
}

pub fn uri_path(uri: &str) -> Result<PathBuf> {
    let path = crate::lsp_mapping::decode_file_uri(uri)?
        .context("only file URIs are project documents")?;
    // Canonicalize existing parents too: editor-only files have no inode yet,
    // and Windows canonical roots include a verbatim drive prefix. This also
    // keeps a symlinked workspace and its editor documents in the same session.
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            component => normalized.push(component.as_os_str()),
        }
    }
    for parent in normalized.ancestors() {
        if let Ok(canonical) = parent.canonicalize() {
            let suffix = normalized.strip_prefix(parent)?;
            // Joining an empty path adds a separator, turning an existing file
            // into `file.py/` in diagnostic notes and generated file URIs.
            return Ok(if suffix.as_os_str().is_empty() {
                canonical
            } else {
                canonical.join(suffix)
            });
        }
    }
    Ok(normalized)
}

fn watcher_glob(path: &Path) -> String {
    let path = path.to_string_lossy();
    let path = if cfg!(windows) {
        path.strip_prefix(r"\\?\UNC\")
            .map(|path| format!("//{path}"))
            .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned())
            .replace('\\', "/")
    } else {
        path.into_owned()
    };
    path.chars()
        .map(|ch| match ch {
            '[' => "[[]".to_owned(),
            ']' => "[]]".to_owned(),
            '*' | '?' | '{' | '}' => format!("[{ch}]"),
            _ => ch.to_string(),
        })
        .collect()
}

fn send(output: &mut impl Write, message: &Value) -> Result<()> {
    let data = serde_json::to_vec(message)?;
    write!(output, "Content-Length: {}\r\n\r\n", data.len())?;
    output.write_all(&data)?;
    output.flush()?;
    Ok(())
}
fn response(output: &mut impl Write, id: &Value, result: Result<Value>) -> Result<()> {
    let message = match result {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(error) => {
            let error = if let Some(error) = error.downcast_ref::<ProtocolError>() {
                json!({"code":error.code,"message":error.message,"data":error.data})
            } else {
                json!({"code":-32603,"message":format!("{error:#}")})
            };
            json!({"jsonrpc":"2.0","id":id,"error":error})
        }
    };
    send(output, &message)
}
fn protocol(code: i64, message: &str) -> anyhow::Error {
    ProtocolError {
        code,
        message: message.into(),
        data: None,
    }
    .into()
}

fn apply_changes(text: &mut String, changes: &[Value], encoding: PositionEncoding) -> Result<()> {
    // Apply a notification atomically. A malformed later change cannot leave
    // the buffer half updated while its document version remains unchanged.
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

impl Editor {
    fn new(options: BTreeMap<String, String>, cancelled: Arc<Mutex<BTreeSet<String>>>) -> Self {
        let encoding = PositionEncoding::from_name("utf-16").unwrap();
        Self {
            options,
            workspaces: vec![],
            buffers: BTreeMap::new(),
            aliases: BTreeMap::new(),
            next_document: 0,
            notebooks: BTreeMap::new(),
            maps: LspSourceMaps::new(encoding, PositionEncoding::from_name("utf-32").unwrap()),
            encoding,
            capabilities: json!({}),
            initialized: false,
            client_ready: false,
            shutdown: false,
            pull: false,
            next_workspace: 0,
            next_client_request: 0,
            watched_patterns: BTreeSet::new(),
            watcher_registration: None,
            cancelled,
            timings: vec![],
            hint_cache: BTreeMap::new(),
            publications_pending: false,
        }
    }

    fn add_workspace(&mut self, root: PathBuf) -> Result<()> {
        let root = root.canonicalize().context("workspace root")?;
        if self.workspaces.iter().any(|w| w.root == root) {
            return Ok(());
        }
        let mut options = self.options.clone();
        let out = PathBuf::from(
            options
                .get("out")
                .context("server needs --out DIRECTORY outside the project")?,
        )
        .join(format!("workspace-{}", self.next_workspace));
        self.next_workspace += 1;
        std::fs::create_dir_all(&out)?;
        self.maps
            .add_generated_root(file_uri(&out.canonicalize()?)?);
        options.insert("out".into(), out.display().to_string());
        options.insert("checker-mode".into(), "push".into());
        options.insert("lsp-capabilities".into(), self.capabilities.to_string());
        let session = Session::new(&root, &options)?;
        self.workspaces.push(Workspace {
            root,
            session,
            dirty: true,
            revision: 0,
            diagnostics: BTreeMap::new(),
            raw_diagnostics: vec![],
            error: None,
            statistics: Value::Null,
        });
        Ok(())
    }

    fn initialize(&mut self, params: &Value, root: Option<&Path>) -> Result<Value> {
        ensure!(!self.initialized, "initialize called twice");
        self.capabilities = params
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let Some(options) = params.get("initializationOptions") {
            self.options
                .insert("lsp-initialization-options".into(), options.to_string());
        }
        self.encoding = self
            .capabilities
            .pointer("/general/positionEncodings")
            .and_then(Value::as_array)
            .and_then(|names| {
                names
                    .iter()
                    .filter_map(Value::as_str)
                    .find_map(PositionEncoding::from_name)
            })
            .unwrap_or_else(|| PositionEncoding::from_name("utf-16").unwrap());
        self.maps = LspSourceMaps::new(
            self.encoding,
            PositionEncoding::from_name("utf-32").unwrap(),
        );
        self.pull = self
            .capabilities
            .pointer("/textDocument/diagnostic")
            .is_some();
        if let Some(folders) = params["workspaceFolders"].as_array() {
            for folder in folders {
                self.add_workspace(uri_path(folder["uri"].as_str().context("workspace URI")?)?)?;
            }
        }
        if self.workspaces.is_empty() {
            let root = if let Some(root) = root {
                root.to_owned()
            } else if let Some(uri) = params["rootUri"].as_str() {
                uri_path(uri)?
            } else if let Some(path) = params["rootPath"].as_str() {
                PathBuf::from(path)
            } else {
                std::env::current_dir()?
            };
            self.add_workspace(root)?;
        }
        let mut result = self.workspaces[0]
            .session
            .checker_mut()
            .context("project checker was not started")?
            .initialization()
            .clone();
        let caps = &mut result["capabilities"];
        caps["positionEncoding"] = json!(self.encoding.as_name());
        caps["textDocumentSync"] = json!({"openClose":true,"change":2,"save":{"includeText":true}});
        caps["diagnosticProvider"] = json!({"identifier":"ty-companion","interFileDependencies":true,"workspaceDiagnostics":true});
        // Full token responses remain valid for clients that support deltas.
        if !caps["semanticTokensProvider"].is_null() {
            caps["semanticTokensProvider"]["full"] = json!(true);
        }
        result["serverInfo"] =
            json!({"name":"ty-companion-rust","version":env!("CARGO_PKG_VERSION")});
        self.initialized = true;
        Ok(result)
    }

    fn workspace_index(&self, uri: &str) -> Result<usize> {
        let path = self.document_path(uri)?;
        self.workspaces
            .iter()
            .enumerate()
            .filter(|(_, w)| path.starts_with(&w.root))
            .max_by_key(|(_, w)| w.root.components().count())
            .map(|(i, _)| i)
            .context("document is outside initialized workspaces")
    }
    fn document_path(&self, uri: &str) -> Result<PathBuf> {
        if let Some(notebook) = self
            .notebooks
            .values()
            .find(|book| book.uri() == uri || book.contains_cell(uri))
        {
            return Ok(notebook.path().to_owned());
        }
        self.aliases
            .get(uri)
            .cloned()
            .map_or_else(|| uri_path(uri), Ok)
    }
    fn register_document(&mut self, uri: &str) -> Result<()> {
        if crate::lsp_mapping::decode_file_uri(uri)?.is_some() {
            let path = uri_path(uri)?;
            self.aliases.insert(uri.into(), path);
        }
        if self.workspace_index(uri).is_ok() {
            return Ok(());
        }
        if crate::lsp_mapping::decode_file_uri(uri)?.is_some() {
            let path = uri_path(uri)?;
            self.add_workspace(path.parent().context("document parent")?.to_owned())?;
        } else {
            if self.workspaces.is_empty() {
                self.add_workspace(std::env::current_dir()?)?;
            }
            let root = &self.workspaces[0].root;
            loop {
                let path = root.join(format!(
                    "__ty_unsaved_{}_{}.py",
                    std::process::id(),
                    self.next_document
                ));
                self.next_document += 1;
                if !path.exists() && !self.aliases.values().any(|p| p == &path) {
                    self.aliases.insert(uri.into(), path);
                    break;
                }
            }
        }
        Ok(())
    }
    fn dirty(&mut self, uri: &str) -> Result<()> {
        let index = self.workspace_index(uri)?;
        self.workspaces[index].dirty = true;
        self.workspaces[index].error = None;
        Ok(())
    }

    fn refresh(&mut self) -> Result<bool> {
        let result = self.refresh_inner();
        if result.is_err() {
            // A preceding workspace may already have committed a semantic
            // revision before another workspace or response mapping fails.
            // The next refresh must not reuse hints from before that revision.
            self.hint_cache.clear();
        }
        result
    }

    fn refresh_inner(&mut self) -> Result<bool> {
        let started = Instant::now();
        let mut changed = false;
        let mut reuse_native_hints = self.notebooks.is_empty();
        let mut hint_requests = 0usize;
        let mut hint_cache_hits = 0usize;
        let aliases = &self.aliases;
        let original_uri = |path: &Path| -> Result<String> {
            aliases
                .iter()
                .find(|(_, p)| p.as_path() == path)
                .map(|(uri, _)| uri.clone())
                .map_or_else(|| file_uri(path), Ok)
        };
        for workspace in &mut self.workspaces {
            if !workspace.dirty {
                continue;
            }
            let mut overlays: BTreeMap<_, _> = self
                .buffers
                .iter()
                .filter_map(|(uri, buffer)| {
                    aliases
                        .get(uri)
                        .cloned()
                        .or_else(|| uri_path(uri).ok())
                        .filter(|p| p.starts_with(&workspace.root))
                        .map(|p| (p, buffer.text.clone()))
                })
                .collect();
            for notebook in self
                .notebooks
                .values()
                .filter(|book| book.path().starts_with(&workspace.root))
            {
                workspace
                    .session
                    .register_notebook_source(notebook.path())?;
                overlays.insert(notebook.path().to_owned(), notebook.combined());
            }
            let result = match workspace.session.check(&overlays) {
                Ok(result) => result,
                Err(error) => {
                    workspace.dirty = false;
                    workspace.error = Some(format!("{error:#}"));
                    return Err(error);
                }
            };
            reuse_native_hints &= result["reused_projection"] == true
                && matches!(
                    result["reuse_reason"].as_str(),
                    Some("unchanged" | "trailing-trivia")
                );
            workspace.error = None;
            workspace.statistics = result.clone();
            if let Some(object) = workspace.statistics.as_object_mut() {
                object.remove("diagnostics");
                object.remove("suppressed_generated");
            }
            workspace.revision += 1;
            workspace.diagnostics.clear();
            workspace.raw_diagnostics.clear();
            let documents = workspace.session.documents();
            for document in &documents {
                // Closed notebooks have no client cell URI/position mapping.
                // Native ty also excludes them from closed-file requests.
                if Path::new(&document.source.path)
                    .extension()
                    .is_some_and(|ext| ext == "ipynb")
                    || self
                        .notebooks
                        .values()
                        .any(|book| book.path() == Path::new(&document.source.path))
                {
                    continue;
                }
                workspace
                    .diagnostics
                    .entry(original_uri(Path::new(&document.source.path))?)
                    .or_default();
            }
            for diagnostic in result["diagnostics"]
                .as_array()
                .context("project result diagnostics")?
            {
                let path = PathBuf::from(
                    diagnostic["location"]["path"]
                        .as_str()
                        .context("diagnostic source path")?,
                );
                // Native cell diagnostics are collected after splitting the
                // final projection. They retain cell-local action data.
                if path.extension().is_some_and(|ext| ext == "ipynb")
                    || self.notebooks.values().any(|book| book.path() == path)
                {
                    continue;
                }
                let uri = original_uri(&path)?;
                let text = match self.buffers.get(&uri) {
                    Some(buffer) => buffer.text.clone(),
                    None => match documents
                        .iter()
                        .find(|document| Path::new(&document.source.path) == path)
                    {
                        Some(document) => document.source.text.clone(),
                        None => std::fs::read_to_string(&path)?,
                    },
                };
                let position = |p: &Value| -> Result<Value> {
                    let line = p["line"]
                        .as_u64()
                        .context("diagnostic line")?
                        .checked_sub(1)
                        .context("zero diagnostic line")?;
                    let mut character = p["column"]
                        .as_u64()
                        .context("diagnostic column")?
                        .checked_sub(1)
                        .context("zero diagnostic column")?;
                    if line == 0 && text.starts_with('\u{feff}') {
                        character += 1;
                    }
                    let offset = position_to_offset(
                        &text,
                        &json!({"line":line,"character":character}),
                        PositionEncoding::from_name("utf-32").unwrap(),
                    )?;
                    offset_to_position(&text, offset, self.encoding)
                };
                let code = diagnostic["check_name"].as_str().unwrap_or("plugin");
                let description = diagnostic["description"].as_str().unwrap_or("");
                let message = description
                    .strip_prefix(&format!("{code}: "))
                    .unwrap_or(description);
                let presented = json!({
                    "range":{"start":position(&diagnostic["location"]["positions"]["begin"])? ,"end":position(&diagnostic["location"]["positions"]["end"])?},
                    "message":message,"code":code,"source":"ty",
                    "severity":match diagnostic["severity"].as_str() { Some("info")=>3,Some("minor")=>2,_=>1 }
                });
                if let Some(raw) = diagnostic.get("lsp_diagnostic") {
                    workspace
                        .raw_diagnostics
                        .push((uri.clone(), presented.clone(), raw.clone()));
                }
                workspace
                    .diagnostics
                    .entry(uri)
                    .or_default()
                    .push(presented);
            }
            workspace.dirty = false;
            changed = true;
        }
        if changed {
            if !reuse_native_hints {
                self.hint_cache.clear();
            }
            let mut active_hint_keys = BTreeSet::new();
            self.maps.clear_documents();
            let mut notebook_reports = Vec::new();
            let mut hint_reports = Vec::new();
            for (index, workspace) in self.workspaces.iter_mut().enumerate() {
                for (generated, original) in workspace.session.names() {
                    self.maps.add_name(generated, original);
                }
                for document in workspace.session.documents() {
                    if let Some(notebook) = self
                        .notebooks
                        .values_mut()
                        .find(|book| book.path() == Path::new(&document.source.path))
                    {
                        let checker = workspace
                            .session
                            .checker_mut()
                            .context("notebook checker")?;
                        for map in notebook.project(&document, checker)? {
                            self.maps.insert(map);
                        }
                        for (original, generated) in notebook.generated_cells() {
                            let report = checker.request(
                                "textDocument/diagnostic",
                                json!({"textDocument":{"uri":generated}}),
                            )?;
                            notebook_reports.push((
                                index,
                                original.clone(),
                                generated.clone(),
                                report,
                            ));
                        }
                        self.maps.add_hidden_prefix(document.source.prefix.clone());
                        continue;
                    }
                    if Path::new(&document.source.path)
                        .extension()
                        .is_some_and(|ext| ext == "ipynb")
                    {
                        continue;
                    }
                    let uri = original_uri(Path::new(&document.source.path))?;
                    let generated_uri = file_uri(&document.path)?;
                    if self.buffers.contains_key(&uri) {
                        let checker = workspace.session.checker_mut().context("editor checker")?;
                        let key = (checker.process_id(), generated_uri.clone());
                        let hints = if let Some(hints) = self.hint_cache.get(&key) {
                            hint_cache_hits += 1;
                            hints.clone()
                        } else {
                            hint_requests += 1;
                            let report = checker.request(
                                "textDocument/diagnostic",
                                json!({"textDocument":{"uri":generated_uri}}),
                            )?;
                            let hints = report["items"]
                                .as_array()
                                .context("editor diagnostics")?
                                .iter()
                                .filter(|diagnostic| diagnostic["code"].is_null())
                                .cloned()
                                .collect::<Vec<_>>();
                            self.hint_cache.insert(key.clone(), hints.clone());
                            hints
                        };
                        active_hint_keys.insert(key);
                        hint_reports.push((
                            index,
                            uri.clone(),
                            generated_uri.clone(),
                            json!({"items":hints}),
                        ));
                    }
                    self.maps.add_hidden_prefix(document.source.prefix.clone());
                    self.maps.insert(
                        DocumentMap::new(
                            uri,
                            generated_uri,
                            document.source.text.clone(),
                            document.rendered,
                        )
                        .with_names(document.names)
                        .with_module(document.source.module.clone()),
                    );
                }
            }
            self.hint_cache
                .retain(|key, _| active_hint_keys.contains(key));
            // CLI normalization deliberately omits uncoded editor hints such
            // as unused parameters. Keep the native final projection's tags,
            // severity and ranges for open editor documents.
            for (index, uri, generated, report) in hint_reports {
                let workspace = &mut self.workspaces[index];
                workspace
                    .diagnostics
                    .entry(uri.clone())
                    .or_default()
                    .retain(|diagnostic| !diagnostic["code"].is_null());
                for raw in report["items"]
                    .as_array()
                    .context("editor diagnostics")?
                    .iter()
                    .filter(|diagnostic| diagnostic["code"].is_null())
                {
                    let mapped = self
                        .maps
                        .diagnostics(&json!({"uri":generated,"diagnostics":[raw]}))?;
                    for diagnostic in mapped["diagnostics"]
                        .as_array()
                        .context("mapped editor hints")?
                    {
                        self.maps.add_diagnostic(&uri, diagnostic, raw.clone());
                        workspace
                            .diagnostics
                            .entry(uri.clone())
                            .or_default()
                            .push(diagnostic.clone());
                    }
                }
            }
            for (index, uri, generated, report) in notebook_reports {
                let workspace = &mut self.workspaces[index];
                workspace
                    .diagnostics
                    .entry(uri.clone())
                    .or_default()
                    .clear();
                for raw in report["items"].as_array().context("notebook diagnostics")? {
                    let mapped = self
                        .maps
                        .diagnostics(&json!({"uri":generated,"diagnostics":[raw]}))?;
                    for diagnostic in mapped["diagnostics"]
                        .as_array()
                        .context("mapped notebook diagnostics")?
                    {
                        self.maps.add_diagnostic(&uri, diagnostic, raw.clone());
                        workspace
                            .diagnostics
                            .entry(uri.clone())
                            .or_default()
                            .push(diagnostic.clone());
                    }
                }
            }
            for workspace in &mut self.workspaces {
                for (uri, presented, raw) in &mut workspace.raw_diagnostics {
                    let mapped = self.maps.diagnostics(
                        &json!({"uri":self.maps.generated_uri(uri),"diagnostics":[raw]}),
                    )?;
                    if let Some(details) = mapped["diagnostics"]
                        .as_array()
                        .and_then(|items| items.first())
                    {
                        let previous = presented.clone();
                        for key in ["relatedInformation", "codeDescription", "tags"] {
                            if let Some(value) = details.get(key) {
                                presented[key] = value.clone();
                            }
                        }
                        if let Some(message) = details["message"]
                            .as_str()
                            .and_then(|message| message.split_once("\n\n"))
                            .map(|(_, notes)| notes)
                        {
                            let notes = format!("\n\n{message}");
                            let primary = presented["message"].as_str().unwrap_or("");
                            if !primary.ends_with(&notes) {
                                presented["message"] = json!(format!("{primary}{notes}"));
                            }
                        }
                        if let Some(items) = workspace.diagnostics.get_mut(uri)
                            && let Some(item) = items.iter_mut().find(|d| **d == previous)
                        {
                            *item = presented.clone();
                        }
                    }
                    self.maps.add_diagnostic(uri, presented, raw.clone());
                }
            }
            self.timings.push(json!({"seconds":started.elapsed().as_secs_f64(),"open_documents":self.buffers.len(),"hint_requests":hint_requests,"hint_cache_hits":hint_cache_hits,"projects":self.workspaces.iter().map(|w|w.statistics.clone()).collect::<Vec<_>>()}));
            if self.timings.len() > 1024 {
                self.timings.remove(0);
            }
            self.publications_pending = true;
        }
        Ok(changed)
    }

    fn sync_watchers(&mut self, output: &mut impl Write) -> Result<()> {
        if !self.client_ready
            || self.capabilities["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"]
                != true
        {
            return Ok(());
        }
        let mut patterns = BTreeSet::new();
        for workspace in &self.workspaces {
            for path in workspace.session.watch_paths() {
                let mut pattern = watcher_glob(&path);
                // Missing dependency roots must already watch their children:
                // an installation can create the root and package atomically.
                if !path.is_file() {
                    pattern.push_str("/**/*");
                }
                patterns.insert(pattern);
            }
        }
        if patterns == self.watched_patterns {
            return Ok(());
        }
        if let Some(registration) = self.watcher_registration.take() {
            let id = format!("ty-companion-client-{}", self.next_client_request);
            self.next_client_request += 1;
            send(
                output,
                &json!({"jsonrpc":"2.0","id":id,"method":"client/unregisterCapability",
                "params":{"unregisterations":[{"id":registration,"method":"workspace/didChangeWatchedFiles"}]}}),
            )?;
        }
        if !patterns.is_empty() {
            let id = format!("ty-companion-client-{}", self.next_client_request);
            self.next_client_request += 1;
            let registration = format!("{id}-files");
            let watchers: Vec<_> = patterns
                .iter()
                .map(|pattern| json!({"globPattern":pattern,"kind":7}))
                .collect();
            send(
                output,
                &json!({"jsonrpc":"2.0","id":id,"method":"client/registerCapability",
                "params":{"registrations":[{"id":registration,"method":"workspace/didChangeWatchedFiles","registerOptions":{"watchers":watchers}}]}}),
            )?;
            self.watcher_registration = Some(registration);
        }
        self.watched_patterns = patterns;
        Ok(())
    }

    fn publish(&mut self, output: &mut impl Write) -> Result<()> {
        self.sync_watchers(output)?;
        if !self.publications_pending {
            return Ok(());
        }
        // Notebook diagnostics are pushed even when ordinary files use pull.
        for notebook in self.notebooks.values() {
            let workspace = &self.workspaces[self.workspace_index(notebook.uri())?];
            for cell in notebook.cells().iter().filter(|cell| cell.kind == 2) {
                let diagnostics = workspace
                    .diagnostics
                    .get(&cell.uri)
                    .cloned()
                    .unwrap_or_default();
                send(
                    output,
                    &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":cell.uri,"version":cell.version,"diagnostics":diagnostics}}),
                )?;
            }
        }
        // A dependency or plugin change can alter hints and tokens in an
        // unchanged open file. Ask supporting editors to invalidate those
        // caches as well as diagnostics after the projection is synchronized.
        for (capability, method, enabled) in [
            ("diagnostics", "workspace/diagnostic/refresh", self.pull),
            ("semanticTokens", "workspace/semanticTokens/refresh", true),
            ("inlayHint", "workspace/inlayHint/refresh", true),
        ] {
            if enabled && self.capabilities["workspace"][capability]["refreshSupport"] == true {
                let id = format!("ty-companion-refresh-{}", self.next_client_request);
                self.next_client_request += 1;
                send(
                    output,
                    &json!({"jsonrpc":"2.0","id":id,"method":method,"params":null}),
                )?;
            }
        }
        if self.pull {
            self.publications_pending = false;
            return Ok(());
        }
        for (uri, buffer) in &self.buffers {
            let workspace = &self.workspaces[self.workspace_index(uri)?];
            let diagnostics = workspace.diagnostics.get(uri).cloned().unwrap_or_default();
            send(
                output,
                &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"version":buffer.version,"diagnostics":diagnostics}}),
            )?;
        }
        self.publications_pending = false;
        Ok(())
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value> {
        if method == "shutdown" {
            self.shutdown = true;
            return Ok(Value::Null);
        }
        if !self.initialized {
            return Err(protocol(-32002, "server is not initialized"));
        }
        if self.shutdown {
            return Err(protocol(-32600, "server has shut down"));
        }
        self.refresh()?;
        if let Some(error) = self.workspaces.iter().find_map(|w| w.error.as_deref()) {
            return Err(protocol(-32803, error));
        }
        if method == "textDocument/diagnostic" {
            let uri = params["textDocument"]["uri"]
                .as_str()
                .context("diagnostic URI")?;
            let workspace = &self.workspaces[self.workspace_index(uri)?];
            let id = workspace.revision.to_string();
            return Ok(if params["previousResultId"] == id {
                json!({"kind":"unchanged","resultId":id})
            } else {
                json!({"kind":"full","resultId":id,"items":workspace.diagnostics.get(uri).cloned().unwrap_or_default()})
            });
        }
        if method == "workspace/diagnostic" {
            let mut items = vec![];
            for workspace in &self.workspaces {
                for (uri, diagnostics) in &workspace.diagnostics {
                    let id = workspace.revision.to_string();
                    let previous = params["previousResultIds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|v| v["uri"] == *uri);
                    let version = self.buffers.get(uri).map(|b| b.version);
                    items.push(if previous.is_some_and(|v| v["value"] == id) {
                        json!({"uri":uri,"version":version,"kind":"unchanged","resultId":id})
                    } else { json!({"uri":uri,"version":version,"kind":"full","resultId":id,"items":diagnostics}) });
                }
            }
            return Ok(json!({"items":items}));
        }
        if method == "tyCompanion/statistics" {
            let pids: Vec<_> = self
                .workspaces
                .iter_mut()
                .filter_map(|w| w.session.checker_mut().map(|c| c.process_id()))
                .collect();
            let notebooks:Vec<_>=self.notebooks.values().map(|book|json!({"uri":book.uri(),"version":book.version(),"python_cells":book.spans().len()})).collect();
            return Ok(
                json!({"updates":self.timings,"workspaces":self.workspaces.len(),"open_documents":self.buffers.len(),"checker_pids":pids,"notebooks":notebooks}),
            );
        }
        if params["textDocument"]["uri"].as_str().is_some_and(|uri| {
            self.notebooks
                .values()
                .any(|book| book.contains_cell(uri) && !book.is_code_cell(uri))
        }) {
            return Ok(match method {
                "textDocument/semanticTokens/full" | "textDocument/semanticTokens/range" => {
                    json!({"data":[]})
                }
                "textDocument/documentSymbol"
                | "textDocument/foldingRange"
                | "textDocument/documentHighlight"
                | "textDocument/inlayHint"
                | "textDocument/codeAction" => json!([]),
                _ => Value::Null,
            });
        }
        let mapped = self.maps.request(method, params)?;
        let uri = self.maps.request_uri(method, params)?;
        let indexes: Vec<usize> = if let Some(uri) = uri {
            vec![self.workspace_index(&uri)?]
        } else if method == "workspace/symbol" {
            (0..self.workspaces.len()).collect()
        } else if self.workspaces.is_empty() {
            vec![]
        } else {
            vec![0]
        };
        let mut results = vec![];
        for index in indexes {
            let checker = self.workspaces[index]
                .session
                .checker_mut()
                .context("project server missing")?;
            if method == "textDocument/references" {
                // A member synthesized by a plugin has no declaration in the
                // user's program. Do not invent navigable references to it.
                let definition = checker.request(
                    "textDocument/definition",
                    self.maps.request("textDocument/definition", params)?,
                )?;
                let definition =
                    self.maps
                        .response("textDocument/definition", params, &definition)?;
                if definition.is_null() || definition.as_array().is_some_and(Vec::is_empty) {
                    results.push(Value::Null);
                    continue;
                }
            }
            let result = if method == "textDocument/documentHighlight"
                && let Some(notebook) = params["textDocument"]["uri"]
                    .as_str()
                    .and_then(|uri| self.notebooks.values().find(|book| book.contains_cell(uri)))
            {
                notebook.document_highlights(
                    checker,
                    mapped["textDocument"]["uri"]
                        .as_str()
                        .context("mapped notebook cell")?,
                    mapped.clone(),
                )?
            } else if method == "textDocument/documentSymbol"
                && let Some(notebook) = params["textDocument"]["uri"]
                    .as_str()
                    .and_then(|uri| self.notebooks.values().find(|book| book.contains_cell(uri)))
            {
                notebook.document_symbols(
                    checker,
                    mapped["textDocument"]["uri"]
                        .as_str()
                        .context("mapped notebook cell")?,
                    mapped.clone(),
                )?
            } else if method == "textDocument/signatureHelp" {
                if let Some((uri, text, mut parameters)) = self.maps.signature_view(params)? {
                    if let Some(notebook) = params["textDocument"]["uri"].as_str().and_then(|uri| {
                        self.notebooks.values().find(|book| book.contains_cell(uri))
                    }) {
                        notebook.signature_help(checker, &uri, &text, parameters)?
                    } else {
                        let path = uri_path(&uri)?;
                        let shadow = path.with_file_name(format!(
                            "__ty_signature_{}.py",
                            path.file_stem()
                                .context("signature source stem")?
                                .to_string_lossy()
                        ));
                        let shadow_uri = file_uri(&shadow)?;
                        parameters["textDocument"]["uri"] = json!(shadow_uri);
                        checker.notify("textDocument/didOpen",json!({"textDocument":{"uri":shadow_uri,"languageId":"python","version":1,"text":text}}))?;
                        let result = checker.request(method, parameters);
                        // Close even when the request fails: subsequent queries
                        // must never observe an obsolete declaration-only view.
                        let closed = checker.notify(
                            "textDocument/didClose",
                            json!({"textDocument":{"uri":shadow_uri}}),
                        );
                        let result = result?;
                        closed?;
                        result
                    }
                } else {
                    checker.request(method, mapped.clone())?
                }
            } else {
                checker.request(method, mapped.clone())?
            };
            results.push(self.maps.response(method, params, &result)?);
        }
        if method == "workspace/symbol" {
            Ok(Value::Array(
                results
                    .into_iter()
                    .filter_map(|v| v.as_array().cloned())
                    .flatten()
                    .collect(),
            ))
        } else {
            Ok(results.pop().unwrap_or(Value::Null))
        }
    }

    fn notification(
        &mut self,
        method: &str,
        params: &Value,
        output: &mut impl Write,
    ) -> Result<()> {
        match method {
            "initialized" => self.client_ready = true,
            "$/setTrace" | "$/cancelRequest" => {}
            "notebookDocument/didOpen" => {
                let uri = params["notebookDocument"]["uri"]
                    .as_str()
                    .context("notebook URI")?;
                ensure!(!self.notebooks.contains_key(uri), "notebook already open");
                self.register_document(uri)?;
                let path = self.document_path(uri)?;
                let notebook = Notebook::open(params, &path)?;
                self.notebooks.insert(uri.into(), notebook);
                self.dirty(uri)?;
            }
            "notebookDocument/didChange" => {
                let uri = params["notebookDocument"]["uri"]
                    .as_str()
                    .context("notebook URI")?;
                let book = self
                    .notebooks
                    .get_mut(uri)
                    .context("change for unopened notebook")?;
                let old: Vec<_> = book.cells().iter().map(|cell| cell.uri.clone()).collect();
                book.apply_change(params, self.encoding)?;
                for cell in old.into_iter().filter(|uri| !book.contains_cell(uri)) {
                    send(
                        output,
                        &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":cell,"diagnostics":[]}}),
                    )?;
                }
                self.dirty(uri)?;
            }
            "notebookDocument/didClose" => {
                let uri = params["notebookDocument"]["uri"]
                    .as_str()
                    .context("notebook URI")?;
                let index = self.workspace_index(uri)?;
                if let Some(mut notebook) = self.notebooks.remove(uri) {
                    if let Some(checker) = self.workspaces[index].session.checker_mut() {
                        notebook.close(checker)?;
                    }
                    for cell in notebook.cells() {
                        send(
                            output,
                            &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":cell.uri,"diagnostics":[]}}),
                        )?;
                    }
                }
                self.workspaces[index].dirty = true;
                self.workspaces[index].error = None;
                self.aliases.remove(uri);
            }
            "notebookDocument/didSave" => {}
            "textDocument/didOpen" => {
                let doc = &params["textDocument"];
                let uri = doc["uri"].as_str().context("open URI")?;
                self.register_document(uri)?;
                self.dirty(uri)?;
                self.buffers.insert(
                    uri.into(),
                    Buffer {
                        text: doc["text"].as_str().context("open text")?.into(),
                        version: doc["version"].as_i64().context("open version")?,
                    },
                );
            }
            "textDocument/didChange" => {
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .context("change URI")?;
                let version = params["textDocument"]["version"]
                    .as_i64()
                    .context("change version")?;
                let buffer = self
                    .buffers
                    .get_mut(uri)
                    .context("change for unopened document")?;
                ensure!(
                    version > buffer.version,
                    "document version did not increase"
                );
                apply_changes(
                    &mut buffer.text,
                    params["contentChanges"]
                        .as_array()
                        .context("content changes")?,
                    self.encoding,
                )?;
                buffer.version = version;
                self.dirty(uri)?;
            }
            "textDocument/didSave" => {
                let uri = params["textDocument"]["uri"].as_str().context("save URI")?;
                if let Some(text) = params["text"].as_str()
                    && let Some(buffer) = self.buffers.get_mut(uri)
                {
                    buffer.text = text.into();
                }
                self.dirty(uri)?;
            }
            "textDocument/didClose" => {
                let uri = params["textDocument"]["uri"]
                    .as_str()
                    .context("close URI")?;
                self.buffers.remove(uri);
                self.dirty(uri)?;
                if !self.pull {
                    send(
                        output,
                        &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[]}}),
                    )?;
                }
            }
            "workspace/didChangeWatchedFiles" => {
                let paths: Vec<_> = params["changes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|change| change["uri"].as_str().and_then(|uri| uri_path(uri).ok()))
                    .collect();
                for workspace in &mut self.workspaces {
                    let watched = workspace.session.watch_paths();
                    if paths.iter().any(|path| {
                        watched
                            .iter()
                            .any(|watch| path.starts_with(watch) || watch.starts_with(path))
                    }) {
                        if let Some(checker) = workspace.session.checker_mut() {
                            // Dependencies live at their original paths in the
                            // official server too, including negative lookups.
                            checker.notify(method, params.clone())?;
                        }
                        if paths.iter().any(|path| {
                            workspace.session.path_requires_reload(path)
                                || watched.iter().any(|watch| {
                                    watch.starts_with(path) && !watch.starts_with(&workspace.root)
                                })
                        }) {
                            workspace.session.invalidate();
                        }
                        workspace.dirty = true;
                    }
                }
            }
            "workspace/didChangeConfiguration" => {
                for workspace in &mut self.workspaces {
                    if let Some(checker) = workspace.session.checker_mut() {
                        checker.notify(method, params.clone())?;
                    }
                    workspace.session.invalidate();
                    workspace.dirty = true;
                }
            }
            "workspace/didChangeWorkspaceFolders" => {
                for folder in params["event"]["removed"].as_array().into_iter().flatten() {
                    let root = uri_path(folder["uri"].as_str().context("removed folder URI")?)?;
                    let removed_books: Vec<_> = self
                        .notebooks
                        .iter()
                        .filter(|(_, book)| book.path().starts_with(&root))
                        .map(|(uri, _)| uri.clone())
                        .collect();
                    for uri in removed_books {
                        let index = self.workspace_index(&uri)?;
                        let mut book = self.notebooks.remove(&uri).unwrap();
                        if let Some(checker) = self.workspaces[index].session.checker_mut() {
                            book.close(checker)?;
                        }
                        for cell in book.cells() {
                            send(
                                output,
                                &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":cell.uri,"diagnostics":[]}}),
                            )?;
                        }
                    }
                    let removed: Vec<_> = self
                        .buffers
                        .keys()
                        .filter(|uri| self.document_path(uri).is_ok_and(|p| p.starts_with(&root)))
                        .cloned()
                        .collect();
                    for uri in removed {
                        self.buffers.remove(&uri);
                        if !self.pull {
                            send(
                                output,
                                &json!({"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":uri,"diagnostics":[]}}),
                            )?;
                        }
                    }
                    self.workspaces.retain(|w| w.root != root);
                    self.aliases.retain(|_, path| !path.starts_with(&root));
                }
                for folder in params["event"]["added"].as_array().into_iter().flatten() {
                    self.add_workspace(uri_path(
                        folder["uri"].as_str().context("added folder URI")?,
                    )?)?;
                }
                // Rebuild maps after removal even if all remaining sessions
                // have reusable projections. Removed documents must not be
                // available to resolve requests from old completion results.
                if self.workspaces.is_empty() {
                    self.maps.clear_documents();
                } else {
                    for workspace in &mut self.workspaces {
                        workspace.dirty = true;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

pub fn run(root: Option<&Path>, mut options: BTreeMap<String, String>) -> Result<()> {
    default_checker(&mut options)?;
    let owned_output = if options.contains_key("out") {
        None
    } else {
        Some(scratch()?)
    };
    if let Some(output) = &owned_output {
        options.insert("out".into(), output.0.display().to_string());
    }
    let cancelled = Arc::new(Mutex::new(BTreeSet::new()));
    let (sender, incoming) = mpsc::channel();
    let reader_cancelled = cancelled.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(io::stdin());
        loop {
            let message = read_frame(&mut reader);
            let failed = message.is_err();
            if let Ok(message) = &message
                && message["method"] == "$/cancelRequest"
            {
                reader_cancelled
                    .lock()
                    .unwrap()
                    .insert(message["params"]["id"].to_string());
            }
            if sender.send(message).is_err() || failed {
                break;
            }
        }
    });
    let mut editor = Editor::new(options, cancelled);
    let mut output = io::stdout().lock();
    let mut pending = VecDeque::new();
    loop {
        let message = if let Some(message) = pending.pop_front() {
            message
        } else {
            match incoming.recv_timeout(Duration::from_millis(30)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if editor.initialized && !editor.shutdown {
                        match editor.refresh() {
                            Ok(true) => editor.publish(&mut output)?,
                            Ok(false) => {}
                            Err(error) => {
                                send(
                                    &mut output,
                                    &json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"type":1,"message":format!("{error:#}")}}),
                                )?;
                            }
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        let message = match message {
            Ok(message) => message,
            Err(error) => {
                if error.to_string().contains("stdout closed") {
                    break;
                }
                return Err(error);
            }
        };
        let Some(method) = message["method"].as_str() else {
            continue;
        };
        if method == "exit" {
            ensure!(editor.shutdown, "exit received before shutdown");
            break;
        }
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = message.get("id") {
            let is_cancelled = editor.cancelled.lock().unwrap().remove(&id.to_string());
            let result = if is_cancelled {
                Err(protocol(-32800, "request cancelled"))
            } else if method == "initialize" {
                editor.initialize(&params, root)
            } else {
                editor.request(method, &params)
            };
            let result = if editor.cancelled.lock().unwrap().remove(&id.to_string()) {
                Err(protocol(-32800, "request cancelled"))
            } else {
                result
            };
            response(&mut output, id, result)?;
        } else if let Err(error) = editor.notification(method, &params, &mut output) {
            send(
                &mut output,
                &json!({"jsonrpc":"2.0","method":"window/logMessage","params":{"type":1,"message":format!("{error:#}")}}),
            )?;
        }
        // Drain queued edits before doing expensive work. Requests remain in
        // arrival order; an edit cannot overtake a preceding hover or rename.
        pending.extend(incoming.try_iter());
        editor.publish(&mut output)?;
    }
    // Drop sessions before removing their generated workspace and server logs.
    drop(editor);
    drop(owned_output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn watcher_patterns_match_literal_project_paths() -> Result<()> {
        for path in [
            "/tmp/project [copy]",
            "/tmp/project{test}",
            "/tmp/project?*",
        ] {
            let glob = format!("{}/**/*", watcher_glob(Path::new(path)));
            let matcher = globset::Glob::new(&glob)?.compile_matcher();
            assert!(matcher.is_match(format!("{path}/demo.py")), "{glob}");
            assert!(!matcher.is_match("/tmp/other/demo.py"));
        }
        Ok(())
    }

    #[test]
    fn incremental_changes_are_atomic_and_utf16_aware() -> Result<()> {
        let encoding = PositionEncoding::from_name("utf-16").unwrap();
        let mut text = "name = '😀'\r\nvalue = 1\r\n".to_string();
        apply_changes(
            &mut text,
            &[
                json!({"range":{"start":{"line":0,"character":8},"end":{"line":0,"character":10}},"text":"ok"}),
            ],
            encoding,
        )?;
        assert_eq!(text, "name = 'ok'\r\nvalue = 1\r\n");
        let before = text.clone();
        assert!(apply_changes(&mut text,&[json!({"text":"changed"}),json!({"range":{"start":{"line":90,"character":0},"end":{"line":90,"character":1}},"text":"x"})],encoding).is_err());
        assert_eq!(text, before);
        Ok(())
    }
    #[test]
    fn file_uris_roundtrip_spaces_unicode_and_percent() -> Result<()> {
        let path = std::env::temp_dir()
            .canonicalize()?
            .join("example 😀/100%.py");
        assert_eq!(uri_path(&file_uri(&path)?)?, path);
        #[cfg(not(windows))]
        assert!(uri_path("file://remote/example.py").is_err());
        assert!(uri_path("file:///tmp/%00.py").is_err());
        Ok(())
    }

    #[test]
    fn existing_external_file_uri_preserves_exact_canonical_path() -> Result<()> {
        let directory = scratch()?;
        let file = directory.0.join("external 😀 100%.pyi");
        std::fs::write(&file, "value: int\n")?;
        let expected = file.canonicalize()?;
        let mapped = uri_path(&file_uri(&file)?)?;
        // Path equality ignores trailing separators; diagnostic rendering and
        // file URI serialization must preserve the exact underlying string.
        assert_eq!(mapped.as_os_str(), expected.as_os_str());
        assert_eq!(file_uri(&mapped)?, file_uri(&expected)?);
        assert_eq!(std::fs::read_to_string(mapped)?, "value: int\n");
        Ok(())
    }

    #[test]
    fn missing_child_uri_keeps_suffix_after_canonical_parent() -> Result<()> {
        let directory = scratch()?;
        let parent = directory.0.join("parent");
        std::fs::create_dir(&parent)?;
        #[cfg(unix)]
        let parent = {
            let alias = directory.0.join("alias");
            std::os::unix::fs::symlink(&parent, &alias)?;
            alias
        };
        let file = parent.join("missing").join("new.py");
        let expected = parent.canonicalize()?.join("missing").join("new.py");
        let mapped = uri_path(&file_uri(&file)?)?;
        assert_eq!(mapped.as_os_str(), expected.as_os_str());
        assert!(!mapped.exists());
        Ok(())
    }
}
