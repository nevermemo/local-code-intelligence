use anyhow::Result;
use clap::{Parser, Subcommand};
use local_code_intelligence::{
    app::{App, CANDIDATE_DUMP_ENV},
    config::{
        Config,
        layers::{self, Flags, LocalState, Provenance, ResolveSpec, Resolved, Source},
    },
    evaluate::{
        self, CapStage, CompareOptions, DEFAULT_RESAMPLES, DEFAULT_SEED, ReplayOptions, RerankMode,
        RunOptions,
    },
    server,
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    io::Write as _,
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Parser)]
#[command(
    version,
    about = "Standalone local Rust, TypeScript, JavaScript, Python, C#, Go, Java, C, and C++ code retrieval over MCP or plain REST/OpenAPI, with optional language-server navigation for every language (rust-analyzer, csharp-ls, typescript-language-server, pyright, gopls, jdtls, clangd)"
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "local_code_intelligence=info".into()),
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
    } = command
    {
        return run_compare(
            &baseline,
            &candidate,
            output.as_deref(),
            &CompareOptions { seed, resamples },
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
    if let Command::Config { json } = command {
        return run_config(cli.config, cli.no_local_config, json);
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
        } => {
            let definition = evaluate::load_definition(&definition)?;
            let workspaces = evaluate::parse_workspace_mappings(&workspaces)?;
            let report = evaluate::run_evaluation_with_options(
                &app,
                &definition,
                &workspaces,
                RunOptions { dump_candidates },
            )
            .await?;
            evaluation_failed = evaluate::has_failures(&report);
            output_path = output;
            let comparison = baseline_report.map(|(path, baseline)| {
                let options = CompareOptions {
                    seed: seed.unwrap_or(DEFAULT_SEED),
                    resamples: resamples.unwrap_or(DEFAULT_RESAMPLES),
                };
                let mut comparison = evaluate::compare(&baseline, &report, &options);
                comparison.baseline_source = Some(path.display().to_string());
                comparison
            });
            if let Some(comparison) = &comparison {
                // Stderr, so a report on stdout stays valid JSON.
                eprint!("{}", comparison.render_summary());
            }
            evaluate::report_with_comparison(&report, comparison.as_ref())?
        }
        Command::Config { .. } => unreachable!("config returns before the application opens"),
        Command::Compare { .. } => unreachable!("compare returns before the application opens"),
        Command::Replay { .. } => unreachable!("replay returns before the application opens"),
        Command::Symbols { workspace, query } => {
            serde_json::to_value(app.symbols(&workspace, &query).await?)?
        }
        Command::Definition {
            workspace,
            relative_file_path,
            line,
            character,
        } => serde_json::to_value(
            app.definition(&workspace, &relative_file_path, line, character)
                .await?,
        )?,
        Command::References {
            workspace,
            relative_file_path,
            line,
            character,
            include_declaration,
        } => serde_json::to_value(
            app.references(
                &workspace,
                &relative_file_path,
                line,
                character,
                include_declaration,
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
            let router = server::router(app, cancellation.clone(), port);
            let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
            tracing::info!(
                "MCP: http://127.0.0.1:{port}/mcp; health: http://127.0.0.1:{port}/health"
            );
            if !profile_endpoints.is_empty() {
                tracing::info!(
                    "MCP search profiles (same tools, search_code bound to the profile): {}",
                    profile_endpoints.join(", ")
                );
            }
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    cancellation.cancel();
                })
                .await?;
            return Ok(());
        }
    };
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
