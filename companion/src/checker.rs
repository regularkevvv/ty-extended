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
    time::{Duration, Instant, SystemTime},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME: usize = 64 * 1024 * 1024;

/// Tool installers expose the companion command without adding dependency
/// executables to the user's PATH. Prefer official ty beside the real installed
/// executable; source builds continue to use PATH when no sibling exists.
pub(crate) fn adjacent_official_checker() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?.canonicalize().ok()?;
    let candidate = executable
        .parent()?
        .join(if cfg!(windows) { "ty.exe" } else { "ty" });
    let candidate = candidate.canonicalize().ok()?;
    (candidate.is_file() && candidate != executable).then_some(candidate)
}

/// Match the native resolver's canonical path spelling at the CLI boundary.
/// On Windows, explicit verbatim paths and simplified import roots otherwise
/// become distinct File identities in ty. Keep paths that require verbatim
/// semantics unchanged, as the native resolver does with dunce::simplified.
pub(crate) fn native_cli_path(path: &Path) -> &Path {
    #[cfg(windows)]
    {
        dunce::simplified(path)
    }
    #[cfg(not(windows))]
    {
        path
    }
}

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
                .arg(native_cli_path(path))
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
    disk_sources: BTreeMap<PathBuf, ([u8; 32], SystemTime)>,
    retired_disk_revisions: BTreeMap<PathBuf, SystemTime>,
    disk_directories: BTreeMap<PathBuf, SystemTime>,
    projection_root: PathBuf,
    document_version: i32,
}

/// A watched-file event makes ty reread metadata, but equal modification times
/// retain its cached source or directory listing. Give our generated files a
/// distinct revision even on coarse filesystems and rapid delete/recreate cycles.
/// This must never be called for original sources or memory-only overlays.
fn advance_disk_revision(path: &Path, previous: Option<SystemTime>) -> Result<SystemTime> {
    let modified = path.metadata()?.modified()?;
    if previous.is_some_and(|previous| previous != modified) {
        return Ok(modified);
    }
    // Two seconds also works on filesystems with coarse timestamp precision.
    let next = modified
        .checked_add(Duration::from_secs(2))
        .context("projection timestamp overflow")?;
    set_disk_modified(path, next)?;
    let revised = path.metadata()?.modified()?;
    ensure!(
        revised != modified,
        "cannot revise projection {}",
        path.display()
    );
    Ok(revised)
}

fn set_disk_modified(path: &Path, modified: SystemTime) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_WRITE_ATTRIBUTES permits timestamp changes without writing file
        // contents; FILE_FLAG_BACKUP_SEMANTICS also permits directory handles.
        options.access_mode(0x100).custom_flags(0x0200_0000);
    }
    options.open(path)?.set_modified(modified)?;
    Ok(())
}

impl ProjectChecker {
    /// Start a checker after every ordinary projection source has been written.
    /// Its initial database already sees those bytes, so no watched-file events
    /// or timestamp changes are needed. Record revisions for subsequent edits.
    /// Notebook and external overlays use the general constructors instead.
    pub fn new_with_disk_projection(
        binary: &Path,
        root: &Path,
        config: &Path,
        sources: &BTreeMap<PathBuf, String>,
    ) -> Result<Self> {
        use sha2::{Digest, Sha256};
        let mut checker = Self::new(binary, root, config)?;
        let mut directories = BTreeSet::new();
        for (path, source) in sources {
            ensure!(
                path.starts_with(root),
                "disk projection outside generated workspace: {}",
                path.display()
            );
            checker.disk_sources.insert(
                path.clone(),
                (
                    Sha256::digest(source.as_bytes()).into(),
                    path.metadata()?.modified()?,
                ),
            );
            directories.extend(
                path.ancestors()
                    .skip(1)
                    .take_while(|parent| parent.starts_with(root))
                    .map(Path::to_owned),
            );
        }
        for path in directories {
            checker
                .disk_directories
                .insert(path.clone(), path.metadata()?.modified()?);
        }
        Ok(checker)
    }

