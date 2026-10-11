#[cfg(windows)]
use crate::lsp::job;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fmt,
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

/// Most time [`JsonRpcClient::shutdown`] gives the `exit` notification's
/// write. It only fills a pipe buffer, so anything longer means the server
/// stopped reading.
const EXIT_WRITE_BOUND: Duration = Duration::from_millis(200);

/// Most time [`JsonRpcClient::shutdown`] waits to reap a child it has just
/// killed; the kill itself does not wait.
const KILL_REAP_BOUND: Duration = Duration::from_millis(500);

type LastActivity = Arc<std::sync::Mutex<Instant>>;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// A JSON-RPC error *response*: the server received the request and answered
/// that it failed, with the `code` and `message` of the protocol's error
/// object.
///
/// It is what [`JsonRpcClient::request`] returns, as the root cause of its
/// `anyhow::Error`, for a well-formed error response, and for nothing else. A
/// transport failure (the process exited, a pipe closed, the protocol reader
/// stopped, the inactivity timeout passed) carries no `RpcError`, and neither
/// does a protocol failure: an `error` member that is not an object with an
/// integer `code` and a string `message`, which a server that speaks the
/// protocol does not send. So a caller that must treat the two differently asks
/// the error: `error.downcast_ref::<RpcError>()` is `Some` for a server that is
/// alive and answering, `None` for one that is gone, silent or confused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    /// `ContentModified`: the content the request was computed on changed.
    pub const CONTENT_MODIFIED: i64 = -32801;
    /// `ServerCancelled`: the server cancelled a request it had accepted.
    pub const SERVER_CANCELLED: i64 = -32802;
    /// `RequestFailed`: a well-formed request failed.
    pub const REQUEST_FAILED: i64 = -32803;
    /// JSON-RPC's `InternalError`, which is also what rust-analyzer answers
    /// for a file it has not loaded yet.
    pub const INTERNAL_ERROR: i64 = -32603;

    /// Reads a response's `error` member. `None` unless it is an object with an
    /// integer `code` and a string `message`, as the protocol defines it:
    /// anything else is not a server's answer to guess a code for.
    fn from_response(error: &Value) -> Option<Self> {
        Some(Self {
            code: error.as_object()?.get("code")?.as_i64()?,
            message: error.get("message")?.as_str()?.to_owned(),
        })
    }
}

impl fmt::Display for RpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Configuration for spawning a JSON-RPC over stdio process.
pub struct TransportConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub timeout: Duration,
    /// Human-readable name used in error messages (e.g. "rust-analyzer").
    pub label: String,
}

impl TransportConfig {
    pub(crate) fn command(&self, environment: &[(String, String)]) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .args(&self.args)
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .current_dir(&self.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        command
    }
}

/// A JSON-RPC client over a child process's stdio.
///
/// Handles process lifecycle (spawn, kill on drop), Content-Length framing,
/// JSON-RPC response correlation by ID, serialized writes, notification
/// draining, stderr separation from stdout, and idle-bounded request
/// timeouts (see [`request`](Self::request)).
///
/// On Windows the server and everything it starts live in a Job Object that
/// the client owns (see `lsp/job.rs`): [`shutdown`](Self::shutdown) and
/// dropping the client end the whole process tree, not only the direct child.
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
    /// The server's process tree. Closing it (dropping the client) kills every
    /// process in it. `None` when the job could not be set up.
    #[cfg(windows)]
    job: Option<job::Job>,
}

