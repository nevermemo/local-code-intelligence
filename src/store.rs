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
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

#[derive(Clone)]
pub struct Store {
    db: Connection,
    legacy_readers: Arc<tokio::sync::RwLock<()>>,
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

/// The most rows a filtered semantic search fetches while looking for
/// chunks that pass its filter (160 rows times 4 to the third).
pub const MAX_FILTERED_SEMANTIC_ROWS: usize = 10_240;

/// What [`Store::search`] found.
pub struct SemanticHits {
    pub hits: Vec<Hit>,
    /// Set when the filter left fewer hits than asked for after the largest
    /// fetch, [`MAX_FILTERED_SEMANTIC_ROWS`] rows: chunks that pass the
    /// filter but lie farther from the query were not considered.
    pub capped_at_rows: Option<usize>,
}

impl SemanticHits {
    /// The search warning for a fetch the row cap stopped, if it was.
    pub fn cap_warning(&self) -> Option<String> {
        self.capped_at_rows.map(|rows| {
            format!(
                "Semantic retrieval checked the {rows} nearest chunks and only {} matched the filters; matching chunks farther from the query were not considered",
                self.hits.len()
            )
        })
    }
}

pub struct Snapshot {
    pub table: Table,
    pub identity: String,
    pub dimension: usize,
    pub indexed_at: String,
    _legacy_read_guard: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
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
            legacy_readers: Arc::default(),
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
        let (name, table, legacy_read_guard) = match self.open_table(&name).await? {
            Some(table) => (name, table, None),
            None => {
                let legacy = workspace.legacy_table_name();
                let guard = self.legacy_readers.clone().read_owned().await;
                match self.open_table(&legacy).await? {
                    Some(table) => (legacy, table, Some(guard)),
                    None => return Ok(None),
                }
            }
        };
        // Keep metadata and every later read on this exact revision, even
        // when an index job overwrites the table while a search is running.
        let table = table.checkout_current().await?;
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
            _legacy_read_guard: legacy_read_guard,
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
        // A pinned revision still needs its files. Defer deletion while any
        // legacy snapshot is in use; a later commit retries cleanup.
        let Ok(_exclusive) = self.legacy_readers.try_write() else {
            return;
        };
        let legacy = workspace.legacy_table_name();
        match self.db.drop_table(&legacy, &[]).await {
            Ok(()) => {
                tracing::info!(workspace_id = %workspace.id, table = %legacy, "migrated the index to the short table name");
            }
            Err(lancedb::Error::TableNotFound { .. }) => {}
            Err(error) => {
                tracing::warn!(workspace_id = %workspace.id, table = %legacy, error = %error, "could not drop the legacy-named index table; the next commit retries");
            }
        }
    }

    /// The `limit` nearest chunks that pass `filters`, nearest first.
    /// `matching` is how many chunks in the snapshot pass `filters` (the
    /// caller has counted them), so a search stops once it has found them all.
    ///
    /// LanceDB cannot express every shared glob/role predicate, so rows are
    /// filtered here after a nearest-row fetch. The first fetch asks for
    /// `min(limit * 4, 160)` rows. Only when the filter rejected one of them
    /// and fewer than `min(limit, matching)` passed is the fetch repeated with
    /// four times as many rows, up to [`MAX_FILTERED_SEMANTIC_ROWS`]; a search
    /// whose filter rejects nothing (every unfiltered search) runs the first
    /// fetch alone.
    pub async fn search(
        snapshot: &Snapshot,
        workspace: &Workspace,
        vector: &[f32],
        limit: usize,
        filters: &EffectiveFilter,
        matching: usize,
    ) -> Result<SemanticHits> {
        Self::search_within(
            snapshot,
            workspace,
            vector,
            limit,
            filters,
            matching,
            MAX_FILTERED_SEMANTIC_ROWS,
        )
        .await
    }

