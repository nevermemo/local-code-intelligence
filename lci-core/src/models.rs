use crate::config::Config;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::time::Duration;

pub const QUERY_INSTRUCTION: &str = "Instruct: Given a code search query, retrieve relevant code passages that answer the query\nQuery: ";

#[derive(Clone)]
pub struct Models {
    client: reqwest::Client,
    config: Config,
}

#[derive(Deserialize)]
struct Embeddings {
    data: Vec<Embedding>,
}
#[derive(Deserialize)]
struct Embedding {
    index: usize,
    embedding: Vec<f32>,
}
#[derive(Deserialize)]
struct Reranking {
    results: Vec<Rank>,
}
#[derive(Deserialize)]
struct Rank {
    index: usize,
    relevance_score: f32,
}

impl Models {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            config: config.clone(),
        })
    }
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        ensure!(!inputs.is_empty(), "embedding input is empty");
        let response = self.client.post(format!("{}/embeddings", self.config.embedding_url.trim_end_matches('/')))
            .timeout(Duration::from_secs(self.config.embedding_timeout_seconds))
            .json(&serde_json::json!({"model": self.config.embedding_model, "input": inputs, "encoding_format": "float"}))
            .send().await.context("embedding service request failed")?.error_for_status().context("embedding service HTTP error")?
            .json::<Embeddings>().await.context("invalid embedding response")?;
        ensure!(
            response.data.len() == inputs.len(),
            "embedding response count mismatch"
        );
        let mut ordered = vec![None; inputs.len()];
        let mut dimension = None;
        for item in response.data {
            ensure!(
                item.index < inputs.len() && ordered[item.index].is_none(),
                "invalid/duplicate embedding index"
            );
            ensure!(
                !item.embedding.is_empty() && item.embedding.iter().all(|v| v.is_finite()),
                "invalid embedding vector"
            );
            ensure!(
                item.embedding.iter().any(|&v| v != 0.0),
                "zero embedding vector"
            );
            ensure!(
                dimension.is_none_or(|d| d == item.embedding.len()),
                "mixed embedding dimensions"
            );
            dimension = Some(item.embedding.len());
            ordered[item.index] = Some(item.embedding);
        }
        ordered
            .into_iter()
            .map(|v| v.context("missing embedding index"))
            .collect()
    }
    pub async fn query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed(&[format!("{QUERY_INSTRUCTION}{query}")])
            .await?
            .remove(0))
    }
    pub async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f32)>> {
        let response = self.client.post(&self.config.reranker_url)
            .timeout(Duration::from_secs(self.config.reranker_timeout_seconds))
            .json(&serde_json::json!({"model": self.config.reranker_model, "query": query, "documents": documents, "top_n": documents.len()}))
            .send().await.context("reranker request failed")?.error_for_status().context("reranker HTTP error")?
            .json::<Reranking>().await.context("invalid reranker response")?;
        ensure!(
            response.results.len() == documents.len(),
            "reranker response count mismatch"
        );
        let mut seen = vec![false; documents.len()];
        let mut ranks = Vec::new();
        for item in response.results {
            ensure!(
                item.index < documents.len()
                    && !seen[item.index]
                    && item.relevance_score.is_finite(),
                "invalid reranker index/score"
            );
            seen[item.index] = true;
            ranks.push((item.index, item.relevance_score));
        }
        ranks.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(ranks)
    }
}
