#[cfg(windows)]
use crate::lsp::job;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::OsStr,
    fmt,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::{Mutex, oneshot, watch},
};

/// Most time [`JsonRpcClient::shutdown`] gives the `exit` notification's
/// write. It only fills a pipe buffer, so anything longer means the server
/// stopped reading.
const EXIT_WRITE_BOUND: Duration = Duration::from_millis(200);

/// Most time [`JsonRpcClient::shutdown`] waits to reap a child it has just
/// killed; the kill itself does not wait.
const KILL_REAP_BOUND: Duration = Duration::from_millis(500);

/// Largest message body the reader accepts. A reply to `workspace/symbol` or
/// `textDocument/references` on a very large workspace is megabytes, never
/// this much; a server that announces more is broken (or hostile) and the
/// body is not allocated: the session is treated like one whose transport
/// failed.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// The longest the deadline of one request is put after its last sign of life,
/// whatever the timeout says: a year cannot overflow the clock, and no
/// configured timeout comes near it (`Config::MAX_LSP_TIMEOUT_SECONDS`).
const LONGEST_WAIT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// Longest header line the reader accepts, terminator included. Real headers
/// are `Content-Length: N` and `Content-Type: ...`.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// Most header lines before the blank line that ends them.
const MAX_HEADER_LINES: usize = 32;

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

/// A request that ended because the server sent nothing for the whole
/// inactivity timeout (`lsp_timeout_seconds`): the root cause of the
/// `anyhow::Error` [`JsonRpcClient::request`] returns then, and of nothing
/// else. It tells a server that is slow (a start that outlasted the bound) from
/// one that is gone (exited, closed its pipe, broke the protocol), which
/// carries none: `error.chain().any(|cause| cause.is::<InactivityTimeout>())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InactivityTimeout {
    pub(crate) label: String,
    pub(crate) seconds: u64,
}

impl fmt::Display for InactivityTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} request timed out after {} seconds of inactivity",
            self.label, self.seconds
        )
    }
}

impl std::error::Error for InactivityTimeout {}

/// Configuration for spawning a JSON-RPC over stdio process.
pub struct TransportConfig {
    pub executable: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub timeout: Duration,
    /// Human-readable name used in error messages (e.g. "rust-analyzer").
    pub label: String,
}

/// The program to start for a configured server `executable` (a bare name or a
/// path): on Windows, a name or path without an extension is looked up as
/// `<it>.exe` and then `<it>.cmd` (a bare name directory by directory along
/// `PATH`), so that `typescript-language-server`, or an extensionless path to an
/// npm install, starts the `.cmd` shim the way a shell would. `PATHEXT` is not
/// consulted: a `.bat` is not found by name, only by a path that names it.
/// A relative path with a directory part comes back absolute, because the
/// server starts in the workspace directory, not in ours. `CreateProcess` itself appends only `.exe`, and a path that
/// names a file (`x.cmd`, `x.bat`, `x.exe`) needs no help: the standard
/// library runs a `.cmd` or `.bat` through `cmd.exe`. Elsewhere, and when
/// nothing is found, the configured value is returned unchanged so the spawn
/// error names what the user wrote.
pub(crate) fn resolve_executable(executable: &str) -> std::ffi::OsString {
    resolve_executable_in(
        Path::new(executable),
        std::env::var_os("PATH").as_deref(),
        cfg!(windows),
    )
    .map_or_else(|| executable.into(), PathBuf::into_os_string)
}

/// [`resolve_executable`] over an explicit `PATH` value and platform, so the
/// lookup is testable anywhere. `None` when there is nothing to change.
fn resolve_executable_in(
    executable: &Path,
    path: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    if !windows || executable.extension().is_some() {
        return None;
    }
    let bare = executable.components().count() == 1
        && matches!(executable.components().next(), Some(Component::Normal(_)));
    if bare {
        // Only the Win32 names: npm installs an extensionless POSIX script
        // beside the shim, and it must not hide a shim further along `PATH`.
        return crate::lexical::find_windows_program_on_path_in(executable.to_str()?, path);
    }
    let found = ["exe", "cmd"]
        .into_iter()
        .map(|extension| executable.with_extension(extension))
        .find(|candidate| candidate.is_file())?;
    Some(std::path::absolute(&found).unwrap_or(found))
}

