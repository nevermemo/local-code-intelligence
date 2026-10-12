//! The bench's deterministic fake embedder (`docs/spike/multimap/bench.md`,
//! section 2.3): a hashed bag of words served as a minimal OpenAI-compatible
//! `/v1/embeddings` and `/v1/models` service on `127.0.0.1` with an ephemeral
//! port. The vector of a text is a pure function of the text, so two machines
//! produce the same vectors.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

/// Vector dimension.
pub const DIMENSION: usize = 512;
/// Model id the service lists and the scratch configuration names.
pub const MODEL: &str = "lci-bench-fake-hash-512";
/// Largest request body the service reads.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The words of a text: split at every non-alphanumeric character and at
/// camelCase boundaries, lowercased, at least two characters, not all digits.
fn words(text: &str) -> Vec<String> {
    crate::mine::split_words(text)
        .into_iter()
        .filter(|word| word.chars().count() >= 2 && !word.chars().all(|c| c.is_ascii_digit()))
        .collect()
}

/// The vector of one input text.
pub fn embed(text: &str) -> Vec<f32> {
    // Only a leading query instruction (the production template starts with
    // `Instruct:`); a document that merely contains `Query: ` is untouched.
    let text = if text.starts_with("Instruct:") {
        text.find("Query: ")
            .map_or(text, |at| &text[at + "Query: ".len()..])
    } else {
        text
    };
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for word in words(text) {
        *counts.entry(word).or_default() += 1;
    }
    let mut vector = vec![0f64; DIMENSION];
    for (word, count) in &counts {
        let hash = fnv1a(word.as_bytes());
        let weight = 1.0 + f64::from(*count).ln();
        let sign = if hash >> 63 == 0 { 1.0 } else { -1.0 };
        vector[(hash % DIMENSION as u64) as usize] += sign * weight;
    }
    let norm = vector.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm == 0.0 {
        let mut unit = vec![0f32; DIMENSION];
        unit[0] = 1.0;
        return unit;
    }
    vector.iter().map(|v| (v / norm) as f32).collect()
}

fn respond(status: &str, body: &Value) -> Vec<u8> {
    let body = body.to_string();
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn answer(method: &str, path: &str, body: &[u8]) -> Vec<u8> {
    let path = path.split('?').next().unwrap_or(path);
    match method {
        "GET" if path.ends_with("/models") => respond(
            "200 OK",
            &json!({"object": "list", "data": [{"id": MODEL, "object": "model"}]}),
        ),
        "GET" => respond("200 OK", &json!({"status": "ok"})),
        "POST" if path.ends_with("/embeddings") => {
            let Ok(request) = serde_json::from_slice::<Value>(body) else {
                return respond("400 Bad Request", &json!({"error": "invalid JSON"}));
            };
            let inputs: Option<Vec<&str>> = match &request["input"] {
                Value::String(text) => Some(vec![text.as_str()]),
                Value::Array(items) => items.iter().map(Value::as_str).collect(),
                _ => None,
            };
            let Some(inputs) = inputs else {
                return respond(
                    "400 Bad Request",
                    &json!({"error": "input must be a string or an array of strings"}),
                );
            };
            let data: Vec<Value> = inputs
                .iter()
                .enumerate()
                .map(|(index, text)| json!({"object": "embedding", "index": index, "embedding": embed(text)}))
                .collect();
            respond(
                "200 OK",
                &json!({"object": "list", "model": MODEL, "data": data}),
            )
        }
        _ => respond("404 Not Found", &json!({"error": "not found"})),
    }
}

async fn serve_connection(stream: tokio::net::TcpStream) -> Result<()> {
    let mut reader = BufReader::new(stream);
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).await? == 0 {
            return Ok(());
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let path = parts.next().unwrap_or_default().to_owned();
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).await? == 0 {
                return Ok(());
            }
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().context("bad content-length")?;
                } else if name.trim().eq_ignore_ascii_case("transfer-encoding") {
                    // Only Content-Length bodies are read; answer and close
                    // rather than desynchronise the connection.
                    let response =
                        respond("411 Length Required", &json!({"error": "length required"}));
                    reader.get_mut().write_all(&response).await?;
                    return Ok(());
                }
            }
        }
        anyhow::ensure!(length <= MAX_BODY_BYTES, "request body too large");
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body).await?;
        let response = answer(&method, &path, &body);
        reader.get_mut().write_all(&response).await?;
    }
}

