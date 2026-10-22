//! The document store and the toy parser behind the fake language server's
//! `--mode documents` (see `fake_lsp.rs`, which includes this file as a
//! module, so the test crate and the `fake-lsp-server` binary share it).
//!
//! In this mode the fake answers from the text it was sent, the way a real
//! server does, and not from canned ranges:
//!
//! - At `initialized` it walks the workspace root once (the start-up scan of a
//!   server). Files edited on disk afterwards stay invisible until a
//!   notification says so.
//! - `didOpen` / `didChange` (full sync) / `didClose` keep an overlay of open
//!   documents that wins over the disk snapshot; `didChangeWatchedFiles`
//!   re-reads the disk at the moment it arrives. Nothing is read from disk when
//!   a request is answered.
//! - Answers come from a deliberately naive per-line parser: simple names, no
//!   scopes, no types, ASCII fixtures (columns are byte offsets). It is a
//!   fixture for tests, not a language implementation.
//!
//! State files, besides the ones `fake_lsp.rs` already writes: `events.jsonl`
//! (one line per message, in arrival order, with the `spawn` ordinal of the
//! server process) and `documents.json` (the documents the fake answers from,
//! rewritten after every change). They are consistent after a request round
//! trip: a test must not read them right after a notification without a
//! following request.

use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Source extensions the start-up walk and watched-file events pick up.
const EXTENSIONS: &[&str] = &[
    "cs", "ts", "tsx", "js", "py", "rs", "go", "java", "c", "h", "cpp",
];
/// Files the start-up walk reads at most.
const MAX_WALKED_FILES: usize = 1000;
/// Directories the start-up walk never enters (hidden ones are skipped too).
const SKIPPED_DIRECTORIES: &[&str] = &["node_modules", "target"];

/// What the fake answers for an LSP method.
pub enum Outcome {
    /// Not (or not only) a document matter: the caller's standard handling of
    /// `initialize`, `initialized`, `shutdown`, `exit` and `didOpen` follows.
    Pass,
    /// A notification that was consumed here.
    Handled,
    /// The `result` to send for a request.
    Response(Value),
    /// A request this mode does not implement.
    Unsupported,
}

/// The server capabilities of the mode: every provider the fake answers.
pub fn capabilities() -> Value {
    json!({
        "textDocumentSync": {"openClose": true, "change": 1},
        "workspaceSymbolProvider": true,
        "definitionProvider": true,
        "referencesProvider": true,
        "documentSymbolProvider": true,
        "implementationProvider": true,
        "callHierarchyProvider": true,
        "typeHierarchyProvider": true,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Walk,
    Open,
    Watched,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Walk => "walk",
            Self::Open => "open",
            Self::Watched => "watched",
        }
    }
}

struct Doc {
    version: i64,
    source: Source,
    text: String,
    parsed: Parsed,
}

impl Doc {
    fn new(version: i64, source: Source, text: String) -> Self {
        let parsed = parse(&text);
        Self {
            version,
            source,
            text,
            parsed,
        }
    }
}

/// The documents the fake answers from. An open document wins over the disk
/// snapshot (the last walk or watched-file read) of the same uri; closing it
/// brings the snapshot back, or forgets the uri when there is none.
pub struct Store {
    state_dir: Option<PathBuf>,
    root: Option<PathBuf>,
    disk: BTreeMap<String, Doc>,
    open: BTreeMap<String, Doc>,
    seq: u64,
    /// The ordinal of this server process in its state directory (1 for the
    /// first), recorded on every event so a restart is told from a reorder.
    spawn: u64,
    /// `--ignore-watched-files`: `didChangeWatchedFiles` is logged, not applied.
    ignore_watched: bool,
}

impl Store {
    pub fn new(state_dir: Option<PathBuf>, spawn: u64, ignore_watched: bool) -> Self {
        Self {
            state_dir,
            root: None,
            disk: BTreeMap::new(),
            open: BTreeMap::new(),
            seq: 0,
            spawn,
            ignore_watched,
        }
    }

