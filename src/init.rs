//! `lci init`: write the local overlay from facts found on this machine.
//!
//! The overlay (see [`crate::config::layers`]) holds the machine-specific
//! settings: where indexes live, where the tools are, which port and model
//! services are in play. `init` writes only keys it established:
//!
//! - `data_dir`, as the platform default made explicit, when no higher layer
//!   sets it;
//! - the absolute path of `rg`, `rust-analyzer`, `csharp-ls`,
//!   `typescript-language-server`, `pyright-langserver`, `gopls` and `clangd`
//!   found on `PATH`, when no higher layer sets that key.
//!
//! Everything else it learns -- a model service that answers or does not,
//! whether the configured model is listed, whether the port is free, a tool
//! that is missing -- goes into the file as a comment. It never writes a model
//! name or a port, never picks another port, never writes the main
//! configuration, and never replaces an existing overlay without `--force`.
//!
//! # What `init` contacts
//!
//! Every request goes through `Prober`, which asks [`probe_refusal`] first:
//! a URL on port 8765 (the generation model, which LCI never contacts) is
//! refused for any host spelling, and so is any host that is not loopback
//! (`127.0.0.1`, `localhost`, `::1`). No flag overrides the refusal. The client
//! follows no redirects and uses no proxy, so a loopback service cannot send
//! `init` elsewhere. The port check binds `127.0.0.1:<port>` and releases it;
//! when the bind fails it may ask `http://127.0.0.1:<port>/health` whether an
//! LCI answers there. A port in 8765-8767 is not checked at all. `--no-probe`
//! sends no request.

use crate::{
    app::readiness::{ModelIds, fetch_model_ids, probe_endpoint_reach},
    config::{
        Config, GENERATION_MODEL_PORT, RESERVED_PORTS,
        layers::{
            self, LocalOverrides, OverlayNote, Provenance, ResolveSpec, Resolved, Source,
            render_overlay,
        },
    },
    lexical,
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Map, Value, json};
use std::{
    ffi::OsStr,
    io::{ErrorKind, Write as _},
    net::{IpAddr, Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    time::Duration,
};

/// The advice that follows the overlay's path when it exists and `--force`
/// was not given.
const EXISTS_ADVICE: &str = "already exists; edit it, or pass --force to replace it";

/// Why `init` stops at an existing overlay.
fn exists_message(path: &Path) -> String {
    format!("{} {EXISTS_ADVICE}", path.display())
}

/// What `init` was asked to do.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Replace an existing overlay.
    pub force: bool,
    /// Detect and print; write nothing.
    pub dry_run: bool,
    /// Send no request (no model-service probe, no `/health` check).
    pub no_probe: bool,
}

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The overlay was written.
    Written,
    /// `--dry-run`: it would be written as `contents`.
    DryRun,
    /// An overlay is already there and `--force` was not given; nothing was
    /// detected or written.
    Exists,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::DryRun => "dry_run",
            Self::Exists => "exists",
        }
    }
}

/// The result of [`run`].
#[derive(Debug)]
pub struct Outcome {
    pub status: Status,
    /// The overlay file this run targets.
    pub path: PathBuf,
    /// The overlay text (empty for [`Status::Exists`]).
    pub contents: String,
    /// The `--json` report.
    pub report: Value,
    /// Lines for stderr: refused probes and a configuration that stays
    /// invalid.
    pub warnings: Vec<String>,
}

impl Outcome {
    /// The text report: what happened to the file, then the file.
    pub fn render_text(&self) -> String {
        let headline = match self.status {
            Status::Written => format!("wrote {}", self.path.display()),
            Status::DryRun => format!("would write {} (dry run)", self.path.display()),
            Status::Exists => exists_message(&self.path),
        };
        if self.contents.is_empty() {
            format!("{headline}\n")
        } else {
            format!("{headline}\n\n{}", self.contents)
        }
    }
}

/// Why a URL may not be contacted by `init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeRefusal {
    /// Port 8765, whatever the host.
    GenerationModelPort,
    /// A host other than `127.0.0.1`, `localhost` or `::1`.
    NotLoopback,
    /// A scheme other than `http` or `https`.
    NotHttp,
    /// Not a URL.
    Invalid,
}

impl ProbeRefusal {
    pub fn message(self) -> &'static str {
        match self {
            Self::GenerationModelPort => {
                "port 8765 is the generation model; LCI never contacts it (AGENTS.md)"
            }
            Self::NotLoopback => "init contacts loopback hosts only (127.0.0.1, localhost, ::1)",
            Self::NotHttp => "not an http(s) URL",
            Self::Invalid => "not a valid URL",
        }
    }
}

