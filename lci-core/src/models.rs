use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

pub const QUERY_INSTRUCTION: &str = "Instruct: Given a code search query, retrieve relevant code passages that answer the query\nQuery: ";

#[derive(Clone)]
pub struct Models {
    client: reqwest::Client,
    config: Config,
    /// Shared by every clone: remembers that the reranker refused a
    /// connection so searches stop paying for it (see [`ConnectBreaker`]).
    reranker_breaker: Arc<ConnectBreaker>,
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

/// Under [`RetryPolicy::Retry`], makes a plain request/response HTTP call up
/// to this many attempts total (the first plus retries) when it fails with a
/// timeout or connection error. A single flat per-request timeout can't tell a
/// dead service from one that's merely cold (many local embedding/reranker
/// servers are genuinely much slower on their first call while a model
/// loads) or momentarily hiccupping -- unlike the LSP transport, there is
/// no mid-request progress signal to reset a deadline against for a plain
/// HTTP call, so bounded retry-with-backoff is the closest equivalent:
/// treat a timeout as "maybe still warming up," not an instant hard
/// failure. Does not retry HTTP error statuses or malformed responses --
/// those indicate a real problem retrying the identical request won't fix.
const MAX_ATTEMPTS: u32 = 3;

/// How [`Models::send_with_retry`] reacts to a failed attempt. Index-time
/// document embedding retries connection errors and timeouts (a long
/// indexing job should ride out a cold or restarting model). Query-time
/// calls -- the query embedding and the reranker -- make a single attempt:
/// a search already fails open without them, retrying a hung service would
/// multiply its stall by `MAX_ATTEMPTS` (about six minutes at the default
/// 120 s timeout), and backing off from a refused connection would add
/// seconds to every search while the service is down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetryPolicy {
    Retry,
    FailFast,
}

/// How long [`Models::rerank`] skips the reranker after it refused a
/// connection. Short, so a reranker that was merely restarting is used again
/// within half a minute; long enough that a reranker that is down costs a
/// search nothing instead of one failed connection attempt.
const RERANKER_DOWN_WINDOW: Duration = Duration::from_secs(30);

/// A tiny in-memory circuit breaker: after [`ConnectBreaker::trip`], callers
/// that check [`ConnectBreaker::remaining`] are told to skip the service until
/// the window has passed. After that the next call probes the service again;
/// if it fails to connect it trips the breaker once more. Time is passed in
/// so the logic is testable without sleeping.
struct ConnectBreaker {
    window: Duration,
    tripped_at: Mutex<Option<Instant>>,
}

impl ConnectBreaker {
    fn new(window: Duration) -> Self {
        Self {
            window,
            tripped_at: Mutex::new(None),
        }
    }

