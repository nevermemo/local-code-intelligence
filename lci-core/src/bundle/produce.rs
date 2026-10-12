//! Producers fill the bundle store from the extraction hook, and [`sync`]
//! brings a store up to a scan's files: a file whose bundle exists is not
//! read; any other file is read, checked against the scan and extracted
//! with one parse.

use super::{BundleKey, BundleStore};
use crate::{
    chunk::{Extraction, decode_source, extract_for_path},
    language,
    manifest::CachedFile,
    node::{Declaration, SymbolKind},
    workspace::hash,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
};

/// Derives one bundle per file from the extraction hook's output.
pub trait Producer: Send + Sync {
    type Payload: Serialize + DeserializeOwned;

    /// Id and version, for example `sym.defs@1`. Bump the version whenever
    /// the payload for the same file content changes, including a change in
    /// the extractor the producer calls.
    fn id(&self) -> &str;

    /// Whether the producer has anything to say about files of `language`
    /// (`LanguageAdapter::identifier()`). Other files get no bundle and are
    /// never read.
    fn applies(&self, language: &str) -> bool;

    /// The payload of the file at `path`. The path is passed only because
    /// the hook needs it for the chunks: neither it nor anything derived
    /// from it may go into the payload (lesson L2). Contract C4 catches a
    /// producer that does at the first replayed move.
    fn produce(&self, path: &str, extraction: &Extraction) -> Self::Payload;

    /// The key of `file`'s bundle.
    fn key(&self, file: &CachedFile) -> BundleKey {
        BundleKey {
            content_hash: file.content_hash.clone(),
            language: file.language.clone(),
            adapter_version: file.adapter_version.clone(),
            producer: self.id().to_owned(),
        }
    }
}

/// What [`sync`] found and did.
#[derive(Debug)]
pub struct Synced<P> {
    /// The payload of every file the producer applies to, by path.
    pub payloads: BTreeMap<String, P>,
    /// The keys those payloads are stored under: what
    /// [`BundleStore::retain`] keeps.
    pub keys: HashSet<BundleKey>,
    /// Files whose bundle was already stored (moved and copied files
    /// included).
    pub reused: usize,
    /// Files read and extracted.
    pub extracted: usize,
}

/// Brings `store` up to `files` (a scan's or a manifest's files of the
/// workspace at `root`) and returns every payload. A file is read only when
/// its bundle is missing; it must still have the content and adapter the
/// scan recorded, or the sync fails and stores nothing more, so a caller can
/// keep its previous data (contract C3). Files are visited in path order.
pub fn sync<P: Producer>(
    store: &BundleStore,
    producer: &P,
    root: &Path,
    files: &HashMap<String, CachedFile>,
) -> Result<Synced<P::Payload>> {
    let mut paths: Vec<&String> = files.keys().collect();
    paths.sort();
    let mut synced = Synced {
        payloads: BTreeMap::new(),
        keys: HashSet::new(),
        reused: 0,
        extracted: 0,
    };
    for path in paths {
        let file = &files[path];
        if !producer.applies(&file.language) {
            continue;
        }
        let key = producer.key(file);
        let payload = match store.get(&key)? {
            Some(payload) => {
                synced.reused += 1;
                payload
            }
            None => {
                let payload = extract(producer, root, path, file)?;
                store.put(&key, &payload)?;
                synced.extracted += 1;
                payload
            }
        };
        synced.payloads.insert(path.clone(), payload);
        synced.keys.insert(key);
    }
    Ok(synced)
}

fn extract<P: Producer>(
    producer: &P,
    root: &Path,
    path: &str,
    file: &CachedFile,
) -> Result<P::Payload> {
    let adapter = language::for_path(Path::new(path))
        .with_context(|| format!("{path}: no language adapter"))?;
    if adapter.identifier() != file.language || adapter.cache_version() != file.adapter_version {
        bail!(
            "{path} was scanned as {} {}, but the adapter is now {} {}",
            file.language,
            file.adapter_version,
            adapter.identifier(),
            adapter.cache_version()
        );
    }
    let bytes = std::fs::read(root.join(path)).with_context(|| format!("read {path}"))?;
    let (source, _) = decode_source(bytes);
    if hash(&source) != file.content_hash {
        bail!("{path} changed since it was scanned");
    }
    let extraction = extract_for_path(path, &source)?;
    Ok(producer.produce(path, &extraction))
}

