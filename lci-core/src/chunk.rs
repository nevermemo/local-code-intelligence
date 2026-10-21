use crate::{language, workspace::hash};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
};
use tree_sitter::{Node, Parser};

const TARGET_BYTES: usize = 6000;
const MAX_DECLARATION_BYTES: usize = 24000;
/// Recursion frames of `ranges`, not syntactic levels: a nested Rust `mod`
/// costs two (`mod_item`, `declaration_list`), so about 64 nested modules.
const MAX_RANGE_DEPTH: usize = 128;
pub const DOCUMENT_FORMAT_VERSION: &str = "rust-chunks-v1";

#[derive(Debug)]
pub struct Scan {
    pub chunks: Vec<Chunk>,
    pub files: HashMap<String, crate::manifest::CachedFile>,
    pub fingerprint: String,
    pub parsed_files: usize,
    pub unchanged_files: usize,
    pub removed_files: usize,
    /// Repository-relative paths of files that were not valid UTF-8 and were
    /// indexed from a lossy decoding (invalid bytes become U+FFFD).
    pub lossy_files: Vec<String>,
    /// Files with a registered source extension that were left out because
    /// their content is a known non-source format (for example a Qt Linguist
    /// catalog saved as `.ts`). They are neither parsed, chunked, hashed nor
    /// recorded in the manifest, so a file that stops being source disappears
    /// from the next snapshot like a deleted one, and the staleness
    /// fingerprint ignores them.
    pub skipped_files: Vec<SkippedFile>,
}

/// A file with a source extension that is not indexed as source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    /// Repository-relative path with `/` separators.
    pub relative_path: String,
    pub reason: language::NonSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub relative_file_path: String,
    pub language: String,
    pub start_line: u32,
    pub end_line: u32,
    pub code: String,
    pub content_hash: String,
}

// Recurse through oversized declarations to syntax boundaries (including match arms).
// A large leaf remains whole: no arbitrary text windows or broken UTF-8.
fn ranges(
    adapter: &language::LanguageAdapter,
    node: Node<'_>,
    out: &mut Vec<(usize, usize)>,
    depth: usize,
) {
    // Keep pathological nesting whole rather than exhausting a blocking worker's
    // stack; such a range can exceed MAX_DECLARATION_BYTES, which the embedder
    // already handles by halving rejected inputs.
    if depth >= MAX_RANGE_DEPTH {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    let container = adapter.is_container(node.kind());
    let boundary = adapter.is_boundary(node.kind());
    let declaration = adapter.is_declaration(node.kind());
    if !container
        && !boundary
        && node.end_byte() - node.start_byte()
            <= if declaration {
                MAX_DECLARATION_BYTES
            } else {
                TARGET_BYTES
            }
    {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    let mut cursor = node.walk();
    let children: Vec<_> = node.named_children(&mut cursor).collect();
    if children.is_empty() {
        out.push((node.start_byte(), node.end_byte()));
        return;
    }
    // Prefix children (comments, attributes) attach to the declaration after
    // them. A run after the last declaration has none to attach to; when it is
    // longer than the target its children are packed like any other child
    // instead of staying whole in the chunk before it.
    let trailing_prefix_start = children
        .iter()
        .rposition(|child| !adapter.is_prefix(child.kind()))
        .map_or(0, |index| index + 1);
    let split_trailing_prefix = children
        .get(trailing_prefix_start)
        .zip(children.last())
        .is_some_and(|(first, last)| last.end_byte() - first.start_byte() > TARGET_BYTES);
    let mut start = node.start_byte();
    let mut end = start;
    let mut pending_prefix = None;
    let mut last_split_range = None;
    for (index, child) in children.into_iter().enumerate() {
        if container
            && adapter.is_prefix(child.kind())
            && (!split_trailing_prefix || index < trailing_prefix_start)
        {
            pending_prefix.get_or_insert(child.start_byte());
            continue;
        }
        let child_start = pending_prefix.take().unwrap_or(child.start_byte());
        let child_limit = if adapter.is_declaration(child.kind()) {
            MAX_DECLARATION_BYTES
        } else {
            TARGET_BYTES
        };
        let split = child.end_byte() - child.start_byte() > child_limit
            || (adapter.is_container(child.kind()) && child.kind() != "source_file")
            || adapter.is_boundary(child.kind());
        if split {
            let attach_header = adapter.attaches_header_to_child(node.kind(), child.kind());
            if child_start > start && !attach_header {
                out.push((start, child_start));
            }
            let first = out.len();
            ranges(adapter, child, out, depth + 1);
            if let Some(range) = out.get_mut(first) {
                range.0 = if attach_header { start } else { child_start };
            }
            last_split_range = out.len().checked_sub(1).filter(|&index| index >= first);
            start = child.end_byte();
            end = start;
        } else if container || child.end_byte() - start > TARGET_BYTES {
            // The chunk reaches the next child's start, so the bytes between two
            // children (an operator, a comma) stay in a chunk: after a pending
            // run, in it; right after a split child, in that child's last chunk.
            if end > start {
                out.push((start, child_start));
            } else if let Some(index) = last_split_range.take()
                && let Some((_, previous_end)) = out.get_mut(index)
                && *previous_end == start
            {
                *previous_end = child_start;
            }
            last_split_range = None;
            start = child_start;
            end = child.end_byte();
        } else {
            last_split_range = None;
            end = child.end_byte();
        }
    }
    if node.end_byte() > start {
        out.push((start, node.end_byte()));
    }
}

pub fn rust_chunks(path: &str, source: &str) -> Result<Vec<Chunk>> {
    let adapter = language::for_extension("rs").expect("Rust adapter must be registered");
    chunks_with_adapter(path, source, adapter)
}

pub fn chunks_for_path(path: &str, source: &str) -> Result<Vec<Chunk>> {
    let Some(adapter) = language::for_path(Path::new(path)) else {
        bail!("unsupported source extension: {path}");
    };
    chunks_with_adapter(path, source, adapter)
}

fn chunks_with_adapter(
    path: &str,
    source: &str,
    adapter: &language::LanguageAdapter,
) -> Result<Vec<Chunk>> {
    Ok(extract(path, source, adapter, false)?.chunks)
}

/// What one parse of a file yields (the extraction hook of the multimap
/// spike, slice S3): the file's chunks, exactly as [`chunks_for_path`] gives
/// them, and its declarations where the language has an extractor (Rust
/// only so far; `None` for every other language).
#[derive(Debug, Clone)]
pub struct Extraction {
    pub chunks: Vec<Chunk>,
    pub declarations: Option<Vec<crate::node::Declaration>>,
}

/// [`chunks_for_path`] plus the declarations, from one parse.
pub fn extract_for_path(path: &str, source: &str) -> Result<Extraction> {
    let Some(adapter) = language::for_path(Path::new(path)) else {
        bail!("unsupported source extension: {path}");
    };
    extract_with_adapter(path, source, adapter)
}

/// [`chunks_with_adapter`] plus the declarations, from one parse (the
/// extraction hook).
pub fn extract_with_adapter(
    path: &str,
    source: &str,
    adapter: &language::LanguageAdapter,
) -> Result<Extraction> {
    extract(path, source, adapter, true)
}

/// The one chunker: picks the adapter's extractor. Code is parsed and cut
/// along its syntax tree; prose goes through its own splitter and has no
/// declarations.
fn extract(
    path: &str,
    source: &str,
    adapter: &language::LanguageAdapter,
    with_declarations: bool,
) -> Result<Extraction> {
    match adapter.extractor() {
        language::Extractor::TreeSitter { .. } => {
            extract_tree_sitter(path, source, adapter, with_declarations)
        }
        language::Extractor::Prose(language::ProseKind::Markdown) => Ok(Extraction {
            chunks: markdown::chunks(path, source, adapter.identifier()),
            declarations: None,
        }),
    }
}

/// The Tree-sitter chunker: parses `source`, cuts the tree into chunks, and
/// walks the declarations only when `with_declarations` asks for them (and
/// the language has an extractor), so a caller that wants chunks alone pays
/// for no declaration walk.
fn extract_tree_sitter(
    path: &str,
    source: &str,
    adapter: &language::LanguageAdapter,
    with_declarations: bool,
) -> Result<Extraction> {
    let mut parser = Parser::new();
    parser.set_language(&adapter.grammar())?;
    let tree = parser
        .parse(source, None)
        .context("Tree-sitter parse cancelled")?;
    let mut spans = Vec::new();
    ranges(adapter, tree.root_node(), &mut spans, 0);
    let mut chunks = Vec::new();
    for &(start, end) in &spans {
        let raw = &source[start..end];
        let code = raw.trim();
        if code.is_empty() || code.chars().all(|c| c.is_whitespace() || "{};".contains(c)) {
            continue;
        }
        let begin = start + raw.len() - raw.trim_start().len();
        let finish = begin + code.len();
        chunks.push(Chunk {
            relative_file_path: path.into(),
            language: adapter.identifier().into(),
            start_line: source[..begin].bytes().filter(|&b| b == b'\n').count() as u32 + 1,
            end_line: source[..finish].bytes().filter(|&b| b == b'\n').count() as u32 + 1,
            code: code.into(),
            content_hash: hash(code),
        });
    }
    #[cfg(test)]
    tests::assert_uncovered_bytes_are_expected(adapter, source, &spans);
    let declarations = (with_declarations && adapter.identifier() == "rust")
        .then(|| crate::node::rust_declarations_in(tree.root_node(), source));
    Ok(Extraction {
        chunks,
        declarations,
    })
}

struct SourceFile {
    relative_path: String,
    source: String,
    content_hash: String,
    adapter: &'static language::LanguageAdapter,
    lossy: bool,
}

/// Decodes source bytes as UTF-8, falling back to a lossy decoding (invalid
/// sequences become U+FFFD) rather than failing. Legacy-encoded files --
/// e.g. Windows-1252 C sources -- are common in real repositories, and one
/// of them must not make the whole workspace unindexable or unsearchable.
/// Line structure is preserved, so line ranges stay valid. One leading byte
/// order mark (U+FEFF) is dropped: it is not text, `trim()` keeps it, and it
/// would otherwise sit in the first chunk's code and in the file's hash.
pub fn decode_source(bytes: Vec<u8>) -> (String, bool) {
    let (mut source, lossy) = match String::from_utf8(bytes) {
        Ok(source) => (source, false),
        Err(error) => (String::from_utf8_lossy(error.as_bytes()).into_owned(), true),
    };
    if source.starts_with('\u{feff}') {
        source.drain(..'\u{feff}'.len_utf8());
    }
    (source, lossy)
}

struct Sources {
    files: Vec<SourceFile>,
    skipped: Vec<SkippedFile>,
}

fn sources(root: &Path, prose: bool) -> Result<Sources> {
    sources_with_read(root, prose, |path| std::fs::read(path))
}

/// The walk of the index: `.gitignore` and `.lciignore` honoured, hidden
/// files included, links not followed, `.git` skipped.
fn source_walk(root: &Path) -> ignore::Walk {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .follow_links(false)
        .require_git(false)
        .add_custom_ignore_filename(".lciignore")
        .filter_entry(|e| e.file_name() != ".git");
    walker.build()
}

/// The language adapter and `/`-separated relative path of a walked entry the
/// index reads, or `None` for a directory, a file of no language, a prose file
/// under `prose = false` or a non-UTF-8 path.
fn source_entry(
    root: &Path,
    entry: &ignore::DirEntry,
    prose: bool,
) -> Result<Option<(&'static language::LanguageAdapter, String)>> {
    if !entry.file_type().is_some_and(|t| t.is_file()) {
        return Ok(None);
    }
    let Some(adapter) = language::for_path(entry.path()) else {
        return Ok(None);
    };
    // `[index].prose = false`: prose files are not indexed at all.
    if !prose && adapter.is_prose() {
        return Ok(None);
    }
    let path = entry.path().strip_prefix(root)?;
    let relative = path
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>();
    let Some(relative) = relative else {
        tracing::warn!(path = ?path, "skipping source file: non-UTF-8 file path");
        return Ok(None);
    };
    Ok(Some((adapter, relative.join("/"))))
}

/// One file the index would read, from [`source_files`]: its path relative
/// to the root (with `/`), its modification time (`None` where the platform
/// has none) and its length in bytes. Nothing is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedSource {
    pub relative: String,
    pub modified: Option<std::time::SystemTime>,
    pub len: u64,
}

/// The source files under `root` by the index's walk rules (the walk of the
/// chunk scan, prose files only with `prose`), with their metadata and
/// without reading them, sorted by path. A file removed during the walk is
/// left out.
pub fn source_files(root: &Path, prose: bool) -> Result<Vec<ListedSource>> {
    let mut listed = Vec::new();
    for entry in source_walk(root) {
        let entry = entry.context("workspace traversal failed")?;
        let Some((_, relative)) = source_entry(root, &entry, prose)? else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error)
                if error.io_error().map(std::io::Error::kind)
                    == Some(std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error).with_context(|| format!("stat {relative}")),
        };
        listed.push(ListedSource {
            relative,
            modified: metadata.modified().ok(),
            len: metadata.len(),
        });
    }
    listed.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(listed)
}