    pub fn new(binary: &Path, root: &Path, config: &Path) -> Result<Self> {
        let mut server = Server::configured(binary, root, config, true)?;
        server.project = true;
        Ok(Self {
            server,
            documents: BTreeMap::new(),
            notebooks: BTreeMap::new(),
            external_sources: BTreeSet::new(),
            disk_sources: BTreeMap::new(),
            retired_disk_revisions: BTreeMap::new(),
            disk_directories: BTreeMap::new(),
            projection_root: root.to_owned(),
            document_version: 0,
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
            disk_sources: BTreeMap::new(),
            retired_disk_revisions: BTreeMap::new(),
            disk_directories: BTreeMap::new(),
            projection_root: root.to_owned(),
            document_version: 0,
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
        self.sync_documents_deferred(sources, &BTreeSet::new())
    }
    fn sync_documents_deferred(
        &mut self,
        sources: &BTreeMap<PathBuf, String>,
        deferred: &BTreeSet<PathBuf>,
    ) -> Result<()> {
        let removed: Vec<_> = self
            .documents
            .iter()
            .filter(|(path, (_, _, previous))| {
                !sources.contains_key(*path)
                    || deferred.contains(*path) && sources.get(*path) != Some(previous)
            })
            .map(|(path, _)| path.clone())
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
            if !deferred.contains(path) {
                self.update(path, source, false)?;
            }
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
        if !force
            && self
                .documents
                .get(path)
                .is_some_and(|(_, _, previous)| previous == source)
        {
            return Ok(());
        }
        // Versions stay monotonic even when an oracle closes and later reopens
        // a document. A delayed publication from its old lifetime cannot satisfy
        // the new query or appear to come from a future version.
        self.document_version = self
            .document_version
            .checked_add(1)
            .context("LSP document version overflow")?;
        if let Some((uri, version, previous)) = self.documents.get_mut(path) {
            *version = self.document_version;
            *previous = source.to_owned();
            if let Some(notebook) = self.notebooks.get(path) {
                self.server.notify("notebookDocument/didChange",json!({"notebookDocument":{"uri":notebook,"version":version},"change":{"cells":{"textContent":[{"document":{"uri":uri,"version":version},"changes":[{"text":source}]}]}}}))?;
            } else {
                self.server.notify("textDocument/didChange",json!({"textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":source}]}))?;
            }
        } else {
            let uri = file_uri(path)?;
            let version = self.document_version;
            if let Some(notebook) = self.notebooks.get(path) {
                self.server.notify("notebookDocument/didOpen",json!({"notebookDocument":{"uri":notebook,"notebookType":"jupyter-notebook","version":version,"cells":[{"kind":2,"document":uri}]},"cellTextDocuments":[{"uri":uri,"languageId":"python","version":version,"text":source}]}))?;
            } else {
                self.server.notify(
                "textDocument/didOpen",
                json!({"textDocument":{"uri":uri,"languageId":"python","version":version,"text":source}}),
            )?;
            }
            self.documents
                .insert(path.to_owned(), (uri, version, source.to_owned()));
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn check(
        &mut self,
        sources: &BTreeMap<PathBuf, String>,
        paths: &[PathBuf],
    ) -> Result<Vec<Value>> {
        self.sync_documents(sources)?;
        self.check_synced(sources, paths)
    }
    fn check_synced(
        &mut self,
        sources: &BTreeMap<PathBuf, String>,
        paths: &[PathBuf],
    ) -> Result<Vec<Value>> {
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
                    let Some(received) = message["params"]["version"].as_i64() else {
                        // Closing a document can clear its diagnostics without
                        // a version. Only the forced current version is evidence
                        // for this batch, including after a close/reopen cycle.
                        continue;
                    };
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

    /// Check a projection that the caller has already written to disk. Closed
    /// modules participate through ordinary imports and watcher invalidation;
    /// opening them all would eagerly check the entire project at every oracle
    /// query. Notebook and external buffers must retain their memory overlays.
    pub fn check_files(
        &mut self,
        sources: &BTreeMap<PathBuf, String>,
        paths: &[PathBuf],
    ) -> Result<Vec<Value>> {
        use sha2::{Digest, Sha256};
        let queried: BTreeSet<_> = paths.iter().collect();
        let mut open = BTreeMap::new();
        let mut current = BTreeMap::new();
        let mut changes = Vec::new();
        let mut structural_changes = BTreeSet::new();
        for (path, source) in sources {
            let overlay = self.notebooks.contains_key(path) || self.external_sources.contains(path);
            if queried.contains(path) || overlay {
                open.insert(path.clone(), source.clone());
            }
            if !overlay {
                let fingerprint: [u8; 32] = Sha256::digest(source.as_bytes()).into();
                let previous = self.disk_sources.get(path);
                let retired_revision = if previous.is_none() {
                    self.retired_disk_revisions.remove(path)
                } else {
                    None
                };
                let revision = if let Some((old, revision)) = previous
                    && *old == fingerprint
                {
                    *revision
                } else {
                    ensure!(
                        path.starts_with(&self.projection_root),
                        "disk projection outside generated workspace: {}",
                        path.display()
                    );
                    let revision = if previous.is_none()
                        && retired_revision.is_none()
                        && self.documents.contains_key(path)
                    {
                        // The initial projection can replace a discovery-stage
                        // memory overlay. Closing it below changes ty's source
                        // revision independently of disk timestamps; a retained
                        // query buffer receives a forced didChange instead.
                        // Avoid touching thousands of already synchronized files.
                        path.metadata()?.modified()?
                    } else {
                        advance_disk_revision(
                            path,
                            previous.map(|(_, revision)| *revision).or(retired_revision),
                        )?
                    };
                    changes.push(
                        json!({"uri":file_uri(path)?,"type":if previous.is_some() {2} else {1}}),
                    );
                    if previous.is_none() {
                        structural_changes.insert(path.clone());
                    }
                    revision
                };
                current.insert(path.clone(), (fingerprint, revision));
            }
        }
        for path in self
            .disk_sources
            .keys()
            .filter(|path| !current.contains_key(*path))
        {
            let exists = path.exists();
            if exists {
                // Removing a projected contribution can restore underlying
                // bytes in the same physical file. Remember its revision even
                // while it is absent from this map: a later write may reuse a
                // timestamp from before our explicit revision bump.
                let revision = advance_disk_revision(path, Some(self.disk_sources[path].1))?;
                self.retired_disk_revisions.insert(path.clone(), revision);
            } else {
                self.retired_disk_revisions.remove(path);
            }
            changes.push(json!({"uri":file_uri(path)?,"type":if exists {2} else {3}}));
            // A preflight can leave the queried source map while remaining on
            // disk. Its directory inventory has not changed.
            if !exists {
                structural_changes.insert(path.clone());
            }
        }
        let mut directories = BTreeSet::new();
        for path in &structural_changes {
            directories.extend(
                path.ancestors()
                    .skip(1)
                    .take_while(|parent| parent.starts_with(&self.projection_root))
                    .map(Path::to_owned),
            );
        }
        for path in directories {
            if path.is_dir() {
                let previous = self.disk_directories.get(&path).copied();
                let revision = advance_disk_revision(&path, previous)?;
                // A new package can invalidate a missing ancestor lookup too.
                // Initial file events already cover the initial workspace.
                if previous.is_none() && !self.disk_sources.is_empty() {
                    changes.push(json!({"uri":file_uri(&path)?,"type":1}));
                }
                self.disk_directories.insert(path, revision);
            } else {
                self.disk_directories.remove(&path);
            }
        }
        // All ordinary sources already contain their final bytes on disk.
        // Close changed query buffers before invalidating imports, then open
        // each exactly once when requesting its diagnostics. Otherwise didOpen
        // immediately checks it and the forced didChange repeats that work.
        // Memory-only overlays still synchronize before any query is sent.
        let deferred = paths
            .iter()
            .filter(|path| {
                !self.notebooks.contains_key(*path) && !self.external_sources.contains(*path)
            })
            .cloned()
            .collect();
        self.sync_documents_deferred(&open, &deferred)?;
        if !changes.is_empty() {
            self.server.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes":changes}),
            )?;
        }
        self.disk_sources = current;
        self.check_synced(&open, paths)
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
    #[cfg(windows)]
    fn native_cli_paths_match_resolver_without_changing_verbatim_only_names() {
        assert_eq!(
            native_cli_path(Path::new(r"\\?\C:\project\demo.py")),
            Path::new(r"C:\project\demo.py")
        );
        for path in [
            r"C:\project\demo.py",
            r"\\?\C:\project\NUL.py",
            r"\\?\C:\project\trailing.\demo.py",
            r"\\?\UNC\server\share\demo.py",
        ] {
            assert_eq!(native_cli_path(Path::new(path)), Path::new(path));
        }
    }

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
    fn disk_projection_invalidates_closed_imports_and_inventory() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        for initialized in [false, true] {
            disk_projection_case(&binary, initialized)?;
        }
        Ok(())
    }

    fn disk_projection_case(binary: &str, initialized: bool) -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "ty-disk-projection-{}-{initialized}",
            std::process::id()
        ));
        fs::create_dir_all(&root)?;
        let config = root.join("ty.toml");
        fs::write(&config, "[environment]\npython-version = '3.13'\n")?;
        let peer = root.join("peer.py");
        let consumer = root.join("consumer.py");
        let mut sources = BTreeMap::from([
            (
                consumer.clone(),
                "from peer import value\nanswer: str = value\n".to_owned(),
            ),
            (peer.clone(), "value: int\n".to_owned()),
        ]);
        for (path, source) in &sources {
            fs::write(path, source)?;
        }
        let initial_revision = peer.metadata()?.modified()?;
        let mut server = if initialized {
            ProjectChecker::new_with_disk_projection(Path::new(binary), &root, &config, &sources)?
        } else {
            ProjectChecker::new(Path::new(binary), &root, &config)?
        };
        let initial = server.check_files(&sources, std::slice::from_ref(&consumer))?;
        assert_eq!(initial.len(), 1, "{initial:#?}");
        if initialized {
            assert_eq!(peer.metadata()?.modified()?, initial_revision);
        }
        for (annotation, errors) in [("int", 1), ("str", 0), ("int", 1), ("str", 0)] {
            sources.insert(peer.clone(), format!("value: {annotation}\n"));
            for (path, source) in &sources {
                let revision = path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .ok();
                fs::write(path, source)?;
                if let Some(revision) = revision {
                    set_disk_modified(path, revision)?;
                }
            }
            let actual = server.check_files(&sources, std::slice::from_ref(&consumer))?;
            assert_eq!(actual.len(), errors, "{actual:#?}");
            assert_eq!(server.documents.len(), 1, "imported files must stay closed");
            assert!(!server.documents.contains_key(&peer));
        }
        server.check_files(&sources, &[peer.clone(), consumer.clone()])?;
        // Both query buffers change. The alphabetically first consumer must see
        // the new dependency even before that dependency is reopened in LSP.
        for (revision, annotation, errors) in [(1, "int", 1), (2, "str", 0), (3, "int", 1)] {
            sources.insert(peer.clone(), format!("value: {annotation}\n"));
            sources.insert(
                consumer.clone(),
                format!("# revision {revision}\nfrom peer import value\nanswer: str = value\n"),
            );
            for (path, source) in &sources {
                fs::write(path, source)?;
            }
            let actual = server.check_files(&sources, &[consumer.clone(), peer.clone()])?;
            assert_eq!(actual.len(), errors, "{actual:#?}");
        }
        sources.remove(&peer);
        fs::remove_file(&peer)?;
        let missing = server.check_files(&sources, std::slice::from_ref(&consumer))?;
        assert!(
            missing.iter().any(|diagnostic| matches!(
                diagnostic["check_name"].as_str(),
                Some("unresolved-import" | "import-not-found")
            )),
            "{missing:#?}"
        );
        assert!(!server.documents.contains_key(&peer));
        let removed_revision = root.metadata()?.modified()?;
        sources.insert(peer.clone(), "value: str\n".into());
        fs::write(&peer, &sources[&peer])?;
        set_disk_modified(&root, removed_revision)?;
        assert_eq!(root.metadata()?.modified()?, removed_revision);
        let restored = server.check_files(&sources, std::slice::from_ref(&consumer))?;
        assert!(restored.is_empty(), "{restored:#?}");
        let process = server.process_id();
        let file_revision = peer.metadata()?.modified()?;
        let directory_revision = root.metadata()?.modified()?;
        assert!(
            server
                .check_files(&sources, std::slice::from_ref(&consumer))?
                .is_empty()
        );
        assert_eq!(peer.metadata()?.modified()?, file_revision);
        assert_eq!(root.metadata()?.modified()?, directory_revision);
        // Dropping a projection can restore an underlying file on disk. The
        // physical file remains importable even though it leaves this map.
        let projected_revision = peer.metadata()?.modified()?;
        fs::write(&peer, "value: int\n")?;
        set_disk_modified(&peer, projected_revision)?;
        sources.remove(&peer);
        let underlying = server.check_files(&sources, std::slice::from_ref(&consumer))?;
        assert_eq!(underlying.len(), 1, "{underlying:#?}");
        assert_eq!(underlying[0]["check_name"], "invalid-assignment");
        assert_eq!(root.metadata()?.modified()?, directory_revision);
        let underlying_revision = peer.metadata()?.modified()?;
        sources.insert(peer.clone(), "value: str\n".into());
        fs::write(&peer, &sources[&peer])?;
        // A coarse write can reuse a timestamp from before the explicit bump.
        // Re-entering the map must compare against the last observed revision.
        set_disk_modified(&peer, underlying_revision - Duration::from_secs(2))?;
        let reentered = server.check_files(&sources, std::slice::from_ref(&consumer))?;
        assert!(reentered.is_empty(), "{reentered:#?}");

