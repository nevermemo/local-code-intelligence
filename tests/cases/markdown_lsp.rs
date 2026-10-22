//! The marksman adapter (provider `marksman`, language `markdown`) through the
//! application: navigation reaches it for an indexed prose file, the search's
//! LSP channel never starts it, and a default install (prose off, or no
//! `[markdown]` path) never runs it. The fake language server answers canned
//! locations; real marksman is covered by `cargo xtask acceptance
//! markdown-lsp`.

use super::*;
use local_code_intelligence::{app::Status, config::MarkdownLspConfig};
use std::path::PathBuf;

const GUIDE: &str = "# Calculator guide\n\n## Adding numbers\n\nThe add function returns the sum of two integers.\n";
const INDEX: &str =
    "# Index\n\nSee [adding numbers](guide.md#adding-numbers) for the calculator.\n";

/// A prose-only workspace (`docs/guide.md`, `docs/index.md`) and a config whose
/// `[markdown]` path is the fake language server in `mode`, with the state
/// directory it records into.
fn prose_fixture(
    temp: &tempfile::TempDir,
    mut config: Config,
    mode: &str,
) -> (Config, PathBuf, PathBuf) {
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    write(&workspace, "docs/guide.md", GUIDE);
    write(&workspace, "docs/index.md", INDEX);
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.markdown = MarkdownLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: [
            "--mode",
            mode,
            "--answer-unhandled",
            "--state",
            &state.to_string_lossy(),
            "--primary-file",
            "docs/guide.md",
            "--secondary-file",
            "docs/index.md",
        ]
        .map(str::to_owned)
        .to_vec(),
        disabled: false,
    };
    (config, workspace, state)
}

fn marksman_running(status: &Status) -> bool {
    status.analyzers_running["marksman"]
}

fn files(report: &local_code_intelligence::app::NavigationReport) -> Vec<&str> {
    let mut files: Vec<&str> = report
        .results
        .iter()
        .filter_map(|location| location.relative_file_path.as_deref())
        .collect();
    files.sort_unstable();
    files.dedup();
    files
}

/// Every location is a markdown location of provider `marksman`.
fn assert_marksman_locations(report: &local_code_intelligence::app::NavigationReport) {
    assert!(!report.results.is_empty());
    for location in &report.results {
        assert_eq!(
            location.language.as_deref(),
            Some("markdown"),
            "{location:?}"
        );
        assert_eq!(
            location.provider.as_deref(),
            Some("marksman"),
            "{location:?}"
        );
    }
}

/// `definition` at a link and `references` at a heading go to marksman for
/// indexed prose files, and the session shows in `analyzers_running`.
#[tokio::test]
async fn definition_and_references_reach_marksman_for_an_indexed_prose_file() {
    let (temp, config, _fake, models_task) = fixture().await;
    let (config, workspace, _state) = prose_fixture(&temp, config, "definition-references");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    assert!(!marksman_running(&app.status(&workspace).await.unwrap()));

    let definition = app
        .definition(&workspace, "docs/index.md", 3, 22)
        .await
        .unwrap();
    assert_marksman_locations(&definition);
    assert!(
        files(&definition).contains(&"docs/guide.md"),
        "{definition:?}"
    );

    let references = app
        .references(&workspace, "docs/guide.md", 3, 4, true)
        .await
        .unwrap();
    assert_marksman_locations(&references);
    assert_eq!(files(&references), ["docs/guide.md", "docs/index.md"]);

    assert!(marksman_running(&app.status(&workspace).await.unwrap()));
    app.shutdown_language_servers().await;
    models_task.abort();
}

/// `search_symbols` maps a marksman symbol (a heading, here the fake's
/// canned one) onto the indexed markdown file.
#[tokio::test]
async fn search_symbols_lists_marksman_headings_of_indexed_prose() {
    let (temp, config, _fake, models_task) = fixture().await;
    let (config, workspace, _state) = prose_fixture(&temp, config, "workspace-symbols");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app.symbols(&workspace, "Calculator").await.unwrap();
    assert_marksman_locations(&report);
    assert_eq!(files(&report), ["docs/guide.md"]);
    assert_eq!(report.results[0].name.as_deref(), Some("Calculator"));
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    app.shutdown_language_servers().await;
    models_task.abort();
}

/// With prose indexed, a search whose candidates are markdown chunks does not
/// start marksman: the LSP channel asks servers for symbols named like the
/// query, and headings are not that. The control after it shows the same
/// server does start when asked for symbols directly, so the absence above is
/// the channel's filter and not a server that could not start.
#[tokio::test]
async fn a_search_never_starts_marksman_but_symbols_do() {
    let (temp, config, _fake, models_task) = fixture().await;
    let (config, workspace, state) = prose_fixture(&temp, config, "workspace-symbols");
    assert!(
        config.channels.lsp,
        "the default search runs the LSP channel"
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app.search(&workspace, "Calculator", Some(4)).await.unwrap();
    assert!(
        report
            .results
            .iter()
            .any(|hit| hit.chunk.language == "markdown"),
        "the candidates must include prose for the filter to matter: {:?}",
        report.results
    );
    assert!(report.warning.is_none(), "{:?}", report.warning);
    let status = app.status(&workspace).await.unwrap();
    assert!(!marksman_running(&status), "{:?}", status.analyzers_running);
    assert!(!state.join("symbol_request_pids.txt").exists());

    app.symbols(&workspace, "Calculator").await.unwrap();
    assert!(state.join("symbol_request_pids.txt").exists());
    assert!(marksman_running(&app.status(&workspace).await.unwrap()));
    app.shutdown_language_servers().await;
    models_task.abort();
}

/// Without a `[markdown]` path the adapter is disabled: navigation on a
/// prose file says so, and nothing starts.
#[tokio::test]
async fn an_unconfigured_markdown_server_is_disabled_and_starts_nothing() {
    let (temp, config, _fake, models_task) = fixture().await;
    let (mut config, workspace, state) = prose_fixture(&temp, config, "definition-references");
    config.markdown = MarkdownLspConfig::default();
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let error = app
        .definition(&workspace, "docs/index.md", 3, 22)
        .await
        .expect_err("no marksman path is configured");
    let message = format!("{error:#}");
    assert!(message.contains("marksman is disabled"), "{message}");
    assert!(!marksman_running(&app.status(&workspace).await.unwrap()));
    assert!(!state.exists(), "no process was started");
    models_task.abort();
}

/// With prose unindexed (the default) a markdown file is not navigable: the
/// not-indexed error comes before any server is started.
#[tokio::test]
async fn markdown_navigation_needs_prose_indexed_and_starts_nothing_otherwise() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.index.prose = false;
    let (config, workspace, state) = prose_fixture(&temp, config, "definition-references");
    write(
        &workspace,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n",
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let error = app
        .definition(&workspace, "docs/index.md", 3, 22)
        .await
        .expect_err("prose is not indexed");
    let message = format!("{error:#}");
    assert!(message.contains("contains no markdown chunks"), "{message}");
    assert!(!marksman_running(&app.status(&workspace).await.unwrap()));
    assert!(!state.exists(), "no process was started");
    models_task.abort();
}
