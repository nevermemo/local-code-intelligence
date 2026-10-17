//! Completion providers for System One, the small model that steers a
//! traversal. A provider takes a prompt and an optional constraint (a GBNF
//! grammar or a JSON schema) and returns the text, the top log-probabilities
//! when the server offers them, and a record of who answered: provider name,
//! model id and capability profile.
//!
//! Two providers exist:
//!
//! * `openai_compat` posts to `<url>/chat/completions` or
//!   `<url>/completions` of any server that speaks that API (llama.cpp,
//!   vLLM, LM Studio, a hosted service), local, on another machine, or
//!   remote. A non-loopback host is refused unless `allow_remote` is set.
//! * `mock` replays recorded request/response pairs from a JSON file, so
//!   tests never touch a network.
//!
//! TODO(hosted_typed): typed providers (a client for one hosted service's own
//! typed API, such as the Jev pilot client) are not implemented. They would
//! be a third [`Provider`] that answers the typed query itself instead of
//! constrained text.

use crate::{
    endpoint::{ApiKey, Endpoint, validate_api_key_env},
    models::{MAX_RESPONSE_BODY_BYTES, check_status, read_limited_body},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

/// Which constraint a server enforces while it decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintSupport {
    /// A GBNF grammar (`grammar`, llama.cpp).
    Gbnf,
    /// A JSON schema (`response_format`, OpenAI, vLLM, LM Studio).
    JsonSchema,
    /// Neither: only an unconstrained request is accepted.
    None,
}

/// What a provider declares it can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Capabilities {
    pub constraint: ConstraintSupport,
    /// The server returns the top log-probabilities of each generated token.
    pub logprobs: bool,
    /// The server takes several prompts in one request. Declared only;
    /// [`Provider::complete_constrained`] sends one.
    pub batch: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            constraint: ConstraintSupport::JsonSchema,
            logprobs: false,
            batch: false,
        }
    }
}

/// The constraint of one request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Constraint {
    Gbnf(String),
    JsonSchema(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub prompt: String,
    pub constraint: Option<Constraint>,
    pub max_tokens: u32,
    /// How many alternatives per generated token to return, when the
    /// provider offers log-probabilities; ignored otherwise.
    pub top_logprobs: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenLogprob {
    pub token: String,
    pub logprob: f64,
}

/// One completion and the record of who produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub text: String,
    /// Per generated token, its alternatives best first; `None` when the
    /// provider offers none or none were asked for.
    pub top_logprobs: Option<TopLogprobs>,
    pub provider: &'static str,
    pub model_id: String,
    pub capabilities: Capabilities,
    pub latency: Duration,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    /// One completion. A constraint the provider's profile does not support
    /// is an error, not a silent downgrade.
    async fn complete_constrained(&self, request: &CompletionRequest) -> Result<Completion>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    OpenaiCompat,
    Mock,
}

/// Which OpenAI endpoint an `openai_compat` server is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiKind {
    /// `/chat/completions`.
    Chat,
    /// `/completions`.
    Completion,
}

/// `[system_one]`. Absent: System One is off.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SystemOneConfig {
    pub provider: ProviderKind,
    /// Base URL of the server, up to and including `/v1`.
    pub url: String,
    pub model: String,
    /// The NAME of the environment variable holding the API key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    /// Accept a non-loopback `url`.
    pub allow_remote: bool,
    pub timeout_ms: u64,
    pub api: ApiKind,
    pub capabilities: Capabilities,
    /// The recorded request/response pairs a `mock` provider replays.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_file: Option<String>,
}

impl Default for SystemOneConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::OpenaiCompat,
            url: "http://127.0.0.1:8080/v1".into(),
            model: "qwen3-4b".into(),
            api_key_env: None,
            allow_remote: false,
            timeout_ms: 30_000,
            api: ApiKind::Chat,
            capabilities: Capabilities::default(),
            replay_file: None,
        }
    }
}

impl SystemOneConfig {
    pub const TIMEOUT_MS_RANGE: std::ops::RangeInclusive<u64> = 100..=600_000;

    pub fn endpoint(&self) -> Endpoint {
        Endpoint {
            url: self.url.clone(),
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
            allow_remote: self.allow_remote,
        }
    }

