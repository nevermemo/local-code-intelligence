//! Windows only: puts every language server, and everything it starts, in a
//! Job Object that ends them together.
//!
//! Windows ends a process's descendants only through a Job Object. Plain
//! `std`/`tokio` spawning sets none up, and language servers are rarely one
//! process: npm installs `typescript-language-server` and `pyright` as `.cmd`
//! shims, so the server is `cmd.exe /c x.cmd` with `node.exe` under it, and
//! `rust-analyzer` can be a `rustup` proxy over the real binary. Killing the
//! direct child (`kill_on_drop`, or a timed-out shutdown) then leaves the rest
//! running, holding gigabytes of memory and the server's pipe handles.
//!
//! The job is created with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: closing the
//! last handle to it, which dropping the owning [`Job`] does, ends every
//! process in it, including the case where this process itself is killed. The
//! child is started suspended and assigned to the job before its first
//! instruction runs, so it cannot start a descendant outside the job.
//!
//! Nothing here may fail a server start for want of a job (a restrictive
//! parent job, for example): that case warns once and the server runs as it
//! did before.

use std::{
    ffi::c_void,
    io, mem,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    ptr,
    sync::Once,
};
use tokio::process::{Child, Command};
use windows_sys::Win32::{
    Foundation::{HANDLE, INVALID_HANDLE_VALUE},
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
        },
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        },
        Threading::{
            CREATE_SUSPENDED, GetProcessIdOfThread, OpenThread, ResumeThread,
            THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
        },
    },
};

/// The exit code `TerminateJobObject` gives every process it ends.
const TERMINATED: u32 = 1;

/// A Job Object that kills its whole process tree when the last handle to it
/// closes. The handle is owned: dropping the `Job` closes it.
pub(super) struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Self> {
        // SAFETY: both arguments may be null (default security, no name).
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `handle` is a new job handle that nothing else owns.
        let job = Self(unsafe { OwnedHandle::from_raw_handle(handle) });
        let limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: `limits` is a live JOBOBJECT_EXTENDED_LIMIT_INFORMATION and
        // the length passed is its size.
        let set = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast::<c_void>(),
                mem::size_of_val(&limits) as u32,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle()
    }

    /// Ends every process in the job now. Best effort: the processes may
    /// already be gone, and the goal is the same either way.
    pub(super) fn terminate(&self) {
        // SAFETY: `raw` is a live job handle for the whole call.
        let _ = unsafe { TerminateJobObject(self.raw(), TERMINATED) };
    }
}

/// Step one of confining a server, before it is spawned: creates its job and
/// marks `command` to start suspended. Returns `None` (after one warning) when
/// the job cannot be created; `command` is then left alone and the server
/// starts unconfined.
pub(super) fn prepare(command: &mut Command) -> Option<Job> {
    match Job::new() {
        Ok(job) => {
            command.creation_flags(CREATE_SUSPENDED);
            Some(job)
        }
        Err(error) => {
            warn_unconfined("create a job object", &error);
            None
        }
    }
}

/// Step two, right after the spawn: puts `child` in the job it was prepared
/// for and lets it run. Returns the job to keep for as long as the server
/// lives, or `None` when the child could not be assigned (warned once; the
/// server then runs unconfined). A `job` of `None` means [`prepare`] started
/// the child normally and there is nothing to do.
///
/// Fails only when no thread of the suspended child can be resumed. Drop
/// `child` then (`kill_on_drop` ends it): it would otherwise never run.
pub(super) fn adopt(job: Option<Job>, child: &Child) -> io::Result<Option<Job>> {
    let Some(job) = job else { return Ok(None) };
    let assigned = match child.raw_handle() {
        // SAFETY: both handles are live: the job is owned here and `child`
        // owns the process handle.
        Some(process) if unsafe { AssignProcessToJobObject(job.raw(), process) } != 0 => Ok(()),
        Some(_) => Err(io::Error::last_os_error()),
        None => Err(io::Error::other("the process has already exited")),
    };
    // The child must run whether or not the assignment worked.
    let id = child
        .id()
        .ok_or_else(|| io::Error::other("the suspended process has already exited"))?;
    resume_suspended_process(id)?;
    match assigned {
        Ok(()) => Ok(Some(job)),
        Err(error) => {
            warn_unconfined("assign the language server to its job object", &error);
            Ok(None)
        }
    }
}