impl JsonRpcClient {
    /// Spawns the process and sets up stdio pipes and background reader tasks.
    ///
    /// `ready_predicate` is called for each notification. When it returns
    /// `true`, the client transitions to the ready state.
    pub async fn spawn(
        config: &TransportConfig,
        environment: &[(String, String)],
        ready_predicate: Box<dyn Fn(&Value) -> bool + Send>,
    ) -> Result<Arc<Self>> {
        let mut command = config.command(environment);
        #[cfg(windows)]
        let job = job::prepare(&mut command);
        let mut child = command
            .spawn()
            .with_context(|| format!("start {}", config.executable))?;
        // On error the early return drops `child`, which ends it.
        #[cfg(windows)]
        let job = job::adopt(job, &child)
            .with_context(|| format!("start {}: run the suspended process", config.executable))?;
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
                let _ = sender.send(Err(anyhow::anyhow!("{label} exited")));
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
            #[cfg(windows)]
            job,
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
    ///
    /// An error *response* from the server comes back as an error whose root
    /// cause is an [`RpcError`]; every other failure (a closed pipe, an exited
    /// process, the timeout) does not carry one.
    ///
    /// Safe to drop mid-flight (a search that hits its latency budget does
    /// exactly that): the request is then abandoned cleanly, with its pending
    /// entry removed and a `$/cancelRequest` sent so the server can stop the
    /// work, and the connection stays usable for the next caller.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        let mut abandoned = AbandonedRequest {
            id,
            pending: Arc::clone(&self.pending),
            stdin: Arc::clone(&self.stdin),
            armed: true,
        };
        if let Err(error) = self
            .write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().await.remove(&id);
            abandoned.armed = false;
            // A failed write means the server closed its input: it exited.
            return Err(error.context(format!("{} exited or closed its input", self.label)));
        }

        let started = Instant::now();
        tokio::pin!(receiver);
        let outcome = loop {
            let last_activity = *self.last_activity.lock().unwrap();
            let deadline = last_activity.max(started) + self.timeout;
            let now = Instant::now();
            if now >= deadline {
                self.pending.lock().await.remove(&id);
                break Err(anyhow::anyhow!(
                    "{} request timed out after {} seconds of inactivity",
                    self.label,
                    self.timeout.as_secs()
                ));
            }
            tokio::select! {
                result = &mut receiver => {
                    break match result {
                        Ok(Ok(value)) => Ok(value),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(anyhow::anyhow!("{} response channel closed", self.label)),
                    };
                }
                _ = tokio::time::sleep(deadline - now) => {
                    // Re-check above: if the server sent anything in the
                    // meantime, last_activity moved and the deadline pushes
                    // out; if not, the next iteration's `now >= deadline`
                    // catches it immediately.
                }
            }
        };
        abandoned.armed = false;
        outcome
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

    /// Stops the server the way the protocol asks: a `shutdown` request, an
    /// `exit` notification, then a wait for the process to end. The request
    /// and the wait are each bounded by `grace`, the `exit` write by 200 ms
    /// (it is a pipe write, not a round trip), so the call returns within
    /// about `2 * grace` for any server, however unresponsive. A server that
    /// is still alive after that is killed and reaped (the kill is immediate;
    /// the reaping is bounded by 500 ms). Returns `true` when the process
    /// ended on its own, `false` when it had to be killed.
    ///
    /// On Windows the whole process tree ends with the server, whichever way
    /// it ends: processes the server started and left behind are terminated
    /// with its job.
    ///
    /// Failures of the first two steps are ignored: the server may already
    /// be gone, and the goal is the same either way. The client is unusable
    /// afterwards. The child lock is held only inside the bounded wait and
    /// kill, so [`Drop`]'s `try_lock` still works.
    pub async fn shutdown(&self, grace: Duration) -> bool {
        if tokio::time::timeout(grace, self.request("shutdown", Value::Null))
            .await
            .is_err()
        {
            tracing::debug!(label = %self.label, "language server did not answer shutdown in time");
        }
        // The write blocks only if a hung server stopped reading its input
        // and the pipe is full; do not let that hold the release.
        let _ = tokio::time::timeout(EXIT_WRITE_BOUND, self.notify("exit", Value::Null)).await;
        if let Ok(Ok(_)) = tokio::time::timeout(grace, self.wait_exit()).await {
            // The server is gone; what it started and left running goes too.
            self.terminate_tree();
            return true;
        }
        tracing::debug!(label = %self.label, "language server did not exit in time; killing it");
        // Do not leave the kill to `Drop`: another holder of this client (a
        // request that is still in flight) may keep it alive. The job goes
        // first: it reaches the processes under the direct child, which
        // `Child::kill` does not.
        self.terminate_tree();
        let _ = tokio::time::timeout(KILL_REAP_BOUND, async {
            self.child.lock().await.kill().await
        })
        .await;
        false
    }

    /// Ends every process the server started, on Windows (a no-op elsewhere,
    /// and when the server runs without a job object).
    fn terminate_tree(&self) {
        #[cfg(windows)]
        if let Some(job) = &self.job {
            job.terminate();
        }
    }

    /// Waits for the child process to end. Cancel-safe: dropping it only
    /// stops waiting, and releases the child lock.
    async fn wait_exit(&self) -> std::io::Result<std::process::ExitStatus> {
        self.child.lock().await.wait().await
    }

    /// Writes one message on a detached task. A caller that is dropped while
    /// waiting (a search past its latency budget) must not leave half a frame
    /// on the pipe, which would desynchronize the stream for every later
    /// caller; the spawned write always runs to completion.
    async fn write(&self, value: &Value) -> Result<()> {
        let stdin = Arc::clone(&self.stdin);
        let value = value.clone();
        tokio::spawn(async move { write_framed(&stdin, &value).await })
            .await
            .context("language-server write task ended unexpectedly")?
    }
}

/// Sent for a request whose caller went away before the response arrived:
/// drops the pending entry (a response that still comes is ignored) and tells
/// the server to stop working on it.
struct AbandonedRequest {
    id: u64,
    pending: Pending,
    stdin: Arc<Mutex<ChildStdin>>,
    /// Cleared once the request completed or failed on its own.
    armed: bool,
}

