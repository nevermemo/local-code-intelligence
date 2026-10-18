//! Plain JSON-over-HTTP mirror of the MCP tool surface, for clients that
//! don't want to speak MCP. Every handler here calls the exact same `App`
//! method as its matching MCP tool in `server.rs` (except `release_workspace`,
//! which is REST only) and shares the same process, port, and `Arc<App>`
//! state -- this is a second transport onto identical application logic, not
//! a second service. Request bodies reuse `server.rs`'s existing
//! `schemars`-derived argument structs directly, so the OpenAPI document
//! below can never drift from what a request actually accepts.

use crate::{
    app::{App, recipes},
    config::Config,
    server::{PositionArgs, ReferencesArgs, SearchArgs, SymbolArgs, WorkspaceArgs},
};
use axum::{
    Json, Router,
    extract::{Query, State},
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

/// REST only: there is no MCP tool for this, so `tools/list` is unchanged.
async fn release_workspace(
    State(app): State<Arc<App>>,
    Json(args): Json<WorkspaceArgs>,
) -> (StatusCode, Json<Value>) {
    respond(
        app.release_language_servers(Path::new(&args.workspace_path))
            .await,
    )
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

/// The query string of `POST /v1/search_code`. The request body is the same
/// [`SearchArgs`] as the MCP tool's; the profile is a query parameter so that
/// the body, like the MCP tool's arguments, is unchanged.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchQuery {
    /// A configured `[search_profiles.<name>]`; absent is the default search.
    /// An unknown name fails the request with an error that lists the
    /// configured ones.
    search_profile: Option<String>,
}

async fn search_code(
    State(app): State<Arc<App>>,
    Query(query): Query<SearchQuery>,
    Json(args): Json<SearchArgs>,
) -> (StatusCode, Json<Value>) {
    respond(
        app.search_in_profile(
            Path::new(&args.workspace_path),
            &args.query,
            args.top_k,
            crate::filter::FilterRequest {
                languages: args.languages,
                include_paths: args.include_paths,
                exclude_paths: args.exclude_paths,
                source_roles: args.source_roles,
            },
            query.search_profile.as_deref(),
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
///
/// `servers` names the loopback port the service is configured to listen on
/// (`Config::port`), so the document is right for any port, not only 8768.
fn openapi_document(config: &Config) -> Value {
    let port = config.port;
    let request_schema = |schema| {
        let mut schema = serde_json::to_value(schema).unwrap();
        schema.as_object_mut().unwrap().remove("$schema");
        schema
    };
    let workspace_schema = request_schema(schema_for!(WorkspaceArgs));
    let search_schema = request_schema(schema_for!(SearchArgs));
    let symbol_schema = request_schema(schema_for!(SymbolArgs));
    let position_schema = request_schema(schema_for!(PositionArgs));
    let references_schema = request_schema(schema_for!(ReferencesArgs));

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
                    },
                    "400": {
                        "description": "Invalid request body or query parameter (axum extractor rejection)",
                        "content": {"text/plain": {"schema": {"type": "string"}}}
                    },
                    "415": {
                        "description": "Unsupported request media type (axum extractor rejection)",
                        "content": {"text/plain": {"schema": {"type": "string"}}}
                    },
                    "422": {
                        "description": "Request body failed deserialization (axum extractor rejection)",
                        "content": {"text/plain": {"schema": {"type": "string"}}}
                    },
                    "403": {
                        "description": "Request rejected by the loopback guard",
                        "content": {"application/json": {"schema": error_schema}}
                    }
                }
            }
        })
    };

    // Same operation as the others, plus the one query parameter that picks a
    // `[search_profiles.<name>]`. Documented statically: the names are the
    // operator's configuration, and an unknown one is an error that lists them.
    let mut search_code_path = post_operation(
        "Hybrid semantic/lexical/LSP code search",
        "search_code",
        search_schema,
    );
    search_code_path["post"]["parameters"] = json!([{
        "name": "search_profile",
        "in": "query",
        "required": false,
        "description": "A configured [search_profiles.<name>] to run this search with; the same switches as the MCP endpoint /mcp/<name>. Omitted, the search is the default one and the result has no search_profile. An unknown name is an error that lists the configured ones.",
        "schema": {"type": "string"}
    }]);

    let mut paths = serde_json::Map::new();
    if config.recipes.enabled {
        for (path, summary, schema) in [
            (
                "config_readers",
                "Declarations that read or set a config key, and the tests that reference them",
                request_schema(schema_for!(recipes::ConfigReadersArgs)),
            ),
            (
                "impact_rings",
                "Declarations that reference a symbol (ring 1) and those that reference them (ring 2)",
                request_schema(schema_for!(recipes::ImpactRingsArgs)),
            ),
            (
                "flow_path",
                "Functions on the shortest call paths between two functions",
                request_schema(schema_for!(recipes::FlowPathArgs)),
            ),
            (
                "flow_from",
                "The traversal engine's flow walk from a function",
                request_schema(schema_for!(recipes::FlowFromArgs)),
            ),
            (
                "config_neighbours",
                "Keys a declaration touches, their other readers, and its callers",
                request_schema(schema_for!(recipes::ConfigNeighboursArgs)),
            ),
        ] {
            paths.insert(format!("/v1/{path}"), post_operation(summary, path, schema));
        }
    }
    if config.compose.enabled {
        paths.insert(
            "/v1/lci_compose".into(),
            post_operation(
                "Up to six operator steps with references between them",
                "lci_compose",
                request_schema(schema_for!(recipes::ComposeArgs)),
            ),
        );
    }

    let mut document = json!({
        "openapi": "3.1.0",
        "info": {
            "title": "local-code-intelligence",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Plain JSON-over-HTTP mirror of LCI's MCP tools, for clients that don't speak MCP. Every endpoint below wraps exactly the same application logic as the matching MCP tool of the same name -- see README.md for full semantics; this document only describes the wire shape. The one exception is /v1/release_workspace, which has no MCP tool."
        },
        "servers": [{"url": format!("http://127.0.0.1:{port}")}],
        "paths": {
            "/v1/index_workspace": post_operation("Index a workspace", "index_workspace", workspace_schema.clone()),
            "/v1/index_status": post_operation("Read persistent index status", "index_status", workspace_schema.clone()),
            "/v1/list_indexed_files": post_operation("List every indexed file", "list_indexed_files", workspace_schema.clone()),
            "/v1/watch_workspace": post_operation("Start watching a workspace for changes", "watch_workspace", workspace_schema.clone()),
            "/v1/unwatch_workspace": post_operation("Stop watching a workspace", "unwatch_workspace", workspace_schema.clone()),
            "/v1/release_workspace": post_operation("Shut down the language servers running for a workspace (REST only; no MCP tool)", "release_workspace", workspace_schema),
            "/v1/search_symbols": post_operation("Search workspace symbols via language servers", "search_symbols", symbol_schema),
            "/v1/find_definition": post_operation("Resolve a definition at a source position", "find_definition", position_schema),
            "/v1/find_references": post_operation("Find references at a source position", "find_references", references_schema),
            "/v1/search_code": search_code_path,
            "/v1/service_status": {
                "get": {
                    "operationId": "service_status",
                    "summary": "Report readiness and optional-tooling diagnostics",
                    "responses": {
                        "200": {
                            "description": "Success",
                            "content": {"application/json": {"schema": result_schema}}
                        },
                        "403": {
                            "description": "Request rejected by the loopback guard",
                            "content": {"application/json": {"schema": error_schema}}
                        }
                    }
                }
            }
        }
    });
    if let Some(existing) = document["paths"].as_object_mut() {
        existing.extend(paths);
    }
    document
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/v1/index_workspace", post(index_workspace))
        .route("/v1/index_status", post(index_status))
        .route("/v1/list_indexed_files", post(list_indexed_files))
        .route("/v1/watch_workspace", post(watch_workspace))
        .route("/v1/unwatch_workspace", post(unwatch_workspace))
        .route("/v1/release_workspace", post(release_workspace))
        .route("/v1/search_symbols", post(search_symbols))
        .route("/v1/find_definition", post(find_definition))
        .route("/v1/find_references", post(find_references))
        .route("/v1/search_code", post(search_code))
        .route("/v1/service_status", get(service_status))
        .route(
            "/openapi.json",
            get(|State(app): State<Arc<App>>| async move { Json(openapi_document(&app.config)) }),
        )
}

