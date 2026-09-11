use crate::{config::Config, workspace::Workspace};
use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
};

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct Location {
    pub file_path: PathBuf,
    pub relative_file_path: Option<String>,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub name: Option<String>,
    pub kind: Option<u64>,
}

struct Client {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    ready: Arc<tokio::sync::Notify>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
}

impl Client {
    async fn spawn(config: &Config, workspace: &Workspace) -> Result<Arc<Self>> {
        let mut child = Command::new(&config.rust_analyzer_path)
            .current_dir(&workspace.path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {}", config.rust_analyzer_path))?;
        let stdin = child
            .stdin
            .take()
            .context("rust-analyzer stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("rust-analyzer stdout unavailable")?;
        let stderr = child
            .stderr
            .take()
            .context("rust-analyzer stderr unavailable")?;
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new(tokio::sync::Notify::new());
        let is_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader_pending = Arc::clone(&pending);
        let reader_ready = Arc::clone(&ready);
        let reader_is_ready = Arc::clone(&is_ready);
        tokio::spawn(async move {
            if let Err(error) = read_messages(
                stdout,
                reader_pending.clone(),
                reader_ready,
                reader_is_ready,
            )
            .await
            {
                tracing::warn!(error = %error, "rust-analyzer protocol reader stopped");
            }
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err("rust-analyzer exited".into()));
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(message = %line, "rust-analyzer");
            }
        });
        let client = Arc::new(Self {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            pending,
            next_id: AtomicU64::new(1),
            timeout: Duration::from_secs(config.lsp_timeout_seconds),
            ready,
            is_ready,
        });
        let root_uri = reqwest::Url::from_directory_path(&workspace.path)
            .map_err(|_| anyhow!("cannot convert workspace path to file URI"))?;
        let root_uri = root_uri.to_string();
        client.request("initialize", json!({
            "processId": std::process::id(), "rootUri": &root_uri,
            "capabilities": {"workspace":{"symbol":{"resolveSupport":{"properties":["location.range"]}}},"experimental":{"serverStatusNotification":true}},
            "workspaceFolders":[{"uri":&root_uri,"name":workspace.path.file_name().and_then(|v|v.to_str()).unwrap_or("workspace")}],
            "initializationOptions":{"workspace":{"symbol":{"search":{"scope":"workspace","kind":"all_symbols","limit":256}}}}
        })).await.context("initialize rust-analyzer")?;
        client.notify("initialized", json!({})).await?;
        if !client.is_ready.load(Ordering::Acquire) {
            // Some rust-analyzer builds do not emit serverStatus even when the
            // capability is advertised. Give crate discovery a bounded head
            // start, then rely on normal request timeouts and restart recovery.
            let readiness_wait = client.timeout.min(Duration::from_secs(10));
            let _ = tokio::time::timeout(readiness_wait, client.ready.notified()).await;
        }
        Ok(client)
    }

    async fn write(&self, value: &Value) -> Result<()> {
        let body = serde_json::to_vec(value)?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        stdin.write_all(&body).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self
            .write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(message))) => bail!(message),
            Ok(Err(_)) => bail!("rust-analyzer response channel closed"),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                bail!(
                    "rust-analyzer request timed out after {} seconds",
                    self.timeout.as_secs()
                )
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

async fn read_messages(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    ready: Arc<tokio::sync::Notify>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
) -> Result<()> {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut content_length = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await? == 0 {
                return Ok(());
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = Some(value.trim().parse::<usize>()?);
            }
        }
        let length = content_length.context("LSP message missing Content-Length")?;
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await?;
        let message: Value = serde_json::from_slice(&body)?;
        if message.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
            && message["params"]["quiescent"].as_bool() == Some(true)
        {
            is_ready.store(true, Ordering::Release);
            ready.notify_waiters();
            ready.notify_one();
        }
        let Some(id) = message.get("id").and_then(Value::as_u64) else {
            continue;
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            let response = match message.get("error") {
                Some(error) => Err(error.to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = sender.send(response);
        }
    }
}

pub struct Manager {
    path: String,
    timeout_seconds: u64,
    sessions: Mutex<HashMap<String, Arc<Client>>>,
}

