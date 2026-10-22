//! `lci config` and the layered configuration, through the real binary.
//!
//! Every spawned process gets `--config <temp file>` (or `LCI_CONFIG` /
//! `LCI_LOCAL_CONFIG` pointing into the temp directory when a test is about
//! the case without `--config`) and has every `LCI_*` variable of the test
//! process removed, so neither the developer's shell nor their own
//! user-level overlay can change a result. Model URLs point at listeners the
//! test owns, which must see no connection, or at nothing; no test touches
//! ports 8765-8767, and `serve` binds loopback ports the test picked.
//!
//! The `init` tests (see [`InitBox`]) also give each process a `PATH` that is
//! one temporary directory, holding copies of the fake language-server binary
//! under the names of the tools `init` looks for, so what it finds never
//! depends on what is installed. They probe only services the test owns
//! (the fake model service, or a listener that must stay unconnected) and
//! pass `--no-probe` whenever the configuration would otherwise name a model
//! service on a port the test does not own.

use super::*;
use std::{
    ffi::OsString,
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Output, Stdio},
};

/// Width of the key column in the text report: the longest machine key,
/// `typescript.disabled`.
const KEY_WIDTH: usize = 19;

/// A temp directory, a configuration file in it whose model URLs name two
/// listeners, and the helpers to run the binary against it.
struct Sandbox {
    temp: tempfile::TempDir,
    config: PathBuf,
    embedder: TcpListener,
    reranker: TcpListener,
}

impl Sandbox {
    /// `extra` is appended to the file after `data_dir` and the two URLs, so
    /// it may hold tables.
    fn new(extra: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let embedder = listener();
        let reranker = listener();
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "data_dir = '{}'\nembedding_url = 'http://127.0.0.1:{}/v1'\nreranker_url = 'http://127.0.0.1:{}/rerank'\n{extra}",
                temp.path().join("data").display(),
                embedder.local_addr().unwrap().port(),
                reranker.local_addr().unwrap().port(),
            ),
        )
        .unwrap();
        Self {
            temp,
            config,
            embedder,
            reranker,
        }
    }

    fn data_dir(&self) -> PathBuf {
        self.temp.path().join("data")
    }

    /// The overlay `lci --config <config>` looks for.
    fn overlay(&self) -> PathBuf {
        self.temp.path().join("config.local.toml")
    }

    fn command(&self, args: &[&str], envs: &[(&str, &str)]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command
            .current_dir(self.temp.path())
            .arg("--config")
            .arg(&self.config)
            .args(args);
        strip_lci_env(&mut command);
        for (name, value) in envs {
            command.env(name, value);
        }
        command
    }

    /// Runs the binary with `--config` and the given extra variables.
    fn run(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.command(args, envs).output().unwrap()
    }

    /// Runs `config --json` and returns the report.
    fn report(&self, args: &[&str], envs: &[(&str, &str)]) -> Value {
        let mut all = vec!["config", "--json"];
        all.extend_from_slice(args);
        let output = self.run(&all, envs);
        assert!(output.status.success(), "{}", stderr(&output));
        parse(&output)
    }

    /// Neither model listener has been connected to.
    fn assert_no_connections(&self) {
        for (name, listener) in [("embedder", &self.embedder), ("reranker", &self.reranker)] {
            match listener.accept() {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                other => panic!("the {name} listener saw a connection: {other:?}"),
            }
        }
    }
}

/// A loopback listener on an ephemeral port that never blocks `accept`.
fn listener() -> TcpListener {
    let listener = loopback_listener();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn parse(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            stderr(output)
        )
    })
}

fn text(path: &Path) -> String {
    path.display().to_string()
}

#[test]
fn a_plain_file_resolves_to_file_and_default_sources_without_touching_anything() {
    let sandbox =
        Sandbox::new("rerank_candidate_count = 12\n[typescript]\npath = 'ts-from-file'\n");
    let report = sandbox.report(&[], &[]);

    assert_eq!(report["valid"], true);
    assert!(report["error"].is_null());
    assert_eq!(report["layers"]["file"]["path"], text(&sandbox.config));
    assert_eq!(report["layers"]["file"]["named_by"], "--config");
    assert_eq!(report["layers"]["local"]["state"], "absent");
    assert_eq!(report["layers"]["local"]["path"], text(&sandbox.overlay()));
    assert_eq!(report["layers"]["env"], json!([]));
    assert_eq!(report["layers"]["flags"], json!([]));

    let values = &report["values"];
    for (key, source) in [
        ("data_dir", "file"),
        ("embedding_url", "file"),
        ("reranker_url", "file"),
        ("typescript.path", "file"),
        ("rerank_candidate_count", "file"),
        ("port", "default"),
        ("ripgrep_path", "default"),
        ("embedding_model", "default"),
        ("csharp.path", "default"),
        ("default_top_k", "default"),
        ("fusion.max_chunks_per_file", "default"),
    ] {
        assert_eq!(values[key]["source"], source, "{key}");
    }
    assert_eq!(values["port"]["value"], 8768);
    assert!(values["port"]["from"].is_null());
    assert_eq!(values["typescript.path"]["value"], "ts-from-file");
    assert_eq!(values["data_dir"]["from"], text(&sandbox.config));
    assert_eq!(values["csharp.path"]["value"], Value::Null);
    assert_eq!(report["config"]["port"], 8768);

    let derived = &report["derived"];
    assert_eq!(derived["mcp_url"], "http://127.0.0.1:8768/mcp");
    assert_eq!(derived["rest_base"], "http://127.0.0.1:8768/v1");
    assert_eq!(derived["health_url"], "http://127.0.0.1:8768/health");
    assert_eq!(derived["ready_url"], "http://127.0.0.1:8768/ready");
    assert_eq!(derived["openapi_url"], "http://127.0.0.1:8768/openapi.json");
    assert_eq!(derived["mcp_profile_urls"], json!({}));
    assert!(
        derived["embedding_identity"]
            .as_str()
            .unwrap()
            .contains("qwen3-embedding-4b")
    );
    let profile_hash = derived["run_profile_hash"].as_str().unwrap();
    assert_eq!(profile_hash.len(), 64, "{profile_hash}");
    assert!(
        profile_hash
            .chars()
            .all(|c| matches!(c, '0'..='9' | 'a'..='f')),
        "{profile_hash}"
    );
    // The test process's `LCI_*` variables are stripped, so the dump is off.
    assert_eq!(
        report["environment"]["other"]["LCI_CANDIDATE_DUMP"],
        "unset"
    );

    // Nothing was created or contacted.
    assert!(
        !sandbox.data_dir().exists(),
        "config created the data directory"
    );
    sandbox.assert_no_connections();
}