/// Resumes the suspended process `pid` by resuming every thread Toolhelp lists
/// for it. `CreateProcess` returns the primary thread's handle, but `std` and
/// `tokio` do not expose it, and the snapshot promises no order and no single
/// thread (a security product may inject one), so no entry can be taken for the
/// primary thread. Resuming a thread that is not suspended does nothing, so each
/// one is resumed, and the process counts as started once at least one of them
/// was suspended. If every thread that could be opened was already running,
/// something else started the process: that is logged, and the start goes on.
/// If some thread could not be opened and none was suspended, the primary
/// thread may be the one that is still suspended, and that is an error.
fn resume_suspended_process(pid: u32) -> io::Result<()> {
    // SAFETY: a thread snapshot ignores its process-id argument.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `snapshot` is a new handle that nothing else owns.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let mut entry = THREADENTRY32 {
        dwSize: mem::size_of::<THREADENTRY32>() as u32,
        ..Default::default()
    };
    let mut threads = 0u32;
    let mut was_suspended = false;
    let mut last_error = None;
    // SAFETY: `entry` is a live THREADENTRY32 with `dwSize` set, and the
    // snapshot handle stays open for the whole loop.
    let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) };
    while more != 0 {
        if entry.th32OwnerProcessID == pid {
            // A thread that ended since the snapshot cannot be opened; that is
            // an error only when no thread is found suspended (see below).
            match resume_thread(entry.th32ThreadID, pid) {
                Ok(Some(suspended)) => {
                    threads += 1;
                    was_suspended |= suspended;
                }
                // The thread ended since the snapshot and its id went to a
                // thread of another process: not ours, and left alone.
                Ok(None) => {}
                Err(error) => last_error = Some(error),
            }
        }
        // SAFETY: as for `Thread32First`.
        more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) };
    }
    if was_suspended {
        return Ok(());
    }
    // Nothing was suspended. A thread that could not be opened may be the
    // primary one, still suspended, so the start is then not known to have
    // worked: the caller drops the child, which ends it.
    if threads == 0 || last_error.is_some() {
        return Err(last_error.unwrap_or_else(|| {
            io::Error::other(format!(
                "the suspended process {pid} has no thread to resume"
            ))
        }));
    }
    tracing::warn!(
        pid,
        threads,
        "no thread of a language server started suspended was suspended; assuming it already runs"
    );
    Ok(())
}

/// Resumes thread `id` if it belongs to process `pid`, and returns whether it
/// was suspended (its previous suspend count was 1 or more), or `None` when it
/// belongs to another process. A thread suspended more than once is resumed
/// until it runs.
///
/// A snapshot names a thread by id, and an id is reused once its thread ends,
/// so the owner is checked on the opened handle, which pins the thread. (The
/// process itself cannot be mistaken for another: its handle is held, so its
/// id is not reused.)
fn resume_thread(id: u32, pid: u32) -> io::Result<Option<bool>> {
    // SAFETY: plain call; a null result is checked below.
    let thread = unsafe {
        OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_QUERY_LIMITED_INFORMATION,
            0,
            id,
        )
    };
    if thread.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `thread` is a new handle that nothing else owns.
    let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
    // SAFETY: `thread` has THREAD_QUERY_LIMITED_INFORMATION access.
    let owner = unsafe { GetProcessIdOfThread(thread.as_raw_handle()) };
    if owner == 0 {
        // The query failed, so the owner is unknown: that is an error, not a
        // thread of another process. It could be the suspended primary thread.
        return Err(io::Error::last_os_error());
    }
    if owner != pid {
        return Ok(None);
    }
    // SAFETY: `thread` has THREAD_SUSPEND_RESUME access.
    let mut previous = unsafe { ResumeThread(thread.as_raw_handle()) };
    if previous == u32::MAX {
        return Err(io::Error::last_os_error());
    }
    let was_suspended = previous >= 1;
    while previous > 1 {
        // SAFETY: as above.
        previous = unsafe { ResumeThread(thread.as_raw_handle()) };
        if previous == u32::MAX {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Some(was_suspended))
}