    /// Applies one client message and records it in `events.jsonl`.
    pub fn handle(&mut self, method: &str, params: &Value, is_request: bool) -> Outcome {
        let mut note = None;
        let outcome = match method {
            "initialize" => {
                self.root = params["rootUri"].as_str().and_then(path_for_uri);
                Outcome::Pass
            }
            "initialized" => {
                self.walk();
                Outcome::Pass
            }
            "shutdown" | "exit" => Outcome::Pass,
            "textDocument/didOpen" => {
                note = self.did_open(params);
                Outcome::Pass
            }
            "textDocument/didChange" => {
                note = self.did_change(params);
                Outcome::Handled
            }
            "textDocument/didClose" => {
                note = self.did_close(params);
                Outcome::Handled
            }
            "workspace/didChangeWatchedFiles" => {
                note = self.did_change_watched_files(params);
                Outcome::Handled
            }
            "workspace/symbol" => Outcome::Response(self.workspace_symbols(params)),
            "textDocument/definition" => Outcome::Response(self.definition(params)),
            "textDocument/references" => Outcome::Response(self.references(params)),
            "textDocument/documentSymbol" => Outcome::Response(self.document_symbols(params)),
            "textDocument/implementation" => Outcome::Response(self.implementation(params)),
            "textDocument/prepareCallHierarchy" => {
                Outcome::Response(self.prepare_call_hierarchy(params))
            }
            "callHierarchy/incomingCalls" => Outcome::Response(self.incoming_calls(params)),
            "callHierarchy/outgoingCalls" => Outcome::Response(self.outgoing_calls(params)),
            "textDocument/prepareTypeHierarchy" => {
                Outcome::Response(self.prepare_type_hierarchy(params))
            }
            "typeHierarchy/supertypes" => Outcome::Response(self.supertypes(params)),
            "typeHierarchy/subtypes" => Outcome::Response(self.subtypes(params)),
            _ if is_request => Outcome::Unsupported,
            _ => Outcome::Handled,
        };
        self.log(method, params, note);
        outcome
    }

    // ----- notifications ---------------------------------------------------

    /// The start-up scan: every source file under the root, in path order.
    fn walk(&mut self) {
        let Some(root) = self.root.clone() else {
            return;
        };
        let mut files = Vec::new();
        collect_files(&root, &mut files);
        for path in files {
            if let (Some(uri), Ok(text)) = (uri_for_path(&path), fs::read_to_string(&path)) {
                self.disk.insert(uri, Doc::new(0, Source::Walk, text));
            }
        }
        self.persist();
    }

