//! The workspace index behind the `search.lexical` and `search.vector` maps:
//! [`AppChunks`] answers `lci_maps::search_maps::ChunkSearch` from the same
//! stores and channels as the normal search (ripgrep and the BM25 chunk
//! mapping for the lexical side, LanceDB cosine search and the embedder for
//! the vector side). It reads the snapshot that exists; it never indexes.

use super::super::{App, indexing::document_text};
use crate::{
    chunk::Chunk,
    filter::{EffectiveFilter, FilterRequest},
    lexical,
    store::{Hit, Snapshot, Store},
    workspace::Workspace,
};
use lci_core::node::{NodeId, chunk_ids};
use lci_maps::search_maps::{ChunkHit, ChunkSearch};
use std::{collections::HashMap, path::Path};
use tokio::sync::OnceCell;

/// What one call reads of the index, loaded on first use.
struct State {
    workspace: Workspace,
    snapshot: Snapshot,
    chunks: Vec<Hit>,
    ids: Vec<NodeId>,
    by_node: HashMap<NodeId, usize>,
    by_path: HashMap<String, Vec<usize>>,
    by_key: HashMap<(String, String, u32, u32), usize>,
}

impl State {
    fn index_of(&self, hit: &Hit) -> Option<usize> {
        let chunk = &hit.chunk;
        self.by_key
            .get(&(
                chunk.relative_file_path.clone(),
                chunk.content_hash.clone(),
                chunk.start_line,
                chunk.end_line,
            ))
            .copied()
    }

    fn hit(&self, index: usize, score: f32) -> ChunkHit {
        let chunk = &self.chunks[index].chunk;
        ChunkHit {
            node: self.ids[index].clone(),
            path: chunk.relative_file_path.replace('\\', "/"),
            start_line: chunk.start_line,
            end_line: chunk.end_line,
            code: chunk.code.clone(),
            score,
        }
    }
}

/// The index of one workspace as the search maps ask for it.
pub struct AppChunks<'a> {
    app: &'a App,
    workspace: &'a Path,
    state: OnceCell<Result<State, String>>,
}

impl<'a> AppChunks<'a> {
    pub fn new(app: &'a App, workspace: &'a Path) -> Self {
        Self {
            app,
            workspace,
            state: OnceCell::new(),
        }
    }

    async fn load(&self) -> Result<State, String> {
        let fail = |error: anyhow::Error| format!("{error:#}");
        let workspace =
            Workspace::resolve(self.workspace, &self.app.config.data_dir).map_err(fail)?;
        let snapshot = self
            .app
            .store
            .snapshot(&workspace)
            .await
            .map_err(fail)?
            .ok_or_else(|| "the workspace is not indexed; call index_workspace first".to_owned())?;
        let chunks = Store::chunks(&snapshot, &workspace).await.map_err(fail)?;
        let raw: Vec<&Chunk> = chunks.iter().map(|hit| &hit.chunk).collect();
        let ids = chunk_ids(&raw);
        let mut by_node = HashMap::new();
        let mut by_path: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_key = HashMap::new();
        for (i, (hit, id)) in chunks.iter().zip(&ids).enumerate() {
            let chunk = &hit.chunk;
            by_node.insert(id.clone(), i);
            by_path
                .entry(chunk.relative_file_path.replace('\\', "/"))
                .or_default()
                .push(i);
            by_key.insert(
                (
                    chunk.relative_file_path.clone(),
                    chunk.content_hash.clone(),
                    chunk.start_line,
                    chunk.end_line,
                ),
                i,
            );
        }
        for indexes in by_path.values_mut() {
            indexes.sort_by_key(|&i| (chunks[i].chunk.start_line, chunks[i].chunk.end_line));
        }
        Ok(State {
            workspace,
            snapshot,
            chunks,
            ids,
            by_node,
            by_path,
            by_key,
        })
    }

    async fn state(&self) -> Result<&State, String> {
        self.state
            .get_or_init(|| self.load())
            .await
            .as_ref()
            .map_err(Clone::clone)
    }
}

#[lci_maps::async_trait]
impl ChunkSearch for AppChunks<'_> {
    async fn lexical(&self, query: &str, limit: usize) -> Result<Vec<ChunkHit>, String> {
        let state = self.state().await?;
        let found = lexical::search(&self.app.config.ripgrep_path, &state.workspace.path, query)
            .await
            .map_err(|error| format!("{error:#}"))?;
        let hits = App::map_lexical_matches(&state.chunks, found, limit);
        Ok(hits
            .iter()
            .filter_map(|hit| {
                let index = state.index_of(hit)?;
                Some(state.hit(index, hit.lexical_score.unwrap_or(0.0)))
            })
            .collect())
    }

    async fn chunk_at(&self, path: &str, line: Option<u32>) -> Result<Option<ChunkHit>, String> {
        let state = self.state().await?;
        let Some(indexes) = state.by_path.get(&path.replace('\\', "/")) else {
            return Ok(None);
        };
        let found = indexes.iter().copied().find(|&i| {
            let chunk = &state.chunks[i].chunk;
            line.is_none_or(|line| chunk.start_line <= line && line <= chunk.end_line)
        });
        Ok(found.map(|i| state.hit(i, 0.0)))
    }

    async fn stored_vector(&self, chunk: &NodeId) -> Result<Option<Vec<f32>>, String> {
        let state = self.state().await?;
        let Some(&index) = state.by_node.get(chunk) else {
            return Ok(None);
        };
        let chunk = &state.chunks[index].chunk;
        Store::chunk_vector(
            &state.snapshot,
            &chunk.relative_file_path,
            &chunk.content_hash,
            chunk.start_line,
        )
        .await
        .map_err(|error| format!("{error:#}"))
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        // A vectorless index (built while the embedder was down) has no
        // vector lane to query, so embed nothing: the map records this
        // reason and the walk goes on without it.
        if self.state().await?.snapshot.vectors_missing {
            return Err(
                "the index has no vectors (the embedding service was unreachable when it was built)"
                    .to_owned(),
            );
        }
        // The text goes through the document template, as an indexed chunk's
        // does (an empty path: the text is not a file's chunk).
        let template = &self.app.config.embedding_document_template;
        let chunk = Chunk {
            relative_file_path: String::new(),
            language: String::new(),
            start_line: 1,
            end_line: 1,
            code: text.to_owned(),
            content_hash: String::new(),
        };
        let document = document_text(template, &chunk);
        self.app
            .models
            .embed_once(&[document])
            .await
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .next()
            .ok_or_else(|| "the embedder returned no vector".to_owned())
    }

    async fn nearest(&self, vector: &[f32], limit: usize) -> Result<Vec<ChunkHit>, String> {
        let state = self.state().await?;
        let filters = EffectiveFilter::from_request(&FilterRequest::default())
            .map_err(|error| format!("{error:#}"))?;
        let found = Store::search(
            &state.snapshot,
            &state.workspace,
            vector,
            limit,
            &filters,
            state.chunks.len(),
        )
        .await
        .map_err(|error| format!("{error:#}"))?;
        Ok(found
            .hits
            .iter()
            .filter_map(|hit| {
                let index = state.index_of(hit)?;
                Some(state.hit(index, hit.semantic_score.unwrap_or(0.0)))
            })
            .collect())
    }

    async fn chunk_lines(&self) -> Result<Vec<(NodeId, (u32, u32))>, String> {
        let state = self.state().await?;
        Ok(state
            .ids
            .iter()
            .zip(&state.chunks)
            .map(|(id, hit)| (id.clone(), (hit.chunk.start_line, hit.chunk.end_line)))
            .collect())
    }
}
