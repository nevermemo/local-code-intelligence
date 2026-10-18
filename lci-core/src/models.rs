use crate::{
    config::{Config, Rescorer, fill_template},
    endpoint::ApiKey,
    provider::{ProviderKind, WireCandidate, parse_wire_scores, wire_request},
};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    error::Error as _,
    io::ErrorKind,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

/// What the default query template opens with: the default template
/// (`config::DEFAULT_EMBEDDING_QUERY_TEMPLATE`) up to its `{query}`
/// placeholder. The configured template is what `Models::query` uses; this
/// is for callers (the fake model service in the tests) that need to tell a
/// default-formatted query from a document.
pub const QUERY_INSTRUCTION: &str = "Instruct: Given a code search query, retrieve relevant code passages that answer the query\nQuery: ";

#[derive(Clone)]
pub struct Models {
    client: reqwest::Client,
    config: Config,
    /// Shared by every clone: remembers that the reranker refused a
    /// connection so searches stop paying for it (see [`ConnectBreaker`]).
    reranker_breaker: Arc<ConnectBreaker>,
    /// The same for the `system_one_wire` rescorer (see
    /// [`Models::system_one_rank`]).
    system_one_breaker: Arc<ConnectBreaker>,
    /// The sleep before the first retry under [`RetryPolicy::Retry`]; it
    /// doubles for each further one. A field so the tests need not wait a
    /// real second.
    initial_backoff: Duration,
    /// Shared by every clone: the width of the vectors in the latest
    /// successful embedding response (documents or a query), 0 before the
    /// first one. Indexing compares it with the cached vectors' width, since
    /// a service can change its width under an unchanged URL and model name.
    observed_dimension: Arc<AtomicUsize>,
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
/// timeout, a connection error, or a connection that was closed or reset
/// while the request was being sent (see [`is_connection_dropped`]: typically
/// a pooled keep-alive connection the server closed while it sat idle). A
/// single flat per-request timeout can't tell a dead service from one that's
/// merely cold (many local embedding/reranker servers are genuinely much
/// slower on their first call while a model loads) or momentarily hiccupping
/// -- unlike the LSP transport, there is no mid-request progress signal to
/// reset a deadline against for a plain HTTP call, so bounded
/// retry-with-backoff is the closest equivalent: treat a timeout as "maybe
/// still warming up," not an instant hard failure. Does not retry HTTP error
/// statuses or malformed responses -- those indicate a real problem retrying
/// the identical request won't fix.
const MAX_ATTEMPTS: u32 = 3;

/// The sleep before the first retry; it doubles for each further one.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);

/// How [`Models::send_with_retry`] reacts to a failed attempt. Index-time
/// document embedding retries connection errors, timeouts, and connections
/// closed or reset mid-request (a long indexing job should ride out a cold or
/// restarting model, and one dropped keep-alive connection must not abort
/// hours of work). Query-time calls -- the query embedding and the reranker --
/// make a single attempt: a search already fails open without them, retrying
/// a hung service would multiply its stall by `MAX_ATTEMPTS` (about six
/// minutes at the default 120 s timeout), and backing off from a refused
/// connection would add seconds to every search while the service is down.
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

/// How long [`Models::system_one_rank`] skips the typed rescorer after it
/// refused a connection (the same 30 s as the reranker's window).
const SYSTEM_ONE_DOWN_WINDOW: Duration = Duration::from_secs(30);

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
pub(crate) fn is_connect_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_connect)
    })
}

/// Whether sending the request failed because the connection was closed or
/// reset under it: the server closed a pooled keep-alive connection while it
/// sat idle (uvicorn does after 5 s) and the next request on it hit the dead
/// socket. reqwest classifies that as a request error, not a connect error or
/// a timeout. Deliberately narrow: the error must be a request error (not a
/// builder, redirect, body or decode error) whose source chain holds either
/// hyper's incomplete-message error (`hyper::Error::is_incomplete_message`:
/// the peer closed after the request was written) or an I/O error of kind reset, aborted, broken pipe or unexpected
/// EOF (the peer reset the connection, or closed a TLS stream without a
/// close-notify). A refused connection, a timeout, an HTTP status and a
/// malformed response are none of these.
fn is_connection_dropped(error: &reqwest::Error) -> bool {
    error.is_request()
        && std::iter::successors(error.source(), |&cause| cause.source()).any(|cause| {
            cause
                .downcast_ref::<hyper::Error>()
                .is_some_and(hyper::Error::is_incomplete_message)
                || cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        ErrorKind::ConnectionReset
                            | ErrorKind::ConnectionAborted
                            | ErrorKind::BrokenPipe
                            | ErrorKind::UnexpectedEof
                    )
                })
        })
}

/// A model service answered with a non-success HTTP status. Carries the
/// status and the (bounded, credential-redacted) response body, which is usually
/// the only place the service says *why* -- e.g. vLLM's "maximum context length
/// is 8192 tokens" for an oversized embedding input.
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

/// Maximum number of bytes buffered from a model response body.
pub(crate) const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

pub(crate) struct LimitedBody {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
}

pub(crate) async fn read_limited_body(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<LimitedBody> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let remaining = limit.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            return Ok(LimitedBody {
                bytes,
                truncated: true,
            });
        }
    }
    Ok(LimitedBody {
        bytes,
        truncated: false,
    })
}

/// Query values and passwords at least this long are redacted wherever they
/// occur in an error body; shorter ones only as a whole query value.
const MIN_FREE_REDACTION_CHARS: usize = 6;

fn continues_query_value(character: char) -> bool {
    character.is_ascii_alphanumeric() || "-._~%+".contains(character)
}

