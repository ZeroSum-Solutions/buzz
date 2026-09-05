//! Unix containment: a dedicated process group, and a cgroup v2 leaf where the
//! kernel offers one.

use std::path::PathBuf;

use nix::sys::signal::{killpg, Signal};
use nix::unistd::Pid;
use tokio::process::Command;

use super::LaunchError;

/// A spawned server plus the scope that will be torn down with it.
pub struct Contained {
    /// The server process.
    pub child: tokio::process::Child,
    /// Its process group id, which equals its pid because it was spawned with
    /// `process_group(0)`.
    pgid: i32,
    /// The cgroup v2 leaf it was placed in, when one could be created.
    cgroup: Option<CgroupLeaf>,
}

impl Contained {
    /// Kill everything in the scope. Idempotent, and safe to call after the
    /// child has already exited.
    pub fn terminate(&mut self) {
        if let Some(leaf) = &self.cgroup {
            match leaf.kill() {
                Ok(()) => {
                    tracing::info!(path = %leaf.path.display(), "killed mcp server cgroup leaf");
                }
                Err(error) => {
                    // Reported, not swallowed: this is the strong guarantee
                    // failing, and the process-group kill below is all that is
                    // left.
                    tracing::error!(%error, path = %leaf.path.display(), "cgroup kill failed");
                }
            }
        }
        if let Err(error) = killpg(Pid::from_raw(self.pgid), Signal::SIGKILL) {
            if error != nix::errno::Errno::ESRCH {
                tracing::error!(%error, pgid = self.pgid, "killpg failed");
            }
        }
        if let Some(leaf) = self.cgroup.take() {
            leaf.remove();
        }
    }
}

impl Drop for Contained {
    fn drop(&mut self) {
        self.terminate();
    }
}

/// Spawn `command` in its own process group, and in a cgroup v2 leaf when one
/// is available.
///
/// # Errors
/// [`LaunchError::Spawn`] when the process could not start. A cgroup leaf that
/// cannot be created is not an error: the memo claims only the process-group
/// guarantee on that path, and says so.
pub fn spawn_contained(command: &mut Command, program: &str) -> Result<Contained, LaunchError> {
    // `process_group(0)` is tokio's safe wrapper for `setpgid` in the pre-exec
    // hook, so the launcher takes no `unsafe` on this path.
    command.process_group(0);

    // The launcher itself asks the kernel to kill it when its parent dies, so
    // an adapter that is SIGKILLed still takes the scope down on Linux.
    #[cfg(target_os = "linux")]
    set_parent_death_signal();

    let child = command.spawn().map_err(|source| LaunchError::Spawn {
        command: program.to_string(),
        source,
    })?;
    let pid = child.id().unwrap_or(0) as i32;
    let cgroup = CgroupLeaf::create_for(pid);
    if cgroup.is_none() {
        tracing::info!(
            "no cgroup v2 leaf available; containment is this server's process group only, so a server that double-forks and calls setsid() can survive"
        );
    }
    Ok(Contained {
        child,
        pgid: pid,
        cgroup,
    })
}

#[cfg(target_os = "linux")]
fn set_parent_death_signal() {
    use nix::sys::prctl::set_pdeathsig;
    if let Err(error) = set_pdeathsig(Some(Signal::SIGTERM)) {
        tracing::warn!(%error, "could not set PR_SET_PDEATHSIG; falling back to stdin EOF");
    }
}

/// A cgroup v2 leaf holding one server tree.
struct CgroupLeaf {
    path: PathBuf,
}

impl CgroupLeaf {
    /// Create a leaf for `pid` and move it in, or `None` when cgroup v2 is
    /// unavailable or this process may not create one.
    fn create_for(pid: i32) -> Option<Self> {
        if !cfg!(target_os = "linux") || pid <= 0 {
            return None;
        }
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        // Unified hierarchy lines start with `0::`.
        let relative = own
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim()
            .trim_start_matches('/');
        let parent = PathBuf::from("/sys/fs/cgroup").join(relative);
        let path = parent.join(format!("buzz-mcp-{pid}"));
        std::fs::create_dir_all(&path).ok()?;
        if std::fs::write(path.join("cgroup.procs"), pid.to_string()).is_err() {
            let _ = std::fs::remove_dir(&path);
            return None;
        }
        Some(Self { path })
    }

    /// Kill every process in the leaf, including any that called `setsid()`.
    fn kill(&self) -> std::io::Result<()> {
        std::fs::write(self.path.join("cgroup.kill"), "1")
    }

    /// Remove the (now empty) leaf directory.
    fn remove(self) {
        let _ = std::fs::remove_dir(&self.path);
    }
}