        sources.insert(
            consumer.clone(),
            "from package.nested.peer import value\nanswer: str = value\n".into(),
        );
        fs::write(&consumer, &sources[&consumer])?;
        let nested = root.join("package/nested/peer.py");
        for _ in 0..3 {
            let missing = server.check_files(&sources, std::slice::from_ref(&consumer))?;
            assert!(
                missing.iter().any(|item| matches!(
                    item["check_name"].as_str(),
                    Some("unresolved-import" | "import-not-found")
                )),
                "{missing:#?}"
            );
            let removed_revision = root.metadata()?.modified()?;
            fs::create_dir_all(nested.parent().unwrap())?;
            sources.insert(nested.clone(), "value: str\n".into());
            fs::write(&nested, &sources[&nested])?;
            set_disk_modified(&root, removed_revision)?;
            let restored = server.check_files(&sources, std::slice::from_ref(&consumer))?;
            assert!(restored.is_empty(), "{restored:#?}");
            sources.remove(&nested);
            fs::remove_dir_all(root.join("package"))?;
        }
        assert_eq!(
            server.process_id(),
            process,
            "revisions retain the checker process"
        );
        drop(server);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn disk_revisions_preserve_external_sources() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let base =
            std::env::temp_dir().join(format!("ty-projection-boundary-{}", std::process::id()));
        let root = base.join("projection");
        fs::create_dir_all(&root)?;
        let config = root.join("ty.toml");
        fs::write(&config, "[environment]\npython-version = '3.13'\n")?;
        let original = base.join("original.py");
        fs::write(&original, "value: int = 1\n")?;
        let revision = original.metadata()?.modified()?;
        let sources = BTreeMap::from([(original.clone(), "value: str = 1\n".into())]);
        let mut server = ProjectChecker::new(Path::new(&binary), &root, &config)?;
        assert!(
            server
                .check_files(&sources, std::slice::from_ref(&original))
                .unwrap_err()
                .to_string()
                .contains("outside generated workspace")
        );
        server.register_external_source(&original)?;
        let diagnostics = server.check_files(&sources, std::slice::from_ref(&original))?;
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(original.metadata()?.modified()?, revision);
        assert_eq!(fs::read_to_string(&original)?, "value: int = 1\n");
        drop(server);
        fs::remove_dir_all(base)?;
        Ok(())
    }

    #[test]
    fn initial_disk_projection_invalidates_open_and_closed_imports() -> Result<()> {
        let binary = std::env::var("TY_COMPANION_TEST_OFFICIAL")?;
        let base =
            std::env::temp_dir().join(format!("ty-initial-projection-{}", std::process::id()));
        for open_peer in [false, true] {
            let root = base.join(if open_peer { "open" } else { "closed" });
            fs::create_dir_all(&root)?;
            let config = root.join("ty.toml");
            fs::write(&config, "[environment]\npython-version = '3.13'\n")?;
            let peer = root.join("peer.py");
            let consumer = root.join("consumer.py");
            fs::write(&peer, "value: int\n")?;
            let revision = peer.metadata()?.modified()?;
            let mut sources = BTreeMap::from([(
                consumer.clone(),
                "from peer import value\nanswer: str = value\n".into(),
            )]);
            fs::write(&consumer, &sources[&consumer])?;
            if open_peer {
                sources.insert(peer.clone(), "value: int\n".into());
            }
            let mut server = ProjectChecker::new(Path::new(&binary), &root, &config)?;
            let old = server.check(&sources, std::slice::from_ref(&consumer))?;
            assert_eq!(old.len(), 1, "{old:#?}");
            sources.insert(peer.clone(), "value: str\n".into());
            fs::write(&peer, &sources[&peer])?;
            set_disk_modified(&peer, revision)?;
            let current = server.check_files(&sources, std::slice::from_ref(&consumer))?;
            assert!(current.is_empty(), "open_peer={open_peer}: {current:#?}");
            assert!(!server.documents.contains_key(&peer));
            if open_peer {
                assert_eq!(peer.metadata()?.modified()?, revision);
            }
        }
        fs::remove_dir_all(base)?;
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
