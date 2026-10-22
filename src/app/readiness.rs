//! Bounded, concurrent readiness probing for every required and optional
//! dependency, shared by `/ready` and the `service_status` MCP tool.

use super::App;
use crate::{
    lexical,
    lsp::adapter::{fresh_probes, probe_subprocess_version},
};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use std::{path::Path, sync::Arc};

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessComponent {
    pub name: &'static str,
    pub ready: bool,
    pub required: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessDegraded {
    pub name: &'static str,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub ready: bool,
    pub can_create_semantic_index: bool,
    pub components: Vec<ReadinessComponent>,
    pub degraded: Vec<ReadinessDegraded>,
}

/// Outcome of a model-listing probe. `NotExposed` (HTTP 404/405) is neutral for
/// an optional component but a failure for a required one.
#[derive(Debug, Clone)]
pub(crate) enum ModelListing {
    Listed,
    NotExposed,
    Failed(String),
}

/// What a model-listing endpoint answered: the model ids it lists,
/// `NotExposed` for HTTP 404/405, or why it could not be read.
#[derive(Debug, Clone)]
pub(crate) enum ModelIds {
    Listed(Vec<String>),
    NotExposed,
    Failed(String),
}

/// Per-process monotonic sequence for readiness probe filenames. Combined with
/// the PID and a wall-clock timestamp, concurrent probes stay unique.
static READINESS_PROBE_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Longest `data_dir` (in characters) that leaves room on Windows for the
/// path LanceDB binds to MAX_PATH below it.
///
/// That path is Lance's data file, `data\<50-character name>.lance`, which is
/// 96 characters below `data_dir` (counting `\lancedb\ws_<16 hex>.lance\`).
/// lance-io's `LocalWriter` persists it at shutdown by moving a tempfile with
/// a raw `MoveFileExW` call (no `\\?\` prefix), so MAX_PATH applies: the
/// ceiling is 259 - 96 = 163 characters (about 110 with the old 64-hex table
/// name). The longer staging manifest under `_versions\` goes through
/// `object_store`'s `std` rename, which `std` makes verbatim-aware, so it is
/// not bound, and LCI creates no indices. 150 leaves 13 characters of slack.
///
/// Measured on a Windows machine: the old binary failed at a 139-character
/// `data_dir`; the new one worked at 139 and still failed at 199.
const WINDOWS_DATA_DIR_LENGTH_LIMIT: usize = 150;

/// Optional readiness component that flags a `data_dir` too long for Windows'
/// 260-character path limit, which LanceDB hits as `os error 3` once its
/// internal paths are appended. Never required, so it degrades the report
/// without changing `ready`. `windows` and `limit` are parameters so the rule
/// is testable on any platform.
fn data_dir_length_component(data_dir: &Path, windows: bool, limit: usize) -> ReadinessComponent {
    let length = data_dir.to_string_lossy().chars().count();
    let too_long = windows && length > limit;
    ReadinessComponent {
        name: "data_dir_path_length",
        ready: !too_long,
        required: false,
        detail: Some(if too_long {
            format!(
                "data_dir is {length} characters; above {limit}, LanceDB's internal paths can exceed Windows' 260-character limit (os error 3); use a shorter data_dir"
            )
        } else {
            format!("data_dir is {length} characters")
        }),
    }
}

impl App {
    /// Builds the shared, serializable readiness report. Each named component
    /// is probed independently and concurrently; required components gate the
    /// overall `ready` / `can_create_semantic_index` outcome while optional
    /// components only surface as degraded.
    pub async fn service_status(&self) -> ServiceStatus {
        let timeout = std::time::Duration::from_secs(self.config.readiness_timeout_seconds);
        let data_dir = self.config.data_dir.clone();
        let lancedb_path = self.config.data_dir.join("lancedb");
        let embedding_url = self.config.embedding_url.clone();
        let embedding_model = self.config.embedding_model.clone();
        let reranker_url = self.config.reranker_url.clone();
        let reranker_model = self.config.reranker_model.clone();
        let ripgrep_resolved = lexical::resolve_ripgrep_path(&self.config.ripgrep_path);
        let client = self.models.client();

        let lsp_probes_future =
            futures::future::join_all(self.analyzer.adapters().iter().map(|adapter| {
                let adapter = Arc::clone(adapter);
                async move {
                    let probe: Result<()> = if !adapter.enabled() {
                        Ok(())
                    } else {
                        fresh_probes(adapter.probe_readiness(timeout)).await
                    };
                    (adapter, probe)
                }
            }));

        let (
            data_dir_probe,
            lancedb_probe,
            embedding_reach,
            embedding_models,
            reranker_reach,
            reranker_models,
            ripgrep_probe,
            lsp_probes,
        ) = tokio::join!(
            Self::probe_data_dir(&data_dir),
            self.store.accessibility_probe(&lancedb_path),
            probe_endpoint_reach(client, &embedding_url, "embedding endpoint", timeout),
            probe_model_listing(
                client,
                &embedding_url,
                &embedding_model,
                "embedding model listing",
                timeout
            ),
            probe_endpoint_reach(client, &reranker_url, "reranker endpoint", timeout),
            probe_model_listing(
                client,
                &reranker_url,
                &reranker_model,
                "reranker model listing",
                timeout
            ),
            fresh_probes(probe_subprocess_version(&ripgrep_resolved, timeout)),
            lsp_probes_future,
        );

        let data_dir_ready = data_dir_probe.is_ok();
        let lancedb_ready = lancedb_probe.is_ok();
        let embedding_reach_ready = embedding_reach.is_ok();
        let reranker_reach_ready = reranker_reach.is_ok();
        let ripgrep_ready = ripgrep_probe.is_ok();

        // Required embedding listing: any non-Listed outcome fails readiness.
        let (embedding_models_ready, embedding_models_detail) = match &embedding_models {
            ModelListing::Listed => (true, None),
            ModelListing::NotExposed => (
                false,
                Some("model listing not exposed (HTTP 404/405)".into()),
            ),
            ModelListing::Failed(reason) => (false, Some(reason.clone())),
        };
        // Optional reranker listing: NotExposed is neutral (ready, no degrade);
        // only a Failed outcome degrades (handled by the generic loop below).
        let (reranker_models_ready, reranker_models_detail) = match &reranker_models {
            ModelListing::Listed => (true, None),
            ModelListing::NotExposed => (
                true,
                Some("model listing not exposed (HTTP 404/405)".into()),
            ),
            ModelListing::Failed(reason) => (false, Some(reason.clone())),
        };

        let components = vec![
            ReadinessComponent {
                name: "writable_data_dir",
                ready: data_dir_ready,
                required: true,
                detail: data_dir_probe.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "lancedb_accessible",
                ready: lancedb_ready,
                required: true,
                detail: lancedb_probe.err().map(|e| format!("{e:#}")),
            },
            data_dir_length_component(&data_dir, cfg!(windows), WINDOWS_DATA_DIR_LENGTH_LIMIT),
            ReadinessComponent {
                name: "embedding_endpoint_reachable",
                ready: embedding_reach_ready,
                required: true,
                detail: embedding_reach.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "embedding_model_listed",
                ready: embedding_models_ready,
                required: true,
                detail: embedding_models_detail,
            },
            ReadinessComponent {
                name: "reranker_endpoint_reachable",
                ready: reranker_reach_ready,
                required: false,
                detail: reranker_reach.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "reranker_model_listed",
                ready: reranker_models_ready,
                required: false,
                detail: reranker_models_detail,
            },
            ReadinessComponent {
                name: "ripgrep_available",
                ready: ripgrep_ready,
                required: false,
                detail: Some(match ripgrep_probe {
                    Ok(()) => format!("resolved ripgrep at {}", ripgrep_resolved.display()),
                    Err(e) => format!(
                        "ripgrep at {} unavailable: {e:#}",
                        ripgrep_resolved.display()
                    ),
                }),
            },
        ];
        let components: Vec<ReadinessComponent> = components
            .into_iter()
            .chain(lsp_probes.iter().map(|(adapter, probe)| {
                let detail = match adapter.configured_command() {
                    None => format!("{} not configured", adapter.provider()),
                    Some(_) if !adapter.enabled() => format!("{} disabled", adapter.provider()),
                    Some(command) => match probe {
                        Ok(()) => format!("configured {} command {command}", adapter.provider()),
                        Err(e) => format!(
                            "configured {} command {command} unavailable: {e:#}",
                            adapter.provider()
                        ),
                    },
                };
                ReadinessComponent {
                    name: adapter.readiness_component_name(),
                    ready: probe.is_ok(),
                    required: false,
                    detail: Some(detail),
                }
            }))
            .collect();

        // The report requests overall degraded components/reasons, so every
        // unavailable component is listed with its reason, required or not.
        let mut degraded = Vec::new();
        for c in &components {
            if !c.ready {
                degraded.push(ReadinessDegraded {
                    name: c.name,
                    reason: c.detail.clone().unwrap_or_else(|| "unavailable".into()),
                });
            }
        }

        let ready =
            data_dir_ready && lancedb_ready && embedding_reach_ready && embedding_models_ready;
        let can_create_semantic_index = ready;

        ServiceStatus {
            ready,
            can_create_semantic_index,
            components,
            degraded,
        }
    }

    async fn probe_data_dir(data_dir: &Path) -> Result<()> {
        let data_dir = data_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir)?;
            // Unique per process, call, and instant so concurrent /ready and
            // service_status probes cannot collide on the same filename.
            let sequence =
                READINESS_PROBE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let probe = data_dir.join(format!(
                ".readiness_probe_{}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                sequence
            ));
            let result = (|| -> std::io::Result<()> {
                std::fs::write(&probe, b"probe")?;
                std::fs::metadata(&probe)?;
                Ok(())
            })();
            // Always attempt cleanup, even when the write or metadata
            // verification above failed, so a failed probe leaves no residue.
            let _ = std::fs::remove_file(&probe);
            result.with_context(|| format!("write readiness probe in {}", data_dir.display()))
        })
        .await?
    }
}

