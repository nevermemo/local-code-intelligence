//! The `grep.structural` map (cycle 1, slice K19): from a symbol, the places
//! where its name occurs in a given syntactic role, found by a text search
//! for the candidate files and confirmed by an ast-grep pattern.
//!
//! # What it does
//!
//! ripgrep names the files that hold the seed's name as a whole word (the
//! [`Candidates`] step); each of those files is parsed once by the chunker's
//! own tree-sitter grammar and searched with the patterns of
//! [`patterns`] (an `impl`, `extends` or `implements` header that names the
//! seed, a call of the seed). A word in a comment or a string matches no
//! pattern, which is the difference from `grep.live`. The cost is one parse
//! per candidate file, so it is bounded by the candidate list, not by the
//! tree, and no server runs.
//!
//! This module holds the matcher: [`scan_files`] over a list of candidate
//! files, with its caps and counters.
//!
//! # Caps
//!
//! At most [`StructuralOptions::max_candidates`] files are looked at (path
//! order), a file above `max_file_bytes` is skipped and counted, the scan
//! stops at the deadline and after `cap` hits. A name that is not a plain
//! identifier makes no scan.

mod lang;
mod patterns;

pub use lang::Lang;
pub use patterns::{Compiled, LANGUAGES, Role, Template, compile, instantiate, patterns};

use ast_grep_core::{AstGrep, tree_sitter::StrDoc};
use lci_core::{language::ADAPTERS, lexical};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::{Duration, Instant},
};

/// The id of the map.
pub const STRUCTURAL_ID: crate::MapId = "grep.structural";

/// Most files one scan reads: the candidate list is cut here, in path order.
pub const MAX_CANDIDATES: usize = 200;

/// Limits of one scan.
#[derive(Debug, Clone)]
pub struct StructuralOptions {
    /// Candidate files looked at, in path order.
    pub max_candidates: usize,
    /// Files above this size are not read.
    pub max_file_bytes: u64,
    /// The longest one scan runs (the call's own deadline also applies).
    pub timeout: Duration,
    /// Most hits one scan keeps.
    pub cap: usize,
}

impl Default for StructuralOptions {
    fn default() -> Self {
        Self {
            max_candidates: MAX_CANDIDATES,
            max_file_bytes: 1 << 20,
            timeout: Duration::from_secs(3),
            cap: crate::EXPAND_LIMIT,
        }
    }
}

/// The step that proposes files: those that hold `name` as a whole word.
#[crate::async_trait]
pub trait Candidates: Send + Sync {
    /// Paths relative to `root`, `/` separated, distinct, in path order, of
    /// the files with one of `extensions` that hold `name` as a whole word.
    async fn files_with_word(
        &self,
        root: &Path,
        name: &str,
        extensions: &[&str],
    ) -> Result<Vec<String>, String>;
}

/// ripgrep, through `lci_core::lexical::search_literals`
/// (`rg --fixed-strings --word-regexp`).
pub struct Ripgrep {
    path: String,
}

impl Ripgrep {
    pub fn new(path: impl Into<String>) -> Self {
        Self { path: path.into() }
    }
}

/// Lines per file asked of ripgrep: only the first whole-identifier line of a
/// file is needed, but ripgrep's word boundary is looser than the identifier
/// test, so a few lines are asked.
const LINES_PER_FILE: u32 = 5;

#[crate::async_trait]
impl Candidates for Ripgrep {
    async fn files_with_word(
        &self,
        root: &Path,
        name: &str,
        extensions: &[&str],
    ) -> Result<Vec<String>, String> {
        let found = lexical::search_literals(&self.path, root, &[name.to_owned()], LINES_PER_FILE)
            .await
            .map_err(|error| error.to_string())?;
        let mut files: Vec<String> = found
            .matches
            .into_iter()
            .map(|hit| hit.relative_path)
            .filter(|path| {
                let extension = path.rsplit('.').next().unwrap_or_default();
                extensions.contains(&extension)
            })
            .collect();
        if files.is_empty()
            && let Some(warning) = found.warning
        {
            return Err(warning);
        }
        files.sort();
        files.dedup();
        Ok(files)
    }
}

/// The test double: a fixed list, whatever the name.
pub struct Listed(pub Vec<String>);

#[crate::async_trait]
impl Candidates for Listed {
    async fn files_with_word(
        &self,
        _root: &Path,
        _name: &str,
        _extensions: &[&str],
    ) -> Result<Vec<String>, String> {
        Ok(self.0.clone())
    }
}

/// The file extensions of a language identifier.
pub fn extensions_of(identifier: &str) -> Vec<&'static str> {
    ADAPTERS
        .iter()
        .filter(|adapter| adapter.identifier() == identifier && !adapter.is_prose())
        .map(|adapter| adapter.glob().trim_start_matches("*."))
        .collect()
}

/// The extensions of every language that has patterns.
pub fn all_extensions() -> Vec<&'static str> {
    LANGUAGES
        .iter()
        .flat_map(|identifier| extensions_of(identifier))
        .collect()
}