/// Logs why a server runs without a job, once per process: the cause is
/// usually the environment (an enclosing job that allows no nesting), so the
/// same line for every server start would only be noise.
fn warn_unconfined(step: &str, error: &io::Error) {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            error = %error,
            "could not {step}; language servers start without one, so a server that is \
             discarded or killed may leave the processes it started running"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{process::Stdio, time::Duration};
    use windows_sys::Win32::{
        Foundation::WAIT_OBJECT_0,
        System::{
            JobObjects::{JobObjectBasicProcessIdList, QueryInformationJobObject},
            Threading::{
                GetCurrentProcessId, GetCurrentThreadId, OpenProcess, PROCESS_SYNCHRONIZE,
                WaitForSingleObject,
            },
        },
    };

    /// `JOBOBJECT_BASIC_PROCESS_ID_LIST` with room for 16 ids.
    #[repr(C)]
    struct ProcessIds {
        assigned: u32,
        listed: u32,
        ids: [usize; 16],
    }

    fn process_ids(job: &Job) -> Vec<u32> {
        let mut list = ProcessIds {
            assigned: 0,
            listed: 0,
            ids: [0; 16],
        };
        // SAFETY: `list` is a live buffer of the size passed, laid out like
        // JOBOBJECT_BASIC_PROCESS_ID_LIST.
        let queried = unsafe {
            QueryInformationJobObject(
                job.raw(),
                JobObjectBasicProcessIdList,
                ptr::from_mut(&mut list).cast::<c_void>(),
                mem::size_of_val(&list) as u32,
                ptr::null_mut(),
            )
        };
        assert_ne!(queried, 0, "{}", io::Error::last_os_error());
        list.ids[..list.listed as usize]
            .iter()
            .map(|id| *id as u32)
            .collect()
    }

    /// The point of the job: `cmd.exe` (the direct child) starts `ping.exe`
    /// (a grandchild, like node under an npm shim), and closing the job handle
    /// ends both while the `Child` for `cmd.exe` is still held, so it is the
    /// job and not `kill_on_drop` that ends them.
    #[tokio::test]
    async fn dropping_the_job_ends_the_grandchild() {
        let mut command = Command::new("cmd.exe");
        command
            .args(["/c", "ping", "-n", "120", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let job = prepare(&mut command).expect("a job object can be created");
        let child = command.spawn().expect("cmd.exe starts");
        let job = adopt(Some(job), &child)
            .expect("the suspended child resumes")
            .expect("the child is assigned to the job");

        // The resumed child runs and starts its grandchild inside the job.
        let started = std::time::Instant::now();
        let mut ids = process_ids(&job);
        while ids.len() < 2 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the job holds only {ids:?}; cmd.exe never started ping.exe"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
            ids = process_ids(&job);
        }
        let waits: Vec<OwnedHandle> = ids
            .iter()
            .map(|id| {
                // SAFETY: plain call; a null result is checked below.
                let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, *id) };
                assert!(!handle.is_null(), "{}", io::Error::last_os_error());
                // SAFETY: `handle` is a new handle that nothing else owns.
                unsafe { OwnedHandle::from_raw_handle(handle) }
            })
            .collect();

        drop(job);
        for (id, handle) in ids.iter().zip(&waits) {
            // SAFETY: `handle` has SYNCHRONIZE access.
            let wait = unsafe { WaitForSingleObject(handle.as_raw_handle(), 10_000) };
            assert_eq!(
                wait, WAIT_OBJECT_0,
                "process {id} was still running 10 s after the job handle closed"
            );
        }
        drop(child);
    }

    /// `terminate` ends the tree without waiting for the handle to close.
    #[tokio::test]
    async fn terminating_the_job_ends_every_process_in_it() {
        let mut command = Command::new("cmd.exe");
        command
            .args(["/c", "ping", "-n", "120", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let job = prepare(&mut command).expect("a job object can be created");
        let child = command.spawn().expect("cmd.exe starts");
        let job = adopt(Some(job), &child)
            .expect("the suspended child resumes")
            .expect("the child is assigned to the job");
        let started = std::time::Instant::now();
        while process_ids(&job).len() < 2 {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "cmd.exe never started ping.exe"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        job.terminate();
        let started = std::time::Instant::now();
        while !process_ids(&job).is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "the job still holds {:?} after TerminateJobObject",
                process_ids(&job)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        drop(child);
    }

    /// A thread id that belongs to another process is left alone: the current
    /// thread is not suspended, so resuming it is harmless, and what is checked
    /// is which answer the owner check gives.
    #[test]
    fn a_thread_of_another_process_is_not_resumed() {
        // SAFETY: plain calls without arguments.
        let (pid, thread) = unsafe { (GetCurrentProcessId(), GetCurrentThreadId()) };
        assert!(matches!(resume_thread(thread, pid), Ok(Some(false))));
        assert!(matches!(resume_thread(thread, pid + 1), Ok(None)));
    }
}
