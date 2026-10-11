//! Native transport to an unchanged official ty executable.
//!
//! This bounded research mode fixes Python 3.13 and disables uv discovery. The
//! persistent path reuses one public LSP session and one virtual document across
//! oracle rounds; it does not link or patch the checker.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug)]
pub struct ProtocolError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}
impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}
impl std::error::Error for ProtocolError {}

pub struct Checker {
    binary: PathBuf,
    server: Option<Server>,
    pub startup_seconds: f64,
    pub queries: usize,
}

impl Checker {
    pub fn new(binary: &Path, out: &Path, persistent: bool) -> Result<Self> {
        let started = Instant::now();
        let binary = fs::canonicalize(binary).context("official ty binary path")?;
        fs::create_dir_all(out)?;
        let out = fs::canonicalize(out)?;
        let server = if persistent {
            Some(Server::new(&binary, &out, false)?)
        } else {
            None
        };
        Ok(Self {
            binary,
            server,
            startup_seconds: started.elapsed().as_secs_f64(),
            queries: 0,
        })
    }

    /// Public LSP push diagnostics use ty's synchronous didChange path.
    /// On macOS this runs on its UserInitiated main thread, while pull
    /// diagnostics run on Utility worker threads. Neither modifies ty.
    pub fn new_push(binary: &Path, out: &Path) -> Result<Self> {
        let started = Instant::now();
        let binary = fs::canonicalize(binary).context("official ty binary path")?;
        fs::create_dir_all(out)?;
        let out = fs::canonicalize(out)?;
        let server = Some(Server::new(&binary, &out, true)?);
        Ok(Self {
            binary,
            server,
            startup_seconds: started.elapsed().as_secs_f64(),
            queries: 0,
        })
    }

