//! One shape for a model service: where it is, which model it serves, and
//! which environment variable holds its API key. The embedder, the reranker
//! and the System One model all use it, so each can be a local server, a
//! hosted provider or a server on another machine.
//!
//! The key itself is never stored: a configuration names the environment
//! variable, [`Endpoint::api_key`] reads it when a request is made, and
//! [`ApiKey`] has no `Display` and a redacted `Debug`.

use crate::config::Config;
use anyhow::{Context, Result, bail, ensure};
use std::net::IpAddr;

/// A model service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub url: String,
    pub model: String,
    /// The NAME of the environment variable holding the API key (sent as a
    /// bearer token); `None` sends no credential.
    pub api_key_env: Option<String>,
    /// Whether a non-loopback host is accepted. Only the System One endpoint
    /// enforces it ([`Endpoint::require_local_unless_allowed`]); the embedder
    /// and reranker, which have always been allowed to run on another
    /// machine, are built with it `true`.
    pub allow_remote: bool,
}

/// An API key read from the environment. Reading it back is explicit
/// ([`ApiKey::expose`]); formatting it never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ApiKey([redacted])")
    }
}

impl Endpoint {
    /// The host of the URL, without brackets; `None` when the URL has none.
    fn host(&self) -> Option<String> {
        let url = reqwest::Url::parse(&self.url).ok()?;
        Some(url.host_str()?.trim_matches(['[', ']']).to_owned())
    }