impl Manager {
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.rust_analyzer_path.clone(),
            timeout_seconds: config.lsp_timeout_seconds,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    async fn client(&self, workspace: &Workspace) -> Result<Arc<Client>> {
        if let Some(client) = self.sessions.lock().await.get(&workspace.id).cloned() {
            return Ok(client);
        }
        let config = Config {
            rust_analyzer_path: self.path.clone(),
            lsp_timeout_seconds: self.timeout_seconds,
            ..Config::default()
        };
        let client = Client::spawn(&config, workspace).await?;
        self.sessions
            .lock()
            .await
            .insert(workspace.id.clone(), client.clone());
        Ok(client)
    }

    async fn request(&self, workspace: &Workspace, method: &str, params: Value) -> Result<Value> {
        let client = self.client(workspace).await?;
        match client.request(method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(first) => {
                self.sessions.lock().await.remove(&workspace.id);
                let client = self
                    .client(workspace)
                    .await
                    .with_context(|| format!("rust-analyzer restart after: {first:#}"))?;
                client.request(method, params).await
            }
        }
    }

    pub async fn symbols(&self, workspace: &Workspace, query: &str) -> Result<Vec<Location>> {
        let value = self
            .request(
                workspace,
                "workspace/symbol",
                json!({
                    "query": query, "searchScope":"workspace", "searchKind":"allSymbols"
                }),
            )
            .await?;
        locations(value, workspace)
    }

    pub async fn definition(
        &self,
        workspace: &Workspace,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?;
        let uri = uri.to_string();
        let value = self
            .request(
                workspace,
                "textDocument/definition",
                json!({
                    "textDocument":{"uri":uri}, "position":{"line":line,"character":character}
                }),
            )
            .await?;
        locations(value, workspace)
    }

    pub async fn references(
        &self,
        workspace: &Workspace,
        file: &Path,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?;
        let uri = uri.to_string();
        let value = self
            .request(
                workspace,
                "textDocument/references",
                json!({
                    "textDocument":{"uri":uri}, "position":{"line":line,"character":character},
                    "context":{"includeDeclaration":include_declaration}
                }),
            )
            .await?;
        locations(value, workspace)
    }

    pub async fn running(&self, workspace_id: &str) -> bool {
        self.sessions.lock().await.contains_key(workspace_id)
    }
}

fn locations(value: Value, workspace: &Workspace) -> Result<Vec<Location>> {
    let values = if value.is_null() {
        vec![]
    } else if let Some(items) = value.as_array() {
        items.clone()
    } else {
        vec![value]
    };
    let mut output = Vec::new();
    for item in values {
        let location = item.get("location").unwrap_or(&item);
        let Some(uri) = location
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let range = location
            .get("range")
            .or_else(|| item.get("range"))
            .or_else(|| item.get("targetSelectionRange"))
            .or_else(|| item.get("targetRange"));
        let Some(range) = range else { continue };
        let url = reqwest::Url::parse(uri)?;
        let Ok(file_path) = url.to_file_path() else {
            continue;
        };
        let relative_file_path = file_path
            .strip_prefix(&workspace.path)
            .ok()
            .and_then(Path::to_str)
            .map(|p| p.replace('\\', "/"));
        output.push(Location {
            file_path,
            relative_file_path,
            start_line: range["start"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            start_character: range["start"]["character"].as_u64().unwrap_or(0) as u32,
            end_line: range["end"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            end_character: range["end"]["character"].as_u64().unwrap_or(0) as u32,
            name: item.get("name").and_then(Value::as_str).map(str::to_owned),
            kind: item.get("kind").and_then(Value::as_u64),
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_locations_and_location_links() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let file = workspace.path.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn item() {}\n").unwrap();
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let result = locations(json!([
            {"name":"item","kind":12,"location":{"uri":uri,"range":{"start":{"line":2,"character":3},"end":{"line":2,"character":7}}}},
            {"targetUri":uri,"targetSelectionRange":{"start":{"line":4,"character":1},"end":{"line":4,"character":5}}}
        ]), &workspace).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].relative_file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(result[0].start_line, 3);
        assert_eq!(result[1].start_line, 5);
    }
}
