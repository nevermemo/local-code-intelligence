//! Stable node identity for the multimap spike (slice S1).
//!
//! One identity space for every node a map can return or take as a seed:
//! file, chunk, symbol, commit, test, config key, doc section and string
//! literal. The canonical string ([`NodeId`]'s `Display`) is the identity:
//! two maps that reach the same node produce the same string, and
//! [`NodeId::from_str`] accepts exactly the strings `Display` produces.
//! Spec: `docs/spike/multimap/specs/node-id.md`.
//!
//! Nothing here is stored: chunk ids are derived from the chunk lists a scan
//! or the manifest already holds ([`chunk_ids`]), so the manifest format is
//! unchanged, and nothing on the search path uses this module yet.

mod rust;

pub use rust::{Declaration, rust_declarations, rust_declarations_in, rust_module_path};

use crate::chunk::Chunk;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{borrow::Borrow, cmp::Ordering, collections::HashMap, fmt, str::FromStr};

/// The kinds of node, in the order of the spec's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NodeKind {
    File,
    Chunk,
    Symbol,
    Commit,
    Test,
    ConfigKey,
    DocSection,
    StringLiteral,
}

impl NodeKind {
    pub const ALL: [NodeKind; 8] = [
        Self::File,
        Self::Chunk,
        Self::Symbol,
        Self::Commit,
        Self::Test,
        Self::ConfigKey,
        Self::DocSection,
        Self::StringLiteral,
    ];

    /// The prefix of the canonical string, before the first `:`.
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Chunk => "chunk",
            Self::Symbol => "sym",
            Self::Commit => "commit",
            Self::Test => "test",
            Self::ConfigKey => "cfg",
            Self::DocSection => "doc",
            Self::StringLiteral => "str",
        }
    }
}

/// The test runner whose own name identifies a test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TestRunner {
    Cargo,
    Pytest,
    Jest,
    Go,
}

/// Where a configuration key lives (see the spec, 3.2 "ConfigKey").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigNamespace {
    /// LCI's own TOML keys (`config.example.toml`).
    Toml,
    /// `LCI_*` environment variables.
    Env,
    /// `Cargo.toml` keys.
    Cargo,
    /// Other repositories' configuration files.
    Repo,
}

/// The kind of a declaration in a [`SymbolKey`]. The first 11 come from the Rust
/// extractor and the SCIP index; `Field`, `Variable`, `Interface` and `Class`
/// come from language-server outlines only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Method,
    Struct,
    Enum,
    Union,
    Trait,
    Const,
    Static,
    TypeAlias,
    Module,
    Macro,
    Field,
    Variable,
    Interface,
    Class,
}

macro_rules! names {
    ($type:ty { $($variant:ident => $name:literal),+ $(,)? }) => {
        impl $type {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name),+ }
            }

            fn from_name(name: &str) -> Option<Self> {
                match name { $($name => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

names!(TestRunner { Cargo => "cargo", Pytest => "pytest", Jest => "jest", Go => "go" });
names!(ConfigNamespace { Toml => "toml", Env => "env", Cargo => "cargo", Repo => "repo" });
names!(SymbolKind {
    Function => "fn",
    Method => "method",
    Struct => "struct",
    Enum => "enum",
    Union => "union",
    Trait => "trait",
    Const => "const",
    Static => "static",
    TypeAlias => "type",
    Module => "mod",
    Macro => "macro",
    Field => "field",
    Variable => "variable",
    Interface => "interface",
    Class => "class",
});

/// The per-declaration key (lesson L1): language, module path, container
/// chain including the impl header, name, kind and a disambiguator that is
/// `0` unless the rest of the key collides inside one file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SymbolKey {
    /// `LanguageAdapter::identifier()`.
    pub language: String,
    /// Derived from the file path when the key is joined (for Rust,
    /// [`rust_module_path`]) plus inline modules; never stored per file.
    pub module_path: Vec<String>,
    /// Enclosing impl headers (`impl Map for LexicalMap`) and traits
    /// (`trait Map`), outermost first. Each segment holds a space (keyword,
    /// then the rest), which the canonical string relies on.
    pub container: Vec<String>,
    pub name: String,
    pub kind: SymbolKind,
    pub disambiguator: u16,
}

/// A node's identity. Construct with the checked constructors or check with
/// [`NodeId::validate`]: the canonical string round-trips for every valid id.
///
/// Fields are plain strings: `content_hash` is whatever the scanner stored
/// for the chunk (SHA-256 hex in practice), and paths follow the scanner's
/// normalisation (decision N-7).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeId {
    File {
        path: String,
    },
    Chunk {
        path: String,
        content_hash: String,
        ordinal: u32,
    },
    Symbol(SymbolKey),
    Commit {
        sha: String,
    },
    Test {
        runner: TestRunner,
        name: String,
    },
    ConfigKey {
        namespace: ConfigNamespace,
        key: String,
    },
    DocSection {
        path: String,
        heading_path: Vec<String>,
        ordinal: u32,
    },
    StringLiteral {
        text_hash: String,
    },
}

/// A string that is not the canonical form of any valid [`NodeId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseNodeIdError {
    input: String,
    reason: &'static str,
}