    async fn search_within(
        snapshot: &Snapshot,
        workspace: &Workspace,
        vector: &[f32],
        limit: usize,
        filters: &EffectiveFilter,
        matching: usize,
        max_rows: usize,
    ) -> Result<SemanticHits> {
        ensure!(
            vector.len() == snapshot.dimension,
            "query embedding dimension differs from index; reindex workspace"
        );
        let mut fetch = limit.saturating_mul(4).min(160);
        loop {
            let batches = snapshot
                .table
                .query()
                .nearest_to(vector)?
                .distance_type(DistanceType::Cosine)
                .limit(fetch)
                .execute()
                .await?
                .try_collect::<Vec<_>>()
                .await?;
            let (hits, rows, rejected) = Self::filtered_hits(&batches, workspace, limit, filters)?;
            // `rows < fetch` means the table is exhausted: LanceDB scans every
            // row (no vector index is built), so a fetch returns fewer rows
            // only when there are no more. An approximate index would break
            // this and must revisit it.
            if hits.len() >= limit.min(matching) || rejected == 0 || rows < fetch {
                return Ok(SemanticHits {
                    hits,
                    capped_at_rows: None,
                });
            }
            if fetch >= max_rows {
                return Ok(SemanticHits {
                    hits,
                    capped_at_rows: Some(fetch),
                });
            }
            fetch = fetch.saturating_mul(4).min(max_rows);
        }
    }

    /// The hits among `batches` (nearest first) that pass `filters`, at most
    /// `limit`, with how many rows were read and how many the filter rejected.
    fn filtered_hits(
        batches: &[RecordBatch],
        workspace: &Workspace,
        limit: usize,
        filters: &EffectiveFilter,
    ) -> Result<(Vec<Hit>, usize, usize)> {
        let mut hits = Vec::new();
        let (mut rows, mut rejected) = (0, 0);
        for batch in batches {
            for i in 0..batch.num_rows() {
                rows += 1;
                let text = |name| -> Result<String> {
                    Ok(column::<StringArray>(batch, name)?.value(i).into())
                };
                let path = text("relative_file_path")?;
                let language = text("language")?;
                let source_role = classify_source_role(&path);
                if !filters.matches(&language, &path, source_role) {
                    rejected += 1;
                    continue;
                }
                let distance = column::<Float32Array>(batch, "_distance")?.value(i);
                ensure!(distance.is_finite(), "nonfinite semantic distance");
                let rank = hits.len() + 1;
                hits.push(Hit {
                    file_path: workspace.path.join(&path).to_string_lossy().into_owned(),
                    chunk: Chunk {
                        relative_file_path: path,
                        language,
                        start_line: column::<UInt32Array>(batch, "start_line")?.value(i),
                        end_line: column::<UInt32Array>(batch, "end_line")?.value(i),
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
                    return Ok((hits, rows, rejected));
                }
            }
        }
        Ok((hits, rows, rejected))
    }

    /// The stored vector of the chunk of `path` with `content_hash` that
    /// starts at `start_line`, if the index holds one.
    pub async fn chunk_vector(
        snapshot: &Snapshot,
        path: &str,
        content_hash: &str,
        start_line: u32,
    ) -> Result<Option<Vec<f32>>> {
        let quoted = |text: &str| text.replace('\'', "''");
        let batches = snapshot
            .table
            .query()
            .only_if(format!(
                "relative_file_path = '{}' AND content_hash = '{}' AND start_line = {start_line}",
                quoted(path),
                quoted(content_hash)
            ))
            .select(Select::columns(&["vector"]))
            .limit(1)
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        for batch in batches {
            if batch.num_rows() == 0 {
                continue;
            }
            let vectors = column::<FixedSizeListArray>(&batch, "vector")?;
            let value = vectors.value(0);
            let vector = value
                .as_any()
                .downcast_ref::<Float32Array>()
                .context("invalid vector values")?;
            return Ok(Some(vector.values().to_vec()));
        }
        Ok(None)
    }

    pub async fn languages(snapshot: &Snapshot) -> Result<HashSet<String>> {
        let mut batches = snapshot
            .table
            .query()
            .select(Select::columns(&["language"]))
            .execute()
            .await?;
        let mut languages = HashSet::new();
        while let Some(batch) = batches.try_next().await? {
            let column = column::<StringArray>(&batch, "language")?;
            for i in 0..batch.num_rows() {
                languages.insert(column.value(i).to_owned());
            }
        }
        Ok(languages)
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
    async fn snapshot_retains_its_revision_after_a_concurrent_commit() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = Workspace {
            id: crate::workspace::hash("pinned"),
            path: temp.path().join("workspace"),
        };
        commit(&store, &workspace, &["fn previous_snapshot() {}"]).await;
        let previous = store.snapshot(&workspace).await.unwrap().unwrap();
        commit(
            &store,
            &workspace,
            &["fn replacement_snapshot() {}", "fn added() {}"],
        )
        .await;

        assert_eq!(previous.table.count_rows(None).await.unwrap(), 1);
        let chunks = Store::chunks(&previous, &workspace).await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk.code, "fn previous_snapshot() {}");
        let hits = Store::search(
            &previous,
            &workspace,
            &[1.0, 0.0, 0.0],
            8,
            &EffectiveFilter::from_request(&Default::default()).unwrap(),
            1,
        )
        .await
        .unwrap()
        .hits;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].chunk.code, "fn previous_snapshot() {}");
        let current = store.snapshot(&workspace).await.unwrap().unwrap();
        assert_eq!(current.table.count_rows(None).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn languages_match_distinct_chunk_languages_in_a_multilanguage_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = Workspace {
            id: crate::workspace::hash("multilanguage"),
            path: temp.path().join("workspace"),
        };
        let chunks: Vec<Chunk> = [
            ("src/a.rs", "rust", "fn a() {}"),
            ("src/b.py", "python", "def b(): pass"),
            ("src/c.rs", "rust", "fn c() {}"),
        ]
        .into_iter()
        .map(|(path, language, code)| Chunk {
            relative_file_path: path.into(),
            language: language.into(),
            start_line: 1,
            end_line: 1,
            code: code.into(),
            content_hash: crate::workspace::hash(code),
        })
        .collect();
        store
            .replace(
                &workspace,
                &chunks,
                &vec![vec![1.0, 0.0, 0.0]; chunks.len()],
                "identity",
                3,
            )
            .await
            .unwrap();
        let snapshot = store.snapshot(&workspace).await.unwrap().unwrap();
        let expected: HashSet<String> = Store::chunks(&snapshot, &workspace)
            .await
            .unwrap()
            .into_iter()
            .map(|hit| hit.chunk.language)
            .collect();
        assert_eq!(expected.len(), 2);
        assert_eq!(Store::languages(&snapshot).await.unwrap(), expected);
    }

