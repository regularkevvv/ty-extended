//! Bidirectional, encoding-aware editor source maps.
//!
//! Read-only results may use an enclosing source origin. Writes require exact
//! copied bytes, and a workspace edit is validated atomically before returning.
//! Each LSP shape is handled explicitly; opaque `data` and command arguments
//! are never traversed as if they contained user-visible source locations.
use std::{
    collections::{BTreeMap, BTreeSet},
    hash::{Hash, Hasher},
};

use anyhow::{Context, Result, bail, ensure};
use ruff_text_size::Ranged;
use serde_json::{Value, json};

use crate::{mapping::Rendered, model::Span};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PositionEncoding {
    Utf8,
    #[default]
    Utf16,
    Utf32,
}
impl PositionEncoding {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "utf-8" => Some(Self::Utf8),
            "utf-16" => Some(Self::Utf16),
            "utf-32" => Some(Self::Utf32),
            _ => None,
        }
    }
    pub fn as_name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }
    fn width(self, ch: char) -> usize {
        match self {
            Self::Utf8 => ch.len_utf8(),
            Self::Utf16 => ch.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

/// Reusable line index. CRLF counts as one newline, and its CR is not an LSP
/// character. A BOM *is* a character in the editor protocol (unlike CLI output).
#[derive(Clone)]
struct Coordinates {
    lines: Vec<usize>,
}
impl Coordinates {
    fn new(text: &str) -> Self {
        let mut lines = vec![0];
        for (i, byte) in text.bytes().enumerate() {
            if byte == b'\n' || (byte == b'\r' && text.as_bytes().get(i + 1) != Some(&b'\n')) {
                lines.push(i + 1);
            }
        }
        Self { lines }
    }
    fn offset(&self, text: &str, position: &Value, encoding: PositionEncoding) -> Result<usize> {
        let line = index(position, "line")?;
        let character = index(position, "character")?;
        let start = *self
            .lines
            .get(line)
            .context("LSP line is outside the document")?;
        let end = self.lines.get(line + 1).copied().unwrap_or(text.len());
        let contents = text[start..end].trim_end_matches(['\r', '\n']);
        let mut count = 0;
        for (offset, ch) in contents.char_indices() {
            if count == character {
                return Ok(start + offset);
            }
            count += encoding.width(ch);
            ensure!(
                count <= character,
                "LSP character splits a Unicode code point"
            );
        }
        // LSP specifies clamping columns beyond the end of a line.
        Ok(start + contents.len())
    }
    fn position(&self, text: &str, offset: usize, encoding: PositionEncoding) -> Result<Value> {
        ensure!(
            offset <= text.len() && text.is_char_boundary(offset),
            "invalid source byte offset {offset}"
        );
        let line = self
            .lines
            .partition_point(|start| *start <= offset)
            .saturating_sub(1);
        let prefix = text[self.lines[line]..offset].trim_end_matches(['\r', '\n']);
        Ok(
            json!({"line":line,"character":prefix.chars().map(|ch|encoding.width(ch)).sum::<usize>()}),
        )
    }
}
fn index(value: &Value, name: &str) -> Result<usize> {
    usize::try_from(
        value[name]
            .as_u64()
            .with_context(|| format!("missing LSP {name}"))?,
    )
    .context("LSP integer exceeds platform address space")
}
fn diagnostic_key(value: &Value) -> String {
    // Editors may strip `data`, tags, source or related information from the
    // diagnostic context; these fields still identify its displayed issue.
    json!([value["range"], value["code"], value["message"]]).to_string()
}
fn source_names(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let _ = crate::project::rewrite_names(source, |name| {
        names.extend(name.split('.').map(str::to_owned));
        name.into()
    });
    names
}
pub fn position_to_offset(
    text: &str,
    position: &Value,
    encoding: PositionEncoding,
) -> Result<usize> {
    Coordinates::new(text).offset(text, position, encoding)
}
pub fn offset_to_position(text: &str, offset: usize, encoding: PositionEncoding) -> Result<Value> {
    Coordinates::new(text).position(text, offset, encoding)
}

/// Preserve upstream diagnostic notes for terminal output while mapping every
/// related location through the complete project. CLI primary descriptions may
/// have extra plugin-specific normalization, so keep their existing wording.
pub fn merge_cli_diagnostic_details(
    diagnostics: &mut [Value],
    rich: &[Value],
    documents: &[crate::project_check::ProjectDocument],
    names: BTreeMap<String, String>,
) -> Result<()> {
    let generated: BTreeMap<_, _> = documents
        .iter()
        .map(|document| {
            (
                document.source.path.as_str(),
                document.path.to_string_lossy().into_owned(),
            )
        })
        .collect();
    let mut by_location = BTreeMap::<(String, String, String), Vec<&Value>>::new();
    for diagnostic in rich {
        let Some(path) = diagnostic["location"]["path"].as_str() else {
            continue;
        };
        let raw = &diagnostic["lsp_diagnostic"];
        let Some(code) = raw["code"].as_str() else {
            continue;
        };
        by_location
            .entry((path.into(), code.into(), raw["range"].to_string()))
            .or_default()
            .push(raw);
    }
    for diagnostic in diagnostics.iter_mut() {
        let Some(path) = diagnostic["location"]["path"]
            .as_str()
            .and_then(|path| generated.get(path))
        else {
            continue;
        };
        let raw = &diagnostic["lsp_diagnostic"];
        let Some(code) = raw["code"].as_str() else {
            continue;
        };
        let Some(candidates) =
            by_location.get(&(path.clone(), code.into(), raw["range"].to_string()))
        else {
            continue;
        };
        let candidate = match candidates.as_slice() {
            [only] => Some(*only),
            many => many
                .iter()
                .copied()
                .find(|candidate| candidate["message"] == raw["message"]),
        };
        if let Some(candidate) = candidate {
            // Never replace the established concise description, public range,
            // rule or severity with the editor's headline-and-label variant.
            diagnostic["lsp_diagnostic"] = candidate.clone();
        }
    }
    cli_diagnostic_details(diagnostics, documents, names)
}

pub fn cli_diagnostic_details(
    diagnostics: &mut [Value],
    documents: &[crate::project_check::ProjectDocument],
    names: BTreeMap<String, String>,
) -> Result<()> {
    let mut maps = LspSourceMaps::new(PositionEncoding::Utf32, PositionEncoding::Utf32);
    for (generated, original) in names {
        maps.add_name(generated, original);
    }
    let mut paths = vec![];
    for document in documents {
        let original = crate::checker::file_uri(std::path::Path::new(&document.source.path))?;
        let generated = crate::checker::file_uri(&document.path)?;
        maps.add_hidden_prefix(document.source.prefix.clone());
        maps.add_generated_root(crate::checker::file_uri(
            document
                .path
                .parent()
                .context("diagnostic projection parent")?,
        )?);
        paths.push((
            document.path.to_string_lossy().into_owned(),
            document.source.path.clone(),
        ));
        paths.push((generated.clone(), original.clone()));
        maps.insert(
            DocumentMap::new(
                original,
                generated,
                document.source.text.clone(),
                document.rendered.clone(),
            )
            .with_names(document.names.clone())
            .with_module(document.source.module.clone()),
        );
    }
    paths.sort_by_key(|(generated, _)| std::cmp::Reverse(generated.len()));
    for diagnostic in diagnostics {
        let Some(raw) = diagnostic.get("lsp_diagnostic") else {
            continue;
        };
        let original = crate::checker::file_uri(std::path::Path::new(
            diagnostic["location"]["path"]
                .as_str()
                .context("diagnostic source path")?,
        ))?;
        let generated = maps.generated_uri(&original);
        let mapped = maps.diagnostics(&json!({"uri":generated,"diagnostics":[raw]}))?;
        let Some(mapped) = mapped["diagnostics"]
            .as_array()
            .and_then(|items| items.first())
        else {
            continue;
        };
        if let Some((_, details)) = mapped["message"].as_str().unwrap_or("").split_once('\n') {
            let mut details = details.trim().to_owned();
            for (generated, original) in &paths {
                details = details.replace(generated, original);
            }
            if !details.is_empty() && !maps.hidden_text(&details) {
                diagnostic["details"] = json!(details);
            }
        }
        if let Some(related) = mapped["relatedInformation"].as_array() {
            let mut related = related.clone();
            for entry in &mut related {
                if let Some(message) = entry["message"].as_str() {
                    let mut message = message.to_owned();
                    for (generated, original) in &paths {
                        message = message.replace(generated, original);
                    }
                    entry["message"] = json!(message);
                }
            }
            related.retain(|entry| !maps.hidden_text(entry["message"].as_str().unwrap_or("")));
            if !related.is_empty() {
                diagnostic["related"] = json!(related);
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct DocumentMap {
    pub original_uri: String,
    pub generated_uri: String,
    original: String,
    rendered: Rendered,
    original_coordinates: Coordinates,
    generated_coordinates: Coordinates,
    fingerprint: u64,
    names: BTreeMap<String, String>,
    module: Option<String>,
    namespace_names: BTreeSet<String>,
}
impl DocumentMap {
    pub fn new(
        original_uri: String,
        generated_uri: String,
        original_text: String,
        rendered: Rendered,
    ) -> Self {
        let original_coordinates = Coordinates::new(&original_text);
        let namespace_names = source_names(&original_text);
        let generated_coordinates = Coordinates::new(&rendered.text);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        original_text.hash(&mut hasher);
        rendered.text.hash(&mut hasher);
        let fingerprint = hasher.finish();
        Self {
            original_uri,
            generated_uri,
            original: original_text,
            rendered,
            original_coordinates,
            generated_coordinates,
            fingerprint,
            names: BTreeMap::new(),
            module: None,
            namespace_names,
        }
    }
    pub fn with_names(mut self, names: BTreeMap<String, String>) -> Self {
        self.names = names;
        self
    }
    pub fn with_namespace(mut self, source: &str) -> Self {
        self.namespace_names = source_names(source);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.fingerprint.hash(&mut hasher);
        source.hash(&mut hasher);
        self.fingerprint = hasher.finish();
        self
    }
    pub fn with_module(mut self, module: String) -> Self {
        self.module = Some(module);
        self
    }
    fn span(&self, range: &Value, original: bool, encoding: PositionEncoding) -> Result<Span> {
        let (text, coordinates) = if original {
            (&self.original, &self.original_coordinates)
        } else {
            (&self.rendered.text, &self.generated_coordinates)
        };
        let start = coordinates.offset(text, &range["start"], encoding)?;
        let end = coordinates.offset(text, &range["end"], encoding)?;
        ensure!(start <= end, "inverted LSP range");
        Ok(Span { start, end })
    }
    fn range(&self, span: Span, original: bool, encoding: PositionEncoding) -> Result<Value> {
        let (text, coordinates) = if original {
            (&self.original, &self.original_coordinates)
        } else {
            (&self.rendered.text, &self.generated_coordinates)
        };
        Ok(
            json!({"start":coordinates.position(text,span.start,encoding)?,"end":coordinates.position(text,span.end,encoding)?}),
        )
    }
    pub fn to_generated_position(
        &self,
        position: &Value,
        client: PositionEncoding,
        server: PositionEncoding,
    ) -> Result<Value> {
        let original = self
            .original_coordinates
            .offset(&self.original, position, client)?;
        let generated = if self.original.is_empty() {
            self.rendered.text.len()
        } else {
            self.rendered
                .generated_span(Span {
                    start: original,
                    end: original,
                })
                .context("source position has no generated origin")?
                .start
        };
        self.generated_coordinates
            .position(&self.rendered.text, generated, server)
    }
    pub fn to_generated_range(
        &self,
        range: &Value,
        client: PositionEncoding,
        server: PositionEncoding,
    ) -> Result<Value> {
        let original = self.span(range, true, client)?;
        let generated = if self.original.is_empty() {
            Span {
                start: self.rendered.text.len(),
                end: self.rendered.text.len(),
            }
        } else {
            self.rendered
                .generated_span(original)
                .context("source range has no generated origin")?
        };
        self.range(generated, false, server)
    }
    pub fn to_original_range(
        &self,
        range: &Value,
        client: PositionEncoding,
        server: PositionEncoding,
        exact: bool,
    ) -> Result<Option<Value>> {
        let generated = self.span(range, false, server)?;
        let original = if self.original.is_empty()
            && generated.start == self.rendered.text.len()
            && generated.end == generated.start
        {
            Some(Span { start: 0, end: 0 })
        } else if exact {
            self.rendered.exact_original_span(generated)
        } else {
            self.rendered.original_span(generated)
        };
        original
            .map(|span| self.range(span, true, client))
            .transpose()
    }

    /// ty may choose a generated `from typing import cast` as the insertion
    /// site for a real user's `Literal` annotation. Relocate that import-only
    /// addition to the original import of the same module, never to a guessed
    /// source offset. Arbitrary synthetic edits remain forbidden.
    fn lower_import_edit(
        &self,
        value: &Value,
        client: PositionEncoding,
        server: PositionEncoding,
    ) -> Result<Option<Value>> {
        let generated = self.span(&value["range"], false, server)?;
        if generated.start != generated.end {
            return Ok(None);
        }
        let Some(addition) = value["newText"]
            .as_str()
            .and_then(|text| text.strip_prefix(','))
        else {
            return Ok(None);
        };
        let line = self
            .generated_coordinates
            .lines
            .partition_point(|start| *start <= generated.start)
            .saturating_sub(1);
        let start = self.generated_coordinates.lines[line];
        let end = self
            .generated_coordinates
            .lines
            .get(line + 1)
            .copied()
            .unwrap_or(self.rendered.text.len());
        let line_text = &self.rendered.text[start..end];
        let Ok(parsed) = ruff_python_parser::parse_module(line_text) else {
            return Ok(None);
        };
        let [ruff_python_ast::Stmt::ImportFrom(import)] = parsed.syntax().body.as_slice() else {
            return Ok(None);
        };
        let Some(module) = import.module.as_ref() else {
            return Ok(None);
        };
        if import.level != 0
            || module.as_str() == "__future__"
            || import
                .names
                .last()
                .is_none_or(|alias| start + alias.end().to_usize() != generated.start)
        {
            return Ok(None);
        }
        let Ok(added) =
            ruff_python_parser::parse_module(&format!("from {module} import {addition}"))
        else {
            return Ok(None);
        };
        if !matches!(
            added.syntax().body.as_slice(),
            [ruff_python_ast::Stmt::ImportFrom(_)]
        ) {
            return Ok(None);
        }
        let Ok(original) = ruff_python_parser::parse_module(&self.original) else {
            return Ok(None);
        };
        for statement in &original.syntax().body {
            if let ruff_python_ast::Stmt::ImportFrom(original) = statement
                && original.level == 0
                && original.module.as_ref() == Some(module)
            {
                let Some(last) = original.names.last() else {
                    continue;
                };
                let offset = last.end().to_usize();
                let mut out = value.clone();
                out["range"] = self.range(
                    Span {
                        start: offset,
                        end: offset,
                    },
                    true,
                    client,
                )?;
                return Ok(Some(out));
            }
        }
        Ok(None)
    }
}

#[derive(Clone)]
struct Resolve {
    method: String,
    uri: Option<String>,
    upstream: Value,
    fingerprint: Option<u64>,
}

pub struct LspSourceMaps {
    pub client_encoding: PositionEncoding,
    pub server_encoding: PositionEncoding,
    documents: BTreeMap<String, DocumentMap>,
    originals: BTreeMap<String, String>,
    generated_roots: BTreeSet<String>,
    hidden_prefixes: BTreeSet<String>,
    names: BTreeMap<String, String>,
    resolves: BTreeMap<u64, Resolve>,
    next_resolve: u64,
    semantic_tokens: BTreeMap<(String, String), Vec<Value>>,
    diagnostic_context: BTreeMap<(String, String), Value>,
}
impl LspSourceMaps {
    pub fn new(client_encoding: PositionEncoding, server_encoding: PositionEncoding) -> Self {
        Self {
            client_encoding,
            server_encoding,
            documents: BTreeMap::new(),
            originals: BTreeMap::new(),
            generated_roots: BTreeSet::new(),
            hidden_prefixes: BTreeSet::new(),
            names: BTreeMap::new(),
            resolves: BTreeMap::new(),
            next_resolve: 1,
            semantic_tokens: BTreeMap::new(),
            diagnostic_context: BTreeMap::new(),
        }
    }
    pub fn insert(&mut self, document: DocumentMap) {
        if let Some(previous) = self.originals.insert(
            document.original_uri.clone(),
            document.generated_uri.clone(),
        ) {
            self.documents.remove(&previous);
        }
        self.documents
            .insert(document.generated_uri.clone(), document);
    }
    pub fn clear_documents(&mut self) {
        self.documents.clear();
        self.originals.clear();
        self.names.clear();
        self.hidden_prefixes.clear();
        self.diagnostic_context.clear();
    }
    pub fn add_generated_root(&mut self, uri: String) {
        self.generated_roots
            .insert(uri.trim_end_matches('/').to_owned());
    }
    pub fn add_hidden_prefix(&mut self, prefix: String) {
        self.hidden_prefixes.insert(prefix);
    }
    pub fn add_name(&mut self, generated: String, original: String) {
        self.names.insert(generated, original);
    }
    /// Preserve the server's opaque diagnostic data and exact generated range
    /// while displaying normalized diagnostic text/locations to the editor.
    pub fn add_diagnostic(&mut self, original_uri: &str, presented: &Value, upstream: Value) {
        self.diagnostic_context.insert(
            (original_uri.to_owned(), diagnostic_key(presented)),
            upstream,
        );
    }
    pub fn document(&self, uri: &str) -> Option<&DocumentMap> {
        self.documents.get(uri).or_else(|| {
            self.originals
                .get(uri)
                .and_then(|generated| self.documents.get(generated))
        })
    }
    fn synthetic_uri(&self, uri: &str) -> bool {
        self.generated_roots.iter().any(|root| {
            uri == root
                || uri
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }
    pub fn generated_uri(&self, uri: &str) -> String {
        self.originals
            .get(uri)
            .cloned()
            .unwrap_or_else(|| uri.to_owned())
    }
    pub fn original_uri(&self, uri: &str) -> Option<String> {
        self.documents
            .get(uri)
            .map(|doc| doc.original_uri.clone())
            .or_else(|| (!self.synthetic_uri(uri)).then(|| uri.to_owned()))
    }
    fn external_range(&self, uri: &str, range: &Value, to_original: bool) -> Result<Value> {
        if self.client_encoding == self.server_encoding {
            return Ok(range.clone());
        }
        // External definitions (stdlib/site-packages) are not projected. Their
        // coordinates still need conversion when the two peers use different encodings.
        let Ok(Some(path)) = decode_file_uri(uri) else {
            return Ok(range.clone());
        };
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(range.clone());
        };
        let coordinates = Coordinates::new(&text);
        let (from, to) = if to_original {
            (self.server_encoding, self.client_encoding)
        } else {
            (self.client_encoding, self.server_encoding)
        };
        Ok(
            json!({"start":coordinates.position(&text,coordinates.offset(&text,&range["start"],from)?,to)?,"end":coordinates.position(&text,coordinates.offset(&text,&range["end"],from)?,to)?}),
        )
    }
    fn out_range(&self, uri: &str, range: &Value, exact: bool) -> Result<Option<Value>> {
        if let Some(doc) = self.document(uri) {
            doc.to_original_range(range, self.client_encoding, self.server_encoding, exact)
        } else if self.synthetic_uri(uri) {
            Ok(None)
        } else {
            Ok(Some(self.external_range(uri, range, true)?))
        }
    }
    fn in_range(&self, uri: &str, range: &Value) -> Result<Value> {
        if let Some(doc) = self.document(uri) {
            doc.to_generated_range(range, self.client_encoding, self.server_encoding)
        } else {
            self.external_range(uri, range, false)
        }
    }
    fn in_position(&self, uri: &str, position: &Value) -> Result<Value> {
        if let Some(doc) = self.document(uri) {
            doc.to_generated_position(position, self.client_encoding, self.server_encoding)
        } else {
            Ok(self.external_range(uri,&json!({"start":position,"end":position}),false)?["start"].clone())
        }
    }
    fn clean_text(&self, uri: Option<&str>, text: &str) -> String {
        let scoped = uri.and_then(|uri| self.document(uri));
        let name_of = |name: &str| {
            scoped
                .and_then(|doc| doc.names.get(name))
                .or_else(|| self.names.get(name))
        };
        crate::project::rewrite_names(text, |name| {
            if let Some(original) = name_of(name) {
                return original.clone();
            }
            let mut prefix = name;
            while let Some((head, _)) = prefix.rsplit_once('.') {
                if let Some(original) = name_of(head) {
                    let suffix = &name[head.len()..];
                    return if matches!(
                        original.as_str(),
                        "builtins" | "typing" | "typing_extensions"
                    ) {
                        suffix.trim_start_matches('.').to_owned()
                    } else {
                        format!("{original}{suffix}")
                    };
                }
                prefix = head;
            }
            name.to_owned()
        })
    }
    fn hidden_text(&self, text: &str) -> bool {
        self.hidden_prefixes
            .iter()
            .any(|prefix| text.contains(prefix))
    }
    fn clean_markup(&self, uri: Option<&str>, value: &mut Value) {
        match value {
            Value::String(s) => *s = self.clean_text(uri, s),
            Value::Array(items) => {
                for item in items {
                    self.clean_markup(uri, item);
                }
            }
            Value::Object(object) => {
                if let Some(content) = object.get_mut("value") {
                    self.clean_markup(uri, content);
                }
            }
            _ => {}
        }
    }
    fn cache_resolve(
        &mut self,
        method: &str,
        uri: Option<&str>,
        upstream: &Value,
        mapped: &mut Value,
    ) {
        let id = self.next_resolve;
        self.next_resolve = self.next_resolve.wrapping_add(1);
        let fingerprint = uri.and_then(|uri| self.document(uri).map(|doc| doc.fingerprint));
        self.resolves.insert(
            id,
            Resolve {
                method: method.to_owned(),
                uri: uri.map(str::to_owned),
                upstream: upstream.clone(),
                fingerprint,
            },
        );
        // Bound memory over long editor sessions. Old resolve requests return
        // a clear stale-item error instead of forwarding remapped coordinates.
        while self.resolves.len() > 8192 {
            self.resolves.pop_first();
        }
        mapped["data"] = json!({"_ty_companion_resolve":id});
    }
    fn resolve(&self, method: &str, params: &Value) -> Result<Option<&Resolve>> {
        let Some(id) = params["data"]["_ty_companion_resolve"].as_u64() else {
            return Ok(None);
        };
        let resolved = self
            .resolves
            .get(&id)
            .context("editor item expired; request a fresh completion/action")?;
        ensure!(
            resolved.method == method,
            "resolve item belongs to a different LSP method"
        );
        if let Some(fingerprint) = resolved.fingerprint {
            let current = resolved
                .uri
                .as_deref()
                .and_then(|uri| self.document(uri))
                .map(|doc| doc.fingerprint);
            if current != Some(fingerprint) {
                return Err(crate::checker::ProtocolError {
                    code: -32801,
                    message:
                        "document changed since this editor item was produced; request a fresh item"
                            .into(),
                    data: None,
                }
                .into());
            }
        }
        Ok(Some(resolved))
    }

    /// Resolve requests carry no textDocument. Route them to the workspace
    /// which owns their original item, including workspace-symbol resolves.
    pub fn request_uri(&self, method: &str, params: &Value) -> Result<Option<String>> {
        if let Some(uri) = params
            .pointer("/textDocument/uri")
            .or_else(|| params.pointer("/item/uri"))
            .and_then(Value::as_str)
        {
            return Ok(Some(uri.to_owned()));
        }
        if method.ends_with("/resolve")
            && let Some(item) = self.resolve(method, params)?
        {
            return Ok(item.uri.as_deref().and_then(|uri| self.original_uri(uri)));
        }
        Ok(None)
    }

    /// Plugin call-signature hooks affect call validation, but the reference
    /// fork's IDE exposes the callable's declared signature. Query that real
    /// callable's type in the same lexical scope rather than displaying our
    /// helper's validation signature. Only projected calls require this query.
    pub fn signature_view(&self, params: &Value) -> Result<Option<(String, String, Value)>> {
        use ruff_python_ast::visitor::{self, Visitor};
        let Some(uri) = params["textDocument"]["uri"].as_str() else {
            return Ok(None);
        };
        let Some(doc) = self.document(uri) else {
            return Ok(None);
        };
        let offset = doc.original_coordinates.offset(
            &doc.original,
            &params["position"],
            self.client_encoding,
        )?;
        struct Calls<'a> {
            doc: &'a DocumentMap,
            offset: usize,
            found: Option<Span>,
        }
        impl<'a, 'b> Visitor<'a> for Calls<'b> {
            fn visit_expr(&mut self, expr: &'a ruff_python_ast::Expr) {
                if let ruff_python_ast::Expr::Call(call) = expr
                    && call.arguments.start().to_usize() <= self.offset
                    && self.offset <= call.end().to_usize()
                    && self
                        .doc
                        .rendered
                        .hover_target(call.func.start().to_usize())
                        .is_some()
                {
                    self.found = Some(crate::model::span(call.func.as_ref()));
                }
                visitor::walk_expr(self, expr);
            }
        }
        let parsed = ruff_python_parser::parse_unchecked_source(
            &doc.original,
            ruff_python_ast::PySourceType::Python,
        );
        let mut calls = Calls {
            doc,
            offset,
            found: None,
        };
        calls.visit_body(&parsed.syntax().body);
        let Some(callee) = calls.found else {
            return Ok(None);
        };
        let _ = callee;
        let Some(text) = doc.rendered.signature_text() else {
            return Ok(None);
        };
        Ok(Some((
            doc.generated_uri.clone(),
            text,
            self.request("textDocument/signatureHelp", params)?,
        )))
    }

    /// Map client request parameters. The caller retains the original params
    /// and supplies them again when mapping the matching response.
    pub fn request(&self, method: &str, params: &Value) -> Result<Value> {
        if method.ends_with("/resolve")
            && let Some(item) = self.resolve(method, params)?
        {
            return Ok(item.upstream.clone());
        }
        let mut out = params.clone();
        if let Some(uri) = params["textDocument"]["uri"].as_str() {
            out["textDocument"]["uri"] = json!(self.generated_uri(uri));
            if !params["position"].is_null() {
                out["position"] = self.in_position(uri, &params["position"])?;
            }
            if !params["range"].is_null() {
                out["range"] = self.in_range(uri, &params["range"])?;
            }
            if let Some(positions) = params["positions"].as_array() {
                out["positions"] = Value::Array(
                    positions
                        .iter()
                        .map(|p| self.in_position(uri, p))
                        .collect::<Result<_>>()?,
                );
            }
            if method == "textDocument/codeAction"
                && let Some(diagnostics) = params["context"]["diagnostics"].as_array()
            {
                let mut mapped = Vec::new();
                for d in diagnostics {
                    if let Some(upstream) = self
                        .diagnostic_context
                        .get(&(uri.to_owned(), diagnostic_key(d)))
                    {
                        mapped.push(upstream.clone());
                        continue;
                    }
                    let mut d = d.clone();
                    d["range"] = self.in_range(uri, &d["range"])?;
                    if let Some(related) = d["relatedInformation"].as_array_mut() {
                        for entry in related {
                            let related_uri = entry["location"]["uri"]
                                .as_str()
                                .context("related diagnostic URI")?
                                .to_owned();
                            entry["location"]["range"] =
                                self.in_range(&related_uri, &entry["location"]["range"])?;
                            entry["location"]["uri"] = json!(self.generated_uri(&related_uri));
                        }
                    }
                    mapped.push(d);
                }
                out["context"]["diagnostics"] = json!(mapped);
            }
        }
        if matches!(
            method,
            "callHierarchy/incomingCalls"
                | "callHierarchy/outgoingCalls"
                | "typeHierarchy/supertypes"
                | "typeHierarchy/subtypes"
        ) {
            let item = &params["item"];
            if let Some(resolved) = self.resolve("hierarchy", item)? {
                out["item"] = resolved.upstream.clone();
            } else {
                out["item"] = self.in_item(item)?;
            }
        }
        Ok(out)
    }
    fn in_item(&self, item: &Value) -> Result<Value> {
        let mut out = item.clone();
        if let Some(uri) = item["uri"].as_str() {
            out["uri"] = json!(self.generated_uri(uri));
            for key in ["range", "selectionRange"] {
                if !item[key].is_null() {
                    out[key] = self.in_range(uri, &item[key])?;
                }
            }
        }
        Ok(out)
    }

    fn location(&self, value: &Value, origin_uri: Option<&str>) -> Result<Option<Value>> {
        let mut out = value.clone();
        if let Some(uri) = value["uri"].as_str() {
            let Some(mapped_uri) = self.original_uri(uri) else {
                return Ok(None);
            };
            let Some(range) = self.out_range(uri, &value["range"], false)? else {
                return self.parameter_location(uri, &value["range"]);
            };
            out["uri"] = json!(mapped_uri);
            out["range"] = range;
        } else if let Some(uri) = value["targetUri"].as_str() {
            let Some(mapped_uri) = self.original_uri(uri) else {
                return Ok(None);
            };
            let Some(range) = self.out_range(uri, &value["targetRange"], false)? else {
                return Ok(None);
            };
            let Some(selection) = self.out_range(uri, &value["targetSelectionRange"], false)?
            else {
                return Ok(None);
            };
            out["targetUri"] = json!(mapped_uri);
            out["targetRange"] = range;
            out["targetSelectionRange"] = selection;
            if let Some(origin_uri) = origin_uri
                && !value["originSelectionRange"].is_null()
            {
                if let Some(range) =
                    self.out_range(origin_uri, &value["originSelectionRange"], false)?
                {
                    out["originSelectionRange"] = range;
                } else {
                    out.as_object_mut().unwrap().remove("originSelectionRange");
                }
            }
        } else {
            bail!("LSP location has neither URI nor targetUri");
        }
        Ok(Some(out))
    }

    fn parameter_location(&self, uri: &str, range: &Value) -> Result<Option<Value>> {
        use ruff_python_ast::visitor::{self, Visitor};
        let Some(doc) = self.document(uri) else {
            return Ok(None);
        };
        let generated = doc.span(range, false, self.server_encoding)?;
        struct GeneratedParameter<'a> {
            doc: &'a DocumentMap,
            span: Span,
            found: Option<(String, String)>,
        }
        impl<'a, 'b> Visitor<'a> for GeneratedParameter<'b> {
            fn visit_stmt(&mut self, stmt: &'a ruff_python_ast::Stmt) {
                if let ruff_python_ast::Stmt::FunctionDef(function) = stmt
                    && let Some(qualified) = self.doc.names.get(function.name.as_str())
                {
                    for parameter in function.parameters.iter() {
                        let name = parameter.name();
                        if name.start().to_usize() <= self.span.start
                            && self.span.end <= name.end().to_usize()
                        {
                            self.found = Some((qualified.clone(), name.as_str().to_owned()));
                        }
                    }
                }
                visitor::walk_stmt(self, stmt);
            }
        }
        let parsed = ruff_python_parser::parse_unchecked_source(
            &doc.rendered.text,
            ruff_python_ast::PySourceType::Python,
        );
        let mut finder = GeneratedParameter {
            doc,
            span: generated,
            found: None,
        };
        finder.visit_body(&parsed.syntax().body);
        let Some((qualified, name)) = finder.found else {
            return Ok(None);
        };
        fn original_parameter(
            suite: &[ruff_python_ast::Stmt],
            prefix: &str,
            qualified: &str,
            name: &str,
        ) -> Option<Span> {
            for stmt in suite {
                match stmt {
                    ruff_python_ast::Stmt::FunctionDef(function) => {
                        let identity = format!("{prefix}.{}", function.name);
                        if identity == qualified {
                            return function
                                .parameters
                                .iter()
                                .find(|parameter| parameter.name().as_str() == name)
                                .map(|parameter| crate::model::span(parameter.name()));
                        }
                        if qualified.starts_with(&format!("{identity}."))
                            && let Some(found) =
                                original_parameter(&function.body, &identity, qualified, name)
                        {
                            return Some(found);
                        }
                    }
                    ruff_python_ast::Stmt::ClassDef(class) => {
                        let identity = format!("{prefix}.{}", class.name);
                        if qualified.starts_with(&format!("{identity}."))
                            && let Some(found) =
                                original_parameter(&class.body, &identity, qualified, name)
                        {
                            return Some(found);
                        }
                    }
                    _ => {}
                }
            }
            None
        }
        for original in self.documents.values() {
            let Some(module) = original.module.as_deref() else {
                continue;
            };
            if !qualified.starts_with(&format!("{module}.")) {
                continue;
            }
            let parsed = ruff_python_parser::parse_unchecked_source(
                &original.original,
                ruff_python_ast::PySourceType::Python,
            );
            if let Some(span) = original_parameter(&parsed.syntax().body, module, &qualified, &name)
            {
                return Ok(Some(
                    json!({"uri":original.original_uri,"range":original.range(span,true,self.client_encoding)?}),
                ));
            }
        }
        Ok(None)
    }
    fn diagnostic(&self, uri: &str, value: &Value) -> Result<Option<Value>> {
        let Some(range) = self.out_range(uri, &value["range"], false)? else {
            return Ok(None);
        };
        let mut out = value.clone();
        out["range"] = range;
        if let Some(message) = value["message"].as_str() {
            out["message"] = json!(self.clean_text(Some(uri), message));
        }
        if let Some(related) = value["relatedInformation"].as_array() {
            let mut mapped = Vec::new();
            for entry in related {
                if let Some(location) = self.location(&entry["location"], None)? {
                    let mut entry = entry.clone();
                    entry["location"] = location;
                    if let Some(message) = entry["message"].as_str() {
                        entry["message"] = json!(self.clean_text(Some(uri), message));
                    }
                    mapped.push(entry);
                }
            }
            out["relatedInformation"] = json!(mapped);
        }
        Ok(Some(out))
    }
    pub fn diagnostics(&self, params: &Value) -> Result<Value> {
        let uri = params["uri"].as_str().context("diagnostic document URI")?;
        let Some(original_uri) = self.original_uri(uri) else {
            return Ok(Value::Null);
        };
        let mut out = params.clone();
        out["uri"] = json!(original_uri);
        out["diagnostics"] = Value::Array(
            params["diagnostics"]
                .as_array()
                .context("diagnostic items")?
                .iter()
                .map(|d| self.diagnostic(uri, d))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
        );
        Ok(out)
    }
    fn text_edit(&self, uri: &str, value: &Value) -> Result<Value> {
        let mut out = value.clone();
        if !value["range"].is_null() {
            if let Some(range) = self.out_range(uri, &value["range"], true)? {
                out["range"] = range;
            } else if let Some(doc) = self.document(uri)
                && let Some(mapped) =
                    doc.lower_import_edit(value, self.client_encoding, self.server_encoding)?
            {
                out = mapped;
            } else {
                bail!("edit touches generated plugin code; refusing the entire edit");
            }
        } else if !value["insert"].is_null() && !value["replace"].is_null() {
            for field in ["insert", "replace"] {
                out[field] = self
                    .out_range(uri, &value[field], true)?
                    .context("completion edit touches generated plugin code")?;
            }
        } else {
            bail!("text edit has no range");
        }
        if let Some(text) = value["newText"].as_str() {
            ensure!(
                !self.hidden_text(text),
                "edit would insert a synthetic plugin name into source"
            );
        }
        Ok(out)
    }
    /// Any unmappable edit rejects the whole transaction. Applying only the
    /// safe half of a rename/code action could corrupt a user's project.
    pub fn workspace_edit(&self, value: &Value) -> Result<Value> {
        let mut out = value.clone();
        if let Some(changes) = value["changes"].as_object() {
            let mut mapped = serde_json::Map::new();
            for (uri, edits) in changes {
                let uri_out = self
                    .original_uri(uri)
                    .context("workspace edit targets generated-only file")?;
                let items = edits
                    .as_array()
                    .context("workspace edit array")?
                    .iter()
                    .map(|edit| self.text_edit(uri, edit))
                    .collect::<Result<Vec<_>>>()?;
                ensure!(
                    !mapped.contains_key(&uri_out),
                    "two generated files edit the same original file"
                );
                mapped.insert(uri_out, json!(items));
            }
            out["changes"] = Value::Object(mapped);
        }
        if let Some(changes) = value["documentChanges"].as_array() {
            let mut mapped = Vec::new();
            for change in changes {
                let mut change_out = change.clone();
                if let Some(uri) = change["textDocument"]["uri"].as_str() {
                    change_out["textDocument"]["uri"] = json!(
                        self.original_uri(uri)
                            .context("workspace edit targets generated-only file")?
                    );
                    // Backend document versions count projection passes, not IDE
                    // edits. Root validates the editor snapshot before applying.
                    change_out["textDocument"]["version"] = Value::Null;
                    change_out["edits"] = Value::Array(
                        change["edits"]
                            .as_array()
                            .context("text document edits")?
                            .iter()
                            .map(|edit| self.text_edit(uri, edit))
                            .collect::<Result<_>>()?,
                    );
                } else {
                    for key in ["uri", "oldUri", "newUri"] {
                        if let Some(uri) = change[key].as_str() {
                            // Creating/deleting an overlay can never imply creating
                            // or deleting the source from which it was generated.
                            ensure!(
                                !self.synthetic_uri(uri),
                                "resource operation targets a generated analysis file"
                            );
                        }
                    }
                }
                mapped.push(change_out);
            }
            out["documentChanges"] = json!(mapped);
        }
        Ok(out)
    }

    fn completion_item(&mut self, uri: &str, value: &Value) -> Result<Option<Value>> {
        let mut out = value.clone();
        for key in [
            "label",
            "detail",
            "filterText",
            "sortText",
            "insertText",
            "textEditText",
        ] {
            if let Some(text) = value[key].as_str() {
                let clean = self.clean_text(Some(uri), text);
                if self.hidden_text(&clean) {
                    return Ok(None);
                }
                out[key] = json!(clean);
            }
        }
        if !value["documentation"].is_null() {
            self.clean_markup(Some(uri), &mut out["documentation"]);
        }
        if !value["textEdit"].is_null() {
            out["textEdit"] = self.text_edit(uri, &value["textEdit"])?;
        }
        if let Some(edits) = value["additionalTextEdits"].as_array() {
            out["additionalTextEdits"] = Value::Array(
                edits
                    .iter()
                    .map(|e| self.text_edit(uri, e))
                    .collect::<Result<_>>()?,
            );
        }
        self.cache_resolve("completionItem/resolve", Some(uri), value, &mut out);
        Ok(Some(out))
    }
    fn hierarchy_item(&mut self, value: &Value) -> Result<Option<Value>> {
        let uri = value["uri"].as_str().context("hierarchy item URI")?;
        let Some(original_uri) = self.original_uri(uri) else {
            return Ok(None);
        };
        // ty represents a module caller by a (0,0)-(0,0) sentinel rather than
        // an actual source token. A generated prelude must not erase the module.
        let module = value["kind"] == 2
            && value["range"]
                == json!({"start":{"line":0,"character":0},"end":{"line":0,"character":0}});
        let Some(range) = (if module {
            Some(value["range"].clone())
        } else {
            self.out_range(uri, &value["range"], false)?
        }) else {
            return Ok(None);
        };
        let Some(selection) = (if module {
            Some(value["selectionRange"].clone())
        } else {
            self.out_range(uri, &value["selectionRange"], false)?
        }) else {
            return Ok(None);
        };
        let mut out = value.clone();
        out["uri"] = json!(original_uri);
        out["range"] = range;
        out["selectionRange"] = selection;
        for key in ["name", "detail"] {
            if let Some(text) = value[key].as_str() {
                let clean = self.clean_text(Some(uri), text);
                if self.hidden_text(&clean) {
                    return Ok(None);
                }
                out[key] = json!(clean);
            }
        }
        self.cache_resolve("hierarchy", Some(uri), value, &mut out);
        Ok(Some(out))
    }
    fn document_symbol(&self, uri: &str, value: &Value) -> Result<Option<Value>> {
        let mut out = value.clone();
        let name = self.clean_text(Some(uri), value["name"].as_str().unwrap_or(""));
        if self.hidden_text(&name) {
            return Ok(None);
        }
        out["name"] = json!(name);
        if !value["location"].is_null() {
            let Some(location) = self.location(&value["location"], None)? else {
                return Ok(None);
            };
            out["location"] = location;
        } else {
            let Some(range) = self.out_range(uri, &value["range"], false)? else {
                return Ok(None);
            };
            let Some(selection) = self.out_range(uri, &value["selectionRange"], false)? else {
                return Ok(None);
            };
            out["range"] = range;
            out["selectionRange"] = selection;
        }
        if let Some(children) = value["children"].as_array() {
            out["children"] = Value::Array(
                children
                    .iter()
                    .map(|c| self.document_symbol(uri, c))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            );
        }
        if let Some(detail) = value["detail"].as_str() {
            out["detail"] = json!(self.clean_text(Some(uri), detail));
        }
        Ok(Some(out))
    }
    fn selection_range(&self, uri: &str, value: &Value) -> Result<Option<Value>> {
        let parent = if value["parent"].is_object() {
            self.selection_range(uri, &value["parent"])?
        } else {
            None
        };
        let Some(range) = self.out_range(uri, &value["range"], false)? else {
            return Ok(parent);
        };
        let mut out = json!({"range":range});
        if let Some(parent) = parent
            && parent["range"] != out["range"]
        {
            out["parent"] = parent;
        }
        Ok(Some(out))
    }
    fn semantic(&mut self, uri: &str, params: &Value, value: &Value) -> Result<Value> {
        let generated_uri = self.generated_uri(uri);
        let raw = if let Some(data) = value["data"].as_array() {
            data.clone()
        } else {
            let previous = params["previousResultId"]
                .as_str()
                .context("semantic token delta without previous result id")?;
            let mut data = self
                .semantic_tokens
                .get(&(generated_uri.clone(), previous.to_owned()))
                .context("semantic token delta baseline expired; request full tokens")?
                .clone();
            // Delta offsets are relative to the same old array, so apply from
            // the greatest start. The order supplied by the server is arbitrary.
            let mut edits = value["edits"]
                .as_array()
                .context("semantic token delta edits")?
                .iter()
                .collect::<Vec<_>>();
            edits.sort_by_key(|edit| std::cmp::Reverse(edit["start"].as_u64().unwrap_or(u64::MAX)));
            let mut next_start = data.len();
            for edit in edits {
                let start = index(edit, "start")?;
                let count = index(edit, "deleteCount")?;
                let end = start
                    .checked_add(count)
                    .context("semantic delta overflow")?;
                ensure!(
                    end <= next_start && end <= data.len(),
                    "overlapping/out-of-bounds semantic token delta"
                );
                let replacement = edit["data"].as_array().cloned().unwrap_or_default();
                data.splice(start..end, replacement);
                next_start = start;
            }
            data
        };
        ensure!(
            raw.len().is_multiple_of(5),
            "semantic token array length is not divisible by five"
        );
        let mut line = 0usize;
        let mut character = 0usize;
        let mut mapped = Vec::new();
        for token in raw.as_chunks::<5>().0 {
            let number = |i: usize| {
                usize::try_from(token[i].as_u64().context("non-integer semantic token")?)
                    .context("semantic token integer overflow")
            };
            let delta_line = number(0)?;
            line = line
                .checked_add(delta_line)
                .context("semantic token line overflow")?;
            character = if delta_line == 0 {
                character
                    .checked_add(number(1)?)
                    .context("semantic token column overflow")?
            } else {
                number(1)?
            };
            let end = character
                .checked_add(number(2)?)
                .context("semantic token length overflow")?;
            let range = json!({"start":{"line":line,"character":character},"end":{"line":line,"character":end}});
            if let Some(range) = self.out_range(uri, &range, true)? {
                if range["start"]["line"] != range["end"]["line"] {
                    continue;
                }
                let start_line = index(&range["start"], "line")?;
                let start_char = index(&range["start"], "character")?;
                let end_char = index(&range["end"], "character")?;
                if end_char > start_char {
                    mapped.push((
                        start_line,
                        start_char,
                        end_char - start_char,
                        number(3)?,
                        number(4)?,
                    ));
                }
            }
        }
        mapped.sort_unstable();
        mapped.dedup();
        let mut data = Vec::with_capacity(mapped.len() * 5);
        let mut previous_line = 0;
        let mut previous_char = 0;
        for (line, character, length, kind, modifiers) in mapped {
            data.extend([
                json!(line - previous_line),
                json!(if line == previous_line {
                    character - previous_char
                } else {
                    character
                }),
                json!(length),
                json!(kind),
                json!(modifiers),
            ]);
            previous_line = line;
            previous_char = character;
        }
        let mut out = json!({"data":data});
        if let Some(id) = value["resultId"].as_str() {
            out["resultId"] = json!(id);
            self.semantic_tokens
                .insert((generated_uri, id.to_owned()), raw);
            while self.semantic_tokens.len() > 128 {
                self.semantic_tokens.pop_first();
            }
        }
        // A full result is allowed in response to a delta request. This avoids
        // delta indices referring to tokens suppressed from the generated view.
        Ok(out)
    }
    fn diagnostic_report(&self, uri: &str, value: &Value) -> Result<Value> {
        let mut out = value.clone();
        if let Some(items) = value["items"].as_array() {
            out["items"] = Value::Array(
                items
                    .iter()
                    .map(|d| self.diagnostic(uri, d))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            );
        }
        if let Some(related) = value["relatedDocuments"].as_object() {
            let mut mapped = serde_json::Map::new();
            for (uri, report) in related {
                if let Some(original_uri) = self.original_uri(uri) {
                    mapped.insert(original_uri, self.diagnostic_report(uri, report)?);
                }
            }
            out["relatedDocuments"] = Value::Object(mapped);
        }
        Ok(out)
    }
    fn inlay_hint(&mut self, uri: &str, value: &Value) -> Result<Option<Value>> {
        if value["kind"] == 2
            && let Some(doc) = self.document(uri)
        {
            let offset = doc.generated_coordinates.offset(
                &doc.rendered.text,
                &value["position"],
                self.server_encoding,
            )?;
            if doc.rendered.synthetic_parameter_hint(offset) {
                return Ok(None);
            }
        }
        let point = json!({"start":value["position"],"end":value["position"]});
        let Some(range) = self.out_range(uri, &point, true)? else {
            return Ok(None);
        };
        let mut out = value.clone();
        out["position"] = range["start"].clone();
        if let Some(label) = value["label"].as_str() {
            let clean = self.clean_text(Some(uri), label);
            if self.hidden_text(&clean) {
                return Ok(None);
            }
            out["label"] = json!(clean);
        } else if let Some(parts) = value["label"].as_array() {
            let mut mapped = Vec::new();
            for part in parts {
                let mut part = part.clone();
                if let Some(label) = part["value"].as_str() {
                    let clean = self.clean_text(Some(uri), label);
                    if self.hidden_text(&clean) {
                        return Ok(None);
                    }
                    part["value"] = json!(clean);
                }
                if !part["location"].is_null() {
                    if let Some(location) = self.location(&part["location"], None)? {
                        part["location"] = location;
                    } else {
                        part.as_object_mut().unwrap().remove("location");
                    }
                }
                if !part["tooltip"].is_null() {
                    self.clean_markup(Some(uri), &mut part["tooltip"]);
                }
                mapped.push(part);
            }
            out["label"] = json!(mapped);
        }
        if let Some(edits) = value["textEdits"].as_array() {
            out["textEdits"] = json!(self.inlay_edits(uri, edits)?);
        }
        if !out["tooltip"].is_null() {
            self.clean_markup(Some(uri), &mut out["tooltip"]);
        }
        self.cache_resolve("inlayHint/resolve", Some(uri), value, &mut out);
        Ok(Some(out))
    }

    /// Type annotations sometimes reuse a hygienic module alias introduced by
    /// a plugin (especially when it was imported in an earlier notebook cell).
    /// Lower only a parsed annotation insertion to real imports; arbitrary
    /// synthetic edit text remains forbidden by `text_edit`.
    fn inlay_edits(&self, uri: &str, edits: &[Value]) -> Result<Vec<Value>> {
        let Some(doc) = self.document(uri) else {
            return edits.iter().map(|edit| self.text_edit(uri, edit)).collect();
        };
        let mut imports = BTreeSet::<(String, String, String)>::new();
        let mut reserved = doc.namespace_names.clone();
        let mut chosen = BTreeMap::<(String, String), String>::new();
        let mut output = vec![];
        for edit in edits {
            let text = edit["newText"].as_str().context("inlay edit text")?;
            if !self.hidden_text(text) {
                output.push(self.text_edit(uri, edit)?);
                continue;
            }
            ensure!(
                text.starts_with(':') && edit["range"]["start"] == edit["range"]["end"],
                "synthetic edit is not an annotation insertion"
            );
            let annotation = text.trim_start_matches(':').trim();
            ruff_python_parser::parse_expression(annotation)
                .context("invalid generated inlay annotation")?;
            let rewritten = crate::project::rewrite_names(annotation, |name| {
                let mut prefix = name;
                let found = loop {
                    if let Some(original) = doc.names.get(prefix).or_else(|| self.names.get(prefix))
                    {
                        break Some((prefix, original.as_str()));
                    }
                    if let Some((head, _)) = prefix.rsplit_once('.') {
                        prefix = head;
                    } else {
                        break None;
                    }
                };
                let Some((prefix, original)) = found else {
                    return name.into();
                };
                let (module, symbol, tail) = if prefix.len() < name.len() {
                    let rest = &name[prefix.len() + 1..];
                    let (symbol, tail) = rest
                        .split_once('.')
                        .map_or((rest, ""), |(symbol, tail)| (symbol, tail));
                    (original, symbol, tail)
                } else if let Some((module, symbol)) = original.rsplit_once('.') {
                    (module, symbol, "")
                } else {
                    return name.into();
                };
                let identifier = |value: &str| {
                    !value.is_empty()
                        && value.chars().enumerate().all(|(index, ch)| {
                            ch == '_' || ch.is_alphabetic() || (index > 0 && ch.is_alphanumeric())
                        })
                };
                if !module.split('.').all(identifier) || !identifier(symbol) {
                    return name.into();
                }
                let key = (module.to_owned(), symbol.to_owned());
                let alias = chosen.entry(key.clone()).or_insert_with(|| {
                    let alias = if !reserved.contains(symbol) {
                        symbol.to_owned()
                    } else {
                        let mut candidate = format!("_ty_annotation_{symbol}");
                        while reserved.contains(&candidate) {
                            candidate.push('_');
                        }
                        candidate
                    };
                    reserved.insert(alias.clone());
                    if module != "builtins" || alias != symbol {
                        imports.insert((key.0, key.1, alias.clone()));
                    }
                    alias
                });
                if tail.is_empty() {
                    alias.clone()
                } else {
                    format!("{alias}.{tail}")
                }
            });
            let mut lowered = edit.clone();
            lowered["newText"] = json!(format!(": {rewritten}"));
            output.push(self.text_edit(uri, &lowered)?);
        }
        if !imports.is_empty() {
            let parsed = ruff_python_parser::parse_unchecked_source(
                &doc.original,
                ruff_python_ast::PySourceType::Ipynb,
            );
            let newline = if doc.original.contains("\r\n") {
                "\r\n"
            } else if doc.original.contains('\r') && !doc.original.contains('\n') {
                "\r"
            } else {
                "\n"
            };
            // Keep executable shebangs, encoding comments, and leading comments
            // ahead of imports. Future imports must also retain their position.
            let mut after = parsed
                .syntax()
                .body
                .first()
                .map_or(0, |statement| statement.start().to_usize());
            let mut after_statement = false;
            for (index, statement) in parsed.syntax().body.iter().enumerate() {
                if (index == 0 && ruff_python_ast::helpers::is_docstring_stmt(statement))
                    || matches!(statement,ruff_python_ast::Stmt::ImportFrom(import) if import.module.as_ref().is_some_and(|name|name.as_str()=="__future__"))
                {
                    after = statement.end().to_usize();
                    after_statement = true;
                } else {
                    break;
                }
            }
            let mut prefix = "";
            if after_statement {
                if let Some(offset) = doc.original[after..].find(['\r', '\n']) {
                    after += offset;
                    after += if doc.original[after..].starts_with("\r\n") {
                        2
                    } else {
                        1
                    };
                } else {
                    after = doc.original.len();
                    prefix = newline;
                }
            }
            let mut text = prefix.to_owned();
            for (module, symbol, alias) in imports {
                text.push_str(&format!("from {module} import {symbol}"));
                if alias != symbol {
                    text.push_str(&format!(" as {alias}"));
                }
                text.push_str(newline);
            }
            let position =
                doc.original_coordinates
                    .position(&doc.original, after, self.client_encoding)?;
            output.push(json!({"range":{"start":position,"end":position},"newText":text}));
        }
        Ok(output)
    }
    fn code_action(&mut self, uri: &str, value: &Value) -> Result<Value> {
        let mut out = value.clone();
        if let Some(title) = value["title"].as_str() {
            out["title"] = json!(self.clean_text(Some(uri), title));
        }
        if !value["edit"].is_null() {
            out["edit"] = self.workspace_edit(&value["edit"])?;
        }
        if let Some(diagnostics) = value["diagnostics"].as_array() {
            out["diagnostics"] = Value::Array(
                diagnostics
                    .iter()
                    .map(|d| self.diagnostic(uri, d))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            );
        }
        // Command is a separate union variant; only CodeAction is resolvable.
        if !value["command"].is_string() {
            self.cache_resolve("codeAction/resolve", Some(uri), value, &mut out);
        }
        Ok(out)
    }
    fn document_link(&mut self, uri: &str, value: &Value) -> Result<Option<Value>> {
        let Some(range) = self.out_range(uri, &value["range"], false)? else {
            return Ok(None);
        };
        let mut out = value.clone();
        out["range"] = range;
        if let Some(target) = value["target"].as_str() {
            let Some(target) = self.original_uri(target) else {
                return Ok(None);
            };
            out["target"] = json!(target);
        }
        self.cache_resolve("documentLink/resolve", Some(uri), value, &mut out);
        Ok(Some(out))
    }

    pub fn response(
        &mut self,
        method: &str,
        original_params: &Value,
        result: &Value,
    ) -> Result<Value> {
        if result.is_null() {
            return Ok(Value::Null);
        }
        let resolve = if method.ends_with("/resolve") {
            self.resolve(method, original_params)?.cloned()
        } else {
            None
        };
        let uri = original_params["textDocument"]["uri"]
            .as_str()
            .map(str::to_owned)
            .or_else(|| resolve.as_ref().and_then(|r| r.uri.clone()));
        let uri = uri.as_deref();
        let require_uri = || uri.context("LSP response needs originating document URI");
        match method {
            "textDocument/definition"
            | "textDocument/declaration"
            | "textDocument/typeDefinition"
            | "textDocument/implementation"
            | "textDocument/references" => {
                if let Some(items) = result.as_array() {
                    Ok(Value::Array(
                        items
                            .iter()
                            .map(|v| self.location(v, uri))
                            .collect::<Result<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect(),
                    ))
                } else {
                    Ok(self.location(result, uri)?.unwrap_or(Value::Null))
                }
            }
            "textDocument/hover" => {
                let mut out = result.clone();
                if !result["range"].is_null() {
                    let Some(range) = self.out_range(require_uri()?, &result["range"], false)?
                    else {
                        return Ok(Value::Null);
                    };
                    out["range"] = range;
                }
                self.clean_markup(uri, &mut out["contents"]);
                if let Some(doc) = self.document(require_uri()?)
                    && !out["range"].is_null()
                {
                    let span = doc.span(&out["range"], true, self.client_encoding)?;
                    if let Some(expression) = doc.rendered.hover_type(span) {
                        let expression = self.clean_text(uri, expression);
                        if !self.hidden_text(&expression) {
                            replace_hover_type(&mut out["contents"], &expression);
                        }
                    }
                }
                // Displaying an implementation helper signature would be false
                // editor information, even when its position maps correctly.
                if self.hidden_text(&out["contents"].to_string()) {
                    return Ok(Value::Null);
                }
                Ok(out)
            }
            "textDocument/completion" => {
                let uri = require_uri()?;
                let items = if result.is_array() {
                    result.as_array().unwrap()
                } else {
                    result["items"].as_array().context("completion items")?
                };
                let defaults = result.get("itemDefaults");
                let mut mapped = Vec::new();
                for item in items {
                    // Expand edit defaults before mapping each item: an item
                    // resolve must see the original generated range and data.
                    let mut item = item.clone();
                    if let Some(defaults) = defaults {
                        for field in [
                            "commitCharacters",
                            "insertTextFormat",
                            "insertTextMode",
                            "data",
                        ] {
                            if item.get(field).is_none()
                                && let Some(v) = defaults.get(field)
                            {
                                item[field] = v.clone();
                            }
                        }
                        if item.get("textEdit").is_none()
                            && let Some(range) = defaults.get("editRange")
                        {
                            let text = item
                                .get("textEditText")
                                .or_else(|| item.get("insertText"))
                                .or_else(|| item.get("label"))
                                .context("completion label")?
                                .clone();
                            item["textEdit"] = if range.get("start").is_some() {
                                json!({"range":range,"newText":text})
                            } else {
                                json!({"insert":range["insert"],"replace":range["replace"],"newText":text})
                            };
                        }
                    }
                    if let Some(item) = self.completion_item(uri, &item)? {
                        mapped.push(item);
                    }
                }
                if result.is_array() {
                    Ok(json!(mapped))
                } else {
                    let mut out = result.clone();
                    out["items"] = json!(mapped);
                    out.as_object_mut().unwrap().remove("itemDefaults");
                    Ok(out)
                }
            }
            "completionItem/resolve" => Ok(self
                .completion_item(require_uri()?, result)?
                .unwrap_or(Value::Null)),
            "textDocument/rename" => self.workspace_edit(result),
            "textDocument/prepareRename" => {
                let uri = require_uri()?;
                if result.get("defaultBehavior").is_some() {
                    return Ok(result.clone());
                }
                if result.get("range").is_some() {
                    let Some(range) = self.out_range(uri, &result["range"], true)? else {
                        return Ok(Value::Null);
                    };
                    let mut out = result.clone();
                    out["range"] = range;
                    Ok(out)
                } else {
                    Ok(self.out_range(uri, result, true)?.unwrap_or(Value::Null))
                }
            }
            "textDocument/codeAction" => {
                let uri = require_uri()?;
                Ok(Value::Array(
                    result
                        .as_array()
                        .context("code action array")?
                        .iter()
                        .map(|v| self.code_action(uri, v))
                        .collect::<Result<_>>()?,
                ))
            }
            "codeAction/resolve" => self.code_action(require_uri()?, result),
            "textDocument/documentSymbol" => {
                let uri = require_uri()?;
                Ok(Value::Array(
                    result
                        .as_array()
                        .context("document symbols")?
                        .iter()
                        .map(|v| self.document_symbol(uri, v))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect(),
                ))
            }
            "workspace/symbol" | "workspaceSymbol/resolve" => {
                let values = if let Some(items) = result.as_array() {
                    items.clone()
                } else {
                    vec![result.clone()]
                };
                let mut mapped = Vec::new();
                for value in values {
                    let mut out = value.clone();
                    let name = self.clean_text(
                        value["location"]["uri"].as_str(),
                        value["name"].as_str().unwrap_or(""),
                    );
                    if self.hidden_text(&name) {
                        continue;
                    }
                    out["name"] = json!(name);
                    if value["location"]["range"].is_object() {
                        let Some(location) = self.location(&value["location"], None)? else {
                            continue;
                        };
                        out["location"] = location;
                    } else if let Some(uri) = value["location"]["uri"].as_str() {
                        let Some(original_uri) = self.original_uri(uri) else {
                            continue;
                        };
                        out["location"]["uri"] = json!(original_uri);
                    }
                    self.cache_resolve(
                        "workspaceSymbol/resolve",
                        value["location"]["uri"].as_str(),
                        &value,
                        &mut out,
                    );
                    mapped.push(out);
                }
                if result.is_array() {
                    Ok(json!(mapped))
                } else {
                    Ok(mapped.pop().unwrap_or(Value::Null))
                }
            }
            "textDocument/selectionRange" => {
                let uri = require_uri()?;
                // SelectionRange requires one result for every requested cursor.
                let mut items = Vec::new();
                for value in result.as_array().context("selection ranges")? {
                    items.push(
                        self.selection_range(uri, value)?
                            .context("selection range has no original source")?,
                    );
                }
                Ok(json!(items))
            }
            "textDocument/documentHighlight" => {
                let uri = require_uri()?;
                let mut mapped = Vec::new();
                for value in result.as_array().context("document highlights")? {
                    if let Some(range) = self.out_range(uri, &value["range"], false)? {
                        let mut value = value.clone();
                        value["range"] = range;
                        mapped.push(value);
                    }
                }
                Ok(json!(mapped))
            }
            "textDocument/formatting"
            | "textDocument/rangeFormatting"
            | "textDocument/onTypeFormatting" => {
                let uri = require_uri()?;
                Ok(Value::Array(
                    result
                        .as_array()
                        .context("formatting edits")?
                        .iter()
                        .map(|v| self.text_edit(uri, v))
                        .collect::<Result<_>>()?,
                ))
            }
            "textDocument/inlayHint" => {
                let uri = require_uri()?;
                let mut items = Vec::new();
                for value in result.as_array().context("inlay hints")? {
                    if let Some(item) = self.inlay_hint(uri, value)? {
                        items.push(item);
                    }
                }
                Ok(json!(items))
            }
            "inlayHint/resolve" => Ok(self
                .inlay_hint(require_uri()?, result)?
                .unwrap_or(Value::Null)),
            "textDocument/documentLink" => {
                let uri = require_uri()?;
                let mut items = Vec::new();
                for value in result.as_array().context("document links")? {
                    if let Some(item) = self.document_link(uri, value)? {
                        items.push(item);
                    }
                }
                Ok(json!(items))
            }
            "documentLink/resolve" => Ok(self
                .document_link(require_uri()?, result)?
                .unwrap_or(Value::Null)),
            "textDocument/semanticTokens/full"
            | "textDocument/semanticTokens/full/delta"
            | "textDocument/semanticTokens/range" => {
                self.semantic(require_uri()?, original_params, result)
            }
            "textDocument/diagnostic" => self.diagnostic_report(require_uri()?, result),
            "workspace/diagnostic" => {
                let mut out = result.clone();
                let mut items = Vec::new();
                for value in result["items"]
                    .as_array()
                    .context("workspace diagnostics")?
                {
                    let uri = value["uri"].as_str().context("diagnostic URI")?;
                    let Some(original_uri) = self.original_uri(uri) else {
                        continue;
                    };
                    let mut item = self.diagnostic_report(uri, value)?;
                    item["uri"] = json!(original_uri);
                    item["version"] = Value::Null;
                    items.push(item);
                }
                out["items"] = json!(items);
                Ok(out)
            }
            "textDocument/prepareCallHierarchy"
            | "textDocument/prepareTypeHierarchy"
            | "typeHierarchy/supertypes"
            | "typeHierarchy/subtypes" => {
                let mut items = Vec::new();
                for value in result.as_array().context("hierarchy items")? {
                    if let Some(item) = self.hierarchy_item(value)? {
                        items.push(item);
                    }
                }
                Ok(json!(items))
            }
            "callHierarchy/incomingCalls" | "callHierarchy/outgoingCalls" => {
                let incoming = method.ends_with("incomingCalls");
                let key = if incoming { "from" } else { "to" };
                let source_uri = original_params["item"]["uri"]
                    .as_str()
                    .context("hierarchy source URI")?;
                let mut items = Vec::new();
                for value in result.as_array().context("hierarchy calls")? {
                    let Some(item) = self.hierarchy_item(&value[key])? else {
                        continue;
                    };
                    let ranges_uri = if incoming {
                        value[key]["uri"].as_str().context("incoming call URI")?
                    } else {
                        source_uri
                    };
                    let ranges = value["fromRanges"]
                        .as_array()
                        .context("call hierarchy ranges")?
                        .iter()
                        .map(|r| self.out_range(ranges_uri, r, false))
                        .collect::<Result<Vec<_>>>()?
                        .into_iter()
                        .flatten()
                        .collect::<Vec<_>>();
                    let mut out = value.clone();
                    out[key] = item;
                    out["fromRanges"] = json!(ranges);
                    items.push(out);
                }
                Ok(json!(items))
            }
            "textDocument/foldingRange" => {
                let uri = require_uri()?;
                let mut items = Vec::new();
                for value in result.as_array().context("folding ranges")? {
                    let range = json!({"start":{"line":value["startLine"],"character":value.get("startCharacter").unwrap_or(&json!(0))},"end":{"line":value["endLine"],"character":value.get("endCharacter").unwrap_or(&json!(0))}});
                    if let Some(range) = self.out_range(uri, &range, false)? {
                        let mut out = value.clone();
                        out["startLine"] = range["start"]["line"].clone();
                        out["endLine"] = range["end"]["line"].clone();
                        if value.get("startCharacter").is_some() {
                            out["startCharacter"] = range["start"]["character"].clone();
                        }
                        if value.get("endCharacter").is_some() {
                            out["endCharacter"] = range["end"]["character"].clone();
                        }
                        if out["startLine"] != out["endLine"] {
                            items.push(out);
                        }
                    }
                }
                Ok(json!(items))
            }
            "textDocument/signatureHelp" => {
                let mut out = result.clone();
                let mut signatures = Vec::new();
                for signature in result["signatures"]
                    .as_array()
                    .context("signature help signatures")?
                {
                    let mut item = signature.clone();
                    let label = self.clean_text(uri, signature["label"].as_str().unwrap_or(""));
                    if self.hidden_text(&label) {
                        continue;
                    }
                    if label != signature["label"]
                        && let Some(parameters) = item["parameters"].as_array_mut()
                    {
                        // Replace positional parameter labels with strings before
                        // changing the containing signature; UTF-16 indices would
                        // otherwise point into the wrong part of the new label.
                        let original = signature["label"].as_str().unwrap_or("");
                        for parameter in parameters {
                            if let Some(span) = parameter["label"].as_array() {
                                ensure!(span.len() == 2, "signature parameter range length");
                                let start = position_to_offset(
                                    original,
                                    &json!({"line":0,"character":span[0]}),
                                    PositionEncoding::Utf16,
                                )?;
                                let end = position_to_offset(
                                    original,
                                    &json!({"line":0,"character":span[1]}),
                                    PositionEncoding::Utf16,
                                )?;
                                ensure!(start <= end, "signature parameter range reversed");
                                parameter["label"] =
                                    json!(self.clean_text(uri, &original[start..end]));
                            }
                        }
                    }
                    item["label"] = json!(label);
                    if !item["documentation"].is_null() {
                        self.clean_markup(uri, &mut item["documentation"]);
                    }
                    signatures.push(item);
                }
                if signatures.len() != result["signatures"].as_array().unwrap().len() {
                    out["activeSignature"] = json!(0);
                }
                out["signatures"] = json!(signatures);
                Ok(out)
            }
            "textDocument/codeLens" => {
                let uri = require_uri()?;
                let mut items = Vec::new();
                for value in result.as_array().context("code lenses")? {
                    if let Some(range) = self.out_range(uri, &value["range"], false)? {
                        let mut out = value.clone();
                        out["range"] = range;
                        self.cache_resolve("codeLens/resolve", Some(uri), value, &mut out);
                        items.push(out);
                    }
                }
                Ok(json!(items))
            }
            "codeLens/resolve" => {
                let mut out = result.clone();
                let Some(range) = self.out_range(require_uri()?, &result["range"], false)? else {
                    return Ok(Value::Null);
                };
                out["range"] = range;
                Ok(out)
            }
            // These return no source coordinates. Unknown methods must be
            // negotiated explicitly instead of leaking generated positions.
            "initialize"
            | "shutdown"
            | "workspace/executeCommand"
            | "workspace/willCreateFiles"
            | "workspace/willRenameFiles"
            | "workspace/willDeleteFiles" => {
                if method.starts_with("workspace/will") {
                    self.workspace_edit(result)
                } else {
                    Ok(result.clone())
                }
            }
            _ => bail!("unsupported editor response mapping for {method}"),
        }
    }
}

pub fn decode_file_uri(uri: &str) -> Result<Option<std::path::PathBuf>> {
    Ok(decode_file_uri_path(uri, cfg!(windows))?.map(std::path::PathBuf::from))
}
fn decode_file_uri_path(uri: &str, windows: bool) -> Result<Option<String>> {
    let Some(rest) = uri.strip_prefix("file://") else {
        return Ok(None);
    };
    let path = if let Some(path) = rest.strip_prefix("localhost/") {
        format!("/{path}")
    } else if windows && !rest.starts_with('/') {
        format!("//{rest}")
    } else {
        rest.to_owned()
    };
    ensure!(
        path.starts_with('/'),
        "remote file URI authorities are not local files"
    );
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            ensure!(i + 2 < bytes.len(), "truncated file URI escape");
            let hex = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            decoded.push(
                hex(bytes[i + 1]).context("invalid file URI escape")? * 16
                    + hex(bytes[i + 2]).context("invalid file URI escape")?,
            );
            i += 3;
        } else {
            ensure!(
                !matches!(bytes[i], b'?' | b'#'),
                "file URI query/fragment unsupported"
            );
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    ensure!(!decoded.contains(&0), "file URI contains NUL");
    let path = String::from_utf8(decoded).context("file URI is not UTF-8")?;
    let path = if windows {
        path.strip_prefix('/')
            .filter(|path| path.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(&path)
            .to_owned()
    } else {
        path
    };
    Ok(Some(path))
}

/// Keep original documentation while replacing the type block whose semantic
/// meaning was changed by a plugin cast. This mirrors ty's hover shape; it does
/// not synthesize documentation from a generated helper declaration.
fn replace_hover_type(value: &mut Value, expression: &str) -> bool {
    fn replace(text: &str, expression: &str, markdown: bool) -> String {
        if markdown && let Some(start) = text.find("```python\n") {
            let body = start + "```python\n".len();
            if let Some(end) = text[body..].find("\n```") {
                return format!("{}{}{}", &text[..body], expression, &text[body + end..]);
            }
        }
        if let Some((_, documentation)) = text.split_once("\n\n") {
            format!("{expression}\n\n{documentation}")
        } else {
            expression.to_owned()
        }
    }
    match value {
        Value::String(text) => {
            *text = replace(text, expression, true);
            true
        }
        Value::Array(items) => items
            .iter_mut()
            .any(|value| replace_hover_type(value, expression)),
        Value::Object(object) => {
            let markdown = object.get("kind").and_then(Value::as_str) == Some("markdown");
            if let Some(Value::String(text)) = object.get_mut("value") {
                *text = replace(text, expression, markdown);
                true
            } else {
                false
            }
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        mapping::{Edits, render},
        model::Source,
    };
    fn range(sl: usize, sc: usize, el: usize, ec: usize) -> Value {
        json!({"start":{"line":sl,"character":sc},"end":{"line":el,"character":ec}})
    }
    fn fixture() -> Result<LspSourceMaps> {
        let source = Source::new(
            "name = '😀'\r\nname\r\n".into(),
            "/source/demo.py".into(),
            "demo".into(),
        );
        let mut edits = Edits::default();
        edits.insert(0, "_ty_ext_helper = 42\n".into());
        let start = source.text.rfind("name").unwrap();
        edits.wrap(
            Span {
                start,
                end: start + 4,
            },
            "cast(str, ".into(),
            ")".into(),
            None,
        );
        let rendered = render(&source, &edits)?;
        let doc = DocumentMap::new(
            "file:///source/demo.py".into(),
            "file:///analysis/demo.py".into(),
            source.text,
            rendered,
        );
        let mut maps = LspSourceMaps::new(PositionEncoding::Utf16, PositionEncoding::Utf32);
        maps.add_generated_root("file:///analysis".into());
        maps.add_hidden_prefix("_ty_ext_".into());
        maps.insert(doc);
        Ok(maps)
    }
    #[test]
    fn encodings_crlf_bom_and_character_boundaries() -> Result<()> {
        let text = "\u{feff}x😀é\r\n雪z\ry\n";
        let after_emoji = text.find('é').unwrap();
        for (encoding, column) in [
            (PositionEncoding::Utf8, 8),
            (PositionEncoding::Utf16, 4),
            (PositionEncoding::Utf32, 3),
        ] {
            assert_eq!(
                position_to_offset(text, &json!({"line":0,"character":column}), encoding)?,
                after_emoji
            );
            assert_eq!(
                offset_to_position(text, after_emoji, encoding)?,
                json!({"line":0,"character":column})
            );
            assert_eq!(
                position_to_offset(text, &json!({"line":1,"character":999}), encoding)?,
                text.find("\ry").unwrap()
            );
            assert_eq!(
                offset_to_position(text, text.find('y').unwrap(), encoding)?,
                json!({"line":2,"character":0})
            );
        }
        assert!(
            position_to_offset(
                text,
                &json!({"line":0,"character":3}),
                PositionEncoding::Utf16
            )
            .is_err()
        );
        assert!(
            position_to_offset(
                text,
                &json!({"line":0,"character":5}),
                PositionEncoding::Utf8
            )
            .is_err()
        );
        assert!(
            position_to_offset(
                text,
                &json!({"line":9,"character":0}),
                PositionEncoding::Utf32
            )
            .is_err()
        );
        assert_eq!(
            offset_to_position(text, text.len(), PositionEncoding::Utf16)?,
            json!({"line":3,"character":0})
        );
        Ok(())
    }
    #[test]
    fn request_and_hover_round_trip_through_wrapped_expression() -> Result<()> {
        let mut maps = fixture()?;
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"},"position":{"line":1,"character":2}});
        let mapped = maps.request("textDocument/hover", &params)?;
        assert_eq!(mapped["textDocument"]["uri"], "file:///analysis/demo.py");
        assert_eq!(mapped["position"], json!({"line":2,"character":12}));
        let response = maps.response(
            "textDocument/hover",
            &params,
            &json!({"contents":{"kind":"markdown","value":"name: str"},"range":range(2,10,2,14)}),
        )?;
        assert_eq!(response["range"], range(1, 0, 1, 4));
        let wrapper = maps.response(
            "textDocument/hover",
            &params,
            &json!({"contents":"str","range":range(2,0,2,15)}),
        )?;
        assert_eq!(wrapper["range"], range(1, 0, 1, 4));
        Ok(())
    }
    #[test]
    fn generated_locations_are_hidden_and_anchored_declarations_navigate() -> Result<()> {
        let source = Source::new(
            "class Book:\n    pass\n".into(),
            "/source/book.py".into(),
            "book".into(),
        );
        let mut edits = Edits::default();
        let start = source.text.find("Book").unwrap();
        edits.insert_with_origin(
            source.text.len(),
            "# generated declaration\nid: int\n".into(),
            Span {
                start,
                end: start + 4,
            },
        );
        let rendered = render(&source, &edits)?;
        let mut maps = fixture()?;
        maps.insert(DocumentMap::new(
            "file:///source/book.py".into(),
            "file:///analysis/book.py".into(),
            source.text,
            rendered,
        ));
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"},"position":{"line":1,"character":0}});
        let result=maps.response("textDocument/definition",&params,&json!([
            {"targetUri":"file:///analysis/book.py","targetRange":range(3,0,3,7),"targetSelectionRange":range(3,0,3,2),"originSelectionRange":range(2,10,2,14)},
            {"uri":"file:///analysis/only_synthetic.pyi","range":range(0,0,0,1)},
            {"uri":"https://example.test/reference","range":range(0,0,0,1)}
        ]))?;
        assert_eq!(result.as_array().unwrap().len(), 2);
        assert_eq!(result[0]["targetUri"], "file:///source/book.py");
        assert_eq!(result[0]["targetSelectionRange"], range(0, 6, 0, 10));
        assert_eq!(result[0]["originSelectionRange"], range(1, 0, 1, 4));
        assert!(maps.workspace_edit(&json!({"changes":{"file:///analysis/book.py":[{"range":range(3,0,3,2),"newText":"pk"}]}})).is_err());
        Ok(())
    }
    #[test]
    fn unsafe_workspace_edit_rejects_whole_transaction() -> Result<()> {
        let maps = fixture()?;
        let upstream = json!({"changes":{"file:///analysis/demo.py":[
            {"range":range(1,0,1,4),"newText":"title"},
            {"range":range(0,0,0,14),"newText":"bad"}
        ]}});
        let original = upstream.clone();
        assert!(
            maps.workspace_edit(&upstream)
                .unwrap_err()
                .to_string()
                .contains("entire edit")
        );
        assert_eq!(upstream, original);
        let safe=maps.workspace_edit(&json!({"changes":{"file:///analysis/demo.py":[{"range":range(2,10,2,14),"newText":"title"}]}}))?;
        assert_eq!(
            safe["changes"]["file:///source/demo.py"][0]["range"],
            range(1, 0, 1, 4)
        );
        // An edit whose endpoints map can still cross a synthesized wrapper.
        assert!(maps.workspace_edit(&json!({"changes":{"file:///analysis/demo.py":[{"range":range(1,0,2,14),"newText":"x"}]}})).is_err());
        Ok(())
    }
    #[test]
    fn completion_resolve_preserves_opaque_data_without_double_mapping() -> Result<()> {
        let mut maps = fixture()?;
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"},"position":{"line":1,"character":2}});
        let backend = json!({"isIncomplete":false,"itemDefaults":{"editRange":range(2,10,2,14),"data":{"uri":"opaque://keep","range":range(77,1,78,2)}},"items":[{"label":"name"},{"label":"_ty_ext_helper"}]});
        let result = maps.response("textDocument/completion", &params, &backend)?;
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        assert!(result.get("itemDefaults").is_none());
        assert_eq!(result["items"][0]["textEdit"]["range"], range(1, 0, 1, 4));
        let upstream = maps.request("completionItem/resolve", &result["items"][0])?;
        assert_eq!(upstream["textEdit"]["range"], range(2, 10, 2, 14));
        assert_eq!(upstream["data"], backend["itemDefaults"]["data"]);
        let resolved = maps.response("completionItem/resolve", &result["items"][0], &upstream)?;
        assert_eq!(resolved["textEdit"]["range"], range(1, 0, 1, 4));
        Ok(())
    }
    #[test]
    fn semantic_tokens_hide_synthetic_tokens_convert_utf16_and_accept_delta() -> Result<()> {
        let mut maps = fixture()?;
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"}});
        // Synthetic prelude, original name, emoji, wrapped original name.
        let backend =
            json!({"resultId":"one","data":[0,0,14,0,0, 1,0,4,1,0, 0,8,1,2,0, 1,10,4,1,0]});
        let result = maps.response("textDocument/semanticTokens/full", &params, &backend)?;
        assert_eq!(
            result["data"],
            json!([0, 0, 4, 1, 0, 0, 8, 2, 2, 0, 1, 0, 4, 1, 0])
        );
        let delta = maps.response(
            "textDocument/semanticTokens/full/delta",
            &json!({"textDocument":{"uri":"file:///source/demo.py"},"previousResultId":"one"}),
            &json!({"resultId":"two","edits":[{"start":18,"deleteCount":1,"data":[9]}]}),
        )?;
        assert_eq!(
            delta["data"],
            json!([0, 0, 4, 1, 0, 0, 8, 2, 2, 0, 1, 0, 4, 9, 0])
        );
        assert!(maps.response("textDocument/semanticTokens/full/delta",&json!({"textDocument":{"uri":"file:///source/demo.py"},"previousResultId":"missing"}),&json!({"edits":[]})).is_err());
        Ok(())
    }
    #[test]
    fn utf16_diagnostics_map_related_locations_and_keep_opaque_data() -> Result<()> {
        let maps = fixture()?;
        let output=maps.diagnostics(&json!({"uri":"file:///analysis/demo.py","version":3,"diagnostics":[
            {"range":range(1,8,1,9),"message":"bad emoji","data":{"range":range(100,0,100,9)},"relatedInformation":[
                {"location":{"uri":"file:///analysis/demo.py","range":range(2,10,2,14)},"message":"name"},
                {"location":{"uri":"file:///analysis/generated_only.py","range":range(0,0,0,2)},"message":"helper"}
            ]}
        ]}))?;
        assert_eq!(output["uri"], "file:///source/demo.py");
        assert_eq!(output["diagnostics"][0]["range"], range(0, 8, 0, 10));
        assert_eq!(
            output["diagnostics"][0]["data"]["range"],
            range(100, 0, 100, 9)
        );
        assert_eq!(
            output["diagnostics"][0]["relatedInformation"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        Ok(())
    }
    #[test]
    fn document_symbols_filter_generated_helpers_and_map_children() -> Result<()> {
        let mut maps = fixture()?;
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"}});
        let output=maps.response("textDocument/documentSymbol",&params,&json!([
            {"name":"_ty_ext_helper","kind":13,"range":range(0,0,0,18),"selectionRange":range(0,0,0,14)},
            {"name":"name","kind":13,"range":range(1,0,2,15),"selectionRange":range(1,0,1,4),"children":[
                {"name":"nested","kind":13,"range":range(2,0,2,15),"selectionRange":range(2,10,2,14)}
            ]}
        ]))?;
        assert_eq!(output.as_array().unwrap().len(), 1);
        assert_eq!(output[0]["range"], range(0, 0, 1, 4));
        assert_eq!(
            output[0]["children"][0]["selectionRange"],
            range(1, 0, 1, 4)
        );
        Ok(())
    }
    #[test]
    fn pure_replacement_is_readable_but_never_editable() -> Result<()> {
        let source = Source::new("book.id\n".into(), "/original.py".into(), "original".into());
        let mut edits = Edits::default();
        edits.replace(Span { start: 0, end: 7 }, "replacement()".into());
        let doc = DocumentMap::new(
            "file:///original.py".into(),
            "file:///generated.py".into(),
            source.text.clone(),
            render(&source, &edits)?,
        );
        assert_eq!(
            doc.to_original_range(
                &range(0, 0, 0, 13),
                PositionEncoding::Utf16,
                PositionEncoding::Utf32,
                false
            )?,
            Some(range(0, 0, 0, 7))
        );
        assert_eq!(
            doc.to_original_range(
                &range(0, 0, 0, 13),
                PositionEncoding::Utf16,
                PositionEncoding::Utf32,
                true
            )?,
            None
        );
        assert_eq!(
            doc.to_generated_position(
                &json!({"line":0,"character":4}),
                PositionEncoding::Utf16,
                PositionEncoding::Utf32
            )?,
            json!({"line":0,"character":0})
        );
        Ok(())
    }
    #[test]
    fn empty_document_and_eof_are_addressable() -> Result<()> {
        let source = Source::new(String::new(), "/empty.py".into(), "empty".into());
        let mut edits = Edits::default();
        edits.insert(0, "helper = 1\n".into());
        let doc = DocumentMap::new(
            "file:///empty.py".into(),
            "file:///generated.py".into(),
            String::new(),
            render(&source, &edits)?,
        );
        let position = json!({"line":0,"character":0});
        assert_eq!(
            doc.to_generated_position(&position, PositionEncoding::Utf16, PositionEncoding::Utf32)?,
            json!({"line":1,"character":0})
        );
        assert_eq!(
            doc.to_original_range(
                &range(1, 0, 1, 0),
                PositionEncoding::Utf16,
                PositionEncoding::Utf32,
                true
            )?,
            Some(range(0, 0, 0, 0))
        );
        Ok(())
    }
    #[test]
    fn file_uris_decode_unicode_and_reject_malformed_escapes() -> Result<()> {
        assert_eq!(
            decode_file_uri("file:///tmp/a%20b/%E9%9B%AA.py")?,
            Some(std::path::PathBuf::from("/tmp/a b/雪.py"))
        );
        assert_eq!(
            decode_file_uri("file://localhost/tmp/a.py")?,
            Some(std::path::PathBuf::from("/tmp/a.py"))
        );
        assert!(decode_file_uri("file:///tmp/bad%Q0").is_err());
        assert!(decode_file_uri("file:///tmp/bad%00").is_err());
        Ok(())
    }

    #[test]
    fn refined_member_hover_keeps_docs_and_does_not_refine_its_receiver() -> Result<()> {
        let source = Source::new(
            "item.status\n".into(),
            "/source/state.py".into(),
            "state".into(),
        );
        let mut edits = Edits::default();
        edits.wrap_with_type(
            Span { start: 0, end: 11 },
            "cast(str, ".into(),
            ")".into(),
            None,
            "str".into(),
        );
        let mut maps = fixture()?;
        maps.insert(DocumentMap::new(
            "file:///source/state.py".into(),
            "file:///analysis/state.py".into(),
            source.text.clone(),
            render(&source, &edits)?,
        ));
        let params = json!({"textDocument":{"uri":"file:///source/state.py"},"position":{"line":0,"character":7}});
        let hover=maps.response("textDocument/hover",&params,&json!({"range":range(0,15,0,21),"contents":{"kind":"markdown","value":"```python\nstr | None\n```\n\nOriginal attribute documentation."}}))?;
        assert_eq!(
            hover["contents"]["value"],
            "```python\nstr\n```\n\nOriginal attribute documentation."
        );
        assert_eq!(hover["range"], range(0, 5, 0, 11));
        let receiver = maps.response(
            "textDocument/hover",
            &params,
            &json!({"range":range(0,10,0,14),"contents":{"kind":"plaintext","value":"Item"}}),
        )?;
        assert_eq!(receiver["contents"]["value"], "Item");
        Ok(())
    }

    #[test]
    fn declared_callable_hover_and_signature_view_preserve_fork_editor_semantics() -> Result<()> {
        let source = Source::new(
            "decode(1)\n".into(),
            "/source/call.py".into(),
            "call".into(),
        );
        let mut edits = Edits::default();
        edits.wrap_callable(
            Span { start: 0, end: 6 },
            "(".into(),
            ", _ty_ext_call_0)[1]".into(),
            "_ty_ext_call_0".into(),
        );
        let rendered = render(&source, &edits)?;
        let rendered_length = rendered.text.len();
        let mut maps = fixture()?;
        maps.add_name("_ty_ext_call_0".into(), "decode".into());
        maps.insert(DocumentMap::new(
            "file:///source/call.py".into(),
            "file:///analysis/call.py".into(),
            source.text,
            rendered,
        ));
        let params = json!({"textDocument":{"uri":"file:///source/call.py"},"position":{"line":0,"character":2}});
        assert_eq!(
            maps.request("textDocument/hover", &params)?["position"],
            json!({"line":0,"character":3})
        );
        assert_eq!(
            maps.request("textDocument/definition", &params)?["position"],
            json!({"line":0,"character":3})
        );
        let response=maps.response("textDocument/hover",&params,&json!({"range":range(0,1,0,7),"contents":{"kind":"markdown","value":"```python\ndef decode(value: object) -> object\n```"}}))?;
        assert_eq!(
            response["contents"]["value"],
            "```python\ndef decode(value: object) -> object\n```"
        );
        assert_eq!(response["range"], range(0, 0, 0, 6));
        let params = json!({"textDocument":{"uri":"file:///source/call.py"},"position":{"line":0,"character":8}});
        let (_, view, mapped) = maps.signature_view(&params)?.unwrap();
        assert_eq!(view.len(), rendered_length);
        assert_eq!(view.replace(' ', ""), "(decode)(1)\n");
        assert_eq!(
            mapped["position"],
            json!({"line":0,"character":view.rfind(')').unwrap()})
        );
        Ok(())
    }

    #[test]
    fn changed_document_rejects_stale_resolve_but_unchanged_refresh_preserves_it() -> Result<()> {
        let mut maps = fixture()?;
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"},"position":{"line":1,"character":0}});
        let result = maps.response(
            "textDocument/completion",
            &params,
            &json!([{"label":"name","textEdit":{"range":range(2,10,2,14),"newText":"name"}}]),
        )?;
        let doc = maps.document("file:///source/demo.py").unwrap().clone();
        maps.clear_documents();
        maps.insert(doc);
        assert!(maps.request("completionItem/resolve", &result[0]).is_ok());
        assert_eq!(
            maps.request_uri("completionItem/resolve", &result[0])?,
            Some("file:///source/demo.py".into())
        );
        let source = Source::new(
            "new_name = 1\n".into(),
            "/source/demo.py".into(),
            "demo".into(),
        );
        maps.insert(DocumentMap::new(
            "file:///source/demo.py".into(),
            "file:///analysis/demo.py".into(),
            source.text.clone(),
            render(&source, &Edits::default())?,
        ));
        let error = maps
            .request("completionItem/resolve", &result[0])
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<crate::checker::ProtocolError>()
                .unwrap()
                .code,
            -32801
        );
        Ok(())
    }

    #[test]
    fn code_action_context_restores_upstream_data_and_original_generated_location() -> Result<()> {
        let mut maps = fixture()?;
        let shown = json!({"range":range(1,0,1,4),"message":"Attribute is incorrect","code":"invalid-assignment"});
        let upstream = json!({"range":range(2,1,2,15),"message":"Invalid assignment to descriptor","code":"invalid-assignment","data":{"fix":42}});
        maps.add_diagnostic("file:///source/demo.py", &shown, upstream.clone());
        let params = json!({"textDocument":{"uri":"file:///source/demo.py"},"range":range(1,0,1,4),"context":{"diagnostics":[shown]}});
        assert_eq!(
            maps.request("textDocument/codeAction", &params)?["context"]["diagnostics"][0],
            upstream
        );
        Ok(())
    }

    #[test]
    fn helper_names_are_scoped_per_document_even_at_identical_offsets() -> Result<()> {
        let mut maps = fixture()?;
        for (module, original) in [("one", "First"), ("two", "Second")] {
            let source = Source::new(
                "value\n".into(),
                format!("/source/{module}.py"),
                module.into(),
            );
            let doc = DocumentMap::new(
                format!("file:///source/{module}.py"),
                format!("file:///analysis/{module}.py"),
                source.text.clone(),
                render(&source, &Edits::default())?,
            )
            .with_names(BTreeMap::from([(
                "_ty_ext_alias_0".into(),
                original.into(),
            )]));
            maps.insert(doc);
        }
        for (module, original) in [("one", "First"), ("two", "Second")] {
            let params = json!({"textDocument":{"uri":format!("file:///source/{module}.py")},"position":{"line":0,"character":1}});
            let result=maps.response("textDocument/hover",&params,&json!({"range":range(0,0,0,5),"contents":{"kind":"markdown","value":"```python\n_ty_ext_alias_0\n```"}}))?;
            assert_eq!(
                result["contents"]["value"],
                format!("```python\n{original}\n```")
            );
        }
        Ok(())
    }

    #[test]
    fn synthetic_cast_parameter_hints_cannot_insert_invalid_user_keywords() -> Result<()> {
        let source = Source::new(
            "item.status\n".into(),
            "/source/state.py".into(),
            "state".into(),
        );
        let mut edits = Edits::default();
        edits.wrap(
            Span { start: 0, end: 11 },
            format!("{}(str, ", source.cast_name()),
            ")".into(),
            None,
        );
        let rendered = render(&source, &edits)?;
        let offset = rendered.text.find("item.status").unwrap();
        let mut maps = fixture()?;
        maps.insert(DocumentMap::new(
            "file:///source/state.py".into(),
            "file:///analysis/state.py".into(),
            source.text,
            rendered,
        ));
        let result=maps.response("textDocument/inlayHint",&json!({"textDocument":{"uri":"file:///source/state.py"}}),&json!([
            {"kind":2,"position":{"line":0,"character":offset},"label":"val=","textEdits":[{"range":range(0,offset,0,offset),"newText":"val="}]}
        ]))?;
        assert_eq!(result, json!([]));
        Ok(())
    }

    #[test]
    fn generated_autoimport_additions_relocate_to_proven_original_import() -> Result<()> {
        let source = Source::new(
            "from typing import assert_type\nvalue = 1\n".into(),
            "/source/imports.py".into(),
            "imports".into(),
        );
        let prefix = "from typing import cast as _ty_ext_cast\n";
        let mut edits = Edits::default();
        edits.insert(0, prefix.into());
        let mut maps = fixture()?;
        maps.insert(DocumentMap::new(
            "file:///source/imports.py".into(),
            "file:///analysis/imports.py".into(),
            source.text.clone(),
            render(&source, &edits)?,
        ));
        let end = prefix.trim_end().len();
        let edit=maps.workspace_edit(&json!({"changes":{"file:///analysis/imports.py":[{"range":range(0,end,0,end),"newText":", Literal"}]}}))?;
        assert_eq!(
            edit["changes"]["file:///source/imports.py"][0],
            json!({"range":range(0,30,0,30),"newText":", Literal"})
        );
        assert!(maps.workspace_edit(&json!({"changes":{"file:///analysis/imports.py":[{"range":range(0,end,0,end),"newText":", Literal; damage()"}]}})).is_err());
        Ok(())
    }

    #[test]
    fn generated_parameter_navigation_uses_original_callable_identity() -> Result<()> {
        let source = Source::new(
            "def decode(value: object) -> object:\n    return value\n".into(),
            "/source/demo.py".into(),
            "demo".into(),
        );
        let prefix = "def _ty_ext_call_0(value: str) -> int: ...\n";
        let mut edits = Edits::default();
        edits.insert(0, prefix.into());
        let mut maps = fixture()?;
        maps.insert(
            DocumentMap::new(
                "file:///source/demo.py".into(),
                "file:///analysis/demo.py".into(),
                source.text.clone(),
                render(&source, &edits)?,
            )
            .with_module("demo".into())
            .with_names(BTreeMap::from([(
                "_ty_ext_call_0".into(),
                "demo.decode".into(),
            )])),
        );
        let start = prefix.find("value").unwrap();
        let location = maps
            .location(
                &json!({"uri":"file:///analysis/demo.py","range":range(0,start,0,start+5)}),
                None,
            )?
            .unwrap();
        assert_eq!(
            location,
            json!({"uri":"file:///source/demo.py","range":range(0,11,0,16)})
        );
        Ok(())
    }

    #[test]
    fn windows_drive_and_unc_uris_are_decoded_without_unix_remote_paths() -> Result<()> {
        assert_eq!(
            decode_file_uri_path("file:///C:/code/a%20b.py", true)?,
            Some("C:/code/a b.py".into())
        );
        assert_eq!(
            decode_file_uri_path("file://server/share/a.py", true)?,
            Some("//server/share/a.py".into())
        );
        assert!(decode_file_uri_path("file://server/share/a.py", false).is_err());
        assert_eq!(decode_file_uri_path("untitled:buffer.py", true)?, None);
        Ok(())
    }

    #[test]
    fn inlay_annotations_replace_synthetic_qualifiers_with_collision_safe_imports() -> Result<()> {
        let source = Source::new("value = 1\n".into(), "/src/cell.py".into(), "cell".into());
        let mut maps = LspSourceMaps::new(PositionEncoding::Utf16, PositionEncoding::Utf32);
        maps.add_hidden_prefix("_ty_ext_".into());
        maps.insert(
            DocumentMap::new(
                "cell:one".into(),
                "file:///analysis/cell.py".into(),
                source.text.clone(),
                render(&source, &Edits::default())?,
            )
            .with_names(BTreeMap::from([("_ty_ext_typing".into(), "typing".into())]))
            .with_namespace("Literal = str\nvalue = 1\n"),
        );
        let edits = maps.inlay_edits(
            "file:///analysis/cell.py",
            &[json!({"range":range(0,5,0,5),"newText":": _ty_ext_typing.Literal[1]"})],
        )?;
        assert_eq!(edits,json!([{"range":range(0,5,0,5),"newText":": _ty_annotation_Literal[1]"},{"range":range(0,0,0,0),"newText":"from typing import Literal as _ty_annotation_Literal\n"}]).as_array().unwrap().clone());
        assert!(maps.inlay_edits("file:///analysis/cell.py",&[json!({"range":range(0,5,0,5),"newText":": _ty_ext_typing.Literal[1]; side_effect()"})]).is_err());
        Ok(())
    }

    #[test]
    fn cli_notes_preserve_original_cross_file_locations_and_hide_helper_targets() -> Result<()> {
        let root = std::env::temp_dir().canonicalize()?;
        let original = root.join("source/demo.py");
        let generated = root.join("generated/demo.py");
        let source = std::sync::Arc::new(Source::new(
            "name\n".into(),
            original.to_string_lossy().into(),
            "demo".into(),
        ));
        let mut edits = Edits::default();
        edits.insert(0, "helper = 1\n".into());
        let document = crate::project_check::ProjectDocument {
            rendered: render(&source, &edits)?,
            source,
            path: generated.clone(),
            names: BTreeMap::from([("_ty_ext_alias".into(), "demo".into())]),
        };
        let mut diagnostics = vec![
            json!({"description":"original primary","location":{"path":original},"lsp_diagnostic":{"range":range(1,0,1,4),"message":"Primary\n\ninfo: _ty_ext_alias.Name was declared here","relatedInformation":[{"message":"declaration","location":{"uri":crate::checker::file_uri(&generated)?,"range":range(1,0,1,4)}},{"message":"helper","location":{"uri":crate::checker::file_uri(&root.join("generated/virtual.pyi"))?,"range":range(0,0,0,1)}}]}}),
        ];
        diagnostics[0]["lsp_diagnostic"]["code"] = json!("test-rule");
        let rich = vec![
            json!({"location":{"path":generated},"lsp_diagnostic":diagnostics[0]["lsp_diagnostic"]}),
        ];
        diagnostics[0]["lsp_diagnostic"]["message"] =
            json!("Custom concise message differing from the editor headline");
        merge_cli_diagnostic_details(&mut diagnostics, &rich, &[document], BTreeMap::new())?;
        assert_eq!(diagnostics[0]["description"], "original primary");
        assert_eq!(
            diagnostics[0]["details"],
            "info: demo.Name was declared here"
        );
        assert_eq!(diagnostics[0]["related"].as_array().unwrap().len(), 1);
        assert_eq!(
            diagnostics[0]["related"][0]["location"],
            json!({"uri":crate::checker::file_uri(&original)?,"range":range(0,0,0,4)})
        );
        Ok(())
    }

    #[test]
    fn inlay_imports_preserve_shebangs_future_imports_and_crlf() -> Result<()> {
        for (text, line) in [
            ("#!/usr/bin/python\r\n# coding: utf-8\r\nvalue = 1\r\n", 2),
            (
                "#!/usr/bin/python\r\n\"\"\"Module docs.\"\"\"\r\nfrom __future__ import annotations\r\nvalue = 1\r\n",
                3,
            ),
        ] {
            let source = Source::new(text.into(), "/src/example.py".into(), "example".into());
            let mut maps = LspSourceMaps::new(PositionEncoding::Utf16, PositionEncoding::Utf32);
            maps.add_hidden_prefix("_ty_ext_".into());
            maps.insert(
                DocumentMap::new(
                    "file:///src/example.py".into(),
                    "file:///analysis/example.py".into(),
                    source.text.clone(),
                    render(&source, &Edits::default())?,
                )
                .with_names(BTreeMap::from([("_ty_ext_typing".into(), "typing".into())])),
            );
            let edits = maps.inlay_edits(
                "file:///analysis/example.py",
                &[json!({"range":range(line,5,line,5),"newText":": _ty_ext_typing.Literal[1]"})],
            )?;
            assert_eq!(edits[1]["newText"], "from typing import Literal\r\n");
            assert_eq!(edits[1]["range"], range(line, 0, line, 0));
        }
        Ok(())
    }
}
