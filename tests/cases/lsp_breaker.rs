//! The LSP down-window breaker (`[run_profile.lsp] breaker`, K11e, d474) is
//! shared with a traversal's own manager (N106, decision d496).

use super::navigation::{
    indexed_manager, reached_run_through_lsp_live, shared_sessions_fixture, spawns_started,
    traverse_add,
};
use local_code_intelligence::config::LspSessions;

/// A traversal under `sessions = "shared"` starts a server that does not see
/// an edit on its own (csharp-ls with `sync = "off"`) on a manager of the
/// call's own (`app::traverse::per_call_manager`). That manager reads and
/// opens the skip windows of the manager the call was offered, so a provider
/// whose server cannot start is left alone by the next walk.
///
/// The fake passes the version probe, which the offered manager answers for
/// both walks (the first runs it, the second finds it remembered), and exits
/// at the `initialize` request (`--exit-at-start`): the failure is the call's
/// own manager's start, never the probe. Remove `.with_breakers(..)` from
/// `per_call_manager` and the first walk leaves the shared windows closed and
/// the second walk starts another server.
#[tokio::test]
async fn a_per_call_manager_opens_the_offered_managers_windows() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state, index) = shared_sessions_fixture(&temp);
    config.csharp.args.push("--exit-at-start".to_owned());
    config.run_profile.lsp.breaker = true;
    let offered = indexed_manager(&config, &workspace);
    let windows = offered.breakers().expect("breaker = true");
    assert!(windows.remaining("csharp-ls").is_none());

    let first = traverse_add(&config, &workspace, &index, LspSessions::Shared, &offered).await;
    assert!(!reached_run_through_lsp_live(&first), "{first}");
    assert!(
        first["notes"].to_string().contains("lsp.live"),
        "the failed start is noted: {first}"
    );
    assert!(
        windows.remaining("csharp-ls").is_some(),
        "the call's own manager opened the offered manager's window: {first}"
    );
    let started = spawns_started(&state);
    assert_eq!(started, 1, "the walk's own manager started the server once");

    let second = traverse_add(&config, &workspace, &index, LspSessions::Shared, &offered).await;
    assert!(!reached_run_through_lsp_live(&second), "{second}");
    assert!(
        second["notes"].to_string().contains("lsp breaker"),
        "the second walk is refused by the window: {second}"
    );
    assert_eq!(
        spawns_started(&state),
        started,
        "a refused start spawns nothing"
    );
    offered.shutdown_all().await;
}