impl fmt::Display for ParseNodeIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid node id {:?}: {}", self.input, self.reason)
    }
}

impl std::error::Error for ParseNodeIdError {}

/// Characters percent-encoded inside a path: the separators that follow a
/// path in some kinds (`@`, `#`, `~`). `/` stays as written.
const PATH_SPECIALS: &[char] = &['@', '#', '~'];
/// Characters percent-encoded inside one segment (a hash, a slug, a symbol
/// segment or name).
const SEGMENT_SPECIALS: &[char] = &['@', '#', '~', '/', ':'];
/// A module segment or a symbol name: a segment that also never holds a raw
/// space, which is how a parser tells it from a container segment.
const MODULE_SPECIALS: &[char] = &['@', '#', '~', '/', ':', ' '];
/// Free text that runs to the end of the string (a test name, a config key).
const TEXT_SPECIALS: &[char] = &[];

/// Percent-encodes `%`, control characters and `specials`, byte by byte of
/// their UTF-8 form, with upper-case hex.
fn encode(value: &str, specials: &[char]) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if c == '%' || c.is_control() || specials.contains(&c) {
            let mut buffer = [0; 4];
            for byte in c.encode_utf8(&mut buffer).bytes() {
                out.push_str(&format!("%{byte:02X}"));
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The inverse of [`encode`]. Whether the encoding was canonical is checked
/// once for the whole id, by formatting the parsed id again.
fn decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = value.get(index + 1..index + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn is_hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl NodeId {
    pub fn file(path: impl Into<String>) -> Result<Self, ParseNodeIdError> {
        Self::File { path: path.into() }.validated()
    }

    pub fn chunk(
        path: impl Into<String>,
        content_hash: impl Into<String>,
        ordinal: u32,
    ) -> Result<Self, ParseNodeIdError> {
        Self::Chunk {
            path: path.into(),
            content_hash: content_hash.into(),
            ordinal,
        }
        .validated()
    }

    pub fn kind(&self) -> NodeKind {
        match self {
            Self::File { .. } => NodeKind::File,
            Self::Chunk { .. } => NodeKind::Chunk,
            Self::Symbol(_) => NodeKind::Symbol,
            Self::Commit { .. } => NodeKind::Commit,
            Self::Test { .. } => NodeKind::Test,
            Self::ConfigKey { .. } => NodeKind::ConfigKey,
            Self::DocSection { .. } => NodeKind::DocSection,
            Self::StringLiteral { .. } => NodeKind::StringLiteral,
        }
    }

    /// The file a node belongs to, for kinds that name one. Global kinds
    /// (commit, test, config key, string literal) and symbols (named by
    /// module path, not file) return `None`.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path } | Self::Chunk { path, .. } | Self::DocSection { path, .. } => {
                Some(path)
            }
            _ => None,
        }
    }

    fn validated(self) -> Result<Self, ParseNodeIdError> {
        self.validate().map(|()| self)
    }

    /// Checks the invariants the canonical string relies on: non-empty
    /// paths, hashes, names and segments, no NUL in a path, and full
    /// lower-case hex object ids and literal hashes.
    pub fn validate(&self) -> Result<(), ParseNodeIdError> {
        let fail = |reason| {
            Err(ParseNodeIdError {
                input: format!("{self:?}"),
                reason,
            })
        };
        let bad_path = |path: &str| path.is_empty() || path.contains('\0');
        let any_empty = |segments: &[String]| segments.iter().any(String::is_empty);
        match self {
            Self::File { path } if bad_path(path) => fail("empty path or NUL in path"),
            Self::Chunk {
                path, content_hash, ..
            } if bad_path(path) || content_hash.is_empty() => fail("empty path or content hash"),
            Self::Symbol(key)
                if key.language.is_empty()
                    || key.name.is_empty()
                    || any_empty(&key.module_path)
                    || key.container.iter().any(|segment| !segment.contains(' ')) =>
            {
                fail("empty symbol segment, or a container segment without a space")
            }
            Self::Commit { sha } if !is_hex(sha, &[40, 64]) => {
                fail("commit id is not 40 or 64 lower-case hex digits")
            }
            Self::Test { name, .. } if name.is_empty() => fail("empty test name"),
            Self::ConfigKey { key, .. } if key.is_empty() => fail("empty config key"),
            Self::DocSection {
                path, heading_path, ..
            } if bad_path(path) || any_empty(heading_path) => fail("empty path or heading slug"),
            Self::StringLiteral { text_hash } if !is_hex(text_hash, &[64]) => {
                fail("literal hash is not 64 lower-case hex digits")
            }
            _ => Ok(()),
        }
    }
}

