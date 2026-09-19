use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot},
};

type LastActivity = Arc<std::sync::Mutex<Instant>>;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Configuration for spawning a JSON-RPC over stdio process.
pub struct TransportConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub timeout: Duration,
    /// Human-readable name used in error messages (e.g. "rust-analyzer").
    pub label: String,
}

/// A JSON-RPC client over a child process's stdio.
///
/// Handles process lifecycle (spawn, kill on drop), Content-Length framing,
/// JSON-RPC response correlation by ID, serialized writes, notification
/// draining, stderr separation from stdout, and idle-bounded request
/// timeouts (see [`request`](Self::request)).
///
/// The caller provides a readiness predicate at spawn time. The predicate is
/// evaluated against each JSON-RPC notification (a message without an `id`
/// field). When it returns `true`, the client is marked ready and waiters on
/// [`wait_ready`](Self::wait_ready) are notified.
pub struct JsonRpcClient {
    stdin: Arc<Mutex<ChildStdin>>,
    child: Mutex<Child>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    label: String,
    ready: Arc<tokio::sync::Notify>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
    /// Updated by the reader task on every parsed message of any kind
    /// (response, notification, or server-initiated request) — not just
    /// ones addressed to a particular pending request. `request` bounds its
    /// wait by *inactivity* against this, not by total call duration: see
    /// `request`'s doc comment for why.
    last_activity: LastActivity,
}

impl JsonRpcClient {
    /// Spawns the process and sets up stdio pipes and background reader tasks.
    ///
    /// `ready_predicate` is called for each notification. When it returns
    /// `true`, the client transitions to the ready state.
    pub async fn spawn(
        config: &TransportConfig,
        ready_predicate: Box<dyn Fn(&Value) -> bool + Send>,
    ) -> Result<Arc<Self>> {
        let mut child = Command::new(&config.executable)
            .args(&config.args)
            .current_dir(&config.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("start {}", config.executable))?;
        let stdin = child
            .stdin
            .take()
            .with_context(|| format!("{} stdin unavailable", config.label))?;
        let stdout = child
            .stdout
            .take()
            .with_context(|| format!("{} stdout unavailable", config.label))?;
        let stderr = child
            .stderr
            .take()
            .with_context(|| format!("{} stderr unavailable", config.label))?;
        let stdin = Arc::new(Mutex::new(stdin));
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new(tokio::sync::Notify::new());
        let is_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_activity: LastActivity = Arc::new(std::sync::Mutex::new(Instant::now()));
        let reader_stdin = Arc::clone(&stdin);
        let reader_pending = Arc::clone(&pending);
        let reader_ready = Arc::clone(&ready);
        let reader_is_ready = Arc::clone(&is_ready);
        let reader_last_activity = Arc::clone(&last_activity);
        let label = config.label.clone();
        tokio::spawn(async move {
            if let Err(error) = read_messages(
                stdout,
                reader_stdin,
                reader_pending.clone(),
                reader_ready,
                reader_is_ready,
                reader_last_activity,
                ready_predicate,
            )
            .await
            {
                tracing::warn!(error = %error, "JSON-RPC protocol reader stopped");
            }
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(format!("{label} exited")));
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(message = %line, "lsp-stderr");
            }
        });
        Ok(Arc::new(Self {
            stdin,
            child: Mutex::new(child),
            pending,
            next_id: AtomicU64::new(1),
            timeout: config.timeout,
            label: config.label.clone(),
            ready,
            is_ready,
            last_activity,
        }))
    }

    /// Sends a JSON-RPC notification (no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    /// Sends a JSON-RPC request and waits for the correlated response.
    ///
    /// Bounded by *inactivity*, not total call duration: the deadline is
    /// `timeout` after the later of (this call starting, the most recent
    /// message of any kind the reader task observed from the server —
    /// tracked in `last_activity`, updated for every response, notification,
    /// or server-initiated request, not just ones matching this call's own
    /// id). A single request that legitimately takes long — rust-analyzer
    /// answering `workspace/symbol` while still cold-indexing a large
    /// project, for instance — keeps extending its own deadline as long as
    /// the server is visibly still alive and doing *something*, the same way
    /// a slow-but-progressing download shouldn't be treated identically to a
    /// stalled one. A server that has gone silent entirely (hung, deadlocked,
    /// or genuinely dead without the process exiting) still times out after
    /// exactly `timeout` of true silence, same as before. On timeout the
    /// pending entry is removed and an error is returned.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self
            .write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().await.remove(&id);
            return Err(error);
        }

        let started = Instant::now();
        tokio::pin!(receiver);
        loop {
            let last_activity = *self.last_activity.lock().unwrap();
            let deadline = last_activity.max(started) + self.timeout;
            let now = Instant::now();
            if now >= deadline {
                self.pending.lock().await.remove(&id);
                bail!(
                    "{} request timed out after {} seconds of inactivity",
                    self.label,
                    self.timeout.as_secs()
                );
            }
            tokio::select! {
                result = &mut receiver => {
                    return match result {
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(message)) => bail!(message),
                        Err(_) => bail!("{} response channel closed", self.label),
                    };
                }
                _ = tokio::time::sleep(deadline - now) => {
                    // Re-check above: if the server sent anything in the
                    // meantime, last_activity moved and the deadline pushes
                    // out; if not, the next iteration's `now >= deadline`
                    // catches it immediately.
                }
            }
        }
    }

    /// Returns `true` if the readiness predicate has matched a notification.
    pub fn is_ready(&self) -> bool {
        self.is_ready.load(Ordering::Acquire)
    }

    /// Waits for the readiness predicate to match, bounded by `timeout`.
    /// Returns `true` if ready within the timeout, `false` otherwise.
    pub async fn wait_ready(&self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, self.ready.notified())
            .await
            .is_ok()
    }

    /// The configured request timeout.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    async fn write(&self, value: &Value) -> Result<()> {
        write_framed(&self.stdin, value).await
    }
}