/// Whether `init` must not contact `url`, and why. Judges the string only:
/// nothing is resolved or opened. Port 8765 comes first, so a URL on it is
/// refused with that rule whatever else is wrong with it.
pub fn probe_refusal(url: &str) -> Option<ProbeRefusal> {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return Some(ProbeRefusal::Invalid);
    };
    // The effective port: an explicit one, else the scheme's default.
    if parsed.port_or_known_default() == Some(GENERATION_MODEL_PORT) {
        return Some(ProbeRefusal::GenerationModelPort);
    }
    if !matches!(parsed.scheme(), "http" | "https") {
        return Some(ProbeRefusal::NotHttp);
    }
    let loopback = parsed.host_str().is_some_and(|host| {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    (!loopback).then_some(ProbeRefusal::NotLoopback)
}

/// Whether `init` may contact `url` (see [`probe_refusal`]).
pub fn probe_allowed(url: &str) -> bool {
    probe_refusal(url).is_none()
}

/// The only holder of a request-sending client in this module. Every method
/// checks [`probe_refusal`] before it sends anything.
struct Prober {
    client: reqwest::Client,
    timeout: Duration,
}

/// What asking a model service found.
#[derive(Debug, Clone)]
enum UrlProbe {
    /// Not contacted: [`ProbeRefusal`].
    Refused(ProbeRefusal),
    /// Not contacted: `--no-probe`.
    Skipped,
    /// No HTTP answer; the reason names a class, never the URL.
    Unreachable(String),
    /// It answered; what its model listing said.
    Reached(ModelIds),
}

impl Prober {
    fn new(timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            // A loopback service must not be able to send `init` to another
            // host or port, and a proxy would carry the request off the
            // machine.
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(Self { client, timeout })
    }

    /// Does `url` answer, and which models does it list.
    async fn survey(&self, label: &str, url: &str) -> UrlProbe {
        if let Some(refusal) = probe_refusal(url) {
            return UrlProbe::Refused(refusal);
        }
        if let Err(error) = probe_endpoint_reach(&self.client, url, label, self.timeout).await {
            return UrlProbe::Unreachable(format!("{error:#}"));
        }
        UrlProbe::Reached(fetch_model_ids(&self.client, url, label, self.timeout).await)
    }

    /// Whether `127.0.0.1:<port>` answers `/health` as this service does.
    async fn lci_serving(&self, port: u16) -> bool {
        let url = format!("http://127.0.0.1:{port}/health");
        if probe_refusal(&url).is_some() || RESERVED_PORTS.contains(&port) {
            return false;
        }
        let Ok(response) = self.client.get(&url).timeout(self.timeout).send().await else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        response.json::<Value>().await.is_ok_and(|body| {
            body["status"] == "ok" && body["service"] == "local-code-intelligence"
        })
    }
}

/// What `init` learns about a model service URL: refused (checked first, with
/// or without `--no-probe`), skipped under `--no-probe`, or surveyed.
async fn survey_url(prober: &Prober, options: &Options, label: &str, url: &str) -> UrlProbe {
    if let Some(refusal) = probe_refusal(url) {
        UrlProbe::Refused(refusal)
    } else if options.no_probe {
        UrlProbe::Skipped
    } else {
        prober.survey(label, url).await
    }
}

/// The outcome of binding the port.
#[derive(Debug)]
enum PortCheck {
    /// 8765-8767: not checked.
    Reserved,
    /// Port 0: the OS picks one at every start.
    Ephemeral,
    Free,
    /// The bind failed; `in_use` when the cause is an address already in use.
    Busy {
        in_use: bool,
        error: String,
    },
}

/// Binds `127.0.0.1:<port>` and releases it. Loopback only: a listener on
/// another interface can raise a firewall prompt.
fn check_port(port: u16) -> PortCheck {
    if RESERVED_PORTS.contains(&port) {
        return PortCheck::Reserved;
    }
    if port == 0 {
        return PortCheck::Ephemeral;
    }
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
        Ok(listener) => {
            drop(listener);
            PortCheck::Free
        }
        Err(error) => PortCheck::Busy {
            in_use: error.kind() == ErrorKind::AddrInUse,
            error: error.to_string(),
        },
    }
}

/// A tool `init` looks for.
struct Tool {
    /// The dotted machine key that takes its path.
    key: &'static str,
    /// The executable name looked up on `PATH`.
    command: &'static str,
    /// How to get it, for the comment when it is missing.
    hint: &'static str,
}

const TOOLS: [Tool; 7] = [
    Tool {
        key: "ripgrep_path",
        command: "rg",
        hint: "install ripgrep, or set ripgrep_path",
    },
    Tool {
        key: "rust_analyzer_path",
        command: "rust-analyzer",
        hint: "rustup component add rust-analyzer",
    },
    Tool {
        key: "csharp.path",
        command: "csharp-ls",
        hint: "dotnet tool install --global csharp-ls",
    },
    Tool {
        key: "typescript.path",
        command: "typescript-language-server",
        hint: "npm install -g typescript-language-server typescript",
    },
    Tool {
        key: "python.path",
        command: "pyright-langserver",
        hint: "npm install -g pyright",
    },
    Tool {
        key: "go.path",
        command: "gopls",
        hint: "go install golang.org/x/tools/gopls@latest",
    },
    Tool {
        key: "clangd.path",
        command: "clangd",
        hint: "install clangd from LLVM or your system packages",
    },
];

