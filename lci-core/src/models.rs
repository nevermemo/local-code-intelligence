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

/// Whether [`Models::send_with_retry`] also retries timeouts, or only
/// connection errors. Index-time document embedding retries timeouts (a
/// long indexing job should ride out a cold model), but query-time calls --
/// the query embedding and the reranker -- do not: a search already fails
/// open without them, and retrying a hung service would multiply its stall
/// by `MAX_ATTEMPTS` (about six minutes at the default 120 s timeout).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimeoutRetry {
    Retry,
    FailFast,
}

/// A model service answered with a non-success HTTP status. Carries the
/// status and the (bounded) response body, which is usually the only place
/// the service says *why* -- e.g. vLLM's "maximum context length is 8192
/// tokens" for an oversized embedding input.
#[derive(Debug)]
pub struct HttpStatusError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for HttpStatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.body.is_empty() {
            write!(f, "HTTP status {}", self.status)
        } else {
            write!(f, "HTTP status {}: {}", self.status, self.body)
        }
    }
}

impl std::error::Error for HttpStatusError {}

/// Whether `error` is the service rejecting the request's input itself
/// (400/413/422) -- for an embedding request, typically an input longer than
/// the model's context -- rather than the service being unavailable.
pub fn is_input_rejected(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpStatusError>()
        .is_some_and(|error| matches!(error.status, 400 | 413 | 422))
}

/// Longest response body kept in an [`HttpStatusError`].
const MAX_ERROR_BODY_CHARS: usize = 600;

async fn check_status(response: reqwest::Response, label: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    let body: String = body.trim().chars().take(MAX_ERROR_BODY_CHARS).collect();
    Err(anyhow::Error::new(HttpStatusError {
        status: status.as_u16(),
        body,
    })
    .context(format!("{label} HTTP error")))
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

    /// Sends the request `build` constructs, retrying up to `MAX_ATTEMPTS`
    /// total on a connection error -- and on a timeout too when `timeouts`
    /// is [`TimeoutRetry::Retry`] -- with a doubling backoff (1s, 2s, ...).
    /// `build` is called fresh on every attempt since a `RequestBuilder` is
    /// consumed by `send`.
    async fn send_with_retry(
        &self,
        label: &str,
        timeouts: TimeoutRetry,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=MAX_ATTEMPTS {
            match build().send().await {
                Ok(response) => return Ok(response),
                Err(error)
                    if attempt < MAX_ATTEMPTS
                        && (error.is_connect()
                            || (timeouts == TimeoutRetry::Retry && error.is_timeout())) =>
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

    /// Embeds documents for indexing, retrying timeouts (see [`TimeoutRetry`]).
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_with(inputs, TimeoutRetry::Retry).await
    }

    async fn embed_with(&self, inputs: &[String], timeouts: TimeoutRetry) -> Result<Vec<Vec<f32>>> {
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
            .send_with_retry("embedding service", timeouts, || {
                self.client.post(&url).timeout(timeout).json(&body)
            })
            .await?;
        let response = check_status(response, "embedding service")
            .await?
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
    /// Embeds a search query. Query-time, so timeouts are not retried.
    pub async fn query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed_with(
                &[format!("{QUERY_INSTRUCTION}{query}")],
                TimeoutRetry::FailFast,
            )
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
            .send_with_retry("reranker", TimeoutRetry::FailFast, || {
                self.client
                    .post(&self.config.reranker_url)
                    .timeout(timeout)
                    .json(&body)
            })
            .await?;
        let response = check_status(response, "reranker")
            .await?
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