/// One syntactic match of the name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Relative to the root, `/` separated.
    pub path: String,
    pub role: Role,
    /// 0-based line and byte column of the matched node's start, as SCIP
    /// counts.
    pub line: u32,
    pub col: u32,
    /// `live:` and the hex of the first 16 bytes of the SHA-256 of the file's
    /// bytes, as `grep.live` writes it: a blob no snapshot holds.
    pub blob: String,
    /// For a header: the bare name of the other party (the trait or type, the
    /// subclass), read from the pattern's `$T` or `$C`.
    pub other: Option<String>,
}

/// What one scan did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// In candidate order, then source order; at most `cap`.
    pub hits: Vec<Hit>,
    /// Candidate files taken from the list (after `max_candidates`).
    pub candidates: u64,
    pub files_parsed: u64,
    pub bytes_parsed: u64,
    pub skipped_large: u64,
    /// Templates that did not compile, once each.
    pub dropped: Vec<String>,
    /// Why the scan ended before the list did, if it did.
    pub stopped: Option<&'static str>,
}

/// The bare name of a type or trait as written in a header: generic
/// arguments, references, paths and qualifiers dropped (`&mut a::Wrapper<T>`
/// gives `Wrapper`). `None` when nothing identifier-like is left.
pub fn bare_name(text: &str) -> Option<String> {
    let head = text.split('<').next().unwrap_or(text);
    let last = head.rsplit(['.', ':']).next().unwrap_or(head);
    let name = last
        .trim()
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .next()
        .unwrap_or_default();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The SHA-256 of `bytes`, first 16 bytes in hex, behind `live:` (the form of
/// `grep.live`'s blobs).
fn live_blob(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(16)
        .fold(String::from("live:"), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// Whether `text` is a name worth searching: identifier characters only.
pub fn is_identifier(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Parses each of the `candidates` (paths relative to `root`) whose language
/// is `language` (every language with patterns when `None`) and keeps the
/// matches of the patterns for `name`. Blocking. A file that cannot be read,
/// is not UTF-8, or has no grammar is passed over without a count.
/// Deterministic for given bytes: candidate order, then source order.
pub fn scan_files(
    root: &Path,
    candidates: &[String],
    name: &str,
    language: Option<&str>,
    options: &StructuralOptions,
    deadline: Instant,
) -> Scan {
    let mut scan = Scan::default();
    if !is_identifier(name) || options.cap == 0 {
        return scan;
    }
    let taken = candidates.len().min(options.max_candidates);
    scan.candidates = taken as u64;
    let mut compiled: HashMap<&'static str, Compiled> = HashMap::new();
    let mut seen_dropped: HashSet<String> = HashSet::new();
    for path in &candidates[..taken] {
        if Instant::now() >= deadline {
            scan.stopped = Some("deadline");
            break;
        }
        let Some(lang) = Lang::for_path(Path::new(path)) else {
            continue;
        };
        if language.is_some_and(|wanted| wanted != lang.identifier()) {
            continue;
        }
        let full = root.join(path);
        if std::fs::metadata(&full).map_or(0, |meta| meta.len()) > options.max_file_bytes {
            scan.skipped_large += 1;
            continue;
        }
        let Ok(bytes) = std::fs::read(&full) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let set = compiled
            .entry(lang.identifier())
            .or_insert_with(|| Compiled::build(&lang, name));
        for dropped in &set.dropped {
            if seen_dropped.insert(dropped.clone()) {
                scan.dropped.push(dropped.clone());
            }
        }
        scan.files_parsed += 1;
        scan.bytes_parsed += bytes.len() as u64;
        let Ok(tree) = AstGrep::<StrDoc<Lang>>::try_new(text, lang) else {
            continue;
        };
        let root_node = tree.root();
        let blob = live_blob(&bytes);
        let mut found: Vec<Hit> = Vec::new();
        for (role, set) in [(Role::Header, &set.header), (Role::Call, &set.call)] {
            for pattern in set {
                for matched in root_node.find_all(pattern) {
                    let (line, col) = matched.get_node().start_pos().byte_point();
                    let env = matched.get_env();
                    // The other party: the pattern's `$T` or `$C`, else (a C++
                    // base clause, which cannot hold the class) the name of
                    // the class around the match.
                    let other = if role == Role::Header {
                        ["T", "C"]
                            .iter()
                            .find_map(|var| env.get_match(var).map(|node| node.text()))
                            .or_else(|| {
                                matched
                                    .get_node()
                                    .parent()
                                    .and_then(|parent| parent.field("name"))
                                    .map(|name| name.text())
                            })
                            .and_then(|other| bare_name(&other))
                    } else {
                        None
                    };
                    found.push(Hit {
                        path: path.clone(),
                        role,
                        line: line as u32,
                        col: col as u32,
                        blob: blob.clone(),
                        other,
                    });
                }
            }
        }
        found.sort_by(|a, b| {
            (a.line, a.col, a.role, &a.other).cmp(&(b.line, b.col, b.role, &b.other))
        });
        found.dedup();
        for hit in found {
            if scan.hits.len() >= options.cap {
                scan.stopped = Some("match cap");
                return scan;
            }
            scan.hits.push(hit);
        }
    }
    scan
}

#[cfg(test)]
mod tests;