    #[tokio::test]
    async fn languages_are_empty_for_an_empty_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = Workspace {
            id: crate::workspace::hash("empty"),
            path: temp.path().join("workspace"),
        };
        store
            .replace(&workspace, &[], &[], "identity", 3)
            .await
            .unwrap();
        let snapshot = store.snapshot(&workspace).await.unwrap().unwrap();
        assert!(Store::languages(&snapshot).await.unwrap().is_empty());
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
    async fn legacy_snapshot_stays_readable_during_migration() {
        let temp = tempfile::tempdir().unwrap();
        let lancedb = temp.path().join("lancedb");
        let store = Store::open(&lancedb).await.unwrap();
        let workspace = Workspace {
            id: crate::workspace::hash("legacy-pinned"),
            path: temp.path().join("workspace"),
        };
        commit(&store, &workspace, &["fn previous_snapshot() {}"]).await;
        let short = table_dir(&lancedb, &workspace.table_name());
        let legacy = table_dir(&lancedb, &workspace.legacy_table_name());
        std::fs::rename(&short, &legacy).unwrap();
        let previous = store.snapshot(&workspace).await.unwrap().unwrap();

        commit(&store, &workspace, &["fn replacement_snapshot() {}"]).await;
        let chunks = Store::chunks(&previous, &workspace).await.unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].chunk.code, "fn previous_snapshot() {}");
        assert!(legacy.is_dir(), "migration deleted an active snapshot");
        drop(previous);
        commit(&store, &workspace, &["fn replacement_snapshot() {}"]).await;
        assert!(!legacy.exists(), "later commits retry deferred cleanup");
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

    /// Commits one chunk per `(path, vector)` row, in order.
    async fn commit_rows(store: &Store, workspace: &Workspace, rows: &[(String, Vec<f32>)]) {
        let chunks: Vec<Chunk> = rows
            .iter()
            .map(|(path, _)| Chunk {
                relative_file_path: path.clone(),
                language: "rust".into(),
                start_line: 1,
                end_line: 1,
                code: format!("fn in_{}() {{}}", path.replace(['/', '.'], "_")),
                content_hash: crate::workspace::hash(path),
            })
            .collect();
        let vectors: Vec<_> = rows.iter().map(|(_, vector)| vector.clone()).collect();
        store
            .replace(workspace, &chunks, &vectors, "identity", 3)
            .await
            .unwrap();
    }

    /// A unit vector `step` milliradians from the query `[1, 0, 0]`: a larger
    /// step is farther from the query, so row order is distance order.
    fn at_angle(step: usize) -> Vec<f32> {
        let angle = step as f32 * 0.005;
        vec![angle.cos(), angle.sin(), 0.0]
    }

    fn include(pattern: &str) -> EffectiveFilter {
        EffectiveFilter::from_request(&crate::filter::FilterRequest {
            include_paths: Some(vec![pattern.to_owned()]),
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn a_path_filter_finds_matching_chunks_beyond_the_first_nearest_rows() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = workspace_at("/filtered");
        // 200 rows nearer the query than every row the filter keeps: more
        // than the 160 rows the first fetch asks for at limit 40.
        let mut rows: Vec<_> = (0..200)
            .map(|i| (format!("noise/f{i}.rs"), at_angle(i)))
            .collect();
        rows.extend((200..205).map(|i| (format!("wanted/f{i}.rs"), at_angle(i))));
        commit_rows(&store, &workspace, &rows).await;
        let snapshot = store.snapshot(&workspace).await.unwrap().unwrap();
        let found = Store::search(
            &snapshot,
            &workspace,
            &[1.0, 0.0, 0.0],
            40,
            &include("wanted/**"),
            usize::MAX,
        )
        .await
        .unwrap();
        assert_eq!(found.capped_at_rows, None);
        let hits = found.hits;
        let paths: Vec<_> = hits
            .iter()
            .map(|h| h.chunk.relative_file_path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "wanted/f200.rs",
                "wanted/f201.rs",
                "wanted/f202.rs",
                "wanted/f203.rs",
                "wanted/f204.rs"
            ]
        );
        let ranks: Vec<_> = hits.iter().map(|h| h.semantic_rank).collect();
        assert_eq!(ranks, [Some(1), Some(2), Some(3), Some(4), Some(5)]);
    }

    #[tokio::test]
    async fn a_filtered_fetch_stops_at_its_row_cap_and_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = workspace_at("/capped");
        let mut rows: Vec<_> = (0..700)
            .map(|i| (format!("noise/f{i}.rs"), at_angle(i % 300)))
            .collect();
        rows.push(("wanted/far.rs".to_owned(), at_angle(310)));
        commit_rows(&store, &workspace, &rows).await;
        let snapshot = store.snapshot(&workspace).await.unwrap().unwrap();
        let query = [1.0, 0.0, 0.0];
        let filter = include("wanted/**");

        // A cap of 640 rows: 160, then 640, and the one match lies beyond.
        let capped =
            Store::search_within(&snapshot, &workspace, &query, 40, &filter, usize::MAX, 640)
                .await
                .unwrap();
        assert!(capped.hits.is_empty());
        assert_eq!(capped.capped_at_rows, Some(640));
        assert_eq!(
            capped.cap_warning().as_deref(),
            Some(
                "Semantic retrieval checked the 640 nearest chunks and only 0 matched the filters; matching chunks farther from the query were not considered"
            )
        );

        // The production cap reaches it, and the table is exhausted before
        // the cap, so nothing is reported as capped.
        let found = Store::search(&snapshot, &workspace, &query, 40, &filter, 1)
            .await
            .unwrap();
        let paths: Vec<_> = found
            .hits
            .iter()
            .map(|h| h.chunk.relative_file_path.as_str())
            .collect();
        assert_eq!(paths, ["wanted/far.rs"]);
        assert_eq!(found.capped_at_rows, None);
        assert_eq!(found.cap_warning(), None);
    }

    #[tokio::test]
    async fn a_filtered_fetch_stops_once_every_matching_chunk_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = workspace_at("/all-found");
        // The one matching chunk lies between rows 200 and 201, inside the
        // 640-row second fetch; 700 farther rows follow it.
        let mut rows: Vec<_> = (0..900)
            .map(|i| (format!("noise/f{i}.rs"), at_angle(i)))
            .collect();
        rows.push(("wanted/near.rs".to_owned(), at_angle(200)));
        commit_rows(&store, &workspace, &rows).await;
        let snapshot = store.snapshot(&workspace).await.unwrap().unwrap();
        let query = [1.0, 0.0, 0.0];
        let filter = include("wanted/**");

        // Knowing one chunk matches, the 640-row fetch that finds it ends the
        // search, though the 640-row cap was reached and 40 hits were asked.
        let found = Store::search_within(&snapshot, &workspace, &query, 40, &filter, 1, 640)
            .await
            .unwrap();
        assert_eq!(found.hits.len(), 1);
        assert_eq!(found.capped_at_rows, None);
        // Without that count the same search reports the cap.
        let unknown =
            Store::search_within(&snapshot, &workspace, &query, 40, &filter, usize::MAX, 640)
                .await
                .unwrap();
        assert_eq!(unknown.hits.len(), 1);
        assert_eq!(unknown.capped_at_rows, Some(640));
    }

    /// What `Store::search` returned before filtered fetches could widen:
    /// one fetch of `min(limit * 4, 160)` nearest rows, the first `limit`
    /// kept, as (path, score, rank).
    async fn single_fetch_reference(
        snapshot: &Snapshot,
        vector: &[f32],
        limit: usize,
    ) -> Vec<(String, f32, usize)> {
        let batches = snapshot
            .table
            .query()
            .nearest_to(vector)
            .unwrap()
            .distance_type(DistanceType::Cosine)
            .limit(limit.saturating_mul(4).min(160))
            .execute()
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        let mut rows = Vec::new();
        for batch in &batches {
            let paths = column::<StringArray>(batch, "relative_file_path").unwrap();
            let distances = column::<Float32Array>(batch, "_distance").unwrap();
            for i in 0..batch.num_rows() {
                rows.push((paths.value(i).to_owned(), 1.0 - distances.value(i)));
            }
        }
        rows.into_iter()
            .take(limit)
            .enumerate()
            .map(|(i, (path, score))| (path, score, i + 1))
            .collect()
    }

    #[tokio::test]
    async fn an_unfiltered_search_returns_exactly_the_single_fetch_results() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(&temp.path().join("lancedb")).await.unwrap();
        let workspace = workspace_at("/unfiltered");
        // 300 rows at distinct distances, more than the 160-row first fetch.
        let rows: Vec<_> = (0..300)
            .map(|i| (format!("src/f{i}.rs"), at_angle((i * 7) % 300)))
            .collect();
        commit_rows(&store, &workspace, &rows).await;
        let snapshot = store.snapshot(&workspace).await.unwrap().unwrap();
        // No filter, an empty exclude list, and an include glob that keeps
        // every path: none rejects a row, so none may widen the fetch.
        let filters = [
            EffectiveFilter::from_request(&Default::default()).unwrap(),
            EffectiveFilter::from_request(&crate::filter::FilterRequest {
                exclude_paths: Some(vec![]),
                ..Default::default()
            })
            .unwrap(),
            include("**"),
        ];
        for (unfiltered, query) in filters
            .iter()
            .flat_map(|f| [[1.0, 0.0, 0.0], [0.6, 0.8, 0.0], [0.0, 1.0, 0.0]].map(|q| (f, q)))
        {
            // 41 and above ask for fewer than 4 * limit rows; above 160 the
            // single fetch returns fewer than `limit` hits.
            for limit in [1, 10, 40, 41, 100, 160, 161, 200] {
                let found =
                    Store::search(&snapshot, &workspace, &query, limit, unfiltered, rows.len())
                        .await
                        .unwrap();
                assert_eq!(found.capped_at_rows, None);
                let actual: Vec<_> = found
                    .hits
                    .iter()
                    .map(|h| {
                        (
                            h.chunk.relative_file_path.clone(),
                            h.semantic_score.unwrap(),
                            h.semantic_rank.unwrap(),
                        )
                    })
                    .collect();
                let expected = single_fetch_reference(&snapshot, &query, limit).await;
                assert_eq!(actual, expected, "query {query:?}, limit {limit}");
            }
        }
    }
}