    pub(crate) fn trim(&mut self) {
        self.url = self.url.trim().to_owned();
        self.model = self.model.trim().to_owned();
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            Self::TIMEOUT_MS_RANGE.contains(&self.timeout_ms),
            "system_one.timeout_ms must be {}..={} (got {})",
            Self::TIMEOUT_MS_RANGE.start(),
            Self::TIMEOUT_MS_RANGE.end(),
            self.timeout_ms
        );
        validate_api_key_env("system_one.api_key_env", self.api_key_env.as_deref())?;
        match self.provider {
            ProviderKind::Mock => ensure!(
                self.replay_file
                    .as_deref()
                    .is_some_and(|f| !f.trim().is_empty()),
                "system_one.replay_file must name the recorded pairs when provider = \"mock\""
            ),
            ProviderKind::OpenaiCompat => {
                ensure!(
                    self.replay_file.is_none(),
                    "system_one.replay_file applies only to provider = \"mock\""
                );
                ensure!(!self.model.is_empty(), "system_one.model must be nonempty");
                let url = reqwest::Url::parse(&self.url).context("invalid system_one.url")?;
                ensure!(
                    matches!(url.scheme(), "http" | "https"),
                    "system_one.url must use HTTP(S)"
                );
                ensure!(
                    url.port_or_known_default() != Some(crate::config::GENERATION_MODEL_PORT),
                    "`system_one.url` uses port {}, which is reserved for the generation model; LCI never contacts it; choose another port",
                    crate::config::GENERATION_MODEL_PORT
                );
                self.endpoint().require_local_unless_allowed("system_one")?;
            }
        }
        Ok(())
    }

    /// The provider this section selects. Validates first.
    pub fn build(&self) -> Result<Box<dyn Provider>> {
        self.validate()?;
        Ok(match self.provider {
            ProviderKind::OpenaiCompat => Box::new(OpenAiCompat::new(self)?),
            ProviderKind::Mock => Box::new(MockProvider::from_file(
                Path::new(self.replay_file.as_deref().unwrap_or_default()),
                self.capabilities,
            )?),
        })
    }
}

// ---------------------------------------------------------------- openai_compat

/// The JSON body of one request. Pure, so the shape each kind of server needs
/// is testable without a network.
pub fn request_body(
    api: ApiKind,
    model: &str,
    capabilities: Capabilities,
    request: &CompletionRequest,
) -> Result<Value> {
    let mut body = match api {
        ApiKind::Chat => json!({
            "model": model,
            "messages": [{"role": "user", "content": request.prompt}],
        }),
        ApiKind::Completion => json!({"model": model, "prompt": request.prompt}),
    };
    body["max_tokens"] = request.max_tokens.into();
    body["temperature"] = 0.into();
    body["stream"] = false.into();
    match (&request.constraint, capabilities.constraint) {
        (None, _) => {}
        (Some(Constraint::Gbnf(grammar)), ConstraintSupport::Gbnf) => {
            body["grammar"] = grammar.clone().into();
        }
        (Some(Constraint::JsonSchema(schema)), ConstraintSupport::JsonSchema) => {
            body["response_format"] = json!({
                "type": "json_schema",
                "json_schema": {"name": "system_one", "strict": true, "schema": schema},
            });
        }
        (Some(constraint), supported) => bail!(
            "the request carries a {} constraint but the provider's constraint support is {}",
            match constraint {
                Constraint::Gbnf(_) => "gbnf",
                Constraint::JsonSchema(_) => "json_schema",
            },
            serde_json::to_value(supported)?
        ),
    }
    if let (true, Some(top)) = (capabilities.logprobs, request.top_logprobs) {
        match api {
            ApiKind::Chat => {
                body["logprobs"] = true.into();
                body["top_logprobs"] = top.into();
            }
            ApiKind::Completion => body["logprobs"] = top.into(),
        }
    }
    Ok(body)
}

