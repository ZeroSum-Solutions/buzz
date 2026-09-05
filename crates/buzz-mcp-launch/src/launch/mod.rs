//! Launcher mode: start one registry stdio server under containment
//! (memo decision 3).
//!
//! The launcher does not `exec`. `exec` preserves one PID and reaps nothing, so
//! a server that forks a daemon outlives the killed adapter. Instead it stays
//! resident as a supervisor over four independent events, any one of which
//! tears the whole scope down:
//!
//! * the server exits;
//! * our own stdin reaches EOF, which is what the adapter's death looks like
//!   from here;
//! * we are signalled — `SIGTERM` or `SIGINT`, which on Linux is also how
//!   `PR_SET_PDEATHSIG` reports that the adapter was `SIGKILL`ed. Without a
//!   handler the default disposition would kill the launcher outright, leaving
//!   the server reparented to init with its cgroup leaf never killed;
//! * a relay half ends: the server closed its stdout, or a read, write or
//!   flush failed. A server that closes the reply path but keeps running can
//!   never answer another call, so forwarding more frames — including mutating
//!   tool calls — to it is worse than stopping.
//!
//! The two directions are decoupled tasks joined by a bounded queue
//! ([`RELAY_QUEUE_CHUNKS`] × [`RELAY_CHUNK_BYTES`]), so a server that stops
//! draining its stdin parks only the writer, not the reader that observes EOF.
//! A queue that stays full — the one window where EOF is still unobservable —
//! is bounded in turn by [`STDIN_STALL`].
//!
//! What the containment scope guarantees is platform-specific and this module
//! claims no more than it delivers:
//!
//! * **Windows** — a kill-on-close Job Object the launcher joins *before* it
//!   spawns anything, so every process it creates is in the job from its first
//!   instruction. Every Win32 call is checked; any failure aborts the launch
//!   before a server exists. The launcher's own death closes the job, so there
//!   is no signal for it to catch.
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
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};

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

/// Start the server described by `spec` and supervise it until it exits, until
/// this process's stdin reaches EOF, until this process is signalled, or until
/// a relay half ends.
///
/// Returns the server's exit code, [`SIGNAL_EXIT_CODE`] when the launcher was
/// signalled, or 1 when the adapter went away first.
///
/// # Errors
/// [`LaunchError`] for a non-absolute command, a spawn failure, a containment
/// failure, or a relay failure — including a server that closes its stdout
/// while it is still running, which ends the reply path. Every one is
/// returned; none is logged and swallowed.
pub async fn run(spec: LaunchSpec) -> Result<i32, LaunchError> {
    if !Path::new(&spec.command).is_absolute() {
        return Err(LaunchError::NotAbsolute(spec.command));
    }

    // Installed before anything is spawned. A SIGTERM that arrived in the
    // window between the spawn and the `select!` would otherwise take the
    // default disposition and kill the launcher outright, leaving the server
    // reparented and its cgroup leaf never killed.
    let mut termination = TerminationSignals::install()?;

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

    let child_stdin = contained
        .child
        .stdin
        .take()
        .ok_or_else(|| LaunchError::Relay("child stdin was not piped".to_string()))?;
    let child_stdout = contained
        .child
        .stdout
        .take()
        .ok_or_else(|| LaunchError::Relay("child stdout was not piped".to_string()))?;

    // The two halves are decoupled: the reader owns the only view of our stdin
    // and therefore of the adapter's death, and it never blocks on the server.
    let (chunks, queued) = mpsc::channel::<Vec<u8>>(RELAY_QUEUE_CHUNKS);
    let mut reader: JoinHandle<Result<(), LaunchError>> = tokio::spawn(read_our_stdin(chunks));
    let mut writer: JoinHandle<Result<(), LaunchError>> =
        tokio::spawn(write_server_stdin(child_stdin, queued, STDIN_STALL));
    let mut upstream: JoinHandle<Result<(), LaunchError>> =
        tokio::spawn(relay_server_stdout(child_stdout));

    // Set when the stdout relay has already been joined by the `select!`, so
    // the drain below never polls a completed `JoinHandle`.
    let mut upstream_joined = false;

    let ending = tokio::select! {
        status = contained.child.wait() => Ending::Exited(status),
        joined = &mut reader => Ending::from_join(joined, Ending::AdapterGone),
        // The writer only ends cleanly once the queue is closed, which happens
        // only after the reader has ended, so a clean end here means the same
        // thing the reader's does.
        joined = &mut writer => Ending::from_join(joined, Ending::AdapterGone),
        joined = &mut upstream => {
            upstream_joined = true;
            Ending::from_join(joined, Ending::ReplyPathClosed)
        }
        () = termination.recv() => Ending::Signalled,
    };

    let outcome = match ending {
        Ending::Exited(status) => status
            .map(|status| status.code().unwrap_or(1))
            .map_err(|error| LaunchError::Relay(format!("wait for the server: {error}"))),
        Ending::AdapterGone => {
            // The queue is closed, so the writer drops the server's stdin,
            // which is how an MCP server is told to stop. Give it a bounded
            // grace to exit on its own before the scope is torn down, so a
            // clean shutdown is not reported as a kill.
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, contained.child.wait()).await;
            Ok(1)
        }
        Ending::Signalled => {
            tracing::info!("signalled; tearing the mcp server scope down");
            // Non-zero: the server did not finish on its own terms, and the
            // adapter must not read this as a clean exit.
            Ok(SIGNAL_EXIT_CODE)
        }
        Ending::ReplyPathClosed => {
            // The server closed its stdout. If it is on its way out that is the
            // ordinary end; if it is still running, nothing it is asked from
            // here can ever be answered, so this is terminal.
            match tokio::time::timeout(SHUTDOWN_GRACE, contained.child.wait()).await {
                Ok(Ok(status)) => Ok(status.code().unwrap_or(1)),
                Ok(Err(error)) => Err(LaunchError::Relay(format!("wait for the server: {error}"))),
                Err(_) => Err(LaunchError::Relay(
                    "the server closed its stdout while still running; no reply can reach the model"
                        .to_string(),
                )),
            }
        }
        Ending::RelayFailed(error) => Err(error),
    };

    // Drain what the server already wrote before the scope goes away; a reply
    // dropped here would look to the model like a call that never answered. A
    // relay that is still held open by an escaped grandchild simply times out;
    // a relay that *failed* is reported rather than discarded.
    let drain = if upstream_joined {
        None
    } else {
        match tokio::time::timeout(RELAY_DRAIN, &mut upstream).await {
            Ok(joined) => match Ending::from_join(joined, Ending::ReplyPathClosed) {
                Ending::RelayFailed(error) => Some(error),
                _ => None,
            },
            Err(_) => None,
        }
    };

    // Nothing may keep running against a scope that is about to be killed.
    reader.abort();
    writer.abort();
    upstream.abort();

    // Teardown runs on every exit path, including the error one, so a failed
    // wait can never leave the tree running. Its own failure is the
    // containment guarantee failing, so it is returned, not logged; when the
    // relay already failed, both are reported in one error rather than one of
    // them being dropped.
    let teardown = contained.terminate();
    match (
        outcome.and_then(|code| drain.map_or(Ok(code), Err)),
        teardown,
    ) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(_), Err(teardown)) => Err(teardown),
        (Err(failure), Ok(())) => Err(failure),
        (Err(failure), Err(teardown)) => Err(LaunchError::Relay(format!(
            "{failure}; containment teardown also failed: {teardown}"
        ))),
    }
}