    pub fn check(&mut self, path: &Path, source: &str) -> Result<Vec<Value>> {
        self.queries += 1;
        if let Some(server) = &mut self.server {
            server.check(path, source)
        } else {
            // Keep CLI controls identical to the original prototype. Each
            // caller path is generated output, never a user source document.
            fs::write(path, source)?;
            let output = Command::new(&self.binary)
                .args([
                    "check",
                    "--python-version",
                    "3.13",
                    "--output-format",
                    "gitlab",
                ])
                .arg(path)
                .current_dir(path.parent().context("checker input parent")?)
                .env("TY_UV", "0")
                .output()
                .context("start official ty check")?;
            ensure!(
                matches!(output.status.code(), Some(0 | 1)),
                "checker failed {}: {} {}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice(&output.stdout).context("checker JSON")
        }
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<std::result::Result<Value, String>>,
    next_id: u64,
    document_uri: String,
    version: i32,
    push: bool,
    stderr: PathBuf,
    initialized: Value,
    project: bool,
    workspace_configuration: Option<Value>,
    notifications: Vec<Value>,
}

/// A persistent official checker with the real generated module paths. All
/// changed documents are synchronized before requesting a batch's diagnostics.
pub struct ProjectChecker {
    server: Server,
    documents: BTreeMap<PathBuf, (String, i32, String)>,
    notebooks: BTreeMap<PathBuf, String>,
    external_sources: BTreeSet<PathBuf>,
}

impl ProjectChecker {
    pub fn new(binary: &Path, root: &Path, config: &Path) -> Result<Self> {
        let mut server = Server::configured(binary, root, config, true)?;
        server.project = true;
        Ok(Self {
            server,
            documents: BTreeMap::new(),
            notebooks: BTreeMap::new(),
            external_sources: BTreeSet::new(),
        })
    }
    pub fn new_editor(
        binary: &Path,
        root: &Path,
        config: &Path,
        capabilities: Value,
    ) -> Result<Self> {
        Self::new_editor_with_options(binary, root, config, capabilities, Value::Null)
    }
    pub fn new_editor_with_options(
        binary: &Path,
        root: &Path,
        config: &Path,
        capabilities: Value,
        initialization_options: Value,
    ) -> Result<Self> {
        Self::new_editor_with_configuration(
            binary,
            root,
            config,
            capabilities,
            initialization_options,
            None,
        )
    }
    /// `Some(null)` enables the configuration handshake before the outer IDE
    /// has replied. The queued request is answered after Session installs the
    /// relocated configuration and cached settings, before its first analysis.
    pub fn new_editor_with_configuration(
        binary: &Path,
        root: &Path,
        config: &Path,
        capabilities: Value,
        initialization_options: Value,
        workspace_configuration: Option<Value>,
    ) -> Result<Self> {
        let mut server = Server::configured_editor(
            binary,
            root,
            config,
            true,
            Some(capabilities),
            initialization_options,
            workspace_configuration,
        )?;
        server.project = true;
        Ok(Self {
            server,
            documents: BTreeMap::new(),
            notebooks: BTreeMap::new(),
            external_sources: BTreeSet::new(),
        })
    }
    pub fn set_workspace_configuration(&mut self, settings: Value) {
        self.server.workspace_configuration = Some(settings);
    }
    pub fn take_notifications(&mut self) -> Vec<Value> {
        std::mem::take(&mut self.server.notifications)
    }
    pub fn initialization(&self) -> &Value {
        &self.server.initialized
    }
    pub fn process_id(&self) -> u32 {
        self.server.child.id()
    }
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.server.request(method, params)
    }
    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.server.notify(method, params)
    }
    /// Use the upstream notebook parser for an aggregate notebook projection,
    /// including IPython escapes. The source retains its normal analysis path
    /// so diagnostics and oracle probes use the same byte source map.
    pub fn register_notebook_source(&mut self, path: &Path) -> Result<()> {
        if self.notebooks.contains_key(path) {
            return Ok(());
        }
        ensure!(
            !self.documents.contains_key(path),
            "notebook must be registered before opening its projection"
        );
        self.notebooks
            .insert(path.into(), file_uri(&path.with_extension("oracle.ipynb"))?);
        Ok(())
    }
    pub fn register_external_source(&mut self, path: &Path) -> Result<()> {
        if self.external_sources.contains(path) {
            return Ok(());
        }
        let parent = path.parent().context("external source has no parent")?;
        if !self
            .external_sources
            .iter()
            .filter_map(|file| file.parent())
            .any(|root| parent.starts_with(root))
        {
            // Pull diagnostics still honor workspace membership. Register the
            // original parent so ty can check the memory overlay in place;
            // the global generated configuration retains its import roots.
            self.server.notify("workspace/didChangeWorkspaceFolders", json!({"event":{"added":[{"uri":file_uri(parent)?,"name":"companion-external"}],"removed":[]}}))?;
        }
        self.external_sources.insert(path.into());
        Ok(())
    }
    pub fn sync_documents(&mut self, sources: &BTreeMap<PathBuf, String>) -> Result<()> {
        let removed: Vec<_> = self
            .documents
            .keys()
            .filter(|p| !sources.contains_key(*p))
            .cloned()
            .collect();
        let mut changed_external = removed
            .iter()
            .filter(|path| self.external_sources.contains(*path))
            .map(|path| {
                file_uri(path).map(|uri| json!({"uri":uri,"type":if path.exists() {2} else {3}}))
            })
            .collect::<Result<Vec<_>>>()?;
        for path in removed {
            let (uri, _, _) = self.documents.remove(&path).unwrap();
            if let Some(notebook) = self.notebooks.get(&path) {
                self.server.notify(
                    "notebookDocument/didClose",
                    json!({"notebookDocument":{"uri":notebook},"cellTextDocuments":[{"uri":uri}]}),
                )?;
            } else {
                self.server
                    .notify("textDocument/didClose", json!({"textDocument":{"uri":uri}}))?;
            }
        }
        changed_external.extend(
            sources
                .iter()
                .filter(|(path, source)| {
                    self.external_sources.contains(*path)
                        && self
                            .documents
                            .get(*path)
                            .is_none_or(|(_, _, previous)| previous != *source)
                })
                .map(|(path, _)| file_uri(path).map(|uri| json!({"uri":uri,"type":2})))
                .collect::<Result<Vec<_>>>()?,
        );
        for (path, source) in sources {
            self.update(path, source, false)?;
        }
        if !changed_external.is_empty() {
            // didChange synchronizes only the document's own project. Other
            // workspace databases may already cache it as an import; watcher
            // events refresh those dependencies against the shared open buffer.
            self.server.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes":changed_external}),
            )?;
        }
        Ok(())
    }
    fn update(&mut self, path: &Path, source: &str, force: bool) -> Result<()> {
        if let Some((uri, version, previous)) = self.documents.get_mut(path) {
            if !force && previous == source {
                return Ok(());
            }
            *version = version
                .checked_add(1)
                .context("LSP document version overflow")?;
            *previous = source.to_owned();
            if let Some(notebook) = self.notebooks.get(path) {
                self.server.notify("notebookDocument/didChange",json!({"notebookDocument":{"uri":notebook,"version":version},"change":{"cells":{"textContent":[{"document":{"uri":uri,"version":version},"changes":[{"text":source}]}]}}}))?;
            } else {
                self.server.notify("textDocument/didChange",json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":source}]}))?;
            }
        } else {
            let uri = file_uri(path)?;
            if let Some(notebook) = self.notebooks.get(path) {
                self.server.notify("notebookDocument/didOpen",json!({"notebookDocument":{"uri":notebook,"notebookType":"jupyter-notebook","version":1,"cells":[{"kind":2,"document":uri}]},"cellTextDocuments":[{"uri":uri,"languageId":"python","version":1,"text":source}]}))?;
            } else {
                self.server.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":"python","version":1,"text":source}}),
            )?;
            }
            self.documents
                .insert(path.to_owned(), (uri, 1, source.to_owned()));
        }
        Ok(())
    }
    pub fn check(
        &mut self,
        sources: &BTreeMap<PathBuf, String>,
        paths: &[PathBuf],
    ) -> Result<Vec<Value>> {
        self.sync_documents(sources)?;
        let mut pending = BTreeMap::new();
        for path in paths {
            let source = sources
                .get(path)
                .context("queried document absent from project view")?;
            if self.external_sources.contains(path) {
                continue;
            }
            // Rechecking after synchronization prevents a publication from an
            // earlier partial project state from satisfying this batch.
            self.update(path, source, true)?;
            let (uri, version, _) = self.documents.get(path).unwrap();
            pending.insert(uri.clone(), (path.clone(), *version));
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut output = Vec::new();
        while !pending.is_empty() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "official ty project diagnostics timed out; pending {} documents; log: {}",
                pending.len(),
                self.server.stderr.display()
            );
            let message = self
                .server
                .messages
                .recv_timeout(remaining)
                .context("waiting for project diagnostics")?
                .map_err(|error| anyhow!("official ty LSP transport: {error}"))?;
            if message["method"] == "textDocument/publishDiagnostics" {
                let uri = message["params"]["uri"]
                    .as_str()
                    .context("published document URI")?;
                if let Some((path, version)) = pending.get(uri) {
                    let received = message["params"]["version"]
                        .as_i64()
                        .context("published document version")?;
                    ensure!(
                        received <= i64::from(*version),
                        "future-version project diagnostics: {message}"
                    );
                    if received != i64::from(*version) {
                        continue;
                    }
                    for item in message["params"]["diagnostics"]
                        .as_array()
                        .context("published diagnostics array")?
                    {
                        if let Some(item) = normalize_diagnostic(path, &sources[path], item)? {
                            output.push(item);
                        }
                    }
                    pending.remove(uri);
                }
            } else {
                ensure!(
                    message.get("method").is_some(),
                    "unexpected response while awaiting project diagnostics: {message}"
                );
                self.server.handle_server_message(&message)?;
            }
        }
        // Documents outside the generated workspace do not publish eagerly.
        // Query these after consuming the workspace publications so handling a
        // request cannot discard a publication still needed by this batch.
        for path in paths
            .iter()
            .filter(|path| self.external_sources.contains(*path))
        {
            let uri = &self
                .documents
                .get(path)
                .context("external source was not synchronized")?
                .0;
            let report = self.server.request(
                "textDocument/diagnostic",
                json!({"textDocument":{"uri":uri}}),
            )?;
            for item in report["items"]
                .as_array()
                .context("external document diagnostics")?
            {
                if let Some(item) = normalize_diagnostic(path, &sources[path], item)? {
                    output.push(item);
                }
            }
        }
        Ok(output)
    }
}

