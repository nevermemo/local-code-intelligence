//! `cargo xtask maps lsp-cost`: what `lsp.live` costs on a live tree (K11d-1).
//!
//! One arm is one `serve` process on port 8769 (127.0.0.1) with its own scratch
//! data directory and one TOML config: the arm's `[run_profile.lsp]` leaves on
//! top of a fixed base. For each question the harness calls MCP `search_code
//! mode = traverse` `--repeat` times round-robin, never restarting the server
//! between calls, so the first call of an arm is the cold run and the rest are
//! warm. The `lsp.live` map is added by `[traverse.policies.explore]
//! extra_maps` because an MCP call has no `maps` argument. Per call the harness
//! reads the answer (`totals`, the per-map ms, notes) and samples the RSS of
//! the server's process tree; the version probe's ms comes from one `debug!`
//! line of the server's log (the probe is memoised for the process since d470:
//! only the first probe of a binary costs real ms, later calls log about 0).
//! Two load conditions exist: `quiet`, and `loaded` (busy CPU threads, no disk
//! traffic, for the whole condition).
//!
//! Everything that needs no process (answer parsing, arm configs, the
//! arithmetic and the report tables) is a pure function with a unit test.

use crate::acceptance::{
    McpSession, PORT, lci_binary, strip_lci_env, wait_http_ok, workspace_root,
};
use crate::live_acceptance::descendants_of;
use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, ValueEnum};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::process::{Child, Command};

/// The committed question list (`xtask/questions/lsp-cost.json`), so the
/// numbers pair across runs.
const DEFAULT_QUESTIONS: &str = include_str!("../../questions/lsp-cost.json");

/// Ports the harness must never touch.
const FORBIDDEN_PORTS: [u16; 2] = [8765, 8768];

/// The `lsp.live` notes the harness classifies.
pub const NOTE_UNAVAILABLE: &str = "lsp.live is unavailable";
pub const NOTE_DEADLINE: &str = "lsp.live: the traversal deadline passed";
pub const NOTE_NODE_ERROR: &str = "lsp.live reported an error for a node";

/// Which load conditions to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum LoadChoice {
    /// Only the quiet condition.
    None,
    /// Only the loaded condition (CPU-only synthetic load).
    Cpu,
    /// Quiet, then loaded.
    Both,
}

/// The language adapter whose server `lsp.live` is measured on (K11d-2a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Adapter {
    Rust,
    Python,
    Go,
    Typescript,
    Java,
    C,
    Cpp,
    Csharp,
}

impl Adapter {
    #[cfg(test)]
    pub const ALL: [Adapter; 8] = [
        Adapter::Rust,
        Adapter::Python,
        Adapter::Go,
        Adapter::Typescript,
        Adapter::Java,
        Adapter::C,
        Adapter::Cpp,
        Adapter::Csharp,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Adapter::Rust => "rust",
            Adapter::Python => "python",
            Adapter::Go => "go",
            Adapter::Typescript => "typescript",
            Adapter::Java => "java",
            Adapter::C => "c",
            Adapter::Cpp => "cpp",
            Adapter::Csharp => "csharp",
        }
    }

    /// The config section holding the adapter's `path` (the acceptance fixtures
    /// write the same sections); rust has none, it uses `rust_analyzer_path`.
    pub fn section(self) -> Option<&'static str> {
        match self {
            Adapter::Rust => None,
            Adapter::Python => Some("python"),
            Adapter::Go => Some("go"),
            Adapter::Typescript => Some("typescript"),
            Adapter::Java => Some("java"),
            Adapter::C | Adapter::Cpp => Some("clangd"),
            Adapter::Csharp => Some("csharp"),
        }
    }

    /// The `which` name used when `--server` is not given (jdtls has none: it
    /// is an install directory).
    pub fn default_server(self) -> Option<&'static str> {
        match self {
            Adapter::Rust => Some("rust-analyzer"),
            Adapter::Python => Some("pyright-langserver"),
            Adapter::Go => Some("gopls"),
            Adapter::Typescript => Some("typescript-language-server"),
            Adapter::Java => None,
            Adapter::C | Adapter::Cpp => Some("clangd"),
            Adapter::Csharp => Some("csharp-ls"),
        }
    }

    /// Process names (substring, case-insensitive) of the language server in
    /// the process tree of `serve`: pyright and typescript-language-server run
    /// on `node`, jdtls on `java`.
    pub fn rss_names(self) -> &'static [&'static str] {
        match self {
            Adapter::Rust => &["rust-analyzer"],
            Adapter::Python | Adapter::Typescript => &["node"],
            Adapter::Go => &["gopls"],
            Adapter::C | Adapter::Cpp => &["clangd"],
            Adapter::Csharp => &["csharp-ls"],
            Adapter::Java => &["java"],
        }
    }

    /// Whether `<server> --version` is a real probe: pyright-langserver has no
    /// such flag and jdtls is a directory.
    pub fn has_version_flag(self) -> bool {
        !matches!(self, Adapter::Python | Adapter::Java)
    }
}

/// The server the arms configure: `--server`, else the adapter's default name
/// (rust: `--rust-analyzer`); jdtls has no default.
pub fn resolve_server(
    adapter: Adapter,
    server: Option<&str>,
    rust_analyzer: &str,
) -> Result<String> {
    if let Some(server) = server {
        return Ok(server.to_owned());
    }
    if adapter == Adapter::Rust {
        return Ok(rust_analyzer.to_owned());
    }
    adapter.default_server().map(str::to_owned).ok_or_else(|| {
        anyhow!(
            "--adapter {} has no default server: pass --server <path> (jdtls: its install directory)",
            adapter.name()
        )
    })
}

/// True when a process named `process_name` is one of `names` (substring,
/// case-insensitive).
pub fn rss_name_matches(process_name: &str, names: &[&str]) -> bool {
    let lower = process_name.to_ascii_lowercase();
    names.iter().any(|name| lower.contains(name))
}

#[derive(Args, Debug)]
pub struct LspCostArgs {
    /// Tree to walk and serve (default: this repository).
    #[arg(long)]
    pub tree: Option<PathBuf>,
    /// SCIP index of the tree. Missing: `rust-analyzer scip` writes it into the
    /// output directory (the report says so); for a non-rust `--adapter` an
    /// empty `index.scip` is written there instead (scip maps give nothing).
    /// Default: `<tree>/index.scip`.
    #[arg(long)]
    pub scip: Option<PathBuf>,
    /// Language adapter whose server is measured. The tree must hold that
    /// language's files; the arms write `[<section>] path = '<server>'`.
    #[arg(long, value_enum, default_value = "rust")]
    pub adapter: Adapter,
    /// The adapter's server, the `[<section>] path` of the arms (default: its
    /// name on PATH; jdtls has none and needs its install directory). For rust
    /// it overrides `--rust-analyzer`.
    #[arg(long)]
    pub server: Option<String>,
    /// JSON list of `{id, question}` (default: the committed list).
    #[arg(long)]
    pub questions: Option<PathBuf>,
    /// Arms, comma separated (default: all):
    /// base, shared, shared+cache, shared+impl, shared+sync, per_call+sync, lifecycle
    /// (`base` and `per_call+sync` write `sessions = "per_call"`: the default is `shared`).
    #[arg(long, value_delimiter = ',')]
    pub arms: Vec<String>,
    /// Calls per question per arm, round-robin over the questions.
    #[arg(long, default_value_t = 5)]
    pub repeat: usize,
    /// Use only the first N questions (smoke runs).
    #[arg(long)]
    pub max_questions: Option<usize>,
    /// Load conditions.
    #[arg(long, value_enum, default_value = "both")]
    pub load: LoadChoice,
    /// Busy threads of the loaded condition (default: half the logical cores, about what two `cargo check` runs keep busy).
    #[arg(long)]
    pub load_threads: Option<usize>,
    /// Request deadline of every call, ms (the config cap is the same).
    #[arg(long, default_value_t = 20_000)]
    pub deadline_ms: u64,
    /// Timed `<server> --version` runs per condition (none for pyright and jdtls).
    #[arg(long, default_value_t = 20)]
    pub probe_runs: usize,
    /// Calls of the N8 window (the first calls of the `shared` arm).
    #[arg(long, default_value_t = 30)]
    pub n8_window: usize,
    /// The server binary (default `$CARGO_TARGET_DIR/debug/..`, else target/debug);
    /// it is copied before it runs.
    #[arg(long)]
    pub binary: Option<PathBuf>,
    /// `rust_analyzer_path` of the arms.
    #[arg(long, default_value = "rust-analyzer")]
    pub rust_analyzer: String,
    /// Where the arms' data directories and the copied binary go (default: the OS temp dir).
    #[arg(long)]
    pub work_dir: Option<PathBuf>,
    /// Output directory (default `test-results/lsp-cost/<stamp>/`).
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Render `report-merged.md` from earlier run directories (comma separated,
    /// in order; an arm present in two comes from the later one) instead of
    /// running anything. Written into `--out`, default the last directory.
    #[arg(long, value_delimiter = ',')]
    pub report_from: Vec<PathBuf>,
    /// Minutes to wait, before each arm of the quiet condition and before the
    /// synthetic load starts, for the machine's CPU use to fall below 20% (some
    /// other job is running); after that the run goes on and says so.
    #[arg(long, default_value_t = 45)]
    pub max_quiet_wait_minutes: u64,
    /// `[run_profile.lsp] places` of every arm. `scip` (this harness's default) writes
    /// `places = "scip"` (the key's own default is `auto`); `outline` lets `lsp.live` place symbols from the server's
    /// outline, which a tree without a SCIP index needs to get any card.
    #[arg(long, value_enum, default_value = "scip")]
    pub places: Places,
}

/// The `--places` choice (mirrors `[run_profile.lsp] places`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Places {
    Scip,
    Outline,
}

// ---------------------------------------------------------------------------
// Arms and configs (pure)
// ---------------------------------------------------------------------------

/// One arm: the `[run_profile.lsp]` keys it turns on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Arm {
    pub name: &'static str,
    pub shared: bool,
    pub outline_cache: bool,
    pub implementations: bool,
    pub sync_watch: bool,
}

pub const ARMS: [Arm; 7] = [
    Arm {
        name: "base",
        shared: false,
        outline_cache: false,
        implementations: false,
        sync_watch: false,
    },
    Arm {
        name: "shared",
        shared: true,
        outline_cache: false,
        implementations: false,
        sync_watch: false,
    },
    Arm {
        name: "shared+cache",
        shared: true,
        outline_cache: true,
        implementations: false,
        sync_watch: false,
    },
    Arm {
        name: "shared+impl",
        shared: true,
        outline_cache: false,
        implementations: true,
        sync_watch: false,
    },
    Arm {
        name: "shared+sync",
        shared: true,
        outline_cache: false,
        implementations: false,
        sync_watch: true,
    },
    Arm {
        name: "per_call+sync",
        shared: false,
        outline_cache: false,
        implementations: false,
        sync_watch: true,
    },
    Arm {
        name: "lifecycle",
        shared: true,
        outline_cache: true,
        implementations: true,
        sync_watch: true,
    },
];

/// The arms named by `--arms` (all when none), in the order given.
pub fn select_arms(names: &[String]) -> Result<Vec<Arm>> {
    if names.is_empty() {
        return Ok(ARMS.to_vec());
    }
    names
        .iter()
        .map(|name| {
            ARMS.iter()
                .find(|arm| arm.name == name.trim())
                .copied()
                .ok_or_else(|| {
                    anyhow!(
                        "unknown arm {name:?}; the arms are {}",
                        ARMS.map(|arm| arm.name).join(", ")
                    )
                })
        })
        .collect()
}

/// The `[run_profile.lsp]` leaf lines of an arm: `sessions` always (the
/// default is `shared`, so `base` and `per_call+sync` must say `per_call` to
/// measure it), then exactly the other keys it turns on, then `sync` always
/// (the default is `watch` since K11b-4, so an arm without the sync must say
/// `off` to keep meaning the old stock: `shared` is the pre-K11b-4 default and
/// `shared+sync` is the stock now).
pub fn arm_overlay_lines(arm: &Arm) -> Vec<String> {
    let mut lines = vec![format!(
        "sessions = \"{}\"",
        if arm.shared { "shared" } else { "per_call" }
    )];
    if arm.outline_cache {
        lines.push("outline_cache = true".to_owned());
    }
    if arm.implementations {
        lines.push("implementations = true".to_owned());
    }
    lines.push(format!(
        "sync = \"{}\"",
        if arm.sync_watch { "watch" } else { "off" }
    ));
    lines
}