/// Why the supervisor stopped waiting.
enum Ending {
    /// The server exited on its own.
    Exited(std::io::Result<std::process::ExitStatus>),
    /// Our stdin reached EOF: the adapter is gone.
    AdapterGone,
    /// The launcher was signalled, which on Linux is also how
    /// `PR_SET_PDEATHSIG` reports a `SIGKILL`ed adapter.
    Signalled,
    /// The server closed its stdout.
    ReplyPathClosed,
    /// A relay read, write or flush failed.
    RelayFailed(LaunchError),
}

impl Ending {
    /// Map a joined relay task to an ending: `clean` when it finished on its
    /// own terms, and a relay failure otherwise. A task that panicked or was
    /// cancelled is a failure too, never a silent success.
    fn from_join(joined: Result<Result<(), LaunchError>, JoinError>, clean: Ending) -> Ending {
        match joined {
            Ok(Ok(())) => clean,
            Ok(Err(error)) => Ending::RelayFailed(error),
            Err(error) => Ending::RelayFailed(LaunchError::Relay(format!(
                "a relay task did not complete: {error}"
            ))),
        }
    }
}

/// Read this process's stdin into `queue` until EOF.
///
/// This task never touches the server, so the adapter's death is observable
/// even while the server has stopped draining its own stdin.
async fn read_our_stdin(queue: mpsc::Sender<Vec<u8>>) -> Result<(), LaunchError> {
    let mut ours = tokio::io::stdin();
    let mut buffer = vec![0u8; RELAY_CHUNK_BYTES];
    loop {
        let read = ours
            .read(&mut buffer)
            .await
            .map_err(|error| LaunchError::Relay(format!("read our stdin: {error}")))?;
        if read == 0 {
            // EOF: the adapter is gone.
            return Ok(());
        }
        if queue.send(buffer[..read].to_vec()).await.is_err() {
            // The queue is only closed by the writer ending, and the writer
            // only ends before us by failing. Never a clean end here.
            return Err(LaunchError::Relay(
                "the server's stdin relay stopped".to_string(),
            ));
        }
    }
}