async fn recipe(app: Arc<App>, request: recipes::Request) -> (StatusCode, Json<Value>) {
    respond(recipes::serve(&app, &request).await)
}

async fn config_readers(
    State(app): State<Arc<App>>,
    Json(args): Json<recipes::ConfigReadersArgs>,
) -> (StatusCode, Json<Value>) {
    recipe(app, recipes::Request::ConfigReaders(args)).await
}

async fn impact_rings(
    State(app): State<Arc<App>>,
    Json(args): Json<recipes::ImpactRingsArgs>,
) -> (StatusCode, Json<Value>) {
    recipe(app, recipes::Request::ImpactRings(args)).await
}

async fn flow_path(
    State(app): State<Arc<App>>,
    Json(args): Json<recipes::FlowPathArgs>,
) -> (StatusCode, Json<Value>) {
    recipe(app, recipes::Request::FlowPath(args)).await
}

async fn flow_from(
    State(app): State<Arc<App>>,
    Json(args): Json<recipes::FlowFromArgs>,
) -> (StatusCode, Json<Value>) {
    recipe(app, recipes::Request::FlowFrom(args)).await
}

async fn config_neighbours(
    State(app): State<Arc<App>>,
    Json(args): Json<recipes::ConfigNeighboursArgs>,
) -> (StatusCode, Json<Value>) {
    recipe(app, recipes::Request::ConfigNeighbours(args)).await
}

async fn lci_compose(
    State(app): State<Arc<App>>,
    Json(args): Json<recipes::ComposeArgs>,
) -> (StatusCode, Json<Value>) {
    recipe(app, recipes::Request::Compose(args)).await
}

/// The recipe routes, present only for the flags that are on: `[recipes]
/// enabled` adds the five recipes, `[compose] enabled` adds `lci_compose`.
/// With both off this is an empty router and the service is what it was.
pub fn recipe_routes(config: &Config) -> Router<Arc<App>> {
    let mut router = Router::new();
    if config.recipes.enabled {
        router = router
            .route("/v1/config_readers", post(config_readers))
            .route("/v1/impact_rings", post(impact_rings))
            .route("/v1/flow_path", post(flow_path))
            .route("/v1/flow_from", post(flow_from))
            .route("/v1/config_neighbours", post(config_neighbours));
    }
    if config.compose.enabled {
        router = router.route("/v1/lci_compose", post(lci_compose));
    }
    router
}