impl TransportConfig {
    pub(crate) fn command(&self, environment: &[(String, String)]) -> Command {
        let mut command = Command::new(resolve_executable(&self.executable));
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

/// What the readiness and busy predicates have reported so far, see
/// [`JsonRpcClient::state`]. It is a report: nothing in it changes how a
/// request is sent, repeated or answered.
#[derive(Debug, Clone, Copy)]
pub struct ServerState {
    /// The last matching status notification was a busy one (the server has
    /// started working again after it was ready). `false` until one matches.
    pub busy: bool,
    /// How many notifications have matched the readiness or the busy
    /// predicate, each one counted (a repeated "ready" counts too).
    pub changes: u64,
    /// When the last of those arrived; the spawn time while `changes` is 0.
    pub changed_at: Instant,
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
/// [`wait_ready`](Self::wait_ready) are notified. A second predicate, the busy
/// one, marks the server as working again; it only feeds [`state`](Self::state)
/// and never clears the ready latch.
pub struct JsonRpcClient {
    writer: Arc<Writer>,
    child: Mutex<Child>,
    pending: Pending,
    next_id: AtomicU64,
    timeout: Duration,
    label: String,
    ready: Arc<tokio::sync::Notify>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
    /// The readiness/busy report, written by the reader task.
    state: watch::Receiver<ServerState>,
    /// Updated by the reader task on every parsed message of any kind
    /// (response, notification, or server-initiated request) — not just
    /// ones addressed to a particular pending request. `request` bounds its
    /// wait by *inactivity* against this, not by total call duration: see
    /// `request`'s doc comment for why.
    last_activity: LastActivity,
    /// How many `$/progress` notifications the server has sent. A server that
    /// reports progress is doing work, so an answer made of transient errors
    /// is "still loading", not "wedged" (see `Session::note_answer` in
    /// `lsp.rs`). Only that one method counts: any other message would make a
    /// server that chats forever look busy.
    progress: Arc<AtomicU64>,
    /// Why the reader task stopped (end of output, or a protocol failure such
    /// as a frame over [`MAX_FRAME_BYTES`]); `None` while it runs. It is set
    /// before the pending requests are failed, so a request that arrives later
    /// fails at once instead of waiting for a timeout nothing can end.
    reader_stopped: Arc<std::sync::Mutex<Option<String>>>,
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
        Self::spawn_with_busy(config, environment, ready_predicate, Box::new(|_| false)).await
    }

    /// [`spawn`](Self::spawn) with a busy predicate, evaluated against each
    /// notification the ready predicate did not match. A match sets
    /// [`ServerState::busy`]; a ready match clears it. Neither touches
    /// [`is_ready`](Self::is_ready).
    pub async fn spawn_with_busy(
        config: &TransportConfig,
        environment: &[(String, String)],
        ready_predicate: Box<dyn Fn(&Value) -> bool + Send>,
        busy_predicate: Box<dyn Fn(&Value) -> bool + Send>,
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
        let writer = Arc::new(Writer {
            stdin: Mutex::new(stdin),
            bound: config.timeout,
            broken: AtomicBool::new(false),
            label: config.label.clone(),
        });
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let ready = Arc::new(tokio::sync::Notify::new());
        let is_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_activity: LastActivity = Arc::new(std::sync::Mutex::new(Instant::now()));
        let progress = Arc::new(AtomicU64::new(0));
        let reader_writer = Arc::clone(&writer);
        let reader_pending = Arc::clone(&pending);
        let reader_ready = Arc::clone(&ready);
        let reader_is_ready = Arc::clone(&is_ready);
        let reader_last_activity = Arc::clone(&last_activity);
        let reader_progress = Arc::clone(&progress);
        let (state_sender, state) = watch::channel(ServerState {
            busy: false,
            changes: 0,
            changed_at: Instant::now(),
        });
        let reader_stopped: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
        let stopped = Arc::clone(&reader_stopped);
        let label = config.label.clone();
        tokio::spawn(async move {
            let reason = match read_messages(
                stdout,
                reader_writer,
                reader_pending.clone(),
                reader_ready,
                reader_is_ready,
                reader_last_activity,
                reader_progress,
                ready_predicate,
                busy_predicate,
                state_sender,
            )
            .await
            {
                Ok(()) => format!("{label} exited"),
                Err(error) => {
                    tracing::warn!(error = %error, "JSON-RPC protocol reader stopped");
                    format!("{label} stopped after a protocol failure: {error:#}")
                }
            };
            // Recorded before the pending requests are failed: a request that
            // registers after the drain below sees it (see `request`).
            *stopped.lock().unwrap() = Some(reason.clone());
            let mut pending = reader_pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(anyhow::anyhow!("{reason}")));
            }
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(message = %line, "lsp-stderr");
            }
        });
        Ok(Arc::new(Self {
            writer,
            child: Mutex::new(child),
            pending,
            next_id: AtomicU64::new(1),
            timeout: config.timeout,
            label: config.label.clone(),
            ready,
            is_ready,
            state,
            last_activity,
            progress,
            reader_stopped,
            #[cfg(windows)]
            job,
        }))
    }

    /// How many `$/progress` notifications the server has sent so far. Only
    /// the change between two readings means anything.
    pub fn progress_count(&self) -> u64 {
        self.progress.load(Ordering::Relaxed)
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
            writer: Arc::clone(&self.writer),
            armed: true,
        };
        // After registering: a reader that stopped before this point has
        // already drained the pending requests, so only this check fails us.
        let stopped = self.reader_stopped.lock().unwrap().clone();
        if let Some(reason) = stopped {
            self.pending.lock().await.remove(&id);
            abandoned.armed = false;
            bail!("{reason}");
        }
        if let Err(error) = self
            .write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await
        {
            self.pending.lock().await.remove(&id);
            abandoned.armed = false;
            // A failed write means the server closed its input (it exited) or
            // stopped reading it (see `Writer::write_framed`).
            return Err(error.context(format!(
                "{} could not be written to (it exited, closed its input or stopped reading it)",
                self.label
            )));
        }

        let started = Instant::now();
        tokio::pin!(receiver);
        let outcome = loop {
            let last_activity = *self.last_activity.lock().unwrap();
            // At most `LONGEST_WAIT` on: a timeout too long for the clock
            // (`Duration::MAX`) must not make the sum panic.
            let deadline = last_activity.max(started) + self.timeout.min(LONGEST_WAIT);
            let now = Instant::now();
            if now >= deadline {
                self.pending.lock().await.remove(&id);
                break Err(anyhow::Error::new(InactivityTimeout {
                    label: self.label.clone(),
                    seconds: self.timeout.as_secs(),
                }));
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
    /// A latch: it never clears, whatever the busy predicate reports.
    pub fn is_ready(&self) -> bool {
        self.is_ready.load(Ordering::Acquire)
    }

    /// What the ready and busy predicates have reported so far.
    pub fn state(&self) -> ServerState {
        *self.state.borrow()
    }

    /// Whether the last matching status notification was a busy one.
    pub fn is_busy(&self) -> bool {
        self.state.borrow().busy
    }

    /// Waits until [`ServerState::changes`] is at least `at_least`, bounded by
    /// `timeout`; `false` when it was not reached (or the reader stopped
    /// first). It waits on the server's own notifications, never on a timer.
    pub async fn wait_for_changes(&self, at_least: u64, timeout: Duration) -> bool {
        let mut state = self.state.clone();
        matches!(
            tokio::time::timeout(timeout, state.wait_for(|state| state.changes >= at_least)).await,
            Ok(Ok(_))
        )
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
        let writer = Arc::clone(&self.writer);
        let value = value.clone();
        tokio::spawn(async move { writer.write_framed(&value).await })
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
    writer: Arc<Writer>,
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
        let (id, pending, writer) = (self.id, Arc::clone(&self.pending), Arc::clone(&self.writer));
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
            let _ = writer.write_framed(&cancel).await;
        });
    }
}

