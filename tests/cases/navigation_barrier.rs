//! Navigation and the search channel pass the tree barrier (N104, decision
//! d495): a kept session whose tree changed is restarted before the call uses
//! it, and the answer says what the barrier did.
//!
//! Every test runs the `documents`-mode fake csharp-ls through a service whose
//! manager is the one navigation, the search channel and a traverse walk
//! share. The fake ignores `didChangeWatchedFiles` (`--ignore-watched-files`),
//! so only a restart can show it an edit, and `traverse` is off, so no warm
//! start touches the server before the call under test. Starts are counted
//! from the fake's `spawns/` directory; nothing sleeps and nothing asserts on
//! elapsed time. `age_tree` runs before the first call, so the edit made later
//! cannot share a clock tick with the tree the stamp recorded.

use super::navigation::{
    CALL_SITE_WITH_TWICE_CS, WarmService, age_tree, csharp_scip_index, indexed_warm_service,
    kept_noted, reached_run_through_lsp_live, restart_noted, spawns_started, traverse_add,
};
use super::*;
use local_code_intelligence::{config::LspSessions, workspace::Workspace};

/// The service of these tests, with the C# workspace indexed and aged.
async fn barrier_service() -> WarmService {
    let service = indexed_warm_service(&["--ignore-watched-files"], |config| {
        config.traverse.enabled = false;
        // A restart lands inside the search channel's budget: keep the budget
        // out of the way of a slow start under load.
        config.lsp_search_budget_ms = LOADED_BUDGET.as_millis() as u64;
    })
    .await;
    age_tree(&service.workspace.path);
    service
}

/// The note of a navigation answer or a search whose kept server was restarted.
const RESTARTED: &str = "lsp: csharp-ls restarted: the tree changed since its last call";

/// Whether `notes` says `csharp-ls` kept its session and the tree stamp had
/// not changed.
fn kept(notes: &[String]) -> bool {
    notes
        .iter()
        .any(|note| note.starts_with("lsp: csharp-ls kept session; tree stamp unchanged since "))
}

/// Whether the search reports no language-server failure (a restart is not one).
fn no_lsp_warning(report: &local_code_intelligence::app::SearchReport) -> bool {
    report
        .warning
        .as_deref()
        .is_none_or(|warning| !warning.contains("LSP"))
}

/// Zero-based column of the first `needle` on the one-based `line` of `file`.
fn column_of(workspace: &Workspace, file: &str, line: usize, needle: &str) -> u32 {
    let text = std::fs::read_to_string(workspace.path.join(file)).unwrap();
    text.lines().nth(line - 1).unwrap().find(needle).unwrap() as u32
}

#[tokio::test]
async fn symbols_through_the_service_restart_the_server_after_an_edit() {
    let service = barrier_service().await;
    let (app, workspace, state) = (&service.app, &service.workspace, &service.state);
    tokio::time::timeout(LOADED_BUDGET, async {
        let first = app.symbols(&workspace.path, "Calculator").await.unwrap();
        assert!(
            first
                .results
                .iter()
                .any(|found| found.name.as_deref() == Some("Calculator")),
            "{first:?}"
        );
        assert!(
            first.notes.is_empty(),
            "a server this call started adds no note: {first:?}"
        );
        assert_eq!(spawns_started(state), 1);

        write(&workspace.path, "src/CallSite.cs", CALL_SITE_WITH_TWICE_CS);
        let second = app.symbols(&workspace.path, "Twice").await.unwrap();
        assert!(
            second
                .results
                .iter()
                .any(|found| found.name.as_deref() == Some("Twice")),
            "the restarted server read the edited file: {second:?}"
        );
        assert_eq!(second.notes, [RESTARTED], "{second:?}");
        assert!(!kept(&second.notes), "a fresh server is not a kept one");
        assert_eq!(spawns_started(state), 2, "the stale server was replaced");
    })
    .await
    .expect("the test finishes within the loaded budget");
    app.shutdown_language_servers().await;
    service.models_task.abort();
}

#[tokio::test]
async fn symbols_through_the_service_keep_the_server_while_the_tree_is_unchanged() {
    let service = barrier_service().await;
    let (app, workspace, state) = (&service.app, &service.workspace, &service.state);
    tokio::time::timeout(LOADED_BUDGET, async {
        let first = app.symbols(&workspace.path, "Calculator").await.unwrap();
        assert!(first.notes.is_empty(), "{first:?}");
        let second = app.symbols(&workspace.path, "Calculator").await.unwrap();
        assert!(
            kept(&second.notes),
            "the second call found the server running: {second:?}"
        );
        assert!(
            !second.notes.iter().any(|note| note.contains("restarted")),
            "{second:?}"
        );
        assert_eq!(
            first.results, second.results,
            "an unchanged tree, the same answer"
        );
        assert_eq!(spawns_started(state), 1, "one server");
    })
    .await
    .expect("the test finishes within the loaded budget");
    app.shutdown_language_servers().await;
    service.models_task.abort();
}