/// Writes `path` into the machine key of `tool`.
fn set_tool_path(overrides: &mut LocalOverrides, key: &str, path: String) {
    match key {
        "ripgrep_path" => overrides.ripgrep_path = Some(path),
        "rust_analyzer_path" => overrides.rust_analyzer_path = Some(path),
        _ => {
            let section = key.split_once('.').map_or(key, |(section, _)| section);
            overrides
                .lsp_mut(section)
                .get_or_insert_with(Default::default)
                .path = Some(path);
        }
    }
}

/// The settings and notes the detection produced.
#[derive(Debug, Default)]
struct Plan {
    overrides: LocalOverrides,
    notes: Vec<OverlayNote>,
    /// The dotted keys written.
    set: Vec<String>,
    /// One entry per tool, for `--json`.
    tools: Map<String, Value>,
    data_dir: Value,
}

/// `provenance` as a phrase: `file <path>`, `env LCI_DATA_DIR`.
fn describe_source(provenance: &Provenance) -> String {
    match &provenance.from {
        Some(from) => format!("{} {from}", provenance.source.as_str()),
        None => provenance.source.as_str().to_owned(),
    }
}

fn is_default(baseline: &Resolved, key: &str) -> bool {
    baseline
        .sources
        .get(key)
        .is_none_or(|provenance| provenance.source == Source::Default)
}

/// Decides what to write for the data directory and the tools. A key that a
/// higher layer already sets is left alone and noted; a tool `find` does not
/// locate is noted with how to get it; `jdtls` (an installation directory, not
/// an executable) is only noted.
fn plan_machine_values(baseline: &Resolved, find: &dyn Fn(&str) -> Option<PathBuf>) -> Plan {
    let mut plan = Plan::default();

    let data_dir = &baseline.config.data_dir;
    if !is_default(baseline, "data_dir") {
        let source = describe_source(&baseline.sources["data_dir"]);
        plan.notes.push(OverlayNote::new(
            "data_dir",
            format!("data_dir: set by {source}; not written"),
        ));
        plan.data_dir = json!({"action": "kept", "source": source});
    } else if let Some(text) = data_dir.to_str().filter(|_| data_dir.is_absolute()) {
        plan.overrides.data_dir = Some(data_dir.clone());
        plan.set.push("data_dir".to_owned());
        plan.notes.push(OverlayNote::new(
            "data_dir",
            "data_dir: the platform default, written out so this file records where indexes live",
        ));
        plan.data_dir = json!({"action": "written", "value": text});
    } else {
        plan.notes.push(OverlayNote::new(
            "data_dir",
            "data_dir: no usable platform data directory; set an absolute data_dir or LCI_DATA_DIR",
        ));
        plan.data_dir = json!({"action": "none"});
    }

    for tool in &TOOLS {
        let entry = if !is_default(baseline, tool.key) {
            let source = describe_source(&baseline.sources[tool.key]);
            plan.notes.push(OverlayNote::new(
                tool.key,
                format!(
                    "{}: {} is already set by {source}; not written",
                    tool.command, tool.key
                ),
            ));
            json!({"action": "kept", "source": source})
        } else if let Some(found) = find(tool.command) {
            // `PATH` may hold a relative directory; an overlay path must not
            // depend on where the service is started.
            let found = std::path::absolute(&found).unwrap_or(found);
            if let Some(text) = found.to_str() {
                set_tool_path(&mut plan.overrides, tool.key, text.to_owned());
                plan.set.push(tool.key.to_owned());
                json!({"action": "written", "path": text})
            } else {
                plan.notes.push(OverlayNote::new(
                    tool.key,
                    format!(
                        "{}: found at a path that is not valid UTF-8; not written",
                        tool.command
                    ),
                ));
                json!({"action": "not_written", "reason": "path is not valid UTF-8"})
            }
        } else {
            plan.notes.push(OverlayNote::new(
                tool.key,
                format!("{}: not on PATH ({})", tool.command, tool.hint),
            ));
            json!({"action": "not_found", "hint": tool.hint})
        };
        plan.tools.insert(tool.command.to_owned(), entry);
    }

    if is_default(baseline, "java.path") {
        plan.notes.push(OverlayNote::new(
            "java.path",
            "jdtls: [java].path is an installation directory, not an executable on PATH; set it by hand",
        ));
    }
    plan
}

/// Most model ids listed in a comment.
const MAX_LISTED_MODELS: usize = 10;
/// Most characters of one model id kept in a comment.
const MAX_MODEL_ID_CHARS: usize = 80;

fn listed_models(ids: &[String]) -> String {
    if ids.is_empty() {
        return "none".to_owned();
    }
    let mut shown: Vec<String> = ids
        .iter()
        .take(MAX_LISTED_MODELS)
        .map(|id| id.chars().take(MAX_MODEL_ID_CHARS).collect())
        .collect();
    if ids.len() > MAX_LISTED_MODELS {
        shown.push(format!("and {} more", ids.len() - MAX_LISTED_MODELS));
    }
    shown.join(", ")
}

