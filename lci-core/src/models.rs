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

/// Retries a plain request/response HTTP call up to this many times total
/// (the first attempt plus this many retries) when it fails with a timeout
/// or connection error. A single flat per-request timeout can't tell a
/// dead service from one that's merely cold (many local embedding/reranker
/// servers are genuinely much slower on their first call while a model
/// loads) or momentarily hiccupping -- unlike the LSP transport, there is
/// no mid-request progress signal to reset a deadline against for a plain
/// HTTP call, so bounded retry-with-backoff is the closest equivalent:
/// treat a timeout as "maybe still warming up," not an instant hard
/// failure. Does not retry HTTP error statuses or malformed responses --
/// those indicate a real problem retrying the identical request won't fix.
const MAX_ATTEMPTS: u32 = 3;

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

    /// Sends the request `build` constructs, retrying up to `MAX_ATTEMPTS`
    /// total on a timeout or connection error with a doubling backoff
    /// (1s, 2s, ...). `build` is called fresh on every attempt since a
    /// `RequestBuilder` is consumed by `send`.
    async fn send_with_retry(
        &self,
        label: &str,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=MAX_ATTEMPTS {
            match build().send().await {
                Ok(response) => return Ok(response),
                Err(error)
                    if attempt < MAX_ATTEMPTS && (error.is_timeout() || error.is_connect()) =>
                {
                    tracing::warn!(
                        attempt,
                        max_attempts = MAX_ATTEMPTS,
                        error = %error,
                        "retrying {label} request after a transient failure"
                    );
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(error) => return Err(error).context(format!("{label} request failed")),
            }
        }
        unreachable!("the loop always returns by its final attempt")
    }

    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        ensure!(!inputs.is_empty(), "embedding input is empty");
        let url = format!(
            "{}/embeddings",
            self.config.embedding_url.trim_end_matches('/')
        );
        let body = serde_json::json!({
            "model": self.config.embedding_model,
            "input": inputs,
            "encoding_format": "float"
        });
        let timeout = Duration::from_secs(self.config.embedding_timeout_seconds);
        let response = self
            .send_with_retry("embedding service", || {
                self.client.post(&url).timeout(timeout).json(&body)
            })
            .await?
            .error_for_status()
            .context("embedding service HTTP error")?
            .json::<Embeddings>()
            .await
            .context("invalid embedding response")?;
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
        let body = serde_json::json!({
            "model": self.config.reranker_model,
            "query": query,
            "documents": documents,
            "top_n": documents.len()
        });
        let timeout = Duration::from_secs(self.config.reranker_timeout_seconds);
        let response = self
            .send_with_retry("reranker", || {
                self.client
                    .post(&self.config.reranker_url)
                    .timeout(timeout)
                    .json(&body)
            })
            .await?
            .error_for_status()
            .context("reranker HTTP error")?
            .json::<Reranking>()
            .await
            .context("invalid reranker response")?;
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
