//! Unix containment: a dedicated process group, and a cgroup v2 leaf the
//! launcher joins *before* it forks the server, where the kernel offers one.

use std::path::{Path, PathBuf};

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
    /// The cgroup v2 leaf it was placed in, when the kernel offered one.
    cgroup: Option<CgroupLeaf>,
    /// Whether teardown has already run, so `Drop` after an explicit
    /// [`Contained::terminate`] does not report the same failures twice.
    torn_down: bool,
}

impl Contained {
    /// Kill everything in the scope. Idempotent, and safe to call after the
    /// child has already exited.
    ///
    /// # Errors
    /// [`LaunchError::Containment`] naming every teardown step that failed. A
    /// teardown failure is the containment guarantee failing, so it is
    /// returned rather than logged: the caller reports it and exits non-zero.
    pub fn terminate(&mut self) -> Result<(), LaunchError> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;

        let mut failures: Vec<String> = Vec::new();
        if let Some(leaf) = &self.cgroup {
            match leaf.kill() {
                Ok(()) => {
                    tracing::info!(path = %leaf.path.display(), "killed mcp server cgroup leaf");
                }
                Err(error) => {
                    failures.push(format!("cgroup kill {}: {error}", leaf.path.display()));
                }
            }
        }
        if let Err(error) = killpg(Pid::from_raw(self.pgid), Signal::SIGKILL) {
            // ESRCH means the group is already gone, which is the ordinary
            // clean-exit case, not a containment failure.
            if error != nix::errno::Errno::ESRCH {
                failures.push(format!("killpg {}: {error}", self.pgid));
            }
        }
        if let Some(leaf) = self.cgroup.take() {
            if let Err(error) = leaf.remove() {
                // The processes are dead; what is left is an empty directory
                // the kernel may still hold briefly. Reported to the operator,
                // but not a containment failure, because nothing is running.
                tracing::warn!(%error, "could not remove the mcp server cgroup leaf");
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(LaunchError::Containment(failures.join("; ")))
        }
    }
}

impl Drop for Contained {
    fn drop(&mut self) {
        // `Drop` cannot propagate; `run` calls `terminate` explicitly and
        // returns its error. This is the panic and early-return safety net.
        if let Err(error) = self.terminate() {
            tracing::error!(%error, "mcp server containment teardown failed");
        }
    }
}

/// Spawn `command` into its own process group, inside a cgroup v2 leaf when
/// the kernel offers one.
///
/// The leaf is created and **joined by the launcher itself before the fork**,
/// so the server is inside the scope from its first instruction: there is no
/// window in which it, or anything it forks, runs outside `cgroup.kill`. The
/// launcher leaves the leaf again immediately afterwards, so tearing the scope
/// down kills the server tree and not the supervisor that must report its exit
/// code.
///
/// # Errors
/// [`LaunchError::Spawn`] when the process could not start, and
/// [`LaunchError::Containment`] when a cgroup v2 hierarchy this process can
/// write to exists but the leaf could not be created, joined, or left. Where
/// no such hierarchy exists — macOS, and any Linux whose session cgroup is not
/// delegated — the scope is the server's process group, which is what
/// `launch/mod.rs` documents and `grandchild_dies_with_adapter` asserts.
pub fn spawn_contained(command: &mut Command, program: &str) -> Result<Contained, LaunchError> {
    // `process_group(0)` is tokio's safe wrapper for `setpgid` in the pre-exec
    // hook, so the launcher takes no `unsafe` on this path.
    command.process_group(0);

    // Ask the kernel to signal the launcher when its parent dies, so an adapter
    // that is SIGKILLed is still noticed on Linux. This delivers SIGTERM to the
    // *launcher*, and it takes the scope down only because `launch::run`
    // catches that signal and calls `Contained::terminate`; on the default
    // disposition it would kill the supervisor and orphan the server tree.
    #[cfg(target_os = "linux")]
    set_parent_death_signal();

    let cgroup = match CgroupLeaf::create_and_join() {
        Ok(leaf) => Some(leaf),
        Err(CgroupUnavailable::NoHierarchy(reason)) => {
            tracing::info!(
                reason,
                "no cgroup v2 leaf available; containment is this server's process group only, so a server that double-forks and calls setsid() can survive"
            );
            None
        }
        // A hierarchy this process can write to exists and the leaf still
        // failed: that is containment failing, not containment being absent.
        Err(CgroupUnavailable::Failed(reason)) => {
            return Err(LaunchError::Containment(reason));
        }
    };

    let child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            if let Some(leaf) = cgroup {
                leaf.leave_and_discard();
            }
            return Err(LaunchError::Spawn {
                command: program.to_string(),
                source,
            });
        }
    };
    let pid = child.id().unwrap_or(0) as i32;

    // The child inherited the leaf at fork; the launcher now steps back out so
    // `cgroup.kill` at teardown does not take the supervisor with it.
    if let Some(leaf) = &cgroup {
        if let Err(reason) = leaf.leave() {
            // The server is already running inside a scope we can no longer
            // tear down without killing ourselves. Kill it by process group
            // and refuse the launch.
            let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
            return Err(LaunchError::Containment(reason));
        }
    }

    Ok(Contained {
        child,
        pgid: pid,
        cgroup,
        torn_down: false,
    })
}