/// Start the service on `127.0.0.1` with an ephemeral port, on its own
/// thread and runtime, so a caller that blocks on a child process (which
/// asks this service for vectors) cannot starve it; returns the embedding URL
/// (`http://127.0.0.1:<port>/v1`). The thread lives until the process exits.
pub fn start() -> Result<String> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").context("binding the fake embedder")?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .context("building the fake embedder runtime")?;
    std::thread::Builder::new()
        .name("bench-fake-embedder".into())
        .spawn(move || {
            runtime.block_on(async move {
                let Ok(listener) = TcpListener::from_std(listener) else {
                    return;
                };
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(async move {
                        let _ = serve_connection(stream).await;
                    });
                }
            });
        })
        .context("starting the fake embedder thread")?;
    Ok(format!("http://{address}/v1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn vectors_are_unit_length_and_deterministic() {
        let a = embed("fn poll_ready(&mut self) -> Poll<()>");
        assert_eq!(a.len(), DIMENSION);
        let norm: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert_eq!(a, embed("fn poll_ready(&mut self) -> Poll<()>"));
        assert_eq!(embed("")[0], 1.0);
    }

    #[test]
    fn the_query_instruction_is_stripped_and_words_drive_similarity() {
        let query = embed("Instruct: retrieve code\nQuery: poll ready timer");
        assert_eq!(query, embed("poll ready timer"));
        // Only a leading instruction is stripped.
        let document = "// Query: parse header\nfn poll_ready() {}";
        assert_ne!(embed(document), embed("parse header fn poll_ready() {}"));
        assert_eq!(embed(document), embed("Query parse header fn poll ready"));
        let related = embed("fn poll_ready(timer: Timer) {}");
        let unrelated = embed("def parse_grammar(table): return table");
        assert!(cosine(&query, &related) > cosine(&query, &unrelated));
    }

    #[test]
    fn a_known_word_lands_on_its_pinned_dimension() {
        // Pins the hash so a change to it shows: FNV-1a 64 of "timer".
        let hash = fnv1a(b"timer");
        assert_eq!(hash, 0x2933_8b6c_3928_6b02);
        let vector = embed("timer");
        let dimension = (hash % DIMENSION as u64) as usize;
        let sign = if hash >> 63 == 0 { 1.0 } else { -1.0 };
        assert_eq!(vector[dimension], sign);
        // Bit 63 set, bit 62 clear: a negative sign on dimension 56.
        assert_eq!(fnv1a(b"poll"), 0x8c18_cf0d_a882_8238);
        assert_eq!(embed("poll")[56], -1.0);
        // Bit 63 clear, bit 62 set: positive on dimension 210.
        assert_eq!(embed("server")[210], 1.0);
    }

    #[test]
    fn word_weights_are_pinned() {
        // `1 + ln(count)` after the f32 cast, for the counts that occur in
        // practice: a libm that differs here would change every vector.
        let expected: [f32; 8] = [
            1.0,
            1.693_147_2,
            2.098_612_3,
            2.386_294_4,
            2.609_438,
            2.791_759_5,
            2.945_910_2,
            3.079_441_5,
        ];
        for (count, want) in (1u32..=8).zip(expected) {
            assert_eq!((1.0 + f64::from(count).ln()) as f32, want, "count {count}");
        }
    }

    #[tokio::test]
    async fn the_service_rejects_non_string_inputs() {
        let url = start().unwrap();
        let status = reqwest::Client::new()
            .post(format!("{url}/embeddings"))
            .json(&json!({"input": ["ok", 3]}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status.as_u16(), 400);
    }

    #[tokio::test]
    async fn the_service_answers_models_and_embeddings_in_order() {
        let url = start().unwrap();
        let client = reqwest::Client::new();
        let models: Value = client
            .get(format!("{url}/models"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(models["data"][0]["id"], MODEL);
        for _ in 0..2 {
            // Twice on one keep-alive connection.
            let response: Value = client
                .post(format!("{url}/embeddings"))
                .json(&json!({"model": MODEL, "input": ["alpha beta", "gamma"]}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let data = response["data"].as_array().unwrap();
            assert_eq!(data.len(), 2);
            assert_eq!(data[1]["index"], 1);
            let vector: Vec<f32> = serde_json::from_value(data[1]["embedding"].clone()).unwrap();
            assert_eq!(vector, embed("gamma"));
        }
    }
}