fn redact_error_body(body: &str, endpoint: &str) -> String {
    let mut values = Vec::new();
    if let Ok(mut url) = reqwest::Url::parse(endpoint) {
        if let Some(query) = url.query() {
            values.extend(
                query
                    .split('&')
                    .filter_map(|pair| pair.split_once('=').map(|(_, value)| value.to_owned())),
            );
        }
        values.extend(url.query_pairs().map(|(_, value)| value.into_owned()));
        if let Some(password) = url.password().map(str::to_owned) {
            values.push(password.clone());
            // Decode userinfo with the existing URL parser, keeping literal
            // '+' and '&' from becoming form-query syntax.
            url.set_query(Some(&format!(
                "value={}",
                password.replace('+', "%2B").replace('&', "%26")
            )));
            values.extend(url.query_pairs().map(|(_, value)| value.into_owned()));
        }
    }
    values.retain(|value| !value.is_empty());
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));

    // Match the original text once (longest value first), never the inserted
    // marker. Redact before capping so the cap cannot expose a secret's prefix.
    // A value shorter than MIN_FREE_REDACTION_CHARS (`?v=1`, `?format=raw`)
    // is no credential worth hiding and occurs inside ordinary words, so it is
    // redacted only where it stands as a whole query value (`=v` followed by
    // a character that cannot continue the value), never inside `HTTP/1.1`.
    // So a short value at the very start of the body or followed by `.` is
    // left as is; that is accepted, not an oversight.
    let mut remaining = body;
    let mut previous = None;
    let mut redacted = String::new();
    let mut written = 0;
    while !remaining.is_empty() && written < MAX_ERROR_BODY_CHARS {
        if let Some(value) = values.iter().find(|value| {
            remaining.starts_with(value.as_str())
                && (value.chars().count() >= MIN_FREE_REDACTION_CHARS
                    || (previous == Some('=')
                        && !remaining[value.len()..]
                            .chars()
                            .next()
                            .is_some_and(continues_query_value)))
        }) {
            redacted.push_str("[redacted]");
            remaining = &remaining[value.len()..];
            written += "[redacted]".len();
            previous = value.chars().next_back();
        } else {
            let character = remaining.chars().next().unwrap();
            remaining = &remaining[character.len_utf8()..];
            previous = Some(character);
            // Trim leading diagnostic whitespace only after checking whether
            // it belongs to a credential.
            if written > 0 || !character.is_whitespace() {
                redacted.push(character);
                written += 1;
            }
        }
    }
    // A marker that starts before the cap stays whole, so the body can pass
    // the cap by up to nine characters rather than end in a torn `[reda`.
    redacted.trim_end().to_owned()
}

/// Replaces `key`, wherever a server echoed it, in `body`.
pub(crate) fn redact_secret(body: &str, key: Option<&ApiKey>) -> String {
    match key {
        Some(key) => body.replace(key.expose(), "[redacted]"),
        None => body.to_owned(),
    }
}

pub(crate) async fn check_status(
    response: reqwest::Response,
    label: &str,
    endpoint: &str,
    key: Option<&ApiKey>,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let bounded = read_limited_body(response, MAX_RESPONSE_BODY_BYTES)
        .await
        .unwrap_or(LimitedBody {
            bytes: Vec::new(),
            truncated: false,
        });
    let text = redact_secret(&String::from_utf8_lossy(&bounded.bytes), key);
    let mut body = redact_error_body(&text, endpoint);
    if bounded.truncated {
        body.push_str(" [truncated]");
    }
    Err(anyhow::Error::new(HttpStatusError {
        status: status.as_u16(),
        body,
    })
    .context(format!("{label} HTTP error")))
}

/// Adds the bearer token of the endpoint's API key, when it has one.
fn with_bearer(builder: reqwest::RequestBuilder, key: Option<&ApiKey>) -> reqwest::RequestBuilder {
    match key {
        Some(key) => builder.bearer_auth(key.expose()),
        None => builder,
    }
}