#[derive(Deserialize)]
struct Reply {
    model: Option<String>,
    choices: Vec<Choice>,
}
#[derive(Deserialize)]
struct Choice {
    message: Option<Message>,
    text: Option<String>,
    logprobs: Option<ChoiceLogprobs>,
}
#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}
#[derive(Deserialize)]
struct ChoiceLogprobs {
    /// Chat: one entry per generated token.
    content: Option<Vec<ChatTokenLogprobs>>,
    /// Completion: one token-to-logprob map per generated token.
    top_logprobs: Option<Vec<std::collections::BTreeMap<String, f64>>>,
}
#[derive(Deserialize)]
struct ChatTokenLogprobs {
    top_logprobs: Vec<TokenLogprob>,
}

/// What a response body holds.
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub text: String,
    pub top_logprobs: Option<TopLogprobs>,
    /// The model id the server reported, if it did.
    pub model_id: Option<String>,
}

/// Per generated token, its alternatives best first.
pub type TopLogprobs = Vec<Vec<TokenLogprob>>;

/// Parses a response body. Pure.
pub fn parse_response(bytes: &[u8]) -> Result<Parsed> {
    let reply: Reply = serde_json::from_slice(bytes).context("invalid system_one response")?;
    let choice = reply
        .choices
        .into_iter()
        .next()
        .context("system_one response has no choices")?;
    let text = choice
        .message
        .and_then(|message| message.content)
        .or(choice.text)
        .context("system_one response has no text")?;
    let top_logprobs = choice.logprobs.and_then(|logprobs| {
        if let Some(tokens) = logprobs.content {
            Some(tokens.into_iter().map(|t| t.top_logprobs).collect())
        } else {
            logprobs.top_logprobs.map(|tokens| {
                tokens
                    .into_iter()
                    .map(|alternatives| {
                        let mut list: Vec<_> = alternatives
                            .into_iter()
                            .map(|(token, logprob)| TokenLogprob { token, logprob })
                            .collect();
                        list.sort_by(|a, b| b.logprob.total_cmp(&a.logprob));
                        list
                    })
                    .collect()
            })
        }
    });
    Ok(Parsed {
        text,
        top_logprobs,
        model_id: reply.model,
    })
}

pub struct OpenAiCompat {
    client: reqwest::Client,
    endpoint: Endpoint,
    api: ApiKind,
    capabilities: Capabilities,
    timeout: Duration,
}

impl OpenAiCompat {
    pub fn new(config: &SystemOneConfig) -> Result<Self> {
        let endpoint = config.endpoint();
        endpoint.require_local_unless_allowed("system_one")?;
        if let Some(warning) = endpoint.cleartext_key_warning("system_one.url") {
            tracing::warn!("{warning}");
        }
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                // A redirect would carry the API key to another host.
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            endpoint,
            api: config.api,
            capabilities: config.capabilities,
            timeout: Duration::from_millis(config.timeout_ms),
        })
    }
}

#[async_trait]
impl Provider for OpenAiCompat {
    fn name(&self) -> &'static str {
        "openai_compat"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// One attempt, no retry: the steering ladder falls back to a cheaper
    /// rung when System One does not answer.
    async fn complete_constrained(&self, request: &CompletionRequest) -> Result<Completion> {
        let body = request_body(self.api, &self.endpoint.model, self.capabilities, request)?;
        let key: Option<ApiKey> = self.endpoint.api_key()?;
        let url = self.endpoint.join(match self.api {
            ApiKind::Chat => "chat/completions",
            ApiKind::Completion => "completions",
        })?;
        let started = Instant::now();
        let mut builder = self
            .client
            .post(url.clone())
            .timeout(self.timeout)
            .json(&body);
        if let Some(key) = &key {
            builder = builder.bearer_auth(key.expose());
        }
        let response = builder
            .send()
            .await
            .map_err(reqwest::Error::without_url)
            .context("system_one request failed")?;
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
        let parsed = parse_response(&bounded.bytes)?;
        Ok(Completion {
            text: parsed.text,
            top_logprobs: parsed.top_logprobs,
            provider: self.name(),
            model_id: parsed
                .model_id
                .unwrap_or_else(|| self.endpoint.model.clone()),
            capabilities: self.capabilities,
            latency: started.elapsed(),
        })
    }
}

// ------------------------------------------------------------------------ mock

#[derive(Deserialize)]
struct Recorded {
    request: CompletionRequest,
    response: RecordedResponse,
}
#[derive(Deserialize)]
struct RecordedResponse {
    text: String,
    top_logprobs: Option<TopLogprobs>,
    model_id: Option<String>,
}