/// What reading one source file gave (see [`read_source`]).
enum ReadSource {
    /// Removed between the walk and the read.
    Removed,
    /// A known non-source format saved under a source extension.
    Skipped(language::NonSource),
    /// The decoded text, its hash and whether the decoding was lossy.
    Source {
        source: String,
        content_hash: String,
        lossy: bool,
    },
}

/// The one place a listed file is read, skipped by content, decoded and
/// hashed: the index scan ([`sources_with_read`]) and the freshness hashes
/// ([`source_hashes_with`]) both go through it, so a hash is the same bytes
/// through the same function whichever path made it.
fn read_source(
    adapter: &language::LanguageAdapter,
    path: &Path,
    relative: &str,
    read: &mut impl FnMut(&Path) -> std::io::Result<Vec<u8>>,
) -> Result<ReadSource> {
    let bytes = match read(path) {
        Ok(bytes) => bytes,
        // Removed between the walk and the read (a branch switch during a
        // run): not an error. Any other failure aborts the pass, so the
        // previous index is kept rather than silently losing the file.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(path = %relative, %error, "skipping source file removed during the scan");
            return Ok(ReadSource::Removed);
        }
        Err(error) => return Err(error).with_context(|| format!("read {relative}")),
    };
    if let Some(reason) = adapter.non_source(&bytes) {
        return Ok(ReadSource::Skipped(reason));
    }
    let (source, lossy) = decode_source(bytes);
    let content_hash = hash(&source);
    Ok(ReadSource::Source {
        source,
        content_hash,
        lossy,
    })
}

fn sources_with_read(
    root: &Path,
    prose: bool,
    mut read: impl FnMut(&Path) -> std::io::Result<Vec<u8>>,
) -> Result<Sources> {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for entry in source_walk(root) {
        let entry = entry.context("workspace traversal failed; previous index retained")?;
        let Some((adapter, relative)) = source_entry(root, &entry, prose)? else {
            continue;
        };
        match read_source(adapter, entry.path(), &relative, &mut read)? {
            ReadSource::Removed => {}
            ReadSource::Skipped(reason) => skipped.push(SkippedFile {
                relative_path: relative,
                reason,
            }),
            ReadSource::Source {
                source,
                content_hash,
                lossy,
            } => files.push(SourceFile {
                relative_path: relative,
                source,
                content_hash,
                adapter,
                lossy,
            }),
        }
    }
    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    skipped.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(Sources { files, skipped })
}

/// One source file's content hash from [`source_hashes`]: the hash is of the
/// decoded text, the one the index scan records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashedSource {
    /// Path relative to the root, with `/`.
    pub relative: String,
    /// The adapter's identifier (`"rust"`), as the index records it.
    pub language: &'static str,
    /// The adapter's cache version, as the index records it.
    pub adapter_version: &'static str,
    pub content_hash: String,
    /// The file's modification time, `None` where the platform has none.
    pub modified: Option<std::time::SystemTime>,
}

/// What [`source_hashes`] gave: the hashed files sorted by path, and how many
/// files it read against how many it listed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceHashes {
    pub files: Vec<HashedSource>,
    /// Files whose bytes were read this call (a gated call reads only the
    /// files whose stamp is new or changed).
    pub read_files: usize,
    /// Files the walk listed that the index would read, including those a
    /// content check skips.
    pub listed_files: usize,
}

/// A file modified less than this before the moment its stamp would be kept
/// (or in the future of that moment) has a racy stamp: the gate does not keep
/// it, so the next call reads the file again. It covers a write that lands in
/// the same clock tick as the stamp and filesystems whose stamps are coarse
/// (FAT and exFAT 2 s, SMB shares, some 1 s filesystems).
const RACY_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

/// What a [`StatGate`] remembers of one file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GateEntry {
    /// `None`: no stamp is kept (the file has no modified time, or its stamp
    /// was racy), so the entry never matches and the file is read next call.
    modified: Option<std::time::SystemTime>,
    len: u64,
    /// The content hash; `None`: the file is a known non-source format and is
    /// skipped (it is not read again while its stamp stays).
    hash: Option<String>,
}

/// The stat gate of [`source_hashes_with`]: per file, the `(modified, length)`
/// it was last read at and the hash that read gave. A file whose stamp is the
/// same and has a modified time is not read again. A stamp younger than
/// [`RACY_WINDOW`] when it is taken is not kept. The gap, by construction: a
/// same-length edit that keeps a modified time older than `RACY_WINDOW`
/// before the previous read (a deliberate `set_modified`, or a clock that went
/// back) is not seen until the file's next edit.
#[derive(Debug, Default)]
pub struct StatGate {
    entries: HashMap<String, GateEntry>,
}