#[test]
fn the_text_report_names_every_machine_setting_its_source_and_the_derived_urls() {
    let sandbox =
        Sandbox::new("[search_profiles.fast]\nreranker = false\n[typescript]\npath = 'ts'\n");
    let output = sandbox.run(&["config"], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = String::from_utf8(output.stdout).unwrap();
    assert!(report.starts_with("configuration: valid"), "{report}");
    let setting = |key: &str, value: &str, source: &str| {
        format!("  {key:<KEY_WIDTH$} = {value}  (source: {source})")
    };
    let derived = |key: &str, value: &str| format!("  {key:<18} = {value}");
    let config = text(&sandbox.config);
    for expected in [
        format!("file:    {config} (--config)"),
        format!(
            "local:   {} (absent; config-file-sibling)",
            text(&sandbox.overlay())
        ),
        setting("port", "8768", "default"),
        setting(
            "data_dir",
            &text(&sandbox.data_dir()),
            &format!("file, {config}"),
        ),
        setting("typescript.path", "ts", &format!("file, {config}")),
        setting("csharp.path", "unset", "default"),
        derived("mcp_url", "http://127.0.0.1:8768/mcp"),
        derived("mcp_url (fast)", "http://127.0.0.1:8768/mcp/fast"),
        "LCI_CANDIDATE_DUMP: unset".to_owned(),
    ] {
        assert!(
            report.contains(&expected),
            "missing {expected:?} in:
{report}"
        );
    }
    assert!(!sandbox.data_dir().exists());
    sandbox.assert_no_connections();
}

#[test]
fn a_sibling_overlay_overrides_only_the_machine_keys_it_names() {
    let sandbox =
        Sandbox::new("rerank_candidate_count = 12\n[typescript]\npath = 'ts-from-file'\n");
    let overlay_data = sandbox.temp.path().join("overlay-data");
    let [port] = free_ports();
    std::fs::write(
        sandbox.overlay(),
        format!(
            "data_dir = '{}'\nport = {port}\n[typescript]\npath = 'ts-from-overlay'\n",
            overlay_data.display()
        ),
    )
    .unwrap();
    let report = sandbox.report(&[], &[]);

    assert_eq!(report["layers"]["local"]["state"], "present");
    let overlay = text(&sandbox.overlay());
    let values = &report["values"];
    for key in ["data_dir", "port", "typescript.path"] {
        assert_eq!(values[key]["source"], "local", "{key}");
        assert_eq!(values[key]["from"], overlay, "{key}");
    }
    assert_eq!(values["data_dir"]["value"], text(&overlay_data));
    assert_eq!(values["port"]["value"], port);
    assert_eq!(values["typescript.path"]["value"], "ts-from-overlay");
    // The keys the overlay leaves alone keep their layer.
    assert_eq!(values["embedding_url"]["source"], "file");
    assert_eq!(values["rerank_candidate_count"]["source"], "file");
    assert_eq!(values["rerank_candidate_count"]["value"], 12);
    assert_eq!(values["ripgrep_path"]["source"], "default");
    assert_eq!(
        report["derived"]["mcp_url"],
        format!("http://127.0.0.1:{port}/mcp")
    );
    assert!(!overlay_data.exists());
    sandbox.assert_no_connections();
}

#[test]
fn environment_variables_beat_the_overlay_and_no_local_config_ignores_it() {
    let sandbox = Sandbox::new("");
    let [overlay_port, env_port] = free_ports();
    let env_port = env_port.to_string();
    let overlay_data = sandbox.temp.path().join("overlay-data");
    std::fs::write(
        sandbox.overlay(),
        format!(
            "data_dir = '{}'\nport = {overlay_port}\nripgrep_path = 'rg-from-overlay'\n",
            overlay_data.display()
        ),
    )
    .unwrap();
    let env_data = sandbox.temp.path().join("env-data");
    let env_data_text = text(&env_data);
    let report = sandbox.report(
        &[],
        &[
            ("LCI_DATA_DIR", env_data_text.as_str()),
            ("LCI_PORT", env_port.as_str()),
        ],
    );
    let values = &report["values"];
    assert_eq!(values["data_dir"]["source"], "env");
    assert_eq!(values["data_dir"]["from"], "LCI_DATA_DIR");
    assert_eq!(values["data_dir"]["value"], env_data_text);
    assert_eq!(values["port"]["source"], "env");
    assert_eq!(values["port"]["from"], "LCI_PORT");
    assert_eq!(values["port"]["value"], env_port.parse::<u16>().unwrap());
    // A key only the overlay sets still comes from it.
    assert_eq!(values["ripgrep_path"]["source"], "local");
    assert_eq!(report["layers"]["env"], json!(["LCI_DATA_DIR", "LCI_PORT"]));

    // `--no-local-config` skips the overlay and says so; the file's and the
    // defaults' values come back.
    let report = sandbox.report(&["--no-local-config"], &[]);
    assert_eq!(report["layers"]["local"]["state"], "disabled");
    assert_eq!(report["layers"]["local"]["path"], text(&sandbox.overlay()));
    assert_eq!(report["values"]["ripgrep_path"]["source"], "default");
    assert_eq!(report["values"]["port"]["value"], 8768);
    assert_eq!(report["values"]["data_dir"]["source"], "file");
    sandbox.assert_no_connections();
}

#[test]
fn without_a_configuration_file_lci_config_and_lci_local_config_pick_the_layers() {
    let sandbox = Sandbox::new("default_top_k = 3\n");
    let user_overlay = sandbox.temp.path().join("user-overlay.toml");
    std::fs::write(&user_overlay, "ripgrep_path = 'rg-from-user-overlay'\n").unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .current_dir(sandbox.temp.path())
        .args(["config", "--json"]);
    strip_lci_env(&mut command)
        .env("LCI_CONFIG", &sandbox.config)
        .env("LCI_LOCAL_CONFIG", &user_overlay);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["layers"]["file"]["named_by"], "LCI_CONFIG");
    assert_eq!(report["layers"]["file"]["path"], text(&sandbox.config));
    assert_eq!(report["layers"]["local"]["named_by"], "LCI_LOCAL_CONFIG");
    assert_eq!(report["layers"]["local"]["state"], "present");
    assert_eq!(report["values"]["default_top_k"]["value"], 3);
    assert_eq!(report["values"]["ripgrep_path"]["source"], "local");

    // With neither a file nor `LCI_LOCAL_CONFIG` pointing here, the only
    // overlay candidate is the user-level one; pointing it into the temp
    // directory keeps the developer's real file out of the test.
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .current_dir(sandbox.temp.path())
        .args(["config", "--json"]);
    strip_lci_env(&mut command).env("LCI_LOCAL_CONFIG", sandbox.temp.path().join("none.toml"));
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert!(report["layers"]["file"]["path"].is_null());
    assert_eq!(report["layers"]["local"]["state"], "absent");
    assert_eq!(report["values"]["port"]["source"], "default");
    assert_eq!(report["values"]["data_dir"]["source"], "default");
}

#[test]
fn an_overlay_key_outside_the_machine_set_fails_naming_the_key() {
    let sandbox = Sandbox::new("");
    for (overlay, key) in [
        ("rerank_candidate_count = 5\n", "rerank_candidate_count"),
        ("[channels]\nsemantic = false\n", "channels"),
        (
            "embedding_query_template = 'q {query}'\n",
            "embedding_query_template",
        ),
    ] {
        std::fs::write(sandbox.overlay(), overlay).unwrap();
        let output = sandbox.run(&["config", "--json"], &[]);
        assert_eq!(output.status.code(), Some(1), "{key}");
        let report = parse(&output);
        assert_eq!(report["valid"], false);
        let error = report["error"].as_str().unwrap();
        assert!(error.contains(&format!("`{key}`")), "{error}");
        assert!(error.contains("not a machine-specific setting"), "{error}");
        assert!(error.contains("configuration file"), "{error}");
        assert!(error.contains("config.local.toml"), "{error}");
        assert!(stderr(&output).contains(key), "{}", stderr(&output));

        // The same file is rejected when any other command resolves it.
        let output = sandbox.run(&["status", "."], &[]);
        assert_eq!(output.status.code(), Some(1), "{key}");
        assert!(stderr(&output).contains(key), "{}", stderr(&output));
    }
    assert!(!sandbox.data_dir().exists());
    sandbox.assert_no_connections();
}