    /// The guarded state is a plain `Option<Instant>`, so a panic elsewhere
    /// cannot leave it inconsistent; ignore lock poisoning.
    fn state(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        self.tripped_at
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Time left until the next call may probe the service, or `None` when
    /// it may do so now (never tripped, cleared, or the window has passed).
    fn remaining(&self, now: Instant) -> Option<Duration> {
        let tripped_at = (*self.state())?;
        self.window
            .checked_sub(now.saturating_duration_since(tripped_at))
            .filter(|left| !left.is_zero())
    }

    fn trip(&self, now: Instant) {
        *self.state() = Some(now);
    }

    fn clear(&self) {
        *self.state() = None;
    }
}

/// Whether `error` was caused by failing to connect to the service at all
/// (refused, unreachable, or a connect timeout), as opposed to a service that
/// answered badly or took too long to answer.
fn is_connect_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_connect)
    })
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
            reranker_breaker: Arc::new(ConnectBreaker::new(RERANKER_DOWN_WINDOW)),
        })
    }
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Sends the request `build` constructs. With [`RetryPolicy::Retry`] a
    /// connection error or a timeout is retried up to `MAX_ATTEMPTS` total
    /// with a doubling backoff (1s, 2s, ...); with [`RetryPolicy::FailFast`]
    /// the first failure is returned at once. `build` is called fresh on
    /// every attempt since a `RequestBuilder` is consumed by `send`.
    async fn send_with_retry(
        &self,
        label: &str,
        policy: RetryPolicy,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=MAX_ATTEMPTS {
            match build().send().await {
                Ok(response) => return Ok(response),
                Err(error)
                    if policy == RetryPolicy::Retry
                        && attempt < MAX_ATTEMPTS
                        && (error.is_connect() || error.is_timeout()) =>
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

    /// Embeds documents for indexing, retrying connection errors and
    /// timeouts (see [`RetryPolicy`]).
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_with(inputs, RetryPolicy::Retry).await
    }

    async fn embed_with(&self, inputs: &[String], policy: RetryPolicy) -> Result<Vec<Vec<f32>>> {
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
            .send_with_retry("embedding service", policy, || {
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
    /// Embeds a search query. Query-time, so one attempt: neither a timeout
    /// nor a refused connection is retried.
    pub async fn query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed_with(
                &[format!("{QUERY_INSTRUCTION}{query}")],
                RetryPolicy::FailFast,
            )
            .await?
            .remove(0))
    }

    /// Reranks `documents` for `query`; best first, as `(index, score)`.
    /// Query-time, so one attempt. After a refused connection the reranker is
    /// skipped for [`RERANKER_DOWN_WINDOW`] and this returns an error at once
    /// (the caller falls back to fusion order); the first call after the
    /// window tries the reranker again.
    pub async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<(usize, f32)>> {
        if let Some(remaining) = self.reranker_breaker.remaining(Instant::now()) {
            bail!(
                "reranker skipped as recently unreachable (next attempt in about {}s)",
                remaining.as_secs() + 1
            );
        }
        let body = serde_json::json!({
            "model": self.config.reranker_model,
            "query": query,
            "documents": documents,
            "top_n": documents.len()
        });
        let timeout = Duration::from_secs(self.config.reranker_timeout_seconds);
        let response = match self
            .send_with_retry("reranker", RetryPolicy::FailFast, || {
                self.client
                    .post(&self.config.reranker_url)
                    .timeout(timeout)
                    .json(&body)
            })
            .await
        {
            Ok(response) => {
                // The service answered (even with an error status), so it
                // is reachable.
                self.reranker_breaker.clear();
                response
            }
            Err(error) => {
                if is_connect_error(&error) {
                    self.reranker_breaker.trip(Instant::now());
                }
                return Err(error);
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An address on which nothing listens: bind to learn a free port, then
    /// release it.
    fn closed_local_address() -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        address
    }

    #[test]
    fn breaker_skips_for_its_window_then_allows_a_probe() {
        let breaker = ConnectBreaker::new(Duration::from_secs(30));
        let start = Instant::now();
        assert_eq!(breaker.remaining(start), None, "a new breaker is closed");

        breaker.trip(start);
        assert_eq!(breaker.remaining(start), Some(Duration::from_secs(30)));
        assert_eq!(
            breaker.remaining(start + Duration::from_secs(10)),
            Some(Duration::from_secs(20))
        );
        assert_eq!(
            breaker.remaining(start + Duration::from_secs(30)),
            None,
            "the first call after the window probes again"
        );
        assert_eq!(breaker.remaining(start + Duration::from_secs(45)), None);

        // A probe that fails to connect re-arms the window.
        breaker.trip(start + Duration::from_secs(45));
        assert!(breaker.remaining(start + Duration::from_secs(46)).is_some());

        // A probe that connects closes it immediately.
        breaker.clear();
        assert_eq!(breaker.remaining(start + Duration::from_secs(46)), None);
    }

    #[tokio::test]
    async fn a_refused_reranker_connection_is_skipped_for_the_window_then_probed_again() {
        let config = Config {
            reranker_url: format!("http://{}/rerank", closed_local_address()),
            ..Config::default()
        };
        let mut models = Models::new(&config).unwrap();
        models.reranker_breaker = Arc::new(ConnectBreaker::new(Duration::from_secs(1)));
        let documents = vec!["fn a() {}".to_owned()];

        let first = models.rerank("query", &documents).await.unwrap_err();
        assert!(is_connect_error(&first), "{first:#}");
        assert!(
            !format!("{first:#}").contains("recently unreachable"),
            "the first call really tries the reranker: {first:#}"
        );

        // Within the window: no connection attempt, so it returns at once
        // and is not a connect error itself.
        let started = Instant::now();
        let skipped = models.rerank("query", &documents).await.unwrap_err();
        assert!(
            format!("{skipped:#}").contains("recently unreachable"),
            "{skipped:#}"
        );
        assert!(!is_connect_error(&skipped), "{skipped:#}");
        assert!(started.elapsed() < Duration::from_millis(500));

        // After the window the next call tries the reranker again.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        let probed = models.rerank("query", &documents).await.unwrap_err();
        assert!(is_connect_error(&probed), "{probed:#}");
    }

    #[tokio::test]
    async fn the_query_embedding_makes_one_attempt_on_a_refused_connection() {
        let config = Config {
            embedding_url: format!("http://{}/v1", closed_local_address()),
            ..Config::default()
        };
        let models = Models::new(&config).unwrap();

        let started = Instant::now();
        let error = models.query("anything").await.unwrap_err();
        assert!(is_connect_error(&error), "{error:#}");
        // The retried path made three ~2 s refused connects (on Windows) plus
        // 1 s + 2 s of sleeps, ~9 s in all; one attempt stays well under 5 s.
        assert!(
            started.elapsed() < Duration::from_millis(5000),
            "took {:?}",
            started.elapsed()
        );
    }
}