impl StatGate {
    /// The files the gate holds an entry for.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// The process-wide gate of one root (keyed by its canonical path, so two
/// spellings of a root share it). A call holds the gate for its walk: two
/// calls on one root run one after the other, and a call on another root is
/// never held up.
pub fn stat_gate(root: &Path) -> std::sync::Arc<std::sync::Mutex<StatGate>> {
    use std::sync::{Arc, Mutex, OnceLock, PoisonError};
    static GATES: OnceLock<Mutex<HashMap<std::path::PathBuf, Arc<Mutex<StatGate>>>>> =
        OnceLock::new();
    let key = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(key)
        .or_default()
        .clone()
}

/// The content hashes of the files the index would read under `root`, for the
/// freshness checks that compare hashes only (no chunks are made).
/// `stat_gate = false`: every file is read and hashed. `true`: through the
/// root's process-wide [`StatGate`], which reads only the files whose
/// `(modified, length)` is new or changed. Prose files only with `prose`.
pub fn source_hashes(root: &Path, prose: bool, stat_gate_on: bool) -> Result<SourceHashes> {
    if !stat_gate_on {
        return source_hashes_with(root, prose, None, |path| std::fs::read(path));
    }
    let gate = stat_gate(root);
    let mut gate = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    source_hashes_with(root, prose, Some(&mut gate), |path| std::fs::read(path))
}

/// [`source_hashes`] with an explicit gate and file reader. A file whose
/// stamp equals the gate's entry (and has a modified time) is not read: its
/// kept hash, or its kept "skipped", stands. Anything else is read, checked
/// against the non-source formats, decoded and hashed by the index's own
/// function ([`read_source`]), and the gate keeps the result. Entries of files
/// no longer listed are dropped. A file removed during the walk is left out.
/// The stamp is taken before the read, so an edit between the two is seen on
/// the next call.
pub fn source_hashes_with(
    root: &Path,
    prose: bool,
    mut gate: Option<&mut StatGate>,
    mut read: impl FnMut(&Path) -> std::io::Result<Vec<u8>>,
) -> Result<SourceHashes> {
    let mut out = SourceHashes::default();
    let mut listed = HashSet::new();
    for entry in source_walk(root) {
        let entry = entry.context("workspace traversal failed")?;
        let Some((adapter, relative)) = source_entry(root, &entry, prose)? else {
            continue;
        };
        // A stat that fails otherwise is no stamp: the file is read and never
        // gated, as a file with no modified time is.
        let (modified, len) = match entry.metadata() {
            Ok(metadata) => (metadata.modified().ok(), metadata.len()),
            Err(error)
                if error.io_error().map(std::io::Error::kind)
                    == Some(std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(_) => (None, 0),
        };
        out.listed_files += 1;
        let kept = gate
            .as_deref()
            .and_then(|gate| gate.entries.get(&relative))
            .filter(|kept| modified.is_some() && kept.modified == modified && kept.len == len)
            .map(|kept| kept.hash.clone());
        // A kept entry was trusted when it was recorded (see `RACY_WINDOW`).
        let mut trusted = true;
        let content_hash = match kept {
            Some(content_hash) => content_hash,
            None => {
                out.read_files += 1;
                let content_hash = match read_source(adapter, entry.path(), &relative, &mut read)? {
                    ReadSource::Removed => {
                        out.listed_files -= 1;
                        continue;
                    }
                    ReadSource::Skipped(_) => None,
                    ReadSource::Source { content_hash, .. } => Some(content_hash),
                };
                // Racy stamp: a file modified within `RACY_WINDOW` of this
                // read can be written again under the same stamp and length
                // (a clock tick, or a coarse filesystem), so its stamp is not
                // kept and the next call reads it again.
                trusted = modified.is_some_and(|modified| {
                    std::time::SystemTime::now()
                        .duration_since(modified)
                        .is_ok_and(|age| age >= RACY_WINDOW)
                });
                content_hash
            }
        };
        if let Some(gate) = gate.as_deref_mut() {
            listed.insert(relative.clone());
            gate.entries.insert(
                relative.clone(),
                GateEntry {
                    modified: modified.filter(|_| trusted),
                    len,
                    hash: content_hash.clone(),
                },
            );
        }
        if let Some(content_hash) = content_hash {
            out.files.push(HashedSource {
                relative,
                language: adapter.identifier(),
                adapter_version: adapter.cache_version(),
                content_hash,
                modified,
            });
        }
    }
    if let Some(gate) = gate {
        gate.entries.retain(|relative, _| listed.contains(relative));
    }
    out.files.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(out)
}

fn fingerprint_material(file: &SourceFile) -> String {
    format!(
        "{}\0{}\0{}\0{}\n",
        file.relative_path,
        file.content_hash,
        file.adapter.identifier(),
        file.adapter.cache_version()
    )
}

/// The workspace fingerprint with every registered adapter, prose included.
pub fn fingerprint(root: &Path) -> Result<String> {
    fingerprint_with(root, true)
}

/// The workspace fingerprint; with `prose = false` the files of prose
/// adapters (Markdown) are left out, as `[index].prose = false` does.
pub fn fingerprint_with(root: &Path, prose: bool) -> Result<String> {
    let material = sources(root, prose)?
        .files
        .iter()
        .map(fingerprint_material)
        .collect::<String>();
    Ok(hash(&material))
}

/// [`scan_incremental_with`] with every registered adapter, prose included.
pub fn scan_incremental(
    root: &Path,
    previous: &HashMap<String, crate::manifest::CachedFile>,
) -> Result<Scan> {
    scan_incremental_with(root, previous, true)
}

/// Scans the workspace, reusing `previous` entries whose file is unchanged.
/// With `prose = false` the files of prose adapters (Markdown) are not
/// indexed (`[index].prose = false`): they yield no chunks, and a file that
/// `previous` holds counts as removed, so flipping the switch re-indexes
/// exactly those files.
pub fn scan_incremental_with(
    root: &Path,
    previous: &HashMap<String, crate::manifest::CachedFile>,
    prose: bool,
) -> Result<Scan> {
    let mut chunks = Vec::new();
    let mut files = HashMap::new();
    let mut parsed_files = 0;
    let mut unchanged_files = 0;
    let Sources {
        files: source_files,
        skipped: skipped_files,
    } = sources(root, prose)?;
    let present: HashSet<_> = source_files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect();
    let removed_files = previous
        .keys()
        .filter(|p| !present.contains(p.as_str()))
        .count();
    let fingerprint_material = source_files
        .iter()
        .map(fingerprint_material)
        .collect::<String>();
    let lossy_files = source_files
        .iter()
        .filter(|file| file.lossy)
        .map(|file| file.relative_path.clone())
        .collect();
    for file in source_files {
        let file_chunks = match previous.get(&file.relative_path) {
            Some(cached)
                if cached.content_hash == file.content_hash
                    && cached.language == file.adapter.identifier()
                    && cached.adapter_version == file.adapter.cache_version() =>
            {
                unchanged_files += 1;
                cached.chunks.clone()
            }
            _ => {
                parsed_files += 1;
                chunks_with_adapter(&file.relative_path, &file.source, file.adapter)?
            }
        };
        chunks.extend(file_chunks.clone());
        files.insert(
            file.relative_path,
            crate::manifest::CachedFile {
                language: file.adapter.identifier().into(),
                adapter_version: file.adapter.cache_version().into(),
                content_hash: file.content_hash,
                chunks: file_chunks,
            },
        );
    }
    chunks.sort_by(|a, b| {
        (&a.relative_file_path, a.start_line).cmp(&(&b.relative_file_path, b.start_line))
    });
    Ok(Scan {
        chunks,
        files,
        fingerprint: hash(&fingerprint_material),
        parsed_files,
        unchanged_files,
        removed_files,
        lossy_files,
        skipped_files,
    })
}

pub fn scan(root: &Path) -> Result<Vec<Chunk>> {
    Ok(scan_incremental(root, &HashMap::new())?.chunks)
}

mod markdown;

#[cfg(test)]
mod extraction_tests;

#[cfg(test)]
mod markdown_tests;

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only check that runs on every chunking call of this crate's unit
    /// tests: a byte that is in no chunk must be whitespace, a separator the
    /// chunker drops on purpose (`{`, `}`, `;`), a leading byte order mark, or a
    /// comment that no chunk carries as a prefix.
    pub(super) fn assert_uncovered_bytes_are_expected(
        adapter: &language::LanguageAdapter,
        source: &str,
        spans: &[(usize, usize)],
    ) {
        let mut covered = vec![false; source.len()];
        for &(start, end) in spans {
            let raw = &source[start..end];
            let code = raw.trim();
            if code.is_empty() || code.chars().all(|c| c.is_whitespace() || "{};".contains(c)) {
                continue;
            }
            let begin = start + raw.len() - raw.trim_start().len();
            covered[begin..begin + code.len()].fill(true);
        }

        let mut parser = Parser::new();
        parser.set_language(&adapter.grammar()).unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut cursor = tree.walk();
        let mut stack = vec![tree.root_node()];
        let mut comments = Vec::new();
        while let Some(node) = stack.pop() {
            if matches!(node.kind(), "line_comment" | "block_comment" | "comment") {
                comments.push((node.kind(), node.start_byte(), node.end_byte()));
            }
            stack.extend(node.children(&mut cursor));
        }
        for (kind, comment_start, comment_end) in comments {
            let attached = spans.iter().any(|&(start, end)| {
                let raw = &source[start..end];
                let code = raw.trim();
                if code.is_empty() || code.chars().all(|c| c.is_whitespace() || "{};".contains(c)) {
                    return false;
                }
                let begin = start + raw.len() - raw.trim_start().len();
                adapter.is_prefix(kind)
                    && begin >= comment_end
                    && source[comment_end..begin].chars().all(char::is_whitespace)
            });
            if attached {
                covered[comment_start..comment_end].fill(true);
            }
        }

        for (offset, character) in source.char_indices() {
            let end = offset + character.len_utf8();
            if covered[offset..end].iter().all(|&byte| byte)
                || character.is_whitespace()
                || "{};".contains(character)
                || (offset == 0 && character == '\u{feff}')
            {
                continue;
            }
            panic!(
                "unexpected uncovered byte at {offset} ({character:?}) in {} source bytes",
                source.len()
            );
        }
    }

    fn compact(source: &str) -> String {
        source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn assert_chunks_cover(chunks: &[Chunk], expected: &str) {
        let packed: String = chunks.iter().map(|chunk| compact(&chunk.code)).collect();
        let expected = compact(expected);
        assert!(
            packed.contains(&expected),
            "chunks do not cover {expected:?}:\n{chunks:#?}"
        );
    }

    fn assert_non_whitespace_coverage(source: &str, expected: &str) {
        let chunks = rust_chunks("lib.rs", source).unwrap();
        assert_chunks_cover(&chunks, expected);
    }

    #[test]
    fn ordinary_nested_modules_keep_exact_chunks() {
        let depth = 32;
        let source = format!(
            "{}fn leaf() {{}}\n{}fn sibling() {{}}\n",
            "mod m {\n".repeat(depth),
            "}\n".repeat(depth)
        );
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        let mut expected: Vec<_> = (1..=depth as u32)
            .map(|line| (line, line, "mod m"))
            .collect();
        expected.push((32, 34, "{\nfn leaf() {}\n}"));
        expected.push((66, 66, "fn sibling() {}"));
        assert_eq!(
            chunks
                .iter()
                .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
                .collect::<Vec<_>>(),
            expected
        );
        for chunk in chunks {
            assert_eq!(chunk.relative_file_path, "lib.rs");
            assert_eq!(chunk.language, "rust");
            assert_eq!(chunk.content_hash, hash(&chunk.code));
        }
    }

    #[test]
    fn deeply_nested_modules_on_default_blocking_stack() {
        const CHILD_DEPTH: &str = "LCI_TEST_NESTING_DEPTH";
        if let Ok(depth) = std::env::var(CHILD_DEPTH) {
            let depth: usize = depth.parse().unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async move {
                tokio::task::spawn_blocking(move || {
                    let source = format!(
                        "{}fn leaf() {{}}\n{}fn sibling() {{}}\n",
                        "mod m {\n".repeat(depth),
                        "}\n".repeat(depth)
                    );
                    let chunks = rust_chunks("deep.rs", &source).unwrap();
                    assert_chunks_cover(&chunks, "fn leaf() {}");
                    assert_chunks_cover(&chunks, "fn sibling() {}");
                    assert_eq!(
                        chunks
                            .iter()
                            .map(|chunk| chunk.code.matches("mod m").count())
                            .sum::<usize>(),
                        depth
                    );
                    for chunk in chunks {
                        assert_eq!(chunk.content_hash, hash(&chunk.code));
                        assert!(source.contains(&chunk.code));
                    }
                })
                .await
                .unwrap();
            });
            return;
        }

        // Isolate stack overflow in a child so the regression cannot abort
        // the rest of the test suite.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "chunk::tests::deeply_nested_modules_on_default_blocking_stack",
                "--nocapture",
            ])
            .env(CHILD_DEPTH, "32768")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "default-stack chunking failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // A renamed test would make the child run nothing and pass.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child ran no test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    #[test]
    fn leading_bom_does_not_change_rust_typescript_or_csharp_chunks() {
        let fixtures = [
            ("lib.rs", "fn rust_value() { let _ = 1; }\n"),
            ("types.ts", "export const typescriptValue = 1;\n"),
            ("Service.cs", "public sealed class CsharpService {}\n"),
        ];
        let without_bom = tempfile::tempdir().unwrap();
        let with_bom = tempfile::tempdir().unwrap();
        for (path, source) in fixtures {
            std::fs::write(without_bom.path().join(path), source).unwrap();
            std::fs::write(with_bom.path().join(path), format!("\u{feff}{source}")).unwrap();
        }

        let without_bom = scan_incremental(without_bom.path(), &HashMap::new()).unwrap();
        let with_bom = scan_incremental(with_bom.path(), &HashMap::new()).unwrap();
        let details = |chunks: &[Chunk]| {
            chunks
                .iter()
                .map(|chunk| {
                    (
                        chunk.relative_file_path.clone(),
                        chunk.language.clone(),
                        chunk.start_line,
                        chunk.end_line,
                        chunk.code.clone(),
                        chunk.content_hash.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            details(&with_bom.chunks),
            details(&without_bom.chunks),
            "chunks with BOM: {:#?}\nchunks without BOM: {:#?}",
            with_bom.chunks,
            without_bom.chunks
        );
        assert!(with_bom.chunks.iter().all(|chunk| chunk.code != "\u{feff}"));
        let differing_source_hashes: Vec<_> = fixtures
            .iter()
            .filter_map(|(path, _)| {
                let with_bom_hash = &with_bom.files[*path].content_hash;
                let without_bom_hash = &without_bom.files[*path].content_hash;
                (with_bom_hash != without_bom_hash).then_some((
                    *path,
                    with_bom_hash,
                    without_bom_hash,
                ))
            })
            .collect();
        assert!(
            differing_source_hashes.is_empty(),
            "decoded source hashes differ by language: {differing_source_hashes:#?}"
        );
    }

    #[test]
    fn decode_source_drops_one_leading_byte_order_mark_and_keeps_the_lossy_flag() {
        assert_eq!(
            decode_source(b"\xEF\xBB\xBFfn a() {}\n".to_vec()),
            ("fn a() {}\n".to_owned(), false)
        );
        assert_eq!(
            decode_source(b"fn a() {}\n".to_vec()),
            ("fn a() {}\n".to_owned(), false)
        );
        assert_eq!(
            decode_source(b"\xEF\xBB\xBF".to_vec()),
            (String::new(), false)
        );
        // Only the first mark is the byte order mark; a second one is content.
        assert_eq!(
            decode_source("\u{feff}\u{feff}x".as_bytes().to_vec()),
            ("\u{feff}x".to_owned(), false)
        );
        // A mark in the middle is content too.
        assert_eq!(
            decode_source("a\u{feff}b".as_bytes().to_vec()),
            ("a\u{feff}b".to_owned(), false)
        );
        // Invalid UTF-8 after the mark: the file is lossy, and the mark still goes.
        assert_eq!(
            decode_source(b"\xEF\xBB\xBFlet a = \xFF;\n".to_vec()),
            ("let a = \u{fffd};\n".to_owned(), true)
        );
    }

    #[test]
    fn rust_boundaries_remain_stable() {
        assert_eq!(DOCUMENT_FORMAT_VERSION, "rust-chunks-v1");
        let source =
            "// café\nfn first() { println!(\"hello\"); }\n\nimpl A {\nfn second() {}\n}\n";
        let chunks = rust_chunks("lib.rs", source).unwrap();
        let boundaries: Vec<_> = chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
            .collect();
        assert_eq!(
            boundaries,
            vec![
                (1, 2, "// café\nfn first() { println!(\"hello\"); }"),
                (4, 4, "impl A"),
                (4, 6, "{\nfn second() {}\n}"),
            ]
        );
        for chunk in chunks {
            assert!(source.contains(&chunk.code));
            assert_eq!(chunk.content_hash, hash(&chunk.code));
        }
    }

    #[test]
    fn chunks_typescript_declarations_prefixes_and_top_level_code() {
        let source = r#"import { metric } from "./metric";
// Streams telemetry.
export function* streamTelemetry() { yield metric; }

/** Runtime service. */
@sealed
export class TelemetryService {
  // Records a sample.
  @trace
  record(): number { return metric; }
}

export interface TelemetrySink {
  write(value: number): void;
}
export type TelemetryId = string;
export enum TelemetryMode { Live, Replay }
export const collectTelemetry = (value: number) => value + 1;
startTelemetry();
"#;
        let chunks = chunks_for_path("telemetry.ts", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "typescript"));
        for expected in [
            "import { metric }",
            "export function* streamTelemetry",
            "export class TelemetryService",
            "record(): number",
            "export interface TelemetrySink",
            "write(value: number): void",
            "export type TelemetryId",
            "export enum TelemetryMode",
            "export const collectTelemetry",
            "startTelemetry();",
        ] {
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(expected)),
                "missing {expected}: {chunks:#?}"
            );
        }
        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class TelemetryService"))
            .unwrap();
        assert!(class.code.starts_with("/** Runtime service. */"));
        assert!(class.code.contains("@sealed"));
        assert!(class.code.contains("@trace"));
        let arrow = chunks
            .iter()
            .find(|chunk| chunk.code.contains("collectTelemetry"))
            .unwrap();
        assert!(arrow.code.starts_with("export const collectTelemetry"));
    }

