//! Plain JSON-over-HTTP mirror of the MCP tool surface, for clients that
//! don't want to speak MCP. Every handler here calls the exact same `App`
//! method as its matching MCP tool in `server.rs` and shares the same
//! process, port, and `Arc<App>` state -- this is a second transport onto
//! identical application logic, not a second service. Request bodies reuse
//! `server.rs`'s existing `schemars`-derived argument structs directly, so
//! the OpenAPI document below can never drift from what a request actually
//! accepts.

use crate::{
    app::App,
    server::{PositionArgs, ReferencesArgs, SearchArgs, SymbolArgs, WorkspaceArgs},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use schemars::schema_for;
use serde::Serialize;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

fn respond<T: Serialize>(value: anyhow::Result<T>) -> (StatusCode, Json<Value>) {
    match value.and_then(|v| Ok(serde_json::to_value(v)?)) {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{error:#}")})),
        ),
    }
}

async fn index_workspace(
    State(app): State<Arc<App>>,
    Json(args): Json<WorkspaceArgs>,
) -> (StatusCode, Json<Value>) {
    respond(app.index(Path::new(&args.workspace_path)).await)
}

async fn index_status(
    State(app): State<Arc<App>>,
    Json(args): Json<WorkspaceArgs>,
) -> (StatusCode, Json<Value>) {
    respond(app.status(Path::new(&args.workspace_path)).await)
}

async fn list_indexed_files(
    State(app): State<Arc<App>>,
    Json(args): Json<WorkspaceArgs>,
) -> (StatusCode, Json<Value>) {
    respond(app.indexed_files(Path::new(&args.workspace_path)).await)
}

async fn watch_workspace(
    State(app): State<Arc<App>>,
    Json(args): Json<WorkspaceArgs>,
) -> (StatusCode, Json<Value>) {
    respond(app.watch(Path::new(&args.workspace_path)).await)
}

async fn unwatch_workspace(
    State(app): State<Arc<App>>,
    Json(args): Json<WorkspaceArgs>,
) -> (StatusCode, Json<Value>) {
    respond(app.unwatch(Path::new(&args.workspace_path)).await)
}

async fn search_symbols(
    State(app): State<Arc<App>>,
    Json(args): Json<SymbolArgs>,
) -> (StatusCode, Json<Value>) {
    respond(
        app.symbols(Path::new(&args.workspace_path), &args.query)
            .await,
    )
}

async fn find_definition(
    State(app): State<Arc<App>>,
    Json(args): Json<PositionArgs>,
) -> (StatusCode, Json<Value>) {
    respond(
        app.definition(
            Path::new(&args.workspace_path),
            &args.relative_file_path,
            args.line,
            args.character,
        )
        .await,
    )
}

async fn find_references(
    State(app): State<Arc<App>>,
    Json(args): Json<ReferencesArgs>,
) -> (StatusCode, Json<Value>) {
    respond(
        app.references(
            Path::new(&args.workspace_path),
            &args.relative_file_path,
            args.line,
            args.character,
            args.include_declaration,
        )
        .await,
    )
}

async fn search_code(
    State(app): State<Arc<App>>,
    Json(args): Json<SearchArgs>,
) -> (StatusCode, Json<Value>) {
    respond(
        app.search_with_filters(
            Path::new(&args.workspace_path),
            &args.query,
            args.top_k,
            crate::filter::FilterRequest {
                languages: args.languages,
                include_paths: args.include_paths,
                exclude_paths: args.exclude_paths,
                source_roles: args.source_roles,
            },
        )
        .await,
    )
}

async fn service_status(State(app): State<Arc<App>>) -> (StatusCode, Json<Value>) {
    respond(Ok(app.service_status().await))
}

/// Hand-assembled rather than generated from route attributes: every
/// request schema below is the identical `schemars` derive already used
/// for the matching MCP tool's arguments in `server.rs`, so the documented
/// request contract cannot silently drift from what a handler actually
/// deserializes. Response bodies are intentionally left as an open
/// `object` -- see README.md's "MCP tools" table for the exact shape each
/// endpoint returns, since committing a second, hand-maintained copy of
/// those result schemas here would just be one more place for them to go
/// stale.
fn openapi_document() -> Value {
    let workspace_schema = serde_json::to_value(schema_for!(WorkspaceArgs)).unwrap();
    let search_schema = serde_json::to_value(schema_for!(SearchArgs)).unwrap();
    let symbol_schema = serde_json::to_value(schema_for!(SymbolArgs)).unwrap();
    let position_schema = serde_json::to_value(schema_for!(PositionArgs)).unwrap();
    let references_schema = serde_json::to_value(schema_for!(ReferencesArgs)).unwrap();

    let result_schema = json!({
        "type": "object",
        "description": "Identical JSON to the matching MCP tool's structured result; see README.md's \"MCP tools\" table for the exact shape."
    });
    let error_schema = json!({
        "type": "object",
        "properties": {"error": {"type": "string"}},
        "required": ["error"]
    });

    let post_operation = |summary: &str, operation_id: &str, schema: Value| {
        json!({
            "post": {
                "operationId": operation_id,
                "summary": summary,
                "requestBody": {
                    "required": true,
                    "content": {"application/json": {"schema": schema}}
                },
                "responses": {
                    "200": {
                        "description": "Success",
                        "content": {"application/json": {"schema": result_schema}}
                    },
                    "500": {
                        "description": "Application error (mirrors the MCP tool's error text)",
                        "content": {"application/json": {"schema": error_schema}}
                    }
                }
            }
        })
    };

    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "local-code-intelligence",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Plain JSON-over-HTTP mirror of LCI's MCP tools, for clients that don't speak MCP. Every endpoint below wraps exactly the same application logic as the matching MCP tool of the same name -- see README.md for full semantics; this document only describes the wire shape."
        },
        "servers": [{"url": "http://127.0.0.1:8768"}],
        "paths": {
            "/v1/index_workspace": post_operation("Index a workspace", "index_workspace", workspace_schema.clone()),
            "/v1/index_status": post_operation("Read persistent index status", "index_status", workspace_schema.clone()),
            "/v1/list_indexed_files": post_operation("List every indexed file", "list_indexed_files", workspace_schema.clone()),
            "/v1/watch_workspace": post_operation("Start watching a workspace for changes", "watch_workspace", workspace_schema.clone()),
            "/v1/unwatch_workspace": post_operation("Stop watching a workspace", "unwatch_workspace", workspace_schema),
            "/v1/search_symbols": post_operation("Search workspace symbols via language servers", "search_symbols", symbol_schema),
            "/v1/find_definition": post_operation("Resolve a definition at a source position", "find_definition", position_schema),
            "/v1/find_references": post_operation("Find references at a source position", "find_references", references_schema),
            "/v1/search_code": post_operation("Hybrid semantic/lexical/LSP code search", "search_code", search_schema),
            "/v1/service_status": {
                "get": {
                    "operationId": "service_status",
                    "summary": "Report readiness and optional-tooling diagnostics",
                    "responses": {
                        "200": {
                            "description": "Success",
                            "content": {"application/json": {"schema": result_schema}}
                        }
                    }
                }
            }
        }
    })
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v1/index_workspace", post(index_workspace))
        .route("/v1/index_status", post(index_status))
        .route("/v1/list_indexed_files", post(list_indexed_files))
        .route("/v1/watch_workspace", post(watch_workspace))
        .route("/v1/unwatch_workspace", post(unwatch_workspace))
        .route("/v1/search_symbols", post(search_symbols))
        .route("/v1/find_definition", post(find_definition))
        .route("/v1/find_references", post(find_references))
        .route("/v1/search_code", post(search_code))
        .route("/v1/service_status", get(service_status))
        .route("/openapi.json", get(|| async { Json(openapi_document()) }))
}