/// Classifies a reqwest transport error by class only. reqwest's `Display`
/// embeds the request URL, which may contain credentials or query secrets,
/// so readiness reports must never echo the raw error text.
pub(crate) fn reqwest_error_class(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failure"
    } else {
        "request failure"
    }
}

/// Contacts the configured base URL. Any HTTP response (including 404/405)
/// proves the endpoint is reachable; only a connection failure or timeout
/// marks it unreachable. Errors identify the dependency generically via
/// `label` because configured URLs may contain credentials or query
/// secrets that must not be echoed into readiness reports.
pub(crate) async fn probe_endpoint_reach(
    client: &reqwest::Client,
    url: &str,
    label: &str,
    timeout: std::time::Duration,
) -> Result<()> {
    let target = url.trim_end_matches('/');
    client
        .get(target)
        .timeout(timeout)
        .send()
        .await
        .map_err(|error| anyhow!("connect to {label}: {}", reqwest_error_class(&error)))?;
    Ok(())
}

/// The OpenAI-compatible model listing URL for a configured service URL: the
/// configured URL's path prefix followed by `/v1/models`. `…/prefix/v1` (the
/// embedder form) lists at `…/prefix/v1/models`, and an endpoint URL such as
/// `…/prefix/rerank` or `…/prefix/embeddings` at `…/prefix/v1/models`. A URL
/// with no path lists at `/v1/models`. The query and fragment are dropped.
pub(crate) fn model_listing_url(url: &reqwest::Url) -> reqwest::Url {
    let path = url.path().trim_end_matches('/');
    let path = ["/rerank", "/embeddings"]
        .iter()
        .find_map(|endpoint| path.strip_suffix(endpoint))
        .unwrap_or(path);
    let prefix = path.strip_suffix("/v1").unwrap_or(path);
    let mut models_url = url.clone();
    models_url.set_path(&format!("{prefix}/v1/models"));
    models_url.set_query(None);
    models_url.set_fragment(None);
    models_url
}