    #[test]
    fn chunks_class_methods_and_interface_signatures_at_member_boundaries() {
        let source = r"/** Coordinates telemetry. */
export class TelemetryCoordinator {
  // Starts collection.
  start(): void {}

  // Stops collection.
  stop(): void {}
}

export interface TelemetryLifecycle {
  start(): void;
  stop(): void;
}
";
        let chunks = chunks_for_path("lifecycle.ts", source).unwrap();
        let start_method = chunks
            .iter()
            .find(|chunk| chunk.code.contains("start(): void {}"))
            .unwrap();
        let stop_method = chunks
            .iter()
            .find(|chunk| chunk.code.contains("stop(): void {}"))
            .unwrap();
        assert!(
            start_method
                .code
                .starts_with("/** Coordinates telemetry. */")
        );
        assert!(
            start_method
                .code
                .contains("export class TelemetryCoordinator")
        );
        assert!(start_method.code.contains("// Starts collection."));
        assert!(!start_method.code.contains("stop(): void {}"));
        assert!(stop_method.code.contains("// Stops collection."));

        let start_signature = chunks
            .iter()
            .find(|chunk| chunk.code.contains("export interface TelemetryLifecycle"))
            .unwrap();
        let stop_signature = chunks
            .iter()
            .find(|chunk| {
                chunk.code.contains("stop(): void;") && !chunk.code.contains("stop(): void {}")
            })
            .unwrap();
        assert!(start_signature.code.contains("start(): void"));
        assert!(!start_signature.code.contains("stop(): void"));
        assert!(!stop_signature.code.contains("start(): void"));
    }

    #[test]
    fn chunks_tsx_and_javascript_variants_with_correct_languages() {
        let cases = [
            (
                "panel.tsx",
                "tsx",
                "// UI\nexport const TelemetryPanel = () => <section>Live</section>;\n",
                "export const TelemetryPanel",
            ),
            (
                "worker.js",
                "javascript",
                "export function collect() { return 1; }\nclass Worker { run() {} }\nboot();\n",
                "class Worker",
            ),
            (
                "view.jsx",
                "jsx",
                "export const View = () => <main>Ready</main>;\nrender(<View />);\n",
                "render(<View />);",
            ),
        ];
        for (path, language, source, expected) in cases {
            let chunks = chunks_for_path(path, source).unwrap();
            assert!(chunks.iter().all(|chunk| chunk.language == language));
            assert!(chunks.iter().any(|chunk| chunk.code.contains(expected)));
            assert!(chunks.iter().all(|chunk| source.contains(&chunk.code)));
        }
    }

