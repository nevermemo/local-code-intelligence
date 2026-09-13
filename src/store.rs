use crate::{chunk::Chunk, workspace::Workspace};
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
    pub lexical_match_count: u32,
    pub lsp_rank: Option<usize>,
    pub fusion_score: f32,
    pub retrieval_channels: Vec<String>,
    pub reranker_score: Option<f32>,
}

pub struct Snapshot {
    pub table: Table,
    pub identity: String,
    pub dimension: usize,
    pub indexed_at: String,
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

    pub async fn cached_vectors(snapshot: &Snapshot) -> Result<HashMap<String, Vec<f32>>> {
        let batches = snapshot
            .table
            .query()
            .select(Select::columns(&["content_hash", "vector"]))
            .execute()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        let mut cache = HashMap::new();
        for batch in batches {
            let hashes = column::<StringArray>(&batch, "content_hash")?;
            let vectors = column::<FixedSizeListArray>(&batch, "vector")?;
            for i in 0..batch.num_rows() {
                let value = vectors.value(i);
                let vector = value
                    .as_any()
                    .downcast_ref::<Float32Array>()
                    .context("invalid vector values")?;
                cache.insert(hashes.value(i).into(), vector.values().to_vec());
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
    ) -> Result<Vec<Hit>> {
        ensure!(
            vector.len() == snapshot.dimension,
            "query embedding dimension differs from index; reindex workspace"
        );
        let batches = snapshot
            .table
            .query()
            .nearest_to(vector)?
            .distance_type(DistanceType::Cosine)
            .limit(limit)
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
                let distance = column::<Float32Array>(&batch, "_distance")?.value(i);
                ensure!(distance.is_finite(), "nonfinite semantic distance");
                let rank = hits.len() + 1;
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
                    semantic_score: Some(1.0 - distance),
                    semantic_rank: Some(rank),
                    lexical_rank: None,
                    lexical_match_count: 0,
                    lsp_rank: None,
                    fusion_score: 0.0,
                    retrieval_channels: vec!["semantic".into()],
                    reranker_score: None,
                });
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
                    lsp_rank: None,
                    fusion_score: 0.0,
                    retrieval_channels: vec![],
                    reranker_score: None,
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
