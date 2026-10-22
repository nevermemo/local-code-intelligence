//! Waiting for a child process under a limit, on an event.
//!
//! A waiter thread owns the child and blocks in `wait()`; the caller blocks
//! on a channel with the limit as its timeout, so the wait ends when the child
//! exits (no polling loop) or when the limit passes, whichever is first. Past
//! the limit the caller kills the child by its pid and the waiter reaps it.
//!
//! The waiter keeps the `Child` alive after `wait()` until the caller is done
//! with the pid (the caller holds a release channel across the kill step), so
//! on Windows the open process handle keeps the pid from being reused while
//! `taskkill` resolves it. On Unix `wait()` itself reaps, so there the pid is
//! reserved only until the child exits; the window is the few milliseconds
//! between the caller's last check and the `kill` command resolving the pid.

use std::{
    io,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread::JoinHandle,
    time::Duration,
};

/// How long a killed child is given to be reaped before the call returns
/// without its status; a grandchild or a stuck process may outlive the kill.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// What a timed-out child's kill reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillScope {
    /// The child only.
    Process,
    /// The child and its descendants.
    Tree,
}

/// How [`wait_limited`] ended.
#[derive(Debug)]
pub struct Waited {
    /// The child's process id (for a log line when it was not reaped).
    pub pid: u32,
    /// The exit status; `None` when the limit passed and the child was killed.
    pub status: Option<ExitStatus>,
    /// The limit passed before the child exited.
    pub timed_out: bool,
    /// The child was reaped: always true after an exit, and after a kill when
    /// the waiter saw the killed child go within the grace.
    pub reaped: bool,
}

/// Something the waiter thread can wait on (a [`Child`]; a test wraps one to
/// see when it is dropped).
trait Waitable: Send + 'static {
    fn wait_exit(&mut self) -> io::Result<ExitStatus>;
}

impl Waitable for Child {
    fn wait_exit(&mut self) -> io::Result<ExitStatus> {
        self.wait()
    }
}

/// Starts the waiter: it waits for `child`, sends the result, then keeps
/// `child` (its handle) until `release` is signalled or disconnected, so the
/// pid stays reserved while the caller kills by it.
fn spawn_waiter<C: Waitable>(
    child: C,
    sender: mpsc::Sender<io::Result<ExitStatus>>,
    release: mpsc::Receiver<()>,
) -> io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("lci-wait-child".into())
        .spawn(move || {
            let mut child = child;
            let _ = sender.send(child.wait_exit());
            let _ = release.recv();
            drop(child);
        })
}

/// Waits for `child` for at most `limit`; kills it (by `scope`) when it
/// outlives the limit. A child that exited at the same moment the limit
/// passed counts as exited.
pub fn wait_limited(child: Child, limit: Duration, scope: KillScope) -> io::Result<Waited> {
    let pid = child.id();
    let (sender, receiver) = mpsc::channel();
    // Held until this function returns, after the kill step: the waiter drops
    // the child only then.
    let (_hold, release) = mpsc::channel::<()>();
    if let Err(error) = spawn_waiter(child, sender, release) {
        // The closure and the child in it are gone; the process still runs.
        kill_pid(pid, scope);
        return Err(error);
    }
    let exited = |result: io::Result<ExitStatus>| {
        result.map(|status| Waited {
            pid,
            status: Some(status),
            timed_out: false,
            reaped: true,
        })
    };
    match receiver.recv_timeout(limit) {
        Ok(result) => exited(result),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            if let Ok(result) = receiver.try_recv() {
                return exited(result);
            }
            kill_pid(pid, scope);
            let reaped = receiver.recv_timeout(REAP_GRACE).is_ok();
            Ok(Waited {
                pid,
                status: None,
                timed_out: true,
                reaped,
            })
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(io::Error::other("the child's waiter thread ended early"))
        }
    }
}

/// Kills process `pid` (and, for [`KillScope::Tree`], its descendants).
fn kill_pid(pid: u32, scope: KillScope) {
    let pid = pid.to_string();
    let quiet = |mut command: Command| {
        // std's `status()` inherits stdin (unlike `output()`), so null it (N107).
        let _ = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    };
    #[cfg(windows)]
    {
        let mut command = Command::new("taskkill");
        command.args(["/PID", &pid]);
        if scope == KillScope::Tree {
            command.arg("/T");
        }
        command.arg("/F");
        quiet(command);
    }
    #[cfg(not(windows))]
    {
        if scope == KillScope::Tree {
            let mut command = Command::new("pkill");
            command.args(["-KILL", "-P", &pid]);
            quiet(command);
        }
        let mut command = Command::new("kill");
        command.args(["-KILL", &pid]);
        quiet(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A child that runs far longer than any test would let it.
    fn sleeper() -> Child {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("ping");
            c.args(["-n", "60", "127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("60");
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn quick() -> Child {
        let mut command = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "exit 0"]);
            c
        } else {
            Command::new("true")
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_child_that_exits_ends_the_wait_with_its_status() {
        // The limit is only the outer bound: the exit is what ends the wait.
        let waited = wait_limited(quick(), Duration::from_secs(60), KillScope::Process).unwrap();
        assert!(!waited.timed_out && waited.reaped);
        assert!(waited.status.is_some_and(|s| s.success()));
    }

    #[test]
    fn a_child_past_its_limit_is_killed_and_reported() {
        // The limit is the test's own bound; the proof is the report: timed
        // out, no status, and the waiter reaped the killed child.
        for scope in [KillScope::Process, KillScope::Tree] {
            let waited = wait_limited(sleeper(), Duration::from_millis(100), scope).unwrap();
            assert!(waited.timed_out, "{scope:?}");
            assert!(waited.status.is_none(), "{scope:?}");
            assert!(waited.reaped, "{scope:?}: the killed child was not reaped");
            assert!(waited.pid > 0);
        }
    }

    /// A child that logs when the waiter drops it.
    struct Logged {
        child: Child,
        log: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Waitable for Logged {
        fn wait_exit(&mut self) -> io::Result<ExitStatus> {
            self.child.wait()
        }
    }

    impl Drop for Logged {
        fn drop(&mut self) {
            self.log.lock().unwrap().push("dropped");
        }
    }

    #[test]
    fn the_waiter_keeps_the_child_until_the_caller_releases_it() {
        // The status is sent after `wait()`; the child (its handle, which
        // reserves the pid) must still be alive then and go only after the
        // caller's release. The log orders the two without any sleep.
        for _ in 0..5 {
            let log = Arc::new(Mutex::new(Vec::new()));
            let (sender, receiver) = mpsc::channel();
            let (hold, release) = mpsc::channel::<()>();
            let waiter = spawn_waiter(
                Logged {
                    child: quick(),
                    log: Arc::clone(&log),
                },
                sender,
                release,
            )
            .unwrap();
            receiver.recv().unwrap().unwrap();
            log.lock().unwrap().push("released");
            drop(hold);
            waiter.join().unwrap();
            assert_eq!(*log.lock().unwrap(), ["released", "dropped"]);
        }
    }
}