/// The comment about one model service. `key` is `embedding_url` or
/// `reranker_url`; `model_key` and `model` the configured model name, which
/// `init` reports on and never changes.
fn url_note(key: &str, model_key: &str, model: &str, probe: &UrlProbe) -> String {
    match probe {
        UrlProbe::Refused(refusal) => format!("{key}: not probed; {}", refusal.message()),
        UrlProbe::Skipped => format!("{key}: not probed (--no-probe)"),
        UrlProbe::Unreachable(reason) => format!("{key}: unreachable ({reason})"),
        UrlProbe::Reached(ModelIds::NotExposed) => format!(
            "{key}: reachable; its model listing is not exposed (HTTP 404/405), so {model_key} '{model}' could not be checked"
        ),
        UrlProbe::Reached(ModelIds::Failed(reason)) => {
            format!("{key}: reachable; its model listing failed ({reason})")
        }
        UrlProbe::Reached(ModelIds::Listed(ids)) => {
            let listed = listed_models(ids);
            if ids.iter().any(|id| id == model) {
                format!("{key}: reachable; lists models: {listed}; {model_key} '{model}' is listed")
            } else {
                format!(
                    "{key}: reachable; lists models: {listed}; {model_key} '{model}' is NOT listed -- set {model_key} to a listed model in the configuration file or this overlay (a different model re-embeds existing indexes); init does not change model names"
                )
            }
        }
    }
}

fn url_probe_json(model: &str, probe: &UrlProbe) -> Value {
    match probe {
        UrlProbe::Refused(refusal) => json!({"status": "refused", "detail": refusal.message()}),
        UrlProbe::Skipped => json!({"status": "skipped", "detail": "--no-probe"}),
        UrlProbe::Unreachable(reason) => json!({"status": "unreachable", "detail": reason}),
        UrlProbe::Reached(ModelIds::NotExposed) => json!({
            "status": "reachable", "model": model, "model_listed": null,
            "detail": "model listing not exposed (HTTP 404/405)",
        }),
        UrlProbe::Reached(ModelIds::Failed(reason)) => json!({
            "status": "reachable", "model": model, "model_listed": null, "detail": reason,
        }),
        UrlProbe::Reached(ModelIds::Listed(ids)) => json!({
            "status": "reachable",
            "model": model,
            "model_listed": ids.iter().any(|id| id == model),
            "models": ids.iter().take(50).collect::<Vec<_>>(),
        }),
    }
}

/// The comment and the JSON entry about the port. `serving` is whether an LCI
/// answered `/health` there (`None`: not asked).
fn port_note(port: u16, check: &PortCheck, serving: Option<bool>) -> (String, Value) {
    let never = "init never writes port and never picks another";
    match check {
        PortCheck::Reserved => (
            format!(
                "port: {port} is reserved for another service (8765 generation model, 8766 embedding, 8767 reranker), so it is not checked; {never}"
            ),
            json!({"value": port, "state": "reserved"}),
        ),
        PortCheck::Ephemeral => (
            format!("port: 0 asks the OS for a free port at every start; {never}"),
            json!({"value": port, "state": "ephemeral"}),
        ),
        PortCheck::Free => (
            format!(
                "port: {port} is free on 127.0.0.1; {never} (set port in the configuration file, this overlay or LCI_PORT to change it)"
            ),
            json!({"value": port, "state": "free"}),
        ),
        PortCheck::Busy { in_use, error } => {
            let state = if *in_use { "in use" } else { "not bindable" };
            let who = match serving {
                Some(true) => "an LCI is serving there".to_owned(),
                Some(false) => "another process holds it".to_owned(),
                None => "not identified: --no-probe sends no request".to_owned(),
            };
            let reason = if *in_use {
                String::new()
            } else {
                format!(" ({error})")
            };
            (
                format!("port: {port} is {state} on 127.0.0.1{reason}; {who}; {never}"),
                json!({
                    "value": port,
                    "state": if *in_use { "in_use" } else { "not_bindable" },
                    "lci_serving": serving,
                    "detail": error,
                }),
            )
        }
    }
}

/// `YYYY-MM-DD` (UTC) of `seconds` since the Unix epoch.
fn utc_date(seconds: u64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let days = i64::try_from(seconds / 86_400).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn header_line() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    format!(
        "local-code-intelligence local overlay -- machine-specific values only; generated by lci init {}; not for version control",
        utc_date(now)
    )
}

/// Looks `command` up the way the service would find it: `rg` also in the
/// VS Code bundles on Windows, everything else on `PATH`.
fn find_tool(command: &str) -> Option<PathBuf> {
    if command == "rg" {
        let resolved = lexical::resolve_ripgrep_path("rg");
        // An unresolved `rg` comes back as the bare configured name.
        return (resolved != Path::new("rg")).then_some(resolved);
    }
    lexical::find_on_path(command)
}

