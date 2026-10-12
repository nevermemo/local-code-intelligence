//! The path-free per-file bundle sidecar of the multimap spike (slice S3;
//! spec `docs/spike/multimap/specs/node-id.md`, 3.4).
//!
//! A bundle is what one producer derived from one file's content: for
//! example the declarations `sym.defs` extracts. It is keyed by
//! `(file content_hash, language, adapter_version, producer)` and holds no
//! path (lesson L2), so a moved or copied file reuses it unchanged and the
//! path is joined only when the bundle is read for a snapshot.
//!
//! Bundles are plain JSON files under `<data_dir>/bundles/<workspace>/`, next
//! to the manifests: no new database (AGENTS.md, Hard rules). The store is
//! per workspace so that dropping the bundles no snapshot uses
//! ([`BundleStore::retain`]) never touches another workspace's. Nothing on
//! the scan or search path uses it yet.

use crate::workspace::hash;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

/// Bumped when the file layout below changes; a bundle of another format
/// reads as missing.
pub const FORMAT: u32 = 1;

/// Hex digits of the workspace id in the directory name, as in table names
/// (`ws_<16 hex>`), so the path stays short on Windows.
const WORKSPACE_DIGITS: usize = 16;
/// How old a temporary file must be before [`BundleStore::retain`] takes
/// it for the leftover of a killed writer and deletes it.
pub const STALE_TEMPORARY: std::time::Duration = std::time::Duration::from_secs(3600);
/// Hex digits of the key hash in a file name (128 bits; the full key is in
/// the file and checked on read, so a collision reads as missing).
const KEY_DIGITS: usize = 32;

/// What a bundle is keyed by. None of it is a path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BundleKey {
    /// The hash of the whole decoded file (`CachedFile::content_hash`).
    pub content_hash: String,
    /// `LanguageAdapter::identifier()`.
    pub language: String,
    /// `LanguageAdapter::cache_version()`: a new chunker or grammar may
    /// change what a producer derives, so it never reuses an older bundle.
    pub adapter_version: String,
    /// The producer's id and version, for example `sym.defs@1`.
    pub producer: String,
}

impl BundleKey {
    /// The key hash that names the bundle's file.
    fn digest(&self) -> String {
        let material = format!(
            "{}\0{}\0{}\0{}",
            self.content_hash, self.language, self.adapter_version, self.producer
        );
        hash(&material)[..KEY_DIGITS].to_owned()
    }
}

#[derive(Serialize)]
struct Written<'p, P: Serialize> {
    format: u32,
    key: &'p BundleKey,
    payload: &'p P,
}

#[derive(Deserialize)]
struct Read<P> {
    format: u32,
    key: BundleKey,
    payload: P,
}

/// The bundles of one workspace.
#[derive(Debug, Clone)]
pub struct BundleStore {
    root: PathBuf,
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

impl BundleStore {
    /// The store of `workspace_id` under `data_dir`. Creates nothing until
    /// the first [`BundleStore::put`].
    pub fn new(data_dir: &Path, workspace_id: &str) -> Self {
        let short: String = workspace_id.chars().take(WORKSPACE_DIGITS).collect();
        Self {
            root: data_dir.join("bundles").join(short),
        }
    }

    /// The directory this store owns.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where the bundle of `key` lives: `<root>/<2 hex>/<30 hex>.json`.
    pub fn path_of(&self, key: &BundleKey) -> PathBuf {
        let digest = key.digest();
        self.root
            .join(&digest[..2])
            .join(format!("{}.json", &digest[2..]))
    }