/// Writes one Content-Length-framed JSON-RPC message. Shared by the client's
/// own outgoing requests/notifications and the reader task's automatic
/// responses to server-initiated requests (both write to the same stdin).
async fn write_framed(stdin: &Mutex<ChildStdin>, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
        }
    }
}

async fn read_messages(
    stdout: tokio::process::ChildStdout,
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    ready: Arc<tokio::sync::Notify>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
    last_activity: LastActivity,
    ready_predicate: Box<dyn Fn(&Value) -> bool + Send>,
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
        // Any successfully parsed message -- response, notification, or a
        // server-initiated request -- is evidence the server is alive and
        // doing something, which is what `request`'s idle timeout keys off.
        *last_activity.lock().unwrap() = Instant::now();
        let id = message.get("id").and_then(Value::as_u64);
        let method = message.get("method").and_then(Value::as_str);
        // A message with both an id and a method is a request FROM the
        // server (e.g. pyright's `workspace/configuration`, or
        // `client/registerCapability`), not a response to one of ours or a
        // plain notification. A well-behaved client must answer these —
        // some servers (pyright confirmed) block subsequent work while
        // waiting for a reply that never comes otherwise. Answer generically
        // with the conservative "no special configuration/capability"
        // shape each known method expects; anything unrecognized gets a
        // bare null result, which every server observed so far accepts.
        if let (Some(id), Some(method)) = (id, method) {
            let result = match method {
                "workspace/configuration" => {
                    let count = message["params"]["items"].as_array().map_or(1, Vec::len);
                    Value::Array(vec![Value::Null; count])
                }
                _ => Value::Null,
            };
            let response = json!({"jsonrpc": "2.0", "id": id, "result": result});
            if let Err(error) = write_framed(&stdin, &response).await {
                tracing::warn!(error = %error, %method, "failed to answer server-initiated request");
            }
            continue;
        }
        // Notifications (no id): evaluate readiness predicate.
        if id.is_none() && ready_predicate(&message) {
            is_ready.store(true, Ordering::Release);
            ready.notify_waiters();
            ready.notify_one();
        }
        let Some(id) = id else {
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