    /// Opening a document that is already open still applies; the note says so.
    fn did_open(&mut self, params: &Value) -> Option<&'static str> {
        let document = &params["textDocument"];
        let uri = normalize_uri(document["uri"].as_str()?);
        let doc = Doc::new(
            document["version"].as_i64().unwrap_or(0),
            Source::Open,
            document["text"].as_str().unwrap_or_default().to_owned(),
        );
        let reopened = self.open.insert(uri, doc).is_some();
        self.persist();
        reopened.then_some("open-reopened")
    }

    /// Full sync only. A change for a document that is not open, one whose
    /// version does not exceed the stored one, a ranged (incremental) one, one
    /// without `contentChanges` and one without text are ignored, and the note
    /// says which.
    fn did_change(&mut self, params: &Value) -> Option<&'static str> {
        let document = &params["textDocument"];
        let uri = normalize_uri(document["uri"].as_str()?);
        let version = document["version"].as_i64().unwrap_or(0);
        let Some(doc) = self.open.get_mut(&uri) else {
            return Some("change-unopened");
        };
        if version <= doc.version {
            return Some("change-stale-version");
        }
        let Some(change) = params["contentChanges"].get(0) else {
            return Some("change-empty");
        };
        if change.get("range").is_some() {
            return Some("change-incremental-ignored");
        }
        let Some(text) = change["text"].as_str() else {
            return Some("change-no-text");
        };
        *doc = Doc::new(version, Source::Open, text.to_owned());
        self.persist();
        None
    }

    fn did_close(&mut self, params: &Value) -> Option<&'static str> {
        let uri = normalize_uri(params["textDocument"]["uri"].as_str()?);
        let closed = self.open.remove(&uri).is_some();
        self.persist();
        (!closed).then_some("close-unopened")
    }

    /// Created (1) and Changed (2) re-read the disk now; Deleted (3) drops the
    /// snapshot. An open document keeps winning over the snapshot.
    fn did_change_watched_files(&mut self, params: &Value) -> Option<&'static str> {
        if self.ignore_watched {
            return Some("watched-ignored");
        }
        let mut note = None;
        for change in params["changes"].as_array().into_iter().flatten() {
            let Some(uri) = change["uri"].as_str().map(normalize_uri) else {
                continue;
            };
            match change["type"].as_u64() {
                Some(1 | 2) => {
                    let Some(path) = path_for_uri(&uri) else {
                        continue;
                    };
                    if !is_source(&path) {
                        continue;
                    }
                    match fs::read_to_string(&path) {
                        Ok(text) => {
                            self.disk.insert(uri, Doc::new(0, Source::Watched, text));
                        }
                        Err(_) => note = Some("watched-read-failed"),
                    }
                }
                Some(3) => {
                    self.disk.remove(&uri);
                }
                _ => note = Some("watched-unknown-type"),
            }
        }
        self.persist();
        note
    }

    // ----- recording -------------------------------------------------------

    fn log(&mut self, method: &str, params: &Value, note: Option<&str>) {
        self.seq += 1;
        let Some(dir) = &self.state_dir else {
            return;
        };
        let mut event = json!({"spawn": self.spawn, "seq": self.seq, "method": method});
        if let Some(query) = params["query"].as_str() {
            event["query"] = json!(query);
        }
        if params["item"].is_object() {
            event["item"] = json!({
                "name": params["item"]["name"],
                "uri": params["item"]["uri"].as_str().map(normalize_uri),
            });
        }
        if let Some(uri) = params["textDocument"]["uri"].as_str() {
            event["uri"] = json!(normalize_uri(uri));
        }
        if let Some(version) = params["textDocument"]["version"].as_i64() {
            event["version"] = json!(version);
        }
        if method == "workspace/didChangeWatchedFiles" {
            let changes: Vec<Value> = params["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|change| {
                    json!({
                        "uri": change["uri"].as_str().map(normalize_uri),
                        "type": change["type"],
                    })
                })
                .collect();
            event["changes"] = json!(changes);
        }
        if let Some(note) = note {
            event["note"] = json!(note);
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("events.jsonl"))
        {
            // One `write_all` of the whole line, like the other state files.
            let _ = file.write_all(format!("{event}\n").as_bytes());
        }
    }

    /// Rewrites `documents.json`: `uri -> {version, source, lines, hash}` of
    /// the documents the fake answers from.
    fn persist(&self) {
        let Some(dir) = &self.state_dir else {
            return;
        };
        let documents: serde_json::Map<String, Value> = self
            .docs()
            .into_iter()
            .map(|(uri, doc)| {
                (
                    uri.to_owned(),
                    json!({
                        "version": doc.version,
                        "source": doc.source.label(),
                        "lines": doc.text.lines().count(),
                        "hash": format!("{:016x}", fnv1a(doc.text.as_bytes())),
                    }),
                )
            })
            .collect();
        let temporary = dir.join("documents.json.tmp");
        if fs::write(&temporary, Value::Object(documents).to_string()).is_ok() {
            let _ = fs::rename(&temporary, dir.join("documents.json"));
        }
    }

    // ----- the documents the fake answers from -----------------------------

    /// Open text wins over the disk snapshot; uris in order.
    fn docs(&self) -> BTreeMap<&str, &Doc> {
        let mut docs = BTreeMap::new();
        for (uri, doc) in self.disk.iter().chain(self.open.iter()) {
            docs.insert(uri.as_str(), doc);
        }
        docs
    }

    fn doc(&self, uri: &str) -> Option<&Doc> {
        self.open.get(uri).or_else(|| self.disk.get(uri))
    }

    /// Every declaration of the store (types and functions), by uri and
    /// position.
    fn decls(&self) -> Vec<Decl<'_>> {
        let mut all = Vec::new();
        for (uri, doc) in self.docs() {
            let mut local: Vec<Decl<'_>> = doc
                .parsed
                .types
                .iter()
                .map(|decl| Decl {
                    uri,
                    tok: &decl.tok,
                    end: decl.end,
                    kind: decl.kind,
                    container: None,
                    is_type: true,
                })
                .chain(doc.parsed.fns.iter().map(|decl| Decl {
                    uri,
                    tok: &decl.tok,
                    end: decl.end,
                    kind: decl.kind,
                    container: decl.container.as_deref(),
                    is_type: false,
                }))
                .collect();
            local.sort_by_key(|decl| (decl.tok.line, decl.tok.start));
            all.extend(local);
        }
        all
    }

    /// `(subtype, supertype)` pairs of the whole store: declared supertype
    /// lists and `impl A for B` (B: A).
    fn relations(&self) -> Vec<(&str, &str)> {
        let mut pairs = Vec::new();
        for doc in self.docs().into_values() {
            for decl in &doc.parsed.types {
                for sup in &decl.supers {
                    pairs.push((decl.tok.name.as_str(), sup.as_str()));
                }
            }
            for (sub, sup) in &doc.parsed.impls {
                pairs.push((sub.as_str(), sup.as_str()));
            }
        }
        pairs
    }

    /// The position of a request as `(uri, line, character)`.
    fn position(params: &Value) -> Option<(String, u32, u32)> {
        let uri = normalize_uri(params["textDocument"]["uri"].as_str()?);
        let line = u32::try_from(params["position"]["line"].as_u64()?).ok()?;
        let character = u32::try_from(params["position"]["character"].as_u64()?).ok()?;
        Some((uri, line, character))
    }

    /// The identifier under the position of a request.
    fn identifier_at(&self, params: &Value) -> Option<(String, &Tok)> {
        let (uri, line, character) = Self::position(params)?;
        let doc = self.doc(&uri)?;
        let tok = token_at(&doc.parsed.idents, line, character)?;
        Some((uri, tok))
    }

    // ----- requests --------------------------------------------------------

    fn workspace_symbols(&self, params: &Value) -> Value {
        let query = params["query"].as_str().unwrap_or_default().to_lowercase();
        let symbols: Vec<Value> = self
            .decls()
            .iter()
            .filter(|decl| decl.tok.name.to_lowercase().contains(&query))
            .map(|decl| decl.symbol(false))
            .collect();
        json!(symbols)
    }

    fn definition(&self, params: &Value) -> Value {
        let Some((_, tok)) = self.identifier_at(params) else {
            return json!([]);
        };
        let found: Vec<Value> = self
            .decls()
            .iter()
            .filter(|decl| decl.tok.name == tok.name)
            .map(Decl::location)
            .collect();
        json!(found)
    }

    fn references(&self, params: &Value) -> Value {
        let Some((_, tok)) = self.identifier_at(params) else {
            return json!([]);
        };
        let include_declaration = params["context"]["includeDeclaration"]
            .as_bool()
            .unwrap_or(false);
        let mut found = Vec::new();
        for (uri, doc) in self.docs() {
            for ident in doc.parsed.idents.iter().filter(|i| i.name == tok.name) {
                let declaration = doc
                    .parsed
                    .types
                    .iter()
                    .map(|decl| &decl.tok)
                    .chain(doc.parsed.fns.iter().map(|decl| &decl.tok))
                    .any(|decl| decl.line == ident.line && decl.start == ident.start);
                if include_declaration || !declaration {
                    found.push(location(uri, ident));
                }
            }
        }
        json!(found)
    }

    fn document_symbols(&self, params: &Value) -> Value {
        let Some(uri) = params["textDocument"]["uri"].as_str().map(normalize_uri) else {
            return json!([]);
        };
        let symbols: Vec<Value> = self
            .decls()
            .iter()
            .filter(|decl| decl.uri == uri)
            .map(|decl| decl.symbol(true))
            .collect();
        json!(symbols)
    }

    /// At a function declaration inside a type `C`: the same-named function
    /// declarations inside the types that are subtypes of `C`, in document
    /// order (a real server answers an interface member the same way). At
    /// anything else: the types whose supertypes name the type under the
    /// position.
    fn implementation(&self, params: &Value) -> Value {
        let Some((uri, tok)) = self.identifier_at(params) else {
            return json!([]);
        };
        let member_of = self.doc(&uri).and_then(|doc| {
            doc.parsed
                .fns
                .iter()
                .find(|decl| decl.tok.line == tok.line && decl.tok.start == tok.start)
                .and_then(|decl| decl.container.as_deref())
        });
        if let Some(container) = member_of {
            let relations = self.relations();
            let found: Vec<Value> = self
                .decls()
                .iter()
                .filter(|decl| {
                    !decl.is_type
                        && decl.tok.name == tok.name
                        && decl
                            .container
                            .is_some_and(|sub| relations.contains(&(sub, container)))
                })
                .map(Decl::location)
                .collect();
            return json!(found);
        }
        let found: Vec<Value> = self
            .subtypes_of(&tok.name)
            .iter()
            .map(Decl::location)
            .collect();
        json!(found)
    }

    fn subtypes_of(&self, name: &str) -> Vec<Decl<'_>> {
        let relations = self.relations();
        self.decls()
            .into_iter()
            .filter(|decl| decl.is_type && relations.contains(&(decl.tok.name.as_str(), name)))
            .collect()
    }

    fn supertypes_of(&self, name: &str) -> Vec<Decl<'_>> {
        let relations = self.relations();
        self.decls()
            .into_iter()
            .filter(|decl| decl.is_type && relations.contains(&(name, decl.tok.name.as_str())))
            .collect()
    }

    /// The function declared at the position; else, for an identifier at the
    /// position (a call site), the function declarations named like it (simple
    /// names, as for `definition`). A position on no identifier, or a name no
    /// function has, gets an empty list.
    fn prepare_call_hierarchy(&self, params: &Value) -> Value {
        let Some((uri, line, character)) = Self::position(params) else {
            return json!([]);
        };
        let Some(doc) = self.doc(&uri) else {
            return json!([]);
        };
        if let Some(function) = doc
            .parsed
            .fns
            .iter()
            .find(|decl| contains(&decl.tok, line, character))
        {
            return json!([hierarchy_item(&uri, &function.tok, function.kind)]);
        }
        let Some(tok) = token_at(&doc.parsed.idents, line, character) else {
            return json!([]);
        };
        let items: Vec<Value> = self
            .decls()
            .iter()
            .filter(|decl| !decl.is_type && decl.tok.name == tok.name)
            .map(Decl::item)
            .collect();
        json!(items)
    }

    /// Call sites of the item's name, grouped by the function that holds them.
    fn incoming_calls(&self, params: &Value) -> Value {
        let Some(name) = params["item"]["name"].as_str() else {
            return json!([]);
        };
        let mut groups: Vec<(String, &FnDecl, Vec<Value>)> = Vec::new();
        for (uri, doc) in self.docs() {
            for call in doc.parsed.calls.iter().filter(|call| call.name == name) {
                let Some(caller) = enclosing(doc, call.line, call.start) else {
                    continue;
                };
                let same = |group: &&mut (String, &FnDecl, Vec<Value>)| {
                    group.0 == uri
                        && group.1.tok.line == caller.tok.line
                        && group.1.tok.start == caller.tok.start
                };
                match groups.iter_mut().find(same) {
                    Some(group) => group.2.push(range(call)),
                    None => groups.push((uri.to_owned(), caller, vec![range(call)])),
                }
            }
        }
        let calls: Vec<Value> = groups
            .iter()
            .map(|(uri, caller, ranges)| {
                json!({
                    "from": hierarchy_item(uri, &caller.tok, caller.kind),
                    "fromRanges": ranges,
                })
            })
            .collect();
        json!(calls)
    }

    /// Calls inside the item's function, resolved to function declarations.
    fn outgoing_calls(&self, params: &Value) -> Value {
        let item = &params["item"];
        let (Some(name), Some(uri)) = (item["name"].as_str(), item["uri"].as_str()) else {
            return json!([]);
        };
        let uri = normalize_uri(uri);
        let line = item["selectionRange"]["start"]["line"].as_u64();
        let start = item["selectionRange"]["start"]["character"].as_u64();
        let Some(doc) = self.doc(&uri) else {
            return json!([]);
        };
        let Some(function) = doc.parsed.fns.iter().find(|decl| {
            decl.tok.name == name
                && line.is_none_or(|line| u64::from(decl.tok.line) == line)
                && start.is_none_or(|start| u64::from(decl.tok.start) == start)
        }) else {
            return json!([]);
        };
        let decls = self.decls();
        let mut groups: Vec<(&Decl<'_>, Vec<Value>)> = Vec::new();
        for call in &doc.parsed.calls {
            let inside = enclosing(doc, call.line, call.start).is_some_and(|holder| {
                holder.tok.line == function.tok.line && holder.tok.start == function.tok.start
            });
            if !inside {
                continue;
            }
            for target in decls
                .iter()
                .filter(|decl| !decl.is_type && decl.tok.name == call.name)
            {
                let same = |group: &&mut (&Decl<'_>, Vec<Value>)| {
                    group.0.uri == target.uri
                        && group.0.tok.line == target.tok.line
                        && group.0.tok.start == target.tok.start
                };
                match groups.iter_mut().find(same) {
                    Some(group) => group.1.push(range(call)),
                    None => groups.push((target, vec![range(call)])),
                }
            }
        }
        let calls: Vec<Value> = groups
            .iter()
            .map(|(target, ranges)| {
                json!({
                    "to": hierarchy_item(target.uri, target.tok, target.kind),
                    "fromRanges": ranges,
                })
            })
            .collect();
        json!(calls)
    }

    /// The types named like the identifier under the position.
    fn prepare_type_hierarchy(&self, params: &Value) -> Value {
        let Some((_, tok)) = self.identifier_at(params) else {
            return json!([]);
        };
        let items: Vec<Value> = self
            .decls()
            .iter()
            .filter(|decl| decl.is_type && decl.tok.name == tok.name)
            .map(Decl::item)
            .collect();
        json!(items)
    }

    fn supertypes(&self, params: &Value) -> Value {
        let Some(name) = params["item"]["name"].as_str() else {
            return json!([]);
        };
        let items: Vec<Value> = self.supertypes_of(name).iter().map(Decl::item).collect();
        json!(items)
    }

    fn subtypes(&self, params: &Value) -> Value {
        let Some(name) = params["item"]["name"].as_str() else {
            return json!([]);
        };
        let items: Vec<Value> = self.subtypes_of(name).iter().map(Decl::item).collect();
        json!(items)
    }
}

