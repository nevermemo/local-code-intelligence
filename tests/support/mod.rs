use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use local_code_intelligence::{
    app::App,
    config::Config,
    evaluate::{
        EvalQuery, EvalWorkspace, EvaluationFile, QueryReport, ResultEvidence, aggregate,
        has_failures, load_definition, parse_workspace_mappings, query_metrics,
    },
    models::QUERY_INSTRUCTION,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct FakeModels {
    documents: AtomicUsize,
    document_requests: AtomicUsize,
    queries: AtomicUsize,
    embed_delay_milliseconds: AtomicUsize,
    rerank_fails: AtomicBool,
    embed_fails: AtomicBool,
    malformed_rerank: AtomicBool,
}

fn evaluation_query() -> EvalQuery {
    EvalQuery {
        query: "find evaluator".to_owned(),
        workspace: "self".to_owned(),
        top_k: Some(8),
        filters: None,
        expected_paths: vec!["src/evaluate.rs".to_owned()],
        preferred_paths: Vec::new(),
        disfavored_paths: Vec::new(),
        expected_roles: vec!["source".to_owned()],
        required_snippet: Some("query_metrics".to_owned()),
    }
}

fn evaluation_evidence(path: &str, role: &str, snippet: &str) -> ResultEvidence {
    ResultEvidence {
        rank: 1,
        relative_file_path: path.to_owned(),
        language: "rust".to_owned(),
        source_role: role.to_owned(),
        start_line: 1,
        end_line: 10,
        snippet: snippet.to_owned(),
        reranker_score: Some(0.9),
    }
}

async fn embed(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if state.embed_fails.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let inputs = body["input"].as_array().unwrap();
    if inputs
        .iter()
        .any(|text| !text.as_str().unwrap().starts_with(QUERY_INSTRUCTION))
    {
        state.document_requests.fetch_add(1, Ordering::SeqCst);
        let delay = state.embed_delay_milliseconds.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
    }
    let mut data = Vec::new();
    for (index, text) in inputs.iter().enumerate() {
        let text = text.as_str().unwrap();
        if text.starts_with(QUERY_INSTRUCTION) {
            state.queries.fetch_add(1, Ordering::SeqCst);
        } else {
            state.documents.fetch_add(1, Ordering::SeqCst);
            assert!(!text.starts_with("Instruct:"));
        }
        let vector = if text.contains("translator") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("buildProductionTelemetryPipeline") {
            vec![0.0, 1.0, 0.0]
        } else {
            vec![0.1, 0.1, 1.0]
        };
        data.push(json!({"index":index, "embedding":vector}));
    }
    // OpenAI-compatible services may return embeddings out of order.
    data.reverse();
    Ok(Json(json!({"data":data})))
}

async fn model_endpoint() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

async fn models() -> Json<Value> {
    Json(json!({"data":[
        {"id":"qwen3-embedding-4b"},
        {"id":"qwen3-reranker-4b"}
    ]}))
}

async fn rerank(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if state.rerank_fails.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if state.malformed_rerank.load(Ordering::SeqCst) {
        return Ok(Json(
            json!({"results":[{"index":999,"relevance_score":1.0}]}),
        ));
    }
    let query = body["query"].as_str().unwrap_or("");
    let results: Vec<_> = body["documents"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let document = d.as_str().unwrap();
            let preferred_implementation = (query == "buildProductionTelemetryPipeline"
                && document.contains("src/telemetry.ts\n")
                && document.contains("function buildProductionTelemetryPipeline"))
                || (query == "build_production_feature_pipeline"
                    && document.contains("src/feature_pipeline.py\n")
                    && document.contains("def build_production_feature_pipeline"));
            let score = if preferred_implementation {
                0.995
            } else if document.contains("translator") {
                0.99
            } else {
                0.1
            };
            json!({"index":i,"relevance_score":score})
        })
        .collect();
    Ok(Json(json!({"results":results})))
}

async fn fixture() -> (
    tempfile::TempDir,
    Config,
    Arc<FakeModels>,
    tokio::task::JoinHandle<()>,
) {
    let temp = tempfile::tempdir().unwrap();
    let state = Arc::new(FakeModels::default());
    let router = Router::new()
        .route("/v1", get(model_endpoint))
        .route("/v1/models", get(models))
        .route("/v1/embeddings", post(embed))
        .route("/rerank", get(model_endpoint).post(rerank))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = Config {
        data_dir: temp.path().join("data"),
        embedding_url: format!("http://{address}/v1"),
        reranker_url: format!("http://{address}/rerank"),
        rust_analyzer_path: "definitely-missing-rust-analyzer".into(),
        ..Config::default()
    };
    (temp, config, state, task)
}

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}