#[tokio::test]
async fn definition_and_references_pass_the_barrier_after_an_edit() {
    let service = barrier_service().await;
    let (app, workspace, state) = (&service.app, &service.workspace, &service.state);
    tokio::time::timeout(LOADED_BUDGET, async {
        let at_call = column_of(workspace, "src/CallSite.cs", 2, "Add");
        let definition = app
            .definition(&workspace.path, "src/CallSite.cs", 2, at_call)
            .await
            .unwrap();
        assert!(
            definition
                .results
                .iter()
                .any(|found| found.relative_file_path.as_deref() == Some("src/Calculator.cs")),
            "{definition:?}"
        );
        assert!(definition.notes.is_empty(), "{definition:?}");
        assert_eq!(spawns_started(state), 1);

        // A second caller of `Add`, on the third line of `CallSite.cs`.
        write(&workspace.path, "src/CallSite.cs", CALL_SITE_WITH_TWICE_CS);
        let declared = column_of(workspace, "src/Calculator.cs", 2, "Add");
        let references = app
            .references(&workspace.path, "src/Calculator.cs", 2, declared, true)
            .await
            .unwrap();
        assert_eq!(references.notes, [RESTARTED], "{references:?}");
        assert!(
            references.results.iter().any(|found| {
                found.relative_file_path.as_deref() == Some("src/CallSite.cs")
                    && found.start_line == 2
            }),
            "the restarted server sees the new caller: {references:?}"
        );
        assert_eq!(spawns_started(state), 2);

        // Unchanged since the restart: the second server is kept.
        let again = app
            .references(&workspace.path, "src/Calculator.cs", 2, declared, true)
            .await
            .unwrap();
        assert!(kept(&again.notes), "{again:?}");
        assert_eq!(again.results, references.results);
        assert_eq!(spawns_started(state), 2);
    })
    .await
    .expect("the test finishes within the loaded budget");
    app.shutdown_language_servers().await;
    service.models_task.abort();
}

/// A navigation call that starts a server records its stamp, so the next
/// traverse walk keeps that server instead of restarting a session that has
/// no stamp (d454 named this residual).
#[tokio::test]
async fn a_walk_after_a_navigation_call_keeps_the_server_it_started() {
    let service = barrier_service().await;
    let (app, workspace, state) = (&service.app, &service.workspace, &service.state);
    tokio::time::timeout(LOADED_BUDGET, async {
        let index = csharp_scip_index(&service.temp, workspace);
        app.symbols(&workspace.path, "Calculator").await.unwrap();
        assert_eq!(spawns_started(state), 1);
        let manager = app.language_servers();
        let answer = traverse_add(
            &service.config,
            workspace,
            &index,
            LspSessions::Shared,
            &manager,
        )
        .await;
        assert!(reached_run_through_lsp_live(&answer), "{answer}");
        assert!(kept_noted(&answer, "csharp-ls"), "{answer}");
        assert!(!restart_noted(&answer, "csharp-ls"), "{answer}");
        assert_eq!(
            spawns_started(state),
            1,
            "the walk used the server navigation started"
        );
    })
    .await
    .expect("the test finishes within the loaded budget");
    app.shutdown_language_servers().await;
    service.models_task.abort();
}

/// The search channel's request is `workspace/symbol` with a `<term>#` query,
/// which the fake does not answer (it matches names that contain the `#`), so
/// the proof here is the server's start count and the report's notes, not a
/// hit from the edited file.
#[tokio::test]
async fn the_search_channel_restarts_a_kept_server_after_an_edit() {
    let service = barrier_service().await;
    let (app, workspace, state) = (&service.app, &service.workspace, &service.state);
    tokio::time::timeout(LOADED_BUDGET, async {
        let first = app
            .search(&workspace.path, "Calculator", Some(3))
            .await
            .unwrap();
        assert!(
            !first.notes.iter().any(|note| note.starts_with("lsp:")),
            "a server the search started adds no note: {:?}",
            first.notes
        );
        assert!(first.lsp_over_budget.is_empty(), "{:?}", first.notes);
        assert_eq!(spawns_started(state), 1);

        let second = app
            .search(&workspace.path, "Calculator", Some(3))
            .await
            .unwrap();
        assert!(kept(&second.notes), "{:?}", second.notes);
        assert!(no_lsp_warning(&second), "{:?}", second.warning);
        assert_eq!(
            spawns_started(state),
            1,
            "an unchanged tree keeps the server"
        );

        write(&workspace.path, "src/CallSite.cs", CALL_SITE_WITH_TWICE_CS);
        let third = app.search(&workspace.path, "Twice", Some(3)).await.unwrap();
        assert!(
            third.notes.iter().any(|note| note == RESTARTED),
            "{:?}",
            third.notes
        );
        assert!(!kept(&third.notes), "{:?}", third.notes);
        assert!(third.lsp_over_budget.is_empty(), "{:?}", third.notes);
        assert!(
            no_lsp_warning(&third),
            "restarting is not a failure: {:?}",
            third.warning
        );
        assert_eq!(spawns_started(state), 2, "the stale server was replaced");
    })
    .await
    .expect("the test finishes within the loaded budget");
    app.shutdown_language_servers().await;
    service.models_task.abort();
}