// ----- answers' shapes -------------------------------------------------------

struct Decl<'a> {
    uri: &'a str,
    tok: &'a Tok,
    /// Where the declaration ends: (line, byte column after its last token).
    end: (u32, u32),
    kind: u32,
    container: Option<&'a str>,
    is_type: bool,
}

impl Decl<'_> {
    fn location(&self) -> Value {
        location(self.uri, self.tok)
    }

    /// A flat `SymbolInformation`. `whole`: its range runs from the name to the
    /// end of the declaration (as `documentSymbol` answers it); else it is the
    /// name token (`workspace/symbol`).
    fn symbol(&self, whole: bool) -> Value {
        let location = if whole {
            json!({"uri": self.uri, "range": {
                "start": {"line": self.tok.line, "character": self.tok.start},
                "end": {"line": self.end.0, "character": self.end.1},
            }})
        } else {
            self.location()
        };
        let mut symbol = json!({
            "name": self.tok.name,
            "kind": self.kind,
            "location": location,
        });
        if let Some(container) = self.container {
            symbol["containerName"] = json!(container);
        }
        symbol
    }

    /// A `TypeHierarchyItem`.
    fn item(&self) -> Value {
        hierarchy_item(self.uri, self.tok, self.kind)
    }
}

fn range(tok: &Tok) -> Value {
    json!({
        "start": {"line": tok.line, "character": tok.start},
        "end": {"line": tok.line, "character": tok.end},
    })
}