fn join_segments(segments: &[String], separator: &str) -> String {
    segments
        .iter()
        .map(|segment| encode(segment, SEGMENT_SPECIALS))
        .collect::<Vec<_>>()
        .join(separator)
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = self.kind().prefix();
        match self {
            Self::File { path } => write!(f, "{prefix}:{}", encode(path, PATH_SPECIALS)),
            Self::Chunk {
                path,
                content_hash,
                ordinal,
            } => write!(
                f,
                "{prefix}:{}@{}#{ordinal}",
                encode(path, PATH_SPECIALS),
                encode(content_hash, SEGMENT_SPECIALS)
            ),
            Self::Symbol(key) => {
                let segments = key
                    .module_path
                    .iter()
                    .map(|segment| encode(segment, MODULE_SPECIALS))
                    .chain(
                        key.container
                            .iter()
                            .map(|segment| encode(segment, SEGMENT_SPECIALS)),
                    )
                    .chain([format!(
                        "{}/{}",
                        encode(&key.name, MODULE_SPECIALS),
                        key.kind.as_str()
                    )])
                    .collect::<Vec<_>>();
                write!(
                    f,
                    "{prefix}:{}:{}",
                    encode(&key.language, SEGMENT_SPECIALS),
                    segments.join("::")
                )?;
                if key.disambiguator > 0 {
                    write!(f, "~{}", key.disambiguator)?;
                }
                Ok(())
            }
            Self::Commit { sha } => write!(f, "{prefix}:{sha}"),
            Self::Test { runner, name } => write!(
                f,
                "{prefix}:{}:{}",
                runner.as_str(),
                encode(name, TEXT_SPECIALS)
            ),
            Self::ConfigKey { namespace, key } => write!(
                f,
                "{prefix}:{}:{}",
                namespace.as_str(),
                encode(key, TEXT_SPECIALS)
            ),
            Self::DocSection {
                path,
                heading_path,
                ordinal,
            } => {
                write!(
                    f,
                    "{prefix}:{}#{}",
                    encode(path, PATH_SPECIALS),
                    join_segments(heading_path, "/")
                )?;
                if *ordinal > 0 {
                    write!(f, "~{ordinal}")?;
                }
                Ok(())
            }
            Self::StringLiteral { text_hash } => write!(f, "{prefix}:{text_hash}"),
        }
    }
}

/// Splits `value` on `separator` into decoded segments; the empty string is
/// the empty list.
fn split_segments(value: &str, separator: &str) -> Option<Vec<String>> {
    if value.is_empty() {
        return Some(Vec::new());
    }
    value.split(separator).map(decode).collect()
}

/// `<body>~<n>` with `n > 0`, or `<body>` and `0`.
fn split_counter(value: &str) -> Option<(&str, u32)> {
    match value.rsplit_once('~') {
        Some((body, counter)) => Some((body, counter.parse().ok().filter(|&n| n > 0)?)),
        None => Some((value, 0)),
    }
}