/// What an arm's config needs besides the arm.
#[derive(Clone, Debug)]
pub struct ConfigCtx {
    pub data_dir: PathBuf,
    pub scip_index: PathBuf,
    pub rust_analyzer: String,
    pub deadline_ms: u64,
    /// The measured adapter and its server (`--server`, else the default).
    pub adapter: Adapter,
    pub server: String,
    /// `--places outline`: every arm writes `places = "outline"`.
    pub places_outline: bool,
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// The whole config of an arm. The semantic, LSP-search and reranker channels
/// are off so that nothing but `lsp.live` starts a language server or reaches
/// a model service (the search channel would warm a shared manager before the
/// first walk); the model URLs name closed ports so the index builds
/// vectorless (d402) without contacting 8766/8767; freshness is `manual` so a
/// call never rescans the tree.
pub fn arm_config(arm: &Arm, ctx: &ConfigCtx) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "data_dir = '{}'", toml_path(&ctx.data_dir));
    let _ = writeln!(text, "rust_analyzer_path = '{}'", ctx.rust_analyzer);
    text.push_str("lsp_timeout_seconds = 120\n");
    text.push_str("embedding_url = \"http://127.0.0.1:18766/v1\"\n");
    text.push_str("reranker_url = \"http://127.0.0.1:18767/rerank\"\n");
    text.push_str("\n[channels]\nsemantic = false\nlsp = false\nreranker = false\n");
    text.push_str("\n[index]\nfreshness = \"manual\"\n");
    let _ = write!(
        text,
        "\n[traverse]\nenabled = true\nscip_index = '{}'\ndeadline_ms = {}\n",
        toml_path(&ctx.scip_index),
        ctx.deadline_ms
    );
    text.push_str("\n[traverse.policies.explore]\nextra_maps = [\"lsp.live\"]\n");
    if let Some(section) = ctx.adapter.section() {
        let _ = write!(
            text,
            "\n[{section}]\npath = '{}'\n",
            ctx.server.replace('\\', "/")
        );
    }
    let overlay = arm_overlay_lines(arm);
    if !overlay.is_empty() {
        text.push_str("\n[run_profile.lsp]\n");
        for line in overlay {
            text.push_str(&line);
            text.push('\n');
        }
        // Written either way: the default is `auto`, which is not either arm
        // on a tree with no SCIP index.
        text.push_str(if ctx.places_outline {
            "places = \"outline\"\n"
        } else {
            "places = \"scip\"\n"
        });
    }
    text
}

// ---------------------------------------------------------------------------
// Answer parsing (pure)
// ---------------------------------------------------------------------------

/// The `lsp.live` notes of an answer, by kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteKinds {
    pub unavailable: bool,
    pub lsp_deadline: bool,
    pub node_error: bool,
}

/// One traverse call, as the harness records it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CallRecord {
    pub ok: bool,
    pub error: Option<String>,
    pub wall_ms: f64,
    /// `totals.elapsed_ms`: the walk's own clock (the seed search is not in it).
    pub elapsed_ms: f64,
    pub pops: u64,
    pub external_calls: u64,
    pub physical_external_calls: u64,
    pub lsp_expands: u64,
    pub lsp_ms: f64,
    pub other_map_ms: BTreeMap<String, f64>,
    pub stop_reason: String,
    pub seed_source: String,
    pub results: usize,
    /// Result cards that name `lsp.live` in their origin or witnesses.
    pub lsp_cards: usize,
    pub notes: NoteKinds,
    pub lsp_notes: Vec<String>,
}

impl CallRecord {
    /// A call whose answer carried a result but no `lsp.live` edge.
    pub fn fail_open(&self) -> bool {
        self.ok && self.results > 0 && self.lsp_cards == 0
    }

    /// The call hit the walk's own deadline.
    pub fn deadline_hit(&self) -> bool {
        self.stop_reason == "deadline"
    }

    /// `lsp.live` ms per expansion, `None` without an expansion.
    pub fn lsp_ms_per_expansion(&self) -> Option<f64> {
        (self.lsp_expands > 0).then(|| self.lsp_ms / self.lsp_expands as f64)
    }
}

/// Takes the JSON the MCP `tools/call` answer carries: `structuredContent` of
/// an SSE `data:` line or of a bare JSON-RPC body.
pub fn tool_json(body: &str) -> Result<Value> {
    let mut candidates: Vec<&str> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|payload| !payload.is_empty())
        .collect();
    if candidates.is_empty() {
        candidates.push(body.trim());
    }
    for payload in candidates {
        let Ok(envelope) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(structured) = envelope.pointer("/result/structuredContent") {
            return Ok(structured.clone());
        }
        if let Some(error) = envelope.get("error") {
            bail!("MCP error: {error}");
        }
    }
    bail!("no structuredContent in the MCP answer: {body}")
}

fn card_names_lsp(card: &Value) -> bool {
    card.to_string().contains("lsp.live")
}

/// Reads the fields of one traverse answer. A missing `lsp.live` map gives
/// `lsp_ms 0` and no expansions; the notes are classified.
pub fn parse_call_record(answer: &Value, wall_ms: f64) -> CallRecord {
    let totals = &answer["totals"];
    let number = |value: &Value| value.as_f64().unwrap_or(0.0);
    let mut record = CallRecord {
        ok: true,
        wall_ms,
        elapsed_ms: number(&totals["elapsed_ms"]),
        pops: totals["pops"].as_u64().unwrap_or(0),
        external_calls: totals["external_calls"].as_u64().unwrap_or(0),
        physical_external_calls: totals["physical_external_calls"].as_u64().unwrap_or(0),
        stop_reason: answer["stop_reason"].as_str().unwrap_or("").to_owned(),
        seed_source: answer["seed_source"].as_str().unwrap_or("").to_owned(),
        ..CallRecord::default()
    };
    if let Some(per_map) = totals["per_map"].as_object() {
        for (id, stat) in per_map {
            let ms = number(&stat["ms"]);
            if id == "lsp.live" {
                record.lsp_ms = ms;
                record.lsp_expands = stat["expands"].as_u64().unwrap_or(0);
            } else {
                record.other_map_ms.insert(id.clone(), ms);
            }
        }
    }
    if let Some(results) = answer["results"].as_array() {
        record.results = results.len();
        record.lsp_cards = results.iter().filter(|card| card_names_lsp(card)).count();
    }
    if let Some(notes) = answer["notes"].as_array() {
        for note in notes.iter().filter_map(Value::as_str) {
            if !note.starts_with("lsp.live") {
                continue;
            }
            record.lsp_notes.push(note.to_owned());
            if note.contains(NOTE_UNAVAILABLE) {
                record.notes.unavailable = true;
            }
            // A node error carries the deadline text when the deadline was the error.
            if note.contains(NOTE_DEADLINE) {
                record.notes.lsp_deadline = true;
            }
            if note.contains(NOTE_NODE_ERROR) {
                record.notes.node_error = true;
            }
        }
    }
    record
}

/// The probe ms of the lines the server's `debug!` wrote after `offset` bytes
/// of its log, and the new offset.
pub fn probe_ms_since(log: &str, offset: usize) -> (Vec<f64>, usize) {
    let offset = offset.min(log.len());
    let offset = (offset..=log.len())
        .find(|at| log.is_char_boundary(*at))
        .unwrap_or(log.len());
    let found = log[offset..]
        .lines()
        .filter(|line| line.contains("lsp.live version probe"))
        .filter_map(|line| {
            let after = line.split("probe_ms=").nth(1)?;
            let number: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            number.parse().ok()
        })
        .collect();
    (found, log.len())
}

// ---------------------------------------------------------------------------
// Report math (pure)
// ---------------------------------------------------------------------------

/// Nearest-rank percentile (`p` in 0..=100) of `values`; `None` when empty.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * sorted.len() as f64).ceil().max(1.0) as usize;
    Some(sorted[rank.min(sorted.len()) - 1])
}

/// Pops per 5 s of the walk's own time.
pub fn pops_per_5s(pops: u64, elapsed_ms: f64) -> f64 {
    if elapsed_ms <= 0.0 {
        0.0
    } else {
        pops as f64 * 5000.0 / elapsed_ms
    }
}

/// One call with where it sits in its arm.
#[derive(Clone, Debug)]
pub struct Sample {
    /// 1-based position in the arm's call order (1 is the cold call).
    pub seq: usize,
    /// 1-based round over the question list.
    pub round: usize,
    pub record: CallRecord,
    pub probe_ms: Vec<f64>,
    pub rust_analyzer_rss_mb: f64,
    pub server_rss_mb: f64,
    /// Mean CPU use of the whole machine (percent) while the call ran: what
    /// the server, its language server and anything else on the machine took.
    pub machine_cpu_pct: f64,
}

/// One `calls.jsonl` line: a sample with its condition, arm and question.
pub fn call_line(condition: &str, arm: &str, question: &str, sample: &Sample) -> Value {
    let record = &sample.record;
    json!({
        "condition": condition, "arm": arm, "seq": sample.seq, "round": sample.round,
        "question": question, "ok": record.ok, "error": record.error,
        "wall_ms": record.wall_ms, "elapsed_ms": record.elapsed_ms,
        "pops": record.pops, "external_calls": record.external_calls,
        "physical_external_calls": record.physical_external_calls,
        "lsp_expands": record.lsp_expands, "lsp_ms": record.lsp_ms,
        "other_map_ms": record.other_map_ms, "stop_reason": record.stop_reason,
        "seed_source": record.seed_source, "results": record.results,
        "lsp_cards": record.lsp_cards,
        "notes": {
            "unavailable": record.notes.unavailable,
            "lsp_deadline": record.notes.lsp_deadline,
            "node_error": record.notes.node_error,
            "lsp": record.lsp_notes,
        },
        "probe_ms": sample.probe_ms,
        "rust_analyzer_rss_mb": sample.rust_analyzer_rss_mb,
        "server_rss_mb": sample.server_rss_mb,
        "machine_cpu_pct": sample.machine_cpu_pct,
    })
}

/// The inverse of [`call_line`]: `(condition, arm, sample)`.
pub fn sample_from_line(line: &Value) -> Option<(String, String, Sample)> {
    let number = |key: &str| line[key].as_f64().unwrap_or(0.0);
    let count = |key: &str| line[key].as_u64().unwrap_or(0);
    let notes = &line["notes"];
    let record = CallRecord {
        ok: line["ok"].as_bool().unwrap_or(false),
        error: line["error"].as_str().map(str::to_owned),
        wall_ms: number("wall_ms"),
        elapsed_ms: number("elapsed_ms"),
        pops: count("pops"),
        external_calls: count("external_calls"),
        physical_external_calls: count("physical_external_calls"),
        lsp_expands: count("lsp_expands"),
        lsp_ms: number("lsp_ms"),
        other_map_ms: line["other_map_ms"]
            .as_object()
            .map(|map| {
                map.iter()
                    .map(|(id, ms)| (id.clone(), ms.as_f64().unwrap_or(0.0)))
                    .collect()
            })
            .unwrap_or_default(),
        stop_reason: line["stop_reason"].as_str().unwrap_or("").to_owned(),
        seed_source: line["seed_source"].as_str().unwrap_or("").to_owned(),
        results: usize::try_from(count("results")).unwrap_or(0),
        lsp_cards: usize::try_from(count("lsp_cards")).unwrap_or(0),
        notes: NoteKinds {
            unavailable: notes["unavailable"].as_bool().unwrap_or(false),
            lsp_deadline: notes["lsp_deadline"].as_bool().unwrap_or(false),
            node_error: notes["node_error"].as_bool().unwrap_or(false),
        },
        lsp_notes: notes["lsp"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    };
    Some((
        line["condition"].as_str()?.to_owned(),
        line["arm"].as_str()?.to_owned(),
        Sample {
            seq: usize::try_from(count("seq")).ok()?,
            round: usize::try_from(count("round")).ok()?,
            record,
            probe_ms: line["probe_ms"]
                .as_array()
                .map(|list| list.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default(),
            rust_analyzer_rss_mb: number("rust_analyzer_rss_mb"),
            server_rss_mb: number("server_rss_mb"),
            machine_cpu_pct: number("machine_cpu_pct"),
        },
    ))
}

/// An arm's numbers for one condition.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArmSummary {
    pub arm: String,
    pub calls: usize,
    pub failed: usize,
    pub cold_ms_per_exp: Option<f64>,
    pub cold_lsp_ms: f64,
    pub cold_wall_ms: f64,
    pub warm_p50: Option<f64>,
    pub warm_p95: Option<f64>,
    /// Warm calls of round 2 and later (a question asked again).
    pub repeat_p50: Option<f64>,
    pub expansions_per_call: f64,
    pub external_per_call: f64,
    pub external_per_expansion: f64,
    pub pops_per_call: f64,
    pub pops_per_5s: f64,
    pub ra_rss_max_mb: f64,
    pub server_rss_max_mb: f64,
    pub deadline: usize,
    pub lsp_deadline: usize,
    pub unavailable: usize,
    pub fail_open: usize,
    pub probe_p50: Option<f64>,
    pub probe_p95: Option<f64>,
    pub probe_max: Option<f64>,
    pub probes: usize,
    /// Mean machine CPU use (percent) over the calls.
    pub machine_cpu: f64,
}

fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let (sum, n) = values.fold((0.0, 0usize), |(s, n), v| (s + v, n + 1));
    if n == 0 { 0.0 } else { sum / n as f64 }
}