fn location(uri: &str, tok: &Tok) -> Value {
    json!({"uri": uri, "range": range(tok)})
}

/// A `CallHierarchyItem` or `TypeHierarchyItem` (the two share their shape).
fn hierarchy_item(uri: &str, tok: &Tok, kind: u32) -> Value {
    json!({
        "name": tok.name,
        "kind": kind,
        "uri": uri,
        "range": range(tok),
        "selectionRange": range(tok),
    })
}

// ----- uris and files --------------------------------------------------------

fn is_source(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            EXTENSIONS
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
}

fn path_for_uri(uri: &str) -> Option<PathBuf> {
    reqwest::Url::parse(uri).ok()?.to_file_path().ok()
}

fn uri_for_path(path: &Path) -> Option<String> {
    reqwest::Url::from_file_path(path).ok().map(String::from)
}

/// The spelling the fake stores a document under, so the uri of the walk and
/// the one a client sends meet (a uri that is no file uri stays as it is).
fn normalize_uri(uri: &str) -> String {
    path_for_uri(uri)
        .and_then(|path| uri_for_path(&path))
        .unwrap_or_else(|| uri.to_owned())
}

/// Source files under `dir` in path order, depth first, up to the limit.
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        if out.len() >= MAX_WALKED_FILES {
            return;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if !name.starts_with('.') && !SKIPPED_DIRECTORIES.contains(&name.as_str()) {
                collect_files(&path, out);
            }
        } else if kind.is_file() && is_source(&path) {
            out.push(path);
        }
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

