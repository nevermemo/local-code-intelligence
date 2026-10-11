use crate::{
    chunk::Chunk,
    filter::{EffectiveFilter, SourceRole, classify_source_role},
    workspace::Workspace,
};
use anyhow::{Context, Result, ensure};
use arrow_array::{
    Array, ArrayRef, FixedSizeListArray, Float32Array, RecordBatch, StringArray, UInt32Array,
    types::Float32Type,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::{
    Connection, DistanceType, Table,
    database::CreateTableMode,
    query::{ExecutableQuery, QueryBase, Select},
};
use serde::Serialize;
use std::{collections::HashMap, path::Path, sync::Arc};

#[derive(Clone)]
pub struct Store {
    db: Connection,
}

#[derive(Clone, Debug, Serialize)]
pub struct Hit {
    pub file_path: String,
    #[serde(flatten)]
    pub chunk: Chunk,
    pub semantic_score: Option<f32>,
    pub semantic_rank: Option<usize>,
    pub lexical_rank: Option<usize>,
    /// Lines of the chunk credited with a scoring query term (see
    /// `lexical::score_chunks`); 0 when the lexical channel did not return it.
    pub lexical_match_count: u32,
    /// IDF-weighted lexical evidence behind `lexical_rank`. Distinct from the
    /// semantic, fusion, and reranker scores.
    pub lexical_score: Option<f32>,
    pub lsp_rank: Option<usize>,
    pub fusion_score: f32,
    pub retrieval_channels: Vec<String>,
    pub reranker_score: Option<f32>,
    pub source_role: SourceRole,
    pub source_role_prior: f32,
}

pub const fn source_role_prior(role: SourceRole) -> f32 {
    match role {
        SourceRole::Source => 0.004,
        _ => 0.0,
    }
}

/// Maximum bytes retained in a bounded evidence snippet, cut at a char boundary.
pub const SNIPPET_BUDGET: usize = 400;

/// Truncate code to the bounded snippet budget, marking truncation with a
/// trailing ellipsis. Short input is returned unchanged.
///
/// Lives here, not in `evaluate`, because the retrieval pipeline records the
/// same snippet in a candidate dump; `evaluate` re-exports both items.
pub fn bounded_snippet(code: &str) -> String {
    if code.len() <= SNIPPET_BUDGET {
        return code.to_owned();
    }
    let mut end = SNIPPET_BUDGET;
    while !code.is_char_boundary(end) {
        end -= 1;
    }
    let mut out: String = code[..end].to_owned();
    out.push('…');
    out
}

pub struct Snapshot {
    pub table: Table,
    pub identity: String,
    pub dimension: usize,
    pub indexed_at: String,
}

/// The key under which a chunk's stored vector is cached between index passes
/// (`Store::cached_vectors`) and looked up while indexing.
///
/// A vector depends only on the chunk's code unless the embedding document
/// template mentions `{path}` (`uses_path`); then it also depends on the file,
/// so the key adds the path and identical code in two files, or a file moved to
/// a new path, is embedded afresh. Without `{path}` the key is the bare content
/// hash, exactly as before the template existed, so existing indexes keep
/// working. The separator (U+001F, a control character) cannot occur in a
/// content hash, so the combined key is unambiguous.
pub(crate) fn vector_key(uses_path: bool, content_hash: &str, path: &str) -> String {
    if uses_path {
        format!("{content_hash}\u{1f}{path}")
    } else {
        content_hash.to_owned()
    }
}

impl Store {
    pub async fn open(path: &Path) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        Ok(Self {
            db: lancedb::connect(path.to_str().context("data path must be UTF-8")?)
                .execute()
                .await?,
        })
    }

    /// Smallest non-mutating accessibility probe: list table names.
    pub async fn accessibility_probe(&self, _path: &Path) -> Result<()> {
        self.db.table_names().execute().await?;
        Ok(())
    }

    /// Opens `name`, or `None` when no such table exists.
    async fn open_table(&self, name: &str) -> Result<Option<Table>> {
        match self.db.open_table(name).execute().await {
            Ok(table) => Ok(Some(table)),
            Err(lancedb::Error::TableNotFound { .. }) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn snapshot(&self, workspace: &Workspace) -> Result<Option<Snapshot>> {
        // An index written before the table name was shortened lives under the
        // legacy name until the workspace's next commit moves it (`replace`).
        let name = workspace.table_name();
        let (name, table) = match self.open_table(&name).await? {
            Some(table) => (name, table),
            None => {
                let legacy = workspace.legacy_table_name();
                match self.open_table(&legacy).await? {
                    Some(table) => (legacy, table),
                    None => return Ok(None),
                }
            }
        };
        let schema = table.schema().await?;
        // The short name keeps only 64 bits of the workspace id. A table that
        // records another workspace path is a collision (or a hand-copied
        // table): fail loudly instead of serving or overwriting its index.
        // Tables without the key predate it and are accepted.
        if let Some(stored) = schema.metadata().get("workspace_path") {
            let current = workspace.path.to_string_lossy();
            ensure!(
                stored.as_str() == current,
                "index table {name} was written for workspace {stored}, not {current}: \
                 table-name collision; give one workspace a different data_dir or delete the table"
            );
        }
        let identity = schema
            .metadata()
            .get("embedding_identity")
            .context("missing index embedding identity")?
            .clone();
        let dimension = match schema.field_with_name("vector")?.data_type() {
            DataType::FixedSizeList(_, n) => *n as usize,
            _ => anyhow::bail!("invalid vector schema"),
        };
        let indexed_at = schema
            .metadata()
            .get("indexed_at")
            .cloned()
            .unwrap_or_default();
        Ok(Some(Snapshot {
            table,
            identity,
            dimension,
            indexed_at,
        }))
    }

    /// Every stored vector, keyed by `vector_key` (`uses_path` as the
    /// caller's document template decides it), for reuse by the next index
    /// pass. With `uses_path` the file path column is read as well.
    pub async fn cached_vectors(
        snapshot: &Snapshot,
        uses_path: bool,
    ) -> Result<HashMap<String, Vec<f32>>> {
        let columns: &[&str] = if uses_path {
            &["content_hash", "relative_file_path", "vector"]
        } else {
            &["content_hash", "vector"]
        };
        let batches = snapshot
            .table
            .query()
            .select(Select::columns(columns))
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let mut cache = HashMap::new();
        for batch in batches {
            let hashes = column::<StringArray>(&batch, "content_hash")?;
            let paths = if uses_path {
                Some(column::<StringArray>(&batch, "relative_file_path")?)
            } else {
                None
            };
            let vectors = column::<FixedSizeListArray>(&batch, "vector")?;
            for i in 0..batch.num_rows() {
                let value = vectors.value(i);
                let vector = value
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .context("invalid vector values")?;
                let path = paths.map_or("", |paths| paths.value(i));
                cache.insert(
                    vector_key(uses_path, hashes.value(i), path),
                    vector.values().to_vec(),
                );
            }
        }
        Ok(cache)
    }

    pub async fn replace(
        &self,
        workspace: &Workspace,
        chunks: &[Chunk],
        vectors: &[Vec<f32>],
        identity: &str,
        dimension: usize,
    ) -> Result<()> {
        ensure!(
            chunks.len() == vectors.len() && dimension > 0,
            "invalid chunk/vector dimensions"
        );
        ensure!(
            vectors.iter().all(|v| v.len() == dimension),
            "embedding dimension changed; reindex with a stable model"
        );
        let mut fields = vec![];
        for name in [
            "workspace_id",
            "relative_file_path",
            "language",
            "code",
            "content_hash",
        ] {
            fields.push(Field::new(name, DataType::Utf8, false));
        }
        fields.push(Field::new("start_line", DataType::UInt32, false));
        fields.push(Field::new("end_line", DataType::UInt32, false));
        fields.push(Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dimension as i32,
            ),
            true,
        ));
        let metadata = HashMap::from([
            ("embedding_identity".into(), identity.into()),
            (
                "workspace_path".into(),
                workspace.path.to_string_lossy().into_owned(),
            ),
            (
                "indexed_at".into(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs()
                    .to_string(),
            ),
        ]);
        let schema = Arc::new(Schema::new_with_metadata(fields, metadata));
        let array = |values: Vec<String>| -> ArrayRef { Arc::new(StringArray::from(values)) };
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                array(chunks.iter().map(|_| workspace.id.clone()).collect()),
                array(
                    chunks
                        .iter()
                        .map(|c| c.relative_file_path.clone())
                        .collect(),
                ),
                array(chunks.iter().map(|c| c.language.clone()).collect()),
                array(chunks.iter().map(|c| c.code.clone()).collect()),
                array(chunks.iter().map(|c| c.content_hash.clone()).collect()),
                Arc::new(UInt32Array::from(
                    chunks.iter().map(|c| c.start_line).collect::<Vec<_>>(),
                )),
                Arc::new(UInt32Array::from(
                    chunks.iter().map(|c| c.end_line).collect::<Vec<_>>(),
                )),
                Arc::new(
                    FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                        vectors
                            .iter()
                            .map(|v| Some(v.iter().copied().map(Some).collect::<Vec<_>>())),
                        dimension as i32,
                    ),
                ),
            ],
        )?;
        self.db
            .create_table(workspace.table_name(), batch)
            .mode(CreateTableMode::Overwrite)
            .execute()
            .await?;
        self.drop_legacy_table(workspace).await;
        Ok(())
    }

    /// Removes the index an earlier version left under the long table name,
    /// now that the same workspace's index is committed under the short one.
    /// A failure never fails the commit: the short table already serves, and
    /// the next commit retries (a Windows "in use" error, say).
    ///
    /// A crash between the commit and this drop leaves both tables;
    /// `snapshot` prefers the short one. Callers hold `<data_dir>/index.lock`,
    /// so two writers never race on the drop.
    async fn drop_legacy_table(&self, workspace: &Workspace) {
        let legacy = workspace.legacy_table_name();
        match self.db.drop_table(&legacy, &[]).await {
            Ok(()) => {
                tracing::info!(workspace_id = %workspace.id, table = %legacy, "migrated the index to the short table name")
            }
            Err(lancedb::Error::TableNotFound { .. }) => {}
            Err(error) => {
                tracing::warn!(workspace_id = %workspace.id, table = %legacy, error = %error, "could not drop the legacy-named index table; the next commit retries")
            }
        }
    }

    pub async fn search(
        snapshot: &Snapshot,
        workspace: &Workspace,
        vector: &[f32],
        limit: usize,
        filters: &EffectiveFilter,
    ) -> Result<Vec<Hit>> {
        ensure!(
            vector.len() == snapshot.dimension,
            "query embedding dimension differs from index; reindex workspace"
        );
        // LanceDB cannot express every shared glob/role predicate. Keep the
        // fallback bounded while allowing selective filters more headroom.
        let oversampled_limit = limit.saturating_mul(4).min(160);
        let batches = snapshot
            .table
            .query()
            .nearest_to(vector)?
            .distance_type(DistanceType::Cosine)
            .limit(oversampled_limit)
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let mut hits = Vec::new();
        for batch in batches {
            for i in 0..batch.num_rows() {
                let text = |name| -> Result<String> {
                    Ok(column::<StringArray>(&batch, name)?.value(i).into())
                };
                let path = text("relative_file_path")?;
                let language = text("language")?;
                let source_role = classify_source_role(&path);
                if !filters.matches(&language, &path, source_role) {
                    continue;
                }
                let distance = column::<Float32Array>(&batch, "_distance")?.value(i);
                ensure!(distance.is_finite(), "nonfinite semantic distance");
                let rank = hits.len() + 1;
                hits.push(Hit {
                    file_path: workspace.path.join(&path).to_string_lossy().into_owned(),
                    chunk: Chunk {
                        relative_file_path: path,
                        language,
                        start_line: column::<UInt32Array>(&batch, "start_line")?.value(i),
                        end_line: column::<UInt32Array>(&batch, "end_line")?.value(i),
                        code: text("code")?,
                        content_hash: text("content_hash")?,
                    },
                    semantic_score: Some(1.0 - distance),
                    semantic_rank: Some(rank),
                    lexical_rank: None,
                    lexical_match_count: 0,
                    lexical_score: None,
                    lsp_rank: None,
                    fusion_score: 0.0,
                    retrieval_channels: vec!["semantic".into()],
                    reranker_score: None,
                    source_role,
                    source_role_prior: source_role_prior(source_role),
                });
                if hits.len() == limit {
                    return Ok(hits);
                }
            }
        }
        Ok(hits)
    }

    pub async fn chunks(snapshot: &Snapshot, workspace: &Workspace) -> Result<Vec<Hit>> {
        let batches = snapshot
            .table
            .query()
            .select(Select::columns(&[
                "relative_file_path",
                "language",
                "start_line",
                "end_line",
                "code",
                "content_hash",
            ]))
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let mut hits = Vec::new();
        for batch in batches {
            for i in 0..batch.num_rows() {
                let text = |name| -> Result<String> {
                    Ok(column::<StringArray>(&batch, name)?.value(i).into())
                };
                let path = text("relative_file_path")?;
                let source_role = classify_source_role(&path);
                hits.push(Hit {
                    file_path: workspace.path.join(&path).to_string_lossy().into_owned(),
                    chunk: Chunk {
                        relative_file_path: path,
                        language: text("language")?,
                        start_line: column::<UInt32Array>(&batch, "start_line")?.value(i),
                        end_line: column::<UInt32Array>(&batch, "end_line")?.value(i),
                        code: text("code")?,
                        content_hash: text("content_hash")?,
                    },
                    semantic_score: None,
                    semantic_rank: None,
                    lexical_rank: None,
                    lexical_match_count: 0,
                    lexical_score: None,
                    lsp_rank: None,
                    fusion_score: 0.0,
                    retrieval_channels: vec![],
                    reranker_score: None,
                    source_role,
                    source_role_prior: source_role_prior(source_role),
                });
            }
        }
        Ok(hits)
    }
}