impl Drop for AbandonedRequest {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Dropped outside a runtime (shutdown): nothing left to clean up.
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (id, pending, stdin) = (self.id, Arc::clone(&self.pending), Arc::clone(&self.stdin));
        // The caller may be dropped while its own request is still being
        // written (that write runs on a detached task), so this cancel can
        // reach the server's stdin before the request it names. That is
        // harmless: the server ignores a cancel for an id it has not seen,
        // runs the request, and the late response finds no pending entry.
        runtime.spawn(async move {
            pending.lock().await.remove(&id);
            let cancel = json!({
                "jsonrpc": "2.0",
                "method": "$/cancelRequest",
                "params": { "id": id },
            });
            let _ = write_framed(&stdin, &cancel).await;
        });
    }
}

/// Writes one Content-Length-framed JSON-RPC message as a single buffer.
/// Shared by the client's own outgoing requests/notifications and the reader
/// task's automatic responses to server-initiated requests (both write to the
/// same stdin).
async fn write_framed(stdin: &Mutex<ChildStdin>, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(&body);
    let mut stdin = stdin.lock().await;
    stdin.write_all(&frame).await?;
    stdin.flush().await?;
    Ok(())
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        // Dropping the job after this closes it, which ends the tree too;
        // doing it here keeps the order the same as `shutdown`'s kill.
        self.terminate_tree();
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
            let _ = sender.send(response_outcome(&message));
        }
    }
}

/// What a response to one of our requests says: its result, or its error as
/// an [`RpcError`]. A response that is not an answer a caller can act on (an
/// error member that is not an error object, or an error next to a result,
/// which JSON-RPC forbids) is not an `RpcError`, so the session is treated
/// like one whose transport failed (see `RpcError`). A `null` result next to
/// an error is let through: it carries nothing that could be mistaken for an
/// answer.
fn response_outcome(message: &Value) -> Result<Value> {
    match message.get("error") {
        Some(error) => Err(match RpcError::from_response(error) {
            Some(response) if message.get("result").is_none_or(Value::is_null) => {
                anyhow::Error::new(response)
            }
            Some(_) => anyhow::anyhow!("malformed JSON-RPC response: both a result and an error"),
            None => anyhow::anyhow!("malformed JSON-RPC error response: {error}"),
        }),
        None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_with_a_result_and_an_error_is_not_an_rpc_error() {
        let error = json!({"code": -32801, "message": "content modified"});
        let both = json!({"jsonrpc": "2.0", "id": 1, "result": [1], "error": error});
        let failure = response_outcome(&both).unwrap_err();
        assert!(failure.downcast_ref::<RpcError>().is_none(), "{failure}");
        // A null result beside the error carries no answer.
        for message in [
            json!({"id": 1, "result": null, "error": error}),
            json!({"id": 1, "error": error}),
        ] {
            let failure = response_outcome(&message).unwrap_err();
            let typed = failure.downcast_ref::<RpcError>().expect("an RpcError");
            assert_eq!(typed.code, RpcError::CONTENT_MODIFIED);
        }
        assert_eq!(
            response_outcome(&json!({"id": 1, "result": {"k": 1}})).unwrap(),
            json!({"k": 1})
        );
        assert_eq!(response_outcome(&json!({"id": 1})).unwrap(), Value::Null);
    }

    #[test]
    fn an_error_object_keeps_its_code_and_message() {
        let error =
            RpcError::from_response(&json!({"code": -32801, "message": "content modified"}))
                .unwrap();
        assert_eq!(error.code, RpcError::CONTENT_MODIFIED);
        assert_eq!(error.message, "content modified");
        assert_eq!(error.to_string(), "JSON-RPC error -32801: content modified");
        // Extra members (`data`) are allowed.
        assert!(
            RpcError::from_response(&json!({"code": 1, "message": "m", "data": {"k": 1}}))
                .is_some()
        );
    }

    #[test]
    fn a_malformed_error_member_is_not_an_rpc_error() {
        for malformed in [
            json!(null),
            json!("plain text"),
            json!(7),
            json!([1, 2]),
            json!({"reason": "boom"}),
            json!({"message": "no code"}),
            json!({"code": 1}),
            json!({"code": "1", "message": "string code"}),
            json!({"code": 1.5, "message": "fractional code"}),
            json!({"code": 1, "message": 5}),
            json!({"code": null, "message": "null code"}),
        ] {
            assert!(RpcError::from_response(&malformed).is_none(), "{malformed}");
        }
    }

    #[test]
    fn only_an_error_response_carries_an_rpc_error() {
        let response = anyhow::Error::new(
            RpcError::from_response(&json!({"code": -32603, "message": "file not found"})).unwrap(),
        )
        .context("workspace/symbol");
        // Context layers added by callers do not hide the root cause.
        let found = response.downcast_ref::<RpcError>().unwrap();
        assert_eq!(found.code, RpcError::INTERNAL_ERROR);
        assert!(
            anyhow::anyhow!("rust-analyzer exited")
                .downcast_ref::<RpcError>()
                .is_none()
        );
    }
}