impl Server {
    fn new(binary: &Path, out: &Path, push: bool) -> Result<Self> {
        let config = out.join("checker-config.toml");
        fs::write(&config, "[environment]\npython-version = \"3.13\"\n")?;
        Self::configured(binary, out, &config, push)
    }

    fn configured(binary: &Path, out: &Path, config: &Path, push: bool) -> Result<Self> {
        Self::configured_editor(binary, out, config, push, None, Value::Null, None)
    }

    fn configured_editor(
        binary: &Path,
        out: &Path,
        config: &Path,
        push: bool,
        capabilities: Option<Value>,
        initialization_options: Value,
        workspace_configuration: Option<Value>,
    ) -> Result<Self> {
        let initialization_options =
            crate::editor_options::initialization(&initialization_options)?;
        let stderr = out.join("checker-server.stderr.log");
        let mut child = Command::new(binary)
            .arg("server")
            .current_dir(out)
            .env("TY_UV", "0")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(File::create(&stderr)?)
            .spawn()
            .context("start official ty server")?;
        let stdin = child.stdin.take().context("server stdin")?;
        let stdout = child.stdout.take().context("server stdout")?;
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let message = read_frame(&mut reader).map_err(|error| format!("{error:#}"));
                let failed = message.is_err();
                if sender.send(message).is_err() || failed {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            stdin,
            messages,
            next_id: 1,
            document_uri: file_uri(&out.join("companion_document.py"))?,
            version: 0,
            push,
            stderr,
            initialized: Value::Null,
            project: false,
            workspace_configuration: workspace_configuration.clone(),
            notifications: vec![],
        };
        let root_uri = file_uri(out)?;
        let mut initialization = json!({
            "processId":std::process::id(),
            "rootUri":root_uri,
            "workspaceFolders":[{"uri":root_uri,"name":"companion-analysis"}],
            "capabilities":{
                "general":{"positionEncodings":["utf-32"]},
                "workspace":{"configuration":false},
                "textDocument":{
                    "diagnostic":{"dynamicRegistration":false,"relatedDocumentSupport":false},
                    "publishDiagnostics":{"relatedInformation":false}
                }
            },
            "initializationOptions":{
                "configurationFile":config,
                "diagnosticMode":"openFilesOnly",
                "showSyntaxErrors":true
            }
        });
        if let Some(mut capabilities) = capabilities {
            // The projection protocol has one fixed coordinate encoding. The
            // editor-facing side negotiates its own encoding and translates it.
            capabilities["general"]["positionEncodings"] = json!(["utf-32"]);
            capabilities["workspace"]["configuration"] = json!(workspace_configuration.is_some());
            capabilities["workspace"]["didChangeWatchedFiles"]["dynamicRegistration"] =
                json!(false);
            capabilities["window"]["workDoneProgress"] = json!(false);
            initialization["capabilities"] = capabilities;
        }
        if let Some(options) = initialization_options.as_object() {
            let merged = initialization["initializationOptions"]
                .as_object_mut()
                .unwrap();
            merged.extend(options.clone());
            // Session consumed original-path typing options. Forwarding them
            // here would undo relocation; pythonExtension must only appear in
            // the native workspace/configuration response (never both inputs).
            merged.remove("configuration");
            merged.remove("pythonExtension");
            // Always analyze the generated workspace with the relocated config.
            merged.insert("configurationFile".into(), json!(config));
            merged.insert("diagnosticMode".into(), json!("openFilesOnly"));
            merged.insert("showSyntaxErrors".into(), json!(true));
            merged.insert("disableLanguageServices".into(), json!(false));
        }
        if push
            && let Some(capabilities) =
                initialization["capabilities"]["textDocument"].as_object_mut()
        {
            capabilities.remove("diagnostic");
        }
        let initialized = server.request("initialize", initialization)?;
        ensure!(
            initialized
                .pointer("/capabilities/positionEncoding")
                .and_then(Value::as_str)
                == Some("utf-32"),
            "official ty did not negotiate required utf-32 position encoding: {initialized}"
        );
        ensure!(
            push || initialized
                .pointer("/capabilities/diagnosticProvider")
                .is_some(),
            "official ty does not advertise pull diagnostics: {initialized}"
        );
        server.notify("initialized", json!({}))?;
        server.initialized = initialized;
        Ok(server)
    }