    /// The payload stored for `key`. A missing file, a file of another
    /// format, a file that does not parse as `P` and a file holding another
    /// key (a digest collision) are all `None`: the caller extracts again.
    /// A file that exists but cannot be read is an error.
    pub fn get<P: DeserializeOwned>(&self, key: &BundleKey) -> Result<Option<P>> {
        let path = self.path_of(key);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("read bundle {}", path.display()));
            }
        };
        match serde_json::from_slice::<Read<P>>(&bytes) {
            Ok(read) if read.format == FORMAT && read.key == *key => Ok(Some(read.payload)),
            Ok(read) => {
                tracing::debug!(
                    path = %path.display(),
                    format = read.format,
                    "bundle of another format or key; extracting again"
                );
                Ok(None)
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "unreadable bundle; extracting again");
                Ok(None)
            }
        }
    }

    /// Stores `payload` for `key`, replacing any previous bundle. The file
    /// is written beside its final name and renamed into place, so a reader
    /// never sees half a bundle. When the rename fails but a bundle of the
    /// same key is in place (another writer won, or Windows refused to
    /// replace a file someone holds open), that bundle is kept: a bundle is
    /// a function of its key, so it is the same payload.
    pub fn put<P: Serialize>(&self, key: &BundleKey, payload: &P) -> Result<()> {
        let path = self.path_of(key);
        let directory = path.parent().expect("a bundle path has a parent");
        std::fs::create_dir_all(directory)
            .with_context(|| format!("create bundle directory {}", directory.display()))?;
        let bytes = serde_json::to_vec(&Written {
            format: FORMAT,
            key,
            payload,
        })?;
        let temporary = directory.join(format!(
            ".{}.{}.{}.tmp",
            key.digest(),
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&temporary, &bytes)
            .with_context(|| format!("write bundle {}", temporary.display()))?;
        if let Err(error) = std::fs::rename(&temporary, &path) {
            let _ = std::fs::remove_file(&temporary);
            if matches!(self.get::<serde_json::Value>(key), Ok(Some(_))) {
                return Ok(());
            }
            return Err(error).with_context(|| format!("store bundle {}", path.display()));
        }
        Ok(())
    }

    /// Deletes every bundle file of this store whose key is not in `keep`;
    /// returns how many it deleted. `keep` must hold the keys of every
    /// producer that uses this store: a file name does not say which producer
    /// wrote it, so a bundle of a producer left out of `keep` is deleted (and
    /// extracted again by that producer's next sync). Only files named as
    /// this store names bundles are touched, plus temporary files of writes
    /// that did not finish within [`STALE_TEMPORARY`] (a killed writer).
    pub fn retain(&self, keep: &HashSet<BundleKey>) -> Result<usize> {
        let keep: HashSet<PathBuf> = keep.iter().map(|key| self.path_of(key)).collect();
        let shards = match std::fs::read_dir(&self.root) {
            Ok(shards) => shards,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => {
                return Err(error).with_context(|| format!("list bundles {}", self.root.display()));
            }
        };
        let is_hex = |name: &str, length: usize| {
            name.len() == length && name.bytes().all(|b| b.is_ascii_hexdigit())
        };
        let mut deleted = 0;
        let listed = |directory: &Path| format!("list bundles {}", directory.display());
        for shard in shards {
            let shard = shard.with_context(|| listed(&self.root))?;
            let shard_name = shard.file_name();
            let is_directory = shard
                .file_type()
                .with_context(|| listed(&shard.path()))?
                .is_dir();
            if !is_directory || !shard_name.to_str().is_some_and(|n| is_hex(n, 2)) {
                continue;
            }
            let shard_path = shard.path();
            for file in std::fs::read_dir(&shard_path).with_context(|| listed(&shard_path))? {
                let file = file.with_context(|| listed(&shard_path))?;
                let path = file.path();
                let name = path.file_name().and_then(|name| name.to_str());
                let named_as_bundle = name
                    .and_then(|name| name.strip_suffix(".json"))
                    .is_some_and(|stem| is_hex(stem, KEY_DIGITS - 2));
                let stale_temporary = name
                    .is_some_and(|name| name.starts_with('.') && name.ends_with(".tmp"))
                    && file
                        .metadata()
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > STALE_TEMPORARY);
                if stale_temporary {
                    // Best effort: a writer that just finished may have
                    // renamed it away.
                    if std::fs::remove_file(&path).is_ok() {
                        deleted += 1;
                    }
                    continue;
                }
                if named_as_bundle && !keep.contains(&path) {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("delete bundle {}", path.display()))?;
                    deleted += 1;
                }
            }
        }
        Ok(deleted)
    }
}

mod produce;

pub use produce::{
    DECLARATIONS, Declarations, Producer, StoredDeclaration, Synced, symbol_kind, sync,
};

#[cfg(test)]
mod tests;