/// Writes `contents` to `path` through a temporary file beside it and a
/// rename. Without `force` an existing `path` is left alone.
fn write_atomically(path: &Path, contents: &str, force: bool) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let name = path
        .file_name()
        .and_then(OsStr::to_str)
        .context("the overlay path has no file name")?;
    let temp = path.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file =
            std::fs::File::create(&temp).with_context(|| format!("create {}", temp.display()))?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        if !force && std::fs::symlink_metadata(path).is_ok() {
            bail!("{}", exists_message(path));
        }
        std::fs::rename(&temp, path)
            .with_context(|| format!("replace {} with the new overlay", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// Detects what this machine has, writes the overlay unless `--dry-run`, and
/// reports. `spec` is resolved without the existing overlay (it is what this
/// run replaces), so the layers above it decide what is already set. See the
/// module documentation for what is contacted.
pub async fn run(spec: &ResolveSpec, options: &Options) -> Result<Outcome> {
    let mut baseline_spec = spec.clone();
    baseline_spec.flags.no_local_config = true;
    let baseline = layers::resolve_unvalidated(&baseline_spec)?;
    let Some(path) = baseline.layers.local.path.clone() else {
        bail!(
            "this platform has no per-user configuration directory; pass --config or set LCI_LOCAL_CONFIG to say where the overlay goes"
        );
    };
    let named_by = baseline.layers.local.named_by;
    let path_text = path.display().to_string();
    let exists = match std::fs::symlink_metadata(&path) {
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", path.display()));
        }
    };
    if exists && !options.force && !options.dry_run {
        let message = exists_message(&path);
        return Ok(Outcome {
            status: Status::Exists,
            report: json!({
                "path": path_text,
                "named_by": named_by,
                "status": Status::Exists.as_str(),
                "written": false,
                "exists": true,
                "error": message,
            }),
            path,
            contents: String::new(),
            warnings: Vec::new(),
        });
    }

    let mut plan = plan_machine_values(&baseline, &find_tool);
    let config = &baseline.config;

    let timeout = Duration::from_secs(config.readiness_timeout_seconds.clamp(1, 30));
    let prober = Prober::new(timeout)?;
    let port = config.port;
    let port_check = check_port(port);
    let (embedding, reranker, serving) = tokio::join!(
        survey_url(
            &prober,
            options,
            "embedding endpoint",
            &config.embedding_url
        ),
        survey_url(&prober, options, "reranker endpoint", &config.reranker_url),
        async {
            match &port_check {
                PortCheck::Busy { .. } if !options.no_probe => Some(prober.lci_serving(port).await),
                _ => None,
            }
        }
    );

    let mut warnings = Vec::new();
    let mut probes = Map::new();
    for (key, model_key, model, probe) in [
        (
            "embedding_url",
            "embedding_model",
            &config.embedding_model,
            &embedding,
        ),
        (
            "reranker_url",
            "reranker_model",
            &config.reranker_model,
            &reranker,
        ),
    ] {
        let note = url_note(key, model_key, model, probe);
        if matches!(probe, UrlProbe::Refused(_)) {
            warnings.push(note.clone());
        }
        plan.notes.push(OverlayNote::new(key, note));
        probes.insert(key.to_owned(), url_probe_json(model, probe));
    }
    let (note, port_json) = port_note(port, &port_check, serving);
    plan.notes.push(OverlayNote::new("port", note));
    probes.insert("port".to_owned(), port_json);

    let contents = render_overlay(&header_line(), &plan.overrides, &plan.notes);
    let (valid, validation) = check_generated(&baseline, &plan.overrides, &contents)?;
    if let Some(message) = validation {
        warnings.push(message);
    }

    if exists && options.dry_run && !options.force {
        warnings.push(format!(
            "{} already exists; a real run needs --force to replace it",
            path.display()
        ));
    }
    let status = if options.dry_run {
        Status::DryRun
    } else {
        write_atomically(&path, &contents, options.force)?;
        Status::Written
    };
    let report = json!({
        "path": path_text,
        "named_by": named_by,
        "status": status.as_str(),
        "written": status == Status::Written,
        "dry_run": options.dry_run,
        "exists": exists,
        "overlay": contents,
        "set": plan.set,
        "detected": {"data_dir": plan.data_dir, "tools": plan.tools},
        "probes": probes,
        "comments": plan.notes.iter().map(|note| note.text.as_str()).collect::<Vec<_>>(),
        "valid": valid,
        "warnings": warnings,
    });
    Ok(Outcome {
        status,
        path,
        contents,
        report,
        warnings,
    })
}

/// Reads the generated text back and applies it over the layers it will sit
/// on. The text must parse as exactly the patch `init` built, and the result
/// must validate, unless it was already invalid for a reason the overlay
/// neither causes nor can fix (for example a model URL on port 8765 in the
/// main configuration): then `init` still writes the machine values and
/// returns the message to warn with. Returns whether the configuration is
/// valid with the overlay, and that message.
fn check_generated(
    baseline: &Resolved,
    overrides: &LocalOverrides,
    contents: &str,
) -> Result<(bool, Option<String>)> {
    let reparsed =
        LocalOverrides::parse(contents).context("the generated overlay does not parse")?;
    ensure!(
        &reparsed == overrides,
        "the generated overlay does not read back as the settings init chose"
    );
    let mut patched: Config = baseline.config.clone();
    reparsed.apply(&mut patched);
    match (baseline.config.validate(), patched.validate()) {
        (_, Ok(())) => Ok((true, None)),
        (Err(before), Err(after)) if format!("{before:#}") == format!("{after:#}") => Ok((
            false,
            Some(format!(
                "the configuration is invalid and init cannot fix it: {after:#}"
            )),
        )),
        (_, Err(after)) => {
            bail!("the generated overlay would make the configuration invalid: {after:#}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::layers::Flags;
    use std::collections::BTreeMap;

    #[test]
    fn probe_refusal_judges_every_url() {
        // port 8765 is refused for every host spelling and scheme
        {
            for url in [
                "http://127.0.0.1:8765/v1",
                "http://localhost:8765/v1",
                "http://LocalHost:8765/v1",
                "http://[::1]:8765/v1",
                "http://127.1:8765/v1",
                "http://0.0.0.0:8765/rerank",
                "http://192.168.1.20:8765/v1",
                "http://models.example.com:8765/v1",
                "https://models.example.com:8765",
                "http://user:secret@127.0.0.1:8765/v1",
                "http://127.0.0.1:08765/v1",
                "https://localhost:8765/v1/models?x=1#frag",
                "ftp://localhost:8765/x",
            ] {
                assert_eq!(
                    probe_refusal(url),
                    Some(ProbeRefusal::GenerationModelPort),
                    "{url}"
                );
                assert!(!probe_allowed(url), "{url}");
            }
            assert!(
                ProbeRefusal::GenerationModelPort
                    .message()
                    .contains("port 8765 is the generation model; LCI never contacts it")
            );
        }

        // only loopback hosts are allowed
        {
            for url in [
                "http://127.0.0.1:8766/v1",
                "http://127.0.0.1/v1",
                "http://127.0.0.2:9000/v1",
                "http://localhost:8767/rerank",
                "http://LOCALHOST:9000",
                "https://localhost/v1",
                "http://[::1]:9000/v1",
                "http://user:secret@127.0.0.1:9000/v1",
                "http://127.1:9000/v1",
                // Ports around 8765 are other services' or nobody's, not refused.
                "http://127.0.0.1:8764/v1",
                "http://127.0.0.1:8766/v1",
                "http://127.0.0.1:18765/v1",
                "http://127.0.0.1:50000/v1",
            ] {
                assert_eq!(probe_refusal(url), None, "{url}");
                assert!(probe_allowed(url), "{url}");
            }
            for url in [
                "http://192.0.2.10:8766/v1",
                "http://192.168.1.20:9000/v1",
                "http://models.example.com:8766/v1",
                "https://models.example.com/v1",
                "http://0.0.0.0:9000/v1",
                "http://[2001:db8::1]:9000/v1",
                "http://localhost.example.com:9000/v1",
                "http://localhost.:9000/v1",
            ] {
                assert_eq!(probe_refusal(url), Some(ProbeRefusal::NotLoopback), "{url}");
            }
        }

        // a URL that is not http or not a URL is refused
        {
            assert_eq!(
                probe_refusal("ftp://127.0.0.1:9000/x"),
                Some(ProbeRefusal::NotHttp)
            );
            assert_eq!(
                probe_refusal("file:///etc/hosts"),
                Some(ProbeRefusal::NotHttp)
            );
            for url in ["", "not a url", "127.0.0.1:8766/v1", "http://"] {
                assert_eq!(probe_refusal(url), Some(ProbeRefusal::Invalid), "{url:?}");
            }
            // A scheme with a default port of its own is judged by that port.
            assert_eq!(probe_refusal("https://localhost"), None);
        }
    }

    #[test]
    fn utc_dates_match_known_instants() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(86_399), "1970-01-01");
        assert_eq!(utc_date(86_400), "1970-01-02");
        assert_eq!(utc_date(951_782_400), "2000-02-29");
        assert_eq!(utc_date(951_868_800), "2000-03-01");
        assert_eq!(utc_date(1_791_676_800), "2026-10-11");
        assert_eq!(utc_date(1_798_761_599), "2026-12-31");
        assert_eq!(utc_date(1_798_761_600), "2027-01-01");
    }

    #[test]
    fn model_listings_are_summarised_in_bounded_text() {
        assert_eq!(listed_models(&[]), "none");
        let many: Vec<String> = (0..13).map(|index| format!("m{index}")).collect();
        let text = listed_models(&many);
        assert!(text.starts_with("m0, m1, "), "{text}");
        assert!(text.ends_with("m9, and 3 more"), "{text}");
        let long = vec!["x".repeat(500)];
        assert_eq!(listed_models(&long).chars().count(), MAX_MODEL_ID_CHARS);
    }

    #[test]
    fn url_notes_report_the_configured_model_without_changing_it() {
        let listed = |ids: &[&str]| {
            UrlProbe::Reached(ModelIds::Listed(
                ids.iter().map(|id| (*id).to_owned()).collect(),
            ))
        };
        let note = url_note(
            "embedding_url",
            "embedding_model",
            "e1",
            &listed(&["e1", "r1"]),
        );
        assert_eq!(
            note,
            "embedding_url: reachable; lists models: e1, r1; embedding_model 'e1' is listed"
        );
        let note = url_note("embedding_url", "embedding_model", "e2", &listed(&["e1"]));
        assert!(
            note.contains("embedding_model 'e2' is NOT listed"),
            "{note}"
        );
        assert!(note.contains("init does not change model names"), "{note}");
        let note = url_note(
            "reranker_url",
            "reranker_model",
            "r",
            &UrlProbe::Unreachable("connect to reranker endpoint: connection failure".into()),
        );
        assert_eq!(
            note,
            "reranker_url: unreachable (connect to reranker endpoint: connection failure)"
        );
        let note = url_note(
            "embedding_url",
            "embedding_model",
            "e",
            &UrlProbe::Refused(ProbeRefusal::GenerationModelPort),
        );
        assert_eq!(
            note,
            "embedding_url: not probed; port 8765 is the generation model; LCI never contacts it (AGENTS.md)"
        );
        let note = url_note("embedding_url", "embedding_model", "e", &UrlProbe::Skipped);
        assert_eq!(note, "embedding_url: not probed (--no-probe)");
        // No note carries a URL: a configured one may hold credentials.
        for probe in [
            UrlProbe::Skipped,
            UrlProbe::Refused(ProbeRefusal::NotLoopback),
            listed(&["a"]),
        ] {
            let note = url_note("embedding_url", "embedding_model", "e", &probe);
            assert!(!note.contains("://"), "{note}");
        }
    }

    #[test]
    fn port_notes_never_promise_to_write_a_port() {
        let (note, json) = port_note(8768, &PortCheck::Free, None);
        assert!(note.contains("port: 8768 is free on 127.0.0.1"), "{note}");
        assert!(note.contains("never writes port"), "{note}");
        assert_eq!(json["state"], "free");
        let busy = PortCheck::Busy {
            in_use: true,
            error: "in use".to_owned(),
        };
        let (note, json) = port_note(9000, &busy, Some(true));
        assert!(note.contains("is in use on 127.0.0.1"), "{note}");
        assert!(note.contains("an LCI is serving there"), "{note}");
        assert_eq!(json["lci_serving"], true);
        let (note, _) = port_note(9000, &busy, Some(false));
        assert!(note.contains("another process holds it"), "{note}");
        let (note, json) = port_note(9000, &busy, None);
        assert!(note.contains("--no-probe sends no request"), "{note}");
        assert!(json["lci_serving"].is_null());
        for port in [8765, 8766, 8767] {
            assert!(matches!(check_port(port), PortCheck::Reserved));
            let (note, json) = port_note(port, &PortCheck::Reserved, None);
            assert!(note.contains("not checked"), "{note}");
            assert_eq!(json["state"], "reserved");
        }
        assert!(matches!(check_port(0), PortCheck::Ephemeral));
    }

    /// The layers of a fresh file in `dir`, resolved without a local overlay.
    fn baseline(dir: &Path, text: &str) -> Resolved {
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        layers::resolve_unvalidated(&ResolveSpec {
            config_path: Some(path),
            env: BTreeMap::new(),
            user_config_dir: None,
            flags: Flags {
                port: None,
                no_local_config: true,
            },
        })
        .unwrap()
    }

    fn finder<'a>(tools: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<PathBuf> + 'a {
        move |name| {
            tools
                .iter()
                .find(|(tool, _)| *tool == name)
                .map(|(_, path)| PathBuf::from(path))
        }
    }

    /// An absolute path for a tool that does not exist, built component by
    /// component so `std::path::absolute` leaves it as written on Windows.
    fn absolute(name: &str) -> String {
        std::env::temp_dir()
            .join("lci-init-unit")
            .join(name)
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn the_plan_writes_what_it_found_and_notes_what_it_did_not() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = baseline(temp.path(), "");
        let ra = absolute("rust-analyzer");
        let ts = absolute("typescript-language-server");
        let plan = plan_machine_values(
            &resolved,
            &finder(&[("rust-analyzer", &ra), ("typescript-language-server", &ts)]),
        );
        assert_eq!(
            plan.overrides.rust_analyzer_path.as_deref(),
            Some(ra.as_str())
        );
        assert_eq!(
            plan.overrides
                .typescript
                .as_ref()
                .and_then(|lsp| lsp.path.as_deref()),
            Some(ts.as_str())
        );
        // Nothing found, nothing written.
        assert!(plan.overrides.ripgrep_path.is_none());
        assert!(plan.overrides.csharp.is_none() && plan.overrides.go.is_none());
        assert!(plan.overrides.port.is_none());
        assert!(plan.overrides.embedding_model.is_none());
        assert!(plan.overrides.embedding_url.is_none());
        // The platform default data_dir is written, made explicit.
        assert_eq!(
            plan.overrides.data_dir.as_deref(),
            Some(Config::default().data_dir.as_path())
        );
        assert_eq!(
            plan.set,
            ["data_dir", "rust_analyzer_path", "typescript.path"]
        );
        let texts: Vec<&str> = plan.notes.iter().map(|note| note.text.as_str()).collect();
        for expected in [
            "rg: not on PATH (install ripgrep, or set ripgrep_path)",
            "csharp-ls: not on PATH (dotnet tool install --global csharp-ls)",
            "pyright-langserver: not on PATH (npm install -g pyright)",
            "gopls: not on PATH (go install golang.org/x/tools/gopls@latest)",
            "clangd: not on PATH (install clangd from LLVM or your system packages)",
        ] {
            assert!(texts.contains(&expected), "{expected}");
        }
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with("jdtls: [java].path")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|text| text.starts_with("rust-analyzer:")),
            "a found tool needs no note"
        );
        assert_eq!(plan.tools["gopls"]["action"], "not_found");
        assert_eq!(plan.tools["rust-analyzer"]["action"], "written");
    }

    #[test]
    fn a_key_a_higher_layer_sets_is_left_alone_and_noted() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let resolved = baseline(
            temp.path(),
            &format!(
                "data_dir = '{}'\nrust_analyzer_path = 'ra-from-file'\n[typescript]\npath = 'ts-from-file'\n",
                data.display()
            ),
        );
        let ra = absolute("rust-analyzer");
        let ts = absolute("typescript-language-server");
        let gopls = absolute("gopls");
        let plan = plan_machine_values(
            &resolved,
            &finder(&[
                ("rust-analyzer", &ra),
                ("typescript-language-server", &ts),
                ("gopls", &gopls),
            ]),
        );
        assert!(plan.overrides.data_dir.is_none());
        assert!(plan.overrides.rust_analyzer_path.is_none());
        assert!(plan.overrides.typescript.is_none());
        assert_eq!(plan.set, ["go.path"]);
        let texts: Vec<&str> = plan.notes.iter().map(|note| note.text.as_str()).collect();
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with("data_dir: set by file ")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|text| text
                .starts_with("rust-analyzer: rust_analyzer_path is already set by file ")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|text| text.starts_with(
                "typescript-language-server: typescript.path is already set by file "
            )),
            "{texts:?}"
        );
        assert_eq!(plan.tools["typescript-language-server"]["action"], "kept");
    }

    #[test]
    fn a_relative_directory_on_path_is_written_as_an_absolute_path() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = baseline(temp.path(), "");
        let plan = plan_machine_values(&resolved, &finder(&[("gopls", "relative/bin/gopls")]));
        let written = plan
            .overrides
            .go
            .as_ref()
            .and_then(|lsp| lsp.path.as_deref())
            .unwrap();
        assert!(Path::new(written).is_absolute(), "{written}");
        assert!(written.ends_with("gopls"), "{written}");
    }

    #[test]
    fn the_generated_text_must_read_back_and_validate() {
        let temp = tempfile::tempdir().unwrap();
        let resolved = baseline(temp.path(), "");
        let overrides = LocalOverrides {
            port: Some(9000),
            ..LocalOverrides::default()
        };
        let text = render_overlay("t", &overrides, &[]);
        assert_eq!(
            check_generated(&resolved, &overrides, &text).unwrap(),
            (true, None)
        );
        // Text that is not the patch is an internal error, not a write.
        let other = render_overlay("t", &LocalOverrides::default(), &[]);
        assert!(check_generated(&resolved, &overrides, &other).is_err());
        // A patch that breaks a valid configuration is refused.
        let reserved = LocalOverrides {
            port: Some(8767),
            ..LocalOverrides::default()
        };
        let text = render_overlay("t", &reserved, &[]);
        let error = check_generated(&resolved, &reserved, &text)
            .unwrap_err()
            .to_string();
        assert!(error.contains("8767"), "{error}");

        // A configuration that is already invalid, for a reason the overlay
        // does not touch, is reported and does not stop the write.
        let invalid = baseline(temp.path(), "embedding_url = 'http://127.0.0.1:8765/v1'\n");
        let text = render_overlay("t", &overrides, &[]);
        let (valid, warning) = check_generated(&invalid, &overrides, &text).unwrap();
        assert!(!valid);
        let warning = warning.unwrap();
        assert!(warning.contains("init cannot fix it"), "{warning}");
        assert!(warning.contains("generation model"), "{warning}");
    }

    #[test]
    fn the_overlay_is_written_atomically_and_never_replaced_without_force() {
        let temp = tempfile::tempdir().unwrap();
        // The parent directory is created.
        let path = temp.path().join("nested").join("user").join("o.local.toml");
        write_atomically(&path, "first\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        let error = write_atomically(&path, "second\n", false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("--force"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        write_atomically(&path, "second\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
        // No temporary file is left behind either way.
        let names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["o.local.toml"]);
    }
}