// ----- the toy parser --------------------------------------------------------

/// An identifier token: a line, and byte columns `[start, end)`.
#[derive(Clone)]
struct Tok {
    name: String,
    line: u32,
    start: u32,
    end: u32,
}

struct TypeDecl {
    tok: Tok,
    end: (u32, u32),
    kind: u32,
    supers: Vec<String>,
}

struct FnDecl {
    tok: Tok,
    end: (u32, u32),
    /// 6 (method) after a type declaration of the document, else 12.
    kind: u32,
    container: Option<String>,
}

/// Everything the answers need from one document.
#[derive(Default)]
struct Parsed {
    /// Every identifier, in document order.
    idents: Vec<Tok>,
    types: Vec<TypeDecl>,
    fns: Vec<FnDecl>,
    /// `NAME(` that is not a function declaration.
    calls: Vec<Tok>,
    /// `impl A for B` as `(B, A)`.
    impls: Vec<(String, String)>,
}

/// Words before `(` that are statements or operators, never a name.
const NOT_A_NAME: &[&str] = &[
    "if", "for", "foreach", "while", "switch", "catch", "using", "lock", "return", "sizeof",
    "typeof", "nameof", "fixed", "when", "match", "await", "throw", "fn", "def", "function",
];
/// Words before a name that make `NAME(` an expression, not a declaration.
const NOT_A_TYPE: &[&str] = &[
    "return", "new", "await", "throw", "else", "case", "in", "is", "as", "not", "yield", "do",
    "typeof", "sizeof", "nameof",
];