impl Models {
    pub fn new(config: &Config) -> Result<Self> {
        // Once per client, not per request.
        for (url_key, endpoint) in [
            ("embedding_url", config.embedder_endpoint()),
            ("reranker_url", config.reranker_endpoint()),
        ] {
            if let Some(warning) = endpoint.cleartext_key_warning(url_key) {
                tracing::warn!("{warning}");
            }
        }
        if config.rescorer == Rescorer::SystemOne
            && let Some(system_one) = &config.system_one
            && let Some(warning) = system_one
                .endpoint()
                .cleartext_key_warning("system_one.url")
        {
            tracing::warn!("{warning}");
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            config: config.clone(),
            reranker_breaker: Arc::new(ConnectBreaker::new(RERANKER_DOWN_WINDOW)),
            system_one_breaker: Arc::new(ConnectBreaker::new(SYSTEM_ONE_DOWN_WINDOW)),
            initial_backoff: INITIAL_BACKOFF,
            observed_dimension: Arc::default(),
        })
    }

    /// The vector width of the latest successful embedding response this
    /// process received, or `None` before the first one.
    pub fn observed_dimension(&self) -> Option<usize> {
        match self.observed_dimension.load(Ordering::Relaxed) {
            0 => None,
            dimension => Some(dimension),
        }
    }
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Sends the request `build` constructs. With [`RetryPolicy::Retry`] a
    /// connection error, a timeout, or a connection closed or reset while the
    /// request was sent ([`is_connection_dropped`]) is retried up to
    /// `MAX_ATTEMPTS` total with a doubling backoff (1s, 2s, ...), so the
    /// request must be safe to repeat (an embedding is a pure function of its
    /// input); with [`RetryPolicy::FailFast`] the first failure is returned at
    /// once. `build` is called fresh on every attempt since a `RequestBuilder`
    /// is consumed by `send`.
    async fn send_with_retry(
        &self,
        label: &str,
        policy: RetryPolicy,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        let mut delay = self.initial_backoff;
        for attempt in 1..=MAX_ATTEMPTS {
            match build().send().await.map_err(reqwest::Error::without_url) {
                Ok(response) => return Ok(response),
                Err(error)
                    if policy == RetryPolicy::Retry
                        && attempt < MAX_ATTEMPTS
                        && (error.is_connect()
                            || error.is_timeout()
                            || is_connection_dropped(&error)) =>
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

    /// Embeds documents for indexing, retrying connection errors, timeouts,
    /// and dropped connections (see [`RetryPolicy`]).
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_with(inputs, RetryPolicy::Retry).await
    }

    /// Embeds documents with one attempt (see [`RetryPolicy::FailFast`]),
    /// for an advisory request whose failure the caller tolerates.
    pub async fn embed_once(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        self.embed_with(inputs, RetryPolicy::FailFast).await
    }

    async fn embed_with(&self, inputs: &[String], policy: RetryPolicy) -> Result<Vec<Vec<f32>>> {
        ensure!(!inputs.is_empty(), "embedding input is empty");
        let mut url = reqwest::Url::parse(&self.config.embedding_url)
            .context("invalid embedding service URL")?;
        ensure!(
            !url.cannot_be_a_base(),
            "embedding service URL cannot be a base URL"
        );
        // Every trailing slash goes before `/embeddings` is appended, like
        // `embedding_identity` does (one `pop_if_empty` would turn `/v1//` into
        // `/v1//embeddings`); the query stays where it is.
        let path = format!("{}/embeddings", url.path().trim_end_matches('/'));
        url.set_path(&path);
        let body = serde_json::json!({
            "model": self.config.embedding_model,
            "input": inputs,
            "encoding_format": "float"
        });
        let timeout = Duration::from_secs(self.config.embedding_timeout_seconds);
        let key = self.config.embedder_endpoint().api_key()?;
        let response = self
            .send_with_retry("embedding service", policy, || {
                with_bearer(
                    self.client.post(url.clone()).timeout(timeout).json(&body),
                    key.as_ref(),
                )
            })
            .await?;
        let bounded = read_limited_body(
            check_status(response, "embedding service", url.as_str(), key.as_ref()).await?,
            MAX_RESPONSE_BODY_BYTES,
        )
        .await
        .context("invalid embedding response")?;
        ensure!(
            !bounded.truncated,
            "embedding response body exceeds {MAX_RESPONSE_BODY_BYTES} byte limit"
        );
        let response = serde_json::from_slice::<Embeddings>(&bounded.bytes)
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
        let vectors = ordered
            .into_iter()
            .map(|v| v.context("missing embedding index"))
            .collect::<Result<Vec<_>>>()?;
        if let Some(dimension) = dimension {
            self.observed_dimension.store(dimension, Ordering::Relaxed);
        }
        Ok(vectors)
    }

    /// The text embedded for `query`: `embedding_query_template` with its
    /// `{query}` placeholder filled. The query is substituted as is, so a
    /// query that mentions `{query}` stays literal.
    fn query_text(&self, query: &str) -> String {
        fill_template(&self.config.embedding_query_template, &[("query", query)])
    }

    /// Embeds a search query. Query-time, so one attempt: neither a timeout
    /// nor a refused connection is retried.
    pub async fn query(&self, query: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed_with(&[self.query_text(query)], RetryPolicy::FailFast)
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
        let key = self.config.reranker_endpoint().api_key()?;
        let response = match self
            .send_with_retry("reranker", RetryPolicy::FailFast, || {
                with_bearer(
                    self.client
                        .post(&self.config.reranker_url)
                        .timeout(timeout)
                        .json(&body),
                    key.as_ref(),
                )
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
        let bounded = read_limited_body(
            check_status(
                response,
                "reranker",
                &self.config.reranker_url,
                key.as_ref(),
            )
            .await?,
            MAX_RESPONSE_BODY_BYTES,
        )
        .await
        .context("invalid reranker response")?;
        ensure!(
            !bounded.truncated,
            "reranker response body exceeds {MAX_RESPONSE_BODY_BYTES} byte limit"
        );
        let response = serde_json::from_slice::<Reranking>(&bounded.bytes)
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

    /// Scores `candidates` for `query` with the typed `system_one_wire`
    /// service (`rescorer = "system_one"`); best first, as `(index,
    /// probability)`, ties in the candidates' order (the fusion order).
    ///
    /// A shortlist longer than `system_one.batch_size` goes in several
    /// sequential requests; any one failing fails the whole call, so a
    /// partial answer never reorders anything. Query-time, so one attempt per
    /// request. After a refused connection the service is skipped for
    /// [`SYSTEM_ONE_DOWN_WINDOW`] and this returns an error at once; the
    /// caller falls back to fusion order. The API key is read from the
    /// environment here, per call, and appears in no error.
    pub async fn system_one_rank(
        &self,
        query: &str,
        candidates: &[WireCandidate],
    ) -> Result<Vec<(usize, f32)>> {
        let system_one = self
            .config
            .system_one
            .as_ref()
            .filter(|s| s.provider == ProviderKind::SystemOneWire)
            .context(
                "system_one rescoring needs [system_one] with provider = \"system_one_wire\"",
            )?;
        system_one.validate()?;
        ensure!(
            !candidates.is_empty(),
            "system_one has no candidates to score"
        );
        if let Some(remaining) = self.system_one_breaker.remaining(Instant::now()) {
            bail!(
                "system_one skipped as recently unreachable (next attempt in about {}s)",
                remaining.as_secs() + 1
            );
        }
        let url = system_one.wire_url()?;
        let key = system_one.endpoint().api_key()?;
        let timeout = Duration::from_millis(system_one.timeout_ms);
        let total = candidates.len();
        let mut ranks = Vec::with_capacity(total);
        for start in (0..total).step_by(system_one.batch_size) {
            let range = start..(start + system_one.batch_size).min(total);
            let body = wire_request(&system_one.model, query, candidates, range.clone());
            let response = match self
                .send_with_retry("system_one", RetryPolicy::FailFast, || {
                    with_bearer(
                        self.client.post(url.clone()).timeout(timeout).json(&body),
                        key.as_ref(),
                    )
                })
                .await
            {
                Ok(response) => {
                    // The service answered (even with an error status).
                    self.system_one_breaker.clear();
                    response
                }
                Err(error) => {
                    if is_connect_error(&error) {
                        self.system_one_breaker.trip(Instant::now());
                    }
                    return Err(error);
                }
            };
            let bounded = read_limited_body(
                check_status(response, "system_one", url.as_str(), key.as_ref()).await?,
                MAX_RESPONSE_BODY_BYTES,
            )
            .await
            .context("invalid system_one response")?;
            ensure!(
                !bounded.truncated,
                "system_one response body exceeds {MAX_RESPONSE_BODY_BYTES} byte limit"
            );
            ranks.extend(parse_wire_scores(&bounded.bytes, range, total)?);
        }
        ranks.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        Ok(ranks)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::{
        net::SocketAddr,
        sync::atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    /// An address on which nothing listens. Where the platform allows it the
    /// socket is bound but never listens, so connections are refused and,
    /// while it lives, no parallel test's server can take the port (releasing
    /// it raced with them). Shared with the provider tests.
    pub(crate) struct Refused(
        #[allow(dead_code)] Option<tokio::net::TcpSocket>,
        SocketAddr,
    );

    impl std::fmt::Display for Refused {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            self.1.fmt(f)
        }
    }

    pub(crate) fn closed_local_address() -> Refused {
        // macOS does not refuse a connect to a loopback port that is bound but
        // not listening: it sends no reset, so the client waits out its connect
        // timeout (~5 s). A port that was listening and has been closed does
        // answer with a reset there, so on macOS take a free port by binding a
        // listener and dropping it. That leaves a small window in which a
        // parallel test's server could take the port; the bound-socket form
        // used elsewhere has none, and Linux and Windows refuse it at once.
        #[cfg(target_os = "macos")]
        {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            drop(listener);
            Refused(None, address)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let socket = tokio::net::TcpSocket::new_v4().unwrap();
            socket.bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let address = socket.local_addr().unwrap();
            Refused(Some(socket), address)
        }
    }

    /// What the fake model server does with one request on a connection.
    #[derive(Clone, Copy)]
    enum Step {
        /// Answer with a well-formed one-vector embeddings response and keep
        /// the connection open.
        Reply,
        /// Answer HTTP 500.
        ServerError,
        /// Reject the request and echo its request line and a password.
        EchoError(u16, &'static str),
        /// Echo a value at both edges of the response body.
        EchoEdgeValue(&'static str),
        /// Answer HTTP 500 with a body larger than the model response limit.
        OversizedError,
        /// Answer with a valid embedding response followed by enough
        /// whitespace to exceed the model response limit.
        OversizedReply,
        /// Answer with a valid reranker response followed by enough
        /// whitespace to exceed the model response limit.
        OversizedRerank,
        /// Redirect the request to another fake server.
        Redirect(SocketAddr),
        /// Answer HTTP 200 with this JSON body.
        Json(&'static str),
        /// Answer HTTP 422 (a state or row limit, as the typed service does).
        Unprocessable,
        /// Read the whole request, then close the connection without
        /// answering: the peer's FIN reaches a client waiting for a response.
        Close,
        /// Never read the request and close the connection: with unread data
        /// the close resets it.
        Reset,
    }

    /// A loopback server (never one of the real model ports) whose n-th
    /// accepted connection runs the steps of `scripts[n]`, one per request;
    /// the last script serves every later connection. A connection is closed
    /// when its script ends.
    struct FakeModelServer {
        address: SocketAddr,
        connections: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        request_targets: Arc<Mutex<Vec<String>>>,
    }

    impl FakeModelServer {
        async fn start(scripts: Vec<Vec<Step>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let connections = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(AtomicUsize::new(0));
            let request_targets = Arc::new(Mutex::new(Vec::new()));
            let (accepted, handled, targets) = (
                connections.clone(),
                requests.clone(),
                request_targets.clone(),
            );
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let index = accepted.fetch_add(1, Ordering::SeqCst);
                    let script = scripts[index.min(scripts.len() - 1)].clone();
                    tokio::spawn(serve_connection(
                        stream,
                        script,
                        handled.clone(),
                        targets.clone(),
                    ));
                }
            });
            Self {
                address,
                connections,
                requests,
                request_targets,
            }
        }

        /// `Models` pointed at this server for both services, with a short
        /// retry backoff so a test does not wait a real second.
        fn models(&self) -> Models {
            let config = Config {
                embedding_url: format!("http://{}/v1", self.address),
                reranker_url: format!("http://{}/rerank", self.address),
                embedding_timeout_seconds: 10,
                reranker_timeout_seconds: 10,
                ..Config::default()
            };
            let mut models = Models::new(&config).unwrap();
            models.initial_backoff = Duration::from_millis(10);
            models
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }

        fn requests(&self) -> usize {
            self.requests.load(Ordering::SeqCst)
        }

        fn request_targets(&self) -> Vec<String> {
            self.request_targets.lock().unwrap().clone()
        }
    }

    async fn serve_connection(
        mut stream: TcpStream,
        script: Vec<Step>,
        requests: Arc<AtomicUsize>,
        request_targets: Arc<Mutex<Vec<String>>>,
    ) {
        for step in script {
            if let Step::Reset = step {
                // Wait for the request to arrive but leave it unread.
                stream.readable().await.ok();
                requests.fetch_add(1, Ordering::SeqCst);
                return;
            }
            let Some(target) = read_request(&mut stream).await else {
                return;
            };
            request_targets.lock().unwrap().push(target.clone());
            requests.fetch_add(1, Ordering::SeqCst);
            let (status, headers, body) = match step {
                Step::Reply => (
                    "200 OK",
                    String::new(),
                    r#"{"data":[{"index":0,"embedding":[0.5,0.25]}]}"#.to_owned(),
                ),
                Step::OversizedError => (
                    "500 Internal Server Error",
                    String::new(),
                    format!("readable error {}", "x".repeat(MAX_RESPONSE_BODY_BYTES + 1)),
                ),
                Step::OversizedReply => (
                    "200 OK",
                    String::new(),
                    r#"{"data":[{"index":0,"embedding":[0.5,0.25]}]}"#.to_owned()
                        + &" ".repeat(MAX_RESPONSE_BODY_BYTES + 1),
                ),
                Step::OversizedRerank => (
                    "200 OK",
                    String::new(),
                    r#"{"results":[{"index":0,"relevance_score":0.9}]}"#.to_owned()
                        + &" ".repeat(MAX_RESPONSE_BODY_BYTES + 1),
                ),
                Step::Redirect(address) => (
                    "307 Temporary Redirect",
                    format!("location: http://{address}/redirected\r\nconnection: close\r\n"),
                    String::new(),
                ),
                Step::ServerError => (
                    "500 Internal Server Error",
                    String::new(),
                    r#"{"error":"boom"}"#.to_owned(),
                ),
                Step::EchoError(status, password) => (
                    if status == 401 {
                        "401 Unauthorized"
                    } else {
                        "500 Internal Server Error"
                    },
                    String::new(),
                    format!("rejected {target}; userinfo: {password}; retry later"),
                ),
                Step::EchoEdgeValue(value) => (
                    "401 Unauthorized",
                    String::new(),
                    format!("{value}\nrejected {target}; userinfo: {value}"),
                ),
                Step::Json(body) => ("200 OK", String::new(), body.to_owned()),
                Step::Unprocessable => (
                    "422 Unprocessable Entity",
                    String::new(),
                    r#"{"detail":"INPUT_TOO_LONG"}"#.to_owned(),
                ),
                Step::Close | Step::Reset => return,
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n{headers}content-length: {}\r\n\r\n{body}",
                body.len(),
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    /// Reads one HTTP request (head and `content-length` body), returning its
    /// request line or `None` when the client went away first.
    async fn read_request(stream: &mut TcpStream) -> Option<String> {
        let mut received = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            if let Some(head_end) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&received[..head_end]).to_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if received.len() >= head_end + 4 + length {
                    return String::from_utf8_lossy(&received[..head_end])
                        .lines()
                        .next()
                        .map(str::to_owned);
                }
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return None,
                Ok(n) => received.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn documents() -> Vec<String> {
        vec!["fn a() {}".to_owned()]
    }

    /// Whether any error in `error`'s chain is a dropped-connection request
    /// error, as `send_with_retry` sees it.
    fn is_dropped(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(is_connection_dropped)
        })
    }

    #[tokio::test]
    async fn document_embedding_retries_a_connection_closed_or_reset_before_the_response() {
        for (name, lost) in [("closed", Step::Close), ("reset", Step::Reset)] {
            let server = FakeModelServer::start(vec![vec![lost], vec![Step::Reply]]).await;
            let models = server.models();

            let vectors = models.embed(&documents()).await.unwrap();

            assert_eq!(vectors, vec![vec![0.5, 0.25]], "{name}");
            assert_eq!(
                server.connections(),
                2,
                "{name}: one failed attempt, one retry"
            );
            assert_eq!(server.requests(), 2, "{name}");
        }
    }

    /// The measured failure: a keep-alive connection the server closed while
    /// idle, found dead by the next request that reused it.
    #[tokio::test]
    async fn document_embedding_retries_a_stale_pooled_connection() {
        let server =
            FakeModelServer::start(vec![vec![Step::Reply, Step::Close], vec![Step::Reply]]).await;
        let models = server.models();

        assert_eq!(models.embed(&documents()).await.unwrap().len(), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(models.embed(&documents()).await.unwrap().len(), 1);

        // Request 2 reused the pooled connection and was closed unanswered;
        // request 3 is its retry, on a new connection.
        assert_eq!(server.requests(), 3);
        assert_eq!(server.connections(), 2);
    }

    #[tokio::test]
    async fn document_embedding_gives_up_after_max_attempts_on_a_connection_that_always_closes() {
        let server = FakeModelServer::start(vec![vec![Step::Close]]).await;
        let models = server.models();

        let error = models.embed(&documents()).await.unwrap_err();

        assert_eq!(server.connections(), MAX_ATTEMPTS as usize);
        assert!(
            format!("{error:#}").contains("embedding service request failed"),
            "{error:#}"
        );
        // Pins the real error's shape against the predicate: if a dependency
        // change stops it being recognised, this fails and so do the retry
        // tests.
        assert!(is_dropped(&error), "{error:#}");
        assert!(!is_connect_error(&error), "{error:#}");
    }

    #[tokio::test]
    async fn an_http_error_status_is_not_retried() {
        let server = FakeModelServer::start(vec![vec![Step::ServerError]]).await;
        let models = server.models();

        let error = models.embed(&documents()).await.unwrap_err();

        assert_eq!(server.connections(), 1);
        assert_eq!(server.requests(), 1);
        assert!(
            error.downcast_ref::<HttpStatusError>().is_some(),
            "{error:#}"
        );
    }

    fn assert_redacted_http_error(
        error: &anyhow::Error,
        status: u16,
        label: &str,
        body: &str,
        secrets: &[&str],
    ) {
        let http = error.downcast_ref::<HttpStatusError>().unwrap();
        assert_eq!(http.status, status);
        assert_eq!(http.body, body);
        let chain = format!("{error:#}");
        assert_eq!(
            chain,
            format!("{label} HTTP error: HTTP status {status}: {body}")
        );
        let debug = format!("{error:?}");
        for line in body.lines() {
            assert!(debug.contains(line), "{debug}");
        }
        for secret in secrets {
            assert!(!chain.contains(secret), "{chain}");
            assert!(!debug.contains(secret), "{debug}");
        }
    }

    #[tokio::test]
    async fn embedding_http_error_body_redacts_query_values() {
        for status in [401, 500] {
            let server = FakeModelServer::start(vec![vec![Step::EchoError(status, "none")]]).await;
            let mut models = server.models();
            models
                .config
                .embedding_url
                .push_str("?api_key=n89-marker&other=n89-other&again=n89-marker&empty=&flag");
            for error in [
                models.embed(&documents()).await.unwrap_err(),
                models.query("query").await.unwrap_err(),
            ] {
                assert_redacted_http_error(
                    &error,
                    status,
                    "embedding service",
                    "rejected POST /v1/embeddings?api_key=[redacted]&other=[redacted]&again=[redacted]&empty=&flag HTTP/1.1; userinfo: none; retry later",
                    &["n89-marker", "n89-other"],
                );
            }
            assert_eq!(server.requests(), 2, "HTTP errors must not be retried");
        }
    }

    #[tokio::test]
    async fn reranker_http_error_body_redacts_query_values() {
        // (name, step, status, query appended to the URL, expected body, secrets)
        type Case<'a> = (&'a str, Step, u16, &'a str, &'a str, &'a [&'a str]);
        let cases: [Case; 5] = [
            (
                "plain value, 401",
                Step::EchoError(401, "none"),
                401,
                "?api_key=n89-marker",
                "rejected POST /rerank?api_key=[redacted] HTTP/1.1; userinfo: none; retry later",
                &["n89-marker"],
            ),
            (
                "plain value, 500",
                Step::EchoError(500, "none"),
                500,
                "?api_key=n89-marker",
                "rejected POST /rerank?api_key=[redacted] HTTP/1.1; userinfo: none; retry later",
                &["n89-marker"],
            ),
            (
                "encoded and overlapping values",
                Step::EchoError(500, "n89 encoded+value"),
                500,
                "?api_key=n89%20encoded%2Bvalue&short=n89-overlap&long=n89-overlap-long&literal=redacted",
                "rejected POST /rerank?api_key=[redacted]&short=[redacted]&long=[redacted]&literal=[redacted] HTTP/1.1; userinfo: [redacted]; retry later",
                &["n89%20encoded%2Bvalue", "n89 encoded+value", "n89-overlap"],
            ),
            (
                "values redacted before trimming",
                Step::EchoEdgeValue(" n89-edge "),
                401,
                "?api_key=%20n89-edge%20",
                "[redacted]\nrejected POST /rerank?api_key=[redacted] HTTP/1.1; userinfo: [redacted]",
                &["n89-edge"],
            ),
            (
                "short values are redacted without mangling the body",
                Step::EchoError(401, "none"),
                401,
                "?k=a&v=1&n=on",
                "rejected POST /rerank?k=[redacted]&v=[redacted]&n=[redacted] HTTP/1.1; userinfo: none; retry later",
                &[],
            ),
        ];
        for (name, step, status, query, body, secrets) in cases {
            let server = FakeModelServer::start(vec![vec![step]]).await;
            let mut models = server.models();
            models.config.reranker_url.push_str(query);
            let error = models.rerank("query", &documents()).await.unwrap_err();
            assert_redacted_http_error(&error, status, "reranker", body, secrets);
            assert_eq!(server.requests(), 1, "{name}");
        }
    }

    #[tokio::test]
    async fn model_http_error_body_redacts_userinfo_password() {
        for (status, password) in [(401, "n89-password"), (500, "n89+p&ass word")] {
            let server =
                FakeModelServer::start(vec![vec![Step::EchoError(status, password)]]).await;
            let mut url = reqwest::Url::parse(&format!("http://{}/v1", server.address)).unwrap();
            url.set_username("user").unwrap();
            url.set_password(Some(password)).unwrap();
            let encoded_password = url.password().unwrap().to_owned();
            let embedding_url = url.to_string();
            url.set_path("/rerank");
            let config = Config {
                embedding_url,
                reranker_url: url.to_string(),
                ..Config::default()
            };
            let models = Models::new(&config).unwrap();
            let errors = [
                (
                    models.embed(&documents()).await.unwrap_err(),
                    "embedding service",
                    "/v1/embeddings",
                ),
                (
                    models.rerank("query", &documents()).await.unwrap_err(),
                    "reranker",
                    "/rerank",
                ),
            ];
            for (error, label, path) in errors {
                assert_redacted_http_error(
                    &error,
                    status,
                    label,
                    &format!("rejected POST {path} HTTP/1.1; userinfo: [redacted]; retry later"),
                    &[password, &encoded_password],
                );
            }
        }
    }

    #[tokio::test]
    async fn embedding_http_error_body_redacts_before_truncation() {
        // (path length, query secret, whether the body is cut at the cap)
        for (path_length, secret, truncated) in [
            (
                546,
                "n89-long-query-value-crossing-the-character-limit",
                true,
            ),
            // A redaction marker that ends past the cap is kept whole.
            (559, "n89-value-at-the-character-limit", false),
        ] {
            let server = FakeModelServer::start(vec![vec![Step::EchoError(401, "none")]]).await;
            let mut models = server.models();
            let path = format!("/{}", "x".repeat(path_length));
            models.config.embedding_url =
                format!("http://{}{path}?api_key={secret}", server.address);
            let error = models.embed(&documents()).await.unwrap_err();
            let expected = if truncated {
                let full = format!(
                    "rejected POST {path}/embeddings?api_key=[redacted] HTTP/1.1; userinfo: none; retry later"
                );
                let expected: String = full.chars().take(MAX_ERROR_BODY_CHARS).collect();
                assert!(expected.contains("[redacted]"));
                expected
            } else {
                let expected = format!("rejected POST {path}/embeddings?api_key=[redacted]");
                assert!(expected.chars().count() > MAX_ERROR_BODY_CHARS);
                expected
            };
            assert_redacted_http_error(
                &error,
                401,
                "embedding service",
                &expected,
                &[secret, "n89-long"],
            );
        }
    }

    #[tokio::test]
    async fn embedding_endpoint_appends_to_the_path_before_its_query() {
        let server = FakeModelServer::start(vec![vec![Step::Reply]]).await;
        let config = Config {
            embedding_url: format!("http://{}/v1?tenant=local", server.address),
            ..Config::default()
        };
        let models = Models::new(&config).unwrap();

        models.embed(&documents()).await.unwrap();

        assert_eq!(
            server.request_targets(),
            ["POST /v1/embeddings?tenant=local HTTP/1.1"]
        );
    }

    /// The configured URL may end in any number of slashes: all of them are
    /// dropped before `/embeddings`, as `embedding_identity` does.
    #[tokio::test]
    async fn embedding_endpoint_ignores_every_trailing_slash() {
        for suffix in ["", "/", "//"] {
            let server = FakeModelServer::start(vec![vec![Step::Reply]]).await;
            let config = Config {
                embedding_url: format!("http://{}/v1{suffix}", server.address),
                ..Config::default()
            };
            let models = Models::new(&config).unwrap();

            models.embed(&documents()).await.unwrap();

            assert_eq!(
                server.request_targets(),
                ["POST /v1/embeddings HTTP/1.1"],
                "embedding_url ending in {suffix:?}"
            );
        }
    }

    #[tokio::test]
    async fn embedding_post_is_not_replayed_to_a_redirect_target() {
        let target = FakeModelServer::start(vec![vec![Step::Reply]]).await;
        let redirect = FakeModelServer::start(vec![vec![Step::Redirect(target.address)]]).await;
        let models = redirect.models();

        let error = models.embed(&documents()).await.unwrap_err();

        assert_eq!(
            error
                .downcast_ref::<HttpStatusError>()
                .map(|error| error.status),
            Some(307),
            "{error:#}"
        );
        assert_eq!(redirect.requests(), 1);
        assert_eq!(target.requests(), 0, "the POST was replayed after redirect");
    }

    #[tokio::test]
    async fn oversized_error_body_is_capped_and_keeps_its_readable_prefix() {
        let server = FakeModelServer::start(vec![vec![Step::OversizedError]]).await;
        let models = server.models();

        let error = models.embed(&documents()).await.unwrap_err();
        let status = error.downcast_ref::<HttpStatusError>().unwrap();

        assert!(
            status.body.starts_with("readable error"),
            "the useful error prefix should remain visible: {:?}",
            status.body
        );
        assert!(
            status.body.ends_with("[truncated]"),
            "the bounded read should be reported: {:?}",
            status.body
        );
    }

    #[tokio::test]
    async fn oversized_success_body_is_rejected_at_the_response_limit() {
        let server = FakeModelServer::start(vec![vec![Step::OversizedReply]]).await;
        let models = server.models();
        let error = models.embed(&documents()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("response body exceeds"),
            "embedding: {error:#}"
        );

        let server = FakeModelServer::start(vec![vec![Step::OversizedRerank]]).await;
        let models = server.models();
        let error = models.rerank("query", &documents()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("reranker response body exceeds"),
            "reranker: {error:#}"
        );
    }

    #[tokio::test]
    async fn the_query_embedding_and_the_reranker_make_one_attempt_on_a_dropped_connection() {
        let server = FakeModelServer::start(vec![vec![Step::Close], vec![Step::Reply]]).await;
        let models = server.models();
        let error = models.query("anything").await.unwrap_err();
        assert!(is_dropped(&error), "query: {error:#}");
        assert_eq!(server.connections(), 1, "query: fail-fast, no retry");
        assert_eq!(server.requests(), 1);

        let server = FakeModelServer::start(vec![vec![Step::Close], vec![Step::Reply]]).await;
        let models = server.models();
        let error = models.rerank("query", &documents()).await.unwrap_err();
        assert!(is_dropped(&error), "rerank: {error:#}");
        assert_eq!(server.connections(), 1, "rerank: fail-fast, no retry");
        // A dropped connection is not a refused one, so the reranker is not
        // skipped afterwards.
        assert_eq!(models.reranker_breaker.remaining(Instant::now()), None);
    }

    #[tokio::test]
    async fn only_a_request_that_lost_its_connection_counts_as_dropped() {
        let models = Models::new(&Config::default()).unwrap();

        // A builder error: the request never left.
        let invalid = models.client().get("not a url").send().await.unwrap_err();
        assert!(!is_connection_dropped(&invalid), "{invalid:#}");

        // A refused connection is a connect error, not a dropped one.
        let closed = closed_local_address();
        let url = format!("http://{closed}/");
        let refused = models.client().get(url).send().await.unwrap_err();
        assert!(
            refused.is_connect() && !is_connection_dropped(&refused),
            "{refused:#}"
        );
    }

    #[test]
    fn query_templates_format_the_query() {
        // The default is the former hard-coded instruction.
        assert_eq!(
            crate::config::DEFAULT_EMBEDDING_QUERY_TEMPLATE,
            format!("{QUERY_INSTRUCTION}{{query}}")
        );
        assert_eq!(
            QUERY_INSTRUCTION,
            "Instruct: Given a code search query, retrieve relevant code passages that answer the query\nQuery: "
        );
        let models = Models::new(&Config::default()).unwrap();
        assert_eq!(
            models.query_text("find the parser"),
            format!("{QUERY_INSTRUCTION}find the parser")
        );

        // A custom template.
        let config = Config {
            embedding_query_template: "task: code retrieval | query: {query}".into(),
            ..Config::default()
        };
        let models = Models::new(&config).unwrap();
        assert_eq!(
            models.query_text("find the parser"),
            "task: code retrieval | query: find the parser"
        );
        // The query is substituted as is and never re-scanned.
        assert_eq!(
            models.query_text("what is {query}?"),
            "task: code retrieval | query: what is {query}?"
        );

        // A multi-line template, as the bge-code format uses.
        let multi_line = Config {
            embedding_query_template: "<instruct>Retrieve code.\n<query>{query}".into(),
            ..Config::default()
        };
        assert_eq!(
            Models::new(&multi_line)
                .unwrap()
                .query_text("find the parser"),
            "<instruct>Retrieve code.\n<query>find the parser"
        );
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
        let closed = closed_local_address();
        let config = Config {
            reranker_url: format!("http://{closed}/rerank"),
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
    async fn a_refused_reranker_error_does_not_carry_the_url_credentials() {
        let closed = closed_local_address();
        let config = Config {
            reranker_url: format!("http://{closed}/rerank?api_key=marker"),
            ..Config::default()
        };
        let models = Models::new(&config).unwrap();
        let documents = vec!["fn a() {}".to_owned()];

        let error = models.rerank("query", &documents).await.unwrap_err();
        assert!(is_connect_error(&error), "{error:#}");
        assert!(!format!("{error:#}").contains("marker"), "{error:#}");
        assert!(!format!("{error:?}").contains("marker"), "{error:?}");
    }

    #[tokio::test]
    async fn a_refused_query_embedding_makes_one_attempt_and_hides_the_url_credentials() {
        let closed = closed_local_address();
        let config = Config {
            embedding_url: format!("http://{closed}/v1?api_key=marker"),
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
        assert!(!format!("{error:#}").contains("marker"), "{error:#}");
        assert!(!format!("{error:?}").contains("marker"), "{error:?}");
    }

    /// The API key goes out as a bearer token, and a server that echoes the
    /// request back in an error cannot put it into the error.
    #[tokio::test]
    async fn the_bearer_key_is_sent_and_redacted_from_an_echoed_error() {
        const SECRET: &str = "sk-echo-me-123";
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut seen = Vec::new();
            let mut buffer = [0u8; 1024];
            while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut buffer).await.unwrap();
                seen.extend_from_slice(&buffer[..n]);
            }
            let echoed = String::from_utf8_lossy(&seen).into_owned();
            let line = echoed
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization"))
                .unwrap_or("no authorization header");
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{line}",
                line.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let endpoint = crate::endpoint::Endpoint {
            url: format!("http://{address}/rerank"),
            model: "m".into(),
            api_key_env: Some("TEST_KEY".into()),
            allow_remote: true,
        };
        let key = endpoint
            .api_key_from(|_| Some(SECRET.into()))
            .unwrap()
            .unwrap();
        let response = with_bearer(reqwest::Client::new().post(&endpoint.url), Some(&key))
            .send()
            .await
            .unwrap();
        let error = format!(
            "{:#}",
            check_status(response, "reranker", &endpoint.url, Some(&key))
                .await
                .unwrap_err()
        );
        assert!(!error.contains(SECRET), "{error}");
        assert!(
            error.contains("[redacted]"),
            "the header was not sent: {error}"
        );
    }

    /// `Models` set to rescore with the typed service at `address`.
    fn wire_models(address: impl std::fmt::Display, batch_size: usize) -> Models {
        let config = Config {
            rescorer: Rescorer::SystemOne,
            system_one: Some(crate::provider::SystemOneConfig {
                provider: ProviderKind::SystemOneWire,
                url: format!("http://{address}"),
                model: "decider".into(),
                timeout_ms: 5_000,
                batch_size,
                ..Default::default()
            }),
            ..Config::default()
        };
        config.validate().unwrap();
        Models::new(&config).unwrap()
    }

    fn wire_candidates(count: usize) -> Vec<WireCandidate> {
        (0..count)
            .map(|i| WireCandidate::new(&format!("f{i}.rs"), 1, "doc"))
            .collect()
    }

    /// An answer for candidates c01..c05 where c04 is the likeliest, c02 and
    /// c05 tie, and c01 is the least likely.
    const FIVE_ANSWERS: &str = r#"{"model":"decider-1","answers":{
        "q_c01":{"noul":0.1},"q_c02":{"noul":0.5},"q_c03":{"noul":0.3},
        "q_c04":{"noul":0.9},"q_c05":{"noul":0.5}}}"#;

    #[tokio::test]
    async fn system_one_scores_order_the_shortlist_in_batches_with_ties_in_fusion_order() {
        // Three requests of two, two and one candidate; the same body answers
        // all three because each batch reads only its own ids.
        let server = FakeModelServer::start(vec![vec![Step::Json(FIVE_ANSWERS); 3]]).await;
        let models = wire_models(server.address, 2);
        let ranks = models
            .system_one_rank("where is it", &wire_candidates(5))
            .await
            .unwrap();
        assert_eq!(
            ranks,
            [(3, 0.9), (1, 0.5), (4, 0.5), (2, 0.3), (0, 0.1)],
            "best first; the 0.5 tie keeps the earlier candidate first"
        );
        assert_eq!(server.requests(), 3);
        assert!(
            server
                .request_targets()
                .iter()
                .all(|target| target.starts_with("POST /v1/systemone ")),
            "{:?}",
            server.request_targets()
        );
    }

    #[tokio::test]
    async fn system_one_failures_are_errors_and_only_a_refused_connection_trips_the_breaker() {
        for (name, step, needle) in [
            ("server error", Step::ServerError, "HTTP status 500"),
            ("row limit", Step::Unprocessable, "HTTP status 422"),
            (
                "missing candidate",
                Step::Json(r#"{"answers":{"q_c01":{"noul":0.1},"q_c02":{"noul":0.2}}}"#),
                "no answer for c03",
            ),
            (
                "not a probability",
                Step::Json(
                    r#"{"answers":{"q_c01":{"noul":0.1},"q_c02":{"noul":7},"q_c03":{"noul":0.2}}}"#,
                ),
                "outside 0..=1",
            ),
            (
                "malformed",
                Step::Json(r#"{"results":[]}"#),
                "invalid system_one wire response",
            ),
        ] {
            let server = FakeModelServer::start(vec![vec![step; 2]]).await;
            let models = wire_models(server.address, 24);
            let candidates = wire_candidates(3);
            for attempt in 1..=2 {
                let error = models.system_one_rank("q", &candidates).await.unwrap_err();
                let text = format!("{error:#}");
                assert!(text.contains(needle), "{name}: {text}");
                assert!(!text.contains("recently unreachable"), "{name}: {text}");
                assert_eq!(
                    server.requests(),
                    attempt,
                    "{name}: the service is asked again"
                );
            }
        }

        let closed = closed_local_address();
        let models = wire_models(&closed, 24);
        let candidates = wire_candidates(2);
        let first = models.system_one_rank("q", &candidates).await.unwrap_err();
        assert!(is_connect_error(&first), "{first:#}");
        let started = Instant::now();
        let skipped = models.system_one_rank("q", &candidates).await.unwrap_err();
        assert!(
            format!("{skipped:#}").contains("recently unreachable"),
            "{skipped:#}"
        );
        assert!(!is_connect_error(&skipped), "{skipped:#}");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn system_one_needs_its_key_variable_and_never_shows_a_value() {
        let server = FakeModelServer::start(vec![vec![Step::Json(FIVE_ANSWERS)]]).await;
        let mut models = wire_models(server.address, 24);
        models.config.system_one.as_mut().unwrap().api_key_env =
            Some("LCI_TEST_SYSTEM_ONE_KEY_NOT_SET".into());
        let error = models
            .system_one_rank("q", &wire_candidates(5))
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("LCI_TEST_SYSTEM_ONE_KEY_NOT_SET"), "{text}");
        assert_eq!(server.requests(), 0, "no request without the key");

        // Without the wire provider the rung refuses instead of guessing.
        let plain = Models::new(&Config::default()).unwrap();
        let error = plain
            .system_one_rank("q", &wire_candidates(1))
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("system_one_wire"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn system_one_validates_unvalidated_configs_before_sending() {
        for (url, needle) in [
            ("http://192.0.2.1:9", "system_one.allow_remote"),
            ("ftp://192.0.2.1:9", "must use HTTP(S)"),
        ] {
            let config = Config {
                rescorer: Rescorer::SystemOne,
                system_one: Some(crate::provider::SystemOneConfig {
                    provider: ProviderKind::SystemOneWire,
                    url: url.into(),
                    allow_remote: false,
                    timeout_ms: 5_000,
                    ..Default::default()
                }),
                ..Config::default()
            };
            let models = Models::new(&config).unwrap();
            let started = Instant::now();
            let error = models
                .system_one_rank("q", &wire_candidates(1))
                .await
                .unwrap_err();
            assert!(started.elapsed() < Duration::from_millis(500));
            assert!(error.to_string().contains(needle), "{error:#}");
            assert!(!error.chain().any(|cause| cause.is::<reqwest::Error>()));
        }
    }

    #[tokio::test]
    async fn system_one_signed_zero_scores_keep_fusion_order() {
        let server = FakeModelServer::start(vec![vec![Step::Json(
            r#"{"answers":{"q_c01":{"noul":-0.0},"q_c02":{"noul":0.0}}}"#,
        )]])
        .await;
        let ranks = wire_models(server.address, 24)
            .system_one_rank("q", &wire_candidates(2))
            .await
            .unwrap();
        assert_eq!(ranks, [(0, 0.0), (1, 0.0)]);
        assert!(ranks.iter().all(|(_, score)| score.is_sign_positive()));
        assert_eq!(server.requests(), 1);
    }
}