fn column<'a, T: 'static>(batch: &'a RecordBatch, name: &str) -> Result<&'a T> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing column {name}"))?
        .as_any()
        .downcast_ref::<T>()
        .with_context(|| format!("invalid column {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "4f2a9c";

    #[test]
    fn bounded_snippet_limits_multibyte_input_by_bytes_at_a_char_boundary() {
        let code = "界😀".repeat(SNIPPET_BUDGET);
        assert!(code.len() > SNIPPET_BUDGET);
        assert!(!code.is_char_boundary(SNIPPET_BUDGET));
        let snippet = bounded_snippet(&code);
        let retained = snippet.strip_suffix('…').unwrap();
        assert!(snippet.len() <= SNIPPET_BUDGET + '…'.len_utf8());
        assert!(code.is_char_boundary(retained.len()));
        assert_eq!(retained, "界😀".repeat(SNIPPET_BUDGET / "界😀".len()));
    }

    #[test]
    fn without_path_the_key_is_the_bare_content_hash() {
        assert_eq!(vector_key(false, HASH, "src/a.rs"), HASH);
        // The path is ignored, so a chunk repeated across files shares one key.
        assert_eq!(
            vector_key(false, HASH, "src/a.rs"),
            vector_key(false, HASH, "src/b.rs")
        );
    }

    #[test]
    fn with_path_the_key_tells_files_apart() {
        let a = vector_key(true, HASH, "src/a.rs");
        let b = vector_key(true, HASH, "src/b.rs");
        assert_ne!(a, b);
        assert_ne!(a, HASH);
        assert_eq!(a, vector_key(true, HASH, "src/a.rs"));
        // Another chunk of the same file has another key.
        assert_ne!(a, vector_key(true, "0e11d7", "src/a.rs"));
    }

    fn workspace_at(path: &str) -> Workspace {
        Workspace {
            id: crate::workspace::hash(path),
            path: path.into(),
        }
    }

    async fn commit(store: &Store, workspace: &Workspace, codes: &[&str]) {
        let chunks: Vec<Chunk> = codes
            .iter()
            .enumerate()
            .map(|(i, code)| Chunk {
                relative_file_path: format!("src/f{i}.rs"),
                language: "rust".into(),
                start_line: 1,
                end_line: 1,
                code: (*code).into(),
                content_hash: crate::workspace::hash(code),
            })
            .collect();
        let vectors = vec![vec![1.0, 0.0, 0.0]; chunks.len()];
        store
            .replace(workspace, &chunks, &vectors, "identity", 3)
            .await
            .unwrap();
    }

    fn table_dir(lancedb: &Path, name: &str) -> std::path::PathBuf {
        lancedb.join(format!("{name}.lance"))
    }

    #[tokio::test]
    async fn snapshot_rejects_a_table_written_for_another_workspace_path() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let a = workspace_at("/projects/a");
        commit(&store, &a, &["fn a() {}"]).await;

        // The same id (hence the same table name) under another path is what a
        // short-name collision looks like to the store.
        let b = Workspace {
            id: a.id.clone(),
            path: "/projects/other".into(),
        };
        let error = match store.snapshot(&b).await {
            Ok(_) => panic!("a table written for another workspace path must not be served"),
            Err(error) => format!("{error:#}"),
        };
        assert!(error.contains(&a.table_name()), "{error}");
        assert!(error.contains("/projects/a"), "{error}");
        assert!(error.contains("/projects/other"), "{error}");
        assert!(error.contains("collision"), "{error}");
        assert!(store.snapshot(&a).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn snapshot_reads_a_legacy_named_table_and_replace_migrates_it() {
        let temp = tempfile::tempdir().unwrap();
        let lancedb = temp.path().join("lancedb");
        let store = Store::open(&lancedb).await.unwrap();
        let a = workspace_at("/projects/a");
        commit(&store, &a, &["fn a() {}", "fn b() {}"]).await;

        // An index an earlier version wrote: the same Lance dataset under the
        // full-id name. Datasets are relocatable, so a rename is the fixture.
        let short = table_dir(&lancedb, &a.table_name());
        let legacy = table_dir(&lancedb, &a.legacy_table_name());
        assert!(short.is_dir(), "{}", short.display());
        std::fs::rename(&short, &legacy).unwrap();

        let snapshot = store.snapshot(&a).await.unwrap().expect("legacy table");
        assert_eq!(snapshot.dimension, 3);
        assert_eq!(snapshot.identity, "identity");
        assert_eq!(snapshot.table.count_rows(None).await.unwrap(), 2);
        drop(snapshot);
        assert!(!short.exists());

        // The next commit writes the short name and removes the legacy table.
        commit(&store, &a, &["fn a() {}", "fn b() {}", "fn c() {}"]).await;
        assert!(short.is_dir());
        assert!(!legacy.exists());
        let snapshot = store.snapshot(&a).await.unwrap().expect("short table");
        assert_eq!(snapshot.table.count_rows(None).await.unwrap(), 3);

        // With nothing left to migrate, a commit leaves the short table alone.
        commit(&store, &a, &["fn a() {}"]).await;
        assert!(short.is_dir());
        assert!(!legacy.exists());
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let target = to.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_dir(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), target).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn snapshot_prefers_the_short_table_when_both_exist_and_replace_drops_the_legacy_one() {
        // A crash between the short table's commit and the legacy drop leaves
        // both tables behind.
        let temp = tempfile::tempdir().unwrap();
        let lancedb = temp.path().join("lancedb");
        let store = Store::open(&lancedb).await.unwrap();
        let a = workspace_at("/projects/a");
        commit(&store, &a, &["fn a() {}", "fn b() {}"]).await;
        let short = table_dir(&lancedb, &a.table_name());
        let legacy = table_dir(&lancedb, &a.legacy_table_name());
        copy_dir(&short, &legacy);

        let snapshot = store.snapshot(&a).await.unwrap().expect("a table");
        assert_eq!(snapshot.table.name(), a.table_name());
        assert_eq!(snapshot.table.count_rows(None).await.unwrap(), 2);
        drop(snapshot);

        commit(&store, &a, &["fn a() {}", "fn b() {}", "fn c() {}"]).await;
        assert!(short.is_dir());
        assert!(!legacy.exists());
        let snapshot = store.snapshot(&a).await.unwrap().expect("short table");
        assert_eq!(snapshot.table.count_rows(None).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn an_absent_legacy_table_is_reported_as_not_found() {
        // `replace` relies on this to tell "nothing to migrate" from a failed
        // drop; a lancedb change that reports it otherwise would make every
        // commit log a warning.
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let a = workspace_at("/projects/a");
        let error = store
            .db
            .drop_table(a.legacy_table_name(), &[])
            .await
            .unwrap_err();
        assert!(
            matches!(error, lancedb::Error::TableNotFound { .. }),
            "{error:?}"
        );
    }
}
