use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use local_code_intelligence::{
    app::{App, CANDIDATE_DUMP_ENV, stamp},
    config::{
        Config,
        layers::{self, Flags, LocalState, Provenance, ResolveSpec, Resolved, Source},
    },
    evaluate::{
        self, CapStage, CompareOptions, DEFAULT_RESAMPLES, DEFAULT_SEED, ReplayOptions, RerankMode,
        RunOptions,
    },
    init, lsp, server,
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    future::Future,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

async fn cancel_on_signal<F>(signal: F, cancellation: tokio_util::sync::CancellationToken) -> bool
where
    F: Future<Output = std::io::Result<()>>,
{
    match signal.await {
        Ok(()) => {
            cancellation.cancel();
            true
        }
        Err(error) => {
            tracing::error!(%error, "shutdown signal wait failed; continuing without it");
            false
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ShutdownSignal {
    Interrupt,
    #[cfg(unix)]
    Terminate,
}

impl ShutdownSignal {
    const fn exit_code(self) -> u8 {
        match self {
            #[cfg(unix)]
            Self::Interrupt => 130,
            #[cfg(not(unix))]
            Self::Interrupt => {
                // Windows has no SIGTERM; Ctrl+C and Ctrl+Break both use 130.
                130
            }
            #[cfg(unix)]
            Self::Terminate => 143,
        }
    }
}

#[derive(Debug)]
struct ShutdownSignalError(ShutdownSignal);

impl std::fmt::Display for ShutdownSignalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("stopped by a shutdown signal")
    }
}

impl std::error::Error for ShutdownSignalError {}

/// Resolves on Ctrl+C, or on SIGTERM on Unix, after cancelling
/// `cancellation`. A signal whose wait fails is logged and never resolves
/// (see [`cancel_on_signal`]), so the process keeps running instead of
/// stopping at once.
async fn shutdown_signal(cancellation: tokio_util::sync::CancellationToken) -> ShutdownSignal {
    let ctrl_c = async {
        if !cancel_on_signal(tokio::signal::ctrl_c(), cancellation.clone()).await {
            std::future::pending::<()>().await;
        }
        ShutdownSignal::Interrupt
    };
    #[cfg(unix)]
    let terminate = async {
        let signal = async {
            let mut terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            terminate
                .recv()
                .await
                .ok_or_else(|| std::io::Error::other("SIGTERM signal stream closed"))
        };
        if !cancel_on_signal(signal, cancellation.clone()).await {
            std::future::pending::<()>().await;
        }
        ShutdownSignal::Terminate
    };
    #[cfg(unix)]
    {
        tokio::select! {
            signal = ctrl_c => signal,
            signal = terminate => signal,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await
}

/// Stops the language servers cleanly instead of leaving it to the process
/// exit killing them. Their shutdowns run side by side, each bounded by twice
/// the grace period, so this budget (the grace period twice over plus a
/// second, 5 s today) is a backstop; exceeding it only logs a warning and the
/// rest are killed on exit.
async fn release_language_servers(app: &App) {
    let budget = 2 * lsp::SHUTDOWN_GRACE + Duration::from_secs(1);
    match tokio::time::timeout(budget, app.shutdown_language_servers()).await {
        Ok(released) => tracing::info!(released, "language servers released at shutdown"),
        Err(_) => tracing::warn!(
            budget_s = budget.as_secs(),
            "language server shutdown did not finish in time; the rest are killed on exit"
        ),
    }
}

/// Runs a command that may start language servers (`search`, `evaluate`,
/// `symbols`, `definition`, `references`) until it ends or a shutdown signal
/// arrives. On a signal the command is dropped, the language servers are
/// released, and an error is returned, so `main` returns and the runtime's
/// shutdown ends whatever was still starting (a server's process is killed
/// when its handle drops). Without this the signal's default action ends the
/// process at once, and a server that outlives its client stays behind.
async fn until_shutdown_signal<T>(
    app: &App,
    command: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::select! {
        result = command => result,
        signal = shutdown_signal(tokio_util::sync::CancellationToken::new()) => {
            release_language_servers(app).await;
            Err(ShutdownSignalError(signal).into())
        }
    }
}

#[derive(Parser)]
#[command(
    version,
    about = "Standalone local Rust, TypeScript, JavaScript, Python, C#, Go, Java, C, and C++ code and Markdown documentation (opt-in, `[index].prose`) retrieval over MCP or plain REST/OpenAPI, with optional language-server navigation for every code language (rust-analyzer, csharp-ls, typescript-language-server, pyright, gopls, jdtls, clangd)"
)]
struct Cli {
    /// Configuration file. Without it `LCI_CONFIG` names one, and without
    /// that the built-in defaults stand alone. Machine-specific values come
    /// from the local overlay beside it (`<stem>.local.toml`), then from
    /// `LCI_*` variables; run `config` to see every value and its source.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Ignore the local overlay file (`<stem>.local.toml` beside the
    /// configuration file, else the user-level `config.local.toml`, or the
    /// file `LCI_LOCAL_CONFIG` names).
    #[arg(long, global = true)]
    no_local_config: bool,
    #[command(subcommand)]
    command: Option<Command>,
}
#[derive(Subcommand)]
enum Command {
    Serve {
        /// TCP port to bind the MCP/health server to. Overrides `port` from
        /// the configuration, the local overlay and `LCI_PORT`; without any
        /// of them it is 8768.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print every resolved configuration value with the layer it came from
    /// (default, file, local overlay, environment), the layers consulted, and
    /// the URLs derived from the port. Needs no services and no index: it
    /// never creates the data directory, opens the database, sends a request
    /// or starts a language server. Exits 0 when the configuration is valid,
    /// 2 when it resolves but is invalid (the report is still printed), and
    /// 1 when a layer cannot be read.
    Config {
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Write this machine's local overlay (the file `config` reports as
    /// `local`) from what it finds: the platform data directory and the
    /// absolute path of each tool on PATH, plus comments on the model
    /// services, the configured models and the port. It never changes a model
    /// name or the port, never writes the main configuration, and never
    /// replaces an existing overlay without --force (exit 3 then). It contacts
    /// only loopback hosts and never port 8765; --no-probe sends no request.
    /// The overlay is the sibling of --config, else the per-user file, or the
    /// file LCI_LOCAL_CONFIG names; --no-local-config does not apply here.
    Init {
        /// Replace an existing overlay.
        #[arg(long)]
        force: bool,
        /// Print the overlay that would be written; write nothing.
        #[arg(long)]
        dry_run: bool,
        /// Send no request: do not ask the model services or the port.
        #[arg(long)]
        no_probe: bool,
        /// Print one JSON object instead of text.
        #[arg(long)]
        json: bool,
    },
    Index {
        workspace: PathBuf,
    },
    Status {
        workspace: PathBuf,
    },
    ListFiles {
        workspace: PathBuf,
    },
    Search {
        workspace: PathBuf,
        query: String,
        #[arg(long)]
        top_k: Option<usize>,
        #[arg(long = "language")]
        languages: Vec<String>,
        #[arg(long = "include-path")]
        include_paths: Vec<String>,
        #[arg(long = "exclude-path")]
        exclude_paths: Vec<String>,
        #[arg(long = "source-role")]
        source_roles: Vec<String>,
        /// Run the search with this `[search_profiles.<name>]` of the
        /// configuration applied over `[channels]` and
        /// `rerank_candidate_count`. The response then carries
        /// `search_profile`. An unknown name is an error that lists the
        /// configured ones.
        #[arg(long = "search-profile")]
        search_profile: Option<String>,
    },
    Evaluate {
        definition: PathBuf,
        #[arg(long = "workspace")]
        workspaces: Vec<String>,
        #[arg(long)]
        output: Option<PathBuf>,
        /// Compare this run against an earlier evaluation report: embeds a
        /// `comparison` field in the output and prints the summary to stderr.
        #[arg(long)]
        baseline: Option<PathBuf>,
        /// Bootstrap seed for the `--baseline` comparison.
        #[arg(long, requires = "baseline")]
        seed: Option<u64>,
        /// Bootstrap resamples for the `--baseline` comparison.
        #[arg(long, requires = "baseline", value_parser = parse_resamples)]
        resamples: Option<usize>,
        /// Record every query's whole fused candidate pool (channel ranks,
        /// scores, per-file cap and shortlist position, reranker scores) and
        /// the retrieval configuration in the report, so other arms can be
        /// scored from it without services. Results and metrics are
        /// unchanged; the pretty-printed report grows by roughly 100 KB per query.
        #[arg(long)]
        dump_candidates: bool,
        /// Do not start and prime the language servers before the first query
        /// of each workspace. The first query then pays the start (and may miss
        /// `lsp_search_budget_ms`): what a cold-start measurement wants. Without
        /// this flag, with `[channels] lsp` on, every enabled server is
        /// primed first and the outcome is recorded under `priming`.
        #[arg(long)]
        no_prime: bool,
        /// Keep, in the `--baseline` comparison, the pairs in which either run's
        /// language server missed `lsp_search_budget_ms`. They are left out by
        /// default and counted.
        #[arg(long, requires = "baseline")]
        keep_failed: bool,
    },
    /// Compare two saved evaluation reports query by query. Works offline: it
    /// needs no services, configuration, or index. A regression is not an
    /// error; the exit code is nonzero only when a report cannot be read.
    Compare {
        /// Report of the run to compare against.
        baseline: PathBuf,
        /// Report of the run being judged.
        candidate: PathBuf,
        /// Also write the full JSON comparison report to this file.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Seed of the paired bootstrap.
        #[arg(long, default_value_t = DEFAULT_SEED)]
        seed: u64,
        /// Number of bootstrap resamples.
        #[arg(long, default_value_t = DEFAULT_RESAMPLES, value_parser = parse_resamples)]
        resamples: usize,
        /// Keep the pairs in which either run's language server missed
        /// `lsp_search_budget_ms` (`lsp_budget_hit`). They measured a cold
        /// start, so they are left out by default and counted in the summary.
        #[arg(long)]
        keep_failed: bool,
    },
    /// Score other retrieval settings from the candidate dump of an earlier
    /// `evaluate --dump-candidates` report, without services. Works offline
    /// like `compare`: it needs no configuration or index, and its report
    /// (`--output`) is an ordinary evaluation report that `compare` pairs with
    /// real ones. Settings you leave out keep the dumped arm's values.
    /// Queries it cannot score exactly are refused, counted, and reported as
    /// skipped; `reranking_ms` and `total_ms` are cost-model estimates.
    Replay {
        /// Report written by `evaluate --dump-candidates`.
        report: PathBuf,
        /// Write the replayed evaluation report (JSON) to this file. The
        /// summary is printed either way.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Rerank window: how many leading candidates the reranker orders.
        /// The rest of the fused order follows them. At most the dumped
        /// shortlist can be scored (default: the dumped
        /// `rerank_candidate_count`).
        #[arg(long, value_parser = parse_window)]
        window: Option<usize>,
        /// Chunks of one file that keep their place before the rest of that
        /// file's chunks are demoted (0 is unlimited; default: the dumped
        /// `max_chunks_per_file`).
        #[arg(long, value_parser = parse_cap)]
        cap: Option<u32>,
        /// Where the cap runs: `shortlist` (before the window cut, as in
        /// production, the default) or `results` (after reranking).
        #[arg(long)]
        cap_stage: Option<CapStage>,
        /// Weight of the semantic channel in fusion.
        #[arg(long, value_parser = parse_positive)]
        semantic_weight: Option<f32>,
        /// Weight of the lexical channel in fusion.
        #[arg(long, value_parser = parse_positive)]
        lexical_weight: Option<f32>,
        /// Weight of the language-server channel in fusion.
        #[arg(long, value_parser = parse_positive)]
        lsp_weight: Option<f32>,
        /// Reciprocal Rank Fusion constant.
        #[arg(long, value_parser = parse_positive)]
        rrf_k: Option<f32>,
        /// Source-role prior of `source` chunks (production: 0.004).
        #[arg(long, value_parser = parse_non_negative)]
        prior: Option<f32>,
        /// How the reranker's scores order the window: `replace` (production),
        /// `vote` (fusion score plus --vote-weight / (rrf_k + reranker rank)),
        /// or `off` (fused order). Default: the dumped mode.
        #[arg(long)]
        rerank_mode: Option<RerankMode>,
        /// Weight of the reranker's vote; required by, and only valid with,
        /// `--rerank-mode vote`.
        #[arg(long, value_parser = parse_non_negative)]
        vote_weight: Option<f32>,
    },
    /// Walk the code graph of a SCIP index from the start nodes a question
    /// finds (or from given nodes), under a budget and optional steering
    /// rules, and write the trace. With `--root` the start nodes are the top
    /// hits of the hybrid search over that tree (its index and the configured
    /// model services, as `search` uses them), mapped to the index's symbols;
    /// without it, or when that search fails or finds nothing, a stand-in that
    /// matches the question's words against symbol names is used and the
    /// output says so (`seed_source`). `[traverse] enabled` does not apply (it
    /// only gates the MCP mode). The limits are the `[traverse]` caps. Prints
    /// one JSON object: the reached nodes, the totals, the handlers'
    /// decisions and, when the ladder could not decide a stop or the budget
    /// ran out, the `handle` and `checkpoint`; continue with `--handle` (and
    /// `--steer`).
    Traverse(Box<TraverseArgs>),
    /// Run one recipe over a SCIP index and print its JSON answer: the nodes,
    /// the steps run, why it stopped and what it cost. The recipes are
    /// config_readers, impact_rings, flow_path, flow_from, config_neighbours
    /// and lci_compose (arguments as the matching MCP tool takes them, minus
    /// `workspace_path`, which is `--root`). `[recipes] enabled` and `[compose]
    /// enabled` only gate the MCP and REST tools; this command always runs.
    /// config_readers, config_neighbours and an lci_compose config.key step
    /// need `rg` on PATH.
    Recipe(RecipeArgs),
    Symbols {
        workspace: PathBuf,
        query: String,
    },
    Definition {
        workspace: PathBuf,
        relative_file_path: String,
        line: u32,
        character: u32,
    },
    References {
        workspace: PathBuf,
        relative_file_path: String,
        line: u32,
        character: u32,
        #[arg(long)]
        include_declaration: bool,
    },
}

#[derive(clap::Args)]
struct TraverseArgs {
    /// The SCIP index to walk (from `rust-analyzer scip`).
    #[arg(long)]
    scip: PathBuf,
    /// The tree the index describes, read for the enclosing-range repair and
    /// searched for the start nodes (it must be the workspace `search` indexes).
    #[arg(long)]
    root: Option<PathBuf>,
    /// The question; the start nodes come from the hybrid search for it (the
    /// stand-in that matches its words against symbol names when that is not
    /// available).
    #[arg(long, conflicts_with = "handle")]
    question: Option<String>,
    /// Start nodes (canonical node ids, comma separated) instead of finding
    /// them from the question.
    #[arg(long, value_delimiter = ',', conflicts_with = "handle")]
    seeds: Vec<String>,
    /// Maps to use, comma separated: graph.scip, structure.contains,
    /// seeds.hybrid, seeds.scip_names (default: all four). Without
    /// seeds.hybrid the stand-in seeds.scip_names finds the start nodes;
    /// seeds.bm25 (BM25 over the SCIP graph's symbol names, paths and
    /// documentation, its nodes after the hybrid search's) joins the default
    /// set and runs only with `[run_profile.seeds] bm25 = true`, and needs no
    /// --root;
    /// structure.contains lets a chunk or file start node reach the symbols it
    /// holds. Also available, only when named, and all need --root: grep.live
    /// (a search of the tree's current bytes), config.key (config keys and
    /// the declarations that read them; needs rg on PATH), git.history
    /// (commits and co-change from the tree's git history) and probe.compile
    /// (compiler errors of a patch to a function, found by `cargo check` on a
    /// scratch copy; seconds per node; needs `rust_analyzer_trusted = true`:
    /// cargo check runs the tree's build scripts), search.lexical (the lexical search
    /// from a node's own name, to the chunks it finds), search.vector (the
    /// chunks nearest to a node's stored embedding; needs the embedder only
    /// for a node with no stored vector), scip.impact (the declarations
    /// that reference a symbol, from the SCIP impact index), scip.implements
    /// (types to the traits they implement, traits to their implementing
    /// types and trait methods to their implementations, from the
    /// `impl .. for ..` headers of the tree (--root) and the SCIP index's
    /// reference occurrences; derived and macro impls are absent),
    /// tests.tested_by (the units in a test file or a test module that
    /// reference a symbol, from the same SCIP impact index), docs.mentions
    /// (the documentation files that mention a node's name, edges to the
    /// files only) and lsp.live (definition, references and document symbols from a running language
    /// server, with the server paths and lsp_timeout_seconds of the
    /// configuration; the server starts at the first expansion, so a cold
    /// start can exceed [traverse] deadline_ms and the walk then ends
    /// with the deadline note), exact.closure (the declarations that
    /// reference the symbol the question names) and exact.pickaxe (the commits
    /// git log -G/-S/--grep finds for a history question); the two exact maps
    /// are never walked, their cards are the first stage (--first-stage).
    #[arg(long, value_delimiter = ',')]
    maps: Vec<String>,
    /// The `[search_profiles.<name>]` the hybrid search for the start nodes
    /// runs with (default: the base configuration).
    #[arg(long)]
    search_profile: Option<String>,
    /// The steering policy: a preset name (explore, impact, flow, where-to, history;
    /// explore is what no policy runs; a preset also sets the measured first-stage
    /// and walk defaults of its question type, see `[traverse.policies]`), or a JSON
    /// file or inline JSON, either
    /// an array of rules or the object of `search_code`'s `steer` (rules,
    /// ladder, max_stops, tau, stop_every, on_disagreement, likely_complete).
    #[arg(long)]
    policy: Option<String>,
    /// A stop (a consultation of the rules) every this many pops.
    #[arg(long)]
    stops: Option<u32>,
    /// Most pops, up to the `[traverse]` cap.
    #[arg(long)]
    pops: Option<u32>,
    /// Depth at which states are recorded, not expanded (the deepest state
    /// expanded is at one less), up to the `[traverse]` cap.
    #[arg(long)]
    max_depth: Option<u32>,
    /// Results to print.
    #[arg(long, default_value_t = 8)]
    top_k: usize,
    /// How results are ranked: `boost`, `rrf` or `log_odds` (default: the `[traverse]
    /// fusion` setting). A continuation keeps the fusion its handle started
    /// with.
    #[arg(long)]
    fusion: Option<local_code_intelligence::config::TraverseFusion>,
    /// Expand connective nodes (files, commits, config keys) without costing
    /// a pop (default: the `[traverse] pass_through` setting). A continuation
    /// keeps the setting its handle started with.
    #[arg(long)]
    pass_through: bool,
    /// Rate the kept hops of each expansion with the reranker and scale their
    /// values by `rerank_floor + (1 - rerank_floor) * relevance` (default: the
    /// `[traverse] rerank_frontier` setting; the floor is `rerank_floor`).
    /// Needs --root. A continuation must be given the same setting.
    #[arg(long)]
    rerank_frontier: bool,
    /// Never end the walk on the score bound (the `[traverse] score_bound`
    /// setting, on by default). A continuation keeps the setting its handle
    /// started with.
    #[arg(long)]
    no_score_bound: bool,
    /// Write a `pop` event per pop into the trace: the popped state and the
    /// best queued candidates (evaluation runs; off by default, and the trace
    /// is the only thing it changes). A continuation must be given it again.
    #[arg(long)]
    trajectory: bool,
    /// A table of fitted hop priors (the `[traverse] priors` setting); needs
    /// --question-type. A continuation must be given the same table.
    #[arg(long, requires = "question_type")]
    priors: Option<String>,
    /// The question type whose priors price the hops (the `[traverse]
    /// question_type` setting); needs a table.
    #[arg(long)]
    question_type: Option<String>,
    /// A fitted tie-break file (the `[traverse] tie_break` setting) that orders
    /// results tied at the same rank. A continuation must be given the same file.
    #[arg(long)]
    tie_break: Option<String>,
    /// How the first-stage answers (the cards of the maps exact.closure and
    /// exact.pickaxe, then with --answer-seeds the start nodes) join the walk's
    /// results: `off`, `append` or `interleave` (default: the `[traverse]
    /// first_stage` setting). A continuation returns the walk alone.
    #[arg(long)]
    first_stage: Option<local_code_intelligence::config::TraverseFirstStage>,
    /// Most first-stage answers, 0 for no cap (default: the `[traverse]
    /// first_k` setting).
    #[arg(long)]
    first_k: Option<u32>,
    /// Also answer with the start nodes (the `[traverse] answer_seeds`
    /// setting); `--answer-seeds=false` turns a configured `true` off.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    answer_seeds: Option<bool>,
    /// Add the exact cards the SCIP graph holds to the start nodes (the
    /// `[traverse] exact_seeds` setting); `--exact-seeds=false` turns a
    /// configured `true` off.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    exact_seeds: Option<bool>,
    /// With `--maps scip.implements`: how the traits and trait methods the
    /// question's code spans name (`tauri::Manager`, `Runtime::run`) join the
    /// other start nodes. `replace` keeps only them, `prepend` puts them first,
    /// `auto` replaces when a path matched with its qualifiers and prepends
    /// when it matched by the last segment alone. The response's `trait_seeds`
    /// says how many start nodes they are.
    #[arg(long, default_value = "auto")]
    trait_seeds: local_code_intelligence::app::traverse::TraitSeedMode,
    /// Continue the traversal this handle names (it must be under --out, or
    /// under the data directory without it).
    #[arg(long)]
    handle: Option<String>,
    /// With --handle: the answer to the checkpoint, a JSON file or inline
    /// JSON, an array of actions or `{"actions": [..]}`.
    #[arg(long, requires = "handle")]
    steer: Option<String>,
    /// Write `traces/` and `handles/` under this directory instead of the
    /// data directory.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[derive(clap::Args)]
struct RecipeArgs {
    /// The recipe: config_readers, impact_rings, flow_path, flow_from,
    /// config_neighbours or lci_compose.
    name: String,
    /// The tree the index describes (also holds `config.example.toml`).
    #[arg(long)]
    root: PathBuf,
    /// The SCIP index (default: `[traverse] scip_index` under --root).
    #[arg(long)]
    scip: Option<PathBuf>,
    /// The recipe's arguments, inline JSON or the path of a JSON file
    /// (default `{}`).
    #[arg(long)]
    args: Option<String>,
    /// Write `traces/` and `handles/` of flow_from under this directory
    /// instead of the data directory.
    #[arg(long)]
    out: Option<PathBuf>,
}

/// `recipe`: one recipe over a SCIP index, printed as JSON. Runs without
/// opening the application.
async fn run_recipe(
    config_path: Option<PathBuf>,
    no_local_config: bool,
    args: RecipeArgs,
) -> Result<()> {
    use local_code_intelligence::app::{
        recipes::{self, Ctx, Request},
        traverse::Dirs,
    };
    let spec = ResolveSpec::from_process(
        config_path,
        Flags {
            port: None,
            no_local_config,
        },
    )?;
    let config = layers::resolve(&spec)?.config;
    let root = dunce::canonicalize(&args.root)
        .with_context(|| format!("the tree {}", args.root.display()))?;
    let mut recipe_args = match &args.args {
        Some(text) => json_argument(text)?,
        None => json!({}),
    };
    let object = recipe_args
        .as_object_mut()
        .context("--args must be a JSON object")?;
    object
        .entry("workspace_path")
        .or_insert_with(|| json!(root.to_string_lossy()));
    let request = Request::parse(&args.name, &recipe_args)?;
    let scip = args
        .scip
        .unwrap_or_else(|| root.join(&config.traverse.scip_index));
    let dirs = match &args.out {
        Some(out) => Dirs {
            traces: out.join("traces"),
            handles: out.join("handles"),
            trajectory_candidates: 0,
        },
        None => Dirs::under(&config.data_dir),
    };
    let ctx = Ctx {
        limits: &config.traverse,
        profile: &config.run_profile,
        dirs: &dirs,
        scip_index: &scip,
        root: &root,
        flow_judge: config.recipes.flow_judge,
    };
    let mut answer = recipes::run(&ctx, &request).await?;
    stamp(&mut answer, &config.run_profile().hash);
    println!("{}", serde_json::to_string_pretty(&answer)?);
    Ok(())
}

/// JSON given inline or as the path of a file.
fn json_argument(text: &str) -> Result<Value> {
    let trimmed = text.trim_start();
    let raw = if trimmed.starts_with('{') || trimmed.starts_with('[') {
        text.to_owned()
    } else {
        std::fs::read_to_string(text).with_context(|| format!("read {text}"))?
    };
    serde_json::from_str(&raw).with_context(|| format!("parse the JSON in {text}"))
}

/// `traverse`: one traversal call over a SCIP index, printed as JSON. Runs
/// without opening the application.
async fn run_traverse(
    config_path: Option<PathBuf>,
    no_local_config: bool,
    args: TraverseArgs,
) -> Result<()> {
    use local_code_intelligence::app::traverse::{self, BudgetArgs, Call, Dirs, SteerArgs};
    if args.question.is_none() && args.seeds.is_empty() && args.handle.is_none() {
        anyhow::bail!("give --question or --seeds, or --handle to continue a traversal");
    }
    let spec = ResolveSpec::from_process(
        config_path,
        Flags {
            port: None,
            no_local_config,
        },
    )?;
    let mut config = layers::resolve(&spec)?.config;
    if let Some(fusion) = args.fusion {
        config.traverse.fusion = fusion;
    }
    config.traverse.pass_through |= args.pass_through;
    config.traverse.rerank_frontier |= args.rerank_frontier;
    config.traverse.score_bound &= !args.no_score_bound;
    config.traverse.priors = args.priors.clone().or(config.traverse.priors);
    config.traverse.question_type = args.question_type.clone().or(config.traverse.question_type);
    config.traverse.tie_break = args.tie_break.clone().or(config.traverse.tie_break);
    if let Some(first_stage) = args.first_stage {
        config.traverse.first_stage = first_stage;
    }
    if let Some(first_k) = args.first_k {
        config.traverse.first_k = first_k;
        // An explicit --first-k also wins over the named policy's own value.
        if let Some(name) = args
            .policy
            .as_deref()
            .filter(|name| traverse::presets::is_preset(name))
        {
            config
                .traverse
                .policies
                .entry(name.to_owned())
                .or_default()
                .first_k = Some(first_k);
        }
    }
    if let Some(answer_seeds) = args.answer_seeds {
        config.traverse.answer_seeds = answer_seeds;
    }
    if let Some(exact_seeds) = args.exact_seeds {
        config.traverse.exact_seeds = exact_seeds;
    }
    // The overrides come after the configuration was checked: check it again.
    config.validate().context("traverse options")?;
    let mut steer = match &args.policy {
        None => SteerArgs::default(),
        Some(name) if traverse::presets::is_preset(name) => SteerArgs {
            policy: Some(name.clone()),
            ..SteerArgs::default()
        },
        Some(text) => match json_argument(text)? {
            rules @ Value::Array(_) => SteerArgs {
                rules: Some(rules),
                ..SteerArgs::default()
            },
            object => serde_json::from_value(object).context("--policy")?,
        },
    };
    if args.stops.is_some() {
        steer.stop_every = args.stops;
    }
    if let Some(text) = &args.steer {
        steer.actions = Some(match json_argument(text)? {
            Value::Object(mut object) => object
                .remove("actions")
                .context("--steer: the object has no `actions`")?,
            actions => actions,
        });
    }
    let budget = BudgetArgs {
        pops: args.pops,
        max_depth: args.max_depth,
        ..BudgetArgs::default()
    };
    let dirs = match &args.out {
        Some(out) => Dirs {
            traces: out.join("traces"),
            handles: out.join("handles"),
            trajectory_candidates: 0,
        },
        None => Dirs::under(&config.data_dir),
    };
    // The flag applies with and without `--out`.
    let dirs = Dirs {
        trajectory_candidates: if args.trajectory {
            lci_maps::traverse::TRAJECTORY_CANDIDATES
        } else {
            0
        },
        ..dirs
    };
    let maps = (!args.maps.is_empty()).then_some(args.maps.as_slice());
    // The hybrid search needs the application (index store, model services);
    // open it only when the start nodes come from a question over a tree.
    let wants_search = args.seeds.is_empty()
        && args.handle.is_none()
        && args.question.is_some()
        && (maps.is_none_or(|ids| ids.iter().any(|id| id == traverse::SEEDS_HYBRID)));
    // The maps search.lexical and search.vector read the index too, from
    // whatever start nodes the call has.
    let wants_chunks = maps.is_some_and(|ids| {
        ids.iter().any(|id| {
            id == lci_maps::search_maps::SEARCH_LEXICAL_ID
                || id == lci_maps::search_maps::SEARCH_VECTOR_ID
        })
    });
    // The reranker frontier calls the reranker through the application.
    let wants_rerank = config.traverse.rerank_frontier;
    let app = match (&args.root, wants_search || wants_chunks || wants_rerank) {
        (Some(_), true) => Some(App::open(config.clone()).await),
        _ => None,
    };
    let seed_search: Option<Box<dyn traverse::SeedSearch + '_>> = match (&app, &args.root) {
        (Some(Ok(app)), Some(root)) => Some(Box::new(traverse::AppSeeds {
            app,
            workspace: root,
            profile: args.search_profile.as_deref(),
        })),
        (Some(Err(error)), _) => Some(Box::new(traverse::NoSearch(format!(
            "the application did not open: {error:#}"
        )))),
        _ => None,
    };
    // `[run_profile.lsp] sessions = "shared"` (the default): this process's one
    // manager, which a CLI call has no other user of, holds the sessions of the
    // servers that see an edit on their own (and every server under `sync =
    // "watch"`); the other servers start per call. Stopped here (cleanly, once
    // the walk ends) instead of at process exit, so a one-shot call leaves no
    // server running. Built only when the call can run lsp.live, so a walk
    // without it starts no idle sweeper and waits for no shutdown.
    let lsp_sessions = (config.run_profile.lsp.sessions
        == local_code_intelligence::config::LspSessions::Shared
        && may_walk_lsp_live(&config, args.root.is_some(), maps))
    .then(|| std::sync::Arc::new(lsp::Manager::new(&config)));
    let report = traverse::run_with_trait_seeds(
        &config.traverse,
        &config.run_profile,
        &dirs,
        &Call {
            scip_index: &args.scip,
            scip_root: args.root.as_deref(),
            question: args.question.as_deref().unwrap_or_default(),
            seeds: &args.seeds,
            seed_search: seed_search.as_deref(),
            top_k: args.top_k,
            maps,
            budget: &budget,
            steer: &steer,
            handle: args.handle.as_deref(),
            lsp: Some(&config),
            lsp_sessions: lsp_sessions.clone(),
        },
        args.trait_seeds,
    )
    .await;
    if let Some(sessions) = &lsp_sessions {
        sessions.shutdown_all().await;
    }
    let report = report?;
    if let Some(trace) = &report.trace {
        eprintln!("trace {}", trace.display());
    }
    let mut response = report.response;
    stamp(&mut response, &config.run_profile().hash);
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(())
}

/// Whether a `traverse` call can run `lsp.live`: it reads the tree, so the
/// call needs `--root`, and it is opt-in, so `--maps` names it or, without
/// `--maps`, a configured `[traverse.policies]` entry lists it in
/// `extra_maps` (the built-in policies never do).
fn may_walk_lsp_live(config: &Config, has_root: bool, maps: Option<&[String]>) -> bool {
    let lsp_live = |id: &String| id == lci_maps::lsp_live::LSP_LIVE_ID;
    has_root
        && match maps {
            Some(ids) => ids.iter().any(lsp_live),
            None => config
                .traverse
                .policies
                .values()
                .any(|policy| policy.extra_maps.iter().flatten().any(lsp_live)),
        }
}

fn parse_resamples(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(count) => Ok(count),
        Err(error) => Err(error.to_string()),
    }
}

fn parse_window(value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(0) => Err("must be at least 1".to_owned()),
        Ok(window) => Ok(window),
        Err(error) => Err(error.to_string()),
    }
}

fn parse_cap(value: &str) -> Result<u32, String> {
    let cap = value.parse::<u32>().map_err(|error| error.to_string())?;
    if cap > local_code_intelligence::config::FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT {
        return Err(format!(
            "must be 0..={}",
            local_code_intelligence::config::FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT
        ));
    }
    Ok(cap)
}

fn parse_positive(value: &str) -> Result<f32, String> {
    let number = value.parse::<f32>().map_err(|error| error.to_string())?;
    if number.is_finite() && number > 0.0 {
        Ok(number)
    } else {
        Err("must be a finite number greater than zero".to_owned())
    }
}

fn parse_non_negative(value: &str) -> Result<f32, String> {
    let number = value.parse::<f32>().map_err(|error| error.to_string())?;
    if number.is_finite() && number >= 0.0 {
        Ok(number)
    } else {
        Err("must be a finite number, zero or more".to_owned())
    }
}

/// `replay`: read a candidate dump, score the replayed arm, print the summary,
/// optionally write the replayed report. Runs without opening the application.
fn run_replay(source: &Path, output: Option<&Path>, options: &ReplayOptions) -> Result<()> {
    let replayed = evaluate::replay_file(source, options)?;
    print!("{}", replayed.render_summary());
    if let Some(path) = output {
        let json = serde_json::to_string_pretty(&replayed.to_json()?)?;
        std::fs::write(
            path,
            format!(
                "{json}
"
            ),
        )?;
    }
    Ok(())
}

/// `compare`: read two saved reports, print the summary, optionally write the
/// JSON report. Runs without opening the application.
fn run_compare(
    baseline: &Path,
    candidate: &Path,
    output: Option<&Path>,
    options: &CompareOptions,
) -> Result<()> {
    let comparison = evaluate::compare_files(baseline, candidate, options)?;
    print!("{}", comparison.render_summary());
    if let Some(path) = output {
        let json = serde_json::to_string_pretty(&comparison)?;
        std::fs::write(path, format!("{json}\n"))?;
    }
    Ok(())
}

/// The URLs the resolved port and search profiles imply, and the embedding
/// identity the configuration would index under.
fn derived_values(config: &Config) -> Value {
    let base = format!("http://127.0.0.1:{}", config.port);
    let profiles: Map<String, Value> = config
        .search_profiles
        .keys()
        .map(|name| {
            (
                name.clone(),
                Value::from(format!("{base}{}", server::mcp_path(Some(name)))),
            )
        })
        .collect();
    json!({
        "mcp_url": format!("{base}{}", server::mcp_path(None)),
        "mcp_profile_urls": profiles,
        "rest_base": format!("{base}/v1"),
        "health_url": format!("{base}/health"),
        "ready_url": format!("{base}/ready"),
        "openapi_url": format!("{base}/openapi.json"),
        "embedding_identity": config.embedding_identity(),
        "run_profile_hash": config.run_profile().hash,
    })
}

/// Whether the process environment asks for a candidate dump, which is
/// process-level behaviour rather than configuration (see
/// [`CANDIDATE_DUMP_ENV`]); reported next to the configuration.
fn candidate_dump_state(env: &BTreeMap<String, String>) -> &'static str {
    match env.get(CANDIDATE_DUMP_ENV) {
        Some(value) if !value.trim().is_empty() => "set",
        _ => "unset",
    }
}

/// The `config --json` object.
fn config_report_json(
    resolved: &Resolved,
    env: &BTreeMap<String, String>,
    validity: &Result<()>,
) -> Result<Value> {
    let mut values = Map::new();
    for entry in resolved.entries()? {
        values.insert(
            entry.key,
            json!({
                "value": entry.value,
                "source": entry.provenance.source,
                "from": entry.provenance.from,
            }),
        );
    }
    let mut other = Map::new();
    other.insert(
        CANDIDATE_DUMP_ENV.to_owned(),
        Value::from(candidate_dump_state(env)),
    );
    Ok(json!({
        "valid": validity.is_ok(),
        "error": validity.as_ref().err().map(|error| format!("{error:#}")),
        "layers": resolved.layers,
        "values": values,
        "config": resolved.config,
        "derived": derived_values(&resolved.config),
        "environment": {"other": other},
    }))
}

/// A configuration value as written: strings bare (so it can be searched for
/// in the file it came from; one with a line break is quoted so the report
/// stays one setting per line), unset optional values as `unset`.
fn display_value(value: &Value) -> String {
    match value {
        Value::String(text) if !text.chars().any(char::is_control) => text.clone(),
        Value::Null => "unset".to_owned(),
        other => other.to_string(),
    }
}

fn describe_source(provenance: &Provenance) -> String {
    match &provenance.from {
        Some(from) => format!("{}, {from}", provenance.source.as_str()),
        None => provenance.source.as_str().to_owned(),
    }
}

/// The `config` text report.
fn config_report_text(
    resolved: &Resolved,
    env: &BTreeMap<String, String>,
    validity: &Result<()>,
) -> Result<String> {
    let mut out = String::new();
    match validity {
        Ok(()) => writeln!(out, "configuration: valid")?,
        Err(error) => writeln!(out, "configuration: INVALID: {error:#}")?,
    }
    let layers = &resolved.layers;
    writeln!(out, "\nlayers (lowest to highest)")?;
    writeln!(out, "  default: built in")?;
    match (&layers.file.path, layers.file.named_by) {
        (Some(path), Some(named_by)) => {
            writeln!(out, "  file:    {} ({named_by})", path.display())?;
        }
        _ => writeln!(
            out,
            "  file:    none (built-in defaults; pass --config or set LCI_CONFIG)"
        )?,
    }
    match &layers.local.path {
        Some(path) => {
            let state = match layers.local.state {
                LocalState::Disabled => "disabled by --no-local-config",
                state => state.as_str(),
            };
            writeln!(
                out,
                "  local:   {} ({state}; {})",
                path.display(),
                layers.local.named_by.unwrap_or("unknown")
            )?;
        }
        None => writeln!(
            out,
            "  local:   none (this platform has no per-user configuration directory; pass --config or set LCI_LOCAL_CONFIG)"
        )?,
    }
    if layers.env.is_empty() {
        writeln!(out, "  env:     none")?;
    } else {
        writeln!(out, "  env:     {}", layers.env.join(", "))?;
    }
    if layers.flags.is_empty() {
        writeln!(out, "  flags:   none")?;
    } else {
        writeln!(out, "  flags:   {}", layers.flags.join(", "))?;
    }

    let entries = resolved.entries()?;
    let width = entries
        .iter()
        .filter(|entry| entry.machine)
        .map(|entry| entry.key.len())
        .max()
        .unwrap_or(0);
    writeln!(
        out,
        "\nmachine settings (the local overlay and LCI_* variables may set these)"
    )?;
    for entry in entries.iter().filter(|entry| entry.machine) {
        writeln!(
            out,
            "  {:<width$} = {}  (source: {})",
            entry.key,
            display_value(&entry.value),
            describe_source(&entry.provenance)
        )?;
    }

    let derived = derived_values(&resolved.config);
    writeln!(out, "\nderived")?;
    for key in [
        "mcp_url",
        "rest_base",
        "health_url",
        "ready_url",
        "openapi_url",
        "embedding_identity",
    ] {
        writeln!(out, "  {key:<18} = {}", display_value(&derived[key]))?;
    }
    if let Some(profiles) = derived["mcp_profile_urls"].as_object() {
        for (name, url) in profiles {
            let label = format!("mcp_url ({name})");
            writeln!(out, "  {label:<18} = {}", display_value(url))?;
        }
    }

    match &layers.file.path {
        Some(path) => writeln!(
            out,
            "\nother settings (from {}; * marks a value that file sets, the rest are built-in defaults)",
            path.display()
        )?,
        None => writeln!(out, "\nother settings (all built-in defaults)")?,
    }
    for entry in entries.iter().filter(|entry| !entry.machine) {
        let mark = if entry.provenance.source == Source::File {
            '*'
        } else {
            ' '
        };
        writeln!(
            out,
            "{mark} {} = {}",
            entry.key,
            display_value(&entry.value)
        )?;
    }

    writeln!(out, "\nprocess environment (not configuration)")?;
    writeln!(out, "  {CANDIDATE_DUMP_ENV}: {}", candidate_dump_state(env))?;
    Ok(out)
}

/// `config`: resolve the layers and print every value with its source.
/// Runs without opening the application: it creates no directory, opens no
/// database, sends no request and starts no language server. An invalid
/// result is still printed, then exits 2; a layer that cannot be read exits 1.
fn run_config(config_path: Option<PathBuf>, no_local_config: bool, json: bool) -> Result<()> {
    let flags = Flags {
        port: None,
        no_local_config,
    };
    let resolution = ResolveSpec::from_process(config_path, flags).and_then(|spec| {
        let resolved = layers::resolve_unvalidated(&spec)?;
        Ok((spec, resolved))
    });
    let (spec, resolved) = match resolution {
        Ok(pair) => pair,
        Err(error) => {
            if json {
                let failure = json!({"valid": false, "error": format!("{error:#}")});
                println!("{}", serde_json::to_string_pretty(&failure)?);
            }
            return Err(error);
        }
    };
    let validity = resolved.config.validate();
    let text = if json {
        let report = config_report_json(&resolved, &spec.env, &validity)?;
        format!("{}\n", serde_json::to_string_pretty(&report)?)
    } else {
        config_report_text(&resolved, &spec.env, &validity)?
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    drop(stdout);
    if validity.is_err() {
        std::process::exit(2);
    }
    Ok(())
}

/// `init`: detect this machine's tools and write the local overlay. Runs
/// without opening the application. Exits 3, writing nothing, when the
/// overlay exists and `--force` was not given.
async fn run_init(config_path: Option<PathBuf>, options: init::Options, json: bool) -> Result<()> {
    // The overlay being replaced must not count as a layer, whatever
    // `--no-local-config` says.
    let flags = Flags {
        port: None,
        no_local_config: true,
    };
    let outcome = match ResolveSpec::from_process(config_path, flags) {
        Ok(spec) => init::run(&spec, &options).await,
        Err(error) => Err(error),
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            if json {
                let failure = json!({"written": false, "error": format!("{error:#}")});
                println!("{}", serde_json::to_string_pretty(&failure)?);
            }
            return Err(error);
        }
    };
    for warning in &outcome.warnings {
        eprintln!("{warning}");
    }
    let exists = outcome.status == init::Status::Exists;
    let text = if json {
        format!("{}\n", serde_json::to_string_pretty(&outcome.report)?)
    } else if exists {
        String::new()
    } else {
        outcome.render_text()
    };
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()?;
    drop(stdout);
    if exists {
        eprint!("{}", outcome.render_text());
        std::process::exit(3);
    }
    Ok(())
}

/// Default log filter (used when `RUST_LOG` is unset); `lci_core` is listed because LSP restart, readiness-timeout, and JSON-RPC reader events come from that crate.
const DEFAULT_LOG_FILTER: &str = "local_code_intelligence=info,lci_core=info";

#[tokio::main]
async fn main() -> Result<std::process::ExitCode> {
    match run().await {
        Ok(()) => Ok(std::process::ExitCode::SUCCESS),
        Err(error) => match error.downcast_ref::<ShutdownSignalError>() {
            Some(signal) => {
                eprintln!("Error: {error:#}");
                Ok(std::process::ExitCode::from(signal.0.exit_code()))
            }
            None => Err(error),
        },
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Serve { port: None });
    if let Command::Compare {
        baseline,
        candidate,
        output,
        seed,
        resamples,
        keep_failed,
    } = command
    {
        return run_compare(
            &baseline,
            &candidate,
            output.as_deref(),
            &CompareOptions {
                seed,
                resamples,
                keep_failed,
            },
        );
    }
    if let Command::Replay {
        report,
        output,
        window,
        cap,
        cap_stage,
        semantic_weight,
        lexical_weight,
        lsp_weight,
        rrf_k,
        prior,
        rerank_mode,
        vote_weight,
    } = command
    {
        return run_replay(
            &report,
            output.as_deref(),
            &ReplayOptions {
                window,
                cap,
                cap_stage,
                semantic_weight,
                lexical_weight,
                lsp_weight,
                rrf_k,
                prior,
                rerank_mode,
                vote_weight,
            },
        );
    }
    if let Command::Traverse(args) = command {
        // Boxed: the traversal future is large, and `run` shares the main
        // thread's stack (1 MiB on Windows) with it in a debug build.
        return Box::pin(run_traverse(cli.config, cli.no_local_config, *args)).await;
    }
    if let Command::Recipe(args) = command {
        return run_recipe(cli.config, cli.no_local_config, args).await;
    }
    if let Command::Config { json } = command {
        return run_config(cli.config, cli.no_local_config, json);
    }
    if let Command::Init {
        force,
        dry_run,
        no_probe,
        json,
    } = command
    {
        let options = init::Options {
            force,
            dry_run,
            no_probe,
        };
        return run_init(cli.config, options, json).await;
    }
    // Read the baseline report before opening the application, so an
    // unreadable path fails now rather than after a long evaluation.
    let baseline_report = match &command {
        Command::Evaluate {
            baseline: Some(path),
            ..
        } => Some((path.clone(), evaluate::load_report(path)?)),
        _ => None,
    };
    let serve_port = match &command {
        Command::Serve { port } => *port,
        _ => None,
    };
    let spec = ResolveSpec::from_process(
        cli.config,
        Flags {
            port: serve_port,
            no_local_config: cli.no_local_config,
        },
    )?;
    let app = Arc::new(App::open(layers::resolve(&spec)?.config).await?);
    let mut evaluation_failed = false;
    let mut output_path = None;
    let output = match command {
        Command::Index { workspace } => serde_json::to_value(app.index(&workspace).await?)?,
        Command::Status { workspace } => serde_json::to_value(app.status(&workspace).await?)?,
        Command::ListFiles { workspace } => {
            serde_json::to_value(app.indexed_files(&workspace).await?)?
        }
        Command::Search {
            workspace,
            query,
            top_k,
            languages,
            include_paths,
            exclude_paths,
            source_roles,
            search_profile,
        } => serde_json::to_value(
            until_shutdown_signal(
                &app,
                app.search_in_profile(
                    &workspace,
                    &query,
                    top_k,
                    local_code_intelligence::filter::FilterRequest {
                        languages: (!languages.is_empty()).then_some(languages),
                        include_paths: (!include_paths.is_empty()).then_some(include_paths),
                        exclude_paths: (!exclude_paths.is_empty()).then_some(exclude_paths),
                        source_roles: (!source_roles.is_empty()).then_some(source_roles),
                    },
                    search_profile.as_deref(),
                ),
            )
            .await?,
        )?,
        Command::Evaluate {
            definition,
            workspaces,
            output,
            baseline: _,
            seed,
            resamples,
            dump_candidates,
            no_prime,
            keep_failed,
        } => {
            let definition = evaluate::load_definition(&definition)?;
            let workspaces = evaluate::parse_workspace_mappings(&workspaces)?;
            output_path = output;
            // The comparison is inside the guarded future too: once the signal
            // handlers are installed they stay for the life of the process, so
            // a step after `until_shutdown_signal` could not be interrupted.
            // It runs on a blocking thread so a signal is seen meanwhile.
            let (report, comparison) = until_shutdown_signal(&app, async {
                let report = evaluate::run_evaluation_with_options(
                    &app,
                    &definition,
                    &workspaces,
                    RunOptions {
                        dump_candidates,
                        prime: !no_prime,
                    },
                )
                .await?;
                Ok(tokio::task::spawn_blocking(move || {
                    let comparison = baseline_report.map(|(path, baseline)| {
                        let options = CompareOptions {
                            seed: seed.unwrap_or(DEFAULT_SEED),
                            resamples: resamples.unwrap_or(DEFAULT_RESAMPLES),
                            keep_failed,
                        };
                        let mut comparison = evaluate::compare(&baseline, &report, &options);
                        comparison.baseline_source = Some(path.display().to_string());
                        comparison
                    });
                    (report, comparison)
                })
                .await?)
            })
            .await?;
            evaluation_failed = evaluate::has_failures(&report);
            if let Some(comparison) = &comparison {
                // Stderr, so a report on stdout stays valid JSON.
                eprint!("{}", comparison.render_summary());
            }
            evaluate::report_with_comparison(&report, comparison.as_ref())?
        }
        Command::Config { .. } => unreachable!("config returns before the application opens"),
        Command::Init { .. } => unreachable!("init returns before the application opens"),
        Command::Compare { .. } => unreachable!("compare returns before the application opens"),
        Command::Replay { .. } => unreachable!("replay returns before the application opens"),
        Command::Traverse(_) => unreachable!("traverse returns before the application opens"),
        Command::Recipe(_) => unreachable!("recipe returns before the application opens"),
        Command::Symbols { workspace, query } => serde_json::to_value(
            until_shutdown_signal(&app, app.symbols(&workspace, &query)).await?,
        )?,
        Command::Definition {
            workspace,
            relative_file_path,
            line,
            character,
        } => serde_json::to_value(
            until_shutdown_signal(
                &app,
                app.definition(&workspace, &relative_file_path, line, character),
            )
            .await?,
        )?,
        Command::References {
            workspace,
            relative_file_path,
            line,
            character,
            include_declaration,
        } => serde_json::to_value(
            until_shutdown_signal(
                &app,
                app.references(
                    &workspace,
                    &relative_file_path,
                    line,
                    character,
                    include_declaration,
                ),
            )
            .await?,
        )?,
        Command::Serve { .. } => {
            // The resolved port: `--port`, else `LCI_PORT`, the local
            // overlay, the configuration file, then 8768.
            let port = app.config.port;
            let cancellation = tokio_util::sync::CancellationToken::new();
            let profile_endpoints: Vec<String> = app
                .config
                .search_profiles
                .keys()
                .map(|name| format!("http://127.0.0.1:{port}{}", server::mcp_path(Some(name))))
                .collect();
            let router = server::router(Arc::clone(&app), cancellation.clone(), port);
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
            tracing::info!(
                "MCP: http://127.0.0.1:{port}/mcp; health: http://127.0.0.1:{port}/health"
            );
            if !profile_endpoints.is_empty() {
                tracing::info!(
                    "MCP search profiles (search_code bound to the profile; tool list and result shape per its tools and response keys): {}",
                    profile_endpoints.join(", ")
                );
            }
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
            let shutdown = async move {
                let signal = shutdown_signal(cancellation).await;
                let _ = shutdown_tx.send(signal);
            };
            let served = axum::serve(listener, router)
                .with_graceful_shutdown(shutdown)
                .await;
            // The server has stopped, gracefully (Ctrl+C or SIGTERM) or not.
            release_language_servers(&app).await;
            served?;
            return Err(ShutdownSignalError(shutdown_rx.await?).into());
        }
    };
    // Every result of the application commands (search, index, status,
    // list-files, navigation, evaluate) carries the profile hash, as it does
    // over MCP and REST.
    let mut output = output;
    stamp(&mut output, app.profile_hash());
    let json = serde_json::to_string_pretty(&output)?;
    if let Some(path) = output_path {
        std::fs::write(&path, format!("{json}\n"))?;
    } else {
        println!("{json}");
    }
    if evaluation_failed {
        anyhow::bail!("one or more required evaluation expectations failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LOG_FILTER, ShutdownSignal, cancel_on_signal};
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt};

    struct Recorder(Arc<Mutex<Vec<String>>>);

    #[cfg(unix)]
    #[test]
    fn shutdown_signal_exit_codes_follow_unix_conventions() {
        assert_eq!(ShutdownSignal::Interrupt.exit_code(), 130);
        assert_eq!(ShutdownSignal::Terminate.exit_code(), 143);
    }

    #[cfg(windows)]
    #[test]
    fn ctrl_c_exit_code_uses_the_sigint_convention() {
        assert_eq!(ShutdownSignal::Interrupt.exit_code(), 130);
    }

    #[tokio::test]
    async fn a_shutdown_signal_error_does_not_cancel() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let signal = std::future::ready(Err(std::io::Error::other("registration failed")));

        assert!(!cancel_on_signal(signal, cancellation.clone()).await);

        assert!(!cancellation.is_cancelled());
    }

    #[tokio::test]
    async fn a_shutdown_signal_success_cancels() {
        let cancellation = tokio_util::sync::CancellationToken::new();
        let signal = std::future::ready(Ok(()));

        assert!(cancel_on_signal(signal, cancellation.clone()).await);

        assert!(cancellation.is_cancelled());
    }

    impl<S: tracing::Subscriber> Layer<S> for Recorder {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(event.metadata().target().to_string());
        }
    }

    #[test]
    fn default_log_filter_enables_app_and_core_at_info() {
        let filter = EnvFilter::try_new(DEFAULT_LOG_FILTER).expect("default filter parses");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let subscriber =
            tracing_subscriber::registry().with(Recorder(seen.clone()).with_filter(filter));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "local_code_intelligence::app", "app info");
            tracing::info!(target: "lci_core::lsp", "core info");
            tracing::warn!(target: "lci_core::lsp", "core warn");
            tracing::debug!(target: "lci_core::lsp", "core debug");
            tracing::info!(target: "some_other_crate", "other info");
        });
        assert_eq!(
            *seen.lock().unwrap(),
            [
                "local_code_intelligence::app",
                "lci_core::lsp",
                "lci_core::lsp"
            ]
        );
    }

    #[test]
    fn the_cli_builds_a_shared_manager_only_for_a_call_that_can_walk_lsp_live() {
        use super::may_walk_lsp_live;
        use local_code_intelligence::config::{Config, TraversePolicy};
        let names = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();
        let plain = Config::default();
        let (with, without) = (names(&["graph.scip", "lsp.live"]), names(&["graph.scip"]));
        assert!(may_walk_lsp_live(&plain, true, Some(&with)));
        assert!(!may_walk_lsp_live(&plain, false, Some(&with)), "no tree");
        assert!(!may_walk_lsp_live(&plain, true, Some(&without)));
        assert!(!may_walk_lsp_live(&plain, true, None), "the default maps");
        let mut policy = plain.clone();
        policy.traverse.policies.insert(
            "flow".into(),
            TraversePolicy {
                extra_maps: Some(names(&["lsp.live"])),
                ..TraversePolicy::default()
            },
        );
        assert!(
            may_walk_lsp_live(&policy, true, None),
            "a policy's extra map"
        );
        assert!(!may_walk_lsp_live(&policy, true, Some(&without)));
    }
}