fn type_kind(word: &str) -> Option<u32> {
    match word {
        "class" => Some(5),
        "interface" | "trait" => Some(11),
        "struct" => Some(23),
        "enum" => Some(10),
        _ => None,
    }
}

fn identifiers(line: &str, line_number: u32) -> Vec<Tok> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphabetic() || bytes[index] == b'_' {
            let start = index;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            out.push(Tok {
                name: line[start..index].to_owned(),
                line: line_number,
                start: start as u32,
                end: index as u32,
            });
        } else {
            index += 1;
        }
    }
    out
}

/// The identifiers after `:`, `extends` or `implements`, up to the first `{`.
fn supertypes(line: &str, from: usize) -> Vec<String> {
    let rest = &line[from..];
    let rest = rest.split('{').next().unwrap_or(rest);
    let toks = identifiers(rest, 0);
    let keyword = toks
        .iter()
        .find(|tok| matches!(tok.name.as_str(), "extends" | "implements"))
        .map(|tok| tok.start as usize);
    let Some(marker) = [rest.find(':'), keyword].into_iter().flatten().min() else {
        return Vec::new();
    };
    toks.into_iter()
        .filter(|tok| {
            tok.start as usize > marker
                && !matches!(
                    tok.name.as_str(),
                    "extends" | "implements" | "where" | "new" | "class" | "struct"
                )
        })
        .map(|tok| tok.name)
        .collect()
}