/// Summarises the samples of one arm in one condition.
pub fn summarize(arm: &str, samples: &[Sample]) -> ArmSummary {
    let ok: Vec<&Sample> = samples.iter().filter(|s| s.record.ok).collect();
    let mut summary = ArmSummary {
        arm: arm.to_owned(),
        calls: samples.len(),
        failed: samples.len() - ok.len(),
        ..ArmSummary::default()
    };
    if let Some(first) = samples.first().filter(|s| s.record.ok) {
        summary.cold_ms_per_exp = first.record.lsp_ms_per_expansion();
        summary.cold_lsp_ms = first.record.lsp_ms;
        summary.cold_wall_ms = first.record.wall_ms;
    }
    let warm: Vec<&&Sample> = ok.iter().filter(|s| s.seq > 1).collect();
    let per_exp = |filter: &dyn Fn(&&&Sample) -> bool| -> Vec<f64> {
        warm.iter()
            .filter(|s| filter(s))
            .filter_map(|s| s.record.lsp_ms_per_expansion())
            .collect()
    };
    let all_warm = per_exp(&|_| true);
    summary.warm_p50 = percentile(&all_warm, 50.0);
    summary.warm_p95 = percentile(&all_warm, 95.0);
    summary.repeat_p50 = percentile(&per_exp(&|s| s.round >= 2), 50.0);
    summary.expansions_per_call = mean(ok.iter().map(|s| s.record.lsp_expands as f64));
    summary.external_per_call = mean(ok.iter().map(|s| s.record.external_calls as f64));
    let (expands, external): (u64, u64) = ok.iter().fold((0, 0), |(e, x), s| {
        (e + s.record.lsp_expands, x + s.record.external_calls)
    });
    summary.external_per_expansion = if expands == 0 {
        0.0
    } else {
        external as f64 / expands as f64
    };
    summary.pops_per_call = mean(ok.iter().map(|s| s.record.pops as f64));
    let (pops, elapsed) = ok.iter().fold((0u64, 0.0f64), |(p, e), s| {
        (p + s.record.pops, e + s.record.elapsed_ms)
    });
    summary.pops_per_5s = pops_per_5s(pops, elapsed);
    summary.ra_rss_max_mb = samples
        .iter()
        .map(|s| s.rust_analyzer_rss_mb)
        .fold(0.0, f64::max);
    summary.server_rss_max_mb = samples.iter().map(|s| s.server_rss_mb).fold(0.0, f64::max);
    summary.deadline = ok.iter().filter(|s| s.record.deadline_hit()).count();
    summary.lsp_deadline = ok.iter().filter(|s| s.record.notes.lsp_deadline).count();
    summary.unavailable = ok.iter().filter(|s| s.record.notes.unavailable).count();
    summary.fail_open = ok.iter().filter(|s| s.record.fail_open()).count();
    let probes: Vec<f64> = samples.iter().flat_map(|s| s.probe_ms.clone()).collect();
    summary.probes = probes.len();
    summary.machine_cpu = mean(samples.iter().map(|s| s.machine_cpu_pct));
    summary.probe_p50 = percentile(&probes, 50.0);
    summary.probe_p95 = percentile(&probes, 95.0);
    summary.probe_max = percentile(&probes, 100.0);
    summary
}

fn fmt_opt(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.1}"))
}

fn fmt_diff(value: Option<f64>, base: Option<f64>) -> String {
    match (value, base) {
        (Some(v), Some(b)) => format!("{:+.1}", v - b),
        _ => "-".to_owned(),
    }
}

/// One markdown table: a row per arm, the `diff` columns against the arm named
/// `base` (when present among the rows).
pub fn report_table(rows: &[ArmSummary]) -> String {
    let base = rows.iter().find(|row| row.arm == "base");
    let mut out = String::new();
    out.push_str(
        "| arm | calls (failed) | cold lsp ms/exp | warm p50 | warm p95 | diff p50 vs base | repeat p50 | exp/call | ext/call | ext/exp | pops/call | pops per 5 s | server RSS MB | serve RSS MB | deadline | lsp deadline | unavailable | fail-open | probe ms p50/p95/max (n) |\n",
    );
    out.push_str(
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
    );
    for row in rows {
        let _ = writeln!(
            out,
            "| {} | {} ({}) | {} | {} | {} | {} | {} | {:.1} | {:.1} | {:.2} | {:.1} | {:.1} | {:.0} | {:.0} | {} | {} | {} | {} | {}/{}/{} ({}) | {:.0} |",
            row.arm,
            row.calls,
            row.failed,
            fmt_opt(row.cold_ms_per_exp),
            fmt_opt(row.warm_p50),
            fmt_opt(row.warm_p95),
            if row.arm == "base" {
                "0.0".to_owned()
            } else {
                fmt_diff(row.warm_p50, base.and_then(|b| b.warm_p50))
            },
            fmt_opt(row.repeat_p50),
            row.expansions_per_call,
            row.external_per_call,
            row.external_per_expansion,
            row.pops_per_call,
            row.pops_per_5s,
            row.ra_rss_max_mb,
            row.server_rss_max_mb,
            row.deadline,
            row.lsp_deadline,
            row.unavailable,
            row.fail_open,
            fmt_opt(row.probe_p50),
            fmt_opt(row.probe_p95),
            fmt_opt(row.probe_max),
            row.probes,
            row.machine_cpu,
        );
    }
    out
}

/// The N8 window: the first `window` calls of an arm, one row per call. A call
/// is "dark" when it carries the deadline or the unavailable note.
pub fn n8_table(samples: &[Sample], window: usize) -> (String, usize, usize) {
    let mut out = String::from(
        "| # | wall ms | lsp ms | expands | stop | lsp notes | dark |\n| --- | --- | --- | --- | --- | --- | --- |\n",
    );
    let (mut dark, mut longest, mut run) = (0usize, 0usize, 0usize);
    for sample in samples.iter().take(window) {
        let record = &sample.record;
        let is_dark = !record.ok
            || record.notes.lsp_deadline
            || record.notes.unavailable
            || record.deadline_hit();
        if is_dark {
            dark += 1;
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
        let notes = record
            .lsp_notes
            .iter()
            .map(|note| note.chars().take(80).collect::<String>())
            .collect::<Vec<_>>()
            .join("; ");
        let _ = writeln!(
            out,
            "| {} | {:.0} | {:.0} | {} | {} | {} | {} |",
            sample.seq,
            record.wall_ms,
            record.lsp_ms,
            record.lsp_expands,
            record.stop_reason,
            notes.replace('|', "/"),
            if is_dark { "dark" } else { "" }
        );
    }
    (out, dark, longest)
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/// UTC `YYYYMMDDTHHMMSSZ` of now.
pub fn utc_stamp() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    stamp_of(seconds)
}

fn stamp_of(seconds: u64) -> String {
    let days = (seconds / 86_400) as i64;
    let rest = seconds % 86_400;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Busy threads: CPU only, no disk traffic, until dropped.
struct CpuLoad {
    stop: Arc<AtomicBool>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl CpuLoad {
    fn start(threads: usize) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let handles = (0..threads)
            .map(|index| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    let mut x: u64 = 0x9E37_79B9_7F4A_7C15 ^ index as u64;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..200_000 {
                            x ^= x << 13;
                            x ^= x >> 7;
                            x ^= x << 17;
                        }
                        std::hint::black_box(x);
                    }
                })
            })
            .collect();
        Self { stop, handles }
    }
}

