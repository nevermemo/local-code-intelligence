use crate::{
    config::Config,
    language,
    lsp::{
        adapter::LspAdapter, csharp::CSharpServer, python::PythonServer, rust::RustServer,
        transport::JsonRpcClient, typescript::TypeScriptServer,
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
};
use tokio::sync::Mutex;

pub mod adapter;
pub mod csharp;
pub mod python;
pub mod rust;
pub mod transport;
pub mod typescript;

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

/// A live JSON-RPC session with one spawned, initialized language server.
struct GenericClient {
    rpc: Arc<JsonRpcClient>,
}

impl GenericClient {
    async fn spawn(adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> Result<Arc<Self>> {
        let transport = adapter
            .transport_config(&workspace.path)
            .with_context(|| format!("{} is disabled or has no executable", adapter.provider()))?;
        let rpc = JsonRpcClient::spawn(&transport, adapter.ready_predicate()).await?;
        let params = adapter
            .initialize_params(&workspace.path)
            .with_context(|| format!("cannot build {} initialization", adapter.provider()))?;
        rpc.request("initialize", params)
            .await
            .with_context(|| format!("initialize {}", adapter.provider()))?;
        rpc.notify("initialized", json!({})).await?;
        adapter.after_initialized(&rpc).await?;
        Ok(Arc::new(Self { rpc }))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc.request(method, params).await
    }
}

/// Per-workspace, per-provider language-server sessions. Rust (rust-analyzer)
/// and C# (csharp-ls) are always constructed; TypeScript/JavaScript and
/// Python join the same registry once their adapters land. A disabled or
/// unconfigured adapter simply never produces a session (`transport_config`
/// returns `None`), which surfaces as a clear error from the caller.
pub struct Manager {
    adapters: Vec<Arc<dyn LspAdapter>>,
    sessions: Mutex<HashMap<(String, String), Arc<GenericClient>>>,
}

impl Manager {
    pub fn new(config: &Config) -> Self {
        let timeout = std::time::Duration::from_secs(config.lsp_timeout_seconds);
        let adapters: Vec<Arc<dyn LspAdapter>> = vec![
            Arc::new(RustServer::new(config)),
            Arc::new(CSharpServer::new(&config.csharp, timeout)),
            Arc::new(TypeScriptServer::new(&config.typescript, timeout)),
            Arc::new(PythonServer::new(&config.python, timeout)),
        ];
        Self {
            adapters,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn adapters(&self) -> &[Arc<dyn LspAdapter>] {
        &self.adapters
    }

    /// Every adapter that can navigate the given Tree-sitter language
    /// identifier (normally at most one, but the registry allows more).
    pub fn adapters_for_language(&self, language: &str) -> Vec<&Arc<dyn LspAdapter>> {
        self.adapters
            .iter()
            .filter(|adapter| adapter.languages().contains(&language))
            .collect()
    }

    fn session_key(adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> (String, String) {
        (adapter.provider().to_string(), workspace.id.clone())
    }

    async fn client(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
    ) -> Result<Arc<GenericClient>> {
        let key = Self::session_key(adapter, workspace);
        if let Some(client) = self.sessions.lock().await.get(&key).cloned() {
            return Ok(client);
        }
        let client = GenericClient::spawn(adapter, workspace).await?;
        self.sessions.lock().await.insert(key, client.clone());
        Ok(client)
    }

    /// A request with no per-file preparation step: spawn-or-reuse a client,
    /// and on any failure discard the session and retry once with a fresh
    /// client (never a retry loop).
    async fn request(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let key = Self::session_key(adapter, workspace);
        let client = self.client(adapter, workspace).await?;
        match client.request(method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(first) => {
                self.sessions.lock().await.remove(&key);
                let client = self
                    .client(adapter, workspace)
                    .await
                    .with_context(|| format!("{} restart after: {first:#}", adapter.provider()))?;
                client.request(method, params).await
            }
        }
    }

    /// Same retry contract as [`Self::request`], but runs the adapter's
    /// `before_position_request` hook (e.g. TypeScript's `textDocument/
    /// didOpen`) against the same client before every attempt.
    async fn position_request(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let key = Self::session_key(adapter, workspace);
        let attempt = async {
            let client = self.client(adapter, workspace).await?;
            adapter
                .before_position_request(&client.rpc, file, relative)
                .await?;
            client.request(method, params.clone()).await
        };
        match attempt.await {
            Ok(value) => Ok(value),
            Err(first) => {
                self.sessions.lock().await.remove(&key);
                let client = self
                    .client(adapter, workspace)
                    .await
                    .with_context(|| format!("{} restart after: {first:#}", adapter.provider()))?;
                adapter
                    .before_position_request(&client.rpc, file, relative)
                    .await?;
                client.request(method, params).await
            }
        }
    }

    pub async fn symbols(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        query: &str,
    ) -> Result<Vec<Location>> {
        let value = self
            .request(
                adapter,
                workspace,
                "workspace/symbol",
                json!({"query": query, "searchScope":"workspace", "searchKind":"allSymbols"}),
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    pub async fn definition(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .position_request(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/definition",
                json!({"textDocument":{"uri":uri}, "position":{"line":line,"character":character}}),
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    // One more parameter than `definition` (`include_declaration`), which is
    // enough to cross clippy's default threshold; a parameter struct would
    // add indirection for one caller (`app.rs`) without real benefit here.
    #[allow(clippy::too_many_arguments)]
    pub async fn references(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .position_request(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/references",
                json!({
                    "textDocument":{"uri":uri}, "position":{"line":line,"character":character},
                    "context":{"includeDeclaration":include_declaration}
                }),
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    pub async fn running(&self, provider: &str, workspace_id: &str) -> bool {
        self.sessions
            .lock()
            .await
            .contains_key(&(provider.to_string(), workspace_id.to_string()))
    }
}

/// Normalizes an LSP `workspace/symbol`/`textDocument/definition`/
/// `textDocument/references` response into this app's `Location` shape.
/// Each location's `language` is derived from its own file (not fixed to one
/// value), since a single TypeScript-adapter response can legitimately span
/// `.ts`/`.tsx`/`.js`/`.jsx` results.
fn locations_for(value: Value, workspace: &Workspace, provider: &str) -> Result<Vec<Location>> {
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
        let detected_language = language::for_path(Path::new(&relative_file_path))
            .map(|adapter| adapter.identifier().to_string());
        output.push(Location {
            file_path,
            relative_file_path: Some(relative_file_path),
            start_line: range["start"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            start_character: range["start"]["character"].as_u64().unwrap_or(0) as u32,
            end_line: range["end"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            end_character: range["end"]["character"].as_u64().unwrap_or(0) as u32,
            name: item.get("name").and_then(Value::as_str).map(str::to_owned),
            kind: item.get("kind").and_then(Value::as_u64),
            language: detected_language,
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
        let result = locations_for(
            json!([
                {"name":"item","kind":12,"location":{"uri":uri,"range":{"start":{"line":2,"character":3},"end":{"line":2,"character":7}}}},
                {"targetUri":uri,"targetSelectionRange":{"start":{"line":4,"character":1},"end":{"line":4,"character":5}}}
            ]),
            &workspace,
            "rust-analyzer",
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].relative_file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(result[0].start_line, 3);
        assert_eq!(result[0].language.as_deref(), Some("rust"));
        assert_eq!(result[0].provider.as_deref(), Some("rust-analyzer"));
        assert_eq!(result[1].start_line, 5);
    }
}