    /// Whether the URL names a host other than this machine: anything but
    /// `localhost`, `*.localhost` and a loopback IP address. A URL that does
    /// not parse counts as remote.
    pub fn is_remote(&self) -> bool {
        let Some(host) = self.host() else {
            return true;
        };
        let host = host.to_ascii_lowercase();
        let loopback = host == "localhost"
            || host.ends_with(".localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
        !loopback
    }

    /// Refuses a non-loopback host unless `allow_remote` is set, naming the
    /// configuration key (`section` is the table, e.g. `system_one`).
    pub fn require_local_unless_allowed(&self, section: &str) -> Result<()> {
        ensure!(
            self.allow_remote || !self.is_remote(),
            "{section}.url names the non-loopback host `{}`; set {section}.allow_remote = true to send requests there",
            self.host().unwrap_or_default()
        );
        Ok(())
    }

    /// The URL with `suffix` after its path (trailing slashes of the path
    /// dropped first); the query stays where it is.
    pub fn join(&self, suffix: &str) -> Result<reqwest::Url> {
        let mut url = reqwest::Url::parse(&self.url).context("invalid service URL")?;
        ensure!(!url.cannot_be_a_base(), "service URL cannot be a base URL");
        let path = format!("{}/{}", url.path().trim_end_matches('/'), suffix);
        url.set_path(&path);
        Ok(url)
    }

    /// Reads the API key from the environment variable `api_key_env` names.
    pub fn api_key(&self) -> Result<Option<ApiKey>> {
        self.api_key_from(|name| std::env::var(name).ok())
    }

    /// [`Endpoint::api_key`] with the environment passed in, so tests need
    /// not touch the process environment. No error shows the value.
    pub fn api_key_from(&self, lookup: impl Fn(&str) -> Option<String>) -> Result<Option<ApiKey>> {
        let Some(name) = &self.api_key_env else {
            return Ok(None);
        };
        let Some(value) = lookup(name).filter(|value| !value.trim().is_empty()) else {
            bail!("environment variable `{name}` (api_key_env) is not set or is empty");
        };
        let value = value.trim().to_owned();
        ensure!(
            value.chars().all(|c| !c.is_control()),
            "environment variable `{name}` (api_key_env) does not hold a valid HTTP header value"
        );
        Ok(Some(ApiKey(value)))
    }
}

/// Checks that `name` could be an environment variable name. Rejecting
/// anything else, lower case included, keeps a pasted key from being
/// accepted as a name and then echoed in an error. The message never shows
/// `name`.
pub fn validate_api_key_env(key: &str, name: Option<&str>) -> Result<()> {
    if let Some(name) = name {
        ensure!(
            !name.is_empty()
                && name.starts_with(|c: char| c.is_ascii_uppercase() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
            "{key} must be the NAME of an environment variable (upper-case letters, digits and '_'), never the key itself"
        );
    }
    Ok(())
}

impl Config {
    /// The embedder as an [`Endpoint`], from `embedding_url`,
    /// `embedding_model` and `embedding_api_key_env`.
    pub fn embedder_endpoint(&self) -> Endpoint {
        Endpoint {
            url: self.embedding_url.clone(),
            model: self.embedding_model.clone(),
            api_key_env: self.embedding_api_key_env.clone(),
            allow_remote: true,
        }
    }

    /// The reranker as an [`Endpoint`], from `reranker_url`,
    /// `reranker_model` and `reranker_api_key_env`.
    pub fn reranker_endpoint(&self) -> Endpoint {
        Endpoint {
            url: self.reranker_url.clone(),
            model: self.reranker_model.clone(),
            api_key_env: self.reranker_api_key_env.clone(),
            allow_remote: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(url: &str, allow_remote: bool) -> Endpoint {
        Endpoint {
            url: url.into(),
            model: "m".into(),
            api_key_env: None,
            allow_remote,
        }
    }

    #[test]
    fn is_remote_tells_loopback_from_other_hosts() {
        for (url, remote) in [
            ("http://localhost:8080/v1", false),
            ("http://LocalHost/v1", false),
            ("http://127.0.0.1:8080/v1", false),
            ("http://127.1.2.3/v1", false),
            ("http://[::1]:8080/v1", false),
            ("http://llm.localhost/v1", false),
            ("http://192.168.1.20:8080/v1", true),
            ("http://gpu-box:8080/v1", true),
            ("https://api.example.com/v1", true),
            ("not a url", true),
        ] {
            assert_eq!(endpoint(url, false).is_remote(), remote, "{url}");
        }
    }

    #[test]
    fn a_remote_host_is_refused_without_allow_remote_and_names_the_key() {
        let remote = "https://api.example.com/v1";
        let error = endpoint(remote, false)
            .require_local_unless_allowed("system_one")
            .unwrap_err()
            .to_string();
        assert!(error.contains("system_one.allow_remote"), "{error}");
        assert!(error.contains("api.example.com"), "{error}");
        endpoint(remote, true)
            .require_local_unless_allowed("system_one")
            .unwrap();
        endpoint("http://127.0.0.1:8080/v1", false)
            .require_local_unless_allowed("system_one")
            .unwrap();
    }

    #[test]
    fn legacy_embedder_and_reranker_keys_map_onto_endpoints() {
        let config = Config {
            embedding_url: "http://gpu-box:8766/v1".into(),
            embedding_model: "e".into(),
            reranker_url: "http://localhost:8767/rerank".into(),
            reranker_model: "r".into(),
            reranker_api_key_env: Some("RERANK_KEY".into()),
            ..Config::default()
        };
        assert_eq!(
            config.embedder_endpoint(),
            Endpoint {
                url: "http://gpu-box:8766/v1".into(),
                model: "e".into(),
                api_key_env: None,
                allow_remote: true,
            }
        );
        assert_eq!(
            config.reranker_endpoint(),
            Endpoint {
                url: "http://localhost:8767/rerank".into(),
                model: "r".into(),
                api_key_env: Some("RERANK_KEY".into()),
                allow_remote: true,
            }
        );
        assert!(config.embedder_endpoint().is_remote());
        assert!(!config.reranker_endpoint().is_remote());
    }

    #[test]
    fn the_api_key_comes_from_the_named_variable_and_never_shows() {
        const SECRET: &str = "sk-s3cret-value";
        let with_key = Endpoint {
            api_key_env: Some("MY_KEY".into()),
            ..endpoint("https://api.example.com/v1", true)
        };
        let key = with_key
            .api_key_from(|name| (name == "MY_KEY").then(|| format!(" {SECRET}\n")))
            .unwrap()
            .unwrap();
        assert_eq!(key.expose(), SECRET);
        for shown in [format!("{key:?}"), format!("{with_key:?}")] {
            assert!(!shown.contains(SECRET), "{shown}");
        }

        for (value, why) in [
            (None, "unset"),
            (Some("  ".to_owned()), "empty"),
            (Some(format!("{SECRET}\r\nX-Evil: 1")), "control character"),
        ] {
            let error = format!(
                "{:#}",
                with_key.api_key_from(|_| value.clone()).unwrap_err()
            );
            assert!(error.contains("MY_KEY"), "{why}: {error}");
            assert!(!error.contains(SECRET), "{why}: {error}");
        }
        assert_eq!(
            endpoint("http://x", true).api_key_from(|_| None).unwrap(),
            None
        );
    }

    #[test]
    fn api_key_env_must_be_a_variable_name_and_never_echoes_its_value() {
        for good in ["OPENAI_API_KEY", "_K1", "K"] {
            validate_api_key_env("k", Some(good)).unwrap();
        }
        validate_api_key_env("k", None).unwrap();
        for bad in ["", "sk-abc123", "lower_case", "1ABC", "A B", "KEY=1"] {
            let error = validate_api_key_env("embedding_api_key_env", Some(bad))
                .unwrap_err()
                .to_string();
            assert!(error.contains("embedding_api_key_env"), "{error}");
            assert!(bad.is_empty() || !error.contains(bad), "{error}");
        }
    }

    #[test]
    fn join_appends_to_the_path_and_keeps_the_query() {
        let url = endpoint("http://h:1/v1//?a=b", true)
            .join("chat/completions")
            .unwrap();
        assert_eq!(url.as_str(), "http://h:1/v1/chat/completions?a=b");
    }
}