/// The producer id of the declaration bundles `sym.defs` reads.
pub const DECLARATIONS: &str = "sym.defs@1";

/// Rust declarations ([`crate::node::rust_declarations_in`]), path-free.
#[derive(Debug, Clone, Copy, Default)]
pub struct Declarations;

impl Producer for Declarations {
    type Payload = Vec<StoredDeclaration>;

    fn id(&self) -> &str {
        DECLARATIONS
    }

    fn applies(&self, language: &str) -> bool {
        language == "rust"
    }

    fn produce(&self, _path: &str, extraction: &Extraction) -> Self::Payload {
        extraction
            .declarations
            .iter()
            .flatten()
            .map(StoredDeclaration::from)
            .collect()
    }
}

/// A [`Declaration`] as a bundle stores it. It holds no path: the module
/// path is joined from the file's path when the bundle is read
/// ([`Declaration::key`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDeclaration {
    pub inline_modules: Vec<String>,
    pub container: Vec<String>,
    pub name: String,
    /// [`SymbolKind::as_str`].
    pub kind: String,
    pub disambiguator: u16,
    pub start_line: u32,
    pub end_line: u32,
}

impl From<&Declaration> for StoredDeclaration {
    fn from(declaration: &Declaration) -> Self {
        Self {
            inline_modules: declaration.inline_modules.clone(),
            container: declaration.container.clone(),
            name: declaration.name.clone(),
            kind: declaration.kind.as_str().to_owned(),
            disambiguator: declaration.disambiguator,
            start_line: declaration.start_line,
            end_line: declaration.end_line,
        }
    }
}

impl StoredDeclaration {
    /// The declaration back, or `None` for a kind this build does not know.
    pub fn declaration(&self) -> Option<Declaration> {
        Some(Declaration {
            inline_modules: self.inline_modules.clone(),
            container: self.container.clone(),
            name: self.name.clone(),
            kind: symbol_kind(&self.kind)?,
            disambiguator: self.disambiguator,
            start_line: self.start_line,
            end_line: self.end_line,
        })
    }
}

const KINDS: [SymbolKind; 11] = [
    SymbolKind::Function,
    SymbolKind::Method,
    SymbolKind::Struct,
    SymbolKind::Enum,
    SymbolKind::Union,
    SymbolKind::Trait,
    SymbolKind::Const,
    SymbolKind::Static,
    SymbolKind::TypeAlias,
    SymbolKind::Module,
    SymbolKind::Macro,
];

/// The kind whose [`SymbolKind::as_str`] is `name`.
pub fn symbol_kind(name: &str) -> Option<SymbolKind> {
    KINDS.into_iter().find(|kind| kind.as_str() == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails to compile when a kind is added, so [`KINDS`] cannot miss one.
    fn position(kind: SymbolKind) -> usize {
        match kind {
            SymbolKind::Function => 0,
            SymbolKind::Method => 1,
            SymbolKind::Struct => 2,
            SymbolKind::Enum => 3,
            SymbolKind::Union => 4,
            SymbolKind::Trait => 5,
            SymbolKind::Const => 6,
            SymbolKind::Static => 7,
            SymbolKind::TypeAlias => 8,
            SymbolKind::Module => 9,
            SymbolKind::Macro => 10,
        }
    }

    #[test]
    fn every_kind_round_trips_through_its_name() {
        for (index, kind) in KINDS.into_iter().enumerate() {
            assert_eq!(position(kind), index);
            assert_eq!(symbol_kind(kind.as_str()), Some(kind));
        }
        assert_eq!(symbol_kind("class"), None);
    }
}