    fn check(&mut self, path: &Path, source: &str) -> Result<Vec<Value>> {
        self.version = self
            .version
            .checked_add(1)
            .context("LSP document version overflow")?;
        if self.version == 1 {
            self.notify("textDocument/didOpen", json!({"textDocument":{
                "uri":self.document_uri,"languageId":"python","version":self.version,"text":source,
            }}))?;
        } else {
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument":{"uri":self.document_uri,"version":self.version},
                    "contentChanges":[{"text":source}],
                }),
            )?;
        }
        let items = if self.push {
            self.wait_published()?
        } else {
            let report = self.request(
                "textDocument/diagnostic",
                json!({"textDocument":{"uri":self.document_uri}}),
            )?;
            ensure!(
                report["kind"] == "full",
                "expected full pull diagnostics without previousResultId: {report}"
            );
            report["items"]
                .as_array()
                .context("LSP diagnostic report missing items")?
                .clone()
        };
        let mut output = Vec::new();
        for item in &items {
            if let Some(item) = normalize_diagnostic(path, source, item)? {
                output.push(item);
            }
        }
        Ok(output)
    }

    fn wait_published(&mut self) -> Result<Vec<Value>> {
        let deadline = Instant::now() + RESPONSE_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "official ty push diagnostics timed out for version {}",
                self.version
            );
            let message = self
                .messages
                .recv_timeout(remaining)
                .with_context(|| {
                    format!(
                        "waiting for official ty push diagnostics version {}; log: {}",
                        self.version,
                        self.stderr.display()
                    )
                })?
                .map_err(|error| anyhow!("official ty LSP transport: {error}"))?;
            if message["method"] == "textDocument/publishDiagnostics"
                && message["params"]["uri"] == self.document_uri
            {
                let version = message["params"]["version"]
                    .as_i64()
                    .context("official ty push diagnostics missing document version")?;
                ensure!(
                    version <= i64::from(self.version),
                    "received future-version diagnostics: {message}"
                );
                if version != i64::from(self.version) {
                    continue;
                }
                return message["params"]["diagnostics"]
                    .as_array()
                    .cloned()
                    .context("published diagnostics are not an array");
            }
            ensure!(
                message.get("method").is_some(),
                "unexpected response while awaiting push diagnostics: {message}"
            );
            self.handle_server_message(&message)?;
        }
    }

    fn send(&mut self, message: &Value) -> Result<()> {
        let body = serde_json::to_vec(message)?;
        write!(self.stdin, "Content-Length: {}\r\n\r\n", body.len())?;
        self.stdin.write_all(&body)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc":"2.0","method":method,"params":params}))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        for attempt in 0..3 {
            let id = self.next_id;
            self.next_id += 1;
            self.send(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))?;
            let deadline = Instant::now() + RESPONSE_TIMEOUT;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                ensure!(
                    !remaining.is_zero(),
                    "official ty LSP {method} timed out; log: {}",
                    self.stderr.display()
                );
                let message = self
                    .messages
                    .recv_timeout(remaining)
                    .with_context(|| {
                        format!(
                            "waiting for official ty LSP {method}; log: {}",
                            self.stderr.display()
                        )
                    })?
                    .map_err(|error| {
                        anyhow!(
                            "official ty LSP transport: {error}; log: {}",
                            self.stderr.display()
                        )
                    })?;
                if message.get("method").is_some() {
                    self.handle_server_message(&message)?;
                    continue;
                }
                ensure!(
                    message["id"].as_u64() == Some(id),
                    "unexpected LSP response id: {message}"
                );
                if let Some(error) = message.get("error") {
                    let cancelled = matches!(error["code"].as_i64(), Some(-32801 | -32802));
                    if method == "textDocument/diagnostic" && cancelled && attempt < 2 {
                        break;
                    }
                    return Err(ProtocolError {
                        code: error["code"].as_i64().unwrap_or(-32603),
                        message: error["message"]
                            .as_str()
                            .unwrap_or("official ty request failed")
                            .into(),
                        data: error.get("data").cloned(),
                    }
                    .into());
                }
                return message
                    .get("result")
                    .cloned()
                    .context("LSP response missing result");
            }
        }
        bail!("official ty LSP {method} repeatedly cancelled")
    }

    fn handle_server_message(&mut self, message: &Value) -> Result<()> {
        let method = message["method"]
            .as_str()
            .context("server method is not a string")?;
        if let Some(id) = message.get("id") {
            let result = match method {
                "workspace/configuration" => {
                    let count = message.pointer("/params/items").and_then(Value::as_array)
                        .context("workspace/configuration has no item array")?.len();
                    Value::Array(vec![self.workspace_configuration.clone().unwrap_or(Value::Null); count])
                }
                "client/registerCapability" | "client/unregisterCapability" |
                "workspace/diagnostic/refresh" | "workspace/semanticTokens/refresh" |
                "workspace/inlayHint/refresh" | "window/showMessageRequest" => Value::Null,
                "workspace/applyEdit" => json!({"applied":false,"failureReason":"analysis companion never edits source files"}),
                _ => return self.send(&json!({"jsonrpc":"2.0","id":id,
                    "error":{"code":-32601,"message":format!("unsupported client request: {method}")}})),
            };
            return self.send(&json!({"jsonrpc":"2.0","id":id,"result":result}));
        }
        if matches!(method, "window/showMessage" | "window/logMessage")
            && self.workspace_configuration.is_some()
        {
            self.notifications.push(message.clone());
            return Ok(());
        }
        if method == "window/showMessage"
            && message.pointer("/params/type").and_then(Value::as_i64) == Some(1)
        {
            bail!(
                "official ty server reported an error: {}",
                message["params"]["message"]
            );
        }
        if !self.project
            && method == "textDocument/publishDiagnostics"
            && message["params"]["uri"] != self.document_uri
        {
            let items = message["params"]["diagnostics"]
                .as_array()
                .context("invalid published diagnostics")?;
            ensure!(
                items.is_empty(),
                "official ty emitted diagnostics outside analysis document (possibly configuration errors): {message}"
            );
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let _ = self.request("shutdown", Value::Null);
        let _ = self.notify("exit", Value::Null);
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Err(_) => break,
                _ => thread::sleep(Duration::from_millis(2)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn read_frame(reader: &mut impl BufRead) -> Result<Value> {
    let mut length = None;
    let mut header_bytes = 0_usize;
    loop {
        let mut line = String::new();
        ensure!(reader.read_line(&mut line)? > 0, "server stdout closed");
        header_bytes += line.len();
        ensure!(header_bytes <= 8192, "oversized LSP headers");
        if line == "\r\n" || line == "\n" {
            break;
        }
        let (name, value) = line.split_once(':').context("malformed LSP header")?;
        if name.eq_ignore_ascii_case("content-length") {
            ensure!(length.is_none(), "duplicate Content-Length header");
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length")?,
            );
        }
    }
    let length = length.context("LSP message has no Content-Length")?;
    ensure!(length <= MAX_FRAME, "LSP frame exceeds {MAX_FRAME} bytes");
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes).context("invalid JSON-RPC frame")
}

pub(crate) fn file_uri(path: &Path) -> Result<String> {
    ensure!(
        path.is_absolute(),
        "file URI needs absolute path: {}",
        path.display()
    );
    encode_file_uri(path.to_str().context("non-UTF-8 file path")?, cfg!(windows))
}

fn encode_file_uri(text: &str, windows: bool) -> Result<String> {
    let normalized;
    let text = if windows {
        // canonicalize() returns verbatim Windows paths. Neither the prefix
        // nor backslashes are part of their corresponding file URI.
        let path = text.strip_prefix(r"\\?\").unwrap_or(text);
        normalized = if let Some(path) = path.strip_prefix(r"UNC\") {
            format!("//{}", path.replace('\\', "/"))
        } else {
            path.replace('\\', "/")
        };
        normalized.as_str()
    } else {
        text
    };
    let mut result = String::from("file://");
    let text = if windows && text.starts_with("//") {
        &text[2..]
    } else if windows && text.as_bytes().get(1) == Some(&b':') {
        result.push('/');
        text
    } else {
        ensure!(text.starts_with('/'), "file URI needs an absolute path");
        text
    };
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-._~:".contains(&byte) {
            result.push(byte as char);
        } else {
            use std::fmt::Write;
            write!(&mut result, "%{byte:02X}")?;
        }
    }
    Ok(result)
}