    #[test]
    fn oversized_exported_typescript_class_splits_at_members_with_header_attached() {
        let methods = (0..120)
            .map(|index| {
                format!(
                    "  // member {index}\n  method_{index}() {{ return \"{}\"; }}\n",
                    "value".repeat(50)
                )
            })
            .collect::<String>();
        let source =
            format!("/** Large service. */\n@sealed\nexport class LargeService {{\n{methods}}}\n");
        assert!(source.len() > MAX_DECLARATION_BYTES);
        let chunks = chunks_for_path("large.ts", &source).unwrap();
        assert!(chunks.len() > 1);
        assert!(chunks[0].code.starts_with("/** Large service. */"));
        assert!(chunks[0].code.contains("export class LargeService"));
        for index in 0..120 {
            let method = format!("method_{index}");
            let chunk = chunks
                .iter()
                .find(|chunk| chunk.code.contains(&method))
                .unwrap_or_else(|| panic!("missing {method}"));
            assert!(chunk.code.contains(&format!("// member {index}")));
        }
    }
    #[test]
    fn respects_ignore_without_git_repository() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(temp.path().join("ignored")).unwrap();
        std::fs::write(temp.path().join("ignored/a.rs"), "fn hidden() {} ").unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {} ").unwrap();
        let chunks = scan(temp.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].relative_file_path, "lib.rs");
    }

    // Linux only: APFS refuses to create a file whose name is not valid UTF-8.
    #[test]
    #[cfg(target_os = "linux")]
    fn scan_skips_non_utf8_paths_and_indexes_other_files() {
        use std::os::unix::ffi::OsStrExt;

        let temp = tempfile::tempdir().unwrap();
        let invalid = std::ffi::OsStr::from_bytes(b"invalid-\xff.rs");
        std::fs::write(temp.path().join(invalid), "fn invisible() {}\n").unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();

        let result = scan_incremental(temp.path(), &HashMap::new());
        assert!(
            result.is_ok(),
            "one non-UTF-8 path failed the scan: {result:?}"
        );
        let scan = result.unwrap();
        assert_eq!(scan.files.len(), 1);
        assert_eq!(scan.chunks.len(), 1);
        assert_eq!(scan.chunks[0].relative_file_path, "lib.rs");
        assert!(scan.chunks[0].code.contains("visible"));
        assert_eq!(fingerprint(temp.path()).unwrap(), scan.fingerprint);
        let repeated = scan_incremental(temp.path(), &scan.files).unwrap();
        assert_eq!(repeated.unchanged_files, 1);
        assert_eq!(repeated.parsed_files, 0);
    }

    #[test]
    fn scan_skips_a_file_removed_between_discovery_and_read() {
        let temp = tempfile::tempdir().unwrap();
        let victim = temp.path().join("vanishing.rs");
        std::fs::write(&victim, "fn vanishing() {}\n").unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();
        let previous = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let mut removed = false;

        let result = sources_with_read(temp.path(), true, |path| {
            if path == victim {
                std::fs::remove_file(path).unwrap();
                removed = true;
            }
            std::fs::read(path)
        });
        assert!(removed, "the walker must discover the file before removal");
        assert!(result.is_ok(), "one disappearing file failed the scan");
        let sources = result.unwrap();
        assert_eq!(sources.files.len(), 1);
        assert_eq!(sources.files[0].relative_path, "lib.rs");
        assert!(sources.files[0].source.contains("visible"));
        let scan = scan_incremental(temp.path(), &previous.files).unwrap();
        assert_eq!(scan.removed_files, 1);
        assert_eq!(scan.unchanged_files, 1);
        assert_eq!(scan.chunks.len(), 1);
        assert!(scan.chunks[0].code.contains("visible"));
        assert_eq!(fingerprint(temp.path()).unwrap(), scan.fingerprint);
    }

    #[test]
    fn prose_files_are_indexed_only_when_asked_and_the_switch_changes_the_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();
        std::fs::write(temp.path().join("README.md"), "# Title\nSome prose.\n").unwrap();

        let without = scan_incremental_with(temp.path(), &HashMap::new(), false).unwrap();
        assert!(without.files.keys().eq(["lib.rs"]));
        assert!(without.chunks.iter().all(|c| c.language != "markdown"));
        let with = scan_incremental_with(temp.path(), &without.files, true).unwrap();
        assert!(with.files.contains_key("README.md"));
        assert_eq!((with.parsed_files, with.unchanged_files), (1, 1));
        assert_ne!(without.fingerprint, with.fingerprint);
        assert_eq!(
            fingerprint_with(temp.path(), false).unwrap(),
            without.fingerprint
        );
        assert_eq!(
            fingerprint_with(temp.path(), true).unwrap(),
            with.fingerprint
        );
        // Switching back drops the prose file and reuses the code file.
        let back = scan_incremental_with(temp.path(), &with.files, false).unwrap();
        assert_eq!(
            (back.removed_files, back.parsed_files, back.unchanged_files),
            (1, 0, 1)
        );
        assert_eq!(back.fingerprint, without.fingerprint);
    }

    #[test]
    fn scan_fails_on_a_read_error_other_than_a_removed_file() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();

        let result = sources_with_read(temp.path(), true, |_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        });

        let Err(error) = result else {
            panic!("an unreadable file must abort the scan, not be skipped");
        };
        assert!(format!("{error:#}").contains("read lib.rs"), "{error:#}");
    }

    #[test]
    fn non_utf8_files_are_indexed_lossily_instead_of_failing_the_scan() {
        let temp = tempfile::tempdir().unwrap();
        // Windows-1252 "café" (0xE9 is not valid UTF-8 on its own).
        std::fs::write(
            temp.path().join("legacy.c"),
            b"/* caf\xe9 */\nint legacy_value(void) { return 7; }\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn visible() {}\n").unwrap();

        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        assert_eq!(scan.lossy_files, vec!["legacy.c".to_owned()]);
        let legacy = scan
            .chunks
            .iter()
            .find(|chunk| chunk.relative_file_path == "legacy.c")
            .expect("the legacy file is still indexed");
        assert!(legacy.code.contains("caf\u{FFFD}"), "{}", legacy.code);
        assert!(legacy.code.contains("legacy_value"));
        // The staleness fingerprint no longer fails on the same file.
        assert_eq!(fingerprint(temp.path()).unwrap(), scan.fingerprint);
    }

    #[test]
    fn respects_project_local_lciignore() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("visible.rs"), "fn visible() {}\n").unwrap();
        std::fs::create_dir_all(temp.path().join(".agents/skills/example")).unwrap();
        std::fs::write(
            temp.path().join(".agents/skills/example/template.rs"),
            "fn skill_template() {}\n",
        )
        .unwrap();
        std::fs::write(temp.path().join(".lciignore"), ".agents/skills/**\n").unwrap();

        let chunks = scan(temp.path()).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].relative_file_path, "visible.rs");
    }

    #[test]
    fn scans_exactly_the_registered_lowercase_extensions() {
        let temp = tempfile::tempdir().unwrap();
        for (name, source) in [
            ("lib.rs", "fn rust_item() {}\n"),
            ("types.ts", "export type Item = string;\n"),
            ("panel.tsx", "export const Panel = () => <main />;\n"),
            ("runtime.js", "export function run() {}\n"),
            ("view.jsx", "export const View = () => <main />;\n"),
            ("Service.cs", "public sealed class Service {}\n"),
            ("guide.md", "# Guide\ntext\n"),
            ("ignored.MD", "# Upper\ntext\n"),
            ("ignored.markdown", "# Long\ntext\n"),
            ("ignored.mdx", "# Mdx\ntext\n"),
            ("ignored.RS", "fn uppercase() {}\n"),
            ("ignored.CS", "public sealed class Ignored {}\n"),
            ("ignored.mts", "export const ignored = 1;\n"),
            ("ignored.py", "ignored = True\n"),
        ] {
            std::fs::write(temp.path().join(name), source).unwrap();
        }
        let chunks = scan(temp.path()).unwrap();
        let files: HashSet<_> = chunks
            .iter()
            .map(|chunk| chunk.relative_file_path.as_str())
            .collect();
        assert_eq!(
            files,
            HashSet::from([
                "lib.rs",
                "types.ts",
                "panel.tsx",
                "runtime.js",
                "view.jsx",
                "ignored.py",
                "Service.cs",
                "guide.md"
            ])
        );
    }

    #[test]
    fn scans_exactly_the_registered_c_and_cpp_extensions() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("include/vendor")).unwrap();
        std::fs::create_dir_all(temp.path().join("out")).unwrap();
        std::fs::write(temp.path().join(".gitignore"), "out/\n").unwrap();
        std::fs::write(temp.path().join("include/.gitignore"), "vendor/\n").unwrap();
        for (name, source) in [
            ("src/a.c", "int a(void) { return 1; }\n"),
            ("include/a.h", "int a(void);\n"),
            ("include/.hidden.h", "int hidden(void);\n"),
            ("src/b.cpp", "int b() { return 1; }\n"),
            ("src/b.cc", "int bb() { return 1; }\n"),
            ("src/b.cxx", "int bx() { return 1; }\n"),
            ("include/b.hpp", "int b();\n"),
            ("include/b.hh", "int bb();\n"),
            ("include/b.hxx", "int bx();\n"),
            // Unsupported or case-variant lookalikes are ignored.
            ("src/upper.C", "int upper() { return 1; }\n"),
            ("include/upper.H", "int upper();\n"),
            ("src/upper.CC", "int upper() { return 1; }\n"),
            ("include/upper.HPP", "int upper();\n"),
            ("src/plus.c++", "int plus() { return 1; }\n"),
            ("include/plus.h++", "int plus();\n"),
            ("include/a.h.in", "int template_only();\n"),
            ("include/a.inl", "int inl();\n"),
            ("src/a.cu", "int cuda() { return 1; }\n"),
            // Ignore files apply to the new extensions like any other.
            ("out/gen.h", "int generated(void);\n"),
            ("include/vendor/third.hh", "int third();\n"),
        ] {
            let path = temp.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }
        let chunks = scan(temp.path()).unwrap();
        let mut languages: HashMap<&str, &str> = HashMap::new();
        for chunk in &chunks {
            languages.insert(chunk.relative_file_path.as_str(), chunk.language.as_str());
        }
        assert_eq!(
            languages,
            HashMap::from([
                ("src/a.c", "c"),
                ("include/a.h", "c"),
                ("include/.hidden.h", "c"),
                ("src/b.cpp", "cpp"),
                ("src/b.cc", "cpp"),
                ("src/b.cxx", "cpp"),
                ("include/b.hpp", "cpp"),
                ("include/b.hh", "cpp"),
                ("include/b.hxx", "cpp"),
            ])
        );
    }

    const QT_LINGUIST_CATALOG: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE TS>\n<TS version=\"2.1\" language=\"de\">\n<context>\n    <name>SendCoinsDialog</name>\n    <message>\n        <source>Send coins</source>\n        <translation>Muenzen senden</translation>\n    </message>\n</context>\n</TS>\n";

    #[test]
    fn qt_linguist_files_are_skipped_while_typescript_is_indexed() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("locale")).unwrap();
        std::fs::write(
            temp.path().join("send.ts"),
            "export function sendCoins() { return 1; }\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("locale/app_de.ts"), QT_LINGUIST_CATALOG).unwrap();

        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        assert_eq!(
            scan.files.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["send.ts"]
        );
        assert_eq!(scan.files["send.ts"].language, "typescript");
        assert!(
            scan.chunks
                .iter()
                .all(|chunk| chunk.language == "typescript"
                    && chunk.relative_file_path == "send.ts"
                    && !chunk.code.contains("Muenzen"))
        );
        assert_eq!(scan.parsed_files, 1);
        assert_eq!(
            scan.skipped_files,
            vec![SkippedFile {
                relative_path: "locale/app_de.ts".into(),
                reason: language::NonSource::QtLinguist,
            }]
        );
        assert!(scan.lossy_files.is_empty());
    }

    #[test]
    fn skipped_files_do_not_change_the_staleness_fingerprint() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("send.ts"), "export const a = 1;\n").unwrap();
        let without = fingerprint(temp.path()).unwrap();
        std::fs::write(temp.path().join("app_de.ts"), QT_LINGUIST_CATALOG).unwrap();
        assert_eq!(fingerprint(temp.path()).unwrap(), without);
        std::fs::write(
            temp.path().join("app_de.ts"),
            QT_LINGUIST_CATALOG.replace("Muenzen", "Coins"),
        )
        .unwrap();
        assert_eq!(fingerprint(temp.path()).unwrap(), without);
        // Once the file turns out to be TypeScript the fingerprint moves.
        std::fs::write(temp.path().join("app_de.ts"), "export const b = 2;\n").unwrap();
        assert_ne!(fingerprint(temp.path()).unwrap(), without);
    }

    #[test]
    fn a_file_reclassified_as_qt_linguist_drops_its_cached_chunks() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("send.ts"), "export const send = 1;\n").unwrap();
        std::fs::write(
            temp.path().join("catalog.ts"),
            "export function translatorOnly() { return 1; }\n",
        )
        .unwrap();
        let first = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        assert!(first.skipped_files.is_empty());
        assert!(
            first
                .chunks
                .iter()
                .any(|chunk| chunk.relative_file_path == "catalog.ts")
        );

        std::fs::write(temp.path().join("catalog.ts"), QT_LINGUIST_CATALOG).unwrap();
        let second = scan_incremental(temp.path(), &first.files).unwrap();
        assert_eq!(second.removed_files, 1);
        assert_eq!(second.unchanged_files, 1);
        assert_eq!(second.parsed_files, 0);
        assert!(!second.files.contains_key("catalog.ts"));
        assert!(
            second
                .chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "send.ts")
        );
        assert_eq!(second.skipped_files.len(), 1);
        assert_ne!(second.fingerprint, first.fingerprint);

        // And back: the same path is parsed again as TypeScript.
        std::fs::write(
            temp.path().join("catalog.ts"),
            "export function translatorOnly() { return 1; }\n",
        )
        .unwrap();
        let third = scan_incremental(temp.path(), &second.files).unwrap();
        assert_eq!(third.parsed_files, 1);
        assert!(third.skipped_files.is_empty());
        assert_eq!(third.files["catalog.ts"].language, "typescript");
    }

    #[test]
    fn qt_linguist_edge_cases_keep_real_typescript_indexed() {
        let temp = tempfile::tempdir().unwrap();
        let bom_catalog = format!("\u{feff}{QT_LINGUIST_CATALOG}");
        let blank_then_catalog = format!("\n\n  \t{QT_LINGUIST_CATALOG}");
        for (name, content) in [
            ("bom.ts", bom_catalog.as_str()),
            ("blank_then_catalog.ts", blank_then_catalog.as_str()),
            (
                "root_only.ts",
                "<TS version=\"2.1\" language=\"fr\">\n</TS>\n",
            ),
            (
                "comment.ts",
                "// <TS version=\"2.1\"> is the Qt Linguist root element\nexport const note = 1;\n",
            ),
            (
                "jsdoc.ts",
                "/** Reads <?xml prologs. */\nexport function readProlog() {}\n",
            ),
            ("assertion.ts", "<TS>value;\n"),
            (
                "generic_arrow.ts",
                "<TS extends object>(item: TS) => item;\n",
            ),
            ("bom_typescript.ts", "\u{feff}export const bomValue = 1;\n"),
            ("empty.ts", ""),
            ("blank.ts", "\n  \n"),
        ] {
            std::fs::write(temp.path().join(name), content).unwrap();
        }
        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let skipped: Vec<_> = scan
            .skipped_files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect();
        assert_eq!(skipped, ["blank_then_catalog.ts", "bom.ts", "root_only.ts"]);
        let mut indexed: Vec<_> = scan.files.keys().map(String::as_str).collect();
        indexed.sort_unstable();
        assert_eq!(
            indexed,
            [
                "assertion.ts",
                "blank.ts",
                "bom_typescript.ts",
                "comment.ts",
                "empty.ts",
                "generic_arrow.ts",
                "jsdoc.ts"
            ]
        );
        // Empty and blank files are indexed (as zero chunks), not skipped.
        assert!(
            scan.chunks
                .iter()
                .all(|chunk| !matches!(chunk.relative_file_path.as_str(), "empty.ts" | "blank.ts"))
        );
        assert!(
            scan.chunks
                .iter()
                .any(|chunk| chunk.relative_file_path == "comment.ts"
                    && chunk.code.contains("export const note"))
        );
    }

    #[test]
    fn utf16_qt_linguist_catalogs_are_skipped_instead_of_decoded_lossily() {
        fn utf16(text: &str, big_endian: bool) -> Vec<u8> {
            std::iter::once(0xFEFF_u16)
                .chain(text.encode_utf16())
                .flat_map(|unit| {
                    if big_endian {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    }
                })
                .collect()
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("app_de_le.ts"),
            utf16(QT_LINGUIST_CATALOG, false),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("app_de_be.ts"),
            utf16(QT_LINGUIST_CATALOG, true),
        )
        .unwrap();
        std::fs::write(
            temp.path().join("send.ts"),
            "export function sendCoins() { return 1; }\n",
        )
        .unwrap();

        let scan = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let skipped: Vec<_> = scan
            .skipped_files
            .iter()
            .map(|file| (file.relative_path.as_str(), file.reason))
            .collect();
        assert_eq!(
            skipped,
            [
                ("app_de_be.ts", language::NonSource::QtLinguist),
                ("app_de_le.ts", language::NonSource::QtLinguist)
            ]
        );
        // Skipped before decoding: neither catalog is reported as lossy.
        assert!(scan.lossy_files.is_empty());
        assert_eq!(scan.files.keys().collect::<Vec<_>>(), ["send.ts"]);
        assert!(
            scan.chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "send.ts")
        );
    }

    #[test]
    fn reparses_only_the_language_with_incompatible_cache_metadata() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("lib.rs"), "fn stable() {}\n").unwrap();
        std::fs::write(
            temp.path().join("panel.ts"),
            "export const panel = () => 1;\n",
        )
        .unwrap();
        let first = scan_incremental(temp.path(), &HashMap::new()).unwrap();
        let mut cached = first.files;
        cached.get_mut("panel.ts").unwrap().adapter_version = "old-typescript".into();
        let second = scan_incremental(temp.path(), &cached).unwrap();
        assert_eq!(second.parsed_files, 1);
        assert_eq!(second.unchanged_files, 1);
        assert_eq!(second.files["lib.rs"].language, "rust");
        assert_eq!(second.files["panel.ts"].language, "typescript");
    }

    #[test]
    fn keeps_docs_attributes_and_medium_function_together() {
        let body = "    let number = 42;\n".repeat(400);
        let source = format!(
            "/// Translate expressions.\n#[allow(unused)]\nfn translator() {{\n{body}}}\nfn next() {{}}\n"
        );
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].code.starts_with("/// Translate"));
        assert!(chunks[0].code.contains("#[allow(unused)]"));
        assert!(chunks[0].code.contains(&body));
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 404);
    }

    #[test]
    fn oversized_trailing_comment_run_splits_at_syntax_boundaries() {
        let body = "    let value = 42;\n".repeat(2000);
        assert!(body.len() > MAX_DECLARATION_BYTES);
        let comment = format!("// {}\n", "x".repeat(56));
        assert_eq!(comment.len(), 60);
        let trailing = comment.repeat(2 * TARGET_BYTES / comment.len());
        assert_eq!(trailing.len(), 2 * TARGET_BYTES);
        for declaration in [
            format!("fn large() {{\n{body}}}\n"),
            "fn small() {}\n".into(),
            String::new(),
        ] {
            let source = format!("impl Example {{\n{declaration}{trailing}}}\n");
            let chunks = rust_chunks("lib.rs", &source).unwrap();
            assert!(
                chunks.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES),
                "chunk lengths exceed target {TARGET_BYTES}: {:?}",
                chunks
                    .iter()
                    .map(|chunk| chunk.code.len())
                    .collect::<Vec<_>>()
            );
            assert_chunks_cover(&chunks, &trailing);
            assert_chunks_cover(&chunks, &declaration);
            for chunk in chunks {
                assert!(source.contains(&chunk.code));
                assert_eq!(chunk.content_hash, hash(&chunk.code));
                let lines: Vec<_> = source.lines().collect();
                assert!(
                    lines[(chunk.start_line - 1) as usize..chunk.end_line as usize]
                        .join("\n")
                        .contains(&chunk.code)
                );
            }
        }
    }

    #[test]
    fn short_trailing_comments_keep_existing_boundaries() {
        let source = "impl Example {\nfn small() {}\n// tail\n}\n";
        let chunks = rust_chunks("lib.rs", source).unwrap();
        let boundaries: Vec<_> = chunks
            .iter()
            .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.as_str()))
            .collect();
        assert_eq!(
            boundaries,
            [
                (1, 1, "impl Example"),
                (1, 4, "{\nfn small() {}\n// tail\n}")
            ]
        );
    }

    #[test]
    fn oversized_trailing_comments_split_for_every_adapter() {
        for (path, declaration, marker) in [
            ("lib.rs", "fn small() {}\n", "//"),
            ("small.ts", "function small() {}\n", "//"),
            ("small.tsx", "function small() {}\n", "//"),
            ("small.js", "function small() {}\n", "//"),
            ("small.jsx", "function small() {}\n", "//"),
            ("small.py", "def small():\n    pass\n", "#"),
            ("Small.cs", "class Small {}\n", "//"),
            ("small.go", "package small\nfunc small() {}\n", "//"),
            ("Small.java", "class Small {}\n", "//"),
            ("small.c", "void small() {}\n", "//"),
            ("small.cpp", "void small() {}\n", "//"),
            ("small.hpp", "void small() {}\n", "//"),
            ("small.h", "void small(void);\n", "//"),
            ("small.cc", "void small() {}\n", "//"),
            ("small.cxx", "void small() {}\n", "//"),
            ("small.hh", "void small();\n", "//"),
            ("small.hxx", "void small();\n", "//"),
        ] {
            let comment = format!("{marker} tail\n");
            let trailing = comment.repeat(2 * TARGET_BYTES / comment.len() + 1);
            let source = format!("{declaration}{trailing}");
            let chunks = chunks_for_path(path, &source).unwrap();
            assert!(
                chunks.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES),
                "{path} has an oversized trailing chunk"
            );
            assert_chunks_cover(&chunks, &trailing);
        }
    }

    #[test]
    fn indivisible_trailing_comment_remains_whole() {
        let comment = format!("/* {} */", "é".repeat(TARGET_BYTES));
        let source = format!("impl Example {{\nfn small() {{}}\n{comment}\n}}\n");
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(chunks.iter().any(|chunk| chunk.code.contains(&comment)));
    }

    #[test]
    fn leading_comments_stay_attached_when_a_long_trailing_run_splits() {
        let comment = format!("// {}\n", "x".repeat(56));
        let trailing = comment.repeat(2 * TARGET_BYTES / comment.len());
        let source = format!(
            "impl Example {{\n    /// Documented.\n    fn documented() {{}}\n{trailing}}}\n"
        );
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(
            chunks.iter().any(|chunk| {
                chunk.code.contains("/// Documented.") && chunk.code.contains("fn documented")
            }),
            "the doc comment left its function: {:?}",
            chunks
                .iter()
                .map(|chunk| &chunk.code[..chunk.code.len().min(40)])
                .collect::<Vec<_>>()
        );
        assert!(chunks.iter().all(|chunk| chunk.code.len() <= TARGET_BYTES));
        assert_chunks_cover(&chunks, &trailing);
    }

    #[test]
    fn trailing_comment_run_at_the_target_is_not_split_by_the_closing_brace() {
        let comment = format!("// {}\n", "x".repeat(TARGET_BYTES / 2 - 4));
        assert_eq!(2 * comment.len(), TARGET_BYTES);
        let source = format!("impl Example {{\nfn small() {{}}\n{comment}{comment}}}\n");
        let chunks = rust_chunks("lib.rs", &source).unwrap();
        assert!(
            chunks.iter().any(|chunk| chunk
                .code
                .contains(&format!("{comment}{}", comment.trim_end()))),
            "a run of exactly the target was split: {:?}",
            chunks
                .iter()
                .map(|chunk| chunk.code.len())
                .collect::<Vec<_>>()
        );
    }

    /// Includes the traced example of N22: an oversized call whose two arguments
    /// are joined by `+`. The operator lies between the two split children and
    /// used to be in no chunk.
    #[test]
    fn oversized_containers_cover_non_whitespace_bytes_across_inputs() {
        let left = "a".repeat(3500);
        let middle = "b".repeat(3500);
        let right = "c".repeat(3500);
        let expressions = [
            format!("emit(\"{left}\" + \"{middle}\")"),
            format!("emit([\"{left}\", \"{middle}\"])"),
            format!("emit((\"{left}\", \"{middle}\"))"),
            format!("emit(\"{left}\" + \"{middle}\" + \"{right}\")"),
        ];

        for expression in expressions {
            let statements = format!("    {expression};\n").repeat(4);
            let source = format!("fn main() {{\n{statements}}}\n");
            assert_non_whitespace_coverage(&source, &expression);
        }
    }

    #[test]
    fn split_children_keep_separators_before_following_small_siblings() {
        let large = "a".repeat(6500);
        for expression in [
            format!("consume(\"{large}\", short_name)"),
            format!("[\"{large}\", short_name]"),
            format!("(\"{large}\", short_name)"),
        ] {
            let statements = format!("    {expression};\n").repeat(4);
            let source = format!("fn main() {{\n{statements}}}\n");
            assert_non_whitespace_coverage(&source, &expression);
        }

        let mut properties: String = (0..1200)
            .map(|index| format!("  field_{index}: string,\n"))
            .collect();
        properties.push_str(&format!("  oversized: \"{large}\",\n  short: string,\n"));
        let source = format!("type Fields = {{\n{properties}}};\n");
        let chunks = chunks_for_path("fields.ts", &source).unwrap();
        assert_chunks_cover(&chunks, &format!("\"{large}\", short: string"));
        assert!(
            chunks
                .iter()
                .all(|chunk| !matches!(chunk.code.trim(), "," | ";")),
            "a chunk contains only separators:\n{chunks:#?}"
        );
    }

    #[test]
    fn chunks_python_declarations_statements_and_decorators() {
        let source = r#"# module comment
import os
from os import path

TOP = 1

# leading comment
@decorator
def sync_func(a, b=2):
    """docstring"""
    x = a + b
    return x

async def async_func():
    await something()

@class_decorator
class Base:
    """class doc"""
    class_attr = 1

    @method_decorator
    def method(self):
        return self

    async def amethod(self):
        await thing()

if TOP:
    print("top")
"#;
        let chunks = chunks_for_path("service.py", source).unwrap();

        assert!(!chunks.is_empty());
        assert!(chunks.iter().all(|chunk| chunk.language == "python"));
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.relative_file_path == "service.py")
        );

        let sync = chunks
            .iter()
            .find(|chunk| chunk.code.contains("def sync_func"))
            .unwrap();
        assert!(
            sync.code
                .starts_with("# leading comment\n@decorator\ndef sync_func")
        );
        assert_eq!(sync.start_line, 7);
        assert_eq!(sync.end_line, 12);

        let async_function = chunks
            .iter()
            .find(|chunk| chunk.code.contains("async def async_func"))
            .unwrap();
        assert_eq!(async_function.start_line, 14);
        assert_eq!(async_function.end_line, 15);

        let class = chunks
            .iter()
            .find(|chunk| chunk.code.contains("class Base"))
            .unwrap();
        assert!(class.code.starts_with("@class_decorator\nclass Base"));
        assert!(class.code.contains("@method_decorator\n    def method"));
        assert!(class.code.contains("async def amethod"));

        assert!(chunks.iter().any(|chunk| chunk.code == "TOP = 1"));
        assert!(chunks.iter().any(|chunk| chunk.code.contains("if TOP:")));
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("def sync_func"))
                .count(),
            1
        );
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("class Base"))
                .count(),
            1
        );

        let lines: Vec<_> = source.lines().collect();
        for chunk in chunks {
            assert!(
                lines[(chunk.start_line - 1) as usize..chunk.end_line as usize]
                    .join("\n")
                    .contains(&chunk.code),
                "invalid range for {chunk:#?}"
            );
        }
    }

    #[test]
    fn oversized_decorated_python_function_keeps_header_and_statements() {
        let statements: Vec<_> = (0..1600)
            .map(|i| format!("    value_{i} = calculate({i})\n"))
            .collect();
        let source = format!(
            "# explanation\n@trace\nasync def produce():\n{}",
            statements.concat()
        );

        let chunks = chunks_for_path("worker.py", &source).unwrap();
        assert!(chunks.len() > 1);
        assert!(
            chunks[0]
                .code
                .starts_with("# explanation\n@trace\nasync def produce():")
        );
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(
            chunks
                .iter()
                .filter(|chunk| chunk.code.contains("async def produce"))
                .count(),
            1
        );
        for statement in statements {
            assert!(
                chunks
                    .iter()
                    .any(|chunk| chunk.code.contains(statement.trim())),
                "missing {statement}"
            );
        }
    }

    #[test]
    fn parser_recovery_keeps_following_declarations_searchable() {
        for (path, source, healthy) in [
            (
                "Recovered.cs",
                "public class Broken { void Run( { }\npublic record HealthyRecord(int Value);\n",
                "HealthyRecord",
            ),
            (
                "Recovered.java",
                "public class Broken { void run( { }\npublic record HealthyRecord(int value) {}\n",
                "HealthyRecord",
            ),
            (
                "recovered.c",
                "int broken( {\nint healthy_value(void) { return 42; }\n",
                "healthy_value",
            ),
            (
                "recovered.cpp",
                "class Broken { void run( { }\nint healthy_value() { return 42; }\n",
                "healthy_value",
            ),
        ] {
            let chunks = chunks_for_path(path, source).unwrap();
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(healthy)),
                "{path}: valid declaration after syntax damage was lost: {chunks:#?}"
            );
        }
    }

    #[test]
    fn oversized_member_splits_at_statements_and_keeps_its_header() {
        let statements = (0..1200)
            .map(|i| format!("        total += {i};\n"))
            .collect::<String>();
        // (path, source, the member's header line, a header attribute)
        let cases = [
            (
                "LargeCalculator.cs",
                format!(
                    "public class LargeCalculator\n{{\n    [Obsolete]\n    public int Calculate()\n    {{\n        var total = 0;\n{statements}        return total;\n    }}\n}}\n"
                ),
                "public int Calculate",
                "[Obsolete]",
            ),
            (
                "LargeCalculator.java",
                format!(
                    "public class LargeCalculator {{\n    @Deprecated\n    public int calculate() {{\n        int total = 0;\n{statements}        return total;\n    }}\n}}\n"
                ),
                "public int calculate",
                "@Deprecated",
            ),
            (
                "LargeCalculator.cpp",
                format!(
                    "class LargeCalculator {{\npublic:\n    int calculate() {{\n        int total = 0;\n{statements}        return total;\n    }}\n}};\n"
                ),
                "int calculate",
                "int calculate",
            ),
        ];
        for (path, source, header_line, attribute) in cases {
            let chunks = chunks_for_path(path, &source).unwrap();
            assert!(
                chunks.len() > 2,
                "{path}: oversized member was not split: {chunks:#?}"
            );
            let header = chunks
                .iter()
                .find(|chunk| chunk.code.contains(header_line))
                .unwrap();
            assert!(header.code.contains(attribute), "{path}");
            assert!(
                chunks
                    .iter()
                    .any(|chunk| chunk.code.contains("return total;")),
                "{path}"
            );
            assert!(
                chunks.iter().all(|chunk| source.contains(&chunk.code)),
                "{path}"
            );
        }
    }

    #[test]
    fn oversized_function_splits_at_statements_without_losing_them() {
        let rust: Vec<_> = (0..2000)
            .map(|i| format!("    let value_{i} = {i};\n"))
            .collect();
        let go: Vec<_> = (0..2000)
            .map(|i| format!("\tvalue{i} := {i}\n\t_ = value{i}\n"))
            .collect();
        let c: Vec<_> = (0..2000)
            .map(|i| format!("\tint value{i} = {i};\n\t(void)value{i};\n"))
            .collect();
        let cases = [
            (
                "lib.rs",
                format!("fn large() {{\n{}}}\n", rust.concat()),
                rust,
            ),
            (
                "large.go",
                format!("package example\n\nfunc large() {{\n{}}}\n", go.concat()),
                go,
            ),
            (
                "large.c",
                format!("int large(void) {{\n{}\treturn 0;\n}}\n", c.concat()),
                c,
            ),
        ];
        for (path, source, statements) in cases {
            let chunks = chunks_for_path(path, &source).unwrap();
            assert!(chunks.len() > 1, "{path}");
            for statement in statements {
                for line in statement.lines().filter(|line| !line.trim().is_empty()) {
                    assert!(
                        chunks.iter().any(|c| c.code.contains(line.trim())),
                        "{path}: missing statement: {line}"
                    );
                }
            }
            let lines: Vec<_> = source.lines().collect();
            for c in chunks {
                assert!(
                    lines[(c.start_line - 1) as usize..c.end_line as usize]
                        .join("\n")
                        .contains(&c.code),
                    "{path}: invalid range"
                );
            }
        }
    }

    #[test]
    fn chunks_go_and_c_declarations_with_leading_comments_attached() {
        let go = r"package example

// Add returns the sum of two numbers.
func Add(a, b int) int {
	return a + b
}

// Counter tracks a running total.
type Counter struct {
	Total int
}

// Increment adds n to the counter's total.
func (c *Counter) Increment(n int) {
	c.Total += n
}

const MaxRetries = 3

var DefaultTimeout = 30
";
        let c = r"#include <stdio.h>

// Add returns the sum of two numbers.
int add(int a, int b) {
    return a + b;
}

// Counter tracks a running total.
struct Counter {
    int total;
};

// increment adds n to the counter's total.
void increment(struct Counter *c, int n) {
    c->total += n;
}

#define MAX_RETRIES 3

int default_timeout = 30;
";
        // (path, language, source, [(declaration, the chunk starts with)], other
        // chunk fragments, a declaration that appears in exactly one chunk)
        let cases = [
            (
                "service.go",
                "go",
                go,
                [
                    (
                        "func Add",
                        "// Add returns the sum of two numbers.\nfunc Add",
                    ),
                    (
                        "type Counter struct",
                        "// Counter tracks a running total.\ntype Counter",
                    ),
                    (
                        "func (c *Counter) Increment",
                        "// Increment adds n to the counter's total.\nfunc (c *Counter) Increment",
                    ),
                ],
                ["const MaxRetries = 3", "var DefaultTimeout = 30"],
                "func Add",
            ),
            (
                "service.c",
                "c",
                c,
                [
                    ("int add", "// Add returns the sum of two numbers.\nint add"),
                    (
                        "struct Counter",
                        "// Counter tracks a running total.\nstruct Counter",
                    ),
                    (
                        "void increment",
                        "// increment adds n to the counter's total.\nvoid increment",
                    ),
                ],
                ["#define MAX_RETRIES 3", "int default_timeout = 30;"],
                "int add",
            ),
        ];
        for (path, language, source, attached, others, unique) in cases {
            let chunks = chunks_for_path(path, source).unwrap();

            assert!(!chunks.is_empty(), "{path}");
            assert!(chunks.iter().all(|chunk| chunk.language == language));
            assert!(chunks.iter().all(|chunk| chunk.relative_file_path == path));
            for (declaration, prefix) in attached {
                let chunk = chunks
                    .iter()
                    .find(|chunk| chunk.code.contains(declaration))
                    .unwrap_or_else(|| panic!("{path}: no chunk for {declaration}"));
                assert!(
                    chunk.code.starts_with(prefix),
                    "{path}: {declaration} lost its comment"
                );
            }
            for fragment in others {
                assert!(
                    chunks.iter().any(|chunk| chunk.code.contains(fragment)),
                    "{path}: missing {fragment}"
                );
            }
            assert_eq!(
                chunks
                    .iter()
                    .filter(|chunk| chunk.code.contains(unique))
                    .count(),
                1,
                "{path}"
            );

            let lines: Vec<_> = source.lines().collect();
            for chunk in chunks {
                assert!(
                    lines[(chunk.start_line - 1) as usize..chunk.end_line as usize]
                        .join("\n")
                        .contains(&chunk.code),
                    "{path}: invalid range for {chunk:#?}"
                );
            }
        }
    }

    #[test]
    fn chunks_java_csharp_and_cpp_declarations_keep_their_attachments() {
        let java = r#"package com.example.telemetry;

import java.util.List;

/**
 * Processes production telemetry.
 */
@Deprecated
public class TelemetryProcessor {
    private String name = "production";

    public TelemetryProcessor(String name) {
        this.name = name;
    }

    public String normalize(String value) {
        String trimmed = value.trim();
        return trimmed.toLowerCase();
    }
}

interface TelemetrySink {
    void write(String value);
}
"#;
        let csharp = r#"using System;

Console.WriteLine("startup");

namespace Telemetry;

/// <summary>Processes production telemetry.</summary>
[Obsolete]
public sealed class TelemetryProcessor
{
    public event EventHandler? Completed;
    public string Name { get; init; } = "production";

    public TelemetryProcessor(string name) { Name = name; }

    public string Normalize(string value)
    {
        string Trimmed() => value.Trim();
        return Trimmed().ToLowerInvariant();
    }
}

public interface ITelemetrySink
{
    void Write(string value);
}
"#;
        let cpp = r"namespace telemetry {

// Processes production telemetry events.
class Processor {
public:
    // Normalizes a single event value.
    std::string normalize(std::string value) {
        return value;
    }

private:
    int count_;
};

// add returns the sum of two values of any numeric type.
template<typename T>
T add(T a, T b) {
    return a + b;
}

}
";
        // (path, language, source, fragments some chunk must contain,
        // [(a chunk's identifying text, fragments that chunk must also contain)])
        type Case<'a> = (
            &'a str,
            &'a str,
            &'a str,
            &'a [&'a str],
            &'a [(&'a str, &'a [&'a str])],
        );
        let cases: [Case; 3] = [
            (
                "TelemetryProcessor.java",
                "java",
                java,
                &[
                    "import java.util.List",
                    "class TelemetryProcessor",
                    "private String name",
                    "TelemetryProcessor(String name)",
                    "String normalize",
                    "interface TelemetrySink",
                    "void write",
                ],
                &[("class TelemetryProcessor", &["/**", "@Deprecated"])],
            ),
            (
                "TelemetryProcessor.cs",
                "csharp",
                csharp,
                &[
                    "using System",
                    "Console.WriteLine",
                    "class TelemetryProcessor",
                    "event EventHandler",
                    "string Name",
                    "TelemetryProcessor(string name)",
                    "string Normalize",
                    "string Trimmed()",
                    "interface ITelemetrySink",
                    "void Write",
                ],
                &[("class TelemetryProcessor", &["/// <summary>", "[Obsolete]"])],
            ),
            (
                "processor.cpp",
                "cpp",
                cpp,
                &[
                    "class Processor",
                    "std::string normalize",
                    "int count_;",
                    "T add(T a, T b)",
                ],
                &[
                    (
                        "class Processor",
                        &["// Processes production telemetry events."],
                    ),
                    (
                        "T add(T a, T b)",
                        &[
                            "template<typename T>",
                            "// add returns the sum of two values of any numeric type.",
                        ],
                    ),
                ],
            ),
        ];
        for (path, language, source, expected, attachments) in cases {
            let chunks = chunks_for_path(path, source).unwrap();
            assert!(
                chunks.iter().all(|chunk| chunk.language == language),
                "{path}"
            );
            for fragment in expected {
                assert!(
                    chunks.iter().any(|chunk| chunk.code.contains(fragment)),
                    "{path}: missing {fragment}: {chunks:#?}"
                );
            }
            for (identity, fragments) in attachments {
                let chunk = chunks
                    .iter()
                    .find(|chunk| chunk.code.contains(identity))
                    .unwrap();
                for fragment in *fragments {
                    assert!(
                        chunk.code.contains(fragment),
                        "{path}: {identity} lost {fragment}"
                    );
                }
            }
            for chunk in &chunks {
                assert!(source.contains(&chunk.code), "{path}");
                assert_eq!(chunk.content_hash, hash(&chunk.code), "{path}");
                assert!(chunk.start_line >= 1, "{path}");
                assert!(chunk.end_line >= chunk.start_line, "{path}");
            }
        }
    }

    #[test]
    fn chunks_hpp_extension_as_cpp() {
        let source = "class Widget {\npublic:\n    int size() const { return 0; }\n};\n";
        let chunks = chunks_for_path("widget.hpp", source).unwrap();
        assert!(chunks.iter().all(|chunk| chunk.language == "cpp"));
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("class Widget"))
        );
    }

    #[test]
    fn chunks_cc_cxx_hh_and_hxx_extensions_as_cpp() {
        let source = "namespace geo {\n// Area of a unit square.\nclass Square {\npublic:\n    double area() const { return 1.0; }\n};\n}\n";
        for path in ["a.cc", "a.cxx", "a.hh", "a.hxx", "inc/deep/dir/a.hxx"] {
            let chunks = chunks_for_path(path, source).unwrap();
            assert!(!chunks.is_empty(), "{path}");
            for chunk in &chunks {
                assert_eq!(chunk.language, "cpp", "{path}");
                assert_eq!(chunk.relative_file_path, path);
                assert!(source.contains(&chunk.code), "{path}");
                assert_eq!(chunk.content_hash, hash(&chunk.code), "{path}");
                assert!(chunk.start_line >= 1 && chunk.end_line >= chunk.start_line);
            }
            // The leading comment stays attached to the class it describes.
            let class = chunks
                .iter()
                .find(|chunk| chunk.code.contains("class Square"))
                .unwrap_or_else(|| panic!("{path}: no class chunk: {chunks:#?}"));
            assert!(class.code.contains("// Area of a unit square."), "{path}");
            // The same text under `.cpp` gives identical chunks, so the
            // extension changes only the file name, never the output.
            let reference = chunks_for_path("a.cpp", source).unwrap();
            let strip = |chunks: &[Chunk]| {
                chunks
                    .iter()
                    .map(|chunk| (chunk.start_line, chunk.end_line, chunk.code.clone()))
                    .collect::<Vec<_>>()
            };
            assert_eq!(strip(&chunks), strip(&reference), "{path}");
        }
    }

    #[test]
    fn chunks_h_extension_as_c() {
        let source = "#ifndef MATH_H\n#define MATH_H\n\n/* adds two ints */\nint add(int a, int b);\n\ntypedef struct pair {\n    int a;\n    int b;\n} pair;\n\n#endif\n";
        let chunks = chunks_for_path("include/math.h", source).unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(chunk.language, "c");
            assert!(source.contains(&chunk.code));
            assert_eq!(chunk.content_hash, hash(&chunk.code));
            assert!(chunk.start_line >= 1 && chunk.end_line >= chunk.start_line);
        }
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("/* adds two ints */")
                    && chunk.code.contains("int add(int a, int b);"))
        );
        assert!(
            chunks
                .iter()
                .any(|chunk| chunk.code.contains("typedef struct pair"))
        );
        // Same text under `.c` gives the same chunks.
        let reference = chunks_for_path("math.c", source).unwrap();
        assert_eq!(
            chunks.iter().map(|chunk| &chunk.code).collect::<Vec<_>>(),
            reference
                .iter()
                .map(|chunk| &chunk.code)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_cpp_header_named_dot_h_stays_searchable_as_c() {
        // `.h` is registered as `c` whatever the header contains (a documented
        // decision, not content sniffing). The C grammar recovers around the
        // C++ constructs, so the file is still indexed, every chunk is exact
        // source, and the declarations are still findable by text.
        let source = "#pragma once\n#include <string>\n\nnamespace app {\n\n// A widget.\nclass Widget {\npublic:\n    int size() const { return 0; }\n    std::string name() const;\n};\n\ntemplate <typename T>\nT twice(T value) { return value + value; }\n\n}\n";
        let chunks = chunks_for_path("include/widget.h", source).unwrap();
        assert!(!chunks.is_empty());
        for chunk in &chunks {
            assert_eq!(chunk.language, "c");
            assert!(source.contains(&chunk.code), "{chunk:#?}");
            assert_eq!(chunk.content_hash, hash(&chunk.code));
            assert!(chunk.start_line >= 1 && chunk.end_line >= chunk.start_line);
        }
        for needle in ["class Widget", "int size() const", "T twice(T value)"] {
            assert!(
                chunks.iter().any(|chunk| chunk.code.contains(needle)),
                "missing {needle}: {chunks:#?}"
            );
        }
        assert_chunks_cover(&chunks, source);
    }

    // ---- the stat-gated hashes of the freshness checks (slice K38) ------------

    /// Sets a file's modified time (std, no sleeping).
    fn stamp(path: &Path, modified: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    /// One pass of [`source_hashes_with`] over `root` with a counting reader:
    /// the result and how many files the reader was asked for.
    fn gated_pass(root: &Path, gate: Option<&mut StatGate>) -> (SourceHashes, usize) {
        let reads = std::cell::Cell::new(0);
        let hashes = source_hashes_with(root, false, gate, |path| {
            reads.set(reads.get() + 1);
            std::fs::read(path)
        })
        .unwrap();
        assert_eq!(hashes.read_files, reads.get());
        (hashes, reads.get())
    }

    fn by_path(hashes: &SourceHashes) -> std::collections::BTreeMap<&str, &str> {
        hashes
            .files
            .iter()
            .map(|file| (file.relative.as_str(), file.content_hash.as_str()))
            .collect()
    }

    fn base_stamp() -> std::time::SystemTime {
        std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)
    }

    #[test]
    fn a_stat_gated_pass_reads_only_what_changed() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(root.join(name), format!("fn {}() {{}}\n", &name[..1])).unwrap();
            stamp(&root.join(name), base_stamp());
        }
        let mut gate = StatGate::default();

        let (first, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!((reads, first.listed_files, gate.len()), (3, 3, 3));
        // Nothing changed: nothing is read, and the hashes are the same.
        let (second, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!(reads, 0);
        assert_eq!(by_path(&second), by_path(&first));
        assert_eq!(second.listed_files, 3);

        // A new length: that file alone is read, with a new hash.
        let mut text = std::fs::read_to_string(root.join("a.rs")).unwrap();
        text.push_str("fn more() {}\n");
        std::fs::write(root.join("a.rs"), text).unwrap();
        // (An old stamp: a stamp near now is racy and is read every call, see
        // the racy test below.)
        stamp(
            &root.join("a.rs"),
            base_stamp() + std::time::Duration::from_secs(5),
        );
        let (third, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!(reads, 1);
        assert_ne!(by_path(&third)["a.rs"], by_path(&first)["a.rs"]);
        assert_eq!(by_path(&third)["b.rs"], by_path(&first)["b.rs"]);

        // A touch (a new modified time, the same bytes): read once, the hash
        // is unchanged.
        stamp(
            &root.join("b.rs"),
            base_stamp() + std::time::Duration::from_secs(1),
        );
        let (fourth, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!(reads, 1);
        assert_eq!(by_path(&fourth), by_path(&third));

        // A file deleted leaves the result and the gate; a file added is read.
        std::fs::remove_file(root.join("c.rs")).unwrap();
        std::fs::write(root.join("d.rs"), "fn d() {}\n").unwrap();
        stamp(
            &root.join("d.rs"),
            base_stamp() + std::time::Duration::from_secs(6),
        );
        let (fifth, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!(reads, 1);
        assert_eq!(
            by_path(&fifth).keys().copied().collect::<Vec<_>>(),
            ["a.rs", "b.rs", "d.rs"]
        );
        assert_eq!(gate.len(), 3);

        // The language and adapter version are the index's.
        let rust = language::for_extension("rs").unwrap();
        assert!(fifth.files.iter().all(|file| {
            file.language == rust.identifier() && file.adapter_version == rust.cache_version()
        }));

        // Without a gate every pass reads every file.
        assert_eq!(gated_pass(root, None).1, 3);
        assert_eq!(gated_pass(root, None).1, 3);
    }

    #[test]
    fn a_file_with_no_modified_time_is_never_gated_and_a_gate_entry_needs_one() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.rs"), "fn a() {}\n").unwrap();
        stamp(&temp.path().join("a.rs"), base_stamp());
        let mut gate = StatGate::default();
        // An entry with no modified time never matches, whatever the length.
        gate.entries.insert(
            "a.rs".into(),
            GateEntry {
                modified: None,
                len: 10,
                hash: Some("kept".into()),
            },
        );
        let (hashes, reads) = gated_pass(temp.path(), Some(&mut gate));
        assert_eq!(reads, 1);
        assert_ne!(by_path(&hashes)["a.rs"], "kept");
        assert!(gate.entries["a.rs"].modified.is_some());
    }

    #[test]
    fn the_stat_gate_misses_a_same_length_same_mtime_edit_and_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        stamp(&file, base_stamp());
        let mut gate = StatGate::default();
        let (before, _) = gated_pass(temp.path(), Some(&mut gate));

        // The documented gap: new bytes of the same length under the old stamp.
        std::fs::write(&file, "fn new() {}\n").unwrap();
        stamp(&file, base_stamp());
        let (gated, reads) = gated_pass(temp.path(), Some(&mut gate));
        assert_eq!(reads, 0);
        assert_eq!(by_path(&gated), by_path(&before), "the gated hash is stale");
        let (ungated, _) = gated_pass(temp.path(), None);
        assert_ne!(
            by_path(&ungated)["a.rs"],
            by_path(&before)["a.rs"],
            "the ungated hash sees the edit"
        );
        // The next edit that moves the stamp is seen.
        stamp(&file, base_stamp() + std::time::Duration::from_secs(1));
        let (after, reads) = gated_pass(temp.path(), Some(&mut gate));
        assert_eq!(reads, 1);
        assert_eq!(by_path(&after), by_path(&ungated));
    }

    #[test]
    fn a_stamp_near_now_is_racy_and_a_same_length_rewrite_is_seen() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("a.rs");
        // Just written: its stamp is within `RACY_WINDOW` of the read.
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let mut gate = StatGate::default();
        let (before, reads) = gated_pass(temp.path(), Some(&mut gate));
        assert_eq!(reads, 1);
        // The same length, no `set_modified`: whether or not the filesystem
        // moved the stamp, the racy file is read again and the new hash shows.
        std::fs::write(&file, "fn new() {}\n").unwrap();
        let (after, reads) = gated_pass(temp.path(), Some(&mut gate));
        assert_eq!(reads, 1, "a racy stamp is not trusted");
        assert_ne!(by_path(&after)["a.rs"], by_path(&before)["a.rs"]);
        assert_eq!(by_path(&after), by_path(&gated_pass(temp.path(), None).0));
        // A stamp older than the window is kept.
        stamp(&file, std::time::SystemTime::now() - RACY_WINDOW * 2);
        assert_eq!(gated_pass(temp.path(), Some(&mut gate)).1, 1);
        assert_eq!(gated_pass(temp.path(), Some(&mut gate)).1, 0);
        // A stamp in the future is racy too.
        stamp(&file, std::time::SystemTime::now() + RACY_WINDOW * 2);
        assert_eq!(gated_pass(temp.path(), Some(&mut gate)).1, 1);
        assert_eq!(gated_pass(temp.path(), Some(&mut gate)).1, 1);
    }

    #[test]
    fn gated_hashes_equal_the_scan_hashes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join("locale")).unwrap();
        std::fs::write(root.join("plain.rs"), "fn plain() {}\n").unwrap();
        std::fs::write(root.join("bom.rs"), "\u{feff}fn bom() {}\n").unwrap();
        std::fs::write(root.join("lossy.c"), b"int x = 1; /* caf\xe9 */\n").unwrap();
        std::fs::write(root.join("send.ts"), "export const a = 1;\n").unwrap();
        std::fs::write(root.join("locale/app_de.ts"), QT_LINGUIST_CATALOG).unwrap();
        std::fs::write(root.join("notes.md"), "# Notes\n\ntext\n").unwrap();
        // Old stamps, so the gate keeps them (a stamp near now is racy).
        for name in [
            "plain.rs",
            "bom.rs",
            "lossy.c",
            "send.ts",
            "locale/app_de.ts",
            "notes.md",
        ] {
            stamp(&root.join(name), base_stamp());
        }

        let scan = scan_incremental_with(root, &HashMap::new(), false).unwrap();
        let scanned: std::collections::BTreeMap<&str, &str> = scan
            .files
            .iter()
            .map(|(path, file)| (path.as_str(), file.content_hash.as_str()))
            .collect();
        assert!(!scanned.contains_key("locale/app_de.ts"));
        assert!(!scanned.contains_key("notes.md"));
        assert_eq!(scanned.len(), 4, "{scanned:?}");

        let mut gate = StatGate::default();
        let (first, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!(by_path(&first), scanned);
        // Five files are read (the catalog too); none again.
        assert_eq!(reads, 5);
        let (second, reads) = gated_pass(root, Some(&mut gate));
        assert_eq!(by_path(&second), scanned);
        assert_eq!(
            reads, 0,
            "a skipped file stays skipped while its stamp stays"
        );
        assert_eq!(second.listed_files, 5);
        assert_eq!(by_path(&gated_pass(root, None).0), scanned);

        // The same through the process-wide gate of the root.
        let global = source_hashes(root, false, true).unwrap();
        assert_eq!(by_path(&global), scanned);
        assert_eq!(global.read_files, 5);
        assert_eq!(source_hashes(root, false, true).unwrap().read_files, 0);
        assert_eq!(source_hashes(root, false, false).unwrap().read_files, 5);
    }

    #[test]
    fn the_stat_gate_registry_is_per_root() {
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let a = stat_gate(one.path());
        assert!(std::sync::Arc::ptr_eq(&a, &stat_gate(one.path())));
        assert!(!std::sync::Arc::ptr_eq(&a, &stat_gate(two.path())));
        // A root's gate does not hold up a call on another root.
        let _held = a.lock().unwrap();
        std::fs::write(two.path().join("a.rs"), "fn a() {}\n").unwrap();
        // Run on a thread with a bound, so a shared gate fails the test
        // instead of hanging it.
        let other = two.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(source_hashes(&other, false, true).map(|h| h.files.len()));
        });
        let files = receiver
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("a call on another root waited for the first root's gate");
        assert_eq!(files.unwrap(), 1);
    }
}
