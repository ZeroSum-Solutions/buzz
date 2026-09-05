//! Windows containment: a kill-on-close Job Object the launcher joins *before*
//! it spawns the server.
//!
//! Windows reaps no descendants, and a child spawned the ordinary way runs for
//! a moment before it can be assigned to a job — memo decision 3 closes that
//! window with `CREATE_SUSPENDED` plus a post-spawn assignment. This module
//! closes it earlier and without the suspended-thread dance: the launcher
//! assigns *itself* to the job first, and the kernel then places every process
//! it creates in the same job automatically. There is no window in which the
//! server runs outside the job, because the job exists before the server does.
//!
//! Every Win32 call is checked and any failure aborts the launch before a
//! server is spawned at all, so a containment failure can never leave a
//! process behind. `crates/buzz-dev-mcp/src/shell.rs:763-790` is the shape of
//! the calls, deliberately not of its error handling: it discards the return
//! value of `SetInformationJobObject` and of `AssignProcessToJobObject`.
//!
//! Because the launcher is in the job, the job handle is held for the whole
//! life of the process and never closed by this module: closing it kills every
//! member, the supervisor included, which would lose the server's exit code on
//! a clean exit and swallow the error on a failed spawn. Process exit closes
//! the last handle, and that is precisely when any survivor must die.
//! `tests/launcher_windows.rs` drives the shipped binary through each
//! fault-injection value and asserts a checked failure leaves nothing behind.

use tokio::process::Command;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::LaunchError;

/// Fault-injection point, read once per launch. `job-create`,
/// `job-set-information` and `job-assign` each make that Win32 call report
/// failure, which is how `job_object_failure_leaves_no_process` proves the
/// launch aborts before any server process exists.
const FAULT_ENV: &str = "BUZZ_MCP_LAUNCH_FAULT";

/// A spawned server plus the job that will be torn down with it.
pub struct Contained {
    /// The server process.
    pub child: tokio::process::Child,
    /// The job handle. Held for the life of the process and never closed by
    /// this module — see the module docs — so it is stored rather than read.
    #[allow(dead_code)]
    job: HANDLE,
}

// SAFETY: `job` is a kernel object reference, not thread-affine. `CloseHandle`
// is documented thread-safe, and Rust's borrows still serialize access to the
// field.
#[allow(unsafe_code)]
unsafe impl Send for Contained {}
#[allow(unsafe_code)]
unsafe impl Sync for Contained {}

impl Contained {
    /// Tear the scope down.
    ///
    /// The launcher is itself in this kill-on-close job, so closing the last
    /// handle would kill the supervisor before it could report the server's
    /// exit code — and, on the spawn-failure path, before it could report the
    /// error at all. The handle is therefore held for the life of the process
    /// and closed by the kernel at exit, which is exactly when every survivor
    /// in the job must die. There is nothing to do here and nothing that can
    /// fail; the `Result` matches the Unix signature so `launch::run` handles
    /// one shape.
    ///
    /// # Errors
    /// Never. The job's teardown is the kernel's, at process exit.
    pub fn terminate(&mut self) -> Result<(), LaunchError> {
        Ok(())
    }
}

/// Join a kill-on-close job, then spawn `command` into it.
///
/// # Errors
/// [`LaunchError::Containment`] for any Win32 failure — raised before the
/// server is spawned — and [`LaunchError::Spawn`] when the process itself could
/// not start.
pub fn spawn_contained(command: &mut Command, program: &str) -> Result<Contained, LaunchError> {
    let job = create_kill_on_close_job()?;

    let child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            // The handle is deliberately not closed here: this process is in
            // the job, so closing it would kill the launcher before the caller
            // could report why the spawn failed. No server started, so the job
            // holds only us, and process exit closes it harmlessly.
            return Err(LaunchError::Spawn {
                command: program.to_string(),
                source,
            });
        }
    };

    Ok(Contained { child, job })
}

/// Create the job, set kill-on-close on it, and put this process in it.
fn create_kill_on_close_job() -> Result<HANDLE, LaunchError> {
    let fault = std::env::var(FAULT_ENV).unwrap_or_default();

    // SAFETY: each call is a documented Win32 FFI call whose arguments satisfy
    // its contract: a null SECURITY_ATTRIBUTES and name for an anonymous job, a
    // zeroed #[repr(C)] struct sized by size_of, and this process's own
    // pseudo-handle. Every return value is checked.
    #[allow(unsafe_code)]
    unsafe {
        let job: HANDLE = if fault == "job-create" {
            std::ptr::null_mut()
        } else {
            CreateJobObjectW(std::ptr::null(), std::ptr::null())
        };
        if job.is_null() {
            return Err(LaunchError::Containment(
                "CreateJobObjectW failed".to_string(),
            ));
        }

        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        // KILL_ON_JOB_CLOSE: when the last handle to the job closes, Windows
        // kills every process still in it — the launcher's own death included.
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = if fault == "job-set-information" {
            0
        } else {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                std::ptr::addr_of!(info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            CloseHandle(job);
            return Err(LaunchError::Containment(
                "SetInformationJobObject failed".to_string(),
            ));
        }

        let assigned = if fault == "job-assign" {
            0
        } else {
            AssignProcessToJobObject(job, GetCurrentProcess())
        };
        if assigned == 0 {
            CloseHandle(job);
            return Err(LaunchError::Containment(
                "AssignProcessToJobObject failed".to_string(),
            ));
        }
        Ok(job)
    }
}