impl Drop for CpuLoad {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

fn kill_tree(pid: u32) {
    let mut system = System::new_all();
    system.refresh_all();
    for descendant in descendants_of(&system, pid) {
        if let Some(process) = system.process(Pid::from_u32(descendant)) {
            process.kill();
        }
    }
    if let Some(process) = system.process(Pid::from_u32(pid)) {
        process.kill();
    }
}

/// A `serve` process of the harness; its whole tree dies with the guard.
struct Server {
    child: Child,
    pid: u32,
    log_path: PathBuf,
}

impl Server {
    fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    async fn stop(&mut self) {
        kill_tree(self.pid);
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        kill_tree(self.pid);
    }
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

async fn wait_port_free(port: u16) -> Result<()> {
    for _ in 0..40 {
        if port_free(port) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("port {port} did not become free")
}

async fn spawn_server(binary: &Path, config: &Path, dir: &Path) -> Result<Server> {
    debug_assert!(!FORBIDDEN_PORTS.contains(&PORT));
    if !port_free(PORT) {
        bail!("port {PORT} is already in use");
    }
    std::fs::create_dir_all(dir)?;
    let log_path = dir.join("serve.stderr.log");
    let stdout = std::fs::File::create(dir.join("serve.stdout.log"))?;
    let stderr = std::fs::File::create(&log_path)?;
    let mut command = Command::new(binary);
    strip_lci_env(&mut command);
    let child = command
        .arg("--config")
        .arg(config)
        .arg("serve")
        .arg("--port")
        .arg(PORT.to_string())
        .current_dir(dir)
        .env("RUST_LOG", "local_code_intelligence=info,lci_core=info,local_code_intelligence::app::traverse=debug")
        .env("NO_COLOR", "1")
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true)
        .spawn()
        .context("start serve")?;
    let pid = child.id().context("serve has no pid")?;
    Ok(Server {
        child,
        pid,
        log_path,
    })
}

/// RSS in MB of the server (`serve`) and of every descendant whose name
/// contains one of `names` (the adapter's language server).
fn sample_rss(system: &mut System, server_pid: u32, names: &[&str]) -> (f64, f64) {
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_memory(),
    );
    let mb = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    let server = system
        .process(Pid::from_u32(server_pid))
        .map_or(0.0, |p| mb(p.memory()));
    let ra = descendants_of(system, server_pid)
        .into_iter()
        .filter_map(|pid| system.process(Pid::from_u32(pid)))
        .filter(|p| rss_name_matches(&p.name().to_string_lossy(), names))
        .map(|p| mb(p.memory()))
        .sum();
    (ra, server)
}

/// What [`RssSampler`] saw since its last [`RssSampler::take`].
#[derive(Clone, Copy, Debug, Default)]
struct SamplerPeak {
    ra_mb: f64,
    server_mb: f64,
    cpu_sum: f64,
    cpu_n: u32,
}

/// A thread that samples [`sample_rss`] and the machine's CPU use every 250 ms
/// and keeps the peaks and the CPU mean since the last [`RssSampler::take`], so
/// that a `per_call` server (gone when the call returns) is still seen.
struct RssSampler {
    stop: Arc<AtomicBool>,
    peak: Arc<std::sync::Mutex<SamplerPeak>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl RssSampler {
    fn start(server_pid: u32, names: &'static [&'static str]) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(std::sync::Mutex::new(SamplerPeak::default()));
        let (stop_flag, peak_cell) = (Arc::clone(&stop), Arc::clone(&peak));
        let handle = std::thread::spawn(move || {
            let mut system = System::new();
            system.refresh_cpu_usage();
            while !stop_flag.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let (ra, server) = sample_rss(&mut system, server_pid, names);
                system.refresh_cpu_usage();
                let cpu = f64::from(system.global_cpu_usage());
                if let Ok(mut peak) = peak_cell.lock() {
                    peak.ra_mb = peak.ra_mb.max(ra);
                    peak.server_mb = peak.server_mb.max(server);
                    peak.cpu_sum += cpu;
                    peak.cpu_n += 1;
                }
            }
        });
        Self {
            stop,
            peak,
            handle: Some(handle),
        }
    }

    /// (language-server family MB peak, `serve` MB peak, mean machine CPU percent)
    /// since the last call, then reset.
    fn take(&self) -> (f64, f64, f64) {
        let peak = self
            .peak
            .lock()
            .map_or_else(|_| SamplerPeak::default(), |mut p| std::mem::take(&mut *p));
        let cpu = if peak.cpu_n == 0 {
            0.0
        } else {
            peak.cpu_sum / f64::from(peak.cpu_n)
        };
        (peak.ra_mb, peak.server_mb, cpu)
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The machine's CPU use (percent) over `window`.
fn machine_cpu_over(window: Duration) -> f64 {
    let mut system = System::new();
    system.refresh_cpu_usage();
    std::thread::sleep(window);
    system.refresh_cpu_usage();
    f64::from(system.global_cpu_usage())
}

/// Waits (up to `max_wait`) until the machine's CPU use over 10 s is below
/// `threshold` percent, with nothing of the harness running. Returns the last
/// reading and whether it is below the threshold.
async fn wait_for_quiet(label: &str, threshold: f64, max_wait: Duration) -> (f64, bool) {
    let started = Instant::now();
    loop {
        let cpu = tokio::task::spawn_blocking(|| machine_cpu_over(Duration::from_secs(10)))
            .await
            .unwrap_or(100.0);
        if cpu < threshold {
            return (cpu, true);
        }
        if started.elapsed() >= max_wait {
            println!(
                "[{label}] machine CPU {cpu:.0}% never fell below {threshold:.0}%; going on, flagged"
            );
            return (cpu, false);
        }
        println!("[{label}] machine CPU {cpu:.0}% (something else is running); waiting");
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
}

/// The program to run for `server`: a path as given, a name resolved through
/// PATH (so npm `.cmd` shims work on Windows), else the name itself.
fn probe_program(server: &str) -> String {
    if Path::new(server).components().count() > 1 {
        return server.to_owned();
    }
    crate::acceptance::which(server).map_or_else(
        || server.to_owned(),
        |path| path.to_string_lossy().into_owned(),
    )
}

/// Times `<server> --version` `runs` times, ms.
fn time_version_probe(binary: &str, runs: usize) -> Vec<f64> {
    (0..runs)
        .filter_map(|_| {
            let started = Instant::now();
            let status = std::process::Command::new(binary)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .ok()?;
            status
                .success()
                .then(|| started.elapsed().as_secs_f64() * 1000.0)
        })
        .collect()
}

fn which_on_path(name: &str) -> bool {
    crate::acceptance::which(name).is_some()
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Question {
    id: String,
    question: String,
}

fn parse_questions(text: &str) -> Result<Vec<Question>> {
    let value: Value = serde_json::from_str(text).context("questions JSON")?;
    let list = value.as_array().context("questions: a JSON list")?;
    list.iter()
        .map(|item| {
            Ok(Question {
                id: item["id"].as_str().context("question id")?.to_owned(),
                question: item["question"]
                    .as_str()
                    .context("question text")?
                    .to_owned(),
            })
        })
        .collect()
}

struct RunContext {
    binary: PathBuf,
    tree: PathBuf,
    questions: Vec<Question>,
    repeat: usize,
    deadline_ms: u64,
    out: PathBuf,
    work: PathBuf,
    scip: PathBuf,
    rust_analyzer: String,
    adapter: Adapter,
    server: String,
    places_outline: bool,
    calls_file: std::fs::File,
}

fn mcp_args(question: &str, tree: &Path, deadline_ms: u64) -> Value {
    json!({
        "workspace_path": tree.to_string_lossy(),
        "query": question,
        "mode": "traverse",
        "budget": {"deadline_ms": deadline_ms},
    })
}

async fn run_arm(ctx: &mut RunContext, arm: &Arm, condition: &str) -> Result<Vec<Sample>> {
    let tag = format!("{condition}-{}", arm.name.replace('+', "_"));
    let data_dir = ctx.work.join(format!("data-{tag}"));
    let _ = std::fs::remove_dir_all(&data_dir);
    let arm_dir = ctx.out.join("arms").join(&tag);
    std::fs::create_dir_all(&arm_dir)?;
    let config_path = arm_dir.join("config.toml");
    std::fs::write(
        &config_path,
        arm_config(
            arm,
            &ConfigCtx {
                data_dir: data_dir.clone(),
                scip_index: ctx.scip.clone(),
                rust_analyzer: ctx.rust_analyzer.clone(),
                deadline_ms: ctx.deadline_ms,
                adapter: ctx.adapter,
                server: ctx.server.clone(),
                places_outline: ctx.places_outline,
            },
        ),
    )?;
    let mut server = spawn_server(&ctx.binary, &config_path, &arm_dir).await?;
    let base = format!("http://127.0.0.1:{PORT}");
    let sampler = RssSampler::start(server.pid, ctx.adapter.rss_names());
    let outcome = run_arm_calls(ctx, arm, condition, &tag, &base, &mut server, &sampler).await;
    drop(sampler);
    server.stop().await;
    let _ = std::fs::remove_dir_all(&data_dir);
    wait_port_free(PORT).await?;
    outcome
}

async fn run_arm_calls(
    ctx: &mut RunContext,
    arm: &Arm,
    condition: &str,
    tag: &str,
    base: &str,
    server: &mut Server,
    sampler: &RssSampler,
) -> Result<Vec<Sample>> {
    let healthy = wait_http_ok(
        &format!("{base}/health"),
        60,
        Duration::from_millis(500),
        || server.has_exited(),
    )
    .await;
    if !healthy {
        bail!("/health did not answer; see {}", server.log_path.display());
    }
    let mut mcp = McpSession::connect(base).await?;
    let tree = ctx.tree.to_string_lossy().into_owned();
    let started = Instant::now();
    let indexed = mcp
        .call_tool("index_workspace", json!({"workspace_path": tree}))
        .await
        .context("index_workspace over MCP")?;
    let index_json = tool_json(&indexed).unwrap_or(Value::Null);
    println!(
        "[{tag}] indexed in {:.1} s, vectors_missing={}",
        started.elapsed().as_secs_f64(),
        index_json["vectors_missing"]
    );
    let raw_dir = ctx.out.join("raw").join(tag);
    std::fs::create_dir_all(&raw_dir)?;
    let mut samples = Vec::new();
    let mut log_offset = std::fs::read_to_string(&server.log_path).map_or(0, |t| t.len());
    let mut seq = 0usize;
    for round in 1..=ctx.repeat {
        for question in ctx.questions.clone() {
            seq += 1;
            let args = mcp_args(&question.question, &ctx.tree, ctx.deadline_ms);
            let _ = sampler.take();
            let call_started = Instant::now();
            let reply = mcp.call_tool("search_code", args).await;
            let wall_ms = call_started.elapsed().as_secs_f64() * 1000.0;
            let record = match reply
                .as_ref()
                .map_err(|e| anyhow!("{e:#}"))
                .and_then(|body| {
                    let answer = tool_json(body)?;
                    std::fs::write(
                        raw_dir.join(format!("{seq:03}-{}.json", question.id)),
                        serde_json::to_string_pretty(&answer)?,
                    )?;
                    Ok(parse_call_record(&answer, wall_ms))
                }) {
                Ok(record) => record,
                Err(error) => CallRecord {
                    ok: false,
                    error: Some(format!("{error:#}")),
                    wall_ms,
                    ..CallRecord::default()
                },
            };
            let (ra_rss, server_rss, machine_cpu) = sampler.take();
            let log_text = std::fs::read_to_string(&server.log_path).unwrap_or_default();
            let (probe_ms, next_offset) = probe_ms_since(&log_text, log_offset);
            log_offset = next_offset;
            // The server's own view of the analyzers, once the call is over.
            let status = mcp
                .call_tool("index_status", json!({"workspace_path": tree}))
                .await
                .ok()
                .and_then(|body| tool_json(&body).ok())
                .unwrap_or(Value::Null);
            let sample = Sample {
                seq,
                round,
                record,
                probe_ms,
                rust_analyzer_rss_mb: ra_rss,
                server_rss_mb: server_rss,
                machine_cpu_pct: machine_cpu,
            };
            let mut line = call_line(condition, arm.name, &question.id, &sample);
            if let Some(object) = line.as_object_mut() {
                object.insert(
                    "analyzers_running".into(),
                    status["analyzers_running"].clone(),
                );
                object.insert("analyzers".into(), status["analyzers"].clone());
            }
            writeln!(ctx.calls_file, "{line}")?;
            ctx.calls_file.flush()?;
            let record = &sample.record;
            println!(
                "[{tag}] #{seq} {} wall {:.0} ms, lsp {:.0} ms / {} exp, pops {}, ext {}, stop {}, cpu {:.0}%{}",
                question.id,
                record.wall_ms,
                record.lsp_ms,
                record.lsp_expands,
                record.pops,
                record.external_calls,
                record.stop_reason,
                sample.machine_cpu_pct,
                record.error.as_deref().map_or(String::new(), |e| format!(
                    " ERROR {}",
                    e.chars().take(120).collect::<String>()
                )),
            );
            samples.push(sample);
        }
    }
    Ok(samples)
}

fn git_head(tree: &Path) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(tree)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map_or_else(
            || "unknown".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )
}

fn command_text(program: &str, arg: &str) -> String {
    std::process::Command::new(program)
        .arg(arg)
        .output()
        .ok()
        .map_or_else(
            || "unavailable".to_owned(),
            |o| String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        )
}

/// One condition's numbers: per arm the samples (or why the arm has none).
#[derive(Clone, Debug, Default)]
struct ConditionResult {
    condition: String,
    threads: Option<usize>,
    version_probe: Vec<f64>,
    arms: Vec<(Arm, std::result::Result<Vec<Sample>, String>)>,
    /// Machine CPU use (percent) just before each arm's server started, and
    /// whether it was under the quiet threshold.
    idle_cpu: BTreeMap<String, (f64, bool)>,
}

/// The part of a [`ConditionResult`] that `calls.jsonl` does not hold.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CondMeta {
    pub condition: String,
    pub threads: Option<usize>,
    pub version_probe: Vec<f64>,
    pub idle_cpu: BTreeMap<String, (f64, bool)>,
    pub arm_errors: BTreeMap<String, String>,
}

pub fn cond_meta_json(meta: &CondMeta) -> Value {
    json!({
        "condition": meta.condition,
        "threads": meta.threads,
        "version_probe": meta.version_probe,
        "idle_cpu": meta.idle_cpu.iter().map(|(arm, (pct, ok))| (arm.clone(), json!([pct, ok]))).collect::<serde_json::Map<_, _>>(),
        "arm_errors": meta.arm_errors,
    })
}

pub fn cond_meta_from_json(value: &Value) -> Option<CondMeta> {
    Some(CondMeta {
        condition: value["condition"].as_str()?.to_owned(),
        threads: value["threads"]
            .as_u64()
            .and_then(|t| usize::try_from(t).ok()),
        version_probe: value["version_probe"]
            .as_array()
            .map(|list| list.iter().filter_map(Value::as_f64).collect())
            .unwrap_or_default(),
        idle_cpu: value["idle_cpu"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(arm, pair)| {
                        Some((arm.clone(), (pair[0].as_f64()?, pair[1].as_bool()?)))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        arm_errors: value["arm_errors"]
            .as_object()
            .map(|map| {
                map.iter()
                    .filter_map(|(arm, text)| Some((arm.clone(), text.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

impl ConditionResult {
    fn meta(&self) -> CondMeta {
        CondMeta {
            condition: self.condition.clone(),
            threads: self.threads,
            version_probe: self.version_probe.clone(),
            idle_cpu: self.idle_cpu.clone(),
            arm_errors: self
                .arms
                .iter()
                .filter_map(|(arm, outcome)| {
                    outcome
                        .as_ref()
                        .err()
                        .map(|error| (arm.name.to_owned(), error.clone()))
                })
                .collect(),
        }
    }
}

/// What one run directory holds.
pub struct RunData {
    pub conditions: Vec<CondMeta>,
    pub samples: BTreeMap<(String, String), Vec<Sample>>,
}

/// The samples of `calls.jsonl` text, grouped by (condition, arm) in call order.
pub fn group_samples(calls: &str) -> BTreeMap<(String, String), Vec<Sample>> {
    let mut grouped: BTreeMap<(String, String), Vec<Sample>> = BTreeMap::new();
    for line in calls.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some((condition, arm, sample)) = sample_from_line(&value) {
            grouped.entry((condition, arm)).or_default().push(sample);
        }
    }
    grouped
}

/// Merges run directories in order: an arm's samples of a later run replace
/// those of an earlier one (the earlier arm was contaminated or cut short); the
/// version-probe timings of a condition are pooled.
fn merge_runs(runs: &[RunData]) -> Vec<ConditionResult> {
    let mut results: Vec<ConditionResult> = Vec::new();
    for run in runs {
        for meta in &run.conditions {
            let index = results
                .iter()
                .position(|r| r.condition == meta.condition)
                .unwrap_or_else(|| {
                    results.push(ConditionResult {
                        condition: meta.condition.clone(),
                        ..ConditionResult::default()
                    });
                    results.len() - 1
                });
            let result = &mut results[index];
            result.threads = result.threads.or(meta.threads);
            result.version_probe.extend(meta.version_probe.iter());
            for (arm, idle) in &meta.idle_cpu {
                result.idle_cpu.insert(arm.clone(), *idle);
            }
        }
        for ((condition, arm_name), samples) in &run.samples {
            let Some(arm) = ARMS.iter().find(|a| a.name == arm_name) else {
                continue;
            };
            let Some(result) = results.iter_mut().find(|r| &r.condition == condition) else {
                continue;
            };
            result.arms.retain(|(a, _)| a.name != arm_name);
            result.arms.push((*arm, Ok(samples.clone())));
        }
        for meta in &run.conditions {
            for (arm_name, error) in &meta.arm_errors {
                let Some(arm) = ARMS.iter().find(|a| a.name == arm_name) else {
                    continue;
                };
                if let Some(result) = results.iter_mut().find(|r| r.condition == meta.condition)
                    && !result.arms.iter().any(|(a, _)| a.name == arm_name)
                {
                    result.arms.push((*arm, Err(error.clone())));
                }
            }
        }
    }
    let order = |name: &str| {
        ARMS.iter()
            .position(|a| a.name == name)
            .unwrap_or(usize::MAX)
    };
    for result in &mut results {
        result.arms.sort_by_key(|(arm, _)| order(arm.name));
    }
    results.sort_by_key(|r| usize::from(r.condition != "quiet"));
    results
}

/// What the report header says.
#[derive(Clone, Debug, Default)]
struct ReportMeta {
    stamp: String,
    summary: Vec<(String, String)>,
    repeat: usize,
    deadline_ms: u64,
    n8_window: usize,
    skipped_servers: Vec<String>,
    /// Free lines under "Setup" (which run directory an arm came from).
    notes: Vec<String>,
}

fn report_meta_json(meta: &ReportMeta) -> Value {
    json!({
        "stamp": meta.stamp,
        "summary": meta.summary,
        "repeat": meta.repeat,
        "deadline_ms": meta.deadline_ms,
        "n8_window": meta.n8_window,
        "skipped_servers": meta.skipped_servers,
        "notes": meta.notes,
    })
}

fn report_meta_from_json(value: &Value) -> ReportMeta {
    ReportMeta {
        stamp: value["stamp"].as_str().unwrap_or("").to_owned(),
        summary: value["summary"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(|pair| {
                        Some((pair[0].as_str()?.to_owned(), pair[1].as_str()?.to_owned()))
                    })
                    .collect()
            })
            .unwrap_or_default(),
        repeat: usize::try_from(value["repeat"].as_u64().unwrap_or(0)).unwrap_or(0),
        deadline_ms: value["deadline_ms"].as_u64().unwrap_or(0),
        n8_window: usize::try_from(value["n8_window"].as_u64().unwrap_or(30)).unwrap_or(30),
        skipped_servers: value["skipped_servers"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        notes: value["notes"]
            .as_array()
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// For each arm with calls on the deadline: how many ended before it and what
/// those did, so that a bimodal arm is visible next to its row.
fn deadline_split_lines(arms: &[(Arm, std::result::Result<Vec<Sample>, String>)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (arm, outcome) in arms {
        let Ok(samples) = outcome else { continue };
        let ok: Vec<&Sample> = samples.iter().filter(|s| s.record.ok).collect();
        let (finished, on_deadline): (Vec<&Sample>, Vec<&Sample>) =
            ok.iter().partition(|s| !s.record.deadline_hit());
        // Only a real split: each group at least a quarter of the calls.
        if on_deadline.len() * 4 < ok.len() || finished.len() * 4 < ok.len() {
            continue;
        }
        let med = |group: &[&Sample], f: &dyn Fn(&Sample) -> f64| {
            percentile(&group.iter().map(|s| f(s)).collect::<Vec<_>>(), 50.0).unwrap_or(0.0)
        };
        let rss = |group: &[&Sample]| med(group, &|s| s.rust_analyzer_rss_mb);
        lines.push(format!(
            "`{}`: {} of {} calls ended before the deadline (median {:.0} expansions, walk {:.1} s, server {:.0} MB); {} ended on it (median {:.0} expansions, walk {:.1} s, server {:.0} MB). Read the row above with the two groups in mind.",
            arm.name,
            finished.len(),
            ok.len(),
            med(&finished, &|s| s.record.lsp_expands as f64),
            med(&finished, &|s| s.record.elapsed_ms) / 1000.0,
            rss(&finished),
            on_deadline.len(),
            med(&on_deadline, &|s| s.record.lsp_expands as f64),
            med(&on_deadline, &|s| s.record.elapsed_ms) / 1000.0,
            rss(&on_deadline),
        ));
    }
    lines
}

fn render_report(meta: &ReportMeta, results: &[ConditionResult]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# LSP cost report {}\n", meta.stamp);
    out.push_str("Generated by `cargo xtask maps lsp-cost` (K11d-1). Wall time is reported only for the cold call, the probe and the pops-per-5-s row; arms are compared on expansions, external calls and ms per expansion.\n\n## Setup\n\n");
    for (key, value) in &meta.summary {
        let _ = writeln!(out, "- {key}: {value}");
    }
    let questions = meta
        .summary
        .iter()
        .find(|(k, _)| k == "questions")
        .map_or("?", |(_, v)| v.as_str());
    let rounds: Vec<String> = results
        .iter()
        .map(|result| {
            let most = result
                .arms
                .iter()
                .filter_map(|(_, outcome)| outcome.as_ref().ok())
                .flat_map(|samples| samples.iter().map(|s| s.round))
                .max()
                .unwrap_or(0);
            format!("{} {most} rounds", result.condition)
        })
        .collect();
    let _ = writeln!(
        out,
        "- calls per arm: {questions} questions x rounds ({}; the calls column of each row is the count); request deadline {} ms",
        if rounds.is_empty() {
            format!("{} rounds asked", meta.repeat)
        } else {
            rounds.join(", ")
        },
        meta.deadline_ms
    );
    for note in &meta.notes {
        let _ = writeln!(out, "- {note}");
    }
    out.push_str("\nMethod: `serve` on 127.0.0.1:8769 per arm (copied binary, own data dir, vectorless index, semantic/LSP-search/reranker channels off, freshness manual). `lsp.live` joins the walk through `[traverse.policies.explore] extra_maps`. Cold = first call of the arm; warm = every later call; `repeat` = warm calls of round 2 and later (a question asked again). Under `per_call` every call starts its own server, so its cold and warm figures are both cold starts (`base` and `per_call+sync` write `sessions = \"per_call\"`, since the default is `shared`). `ms/exp` = `totals.per_map[\"lsp.live\"].ms / expands`. `pops per 5 s` = total pops / total walk `elapsed_ms` x 5000. server RSS = peak sum of RSS of the descendants of `serve` named by the adapter (rust-analyzer; `node` for pyright and typescript-language-server; gopls; clangd; csharp-ls; `java` for jdtls), max over calls; serve RSS = the `serve` process itself. `fail-open` = calls with a result but no result card naming `lsp.live`. `deadline` = stop_reason deadline; `lsp deadline` = the `lsp.live` deadline note; `unavailable` = the `lsp.live is unavailable` note. `machine CPU %` = mean CPU use of the whole machine while the calls ran (the server and its language server included); `idle CPU` below = the machine's CPU use over 10 s just before the arm's server started, a reading of whatever else runs: above 20% the quiet condition is not quiet. The version probe is memoised for the server's process (d470), so only the first probe of a binary logs a real ms and every arm's later calls log about 0: the older comparison of one probe per call under `base` and one under `shared` no longer applies.\n");
    for result in results {
        let _ = writeln!(
            out,
            "\n## Condition `{}`{}\n",
            result.condition,
            result.threads.map_or(String::new(), |t| format!(
                " ({t} busy CPU threads, no disk load)"
            ))
        );
        if result.version_probe.is_empty() {
            out.push_str("`<server> --version`: no timing (the adapter has no `--version` flag, or the run did not time it).\n\n");
        } else {
            let _ = writeln!(
                out,
                "`<server> --version`, {} runs: p50 {} ms, p95 {} ms, max {} ms (limit 5000 ms).\n",
                result.version_probe.len(),
                fmt_opt(percentile(&result.version_probe, 50.0)),
                fmt_opt(percentile(&result.version_probe, 95.0)),
                fmt_opt(percentile(&result.version_probe, 100.0)),
            );
        }
        let rows: Vec<ArmSummary> = result
            .arms
            .iter()
            .filter_map(|(arm, outcome)| outcome.as_ref().ok().map(|s| summarize(arm.name, s)))
            .collect();
        out.push_str(&report_table(&rows));
        let mut names: Vec<&str> = result
            .arms
            .iter()
            .map(|(arm, _)| arm.name)
            .filter(|name| result.idle_cpu.contains_key(*name))
            .collect();
        let extra: Vec<&str> = result
            .idle_cpu
            .keys()
            .map(String::as_str)
            .filter(|name| !names.contains(name))
            .collect();
        names.extend(extra);
        let idle: Vec<String> = names
            .iter()
            .filter_map(|name| {
                result.idle_cpu.get(*name).map(|(pct, ok)| {
                    format!("{name} {pct:.0}%{}", if *ok { "" } else { " (not quiet)" })
                })
            })
            .collect();
        if !idle.is_empty() {
            let _ = writeln!(
                out,
                "\nidle CPU (machine, before the arm or the synthetic load started; above 20% something else was running): {}.",
                idle.join(", ")
            );
        }
        for line in deadline_split_lines(&result.arms) {
            let _ = writeln!(out, "\n{line}");
        }
        for (arm, outcome) in &result.arms {
            if let Err(error) = outcome {
                let _ = writeln!(out, "\nArm `{}` skipped or failed: {error}", arm.name);
            }
        }
        if let Some((_, Ok(samples))) = result.arms.iter().find(|(arm, _)| arm.name == "shared") {
            let (table, dark, longest) = n8_table(samples, meta.n8_window);
            let _ = writeln!(
                out,
                "\n### N8 window, `shared`, first {} calls (back to back)\n\nDark calls (deadline or unavailable note, or a failed call): {dark}, longest run of consecutive dark calls: {longest}. The server's `analyzers` block (K7b-3a) is not in this build, so the state of the analyzer at each dark call is not known: needs K7b-3a. The `lsp` notes column is what the answer said.\n",
                meta.n8_window
            );
            out.push_str(&table);
        }
    }
    out.push_str("\n## Skipped\n\n");
    if meta.skipped_servers.is_empty() {
        out.push_str("- no language server was found missing from PATH at the start of the run (a non-rust `--adapter` run checks none: its server is the one it was given)\n");
    }
    for server in &meta.skipped_servers {
        let _ = writeln!(
            out,
            "- `{server}` is not on PATH: not measured here (K11d-1 walks this Rust tree; per-adapter numbers are K11d-2)"
        );
    }
    out.push_str("- the N8 analyzer state per dark call: needs K7b-3a (`Status.analyzers`)\n");
    out
}

/// Writes `conditions.json`, `meta.json` and `report.md` of `out`.
fn write_report_files(out: &Path, meta: &ReportMeta, results: &[ConditionResult]) -> Result<()> {
    let conditions: Vec<Value> = results.iter().map(|r| cond_meta_json(&r.meta())).collect();
    std::fs::write(
        out.join("conditions.json"),
        serde_json::to_string_pretty(&conditions)?,
    )?;
    std::fs::write(
        out.join("meta.json"),
        serde_json::to_string_pretty(&report_meta_json(meta))?,
    )?;
    std::fs::write(out.join("report.md"), render_report(meta, results))?;
    Ok(())
}

fn read_run_dir(dir: &Path) -> Result<(RunData, ReportMeta)> {
    let conditions: Vec<CondMeta> = serde_json::from_str::<Value>(
        &std::fs::read_to_string(dir.join("conditions.json"))
            .with_context(|| format!("read {}/conditions.json", dir.display()))?,
    )?
    .as_array()
    .context("conditions.json: a list")?
    .iter()
    .filter_map(cond_meta_from_json)
    .collect();
    let calls = std::fs::read_to_string(dir.join("calls.jsonl"))
        .with_context(|| format!("read {}/calls.jsonl", dir.display()))?;
    let meta = report_meta_from_json(&serde_json::from_str(&std::fs::read_to_string(
        dir.join("meta.json"),
    )?)?);
    Ok((
        RunData {
            conditions,
            samples: group_samples(&calls),
        },
        meta,
    ))
}

pub fn run(args: LspCostArgs) -> Result<()> {
    // `xtask`'s main already runs a multi-thread runtime; `maps::run` is sync.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(run_async(args)))
}

/// `--report-from`: renders one report from the run directories given.
fn report_from(dirs: &[PathBuf], out: Option<&Path>) -> Result<()> {
    let mut loaded = Vec::new();
    for dir in dirs {
        loaded.push((dir, read_run_dir(dir)?));
    }
    // Arms are merged by name, so runs of different adapters (each has its own
    // `base`) must not be merged into one another: one report per adapter.
    let keys: Vec<String> = loaded
        .iter()
        .map(|(_, (_, meta))| adapter_key(meta))
        .collect();
    let groups = group_indices_by_key(&keys);
    let mut sections = Vec::new();
    let mut slots: Vec<Option<(&PathBuf, (RunData, ReportMeta))>> =
        loaded.into_iter().map(Some).collect();
    for (key, indices) in &groups {
        let members: Vec<(&PathBuf, (RunData, ReportMeta))> =
            indices.iter().filter_map(|i| slots[*i].take()).collect();
        let mut report = merged_report(members);
        if groups.len() > 1 {
            report = report.replacen(" (merged)\n", &format!(" (merged, adapter {key})\n"), 1);
        }
        sections.push(report);
    }
    let out = out
        .map(Path::to_path_buf)
        .or_else(|| dirs.last().cloned())
        .context("--report-from needs a run directory")?;
    std::fs::create_dir_all(&out)?;
    let report = out.join("report-merged.md");
    std::fs::write(&report, sections.join("\n\n---\n\n"))?;
    println!("report: {}", report.display());
    Ok(())
}

/// The adapter a run directory measured (`rust` for runs before `--adapter`).
fn adapter_key(meta: &ReportMeta) -> String {
    meta.summary
        .iter()
        .find(|(key, _)| key == "adapter")
        .map_or_else(|| "rust".to_owned(), |(_, value)| value.clone())
}

/// Indices grouped by equal key, in the order the keys first appear.
fn group_indices_by_key(keys: &[String]) -> Vec<(String, Vec<usize>)> {
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, key) in keys.iter().enumerate() {
        match groups.iter_mut().find(|(k, _)| k == key) {
            Some((_, indices)) => indices.push(index),
            None => groups.push((key.clone(), vec![index])),
        }
    }
    groups
}

/// The merged report text of run directories of one adapter.
fn merged_report(members: Vec<(&PathBuf, (RunData, ReportMeta))>) -> String {
    let mut runs = Vec::new();
    let mut meta = ReportMeta::default();
    let mut notes = Vec::new();
    for (dir, (data, run_meta)) in members {
        let arms: Vec<String> = data
            .samples
            .keys()
            .map(|(condition, arm)| format!("{condition}/{arm}"))
            .collect();
        // Per run, since the one Setup block is the last run's and `--places`
        // changes what an arm measures (a run before the flag had none: scip).
        let places = run_meta
            .summary
            .iter()
            .find(|(key, _)| key == "places")
            .map_or("scip", |(_, value)| {
                value.split_whitespace().next().unwrap_or("scip")
            });
        notes.push(format!(
            "merged run {} ({}, places {places}): {}",
            dir.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
            run_meta.stamp,
            arms.join(", ")
        ));
        notes.extend(
            run_meta
                .notes
                .iter()
                .map(|note| format!("run {}: {note}", run_meta.stamp)),
        );
        meta = run_meta;
        runs.push(data);
    }
    notes.push("an arm present in two runs comes from the later one".to_owned());
    meta.notes = notes;
    meta.stamp = format!("{} (merged)", meta.stamp);
    let results = merge_runs(&runs);
    render_report(&meta, &results)
}

/// The report's SCIP line when a non-rust adapter has no index to walk.
pub const EMPTY_SCIP_NOTE: &str = "no SCIP index (empty file; scip maps give nothing)";

/// Writes the 0-byte `index.scip` of a non-rust adapter and returns the report's
/// note for it.
pub fn write_empty_scip(target: &Path) -> Result<String> {
    std::fs::write(target, b"").context("write the empty index.scip")?;
    Ok(EMPTY_SCIP_NOTE.to_owned())
}

/// The quiet threshold: machine CPU use (percent) below which nothing else is running.
const QUIET_CPU_PCT: f64 = 20.0;

async fn run_async(args: LspCostArgs) -> Result<()> {
    if !args.report_from.is_empty() {
        return report_from(&args.report_from, args.out.as_deref());
    }
    let tree = match &args.tree {
        Some(tree) => tree.clone(),
        None => workspace_root(),
    };
    let tree = dunce_like(&tree)?;
    let stamp = utc_stamp();
    let out = match &args.out {
        Some(out) => out.clone(),
        None => workspace_root().join("test-results/lsp-cost").join(&stamp),
    };
    std::fs::create_dir_all(&out)?;
    let out = dunce_like(&out)?;
    let work = match &args.work_dir {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join(format!("lci-lsp-cost-{stamp}")),
    };
    std::fs::create_dir_all(&work)?;
    let work = dunce_like(&work)?;

    let questions_text = match &args.questions {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read questions {}", path.display()))?,
        None => DEFAULT_QUESTIONS.to_owned(),
    };
    let mut questions = parse_questions(&questions_text)?;
    if let Some(n) = args.max_questions {
        questions.truncate(n);
    }
    if questions.is_empty() || args.repeat == 0 {
        bail!("no questions or --repeat 0");
    }
    let arms = select_arms(&args.arms)?;
    let server = resolve_server(args.adapter, args.server.as_deref(), &args.rust_analyzer)?;
    // For rust the resolved server is rust-analyzer: it writes the SCIP index
    // and is the arms' `rust_analyzer_path`, so `--server` overrides both.
    let rust_analyzer = if args.adapter == Adapter::Rust {
        server.clone()
    } else {
        args.rust_analyzer.clone()
    };

    let binary_source =
        args.binary
            .clone()
            .unwrap_or_else(|| match std::env::var_os("CARGO_TARGET_DIR") {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir).join("debug").join(format!(
                    "local-code-intelligence{}",
                    std::env::consts::EXE_SUFFIX
                )),
                _ => lci_binary(),
            });
    if !binary_source.is_file() {
        bail!(
            "server binary {} not found; build it first or pass --binary",
            binary_source.display()
        );
    }
    let binary = work.join(format!("lci-lsp-cost{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&binary_source, &binary).context("copy the server binary")?;

    let mut scip_note = String::new();
    let scip = match &args.scip {
        Some(path) => path.clone(),
        None => tree.join("index.scip"),
    };
    let scip = if scip.is_file() {
        scip_note.push_str(if args.adapter == Adapter::Rust {
            "an existing index (written earlier by `rust-analyzer scip` for this tree)"
        } else {
            "an existing index given for this tree"
        });
        dunce_like(&scip)?
    } else if args.adapter != Adapter::Rust {
        // Only rust-analyzer writes a SCIP index here; the other adapters walk
        // without one (the config needs a non-empty `scip_index` path).
        let target = out.join("index.scip");
        scip_note.push_str(&write_empty_scip(&target)?);
        target
    } else {
        let target = out.join("index.scip");
        println!("writing {} with `{} scip`", target.display(), rust_analyzer);
        let started = Instant::now();
        let status = std::process::Command::new(&rust_analyzer)
            .arg("scip")
            .arg(&tree)
            .arg("--output")
            .arg(&target)
            .current_dir(&tree)
            .status()
            .context("run rust-analyzer scip")?;
        if !status.success() || !target.is_file() {
            bail!("rust-analyzer scip failed ({status})");
        }
        let _ = write!(
            scip_note,
            "written by `rust-analyzer scip` in {:.0} s",
            started.elapsed().as_secs_f64()
        );
        target
    };

    let thread_count = args.load_threads.unwrap_or_else(|| {
        (std::thread::available_parallelism().map_or(4, usize::from) / 2).max(2)
    });
    let skipped_servers: Vec<String> = if args.adapter == Adapter::Rust {
        [
            "pyright-langserver",
            "gopls",
            "typescript-language-server",
            "jdtls",
            "csharp-ls",
            "clangd",
        ]
        .into_iter()
        .filter(|name| !which_on_path(name))
        .map(str::to_owned)
        .collect()
    } else {
        Vec::new()
    };
    let server_version = if args.adapter.has_version_flag() {
        command_text(&probe_program(&server), "--version")
    } else {
        format!(
            "{} has no `--version` (probe skipped)",
            args.adapter.default_server().unwrap_or("jdtls")
        )
    };
    let mut summary = vec![
        ("tree".to_owned(), format!("HEAD {}", git_head(&tree))),
        ("adapter".to_owned(), args.adapter.name().to_owned()),
        (
            "binary".to_owned(),
            format!(
                "copy of {} (as built: debug unless the path says otherwise)",
                file_name(&binary_source)
            ),
        ),
        ("server".to_owned(), server_version),
        ("scip index".to_owned(), scip_note),
        ("questions".to_owned(), questions.len().to_string()),
        (
            "arms of this run".to_owned(),
            arms.iter().map(|a| a.name).collect::<Vec<_>>().join(", "),
        ),
        (
            "places".to_owned(),
            match args.places {
                Places::Scip => "scip (this harness's default; written explicitly)",
                Places::Outline => "outline (`[run_profile.lsp] places`, every arm)",
            }
            .to_owned(),
        ),
    ];
    if args.adapter == Adapter::Rust {
        summary.push((
            "rust_analyzer_trusted".to_owned(),
            "false (the default: build scripts and proc macros are off)".to_owned(),
        ));
    }
    summary.push((
        "logical cores".to_owned(),
        std::thread::available_parallelism()
            .map_or(0, usize::from)
            .to_string(),
    ));
    let meta = ReportMeta {
        stamp: stamp.clone(),
        summary,
        repeat: args.repeat,
        deadline_ms: args.deadline_ms,
        n8_window: args.n8_window,
        skipped_servers,
        notes: Vec::new(),
    };

    let calls_file = std::fs::File::create(out.join("calls.jsonl"))?;
    let mut ctx = RunContext {
        binary,
        tree: tree.clone(),
        questions,
        repeat: args.repeat,
        deadline_ms: args.deadline_ms,
        out: out.clone(),
        work: work.clone(),
        scip,
        rust_analyzer,
        adapter: args.adapter,
        server: server.clone(),
        places_outline: args.places == Places::Outline,
        calls_file,
    };
    let conditions: Vec<(&str, bool)> = match args.load {
        LoadChoice::None => vec![("quiet", false)],
        LoadChoice::Cpu => vec![("loaded", true)],
        LoadChoice::Both => vec![("quiet", false), ("loaded", true)],
    };

    let max_wait = Duration::from_secs(args.max_quiet_wait_minutes * 60);
    let mut results: Vec<ConditionResult> = Vec::new();
    for (condition, loaded) in conditions {
        let mut result = ConditionResult {
            condition: condition.to_owned(),
            threads: loaded.then_some(thread_count),
            ..ConditionResult::default()
        };
        let mut load = None;
        if loaded {
            // The synthetic load goes on top of a quiet machine, not of someone else's job.
            let (cpu, quiet) = wait_for_quiet(condition, QUIET_CPU_PCT, max_wait).await;
            result
                .idle_cpu
                .insert("(before the load)".to_owned(), (cpu, quiet));
            load = Some(CpuLoad::start(thread_count));
            println!("[{condition}] {thread_count} busy threads started");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
        let version_probe = if args.adapter.has_version_flag() {
            time_version_probe(&probe_program(&server), args.probe_runs)
        } else {
            println!(
                "[{condition}] the {} server has no --version: probe skipped",
                args.adapter.name()
            );
            Vec::new()
        };
        println!(
            "[{condition}] --version p50 {} ms p95 {} ms",
            fmt_opt(percentile(&version_probe, 50.0)),
            fmt_opt(percentile(&version_probe, 95.0))
        );
        result.version_probe = version_probe;
        for arm in &arms {
            if !loaded {
                let (cpu, quiet) = wait_for_quiet(condition, QUIET_CPU_PCT, max_wait).await;
                result.idle_cpu.insert(arm.name.to_owned(), (cpu, quiet));
            }
            println!("[{condition}] arm {}", arm.name);
            let outcome = run_arm(&mut ctx, arm, condition)
                .await
                .map_err(|e| format!("{e:#}"));
            if let Err(error) = &outcome {
                println!("[{condition}] arm {} FAILED: {error}", arm.name);
            }
            result.arms.push((*arm, outcome));
            // A partial report after every arm.
            let mut so_far = results.clone();
            so_far.push(result.clone());
            write_report_files(&out, &meta, &so_far)?;
        }
        drop(load);
        results.push(result);
    }
    write_report_files(&out, &meta, &results)?;
    let _ = std::fs::remove_dir_all(&work);
    if !port_free(PORT) {
        bail!("port {PORT} is still in use after the run");
    }
    println!("report: {}", out.join("report.md").display());
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// The canonical form of `path` without the Windows `\\?\` prefix.
fn dunce_like(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("tree {} is not a directory", path.display()))?;
    let text = canonical.to_string_lossy();
    Ok(match text.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => canonical,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal_answer() -> Value {
        json!({
            "mode": "traverse",
            "stop_reason": "complete",
            "seed_source": "hybrid",
            "results": [
                {"node": "a", "origin": "lsp.live"},
                {"node": "b", "origin": "scip.graph"}
            ],
            "totals": {
                "pops": 40, "external_calls": 12, "physical_external_calls": 12,
                "elapsed_ms": 2500.0,
                "per_map": {
                    "lsp.live": {"expands": 6, "ms": 900.0},
                    "scip.graph": {"expands": 40, "ms": 3.5}
                }
            },
            "notes": [
                "lsp.live reported an error for a node: lsp.live: the traversal deadline passed",
                "something else"
            ]
        })
    }

    #[test]
    fn parse_call_record_reads_the_fields() {
        let record = parse_call_record(&literal_answer(), 3100.0);
        assert!(record.ok);
        assert_eq!(record.pops, 40);
        assert_eq!(record.external_calls, 12);
        assert_eq!(record.lsp_expands, 6);
        assert_eq!(record.lsp_ms, 900.0);
        assert_eq!(record.lsp_ms_per_expansion(), Some(150.0));
        assert_eq!(record.other_map_ms["scip.graph"], 3.5);
        assert_eq!(record.results, 2);
        assert_eq!(record.lsp_cards, 1);
        assert!(!record.fail_open());
        assert!(record.notes.node_error && record.notes.lsp_deadline);
        assert!(!record.notes.unavailable);
        assert_eq!(record.lsp_notes.len(), 1);
    }

    #[test]
    fn missing_lsp_map_is_zero_ms_and_unavailable_is_classified() {
        let answer = json!({
            "stop_reason": "deadline",
            "results": [{"node": "x"}],
            "totals": {"pops": 3, "elapsed_ms": 20000.0, "per_map": {"scip.graph": {"expands": 3, "ms": 1.0}}},
            "notes": ["lsp.live is unavailable: no language server is available (rust-analyzer: timed out)"]
        });
        let record = parse_call_record(&answer, 20500.0);
        assert_eq!(record.lsp_ms, 0.0);
        assert_eq!(record.lsp_expands, 0);
        assert_eq!(record.lsp_ms_per_expansion(), None);
        assert!(record.notes.unavailable);
        assert!(record.deadline_hit());
        assert!(record.fail_open());
    }

    #[test]
    fn tool_json_reads_sse_and_bare_bodies() {
        let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"structuredContent\":{\"a\":1}}}\n\n";
        assert_eq!(tool_json(sse).unwrap()["a"], 1);
        let bare = "{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"structuredContent\":{\"b\":2}}}";
        assert_eq!(tool_json(bare).unwrap()["b"], 2);
        assert!(tool_json("data: {\"error\":{\"code\":-1}}").is_err());
    }

    #[test]
    fn probe_ms_are_read_from_the_log_after_an_offset() {
        let log = "INFO x: started\nDEBUG a::traverse: lsp.live version probe probe_ms=12.5 probe_limit_ms=5000 ok=true\n";
        let (found, offset) = probe_ms_since(log, 0);
        assert_eq!(found, vec![12.5]);
        assert_eq!(offset, log.len());
        let more = format!(
            "{log}DEBUG a::traverse: lsp.live version probe probe_ms=5003.0 probe_limit_ms=5000 ok=false\n"
        );
        let (found, _) = probe_ms_since(&more, offset);
        assert_eq!(found, vec![5003.0]);
    }

    #[test]
    fn arm_overlay_lines_are_exactly_the_keys_an_arm_turns_on() {
        let lines = |name: &str| arm_overlay_lines(ARMS.iter().find(|a| a.name == name).unwrap());
        assert_eq!(lines("base"), ["sessions = \"per_call\"", "sync = \"off\""]);
        assert_eq!(lines("shared"), ["sessions = \"shared\"", "sync = \"off\""]);
        assert_eq!(
            lines("shared+cache"),
            [
                "sessions = \"shared\"",
                "outline_cache = true",
                "sync = \"off\""
            ]
        );
        assert_eq!(
            lines("shared+impl"),
            [
                "sessions = \"shared\"",
                "implementations = true",
                "sync = \"off\""
            ]
        );
        assert_eq!(
            lines("shared+sync"),
            ["sessions = \"shared\"", "sync = \"watch\""]
        );
        assert_eq!(
            lines("per_call+sync"),
            ["sessions = \"per_call\"", "sync = \"watch\""]
        );
        assert_eq!(
            lines("lifecycle"),
            [
                "sessions = \"shared\"",
                "outline_cache = true",
                "implementations = true",
                "sync = \"watch\""
            ]
        );
    }

    #[test]
    fn every_arm_config_is_a_valid_service_config() {
        for places_outline in [false, true] {
            let ctx = ConfigCtx {
                data_dir: std::env::temp_dir().join("lci-lsp-cost-test-data"),
                scip_index: std::env::temp_dir().join("index.scip"),
                rust_analyzer: "rust-analyzer".into(),
                deadline_ms: 20_000,
                adapter: Adapter::Rust,
                server: "rust-analyzer".into(),
                places_outline,
            };
            for arm in ARMS {
                let text = arm_config(&arm, &ctx);
                let config: lci_core::config::Config =
                    toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}\n{text}", arm.name));
                config
                    .validate()
                    .unwrap_or_else(|e| panic!("{}: {e:#}", arm.name));
                assert!(config.traverse.enabled);
                assert_eq!(
                    config.traverse.policies["explore"].extra_maps.as_deref(),
                    Some(&["lsp.live".to_owned()][..])
                );
                assert_eq!(
                    config.run_profile.lsp.sessions == lci_core::config::LspSessions::Shared,
                    arm.shared,
                    "{}",
                    arm.name
                );
                assert_eq!(config.run_profile.lsp.outline_cache, arm.outline_cache);
                assert_eq!(config.run_profile.lsp.implementations, arm.implementations);
                assert_eq!(
                    config.run_profile.lsp.places,
                    if places_outline {
                        lci_core::config::LspPlaces::Outline
                    } else {
                        lci_core::config::LspPlaces::Scip
                    },
                    "{}",
                    arm.name
                );
            }
        }
    }

    #[test]
    fn select_arms_keeps_the_given_order_and_refuses_unknown_names() {
        let names = vec!["shared".to_owned(), "base".to_owned()];
        let arms = select_arms(&names).unwrap();
        assert_eq!(arms[0].name, "shared");
        assert_eq!(arms[1].name, "base");
        assert_eq!(select_arms(&[]).unwrap().len(), 7);
        assert!(select_arms(&["nope".to_owned()]).is_err());
    }

    #[test]
    fn pops_per_5s_arithmetic() {
        assert_eq!(pops_per_5s(40, 2500.0), 80.0);
        assert_eq!(pops_per_5s(10, 5000.0), 10.0);
        assert_eq!(pops_per_5s(10, 0.0), 0.0);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let values = [10.0, 20.0, 30.0, 40.0, 50.0];
        assert_eq!(percentile(&values, 50.0), Some(30.0));
        assert_eq!(percentile(&values, 95.0), Some(50.0));
        assert_eq!(percentile(&values, 100.0), Some(50.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    fn sample(seq: usize, round: usize, lsp_ms: f64, expands: u64, ext: u64) -> Sample {
        Sample {
            seq,
            round,
            record: CallRecord {
                ok: true,
                wall_ms: lsp_ms + 100.0,
                elapsed_ms: 1000.0,
                pops: 10,
                external_calls: ext,
                lsp_expands: expands,
                lsp_ms,
                results: 3,
                lsp_cards: 1,
                stop_reason: "complete".into(),
                ..CallRecord::default()
            },
            probe_ms: vec![],
            rust_analyzer_rss_mb: 100.0 * seq as f64,
            server_rss_mb: 50.0,
            machine_cpu_pct: 30.0,
        }
    }

    #[test]
    fn summarize_splits_cold_and_warm() {
        let samples = vec![
            sample(1, 1, 8000.0, 4, 8),
            sample(2, 1, 400.0, 4, 8),
            sample(3, 2, 200.0, 4, 8),
            sample(4, 2, 300.0, 4, 8),
        ];
        let summary = summarize("base", &samples);
        assert_eq!(summary.cold_ms_per_exp, Some(2000.0));
        assert_eq!(summary.warm_p50, Some(75.0));
        assert_eq!(summary.warm_p95, Some(100.0));
        assert_eq!(summary.repeat_p50, Some(50.0));
        assert_eq!(summary.expansions_per_call, 4.0);
        assert_eq!(summary.external_per_expansion, 2.0);
        assert_eq!(summary.pops_per_5s, 50.0);
        assert_eq!(summary.ra_rss_max_mb, 400.0);
    }

    #[test]
    fn report_table_renders_a_two_arm_diff() {
        let base = summarize(
            "base",
            &[sample(1, 1, 800.0, 4, 8), sample(2, 1, 400.0, 4, 8)],
        );
        let shared = summarize(
            "shared",
            &[sample(1, 1, 800.0, 4, 8), sample(2, 1, 40.0, 4, 8)],
        );
        let table = report_table(&[base, shared]);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[2].starts_with("| base | 2 (0) | 200.0 | 100.0 |"),
            "{}",
            lines[2]
        );
        assert!(
            lines[3].starts_with("| shared | 2 (0) | 200.0 | 10.0 | 10.0 | -90.0 |"),
            "{}",
            lines[3]
        );
    }

    #[test]
    fn n8_table_counts_dark_calls_and_the_longest_run() {
        let mut samples: Vec<Sample> = (1..=5).map(|i| sample(i, 1, 100.0, 2, 4)).collect();
        for index in [1, 2, 4] {
            samples[index].record.notes.lsp_deadline = true;
        }
        let (table, dark, longest) = n8_table(&samples, 30);
        assert_eq!((dark, longest), (3, 2));
        assert_eq!(table.matches("dark |").count(), 3 + 1);
    }

    #[test]
    fn stamp_formats_a_utc_time() {
        assert_eq!(stamp_of(0), "19700101T000000Z");
        assert_eq!(stamp_of(1_792_000_000), "20261014T174640Z");
    }

    #[test]
    fn a_call_line_round_trips_through_calls_jsonl() {
        let mut original = sample(7, 2, 321.5, 3, 5);
        original.record.notes.lsp_deadline = true;
        original.record.lsp_notes = vec!["lsp.live: the traversal deadline passed".to_owned()];
        original.probe_ms = vec![12.5];
        let line = call_line("loaded", "shared", "q03", &original);
        let (condition, arm, back) = sample_from_line(&line).unwrap();
        assert_eq!((condition.as_str(), arm.as_str()), ("loaded", "shared"));
        assert_eq!(back.seq, 7);
        assert_eq!(back.round, 2);
        assert_eq!(back.record, original.record);
        assert_eq!(back.probe_ms, vec![12.5]);
        assert_eq!(back.machine_cpu_pct, 30.0);
    }

    #[test]
    fn merged_runs_take_an_arm_from_the_later_run_and_pool_the_probes() {
        let run = |cpu: f64, arm: &str, probe: f64| {
            let mut s = sample(1, 1, 100.0, 2, 4);
            s.machine_cpu_pct = cpu;
            let mut samples = BTreeMap::new();
            samples.insert(("quiet".to_owned(), arm.to_owned()), vec![s]);
            RunData {
                conditions: vec![CondMeta {
                    condition: "quiet".into(),
                    version_probe: vec![probe],
                    ..CondMeta::default()
                }],
                samples,
            }
        };
        let merged = merge_runs(&[
            run(10.0, "base", 90.0),
            run(95.0, "base", 100.0),
            run(5.0, "shared", 80.0),
        ]);
        assert_eq!(merged.len(), 1);
        let quiet = &merged[0];
        assert_eq!(quiet.version_probe, vec![90.0, 100.0, 80.0]);
        let names: Vec<&str> = quiet.arms.iter().map(|(a, _)| a.name).collect();
        assert_eq!(names, ["base", "shared"]);
        let base = quiet.arms[0].1.as_ref().unwrap();
        assert_eq!(base[0].machine_cpu_pct, 95.0);
    }

    #[test]
    fn a_merged_report_names_the_places_of_every_run() {
        let run = |arm: &str| {
            let mut samples = BTreeMap::new();
            samples.insert(
                ("quiet".to_owned(), arm.to_owned()),
                vec![sample(1, 1, 100.0, 2, 4)],
            );
            RunData {
                conditions: vec![CondMeta {
                    condition: "quiet".into(),
                    ..CondMeta::default()
                }],
                samples,
            }
        };
        let meta = |stamp: &str, places: Option<&str>| ReportMeta {
            stamp: stamp.into(),
            summary: places
                .map(|p| vec![("places".to_owned(), p.to_owned())])
                .unwrap_or_default(),
            ..ReportMeta::default()
        };
        let (old, new) = (PathBuf::from("old"), PathBuf::from("new"));
        let report = merged_report(vec![
            (&old, (run("base"), meta("s1", None))),
            (
                &new,
                (
                    run("shared"),
                    meta(
                        "s2",
                        Some("outline (`[run_profile.lsp] places`, every arm)"),
                    ),
                ),
            ),
        ]);
        assert!(
            report.contains("merged run old (s1, places scip): quiet/base"),
            "{report}"
        );
        assert!(
            report.contains("merged run new (s2, places outline): quiet/shared"),
            "{report}"
        );
    }

    #[test]
    fn a_bimodal_arm_and_every_idle_entry_reach_the_report() {
        let mut fast = sample(1, 1, 100.0, 40, 5);
        fast.record.stop_reason = "bound".into();
        let mut slow = sample(2, 1, 20000.0, 2, 2);
        slow.record.stop_reason = "deadline".into();
        let mut result = ConditionResult {
            condition: "loaded".into(),
            arms: vec![(ARMS[0], Ok(vec![fast, slow]))],
            ..ConditionResult::default()
        };
        result
            .idle_cpu
            .insert("(before the load)".into(), (98.0, false));
        result.idle_cpu.insert("base".into(), (3.0, true));
        let lines = deadline_split_lines(&result.arms);
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains("1 of 2 calls ended before the deadline"),
            "{}",
            lines[0]
        );
        let meta = ReportMeta {
            stamp: "s".into(),
            ..ReportMeta::default()
        };
        let report = render_report(&meta, &[result]);
        assert!(
            report.contains("base 3%, (before the load) 98% (not quiet)"),
            "{report}"
        );
        assert!(
            report.contains("loaded 2 rounds") || report.contains("loaded 1 rounds"),
            "{report}"
        );
    }

    #[test]
    fn condition_meta_round_trips() {
        let mut meta = CondMeta {
            condition: "loaded".into(),
            threads: Some(12),
            version_probe: vec![100.0, 120.0],
            ..CondMeta::default()
        };
        meta.idle_cpu.insert("base".into(), (3.5, true));
        meta.arm_errors.insert("lifecycle".into(), "no".into());
        assert_eq!(cond_meta_from_json(&cond_meta_json(&meta)).unwrap(), meta);
    }

    fn test_ctx(adapter: Adapter, server: &str) -> ConfigCtx {
        ConfigCtx {
            data_dir: std::env::temp_dir().join("lci-lsp-cost-test-data"),
            scip_index: std::env::temp_dir().join("index.scip"),
            rust_analyzer: "rust-analyzer".into(),
            deadline_ms: 20_000,
            adapter,
            server: server.into(),
            places_outline: false,
        }
    }

    #[test]
    fn arm_config_is_unchanged_for_rust() {
        // The text of the `base` arm as written before `--adapter` existed,
        // plus the explicit `places = "scip"` since `auto` became the default
        // and the explicit `sync = "off"` since `watch` became the default.
        let ctx = test_ctx(Adapter::Rust, "rust-analyzer");
        let expected = format!(
            "data_dir = '{}'\n{}",
            toml_path(&ctx.data_dir),
            concat!(
                "rust_analyzer_path = 'rust-analyzer'\n",
                "lsp_timeout_seconds = 120\n",
                "embedding_url = \"http://127.0.0.1:18766/v1\"\n",
                "reranker_url = \"http://127.0.0.1:18767/rerank\"\n",
                "\n[channels]\nsemantic = false\nlsp = false\nreranker = false\n",
                "\n[index]\nfreshness = \"manual\"\n",
                "\n[traverse]\nenabled = true\nscip_index = '@SCIP@'\ndeadline_ms = 20000\n",
                "\n[traverse.policies.explore]\nextra_maps = [\"lsp.live\"]\n",
                "\n[run_profile.lsp]\nsessions = \"per_call\"\nsync = \"off\"\nplaces = \"scip\"\n",
            )
        )
        .replace("@SCIP@", &toml_path(&ctx.scip_index));
        assert_eq!(arm_config(&ARMS[0], &ctx), expected);
    }

    #[test]
    fn a_non_rust_adapter_adds_exactly_its_section_and_path() {
        let cases = [
            (Adapter::Python, "python", "pyright-langserver"),
            (Adapter::Go, "go", "gopls"),
            (
                Adapter::Typescript,
                "typescript",
                "typescript-language-server",
            ),
            (Adapter::Java, "java", "C:\\tools\\jdtls"),
            (Adapter::C, "clangd", "clangd"),
            (Adapter::Cpp, "clangd", "clangd"),
            (Adapter::Csharp, "csharp", "csharp-ls"),
        ];
        for (adapter, section, server) in cases {
            for arm in ARMS {
                let text = arm_config(&arm, &test_ctx(adapter, server));
                let rust_text = arm_config(&arm, &test_ctx(Adapter::Rust, "rust-analyzer"));
                let added: Vec<&str> = text.lines().filter(|l| !rust_text.contains(l)).collect();
                let slashed = server.replace('\\', "/");
                let path_line = format!("path = '{slashed}'");
                let section_line = format!("[{section}]");
                assert_eq!(
                    added,
                    vec![section_line.as_str(), path_line.as_str()],
                    "{adapter:?} {}",
                    arm.name
                );
                // Nothing the rust config has is lost.
                assert!(rust_text.lines().all(|l| text.contains(l)));
                let config: lci_core::config::Config = toml::from_str(&text)
                    .unwrap_or_else(|e| panic!("{adapter:?} {}: {e}\n{text}", arm.name));
                config.validate().unwrap();
                let wrote = match adapter {
                    Adapter::Python => config.python.path,
                    Adapter::Go => config.go.path,
                    Adapter::Typescript => config.typescript.path,
                    Adapter::Java => config.java.path,
                    Adapter::C | Adapter::Cpp => config.clangd.path,
                    Adapter::Csharp => config.csharp.path,
                    Adapter::Rust => unreachable!(),
                };
                assert_eq!(wrote.as_deref(), Some(slashed.as_str()));
            }
        }
        assert!(
            !arm_config(&ARMS[0], &test_ctx(Adapter::Rust, "rust-analyzer")).contains("\npath = ")
        );
    }

    #[test]
    fn the_server_defaults_to_the_adapter_and_jdtls_needs_one() {
        let pick = |adapter, server: Option<&str>| resolve_server(adapter, server, "ra-custom");
        assert_eq!(pick(Adapter::Rust, None).unwrap(), "ra-custom");
        assert_eq!(pick(Adapter::Rust, Some("x")).unwrap(), "x");
        assert_eq!(pick(Adapter::Python, None).unwrap(), "pyright-langserver");
        assert_eq!(pick(Adapter::Cpp, None).unwrap(), "clangd");
        assert_eq!(pick(Adapter::Go, Some("/bin/gopls")).unwrap(), "/bin/gopls");
        let error = pick(Adapter::Java, None).unwrap_err().to_string();
        assert!(error.contains("--server"), "{error}");
        assert_eq!(pick(Adapter::Java, Some("/jdtls")).unwrap(), "/jdtls");
        assert_eq!(Adapter::ALL.len(), 8);
    }

    #[test]
    fn an_empty_scip_is_written_and_said_so() {
        let dir = std::env::temp_dir().join(format!("lci-lsp-cost-scip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("index.scip");
        let note = write_empty_scip(&target).unwrap();
        assert_eq!(std::fs::metadata(&target).unwrap().len(), 0);
        assert_eq!(note, "no SCIP index (empty file; scip maps give nothing)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rss_counts_the_process_names_of_the_adapter() {
        assert!(rss_name_matches(
            "rust-analyzer.exe",
            Adapter::Rust.rss_names()
        ));
        assert!(rss_name_matches("Node.EXE", Adapter::Python.rss_names()));
        assert!(rss_name_matches("node", Adapter::Typescript.rss_names()));
        assert!(rss_name_matches("gopls.exe", Adapter::Go.rss_names()));
        assert!(rss_name_matches("clangd.exe", Adapter::C.rss_names()));
        assert!(rss_name_matches("clangd", Adapter::Cpp.rss_names()));
        assert!(rss_name_matches(
            "csharp-ls.exe",
            Adapter::Csharp.rss_names()
        ));
        assert!(rss_name_matches("java.exe", Adapter::Java.rss_names()));
        // A server of another adapter, or `serve` itself, is not counted.
        assert!(!rss_name_matches(
            "rust-analyzer.exe",
            Adapter::Python.rss_names()
        ));
        assert!(!rss_name_matches("node.exe", Adapter::Go.rss_names()));
        assert!(!rss_name_matches(
            "lci-lsp-cost.exe",
            Adapter::Rust.rss_names()
        ));
        for adapter in Adapter::ALL {
            assert!(!adapter.rss_names().is_empty(), "{adapter:?}");
        }
    }

    #[test]
    fn runs_of_different_adapters_get_their_own_report() {
        let keys: Vec<String> = ["rust", "python", "rust", "go"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        assert_eq!(
            group_indices_by_key(&keys),
            vec![
                ("rust".to_owned(), vec![0, 2]),
                ("python".to_owned(), vec![1]),
                ("go".to_owned(), vec![3]),
            ]
        );
        let mut meta = ReportMeta::default();
        assert_eq!(adapter_key(&meta), "rust");
        meta.summary.push(("adapter".to_owned(), "go".to_owned()));
        assert_eq!(adapter_key(&meta), "go");
    }

    #[test]
    fn the_committed_questions_parse() {
        let questions = parse_questions(DEFAULT_QUESTIONS).unwrap();
        assert_eq!(questions.len(), 12);
        let mut ids: Vec<&str> = questions.iter().map(|q| q.id.as_str()).collect();
        ids.dedup();
        assert_eq!(ids.len(), 12);
    }
}
