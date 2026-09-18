use crate::{chunk::Chunk, language, workspace::Workspace};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedFile {
    pub language: String,
    pub adapter_version: String,
    pub content_hash: String,
    pub chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    pub fingerprint: String,
    pub files: HashMap<String, CachedFile>,
}

impl Manifest {
    pub fn empty(workspace: &Workspace) -> Self {
        Self {
            version: VERSION,
            workspace_id: workspace.id.clone(),
            workspace_path: workspace.path.clone(),
            fingerprint: String::new(),
            files: HashMap::new(),
        }
    }

    pub fn is_compatible(&self, workspace: &Workspace) -> bool {
        self.version == VERSION
            && self.workspace_id == workspace.id
            && self.workspace_path == workspace.path
            && self.files.iter().all(|(path, file)| {
                language::for_path(Path::new(path)).is_some_and(|adapter| {
                    file.language == adapter.identifier()
                        && file.chunks.iter().all(|chunk| {
                            chunk.relative_file_path == *path
                                && chunk.language == file.language
                                && chunk.content_hash == crate::workspace::hash(&chunk.code)
                        })
                })
            })
    }
}

#[derive(Deserialize)]
struct ManifestV1 {
    version: u32,
    chunker_version: String,
    workspace_id: String,
    workspace_path: PathBuf,
    fingerprint: String,
    files: HashMap<String, CachedFileV1>,
}

#[derive(Deserialize)]
struct CachedFileV1 {
    content_hash: String,
    chunks: Vec<Chunk>,
}

impl ManifestV1 {
    fn migrate(self, workspace: &Workspace) -> Option<Manifest> {
        let rust = language::for_extension("rs").expect("Rust adapter must be registered");
        if self.version != 1
            || self.chunker_version != "rust-chunks-v1"
            || self.workspace_id != workspace.id
            || self.workspace_path != workspace.path
            || self.files.iter().any(|(path, file)| {
                language::for_path(Path::new(path))
                    .is_none_or(|adapter| adapter.identifier() != "rust")
                    || file.chunks.iter().any(|chunk| {
                        chunk.relative_file_path != *path
                            || chunk.language != "rust"
                            || chunk.content_hash != crate::workspace::hash(&chunk.code)
                    })
            })
        {
            return None;
        }
        Some(Manifest {
            version: VERSION,
            workspace_id: self.workspace_id,
            workspace_path: self.workspace_path,
            fingerprint: self.fingerprint,
            files: self
                .files
                .into_iter()
                .map(|(path, file)| {
                    (
                        path,
                        CachedFile {
                            language: rust.identifier().into(),
                            adapter_version: rust.cache_version().into(),
                            content_hash: file.content_hash,
                            chunks: file.chunks,
                        },
                    )
                })
                .collect(),
        })
    }
}

fn path(data_dir: &Path, workspace_id: &str) -> PathBuf {
    data_dir
        .join("manifests")
        .join(format!("{workspace_id}.json"))
}

pub fn load(data_dir: &Path, workspace: &Workspace) -> Manifest {
    let manifest_path = path(data_dir, &workspace.id);
    let Some(bytes) = std::fs::read(&manifest_path).ok() else {
        return Manifest::empty(workspace);
    };
    if let Ok(manifest) = serde_json::from_slice::<Manifest>(&bytes)
        && manifest.is_compatible(workspace)
    {
        return manifest;
    }
    serde_json::from_slice::<ManifestV1>(&bytes)
        .ok()
        .and_then(|manifest| manifest.migrate(workspace))
        .unwrap_or_else(|| Manifest::empty(workspace))
}

pub fn save(data_dir: &Path, manifest: &Manifest) -> Result<()> {
    let directory = data_dir.join("manifests");
    std::fs::create_dir_all(&directory).context("create manifest directory")?;
    let destination = path(data_dir, &manifest.workspace_id);
    let temporary = directory.join(format!("{}.json.tmp", manifest.workspace_id));
    std::fs::write(&temporary, serde_json::to_vec_pretty(manifest)?)
        .context("write workspace manifest")?;
    if destination.exists() {
        std::fs::remove_file(&destination).context("replace workspace manifest")?;
    }
    std::fs::rename(&temporary, &destination).context("commit workspace manifest")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{chunk, workspace::hash};

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Workspace) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        let data = temp.path().join("data");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let workspace = Workspace::resolve(&root, &data).unwrap();
        (temp, root, data, workspace)
    }

    #[test]
    fn migrates_compatible_v1_rust_cache_without_reparsing() {
        let (_temp, root, data, workspace) = fixture();
        let source = "// stable\nfn migrated() {}\n";
        std::fs::write(root.join("lib.rs"), source).unwrap();
        let source_hash = hash(source);
        let old_fingerprint = hash(&format!("lib.rs\0{source_hash}\n"));
        let legacy = serde_json::json!({
            "version": 1,
            "chunker_version": "rust-chunks-v1",
            "workspace_id": workspace.id,
            "workspace_path": workspace.path,
            "fingerprint": old_fingerprint,
            "files": {
                "lib.rs": {
                    "content_hash": source_hash,
                    "chunks": chunk::rust_chunks("lib.rs", source).unwrap()
                }
            }
        });
        let manifest_path = path(&data, &workspace.id);
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let migrated = load(&data, &workspace);
        assert_eq!(migrated.version, VERSION);
        assert_eq!(migrated.fingerprint, old_fingerprint);
        assert_eq!(migrated.files["lib.rs"].language, "rust");
        assert_eq!(
            migrated.files["lib.rs"].adapter_version,
            language::for_extension("rs").unwrap().cache_version()
        );
        let scan = chunk::scan_incremental(&root, &migrated.files).unwrap();
        assert_eq!(scan.parsed_files, 0);
        assert_eq!(scan.unchanged_files, 1);
        assert_ne!(scan.fingerprint, old_fingerprint);
    }

    #[test]
    fn rejects_incompatible_or_non_rust_v1_manifests() {
        let (_temp, _root, data, workspace) = fixture();
        for (version, chunker_version, file) in [
            (1, "other-version", "lib.rs"),
            (1, "rust-chunks-v1", "panel.ts"),
            (7, "rust-chunks-v1", "lib.rs"),
        ] {
            let legacy = serde_json::json!({
                "version": version,
                "chunker_version": chunker_version,
                "workspace_id": workspace.id,
                "workspace_path": workspace.path,
                "fingerprint": "old",
                "files": {
                    (file): {
                        "content_hash": "hash",
                        "chunks": []
                    }
                }
            });
            let manifest_path = path(&data, &workspace.id);
            std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
            std::fs::write(&manifest_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
            assert!(load(&data, &workspace).files.is_empty());
        }
        std::fs::write(path(&data, &workspace.id), b"not json").unwrap();
        assert!(load(&data, &workspace).files.is_empty());
    }

    #[test]
    fn rejects_structurally_inconsistent_v2_manifest() {
        let (_temp, _root, data, workspace) = fixture();
        let manifest = Manifest {
            version: VERSION,
            workspace_id: workspace.id.clone(),
            workspace_path: workspace.path.clone(),
            fingerprint: "fingerprint".into(),
            files: HashMap::from([(
                "panel.ts".into(),
                CachedFile {
                    language: "rust".into(),
                    adapter_version: "old-version".into(),
                    content_hash: "source".into(),
                    chunks: vec![],
                },
            )]),
        };
        let manifest_path = path(&data, &workspace.id);
        std::fs::create_dir_all(manifest_path.parent().unwrap()).unwrap();
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(load(&data, &workspace).files.is_empty());
    }
}
