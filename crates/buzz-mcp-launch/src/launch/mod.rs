//! Launcher mode: start one registry stdio server under containment
//! (memo decision 3).
//!
//! The launcher does not `exec`. `exec` preserves one PID and reaps nothing, so
//! a server that forks a daemon outlives the killed adapter. Instead it stays
//! resident as a supervisor: it builds the child environment from empty
//! (`crate::env`), places the server in its own containment scope, relays stdio
//! both ways, and tears the whole scope down when the server exits or its own
//! stdin reaches EOF — which is what the parent's death looks like from here.
//!
//! What the containment scope guarantees is platform-specific and this module
//! claims no more than it delivers:
//!
//! * **Windows** — a kill-on-close Job Object the launcher joins *before* it
//!   spawns anything, so every process it creates is in the job from its first
//!   instruction. Every Win32 call is checked; any failure aborts the launch
//!   before a server exists.
//! * **Linux with a writable cgroup v2 hierarchy** — a dedicated leaf cgroup
//!   killed through `cgroup.kill`, which no `setsid()` can leave. The launcher
//!   joins the leaf before it forks the server and steps out again straight
//!   after, so nothing the server forks can be created outside the scope, and
//!   a leaf that exists but cannot be joined or left is a
//!   [`LaunchError::Containment`], never a downgrade.
//! * **Everywhere else on Unix** — macOS, and any Linux whose session cgroup
//!   is not delegated — the server's own process group, killed with `killpg`.
//!   A server that double-forks and calls `setsid()` enters a new session,
//!   leaves that group, and survives. That residue is documented, and
//!   `grandchild_dies_with_adapter` asserts it exactly, so a silent change in
//!   either direction fails the test.
//!
//! Teardown failures on any of these paths are returned from
//! [`Contained::terminate`] and out of [`run`]; none is logged and swallowed.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Why a server could not be launched or contained.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    /// The command is not an absolute path.
    ///
    /// The launcher clears `PATH` for its own resolution on purpose: a bare
    /// name would resolve through an environment the operator does not control.
    #[error("server command must be an absolute path, got `{0}`")]
    NotAbsolute(String),
    /// The child could not be spawned.
    #[error("spawn `{command}`: {source}")]
    Spawn {
        /// The command that failed.
        command: String,
        /// The OS error.
        source: std::io::Error,
    },
    /// Containment could not be established. Never a warning: a tolerated
    /// containment failure leaks whole process trees.
    #[error("containment: {0}")]
    Containment(String),
    /// The stdio relay failed.
    #[error("stdio relay: {0}")]
    Relay(String),
}

/// One server's launch description.
pub struct LaunchSpec {
    /// Absolute path of the server executable.
    pub command: String,
    /// Arguments, passed through unchanged.
    pub args: Vec<String>,
    /// The complete child environment, already built from empty.
    pub env: BTreeMap<String, String>,
}

/// Start the server described by `spec` and supervise it until it exits or
/// this process's stdin reaches EOF.
///
/// Returns the server's exit code, or 1 when it was terminated by containment.
///
/// # Errors
/// [`LaunchError`] for a non-absolute command, a spawn failure, a containment
/// failure, or a relay failure. Every one is returned; none is logged and
/// swallowed.
pub async fn run(spec: LaunchSpec) -> Result<i32, LaunchError> {
    if !Path::new(&spec.command).is_absolute() {
        return Err(LaunchError::NotAbsolute(spec.command));
    }

    let mut command = Command::new(&spec.command);
    command
        .args(&spec.args)
        .env_clear()
        .envs(&spec.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    #[cfg(unix)]
    let mut contained = unix::spawn_contained(&mut command, &spec.command)?;
    #[cfg(windows)]
    let mut contained = windows::spawn_contained(&mut command, &spec.command)?;
    #[cfg(not(any(unix, windows)))]
    compile_error!("buzz-mcp-launch supports unix and windows only");

    let mut child_stdin = contained
        .child
        .stdin
        .take()
        .ok_or_else(|| LaunchError::Relay("child stdin was not piped".to_string()))?;
    let mut child_stdout = contained
        .child
        .stdout
        .take()
        .ok_or_else(|| LaunchError::Relay("child stdout was not piped".to_string()))?;

    let downstream = tokio::spawn(async move {
        // Our stdin reaching EOF is what the adapter's death looks like from
        // here; the relay ending is the signal to tear the scope down.
        let mut ours = tokio::io::stdin();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match ours.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if child_stdin.write_all(&buffer[..read]).await.is_err()
                        || child_stdin.flush().await.is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let upstream = tokio::spawn(async move {
        let mut ours = tokio::io::stdout();
        let mut buffer = [0u8; 16 * 1024];
        loop {
            match child_stdout.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if ours.write_all(&buffer[..read]).await.is_err() || ours.flush().await.is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let status = tokio::select! {
        status = contained.child.wait() => {
            status.map(|s| s.code().unwrap_or(1)).map_err(|e| LaunchError::Relay(e.to_string()))
        }
        _ = async {
            let _ = downstream.await;
            // Our stdin reached EOF: the server's own stdin is now closed too,
            // which is how an MCP server is told to stop. Give it a bounded
            // grace period to exit on its own before the scope is torn down,
            // so a clean shutdown is not reported as a kill.
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => Ok(1),
    };

    // Drain what the server already wrote before the scope goes away; a reply
    // dropped here would look to the model like a call that never answered.
    let _ = tokio::time::timeout(RELAY_DRAIN, upstream).await;

    // Teardown runs on every exit path, including the error one, so a failed
    // wait can never leave the tree running. Its own failure is the
    // containment guarantee failing, so it is returned, not logged: the
    // relay's status is reported first when it already failed, and otherwise
    // the teardown decides the result.
    let teardown = contained.terminate();
    let code = status?;
    teardown?;
    Ok(code)
}

/// How long a server may take to exit after its stdin closes.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the stdout relay may take to drain after the server exits.
const RELAY_DRAIN: std::time::Duration = std::time::Duration::from_millis(500);

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_relative_command_is_refused() {
        let error = run(LaunchSpec {
            command: "node".to_string(),
            args: vec![],
            env: BTreeMap::new(),
        })
        .await
        .expect_err("a bare name must be refused");
        assert!(matches!(error, LaunchError::NotAbsolute(_)), "{error:?}");
    }
}