/// Write queued chunks to the server's stdin until the queue closes.
///
/// `stall` bounds the one window in which the adapter's death is unobservable:
/// a server that accepts nothing keeps the queue full, which parks the reader
/// on its next send. A server that has not taken a byte in that long, while
/// frames are still arriving, is not applying backpressure — it is wedged.
async fn write_server_stdin(
    mut server_stdin: ChildStdin,
    mut queue: mpsc::Receiver<Vec<u8>>,
    stall: Duration,
) -> Result<(), LaunchError> {
    while let Some(chunk) = queue.recv().await {
        let written = tokio::time::timeout(stall, async {
            server_stdin.write_all(&chunk).await?;
            server_stdin.flush().await
        })
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(LaunchError::Relay(format!(
                    "write the server's stdin: {error}"
                )))
            }
            Err(_) => {
                return Err(LaunchError::Relay(format!(
                    "the server accepted no stdin for {stall:?}; it is not draining the transport"
                )))
            }
        }
    }
    // Our stdin reached EOF; dropping the handle closes the server's stdin.
    Ok(())
}

/// Relay the server's stdout to ours until the server closes it.
async fn relay_server_stdout(mut server_stdout: ChildStdout) -> Result<(), LaunchError> {
    let mut ours = tokio::io::stdout();
    let mut buffer = vec![0u8; RELAY_CHUNK_BYTES];
    loop {
        let read = server_stdout
            .read(&mut buffer)
            .await
            .map_err(|error| LaunchError::Relay(format!("read the server's stdout: {error}")))?;
        if read == 0 {
            return Ok(());
        }
        ours.write_all(&buffer[..read])
            .await
            .map_err(|error| LaunchError::Relay(format!("write our stdout: {error}")))?;
        ours.flush()
            .await
            .map_err(|error| LaunchError::Relay(format!("flush our stdout: {error}")))?;
    }
}

/// The termination signals the launcher must catch rather than die on.
#[cfg(unix)]
struct TerminationSignals {
    terminate: tokio::signal::unix::Signal,
    interrupt: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl TerminationSignals {
    /// Install the handlers.
    ///
    /// # Errors
    /// [`LaunchError::Containment`]: without these handlers a signalled
    /// launcher dies on the default disposition and the server tree survives
    /// it, which is the containment guarantee failing.
    fn install() -> Result<Self, LaunchError> {
        use tokio::signal::unix::{signal, SignalKind};
        let install = |kind: SignalKind, name: &str| {
            signal(kind).map_err(|error| {
                LaunchError::Containment(format!("install the {name} handler: {error}"))
            })
        };
        Ok(Self {
            terminate: install(SignalKind::terminate(), "SIGTERM")?,
            interrupt: install(SignalKind::interrupt(), "SIGINT")?,
        })
    }

    /// Resolve when either signal arrives.
    async fn recv(&mut self) {
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
    }
}

/// Windows containment is a kill-on-close Job Object, so the launcher's own
/// death already kills the tree and there is no signal to catch.
#[cfg(windows)]
struct TerminationSignals;

#[cfg(windows)]
impl TerminationSignals {
    fn install() -> Result<Self, LaunchError> {
        Ok(Self)
    }

    async fn recv(&mut self) {
        std::future::pending::<()>().await
    }
}

/// How long a server may take to exit after its stdin closes.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long the stdout relay may take to drain after the server exits.
const RELAY_DRAIN: Duration = Duration::from_millis(500);

/// How many chunks may wait for a server that is slow to drain its stdin.
const RELAY_QUEUE_CHUNKS: usize = 8;

/// The size of one relay chunk, so the queue holds at most
/// `RELAY_QUEUE_CHUNKS * RELAY_CHUNK_BYTES` — 128 KiB — of relayed input.
const RELAY_CHUNK_BYTES: usize = 16 * 1024;

/// How long one write to a server's stdin may take before the server counts as
/// wedged rather than busy.
const STDIN_STALL: Duration = Duration::from_secs(120);

/// The exit code for a signalled launcher, `128 + SIGTERM` by convention.
const SIGNAL_EXIT_CODE: i32 = 143;

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

    /// The guard of [`STDIN_STALL`]: a server that never reads its stdin must
    /// not park the writer forever. Remove the `timeout` in
    /// [`write_server_stdin`] and this test hangs instead of passing.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_never_reads_its_stdin_fails_the_relay() {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .expect("the fixture starts");
        let stdin = child.stdin.take().expect("stdin was piped");

        // More than any pipe buffer, so the write cannot complete: 512 KiB
        // against a 16 to 64 KiB pipe that nothing is draining.
        let (queue, queued) = mpsc::channel::<Vec<u8>>(64);
        for _ in 0..32 {
            queue
                .send(vec![0u8; RELAY_CHUNK_BYTES])
                .await
                .expect("the queue takes the chunk");
        }

        let error = write_server_stdin(stdin, queued, Duration::from_millis(200))
            .await
            .expect_err("a wedged server must fail the relay");
        let _ = child.kill().await;
        assert!(
            error.to_string().contains("accepted no stdin"),
            "the failure must name the stall: {error}"
        );
    }
}
