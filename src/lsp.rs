use crate::{
    config::Config,
    lsp::{
        csharp::CSharpServer,
        transport::{JsonRpcClient, TransportConfig},
    },
    workspace::Workspace,
};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

pub mod csharp;
pub mod transport;

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
    pub language: Option<String>,
    pub provider: Option<String>,
}

struct Client {
    rpc: Arc<JsonRpcClient>,
    timeout: Duration,
}

impl Client {
    async fn spawn(config: &Config, workspace: &Workspace) -> Result<Arc<Self>> {
        let transport_config = TransportConfig {
            executable: config.rust_analyzer_path.clone(),
            args: Vec::new(),
            working_dir: workspace.path.clone(),
            timeout: Duration::from_secs(config.lsp_timeout_seconds),
            label: "rust-analyzer".into(),
        };
        let rpc = JsonRpcClient::spawn(
            &transport_config,
            Box::new(|msg: &Value| {
                msg.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
                    && msg["params"]["quiescent"].as_bool() == Some(true)
            }),
        )
        .await?;
        let client = Arc::new(Self {
            rpc: Arc::clone(&rpc),
            timeout: Duration::from_secs(config.lsp_timeout_seconds),
        });
        let root_uri = reqwest::Url::from_directory_path(&workspace.path)
            .map_err(|_| anyhow!("cannot convert workspace path to file URI"))?;
        let root_uri = root_uri.to_string();
        client
            .rpc
            .request(
                "initialize",
                json!({
                    "processId": std::process::id(), "rootUri": &root_uri,
                    "capabilities": {"workspace":{"symbol":{"resolveSupport":{"properties":["location.range"]}}},"experimental":{"serverStatusNotification":true}},
                    "workspaceFolders":[{"uri":&root_uri,"name":workspace.path.file_name().and_then(|v|v.to_str()).unwrap_or("workspace")}],
                    "initializationOptions":{"workspace":{"symbol":{"search":{"scope":"workspace","kind":"all_symbols","limit":256}}}}
                }),
            )
            .await
            .context("initialize rust-analyzer")?;
        client.rpc.notify("initialized", json!({})).await?;
        if !client.rpc.is_ready() {
            // Some rust-analyzer builds do not emit serverStatus even when the
            // capability is advertised. Give crate discovery a bounded head
            // start, then rely on normal request timeouts and restart recovery.
            let readiness_wait = client.timeout.min(Duration::from_secs(10));
            let _ = client.rpc.wait_ready(readiness_wait).await;
        }
        Ok(client)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc.request(method, params).await
    }
}

struct CSharpClient {
    rpc: Arc<JsonRpcClient>,
}

impl CSharpClient {
    async fn spawn(config: &Config, workspace: &Workspace) -> Result<Arc<Self>> {
        let adapter = CSharpServer::new(
            &config.csharp,
            Duration::from_secs(config.lsp_timeout_seconds),
        );
        let transport = adapter
            .transport_config(&workspace.path)
            .context("C# language server is disabled or has no executable")?;
        let rpc = JsonRpcClient::spawn(&transport, Box::new(|_| false)).await?;
        let params = adapter
            .initialize_params(&workspace.path)
            .context("cannot build C# language-server initialization")?;
        rpc.request("initialize", params)
            .await
            .context("initialize csharp-ls")?;
        rpc.notify("initialized", json!({})).await?;
        Ok(Arc::new(Self { rpc }))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc.request(method, params).await
    }
}

pub struct Manager {
    path: String,
    timeout_seconds: u64,
    sessions: Mutex<HashMap<String, Arc<Client>>>,
    csharp: crate::config::CSharpLspConfig,
    csharp_sessions: Mutex<HashMap<String, Arc<CSharpClient>>>,
}

impl Manager {
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.rust_analyzer_path.clone(),
            timeout_seconds: config.lsp_timeout_seconds,
            sessions: Mutex::new(HashMap::new()),
            csharp: config.csharp.clone(),
            csharp_sessions: Mutex::new(HashMap::new()),
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