/// `impl Trait for Type` (generics after `impl` skipped) as `(Type, Trait)`.
fn impl_pair(line: &str, toks: &[Tok], at: usize) -> Option<(String, String)> {
    let mut position = toks[at].end as usize;
    if line[position..].starts_with('<') {
        let mut depth = 0;
        for (offset, character) in line[position..].char_indices() {
            match character {
                '<' => depth += 1,
                '>' => {
                    depth -= 1;
                    if depth == 0 {
                        position += offset + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    let tr = toks.iter().find(|tok| tok.start as usize >= position)?;
    let for_at = toks
        .iter()
        .position(|tok| tok.name == "for" && tok.start > tr.end)?;
    let ty = toks.get(for_at + 1)?;
    Some((ty.name.clone(), tr.name.clone()))
}

/// Whether the parameter list that starts `after` (at its `(`) is followed by
/// `;` before any body or initializer: `double Area();`, `fn area(&self) -> f64;`.
fn ends_without_body(after: &str) -> bool {
    let mut depth = 0;
    let mut close = None;
    for (index, character) in after.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return false;
    };
    let rest = &after[close + 1..];
    rest.find([';', '{', '='])
        .is_some_and(|index| rest.as_bytes()[index] == b';')
}

/// Whether the name at `toks[at]`, directly followed by `(`, declares a
/// function: preceded by `fn`/`def`/`function` or by a type-like identifier
/// (`int Add(`, `Task<int> Run(`), and followed on the line by `{`, `=>`, a
/// final `:`, or a parameter list that ends in `;` (a bodiless member).
fn declares_function(line: &str, toks: &[Tok], at: usize) -> bool {
    let tok = &toks[at];
    let Some(previous) = at.checked_sub(1).map(|index| &toks[index]) else {
        return false;
    };
    let between = &line[previous.end as usize..tok.start as usize];
    let prefixed = if matches!(previous.name.as_str(), "fn" | "def" | "function") {
        between.trim().is_empty()
    } else if NOT_A_TYPE.contains(&previous.name.as_str()) {
        false
    } else {
        between.chars().all(|c| " \t<>[]?*&,".contains(c))
    };
    let after = &line[tok.end as usize..];
    prefixed
        && (after.contains('{')
            || after.contains("=>")
            || after.trim_end().ends_with(':')
            || ends_without_body(after))
}

/// Where the declaration whose name is `tok` ends: after the `}` that closes
/// its first `{` (counting braces from the name on), or, when a `;` comes
/// before any `{` (a bodiless or `=>` member), after that `;`. The end of the
/// text when neither comes.
fn declaration_end(lines: &[&str], tok: &Tok) -> (u32, u32) {
    let mut depth = 0u32;
    let mut opened = false;
    for (number, line) in lines.iter().enumerate().skip(tok.line as usize) {
        let from = if number == tok.line as usize {
            tok.end as usize
        } else {
            0
        };
        for (offset, character) in line.bytes().enumerate().skip(from) {
            match character {
                b'{' => {
                    depth += 1;
                    opened = true;
                }
                b'}' if opened => {
                    depth -= 1;
                    if depth == 0 {
                        return (number as u32, offset as u32 + 1);
                    }
                }
                b';' if !opened => return (number as u32, offset as u32 + 1),
                _ => {}
            }
        }
    }
    let last = lines.len().saturating_sub(1);
    (
        last as u32,
        lines.get(last).map_or(0, |line| line.len() as u32),
    )
}

fn parse(text: &str) -> Parsed {
    let mut parsed = Parsed::default();
    let lines: Vec<&str> = text.lines().collect();
    for (number, line) in text.lines().enumerate() {
        let toks = identifiers(line, number as u32);
        for (at, tok) in toks.iter().enumerate() {
            if let Some(kind) = type_kind(&tok.name)
                && let Some(name) = toks.get(at + 1)
                && type_kind(&name.name).is_none()
                && name.name != "new"
                && line[tok.end as usize..name.start as usize]
                    .trim()
                    .is_empty()
            {
                parsed.types.push(TypeDecl {
                    tok: name.clone(),
                    end: declaration_end(&lines, name),
                    kind,
                    supers: supertypes(line, name.end as usize),
                });
            }
            if tok.name == "impl"
                && let Some(pair) = impl_pair(line, &toks, at)
            {
                parsed.impls.push(pair);
            }
            if line[tok.end as usize..].starts_with('(') && !NOT_A_NAME.contains(&tok.name.as_str())
            {
                if declares_function(line, &toks, at) {
                    parsed.fns.push(FnDecl {
                        tok: tok.clone(),
                        end: declaration_end(&lines, tok),
                        kind: 12,
                        container: None,
                    });
                } else {
                    parsed.calls.push(tok.clone());
                }
            }
        }
        parsed.idents.extend(toks);
    }
    for function in &mut parsed.fns {
        let holder = parsed.types.iter().rev().find(|decl| {
            (decl.tok.line, decl.tok.start) <= (function.tok.line, function.tok.start)
        });
        if let Some(holder) = holder {
            function.kind = 6;
            function.container = Some(holder.tok.name.clone());
        }
    }
    parsed
}

/// Whether the position touches the token (its end counts, as a cursor placed
/// right after a name does).
fn contains(tok: &Tok, line: u32, character: u32) -> bool {
    tok.line == line && tok.start <= character && character <= tok.end
}

fn token_at(idents: &[Tok], line: u32, character: u32) -> Option<&Tok> {
    idents
        .iter()
        .find(|tok| tok.line == line && tok.start <= character && character < tok.end)
        .or_else(|| idents.iter().find(|tok| contains(tok, line, character)))
}

/// The nearest function declaration at or before the position.
fn enclosing(doc: &Doc, line: u32, character: u32) -> Option<&FnDecl> {
    doc.parsed
        .fns
        .iter()
        .rev()
        .find(|decl| (decl.tok.line, decl.tok.start) <= (line, character))
}