fn parse_unchecked(input: &str) -> Option<NodeId> {
    let (prefix, rest) = input.split_once(':')?;
    let kind = NodeKind::ALL
        .into_iter()
        .find(|kind| kind.prefix() == prefix)?;
    Some(match kind {
        NodeKind::File => NodeId::File {
            path: decode(rest)?,
        },
        NodeKind::Chunk => {
            let (head, ordinal) = rest.rsplit_once('#')?;
            let (path, content_hash) = head.split_once('@')?;
            NodeId::Chunk {
                path: decode(path)?,
                content_hash: decode(content_hash)?,
                ordinal: ordinal.parse().ok()?,
            }
        }
        NodeKind::Symbol => {
            let (language, rest) = rest.split_once(':')?;
            let (rest, disambiguator) = split_counter(rest)?;
            let mut segments: Vec<&str> = rest.split("::").collect();
            let (name, kind) = segments.pop()?.rsplit_once('/')?;
            // A module segment never holds a raw space and a container
            // segment always does (`impl App`, `trait Map`).
            let modules = segments.iter().take_while(|s| !s.contains(' ')).count();
            NodeId::Symbol(SymbolKey {
                language: decode(language)?,
                module_path: segments[..modules]
                    .iter()
                    .map(|s| decode(s))
                    .collect::<Option<_>>()?,
                container: segments[modules..]
                    .iter()
                    .map(|s| decode(s))
                    .collect::<Option<_>>()?,
                name: decode(name)?,
                kind: SymbolKind::from_name(kind)?,
                disambiguator: u16::try_from(disambiguator).ok()?,
            })
        }
        NodeKind::Commit => NodeId::Commit { sha: rest.into() },
        NodeKind::Test => {
            let (runner, name) = rest.split_once(':')?;
            NodeId::Test {
                runner: TestRunner::from_name(runner)?,
                name: decode(name)?,
            }
        }
        NodeKind::ConfigKey => {
            let (namespace, key) = rest.split_once(':')?;
            NodeId::ConfigKey {
                namespace: ConfigNamespace::from_name(namespace)?,
                key: decode(key)?,
            }
        }
        NodeKind::DocSection => {
            let (path, headings) = rest.split_once('#')?;
            let (headings, ordinal) = split_counter(headings)?;
            NodeId::DocSection {
                path: decode(path)?,
                heading_path: split_segments(headings, "/")?,
                ordinal,
            }
        }
        NodeKind::StringLiteral => NodeId::StringLiteral {
            text_hash: rest.into(),
        },
    })
}

impl FromStr for NodeId {
    type Err = ParseNodeIdError;

    /// Accepts exactly the canonical strings of valid ids: anything that
    /// parses but formats differently (`%41` for `A`, `#01`, `~0`) is
    /// refused, so the string is the identity.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let fail = |reason| ParseNodeIdError {
            input: input.into(),
            reason,
        };
        let id = parse_unchecked(input).ok_or_else(|| fail("malformed"))?;
        id.validate().map_err(|error| fail(error.reason))?;
        if id.to_string() != input {
            return Err(fail("not in canonical form"));
        }
        Ok(id)
    }
}

/// Ordered by canonical string (the spec's tie order for non-chunk nodes).
/// Two invalid ids can share a string (an empty heading slug and none);
/// they are then ordered by their debug form, so `Ord` agrees with `Eq`.
impl Ord for NodeId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.to_string()
            .cmp(&other.to_string())
            .then_with(|| format!("{self:?}").cmp(&format!("{other:?}")))
    }
}

impl PartialOrd for NodeId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Today's fusion key of a chunk (`src/app/search.rs`, `fuse_hits`): it
/// includes `start_line`, which a NodeId does not.
pub fn legacy_fusion_key(chunk: &Chunk) -> (String, u32, String) {
    (
        chunk.relative_file_path.clone(),
        chunk.start_line,
        chunk.content_hash.clone(),
    )
}

/// The Chunk NodeId of every chunk, aligned with `chunks`.
///
/// The ordinal of a chunk is its position among the chunks of the same file
/// with the same `content_hash`, in source order: `start_line`, then
/// `end_line`, then the position in `chunks` (so a scan's or a manifest
/// file's own order, which is byte order, decides two chunks on the same
/// lines). Any order of `chunks` gives the same ids, except for that last
/// tie.
pub fn chunk_ids<C: Borrow<Chunk>>(chunks: &[C]) -> Vec<NodeId> {
    let chunks: Vec<&Chunk> = chunks.iter().map(Borrow::borrow).collect();
    let mut order: Vec<usize> = (0..chunks.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (chunks[a], chunks[b]);
        (&a.relative_file_path, a.start_line, a.end_line).cmp(&(
            &b.relative_file_path,
            b.start_line,
            b.end_line,
        ))
    });
    let mut seen = HashMap::<(&str, &str), u32>::new();
    let mut ids = vec![None; chunks.len()];
    for index in order {
        let chunk = chunks[index];
        let ordinal = seen
            .entry((&chunk.relative_file_path, &chunk.content_hash))
            .or_insert(0);
        ids[index] = Some(NodeId::Chunk {
            path: chunk.relative_file_path.clone(),
            content_hash: chunk.content_hash.clone(),
            ordinal: *ordinal,
        });
        *ordinal += 1;
    }
    ids.into_iter().map(|id| id.expect("every index")).collect()
}

/// The Chunk NodeIds of a manifest's (or a scan's) per-file chunk lists.
pub fn manifest_chunk_ids<'a>(
    files: impl IntoIterator<Item = &'a crate::manifest::CachedFile>,
) -> Vec<NodeId> {
    files
        .into_iter()
        .flat_map(|file| chunk_ids(&file.chunks))
        .collect()
}

#[cfg(test)]
mod tests;