pub(crate) fn normalize_diagnostic(
    path: &Path,
    source: &str,
    item: &Value,
) -> Result<Option<Value>> {
    let Some(code) = item["code"].as_str() else {
        ensure!(
            item["code"].is_null()
                && item["severity"] == 4
                && item["tags"]
                    .as_array()
                    .is_some_and(|tags| tags.iter().any(|tag| *tag == 1)),
            "unexpected code-less/non-string diagnostic; refusing to drop it: {item}"
        );
        return Ok(None); // Official LSP-only unnecessary hint, absent from CLI.
    };
    let message = item["message"]
        .as_str()
        .context("LSP diagnostic message is not text")?;
    // ty_server appends locationless sub-diagnostics after a blank line.
    // GitLab reports only concise_message(), so keep those explanatory notes
    // separate from the primary diagnostic used by this transport-neutral API.
    let primary_message = message
        .split_once("\n\n")
        .filter(|(_, notes)| {
            ["info: ", "help: ", "warning: ", "error: ", "note: "]
                .iter()
                .any(|prefix| notes.starts_with(prefix))
        })
        .map_or(message, |(primary, _)| primary);
    let severity = match item["severity"].as_u64() {
        Some(1) => "major",
        Some(2) => "minor",
        Some(3) => "info",
        _ => bail!("unexpected coded LSP diagnostic severity: {item}"),
    };
    let position = |value: &Value| -> Result<Value> {
        let line = value["line"]
            .as_u64()
            .context("LSP diagnostic missing line")?;
        let character = value["character"]
            .as_u64()
            .context("LSP diagnostic missing character")?;
        // GitLab excludes a first-line BOM; LSP UTF-32 counts it.
        let character = if line == 0 && source.starts_with('\u{feff}') {
            character.saturating_sub(1)
        } else {
            character
        };
        Ok(json!({"line":line + 1,"column":character + 1}))
    };
    Ok(Some(json!({
        "check_name":code,"description":format!("{code}: {primary_message}"),"severity":severity,
        "lsp_message":message,
        "lsp_diagnostic":item,
        "location":{"path":path,"positions":{
            "begin":position(&item["range"]["start"])? ,"end":position(&item["range"]["end"])?
        }}
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn file_uri_handles_windows_drives_and_unc_paths() -> Result<()> {
        assert_eq!(
            encode_file_uri(r"C:\Users\Name Here\雪.py", true)?,
            "file:///C:/Users/Name%20Here/%E9%9B%AA.py"
        );
        assert_eq!(
            encode_file_uri(r"\\?\C:\Users\100%.py", true)?,
            "file:///C:/Users/100%25.py"
        );
        assert_eq!(
            encode_file_uri(r"\\?\UNC\server\share\file.py", true)?,
            "file://server/share/file.py"
        );
        assert_eq!(
            encode_file_uri(r"\\server\share\file.py", true)?,
            "file://server/share/file.py"
        );
        Ok(())
    }

    #[test]
    fn framing_accepts_utf8_and_rejects_missing_lengths() -> Result<()> {
        let payload = r#"{"jsonrpc":"2.0","result":"雪"}"#;
        let bytes = format!(
            "Content-Length: {}\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n{payload}",
            payload.len()
        );
        assert_eq!(read_frame(&mut Cursor::new(bytes))?["result"], "雪");
        assert!(read_frame(&mut Cursor::new("Content-Type: x\r\n\r\n{}")).is_err());
        assert!(
            read_frame(&mut Cursor::new(
                "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}"
            ))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn uri_encoder_handles_spaces_and_unicode() -> Result<()> {
        assert_eq!(
            encode_file_uri("/tmp/a b/雪.py", false)?,
            "file:///tmp/a%20b/%E9%9B%AA.py"
        );
        Ok(())
    }

    #[test]
    fn normalization_preserves_reveal_and_only_omits_ide_hints() -> Result<()> {
        let reveal = json!({"code":"revealed-type","severity":3,"message":"Revealed type: `int`",
            "range":{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}});
        let normalized =
            normalize_diagnostic(Path::new("/tmp/a.py"), "\u{feff}abcx", &reveal)?.unwrap();
        assert_eq!(
            normalized["description"],
            "revealed-type: Revealed type: `int`"
        );
        assert_eq!(
            normalized["location"]["positions"]["begin"],
            json!({"line":1,"column":4})
        );
        assert!(
            normalize_diagnostic(
                Path::new("/tmp/a.py"),
                "",
                &json!({"severity":4,"tags":[1]})
            )?
            .is_none()
        );
        assert!(
            normalize_diagnostic(
                Path::new("/tmp/a.py"),
                "",
                &json!({"severity":1,"message":"bad"})
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn normalization_separates_lsp_notes_without_truncating_primary_messages() -> Result<()> {
        let mut diagnostic = json!({"code":"invalid-argument-type","severity":1,
            "message":"Expected `Iterable[int]`, found `list[str]`\n\ninfo: incompatible protocol member\ninfo: incompatible return type",
            "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}});
        let normalized = normalize_diagnostic(Path::new("/tmp/a.py"), "x", &diagnostic)?.unwrap();
        assert_eq!(
            normalized["description"],
            "invalid-argument-type: Expected `Iterable[int]`, found `list[str]`"
        );
        assert_eq!(normalized["lsp_message"], diagnostic["message"]);
        diagnostic["message"] = json!("primary\n\ncontinuation");
        let normalized = normalize_diagnostic(Path::new("/tmp/a.py"), "x", &diagnostic)?.unwrap();
        assert_eq!(
            normalized["description"],
            "invalid-argument-type: primary\n\ncontinuation"
        );
        Ok(())
    }

    #[test]
    fn actual_official_push_reuses_document_and_matches_cli() -> Result<()> {
        actual_repeated_edits(true)
    }

    #[test]
    fn actual_official_lsp_reuses_document_and_matches_cli() -> Result<()> {
        actual_repeated_edits(false)
    }

    fn actual_repeated_edits(push: bool) -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")
            .context("TY_COMPANION_TEST_OFFICIAL must name the official ty executable; this integration test must not silently skip")?;
        let out = std::env::temp_dir().join(format!(
            "ty-native-lsp-test-{}-{}",
            std::process::id(),
            if push { "push" } else { "pull" }
        ));
        fs::create_dir_all(&out)?;
        let mut server = if push {
            Checker::new_push(Path::new(&binary), &out)?
        } else {
            Checker::new(Path::new(&binary), &out, true)?
        };
        let mut cli = Checker::new(Path::new(&binary), &out, false)?;
        let path = out.join("generated.py");
        let sources = [
            "from typing import reveal_type\né = 1; value: str = 42\nreveal_type(value)\n",
            "from typing import reveal_type\né = 1; value: int = 42\nreveal_type(value)\n",
            "from typing import reveal_type\né = 1; value: str = 42\nreveal_type(value)\n",
            "from typing import Iterable\ndef accept(values: Iterable[int]): ...\naccept(['bad'])\n",
        ];
        let mut first = None;
        for source in sources {
            let actual = server.check(&path, source)?;
            let expected = cli.check(&path, source)?;
            let signature = |values: &[Value]| -> Vec<String> {
                let mut result:Vec<_>=values.iter().map(|value| json!({"code":value["check_name"],
                    "description":value["description"],"severity":value["severity"],"positions":value["location"]["positions"]}).to_string()).collect();
                result.sort();
                result
            };
            assert_eq!(signature(&actual), signature(&expected));
            if let Some(first) = &first {
                if source == sources[0] {
                    assert_eq!(first, &actual);
                }
            } else {
                first = Some(actual);
            }
        }
        assert_eq!(server.queries, 4);
        Ok(())
    }

    #[test]
    fn actual_project_server_observes_cross_module_edits_before_diagnostics() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")
            .context("TY_COMPANION_TEST_OFFICIAL must name official ty")?;
        let root =
            std::env::temp_dir().join(format!("ty-native-project-test-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let config = root.join("ty.toml");
        fs::write(&config, "[environment]\npython-version = \"3.13\"\n")?;
        let peer = root.join("peer.py");
        let consumer = root.join("consumer.py");
        let mut server = ProjectChecker::new(Path::new(&binary), &root, &config)?;
        for (returns, value, expected) in [("int", "1", 1), ("str", "'ok'", 0), ("int", "1", 1)] {
            let sources = BTreeMap::from([
                (
                    peer.clone(),
                    format!("def value() -> {returns}: return {value}\n"),
                ),
                (
                    consumer.clone(),
                    "from peer import value\nanswer: str = value()\n".into(),
                ),
            ]);
            for (path, source) in &sources {
                fs::write(path, source)?;
            }
            let actual = server.check(&sources, std::slice::from_ref(&consumer))?;
            assert_eq!(
                actual.len(),
                expected,
                "cross-module change did not reach the checker: {actual:?}"
            );
            let cli = Command::new(&binary)
                .args(["check", "--config-file"])
                .arg(&config)
                .args(["--output-format", "gitlab"])
                .arg(&consumer)
                .current_dir(&root)
                .env("TY_UV", "0")
                .output()?;
            ensure!(
                matches!(cli.status.code(), Some(0 | 1)),
                "official CLI failed"
            );
            let raw: Vec<Value> = serde_json::from_slice(&cli.stdout)?;
            let signature = |values: &[Value]| {
                values.iter().map(|v|json!({"code":v["check_name"],"description":v["description"],"positions":v["location"]["positions"]})).collect::<Vec<_>>()
            };
            assert_eq!(signature(&actual), signature(&raw));
        }
        Ok(())
    }

    #[test]
    fn actual_project_server_checks_sibling_stubs_and_source_together() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")
            .context("TY_COMPANION_TEST_OFFICIAL must name official ty")?;
        let root =
            std::env::temp_dir().join(format!("ty-native-sibling-stubs-{}", std::process::id()));
        fs::create_dir_all(&root)?;
        let root = root.canonicalize()?;
        let project = root.join("project");
        let stubs = root.join("stubs");
        fs::create_dir_all(&project)?;
        fs::create_dir_all(&stubs)?;
        let config = root.join("checker.toml");
        fs::write(
            &config,
            format!(
                "[environment]\npython-version = \"3.10\"\nextra-paths = [{}, {}]\n",
                serde_json::to_string(&stubs)?,
                serde_json::to_string(&project)?
            ),
        )?;
        let stub = stubs.join("support.pyi");
        let consumer = project.join("consumer.py");
        let paths = vec![stub.clone(), consumer.clone()];
        // Use their common generated workspace. extra-paths alone makes the
        // stub importable but does not include it in push diagnostics.
        let mut server = ProjectChecker::new(Path::new(&binary), &root, &config)?;
        for value_type in ["int", "str", "int"] {
            let sources = BTreeMap::from([
                (stub.clone(), format!("from typing_extensions import reveal_type\nreveal_type(None)\nvalue: {value_type}\n")),
                (consumer.clone(), "from typing_extensions import reveal_type\nfrom support import value\nreveal_type(value)\nanswer: int = value\n".into()),
            ]);
            for (path, text) in &sources {
                fs::write(path, text)?;
            }
            let actual = server.check(&sources, &paths)?;
            let cli = Command::new(&binary)
                .args(["check", "--output-format", "gitlab", "--config-file"])
                .arg(&config)
                .args(&paths)
                .current_dir(&project)
                .env("TY_UV", "0")
                .output()?;
            ensure!(
                matches!(cli.status.code(), Some(0 | 1)),
                "official CLI failed"
            );
            let expected: Vec<Value> = serde_json::from_slice(&cli.stdout)?;
            let signature = |values: &[Value]| {
                let mut result: Vec<_> = values
                    .iter()
                    .map(|value| {
                        let mut location = value["location"].clone();
                        let path = PathBuf::from(location["path"].as_str().unwrap());
                        location["path"] = json!(if path.is_absolute() {
                            path
                        } else {
                            project.join(path)
                        });
                        json!({
                            "code":value["check_name"], "description":value["description"],
                            "location":location, "severity":value["severity"],
                        })
                        .to_string()
                    })
                    .collect();
                result.sort();
                result
            };
            assert_eq!(signature(&actual), signature(&expected));
            assert_eq!(
                actual
                    .iter()
                    .filter(|item| item["check_name"] == "revealed-type")
                    .count(),
                2,
                "source and stub must both return probes: {actual:?}"
            );
        }
        drop(server);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