/// The service of the watching-server test: a Rust workspace indexed and aged,
/// whose rust-analyzer is a copy of the fake (`with_fake_rust_analyzer`).
struct RustService {
    app: App,
    workspace: Workspace,
    state: std::path::PathBuf,
    _temp: tempfile::TempDir,
    _fake: Arc<FakeModels>,
    models_task: tokio::task::JoinHandle<()>,
}

async fn rust_service() -> RustService {
    let (temp, config, fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let path = temp.path().join("workspace");
    write(
        &path,
        "Cargo.toml",
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write(
        &path,
        "src/lib.rs",
        "pub mod call_site;\npub fn calculate_total(a: i32, b: i32) -> i32 { a + b }\n",
    );
    write(
        &path,
        "src/call_site.rs",
        "pub fn run() -> i32 { crate::calculate_total(1, 2) }\n",
    );
    let mut config = with_fake_rust_analyzer(config, temp.path(), &state, &[]);
    config.traverse.enabled = false;
    // A start lands inside the search channel's budget: keep the budget out of
    // the way of a slow start under load.
    config.lsp_search_budget_ms = LOADED_BUDGET.as_millis() as u64;
    let app = App::open(config).await.unwrap();
    let workspace = app.index(&path).await.unwrap().workspace;
    age_tree(&workspace.path);
    RustService {
        app,
        workspace,
        state,
        _temp: temp,
        _fake: fake,
        models_task,
    }
}

const RUST_KEPT_AFTER_EDIT: &str = "lsp: rust-analyzer kept session; the tree changed since its last call (kept: it watches its workspace)";
const RUST_RESTARTED: &str = "lsp: rust-analyzer restarted: the tree changed since its last call";

/// N108: an edit restarted rust-analyzer and the next searches lost the LSP
/// channel. The search channel keeps a server that watches its workspace
/// through a changed stamp and says so; it records no stamp for it, so
/// navigation (N104) restarts it as before, and the search after that finds
/// the new session unchanged. The C# test above is the non-watching control:
/// the same edit restarts the server.
#[tokio::test]
async fn the_search_channel_keeps_a_watching_server_after_an_edit_and_navigation_restarts_it() {
    let service = rust_service().await;
    let (app, workspace, state) = (&service.app, &service.workspace, &service.state);
    let notes_with_kept_unchanged = |notes: &[String]| {
        notes.iter().any(|note| {
            note.starts_with("lsp: rust-analyzer kept session; tree stamp unchanged since ")
        })
    };
    tokio::time::timeout(LOADED_BUDGET, async {
        let first = app.search(&workspace.path, "calculate_total", Some(3)).await.unwrap();
        assert!(
            !first.notes.iter().any(|note| note.starts_with("lsp:")),
            "a server the search started adds no note: {:?}",
            first.notes
        );
        assert!(first.lsp_over_budget.is_empty(), "{:?}", first.notes);
        assert_eq!(spawns_started(state), 1, "{first:?}");

        let second = app.search(&workspace.path, "calculate_total", Some(3)).await.unwrap();
        assert!(
            notes_with_kept_unchanged(&second.notes),
            "{:?}",
            second.notes
        );
        assert_eq!(spawns_started(state), 1);

        write(
            &workspace.path,
            "src/call_site.rs",
            "pub fn run() -> i32 { crate::calculate_total(1, 2) }\npub fn run_again() -> i32 { crate::calculate_total(3, 4) }\n",
        );
        let third = app.search(&workspace.path, "calculate_total", Some(3)).await.unwrap();
        let lsp_notes: Vec<&String> = third
            .notes
            .iter()
            .filter(|note| note.starts_with("lsp:"))
            .collect();
        assert_eq!(lsp_notes, [RUST_KEPT_AFTER_EDIT], "{:?}", third.notes);
        assert!(no_lsp_warning(&third), "{:?}", third.warning);
        assert!(third.lsp_over_budget.is_empty(), "{:?}", third.notes);
        assert_eq!(spawns_started(state), 1, "the watcher kept its session");

        // Navigation keeps the N104 barrier: the search recorded no stamp for
        // the session it kept, so the server is restarted now.
        let symbols = app.symbols(&workspace.path, "calculate_total").await.unwrap();
        assert_eq!(symbols.notes, [RUST_RESTARTED], "{:?}", symbols.notes);
        assert_eq!(spawns_started(state), 2);

        // The restarted session carries the current stamp.
        let fourth = app.search(&workspace.path, "calculate_total", Some(3)).await.unwrap();
        assert!(
            notes_with_kept_unchanged(&fourth.notes),
            "{:?}",
            fourth.notes
        );
        assert!(
            !fourth.notes.iter().any(|note| note.contains("(kept: it")),
            "{:?}",
            fourth.notes
        );
        assert_eq!(spawns_started(state), 2);
    })
    .await
    .expect("the test finishes within the loaded budget");
    app.shutdown_language_servers().await;
    service.models_task.abort();
}
