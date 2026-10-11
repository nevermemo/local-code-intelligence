//! `lci config` and the layered configuration, through the real binary.
//!
//! Every spawned process gets `--config <temp file>` (or `LCI_CONFIG` /
//! `LCI_LOCAL_CONFIG` pointing into the temp directory when a test is about
//! the case without `--config`) and has every `LCI_*` variable of the test
//! process removed, so neither the developer's shell nor their own
//! user-level overlay can change a result. Model URLs point at listeners the
//! test owns, which must see no connection, or at nothing; no test touches
//! ports 8765-8767, and `serve` binds loopback ports the test picked.

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
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

/// Ports nothing is listening on, all distinct: they are held together until
/// every one is chosen.
fn free_ports<const N: usize>() -> [u16; N] {
    let held: Vec<TcpListener> = (0..N)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let mut ports = [0; N];
    for (port, listener) in ports.iter_mut().zip(&held) {
        *port = listener.local_addr().unwrap().port();
    }
    ports
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
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(sink)
            .spawn()
            .unwrap();
        Self { child, log }
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

fn nothing_listens_on(port: u16) -> bool {
    TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_err()
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

#[tokio::test]
async fn serve_port_beats_lci_port_which_beats_the_overlay() {
    let sandbox = Sandbox::new("");
    let [overlay_port, env_port, flag_port] = free_ports();
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
        assert!(nothing_listens_on(overlay_port));
    }
    // The flag beats both.
    let mut server = Serve::spawn(
        &sandbox,
        &["serve", "--port", flag_port_text.as_str()],
        &[("LCI_PORT", env_port_text.as_str())],
    );
    server
        .wait_healthy(flag_port, &reqwest::Client::new())
        .await;
    assert!(nothing_listens_on(overlay_port));
    assert!(nothing_listens_on(env_port));
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
