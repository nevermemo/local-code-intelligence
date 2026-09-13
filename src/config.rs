use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub embedding_url: String,
    pub embedding_model: String,
    pub reranker_url: String,
    pub reranker_model: String,
    pub data_dir: PathBuf,
    pub default_top_k: usize,
    pub embedding_batch_size: usize,
    pub embedding_timeout_seconds: u64,
    pub reranker_timeout_seconds: u64,
    pub readiness_timeout_seconds: u64,
    pub ripgrep_path: String,
    pub semantic_candidate_count: usize,
    pub lexical_candidate_count: usize,
    pub rerank_candidate_count: usize,
    pub rrf_k: f32,
    pub watch_poll_milliseconds: u64,
    pub watch_debounce_milliseconds: u64,
    pub rust_analyzer_path: String,
    pub lsp_timeout_seconds: u64,
    pub lsp_candidate_count: usize,
}

impl Default for Config {
    fn default() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .unwrap_or_default();
        Self {
            embedding_url: "http://localhost:8766/v1".into(),
            embedding_model: "qwen3-embedding-4b".into(),
            reranker_url: "http://localhost:8767/rerank".into(),
            reranker_model: "qwen3-reranker-4b".into(),
            data_dir: base.join("local-code-intelligence"),
            default_top_k: 8,
            embedding_batch_size: 8,
            embedding_timeout_seconds: 120,
            reranker_timeout_seconds: 120,
            readiness_timeout_seconds: 5,
            ripgrep_path: "rg".into(),
            semantic_candidate_count: 40,
            lexical_candidate_count: 40,
            rerank_candidate_count: 24,
            rrf_k: 60.0,
            watch_poll_milliseconds: 2000,
            watch_debounce_milliseconds: 750,
            rust_analyzer_path: "rust-analyzer".into(),
            lsp_timeout_seconds: 60,
            lsp_candidate_count: 40,
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config: Self = match path {
            Some(p) => toml::from_str(&std::fs::read_to_string(p).context("read configuration")?)?,
            None => Self::default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.data_dir.is_absolute(),
            "data_dir must be absolute; set it explicitly if LOCALAPPDATA is unavailable"
        );
        ensure!(
            (1..=200).contains(&self.semantic_candidate_count)
                && (1..=200).contains(&self.lexical_candidate_count)
                && (1..=200).contains(&self.rerank_candidate_count)
                && (1..=200).contains(&self.lsp_candidate_count),
            "candidate counts must be 1..=200"
        );
        ensure!(
            self.rrf_k.is_finite() && self.rrf_k > 0.0,
            "rrf_k must be positive"
        );
        ensure!(
            self.watch_poll_milliseconds > 0 && self.watch_debounce_milliseconds > 0,
            "watch intervals must be positive"
        );
        ensure!(
            !self.ripgrep_path.trim().is_empty(),
            "ripgrep_path must be nonempty"
        );
        ensure!(
            !self.rust_analyzer_path.trim().is_empty() && self.lsp_timeout_seconds > 0,
            "rust-analyzer path must be nonempty and LSP timeout must be positive"
        );
        ensure!(
            (1..=40).contains(&self.default_top_k),
            "default_top_k must be 1..=40"
        );
        ensure!(
            (1..=64).contains(&self.embedding_batch_size),
            "embedding_batch_size must be 1..=64"
        );
        ensure!(
            self.embedding_timeout_seconds > 0 && self.reranker_timeout_seconds > 0,
            "timeouts must be positive"
        );
        ensure!(
            (1..=30).contains(&self.readiness_timeout_seconds),
            "readiness_timeout_seconds must be 1..=30"
        );
        for value in [&self.embedding_url, &self.reranker_url] {
            let url = reqwest::Url::parse(value)?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "model URL must use HTTP(S)"
            );
        }
        ensure!(
            !self.embedding_model.is_empty() && !self.reranker_model.is_empty(),
            "model names must be nonempty"
        );
        Ok(())
    }

    pub fn embedding_identity(&self) -> String {
        format!(
            "{}|{}|{}",
            self.embedding_url.trim_end_matches('/'),
            self.embedding_model,
            crate::chunk::DOCUMENT_FORMAT_VERSION
        )
    }
}