    async fn csharp_client(&self, workspace: &Workspace) -> Result<Arc<CSharpClient>> {
        if let Some(client) = self
            .csharp_sessions
            .lock()
            .await
            .get(&workspace.id)
            .cloned()
        {
            return Ok(client);
        }
        let config = Config {
            csharp: self.csharp.clone(),
            lsp_timeout_seconds: self.timeout_seconds,
            ..Config::default()
        };
        let client = CSharpClient::spawn(&config, workspace).await?;
        self.csharp_sessions
            .lock()
            .await
            .insert(workspace.id.clone(), client.clone());
        Ok(client)
    }

    async fn csharp_request(
        &self,
        workspace: &Workspace,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let client = self.csharp_client(workspace).await?;
        match client.request(method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(first) => {
                self.csharp_sessions.lock().await.remove(&workspace.id);
                let client = self
                    .csharp_client(workspace)
                    .await
                    .with_context(|| format!("csharp-ls restart after: {first:#}"))?;
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
        locations_for(value, workspace, "rust", "rust-analyzer")
    }

    pub async fn csharp_symbols(
        &self,
        workspace: &Workspace,
        query: &str,
    ) -> Result<Vec<Location>> {
        let value = self
            .csharp_request(workspace, "workspace/symbol", json!({"query": query}))
            .await?;
        locations_for(value, workspace, "csharp", "csharp-ls")
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
        locations_for(value, workspace, "rust", "rust-analyzer")
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
        locations_for(value, workspace, "rust", "rust-analyzer")
    }

    pub async fn csharp_definition(
        &self,
        workspace: &Workspace,
        file: &Path,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .csharp_request(
                workspace,
                "textDocument/definition",
                json!({
                    "textDocument":{"uri":uri}, "position":{"line":line,"character":character}
                }),
            )
            .await?;
        locations_for(value, workspace, "csharp", "csharp-ls")
    }

    pub async fn csharp_references(
        &self,
        workspace: &Workspace,
        file: &Path,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .csharp_request(
                workspace,
                "textDocument/references",
                json!({
                    "textDocument":{"uri":uri}, "position":{"line":line,"character":character},
                    "context":{"includeDeclaration":include_declaration}
                }),
            )
            .await?;
        locations_for(value, workspace, "csharp", "csharp-ls")
    }

    pub async fn running(&self, workspace_id: &str) -> bool {
        self.sessions.lock().await.contains_key(workspace_id)
    }

    pub async fn csharp_running(&self, workspace_id: &str) -> bool {
        self.csharp_sessions.lock().await.contains_key(workspace_id)
    }
}

fn locations_for(
    value: Value,
    workspace: &Workspace,
    language: &str,
    provider: &str,
) -> Result<Vec<Location>> {
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
        let Some(relative_file_path) = relative_file_path else {
            continue;
        };
        output.push(Location {
            file_path,
            relative_file_path: Some(relative_file_path),
            start_line: range["start"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            start_character: range["start"]["character"].as_u64().unwrap_or(0) as u32,
            end_line: range["end"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            end_character: range["end"]["character"].as_u64().unwrap_or(0) as u32,
            name: item.get("name").and_then(Value::as_str).map(str::to_owned),
            kind: item.get("kind").and_then(Value::as_u64),
            language: Some(language.to_string()),
            provider: Some(provider.to_string()),
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
        let result = locations_for(json!([
            {"name":"item","kind":12,"location":{"uri":uri,"range":{"start":{"line":2,"character":3},"end":{"line":2,"character":7}}}},
            {"targetUri":uri,"targetSelectionRange":{"start":{"line":4,"character":1},"end":{"line":4,"character":5}}}
        ]), &workspace, "rust", "rust-analyzer").unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].relative_file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(result[0].start_line, 3);
        assert_eq!(result[1].start_line, 5);
    }
}