/// Replays recorded pairs: a request equal to a recorded one gets its
/// response, any other request is an error.
pub struct MockProvider {
    pairs: Vec<Recorded>,
    capabilities: Capabilities,
}

impl MockProvider {
    /// `json` is an array of `{"request": ..., "response": {"text": ...,
    /// "top_logprobs"?: ..., "model_id"?: ...}}`.
    pub fn from_json(json: &str, capabilities: Capabilities) -> Result<Self> {
        Ok(Self {
            pairs: serde_json::from_str(json).context("invalid mock replay file")?,
            capabilities,
        })
    }

    pub fn from_file(path: &Path, capabilities: Capabilities) -> Result<Self> {
        Self::from_json(
            &std::fs::read_to_string(path).context("read mock replay file")?,
            capabilities,
        )
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn complete_constrained(&self, request: &CompletionRequest) -> Result<Completion> {
        let started = Instant::now();
        let Some(pair) = self.pairs.iter().find(|pair| pair.request == *request) else {
            bail!(
                "mock provider has no recorded response for this request ({} recorded; prompt starts {:?})",
                self.pairs.len(),
                request.prompt.chars().take(60).collect::<String>()
            );
        };
        Ok(Completion {
            text: pair.response.text.clone(),
            top_logprobs: pair.response.top_logprobs.clone(),
            provider: self.name(),
            model_id: pair
                .response
                .model_id
                .clone()
                .unwrap_or_else(|| "mock".into()),
            capabilities: self.capabilities,
            latency: started.elapsed(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::is_connect_error;

    fn request(constraint: Option<Constraint>) -> CompletionRequest {
        CompletionRequest {
            prompt: "pick".into(),
            constraint,
            max_tokens: 16,
            top_logprobs: Some(3),
        }
    }

    fn caps(constraint: ConstraintSupport, logprobs: bool) -> Capabilities {
        Capabilities {
            constraint,
            logprobs,
            batch: false,
        }
    }

    #[test]
    fn the_request_body_fits_each_kind_of_server() {
        let schema = json!({"type": "object"});
        let gbnf = Some(Constraint::Gbnf("root ::= \"a\"".into()));
        let json_schema = Some(Constraint::JsonSchema(schema.clone()));
        // (api, support, constraint, logprobs, expected extra fields)
        let cases = [
            (
                ApiKind::Chat,
                ConstraintSupport::Gbnf,
                gbnf.clone(),
                false,
                json!({"grammar": "root ::= \"a\""}),
            ),
            (
                ApiKind::Chat,
                ConstraintSupport::JsonSchema,
                json_schema.clone(),
                true,
                json!({
                    "response_format": {"type": "json_schema", "json_schema": {
                        "name": "system_one", "strict": true, "schema": schema}},
                    "logprobs": true, "top_logprobs": 3,
                }),
            ),
            (
                ApiKind::Completion,
                ConstraintSupport::None,
                None,
                true,
                json!({"logprobs": 3}),
            ),
            (
                ApiKind::Completion,
                ConstraintSupport::Gbnf,
                gbnf.clone(),
                false,
                json!({"grammar": "root ::= \"a\""}),
            ),
        ];
        for (api, support, constraint, logprobs, extra) in cases {
            let body =
                request_body(api, "m", caps(support, logprobs), &request(constraint)).unwrap();
            let mut expected = match api {
                ApiKind::Chat => {
                    json!({"model": "m", "messages": [{"role": "user", "content": "pick"}]})
                }
                ApiKind::Completion => json!({"model": "m", "prompt": "pick"}),
            };
            expected["max_tokens"] = 16.into();
            expected["temperature"] = 0.into();
            expected["stream"] = false.into();
            for (key, value) in extra.as_object().unwrap() {
                expected[key] = value.clone();
            }
            assert_eq!(body, expected, "{api:?} {support:?}");
        }
        // A constraint the server does not enforce is refused, not dropped.
        for (support, constraint) in [
            (ConstraintSupport::JsonSchema, gbnf),
            (ConstraintSupport::Gbnf, json_schema.clone()),
            (ConstraintSupport::None, json_schema),
        ] {
            let error = request_body(
                ApiKind::Chat,
                "m",
                caps(support, false),
                &request(constraint),
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("constraint"), "{error}");
        }
    }

    #[test]
    fn responses_parse_for_both_apis() {
        let chat = br#"{"model":"served-as","choices":[{"message":{"content":"{\"a\":1}"},
            "logprobs":{"content":[{"top_logprobs":[{"token":"{","logprob":-0.1},{"token":"[","logprob":-2.0}]}]}}]}"#;
        let parsed = parse_response(chat).unwrap();
        assert_eq!(parsed.text, "{\"a\":1}");
        assert_eq!(parsed.model_id.as_deref(), Some("served-as"));
        assert_eq!(parsed.top_logprobs.unwrap()[0][1].token, "[");

        let completion =
            br#"{"choices":[{"text":"x","logprobs":{"top_logprobs":[{"a":-3.0,"b":-0.5}]}}]}"#;
        let parsed = parse_response(completion).unwrap();
        assert_eq!((parsed.text.as_str(), parsed.model_id), ("x", None));
        let first = &parsed.top_logprobs.unwrap()[0];
        assert_eq!(first[0].token, "b", "best first");

        assert!(parse_response(br#"{"choices":[]}"#).is_err());
    }

    #[tokio::test]
    async fn the_mock_replays_recorded_pairs_and_records_who_answered() {
        let known = request(Some(Constraint::Gbnf("g".into())));
        let json = serde_json::to_string(&json!([{
            "request": known,
            "response": {"text": "yes", "model_id": "recorded-model"},
        }]))
        .unwrap();
        let capabilities = caps(ConstraintSupport::Gbnf, true);
        let mock = MockProvider::from_json(&json, capabilities).unwrap();
        let done = mock.complete_constrained(&known).await.unwrap();
        assert_eq!(
            (done.text.as_str(), done.provider, done.model_id.as_str()),
            ("yes", "mock", "recorded-model")
        );
        assert_eq!(done.capabilities, capabilities);
        let error = mock
            .complete_constrained(&request(None))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("no recorded response"), "{error}");
    }

    #[test]
    fn config_selects_a_provider_and_checks_remote_hosts() {
        let remote = SystemOneConfig {
            url: "https://api.example.com/v1".into(),
            ..SystemOneConfig::default()
        };
        let error = remote.validate().unwrap_err().to_string();
        assert!(error.contains("system_one.allow_remote"), "{error}");
        SystemOneConfig {
            allow_remote: true,
            api_key_env: Some("EXAMPLE_API_KEY".into()),
            ..remote.clone()
        }
        .validate()
        .unwrap();
        SystemOneConfig::default().validate().unwrap();

        let invalid = [
            SystemOneConfig {
                timeout_ms: 5,
                ..SystemOneConfig::default()
            },
            SystemOneConfig {
                provider: ProviderKind::Mock,
                ..SystemOneConfig::default()
            },
            SystemOneConfig {
                api_key_env: Some("sk-abc".into()),
                ..SystemOneConfig::default()
            },
            SystemOneConfig {
                url: "http://127.0.0.1:8765/v1".into(),
                ..SystemOneConfig::default()
            },
        ];
        for config in invalid {
            assert!(config.validate().is_err(), "{config:?}");
        }
        let parsed: SystemOneConfig =
            toml::from_str("provider = \"mock\"\nreplay_file = \"r.json\"").unwrap();
        assert_eq!(parsed.provider, ProviderKind::Mock);
        assert!(toml::from_str::<SystemOneConfig>("apikey = \"x\"").is_err());
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_a_quick_connect_error() {
        // Refused at once on every OS (the shared helper explains how, and why
        // macOS needs a different port than a bound-but-not-listening one).
        let closed = crate::models::tests::closed_local_address();
        let config = SystemOneConfig {
            url: format!("http://{closed}/v1"),
            ..SystemOneConfig::default()
        };
        let provider = config.build().unwrap();
        let started = Instant::now();
        let error = provider
            .complete_constrained(&request(None))
            .await
            .unwrap_err();
        assert!(is_connect_error(&error), "{error:#}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "{:?}",
            started.elapsed()
        );
        drop(closed);
    }
}
