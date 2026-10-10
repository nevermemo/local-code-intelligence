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

/// Maximum characters retained in a bounded evidence snippet.
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

    pub async fn snapshot(&self, workspace: &Workspace) -> Result<Option<Snapshot>> {
        let table = match self.db.open_table(workspace.table_name()).execute().await {
            Ok(t) => t,
            Err(lancedb::Error::TableNotFound { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let schema = table.schema().await?;
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
        Ok(())
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
}