#[cfg(target_os = "linux")]
fn set_parent_death_signal() {
    use nix::sys::prctl::set_pdeathsig;
    // SIGTERM rather than SIGKILL: `launch::run` has a handler for it and tears
    // the containment scope down there. SIGKILL cannot be caught, so it would
    // leave the server tree behind.
    if let Err(error) = set_pdeathsig(Some(Signal::SIGTERM)) {
        tracing::warn!(%error, "could not set PR_SET_PDEATHSIG; falling back to stdin EOF");
    }
}

/// Why no cgroup v2 leaf was created.
enum CgroupUnavailable {
    /// This host offers no cgroup v2 hierarchy this process may write to. The
    /// documented process-group-only path.
    NoHierarchy(&'static str),
    /// A writable hierarchy exists and the leaf still could not be set up.
    Failed(String),
}

/// A cgroup v2 leaf holding one server tree.
struct CgroupLeaf {
    path: PathBuf,
    parent: PathBuf,
}

impl CgroupLeaf {
    /// The unified-hierarchy cgroup directory this process currently sits in.
    fn own_cgroup_dir() -> Option<PathBuf> {
        let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        // Unified hierarchy lines start with `0::`.
        let relative = own
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?
            .trim()
            .trim_start_matches('/');
        let path = PathBuf::from("/sys/fs/cgroup").join(relative);
        path.is_dir().then_some(path)
    }

    /// Create a leaf under this process's own cgroup and move this process
    /// into it, before any server is forked.
    fn create_and_join() -> Result<Self, CgroupUnavailable> {
        if !cfg!(target_os = "linux") {
            return Err(CgroupUnavailable::NoHierarchy(
                "cgroup v2 is a linux facility",
            ));
        }
        let parent = Self::own_cgroup_dir().ok_or(CgroupUnavailable::NoHierarchy(
            "this process is in no writable cgroup v2 unified hierarchy",
        ))?;
        let path = parent.join(format!("buzz-mcp-{}", std::process::id()));
        if let Err(error) = std::fs::create_dir_all(&path) {
            // A hierarchy that will not take a new leaf at all — an undelegated
            // session scope, a read-only mount — is the absent case, not a
            // failure of a scope this platform promised.
            return Err(CgroupUnavailable::NoHierarchy(
                if error.kind() == std::io::ErrorKind::PermissionDenied {
                    "this process may not create a cgroup v2 leaf"
                } else {
                    "no cgroup v2 leaf could be created"
                },
            ));
        }
        let leaf = Self { path, parent };
        match write_own_pid(&leaf.path) {
            Ok(()) => Ok(leaf),
            Err(reason) => {
                // The leaf exists but will not take us: containment is
                // available here and it failed.
                let _ = std::fs::remove_dir(&leaf.path);
                Err(CgroupUnavailable::Failed(reason))
            }
        }
    }

    /// Move this process back to the cgroup it came from, leaving only the
    /// server tree in the leaf.
    fn leave(&self) -> Result<(), String> {
        write_own_pid(&self.parent)
    }

    /// Best-effort unwind for the spawn-failure path: no server was started,
    /// so there is nothing to contain and nothing to report.
    fn leave_and_discard(self) {
        let _ = self.leave();
        let _ = std::fs::remove_dir(&self.path);
    }

    /// Kill every process in the leaf, including any that called `setsid()`.
    fn kill(&self) -> std::io::Result<()> {
        std::fs::write(self.path.join("cgroup.kill"), "1")
    }

    /// Remove the (now empty) leaf directory.
    fn remove(self) -> std::io::Result<()> {
        std::fs::remove_dir(&self.path)
    }
}

/// Move this process into the cgroup at `dir`.
fn write_own_pid(dir: &Path) -> Result<(), String> {
    let procs = dir.join("cgroup.procs");
    std::fs::write(&procs, std::process::id().to_string())
        .map_err(|error| format!("write {}: {error}", procs.display()))
}