#[test]
fn bad_values_fail_with_the_exit_code_of_their_kind() {
    let sandbox = Sandbox::new("");

    // A port belonging to another service: resolves, is invalid, still
    // printed, exit 2.
    for reserved in ["8765", "8766", "8767"] {
        let output = sandbox.run(&["config", "--json"], &[("LCI_PORT", reserved)]);
        assert_eq!(output.status.code(), Some(2), "{reserved}");
        let report = parse(&output);
        assert_eq!(report["valid"], false);
        assert!(
            report["error"].as_str().unwrap().contains(reserved),
            "{report}"
        );
        assert_eq!(report["values"]["port"]["source"], "env");
        assert_eq!(
            report["values"]["port"]["value"],
            reserved.parse::<u16>().unwrap()
        );
    }
    let output = sandbox.run(&["config"], &[("LCI_PORT", "8766")]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stdout).starts_with("configuration: INVALID"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The file or the overlay can name one too.
    std::fs::write(sandbox.overlay(), "port = 8765\n").unwrap();
    let output = sandbox.run(&["config", "--json"], &[]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(parse(&output)["values"]["port"]["source"], "local");
    std::fs::remove_file(sandbox.overlay()).unwrap();

    // A malformed variable cannot be resolved: exit 1, naming the variable.
    let output = sandbox.run(&["config", "--json"], &[("LCI_PORT", "abc")]);
    assert_eq!(output.status.code(), Some(1));
    let report = parse(&output);
    assert_eq!(report["valid"], false);
    assert!(report["error"].as_str().unwrap().contains("LCI_PORT"));
    let output = sandbox.run(&["config", "--json"], &[("LCI_GO_DISABLED", "maybe")]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        parse(&output)["error"]
            .as_str()
            .unwrap()
            .contains("LCI_GO_DISABLED")
    );

    // An empty variable is unset.
    let report = sandbox.report(&[], &[("LCI_CSHARP_PATH", ""), ("LCI_PORT", "")]);
    assert_eq!(report["values"]["csharp.path"]["source"], "default");
    assert_eq!(report["values"]["port"]["source"], "default");
    assert_eq!(report["layers"]["env"], json!([]));

    // A configuration file that is not there is an I/O error, as before.
    let missing = sandbox.temp.path().join("missing.toml");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .current_dir(sandbox.temp.path())
        .arg("--config")
        .arg(&missing)
        .arg("config");
    let output = strip_lci_env(&mut command).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("read configuration"),
        "{}",
        stderr(&output)
    );
    sandbox.assert_no_connections();
}

/// Port 8765 is the generation model's: a model URL naming it, for any host,
/// is an invalid configuration (exit 2, still printed) whichever layer
/// supplies it. The binary only parses the string; nothing here opens a
/// connection, and the listeners the file's own URLs name stay untouched.
#[test]
fn a_model_url_on_port_8765_is_invalid_whichever_layer_supplies_it() {
    let sandbox = Sandbox::new("");

    // The environment, with a loopback host.
    let output = sandbox.run(
        &["config", "--json"],
        &[("LCI_EMBEDDING_URL", "http://localhost:8765/v1")],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["valid"], false);
    let error = report["error"].as_str().unwrap();
    assert!(error.contains("`embedding_url`"), "{error}");
    assert!(error.contains("port 8765"), "{error}");
    assert!(error.contains("generation model"), "{error}");
    assert!(error.contains("never contacts it"), "{error}");
    assert_eq!(report["values"]["embedding_url"]["source"], "env");
    assert_eq!(
        report["values"]["embedding_url"]["value"],
        "http://localhost:8765/v1"
    );

    // The local overlay, with another host spelling (an address reserved for
    // documentation, so no name is ever resolved).
    std::fs::write(
        sandbox.overlay(),
        "reranker_url = 'http://192.0.2.10:8765/rerank'\n",
    )
    .unwrap();
    let output = sandbox.run(&["config", "--json"], &[]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["valid"], false);
    let error = report["error"].as_str().unwrap();
    assert!(error.contains("`reranker_url`"), "{error}");
    assert!(error.contains("generation model"), "{error}");
    assert_eq!(report["values"]["reranker_url"]["source"], "local");

    // The text report says so too.
    let output = sandbox.run(&["config"], &[]);
    assert_eq!(output.status.code(), Some(2));
    let text_report = String::from_utf8_lossy(&output.stdout);
    assert!(
        text_report.starts_with("configuration: INVALID"),
        "{text_report}"
    );
    assert!(text_report.contains("generation model"), "{text_report}");

    // Another service's port in the same place is fine, so the rule is about
    // 8765 only.
    std::fs::write(
        sandbox.overlay(),
        "reranker_url = 'http://localhost:8767/rerank'\n",
    )
    .unwrap();
    let report = sandbox.report(&[], &[("LCI_EMBEDDING_URL", "http://localhost:8766/v1")]);
    assert_eq!(report["valid"], true);
    assert!(!sandbox.data_dir().exists());
    sandbox.assert_no_connections();
}

/// A `serve` child that is killed when it goes out of scope.
struct Serve {
    child: Child,
    log: PathBuf,
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Serve {
    fn spawn(sandbox: &Sandbox, args: &[&str], envs: &[(&str, &str)]) -> Self {
        let log = sandbox.temp.path().join("serve.log");
        let sink = std::fs::File::create(&log).unwrap();
        let mut command = sandbox.command(args, envs);
        let child = command
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(sink)
            .spawn()
            .unwrap();
        Self { child, log }
    }

    /// Polls the log until `serve` reports the port it bound, failing if the
    /// process exits first.
    async fn wait_bound_port(&mut self) -> u16 {
        const MARKER: &str = "MCP: http://127.0.0.1:";
        for _ in 0..240 {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "serve exited early ({status}): {}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            let log = std::fs::read_to_string(&self.log).unwrap_or_default();
            if let Some(start) = log.find(MARKER) {
                let digits: String = log[start + MARKER.len()..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if let Ok(port) = digits.parse() {
                    return port;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!(
            "serve never reported its port: {}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }

    /// Polls `/health` on `port` until it answers, failing if the process
    /// exits first.
    async fn wait_healthy(&mut self, port: u16, client: &reqwest::Client) {
        for _ in 0..240 {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "serve exited early ({status}): {}",
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/health"))
                .send()
                .await
                && response.status().is_success()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!(
            "no /health on {port}: {}",
            std::fs::read_to_string(&self.log).unwrap_or_default()
        );
    }
}

/// What the tests rely on [`ClosedPort`] for: a connection to its port is
/// refused, not left waiting, and (except on macOS, see [`ClosedPort`]) no
/// other socket can take the port while it lives, even one that sets
/// `SO_REUSEADDR` (as the standard library's and tokio's listeners do on
/// Unix).
#[tokio::test]
async fn a_closed_port_refuses_connections_and_cannot_be_taken() {
    let closed = ClosedPort::new();
    // No timeout: a refusal takes about 2 s on Windows, and a dropped
    // connection would end, much later, at the system's connect timeout as a
    // `TimedOut` error, which fails the assertion.
    let error = TcpStream::connect(("127.0.0.1", closed.port)).unwrap_err();
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "{error}"
    );
    #[cfg(not(target_os = "macos"))]
    {
        assert!(TcpListener::bind(("127.0.0.1", closed.port)).is_err());
        assert!(
            tokio::net::TcpListener::bind(("127.0.0.1", closed.port))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn serve_binds_the_port_the_overlay_sets_and_the_openapi_document_names_it() {
    let sandbox = Sandbox::new("");
    let [port] = free_ports();
    std::fs::write(sandbox.overlay(), format!("port = {port}\n")).unwrap();
    let mut server = Serve::spawn(&sandbox, &["serve"], &[]);
    let client = reqwest::Client::new();
    server.wait_healthy(port, &client).await;

    let document: Value = client
        .get(format!("http://127.0.0.1:{port}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        document["servers"][0]["url"],
        format!("http://127.0.0.1:{port}")
    );
    // The same process reports the port it serves on through `config`.
    let report = sandbox.report(&[], &[]);
    assert_eq!(
        report["derived"]["health_url"],
        format!("http://127.0.0.1:{port}/health")
    );
    // Serving created the data directory; the earlier tests prove `config`
    // alone does not.
    assert!(sandbox.data_dir().exists());
}

/// `serve --port 0` takes any free port; the log, `/openapi.json` and the
/// Origin check must all name the port it got, never 0.
#[tokio::test]
async fn serve_port_zero_names_the_bound_port_in_the_log_openapi_and_origin_check() {
    let sandbox = Sandbox::new("");
    let mut server = Serve::spawn(&sandbox, &["serve", "--port", "0"], &[]);
    let client = reqwest::Client::new();
    let port = server.wait_bound_port().await;
    assert_ne!(port, 0);
    server.wait_healthy(port, &client).await;

    let document: Value = client
        .get(format!("http://127.0.0.1:{port}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        document["servers"][0]["url"],
        format!("http://127.0.0.1:{port}")
    );
    let with_origin = |origin: String| {
        client
            .get(format!("http://127.0.0.1:{port}/health"))
            .header("Origin", origin)
            .send()
    };
    assert!(
        with_origin(format!("http://127.0.0.1:{port}"))
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert_eq!(
        with_origin("http://127.0.0.1:0".into())
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn serve_port_beats_lci_port_which_beats_the_overlay() {
    let sandbox = Sandbox::new("");
    // The overlay's port stays held, so a server that chose it would fail to
    // bind and never become healthy on the port it should have chosen. (An
    // assertion that nothing listens on a port released earlier could see
    // another test's socket there.)
    let overlay = ClosedPort::new();
    let overlay_port = overlay.port;
    // Held before `free_ports` chooses, so it cannot land on a chosen port.
    let second_env = ClosedPort::new();
    let [env_port, flag_port] = free_ports();
    std::fs::write(sandbox.overlay(), format!("port = {overlay_port}\n")).unwrap();
    let env_port_text = env_port.to_string();
    let flag_port_text = flag_port.to_string();

    // The environment beats the overlay.
    {
        let mut server = Serve::spawn(
            &sandbox,
            &["serve"],
            &[("LCI_PORT", env_port_text.as_str())],
        );
        server.wait_healthy(env_port, &reqwest::Client::new()).await;
    }
    // The flag beats both. `LCI_PORT` names another held port here: the
    // first server's port is free again and could be anyone's by now.
    let env_port_text = second_env.port.to_string();
    let mut server = Serve::spawn(
        &sandbox,
        &["serve", "--port", flag_port_text.as_str()],
        &[("LCI_PORT", env_port_text.as_str())],
    );
    server
        .wait_healthy(flag_port, &reqwest::Client::new())
        .await;
}

#[test]
fn the_cli_spawn_helper_removes_lci_variables_and_only_those() {
    let mut command = std::process::Command::new("not-run");
    strip_lci_env_named(
        &mut command,
        [
            "LCI_PORT",
            "lci_data_dir",
            "LCI_CANDIDATE_DUMP",
            "LCI_SOMETHING_NEW",
            "PATH",
            "LCIX",
            "HOME",
        ]
        .map(OsString::from),
    );
    let mut removed: Vec<String> = command
        .get_envs()
        .map(|(name, value)| {
            assert!(value.is_none(), "{name:?} is set, not removed");
            name.to_string_lossy().into_owned()
        })
        .collect();
    removed.sort();
    assert_eq!(
        removed,
        [
            "LCI_CANDIDATE_DUMP",
            "LCI_PORT",
            "LCI_SOMETHING_NEW",
            "lci_data_dir"
        ]
    );

    // Applied to the real environment it covers whatever the shell exports.
    let mut command = std::process::Command::new("not-run");
    strip_lci_env(&mut command);
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("LCI_")
        {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == name && value.is_none()),
                "{name:?} would reach a spawned CLI"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// `lci init`
// ---------------------------------------------------------------------------

/// A temp directory with a configuration file and a `PATH` directory, and the
/// helpers to run `init` and `config` against them.
struct InitBox {
    temp: tempfile::TempDir,
    config: PathBuf,
    bin: PathBuf,
    /// The `port` the configuration file names: free when chosen by
    /// [`Self::new`], or one the test holds itself ([`Self::on_port`]).
    port: u16,
    /// The ports [`Self::new`] and [`Self::on_port`] name in the model URLs,
    /// held for as long as the box lives.
    _model_ports: Vec<ClosedPort>,
}

impl InitBox {
    /// A configuration whose model URLs name ports that refuse connections,
    /// so a probe that slipped through would find nothing and never a real
    /// service.
    fn new(extra: &str) -> Self {
        // The model ports are held before `free_ports` chooses, so neither
        // can land on the chosen port.
        let model_ports = [ClosedPort::new(), ClosedPort::new()];
        let [port] = free_ports();
        Self::with_model_ports(port, extra, model_ports)
    }

    /// [`Self::new`] with the given `port`: one a test holds itself.
    fn on_port(port: u16, extra: &str) -> Self {
        Self::with_model_ports(port, extra, [ClosedPort::new(), ClosedPort::new()])
    }

    fn with_model_ports(port: u16, extra: &str, [embedder, reranker]: [ClosedPort; 2]) -> Self {
        let mut init = Self::build(
            &format!("http://127.0.0.1:{}/v1", embedder.port),
            &format!("http://127.0.0.1:{}/rerank", reranker.port),
            port,
            extra,
        );
        init._model_ports = vec![embedder, reranker];
        init
    }

    fn with_urls(embedding_url: &str, reranker_url: &str, extra: &str) -> Self {
        let [port] = free_ports();
        Self::build(embedding_url, reranker_url, port, extra)
    }

    fn build(embedding_url: &str, reranker_url: &str, port: u16, extra: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let config = temp.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                "port = {port}\nreadiness_timeout_seconds = 1\nembedding_url = '{embedding_url}'\nreranker_url = '{reranker_url}'\n{extra}"
            ),
        )
        .unwrap();
        Self {
            temp,
            config,
            bin,
            port,
            _model_ports: Vec::new(),
        }
    }

    /// The overlay `lci --config <config> init` writes.
    fn overlay(&self) -> PathBuf {
        self.temp.path().join("config.local.toml")
    }

    fn exe(tool: &str) -> String {
        format!("{tool}{}", std::env::consts::EXE_SUFFIX)
    }

    /// Where `tool` is installed by [`Self::install`].
    fn tool(&self, tool: &str) -> PathBuf {
        self.bin.join(Self::exe(tool))
    }

    /// Puts a copy of the fake language-server binary on this box's `PATH`
    /// under each name. Nothing runs it: `init` only looks for the file.
    fn install(&self, tools: &[&str]) {
        for tool in tools {
            std::fs::copy(env!("CARGO_BIN_EXE_fake-lsp-server"), self.tool(tool)).unwrap();
        }
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command
            .current_dir(self.temp.path())
            .arg("--config")
            .arg(&self.config)
            .args(args);
        strip_lci_env(&mut command)
            // The only place a tool can be found. The two Windows locations of
            // the ripgrep copies bundled with VS Code are pointed at the temp
            // directory too, where there are none.
            .env("PATH", &self.bin)
            .env("LOCALAPPDATA", self.temp.path())
            .env("ProgramFiles", self.temp.path());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// For tests whose fake service runs on the test's own runtime, which a
    /// blocking wait would starve.
    async fn run_async(&self, args: &[&str]) -> Output {
        tokio::process::Command::from(self.command(args))
            .output()
            .await
            .unwrap()
    }

    /// `config --json` of this box, which must succeed.
    fn config_report(&self) -> Value {
        let output = self.run(&["config", "--json"]);
        assert!(output.status.success(), "{}", stderr(&output));
        parse(&output)
    }

    fn overlay_text(&self) -> String {
        std::fs::read_to_string(self.overlay()).unwrap()
    }

    /// The overlay's setting lines (not comments, not blank, not tables).
    fn overlay_settings(&self) -> Vec<String> {
        self.overlay_text()
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#') && !line.starts_with('['))
            .map(str::to_owned)
            .collect()
    }

    /// Whether the overlay sets the top-level `key`.
    fn overlay_sets(&self, key: &str) -> bool {
        self.overlay_settings()
            .iter()
            .any(|line| line.starts_with(&format!("{key} = ")))
    }

    /// Every entry of the temp directory.
    fn entries(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}

fn setting_keys(settings: &[String]) -> Vec<String> {
    settings
        .iter()
        .map(|line| line.split(" = ").next().unwrap().to_owned())
        .collect()
}

#[test]
fn init_writes_the_absolute_paths_of_the_tools_on_path_and_config_reads_them_back() {
    // No `data_dir` in the file: the platform default is written out.
    let init = InitBox::new("");
    let installed = [
        "rust-analyzer",
        "typescript-language-server",
        "gopls",
        "clangd",
    ];
    init.install(&installed);
    let config_before = std::fs::read(&init.config).unwrap();
    assert_eq!(init.config_report()["layers"]["local"]["state"], "absent");

    let output = init.run(&["init", "--no-probe"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!("wrote {}\n", text(&init.overlay()))),
        "{stdout}"
    );
    assert!(stdout.contains("generated by lci init "), "{stdout}");

    // Exactly what was found, and the data directory, were written.
    let settings = init.overlay_settings();
    assert_eq!(
        setting_keys(&settings),
        [
            "data_dir",
            "rust_analyzer_path",
            "path", // [typescript]
            "path", // [go]
            "path", // [clangd]
        ]
    );
    let overlay = init.overlay_text();
    for table in ["[typescript]", "[go]", "[clangd]"] {
        assert!(overlay.contains(table), "{overlay}");
    }
    for table in ["[csharp]", "[python]", "[java]"] {
        assert!(!overlay.contains(&format!("\n{table}\n")), "{overlay}");
    }
    // The tools that were not there are explained, with how to get them.
    for expected in [
        "# rg: not on PATH (install ripgrep, or set ripgrep_path)",
        "# csharp-ls: not on PATH (dotnet tool install --global csharp-ls)",
        "# pyright-langserver: not on PATH (npm install -g pyright)",
        "# jdtls: [java].path is an installation directory",
    ] {
        assert!(
            overlay.contains(expected),
            "missing {expected:?} in\n{overlay}"
        );
    }
    assert!(overlay.contains("not for version control"), "{overlay}");
    // Nothing init does not establish is written: no port, model or URL.
    for key in [
        "port",
        "embedding_url",
        "embedding_model",
        "reranker_url",
        "reranker_model",
        "ripgrep_path",
    ] {
        assert!(!init.overlay_sets(key), "{key} was written:\n{overlay}");
    }

    // `config` now shows them, from the overlay, and the configuration is valid.
    let report = init.config_report();
    assert_eq!(report["valid"], true);
    assert_eq!(report["layers"]["local"]["state"], "present");
    assert_eq!(report["layers"]["local"]["path"], text(&init.overlay()));
    let values = &report["values"];
    for (key, tool) in [
        ("rust_analyzer_path", "rust-analyzer"),
        ("typescript.path", "typescript-language-server"),
        ("go.path", "gopls"),
        ("clangd.path", "clangd"),
    ] {
        assert_eq!(values[key]["source"], "local", "{key}");
        assert_eq!(values[key]["value"], text(&init.tool(tool)), "{key}");
    }
    assert_eq!(values["data_dir"]["source"], "local");
    assert_eq!(
        values["data_dir"]["value"],
        text(&local_code_intelligence::config::Config::default().data_dir)
    );
    for key in ["csharp.path", "python.path", "java.path", "ripgrep_path"] {
        assert_eq!(values[key]["source"], "default", "{key}");
    }
    // The main configuration was not touched, and nothing else was left over.
    assert_eq!(std::fs::read(&init.config).unwrap(), config_before);
    assert_eq!(init.entries(), ["bin", "config.local.toml", "config.toml"]);
}

#[test]
fn init_json_reports_what_it_set_found_and_noted() {
    let init = InitBox::new("");
    init.install(&["gopls"]);
    let output = init.run(&["init", "--no-probe", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["status"], "written");
    assert_eq!(report["written"], true);
    assert_eq!(report["exists"], false);
    assert_eq!(report["path"], text(&init.overlay()));
    assert_eq!(report["named_by"], "config-file-sibling");
    assert_eq!(report["overlay"], init.overlay_text());
    assert_eq!(report["set"], json!(["data_dir", "go.path"]));
    let tools = &report["detected"]["tools"];
    assert_eq!(tools["gopls"]["action"], "written");
    assert_eq!(tools["gopls"]["path"], text(&init.tool("gopls")));
    assert_eq!(tools["csharp-ls"]["action"], "not_found");
    assert!(
        tools["csharp-ls"]["hint"]
            .as_str()
            .unwrap()
            .contains("dotnet tool")
    );
    assert_eq!(report["detected"]["data_dir"]["action"], "written");
    assert_eq!(report["probes"]["embedding_url"]["status"], "skipped");
    assert_eq!(report["probes"]["reranker_url"]["status"], "skipped");
    assert_eq!(report["probes"]["port"]["value"], init.port);
    assert_eq!(report["valid"], true);
    assert!(report["comments"].as_array().unwrap().len() >= 8);
    assert_eq!(report["warnings"], json!([]));
}

/// N116: `serve` binds a free port for `port = 0`, so `init` has nothing to
/// probe: it reports the port as ephemeral, never as free or busy, and never
/// writes a port.
#[test]
fn init_does_not_probe_a_configured_port_of_zero() {
    let init = InitBox::on_port(0, "");
    let output = init.run(&["init", "--no-probe", "--dry-run", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["probes"]["port"]["value"], 0);
    assert_eq!(report["probes"]["port"]["state"], "ephemeral");
    assert!(report["probes"]["port"]["lci_serving"].is_null());
    let note = report["comments"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|comment| comment.as_str())
        .find(|comment| comment.starts_with("port:"))
        .expect("a port comment");
    assert!(
        note.contains("port: 0 asks the OS for a free port at every start"),
        "{note}"
    );
    assert!(!note.contains("is free on"), "{note}");
    assert_eq!(report["valid"], true);
    assert!(!report["overlay"].as_str().unwrap().contains("\nport = "));
}

#[test]
fn init_never_replaces_an_existing_overlay_without_force_and_dry_run_writes_nothing() {
    let init = InitBox::new("");
    init.install(&["gopls"]);
    let config_before = std::fs::read(&init.config).unwrap();

    // A dry run on a fresh box prints the overlay and creates no file.
    let output = init.run(&["init", "--no-probe", "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with(&format!(
            "would write {} (dry run)\n",
            text(&init.overlay())
        )),
        "{stdout}"
    );
    assert!(stdout.contains("[go]"), "{stdout}");
    assert!(!init.overlay().exists());
    let output = init.run(&["init", "--no-probe", "--dry-run", "--json"]);
    let report = parse(&output);
    assert_eq!(report["status"], "dry_run");
    assert_eq!(report["written"], false);
    assert!(!init.overlay().exists());

    // The first real run writes.
    let output = init.run(&["init", "--no-probe"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let written = std::fs::read(init.overlay()).unwrap();

    // The second refuses: exit 3, nothing printed on stdout, the file as it was.
    for args in [
        &["init", "--no-probe"][..],
        &["init", "--no-probe", "--no-local-config"][..],
    ] {
        let output = init.run(args);
        assert_eq!(output.status.code(), Some(3), "{}", stderr(&output));
        assert!(output.stdout.is_empty());
        let message = stderr(&output);
        assert!(message.contains(&text(&init.overlay())), "{message}");
        assert!(message.contains("--force"), "{message}");
        assert_eq!(std::fs::read(init.overlay()).unwrap(), written);
    }
    let output = init.run(&["init", "--no-probe", "--json"]);
    assert_eq!(output.status.code(), Some(3));
    let report = parse(&output);
    assert_eq!(report["status"], "exists");
    assert_eq!(report["written"], false);
    assert_eq!(report["exists"], true);
    assert!(report["error"].as_str().unwrap().contains("--force"));
    assert_eq!(std::fs::read(init.overlay()).unwrap(), written);

    // A dry run beside an existing file still previews, and says what a real
    // run would need.
    std::fs::write(init.overlay(), "# my own edits\n").unwrap();
    let output = init.run(&["init", "--no-probe", "--dry-run"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("needs --force"),
        "{}",
        stderr(&output)
    );
    assert_eq!(
        std::fs::read_to_string(init.overlay()).unwrap(),
        "# my own edits\n"
    );

    // --force replaces it, even when the old file is not a valid overlay.
    std::fs::write(init.overlay(), "port = \n").unwrap();
    let output = init.run(&["init", "--no-probe", "--force"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(
        setting_keys(&init.overlay_settings()),
        ["data_dir", "path"],
        "{}",
        init.overlay_text()
    );

    // The main configuration was never touched, and no temporary file stayed.
    assert_eq!(std::fs::read(&init.config).unwrap(), config_before);
    assert_eq!(init.entries(), ["bin", "config.local.toml", "config.toml"]);
}

#[test]
fn init_leaves_a_key_that_a_higher_layer_sets_alone() {
    let data = tempfile::tempdir().unwrap();
    let init = InitBox::new(&format!(
        "data_dir = '{}'\n[typescript]\npath = 'ts-from-file'\n",
        data.path().display()
    ));
    init.install(&["rust-analyzer", "typescript-language-server", "gopls"]);
    let mut command = init.command(&["init", "--no-probe"]);
    command.env("LCI_RUST_ANALYZER_PATH", "ra-from-env");
    let output = command.output().unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    let overlay = init.overlay_text();
    assert_eq!(
        setting_keys(&init.overlay_settings()),
        ["path"],
        "{overlay}"
    );
    assert!(overlay.contains("[go]"), "{overlay}");
    assert!(!overlay.contains("[typescript]"), "{overlay}");
    assert!(overlay.contains("# data_dir: set by file "), "{overlay}");
    assert!(
        overlay.contains("typescript.path is already set by file "),
        "{overlay}"
    );
    assert!(
        overlay.contains("rust_analyzer_path is already set by env LCI_RUST_ANALYZER_PATH"),
        "{overlay}"
    );
    let report = init.config_report();
    assert_eq!(report["values"]["typescript.path"]["source"], "file");
    assert_eq!(report["values"]["typescript.path"]["value"], "ts-from-file");
    assert_eq!(report["values"]["go.path"]["source"], "local");
    assert_eq!(report["values"]["data_dir"]["source"], "file");
    assert_eq!(report["values"]["rust_analyzer_path"]["source"], "default");
}

#[test]
fn init_without_a_configuration_file_writes_where_lci_local_config_points() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let gopls = bin.join(InitBox::exe("gopls"));
    std::fs::copy(env!("CARGO_BIN_EXE_fake-lsp-server"), &gopls).unwrap();
    // A file that does not exist, in a directory that does not exist either:
    // that is what `init` is for.
    let target = temp.path().join("user").join("nested").join("overlay.toml");
    // Without a configuration file the port would be the default 8768, which
    // `init` binds to see whether it is free: that is the real service's port,
    // so the test names one of its own.
    let [port] = free_ports();
    let port = port.to_string();
    let spawn = |args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command.current_dir(temp.path()).args(args);
        strip_lci_env(&mut command)
            .env("LCI_LOCAL_CONFIG", &target)
            .env("LCI_PORT", &port)
            .env("PATH", &bin)
            .env("LOCALAPPDATA", temp.path())
            .env("ProgramFiles", temp.path());
        command.output().unwrap()
    };

    // `config` keeps calling the absent file absent.
    let output = spawn(&["config", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["layers"]["local"]["state"], "absent");
    assert_eq!(report["layers"]["local"]["path"], text(&target));
    assert!(!target.exists());

    // No configuration file: the built-in model URLs are 8766 and 8767, so
    // nothing may be probed.
    let output = spawn(&["init", "--no-probe"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let overlay = std::fs::read_to_string(&target).unwrap();
    assert!(overlay.contains("[go]"), "{overlay}");
    let report = parse(&spawn(&["config", "--json"]));
    assert_eq!(report["layers"]["file"]["path"], Value::Null);
    assert_eq!(report["layers"]["local"]["named_by"], "LCI_LOCAL_CONFIG");
    assert_eq!(report["layers"]["local"]["state"], "present");
    assert_eq!(report["values"]["go.path"]["source"], "local");
    assert_eq!(report["values"]["go.path"]["value"], text(&gopls));
    assert_eq!(report["values"]["data_dir"]["source"], "local");

    // A second run refuses.
    let output = spawn(&["init", "--no-probe"]);
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn init_fails_without_writing_when_a_layer_cannot_be_read() {
    let init = InitBox::new("");
    let missing = init.temp.path().join("missing.toml");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .current_dir(init.temp.path())
        .arg("--config")
        .arg(&missing)
        .args(["init", "--no-probe"]);
    let output = strip_lci_env(&mut command).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("read configuration"),
        "{}",
        stderr(&output)
    );
    assert!(!init.temp.path().join("missing.local.toml").exists());

    // A malformed variable is the same kind of failure: nothing is guessed.
    let output = init
        .command(&["init", "--no-probe"])
        .env("LCI_PORT", "abc")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("LCI_PORT"), "{}", stderr(&output));
    assert!(!init.overlay().exists());
}

#[tokio::test]
async fn init_reports_the_model_services_and_never_writes_a_model_name() {
    let (_temp, fake, _state, _task) = fixture().await;
    let init = InitBox::with_urls(&fake.embedding_url, &fake.reranker_url, "");
    let output = init.run_async(&["init", "--json"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    for key in ["embedding_url", "reranker_url"] {
        let probe = &report["probes"][key];
        assert_eq!(probe["status"], "reachable", "{key}");
        assert_eq!(probe["model_listed"], true, "{key}");
        assert_eq!(
            probe["models"],
            json!(["qwen3-embedding-4b", "qwen3-reranker-4b"]),
            "{key}"
        );
    }
    let overlay = init.overlay_text();
    assert!(
        overlay.contains(
            "# embedding_url: reachable; lists models: qwen3-embedding-4b, qwen3-reranker-4b; embedding_model 'qwen3-embedding-4b' is listed"
        ),
        "{overlay}"
    );
    assert!(overlay.contains("# reranker_url: reachable"), "{overlay}");
    // No URL or model name leaked into the file's settings.
    assert!(
        !overlay.contains(&fake.embedding_url),
        "a configured URL was copied into the overlay:\n{overlay}"
    );
    assert_eq!(setting_keys(&init.overlay_settings()), ["data_dir"]);

    // The configured model is not among those listed: the comment says what
    // to set, the file's value stays, and nothing is written for it.
    let init = InitBox::with_urls(
        &fake.embedding_url,
        &fake.reranker_url,
        "embedding_model = 'other-embedder'\n",
    );
    let output = init.run_async(&["init"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let overlay = init.overlay_text();
    assert!(
        overlay.contains("embedding_model 'other-embedder' is NOT listed"),
        "{overlay}"
    );
    assert!(
        overlay.contains("init does not change model names"),
        "{overlay}"
    );
    assert!(
        overlay.contains("reranker_model 'qwen3-reranker-4b' is listed"),
        "{overlay}"
    );
    assert!(!init.overlay_sets("embedding_model"));
    let report = init.config_report();
    assert_eq!(report["values"]["embedding_model"]["source"], "file");
    assert_eq!(
        report["values"]["embedding_model"]["value"],
        "other-embedder"
    );
}

#[test]
fn init_succeeds_and_says_so_when_the_model_services_are_down() {
    let init = InitBox::new("");
    let started = std::time::Instant::now();
    let output = init.run(&["init", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(started.elapsed() < Duration::from_secs(60));
    let report = parse(&output);
    for key in ["embedding_url", "reranker_url"] {
        assert_eq!(report["probes"][key]["status"], "unreachable", "{key}");
    }
    let overlay = init.overlay_text();
    assert!(
        overlay.contains("# embedding_url: unreachable ("),
        "{overlay}"
    );
    assert!(
        overlay.contains("# reranker_url: unreachable ("),
        "{overlay}"
    );
    // A configured URL is not echoed into the file.
    assert!(!overlay.contains("://"), "{overlay}");
}

/// The generation model's port is never contacted, whichever host spelling
/// names it and whatever flags are given. The binary only judges the strings;
/// no socket is opened to the port by the product or by this test.
#[test]
fn init_refuses_to_probe_port_8765_and_still_writes_the_rest() {
    let init = InitBox::with_urls(
        "http://127.0.0.1:8765/v1",
        "http://192.0.2.10:8765/rerank",
        "",
    );
    init.install(&["gopls"]);
    for flags in [&[][..], &["--no-probe"][..]] {
        let mut args = vec!["init", "--force", "--json"];
        args.extend_from_slice(flags);
        let started = std::time::Instant::now();
        let output = init.run(&args);
        assert!(output.status.success(), "{}", stderr(&output));
        // A hang guard only: "refused" below is decided from the URL string
        // (`probe_refusal`), with no outbound probe, so wall time is not the
        // evidence (a 30 s bound failed under parallel builds).
        assert!(started.elapsed() < 2 * LOADED_BUDGET);
        let report = parse(&output);
        for key in ["embedding_url", "reranker_url"] {
            assert_eq!(report["probes"][key]["status"], "refused", "{key}");
            assert!(
                report["probes"][key]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("port 8765 is the generation model; LCI never contacts it"),
                "{key}"
            );
        }
        let message = stderr(&output);
        assert!(
            message.contains("embedding_url: not probed; port 8765"),
            "{message}"
        );
        assert!(
            message.contains("reranker_url: not probed; port 8765"),
            "{message}"
        );
        let overlay = init.overlay_text();
        assert!(
            overlay.contains("# embedding_url: not probed; port 8765 is the generation model; LCI never contacts it (AGENTS.md)"),
            "{overlay}"
        );
        // The rest was written; the configuration stays invalid, which init
        // reports rather than hides.
        assert!(overlay.contains("[go]"), "{overlay}");
        assert_eq!(report["valid"], false);
        let warnings = report["warnings"].as_array().unwrap();
        assert!(
            warnings
                .iter()
                .any(|warning| warning.as_str().unwrap().contains("init cannot fix it")),
            "{warnings:?}"
        );
    }
    // Reading the result back still reports the file's own invalid value.
    let output = init.run(&["config", "--json"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn init_refuses_to_probe_a_host_that_is_not_loopback() {
    let init = InitBox::with_urls(
        "http://192.0.2.10:8766/v1",
        "https://models.example.com/rerank",
        "",
    );
    let started = std::time::Instant::now();
    let output = init.run(&["init", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    // A hang guard only: the "refused" status below is decided from the URL
    // string (`probe_refusal`), with no outbound probe.
    assert!(started.elapsed() < 2 * LOADED_BUDGET);
    let report = parse(&output);
    for key in ["embedding_url", "reranker_url"] {
        assert_eq!(report["probes"][key]["status"], "refused", "{key}");
    }
    let overlay = init.overlay_text();
    assert!(
        overlay.contains("# embedding_url: not probed; init contacts loopback hosts only"),
        "{overlay}"
    );
    assert!(
        stderr(&output).contains("loopback hosts only"),
        "{}",
        stderr(&output)
    );
    // The hosts are not echoed into the file.
    assert!(!overlay.contains("192.0.2.10") && !overlay.contains("example.com"));
}

#[tokio::test]
async fn init_follows_no_redirect_from_a_service_it_asks() {
    // The service answers every request with a redirect to a listener the
    // test holds; that listener must never see a connection.
    let target = listener();
    let target_url = format!("http://{}/v1/models", target.local_addr().unwrap());
    let router = Router::new().fallback(move || {
        let target_url = target_url.clone();
        async move { axum::response::Redirect::temporary(&target_url) }
    });
    let service = loopback_tokio_listener();
    let address = service.local_addr().unwrap();
    let _task = tokio::spawn(async move {
        axum::serve(service, router).await.unwrap();
    });
    let init = InitBox::with_urls(
        &format!("http://{address}/v1"),
        &format!("http://{address}/rerank"),
        "",
    );
    let output = init.run_async(&["init", "--json"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    // The redirect is an answer, not a path to follow.
    let probe = &report["probes"]["embedding_url"];
    assert_eq!(probe["status"], "reachable");
    assert!(probe["detail"].as_str().unwrap().contains("307"), "{probe}");
    match target.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("init followed a redirect: {other:?}"),
    }
}

/// A model id is data from a service. A line break in one must not turn the
/// comment that lists it into a setting.
#[tokio::test]
async fn init_cannot_be_made_to_write_a_setting_by_what_a_service_lists() {
    let router = Router::new().route(
        "/v1/models",
        get(|| async {
            Json(json!({"data": [
                {"id": "evil\nport = 1\n[csharp]\npath = 'x'"},
                {"id": "ok-model"},
            ]}))
        }),
    );
    let service = loopback_tokio_listener();
    let address = service.local_addr().unwrap();
    let _task = tokio::spawn(async move {
        axum::serve(service, router).await.unwrap();
    });
    let init = InitBox::with_urls(
        &format!("http://{address}/v1"),
        &format!("http://{address}/rerank"),
        "",
    );
    let output = init.run_async(&["init"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let overlay = init.overlay_text();
    assert!(
        overlay.contains("evil port = 1 [csharp] path = 'x'"),
        "{overlay}"
    );
    for line in overlay.lines().filter(|line| !line.is_empty()) {
        assert!(
            line.starts_with('#') || line.starts_with("data_dir = "),
            "{line:?} in\n{overlay}"
        );
    }
    let report = init.config_report();
    assert_eq!(report["valid"], true);
    assert_eq!(report["values"]["port"]["source"], "file");
    assert_eq!(report["values"]["csharp.path"]["source"], "default");
}

#[test]
fn init_checks_the_port_by_binding_and_never_writes_it() {
    // Free. The port was free when chosen, but `init` has to bind it itself,
    // so it cannot stay held: another test's ephemeral bind may take it in
    // between. A port reported in use is retried with a fresh one, twice.
    let (init, report) = (0..3)
        .find_map(|_| {
            let init = InitBox::new("");
            let output = init.run(&["init", "--no-probe", "--json"]);
            assert!(output.status.success(), "{}", stderr(&output));
            let report = parse(&output);
            (report["probes"]["port"]["state"] != "in_use").then_some((init, report))
        })
        .expect("three freshly chosen ports were all reported in use");
    assert_eq!(report["probes"]["port"]["state"], "free");
    assert!(
        init.overlay_text()
            .contains(&format!("# port: {} is free on 127.0.0.1", init.port)),
        "{}",
        init.overlay_text()
    );
    assert!(!init.overlay_sets("port"));

    // In use, with --no-probe: reported, and the holder is not contacted.
    // The holder binds first, so the port is never released in between.
    let holder = loopback_listener();
    holder.set_nonblocking(true).unwrap();
    let init = InitBox::on_port(holder.local_addr().unwrap().port(), "");
    let output = init.run(&["init", "--no-probe", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["probes"]["port"]["state"], "in_use");
    assert!(report["probes"]["port"]["lci_serving"].is_null());
    let overlay = init.overlay_text();
    assert!(overlay.contains("is in use on 127.0.0.1"), "{overlay}");
    assert!(overlay.contains("--no-probe sends no request"), "{overlay}");
    match holder.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        other => panic!("init contacted the port holder under --no-probe: {other:?}"),
    }
    assert!(!init.overlay_sets("port"));
}

#[tokio::test]
async fn init_tells_a_running_lci_from_another_process_on_the_port() {
    // An LCI answering `/health`. Each holder binds before the box names its
    // port, so the port is never released in between.
    let holder = loopback_tokio_listener();
    let init = InitBox::on_port(holder.local_addr().unwrap().port(), "");
    let router = Router::new().route(
        "/health",
        get(|| async {
            Json(json!({"status": "ok", "service": "local-code-intelligence", "version": "0"}))
        }),
    );
    let task = tokio::spawn(async move {
        axum::serve(holder, router).await.unwrap();
    });
    let output = init.run_async(&["init", "--json"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["probes"]["port"]["state"], "in_use");
    assert_eq!(report["probes"]["port"]["lci_serving"], true);
    assert!(
        init.overlay_text().contains("an LCI is serving there"),
        "{}",
        init.overlay_text()
    );
    assert!(!init.overlay_sets("port"));
    task.abort();
    let _ = task.await;

    // Something else on the port: an HTTP server with no `/health`, and one
    // whose `/health` is not this service's.
    for router in [
        Router::new(),
        Router::new().route("/health", get(|| async { Json(json!({"status": "ok"})) })),
    ] {
        let holder = loopback_tokio_listener();
        let init = InitBox::on_port(holder.local_addr().unwrap().port(), "");
        let task = tokio::spawn(async move {
            axum::serve(holder, router).await.unwrap();
        });
        let output = init.run_async(&["init", "--json"]).await;
        assert!(output.status.success(), "{}", stderr(&output));
        let report = parse(&output);
        assert_eq!(report["probes"]["port"]["lci_serving"], false);
        assert!(
            init.overlay_text().contains("another process holds it"),
            "{}",
            init.overlay_text()
        );
        task.abort();
        let _ = task.await;
    }
}

#[test]
fn init_does_not_check_a_port_that_belongs_to_another_service() {
    // The configuration is invalid with port 8766: init reports that, binds
    // and asks nothing on that port, and says so.
    let [embedder, reranker] = free_ports();
    let init = InitBox::build(
        &format!("http://127.0.0.1:{embedder}/v1"),
        &format!("http://127.0.0.1:{reranker}/rerank"),
        8766,
        "",
    );
    let output = init.run(&["init", "--json"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let report = parse(&output);
    assert_eq!(report["probes"]["port"]["state"], "reserved");
    assert_eq!(report["valid"], false);
    assert!(
        init.overlay_text()
            .contains("is reserved for another service")
    );
    assert!(!init.overlay_sets("port"));
}