/// Reads the model ids the OpenAI-compatible listing endpoint derived from
/// the configured URL (see [`model_listing_url`]) reports. A 404/405 means
/// the listing is not exposed. Errors identify the dependency generically via
/// `label` (with HTTP status where relevant) because configured URLs may
/// contain credentials or query secrets that must not be echoed into
/// readiness reports.
pub(crate) async fn fetch_model_ids(
    client: &reqwest::Client,
    url: &str,
    label: &str,
    timeout: std::time::Duration,
) -> ModelIds {
    let parsed = match reqwest::Url::parse(url) {
        Ok(u) => u,
        Err(e) => return ModelIds::Failed(format!("invalid {label} URL: {e}")),
    };
    let models_url = model_listing_url(&parsed);
    let response = match client.get(models_url).timeout(timeout).send().await {
        Ok(r) => r,
        Err(e) => {
            return ModelIds::Failed(format!("connect to {label}: {}", reqwest_error_class(&e)));
        }
    };
    let status = response.status();
    if status.as_u16() == 404 || status.as_u16() == 405 {
        return ModelIds::NotExposed;
    }
    if !status.is_success() {
        return ModelIds::Failed(format!("{label} HTTP error: {status}"));
    }
    let body: serde_json::Value = match response.json().await {
        Ok(b) => b,
        Err(e) => return ModelIds::Failed(format!("invalid {label} response: {e:#}")),
    };
    let ids = body
        .get("data")
        .and_then(|d| d.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ModelIds::Listed(ids)
}

/// Derives the likely OpenAI-compatible model listing endpoint from the
/// configured URL origin and checks that the configured model is present
/// (see [`fetch_model_ids`]).
pub(crate) async fn probe_model_listing(
    client: &reqwest::Client,
    url: &str,
    model: &str,
    label: &str,
    timeout: std::time::Duration,
) -> ModelListing {
    match fetch_model_ids(client, url, label, timeout).await {
        ModelIds::NotExposed => ModelListing::NotExposed,
        ModelIds::Failed(reason) => ModelListing::Failed(reason),
        ModelIds::Listed(ids) if ids.iter().any(|id| id == model) => ModelListing::Listed,
        ModelIds::Listed(_) => {
            ModelListing::Failed(format!("configured model {model} not present in {label}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_of(characters: usize) -> std::path::PathBuf {
        "d".repeat(characters).into()
    }

    #[test]
    fn data_dir_length_warns_only_on_windows_above_the_limit() {
        let at_limit = data_dir_length_component(&dir_of(150), true, 150);
        assert_eq!(at_limit.name, "data_dir_path_length");
        assert!(at_limit.ready);
        assert!(!at_limit.required);
        assert_eq!(
            at_limit.detail.as_deref(),
            Some("data_dir is 150 characters")
        );

        let above = data_dir_length_component(&dir_of(151), true, 150);
        assert!(!above.ready);
        assert!(!above.required);
        let detail = above.detail.unwrap();
        assert!(detail.contains("151 characters"), "{detail}");
        assert!(detail.contains("260"), "{detail}");
        assert!(detail.contains("shorter data_dir"), "{detail}");

        // Other platforms have no such limit, however long the path.
        let elsewhere = data_dir_length_component(&dir_of(300), false, 150);
        assert!(elsewhere.ready);
        assert!(!elsewhere.required);
        assert_eq!(
            elsewhere.detail.as_deref(),
            Some("data_dir is 300 characters")
        );
    }

    #[test]
    fn model_listing_url_keeps_the_configured_path_prefix() {
        let derive = |url: &str| model_listing_url(&reqwest::Url::parse(url).unwrap()).to_string();
        // Defaults and origin-only URLs list at the origin's `/v1/models`.
        assert_eq!(
            derive("http://localhost:8766/v1"),
            "http://localhost:8766/v1/models"
        );
        assert_eq!(
            derive("http://localhost:8766/v1/"),
            "http://localhost:8766/v1/models"
        );
        assert_eq!(
            derive("http://localhost:8767/rerank"),
            "http://localhost:8767/v1/models"
        );
        assert_eq!(derive("http://h:1"), "http://h:1/v1/models");
        // A path prefix is kept, and the query and fragment are dropped.
        assert_eq!(
            derive("https://gateway.test/models/embed/v1"),
            "https://gateway.test/models/embed/v1/models"
        );
        assert_eq!(
            derive("https://gateway.test/team/rerank?key=secret#frag"),
            "https://gateway.test/team/v1/models"
        );
        assert_eq!(
            derive("https://gateway.test/team/v1/embeddings"),
            "https://gateway.test/team/v1/models"
        );
    }

    #[test]
    fn data_dir_length_counts_characters_not_bytes() {
        // 100 two-byte characters are 200 bytes but 100 characters.
        let multibyte: std::path::PathBuf = "é".repeat(100).into();
        let component = data_dir_length_component(&multibyte, true, 150);
        assert!(component.ready);
        assert_eq!(
            component.detail.as_deref(),
            Some("data_dir is 100 characters")
        );
    }
}