/// The server's stdin, shared by the client's own outgoing requests and
/// notifications and by the reader task's automatic responses to
/// server-initiated requests.
struct Writer {
    stdin: Mutex<ChildStdin>,
    /// Longest one message may take to write, waiting for the lock included.
    /// A server that stops reading fills the pipe, and without a bound the
    /// write (and every write queued behind it on the lock) would never end.
    bound: Duration,
    /// Set when a write ran out of time: its message may be half written, so
    /// nothing is written again and the session has to be replaced.
    broken: AtomicBool,
    label: String,
}

impl Writer {
    fn ensure_not_broken(&self) -> Result<()> {
        ensure!(
            !self.broken.load(Ordering::Acquire),
            "{} stopped reading its input: an earlier write did not finish",
            self.label
        );
        Ok(())
    }

    /// Writes one Content-Length-framed JSON-RPC message as a single buffer.
    /// Fails with a timeout error when the server does not take it within
    /// `bound`, and from then on fails at once.
    async fn write_framed(&self, value: &Value) -> Result<()> {
        self.ensure_not_broken()?;
        let body = serde_json::to_vec(value)?;
        let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        frame.extend_from_slice(&body);
        let write = async {
            let mut stdin = self.stdin.lock().await;
            // The check above came before the wait for the lock, and the write
            // ahead of this one may have run out of time meanwhile: its message
            // is half written, so nothing goes after it.
            self.ensure_not_broken()?;
            stdin.write_all(&frame).await?;
            stdin.flush().await?;
            Ok::<(), anyhow::Error>(())
        };
        match tokio::time::timeout(self.bound, write).await {
            Ok(written) => written,
            Err(_) => {
                self.broken.store(true, Ordering::Release);
                bail!(
                    "{} did not take a message within {} seconds",
                    self.label,
                    self.bound.as_secs()
                )
            }
        }
    }
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

// One handle for each thing the reader signals; a struct would only rename them.
#[allow(clippy::too_many_arguments)]
async fn read_messages(
    stdout: tokio::process::ChildStdout,
    writer: Arc<Writer>,
    pending: Pending,
    ready: Arc<tokio::sync::Notify>,
    is_ready: Arc<std::sync::atomic::AtomicBool>,
    last_activity: LastActivity,
    progress: Arc<AtomicU64>,
    ready_predicate: Box<dyn Fn(&Value) -> bool + Send>,
    busy_predicate: Box<dyn Fn(&Value) -> bool + Send>,
    state: watch::Sender<ServerState>,
) -> Result<()> {
    let mut reader = BufReader::new(stdout);
    loop {
        let Some(body) = read_frame(&mut reader, MAX_FRAME_BYTES).await? else {
            return Ok(());
        };
        let message: Value = serde_json::from_slice(&body)?;
        // Any successfully parsed message -- response, notification, or a
        // server-initiated request -- is evidence the server is alive and
        // doing something, which is what `request`'s idle timeout keys off.
        *last_activity.lock().unwrap() = Instant::now();
        // JSON-RPC allows a number or a string as an id. A response to one of
        // our requests always has a number (ours); a server's own request may
        // have either, and its id is echoed as it came.
        let request_id = message
            .get("id")
            .filter(|id| id.is_number() || id.is_string());
        let id = request_id.and_then(Value::as_u64);
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
        if let (Some(request_id), Some(method)) = (request_id, method) {
            let result = match method {
                "workspace/configuration" => {
                    let count = message["params"]["items"].as_array().map_or(1, Vec::len);
                    Value::Array(vec![Value::Null; count])
                }
                _ => Value::Null,
            };
            let response = json!({"jsonrpc": "2.0", "id": request_id, "result": result});
            if let Err(error) = writer.write_framed(&response).await {
                tracing::warn!(error = %error, %method, "failed to answer server-initiated request");
            }
            continue;
        }
        if request_id.is_none() && method == Some("$/progress") {
            progress.fetch_add(1, Ordering::Relaxed);
        }
        // Notifications (no id): evaluate readiness predicate.
        if request_id.is_none() && ready_predicate(&message) {
            is_ready.store(true, Ordering::Release);
            state.send_modify(|state| {
                state.busy = false;
                state.changes += 1;
                state.changed_at = Instant::now();
            });
            ready.notify_waiters();
            ready.notify_one();
        } else if request_id.is_none() && busy_predicate(&message) {
            // A report only: the ready latch above stays as it is.
            state.send_modify(|state| {
                state.busy = true;
                state.changes += 1;
                state.changed_at = Instant::now();
            });
        }
        let Some(id) = id else {
            continue;
        };
        if let Some(sender) = pending.lock().await.remove(&id) {
            let _ = sender.send(response_outcome(&message));
        }
    }
}

/// Reads one Content-Length-framed message body. `Ok(None)` is the end of
/// the server's output. Every length is checked before anything is allocated
/// or read for it: a header line over [`MAX_HEADER_LINE_BYTES`], more than
/// [`MAX_HEADER_LINES`] header lines, or a body over `max_body` is an error,
/// so a broken or hostile server cannot make the client allocate what it
/// announces or buffer an unterminated line without end.
async fn read_frame<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    max_body: usize,
) -> Result<Option<Vec<u8>>> {
    let mut content_length = None;
    let mut line = Vec::new();
    for _ in 0..MAX_HEADER_LINES {
        line.clear();
        let read = (&mut *reader)
            .take(MAX_HEADER_LINE_BYTES as u64 + 1)
            .read_until(b'\n', &mut line)
            .await?;
        if read == 0 {
            return Ok(None);
        }
        ensure!(
            line.len() <= MAX_HEADER_LINE_BYTES,
            "LSP header line longer than {MAX_HEADER_LINE_BYTES} bytes"
        );
        if line == b"\r\n" || line == b"\n" {
            let length = content_length.context("LSP message missing Content-Length")?;
            ensure!(
                length <= max_body,
                "LSP message of {length} bytes is over the limit of {max_body} bytes"
            );
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await?;
            return Ok(Some(body));
        }
        let header = std::str::from_utf8(&line).context("LSP header is not UTF-8")?;
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            // Two lengths cannot both be right, and taking the last would read
            // part of a body as the next message.
            ensure!(
                content_length.is_none(),
                "LSP message with more than one Content-Length"
            );
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    bail!("LSP message with more than {MAX_HEADER_LINES} header lines")
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
    fn rpc_errors_are_read_from_error_responses_only() {
        // a response with a result and an error is not an RPC error
        {
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

        // an error object keeps its code and message
        {
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

        // a malformed error member is not an RPC error
        {
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

        // only an error response carries an RPC error
        {
            let response = anyhow::Error::new(
                RpcError::from_response(&json!({"code": -32603, "message": "file not found"}))
                    .unwrap(),
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

    async fn frame_of(bytes: &[u8], max_body: usize) -> (Result<Option<Vec<u8>>>, Vec<u8>) {
        let mut reader = BufReader::new(bytes);
        let outcome = read_frame(&mut reader, max_body).await;
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).await.unwrap();
        (outcome, rest)
    }

    #[tokio::test]
    async fn frame_reading() {
        // a frame is read whole and the next one is left alone
        {
            let bytes = b"Content-Length: 5\r\nContent-Type: application/vscode-jsonrpc\r\n\r\nhelloContent-Length: 1\r\n\r\nx";
            let (first, rest) = frame_of(bytes, MAX_FRAME_BYTES).await;
            assert_eq!(first.unwrap().unwrap(), b"hello");
            assert_eq!(rest, b"Content-Length: 1\r\n\r\nx");
            // A bare line feed ends the headers too, and the name's case is free.
            let (lenient, _) = frame_of(b"content-length: 2\n\nab", MAX_FRAME_BYTES).await;
            assert_eq!(lenient.unwrap().unwrap(), b"ab");
        }

        // the end of the output is not an error before a frame
        {
            for bytes in [&b""[..], b"Content-Length: 5\r\n"] {
                let (outcome, _) = frame_of(bytes, MAX_FRAME_BYTES).await;
                assert!(outcome.unwrap().is_none());
            }
            let (cut, _) = frame_of(b"Content-Length: 5\r\n\r\nhe", MAX_FRAME_BYTES).await;
            assert!(cut.is_err(), "a body that ends early is a failure");
        }

        // a frame over the limit is refused before its body is read
        // The announced length is checked before the body is allocated or read:
        // the bytes after the headers are still unread when the error comes.
        {
            let (at_limit, _) = frame_of(b"Content-Length: 16\r\n\r\n0123456789abcdef", 16).await;
            assert_eq!(at_limit.unwrap().unwrap().len(), 16);
            for announced in ["17", "4294967296", "18446744073709551615"] {
                let message = format!("Content-Length: {announced}\r\n\r\nBODY");
                let (outcome, rest) = frame_of(message.as_bytes(), 16).await;
                let error = outcome.unwrap_err();
                assert!(
                    format!("{error:#}").contains("over the limit"),
                    "{announced}: {error:#}"
                );
                assert_eq!(rest, b"BODY", "{announced}: the body must not be consumed");
            }
        }

        // a header line over the bound is refused without buffering it
        {
            let longest = format!("X-Pad: {}\r\n", "a".repeat(MAX_HEADER_LINE_BYTES - 9));
            assert_eq!(longest.len(), MAX_HEADER_LINE_BYTES);
            let framed = format!("{longest}Content-Length: 2\r\n\r\nok");
            let (accepted, _) = frame_of(framed.as_bytes(), MAX_FRAME_BYTES).await;
            assert_eq!(accepted.unwrap().unwrap(), b"ok");

            let endless = vec![b'a'; MAX_HEADER_LINE_BYTES * 4];
            let (outcome, rest) = frame_of(&endless, MAX_FRAME_BYTES).await;
            assert!(format!("{:#}", outcome.unwrap_err()).contains("header line longer"));
            assert!(
                rest.len() >= MAX_HEADER_LINE_BYTES * 3 - 1,
                "only about one bound of the line may be read, {} bytes left",
                rest.len()
            );
        }

        // too many header lines, a missing length and a bad length are refused
        {
            let mut many = "X: 1\r\n".repeat(MAX_HEADER_LINES);
            many.push_str("Content-Length: 2\r\n\r\nok");
            let (outcome, _) = frame_of(many.as_bytes(), MAX_FRAME_BYTES).await;
            assert!(format!("{:#}", outcome.unwrap_err()).contains("header lines"));

            let (missing, _) = frame_of(b"X: 1\r\n\r\nbody", MAX_FRAME_BYTES).await;
            assert!(format!("{:#}", missing.unwrap_err()).contains("missing Content-Length"));

            let (bad, _) = frame_of(b"Content-Length: lots\r\n\r\n", MAX_FRAME_BYTES).await;
            assert!(bad.is_err());
            let (not_utf8, _) = frame_of(b"X: \xff\r\n\r\n", MAX_FRAME_BYTES).await;
            assert!(format!("{:#}", not_utf8.unwrap_err()).contains("UTF-8"));
        }

        // a second Content-Length is refused before the body is read
        // A second Content-Length is refused, whatever it says, and before the
        // body: with the last one winning, `100` then `1` would read one byte of
        // the body and take the rest for the next message.
        {
            for headers in [
                "Content-Length: 100\r\nContent-Length: 1\r\n",
                "Content-Length: 1\r\nX: y\r\ncontent-length: 1\r\n",
            ] {
                let message = format!("{headers}\r\nBODY");
                let (outcome, rest) = frame_of(message.as_bytes(), MAX_FRAME_BYTES).await;
                let error = outcome.unwrap_err();
                assert!(
                    format!("{error:#}").contains("more than one Content-Length"),
                    "{headers:?}: {error:#}"
                );
                // Only the header lines were read: the blank line and the body are left.
                assert_eq!(
                    rest, b"\r\nBODY",
                    "{headers:?}: the body must not be consumed"
                );
            }
        }
    }

    #[test]
    fn a_windows_server_name_without_an_extension_finds_its_cmd_shim() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("typescript-language-server.cmd");
        // npm installs the extensionless POSIX script beside the shim.
        std::fs::write(dir.path().join("typescript-language-server"), b"").unwrap();
        std::fs::write(&shim, b"").unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();

        // a bare name is found on PATH, and the shim beats the POSIX script
        assert_eq!(
            resolve_executable_in(
                Path::new("typescript-language-server"),
                Some(path.as_os_str()),
                true
            ),
            Some(shim.clone())
        );
        // an extensionless path gets the shim beside it
        assert_eq!(
            resolve_executable_in(&dir.path().join("typescript-language-server"), None, true),
            Some(shim.clone())
        );
        // a name with an extension, a missing tool, and other platforms are left alone
        assert_eq!(
            resolve_executable_in(&shim, Some(path.as_os_str()), true),
            None
        );
        assert_eq!(
            resolve_executable_in(Path::new("absent"), Some(path.as_os_str()), true),
            None
        );
        assert_eq!(
            resolve_executable_in(
                Path::new("typescript-language-server"),
                Some(path.as_os_str()),
                false
            ),
            None
        );
    }

    #[test]
    fn a_posix_script_early_on_path_does_not_hide_a_later_cmd_shim() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("pyright-langserver"), b"").unwrap();
        let shim = second.path().join("pyright-langserver.cmd");
        std::fs::write(&shim, b"").unwrap();
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(
            resolve_executable_in(
                Path::new("pyright-langserver"),
                Some(path.as_os_str()),
                true
            ),
            Some(shim)
        );
        // `PATHEXT` is not consulted: a `.bat` is not found by name.
        std::fs::write(second.path().join("only-bat.bat"), b"").unwrap();
        assert_eq!(
            resolve_executable_in(Path::new("only-bat"), Some(path.as_os_str()), true),
            None
        );
    }

    #[test]
    fn a_relative_path_resolved_to_a_shim_comes_back_absolute() {
        // The server starts in the workspace directory, so a path relative to
        // our working directory would name a different file there.
        let cwd = std::env::current_dir().unwrap();
        let dir = tempfile::Builder::new()
            .prefix("shimdir")
            .tempdir_in(&cwd)
            .unwrap();
        std::fs::write(dir.path().join("tool.cmd"), b"").unwrap();
        let relative = Path::new(dir.path().file_name().unwrap()).join("tool");
        let found = resolve_executable_in(&relative, None, true).unwrap();
        assert!(found.is_absolute(), "{found:?}");
        assert!(found.is_file(), "{found:?}");
        assert!(found.ends_with("tool.cmd"), "{found:?}");
    }

    /// End to end on Windows: the spawned program is the `.cmd` shim, which
    /// the standard library starts through `cmd.exe`, and its output arrives.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_cmd_shim_configured_without_its_extension_starts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("shim.cmd"), "@echo shim-ran %1\r\n").unwrap();
        let config = TransportConfig {
            executable: dir.path().join("shim").to_string_lossy().into_owned(),
            args: vec!["arg".into()],
            working_dir: dir.path().to_path_buf(),
            timeout: Duration::from_secs(30),
            label: "shim".into(),
        };
        let output = config.command(&[]).output().await.unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("shim-ran arg"),
            "{output:?}"
        );
    }
}
