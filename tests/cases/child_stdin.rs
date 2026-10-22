//! A child process LCI starts for its own purposes must not inherit LCI's
//! stdin (N107). Under the MCP stdio transport that stream is the client's
//! JSON-RPC channel: a child that reads it blocks, or consumes protocol bytes.

use super::*;
use local_code_intelligence::lsp::adapter::probe_subprocess_version;
use std::process::Stdio;

const CHILD: &str = "LCI_TEST_PROBE_STDIN_CHILD";
const FINISHED: &str = "PROBE_FINISHED";

/// The fake language server (`fake_lsp.rs`) reads stdin until it closes, so
/// its `--version` run ends only when its stdin is the null device or a pipe
/// that closes. The probe runs in a child of this test whose own stdin is a
/// pipe the test holds open for the whole run; the probe finishing inside the
/// budget shows that it did not hand that pipe on.
#[tokio::test]
async fn a_version_probe_does_not_inherit_a_stdin_that_stays_open() {
    let fake = env!("CARGO_BIN_EXE_fake-lsp-server");
    if std::env::var_os(CHILD).is_some() {
        // The probe's own bound is wider than the parent's wait, so a hang is
        // seen by the parent and not hidden by the probe's timeout.
        let probed = probe_subprocess_version(Path::new(fake), 2 * LOADED_BUDGET).await;
        match probed {
            Ok(()) => {
                println!("{FINISHED}");
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("probe failed: {error:#}");
                std::process::exit(1);
            }
        }
    }

    let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "child_stdin::a_version_probe_does_not_inherit_a_stdin_that_stays_open",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    // Held until the assertions: `wait_with_output` would close an stdin it
    // still owned.
    let held_open = child.stdin.take().unwrap();
    let waited = tokio::time::timeout(LOADED_BUDGET, child.wait_with_output()).await;
    // Closing the pipe lets a fake server that did inherit it exit.
    drop(held_open);
    let output = waited
        .expect("the version probe is still waiting on the stdin of its parent")
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "probe child failed: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    // A renamed test would make the child run nothing and pass.
    assert!(
        stdout.contains(FINISHED),
        "the child ran no probe: {stdout}"
    );
}
