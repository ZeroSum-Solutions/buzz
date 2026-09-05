//! End-to-end tests through the shipped `buzz-mcp-launch` binary.
//!
//! Every one of these drives the real binary, not a helper: the guards they
//! cover — the environment built from empty, the capability strip, the process
//! -group teardown — only exist on that path.

#![cfg(unix)]

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const LAUNCHER: &str = env!("CARGO_BIN_EXE_buzz-mcp-launch");

/// Wait until `condition` holds, or fail after `limit`.
fn wait_until(limit: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    condition()
}

fn pid_is_alive(pid: i32) -> bool {
    // `kill(pid, 0)` is the portable liveness probe; it reports EPERM (still
    // alive, not ours) and ESRCH (gone) distinctly.
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// A launcher process that is killed if a test panics before finishing it.
struct Launched {
    child: Option<Child>,
}

impl Launched {
    fn child(&mut self) -> &mut Child {
        self.child.as_mut().expect("launcher still owned")
    }

    /// Take ownership of the process, so `wait_with_output` can consume it.
    fn take(&mut self) -> Child {
        self.child.take().expect("launcher still owned")
    }
}

impl Drop for Launched {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Run the launcher over `/bin/sh -c <script>` with `pairs` as `--set` values.
///
/// `log` redirects the launcher's stderr to a file rather than a pipe. A pipe
/// would stay open for as long as any escaped descendant holds it, which would
/// make a test that reads it wait for the very process it is asserting about.
fn launch(
    script: &str,
    pairs: &[(&str, &str)],
    harness_env: &[(&str, &str)],
    log: Option<&Path>,
) -> Launched {
    let mut command = Command::new(LAUNCHER);
    command.args(["launch", "--server", "fixture"]);
    for (name, value) in pairs {
        command.args(["--set", &format!("{name}={value}")]);
    }
    command.args(["--", "/bin/sh", "-c", script]);
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", std::env::var("HOME").unwrap_or_default())
        .env("RUST_LOG", "info");
    for (name, value) in harness_env {
        command.env(name, value);
    }
    let stderr = match log {
        Some(path) => Stdio::from(std::fs::File::create(path).expect("create log")),
        None => Stdio::piped(),
    };
    let child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("launcher starts");
    Launched { child: Some(child) }
}

fn child_environment(
    harness_env: &[(&str, &str)],
    pairs: &[(&str, &str)],
) -> HashMap<String, String> {
    let mut launched = launch("env", pairs, harness_env, None);
    // Closing stdin lets the relay finish; the child has already exited.
    drop(launched.child().stdin.take());
    let output = launched
        .take()
        .wait_with_output()
        .expect("launcher completes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn the_server_environment_is_built_from_empty() {
    let env = child_environment(
        &[
            ("ANTHROPIC_API_KEY", "sk-live-should-not-leak"),
            ("BUZZ_PRIVATE_KEY", "nsec-should-not-leak"),
            ("NOSTR_PRIVATE_KEY", "nsec-should-not-leak"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("GIT_ASKPASS", "/usr/bin/askpass"),
            ("HTTPS_PROXY", "http://employee:token@proxy.example"),
            ("HTTP_PROXY", "http://proxy.example:3128"),
        ],
        &[("SERVER_OPTION", "on")],
    );

    assert!(env.contains_key("PATH"), "the child keeps PATH");
    assert_eq!(env.get("SERVER_OPTION").map(String::as_str), Some("on"));
    for withheld in [
        "ANTHROPIC_API_KEY",
        "BUZZ_PRIVATE_KEY",
        "NOSTR_PRIVATE_KEY",
        "SSH_AUTH_SOCK",
        "GIT_ASKPASS",
        // The authenticated proxy value is the sentinel of memo decision 1.
        "HTTPS_PROXY",
    ] {
        assert!(!env.contains_key(withheld), "{withheld} reached the server");
    }
    assert_eq!(
        env.get("HTTP_PROXY").map(String::as_str),
        Some("http://proxy.example:3128"),
        "a proxy value with no userinfo still passes"
    );
}

#[test]
fn the_capability_is_stripped_before_the_server_starts() {
    const FORGED: &str = "v1.agent-a.1.0102030405060708090a0b0c0d0e0f10";

    // Declared as an approved `--set` pair, which is the only way the name can
    // reach the child map at all: it is not on `BASE_ALLOWLIST`, so an
    // inherited one never enters it and a test that only sets it on the
    // harness would still pass with `build_child_env`'s unconditional
    // `child.remove(CAPABILITY_ENV_VAR)` deleted. With the pair declared, that
    // one line is the whole guard and its removal fails this test.
    let env = child_environment(
        &[("BUZZ_MCP_CAPABILITY", FORGED)],
        &[("BUZZ_MCP_CAPABILITY", FORGED)],
    );
    assert!(
        !env.contains_key("BUZZ_MCP_CAPABILITY"),
        "the capability authorizes every secret bound to the agent and must never reach a server"
    );
    // The run is otherwise ordinary, so the assertion above is about the strip
    // and not about a launch that failed before the server ran.
    assert!(env.contains_key("PATH"), "the server did start");
}

#[test]
fn the_child_tree_dies_when_our_stdin_reaches_eof() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pidfile = dir.path().join("grandchild.pid");
    let script = "sleep 300 & echo $! > \"$PIDFILE\"; sleep 300";
    let mut launched = launch(
        script,
        &[("PIDFILE", pidfile.to_str().expect("utf-8 path"))],
        &[],
        None,
    );

    assert!(
        wait_until(Duration::from_secs(10), || read_pid(&pidfile).is_some()),
        "the fixture never reported its grandchild"
    );
    let grandchild = read_pid(&pidfile).expect("a pid");
    assert!(pid_is_alive(grandchild), "the grandchild should be running");

    // Closing our stdin is what the adapter's death looks like to the launcher.
    drop(launched.child().stdin.take());
    assert!(
        wait_until(Duration::from_secs(10), || !pid_is_alive(grandchild)),
        "the grandchild outlived the launcher: process-group teardown did not run"
    );
}

#[test]
fn grandchild_dies_with_adapter() {
    // The fixture double-forks and calls setsid(), which leaves the launcher's
    // process group. Under a Linux cgroup v2 leaf it still dies; with only a
    // process group it survives, and this test asserts that residue exactly, so
    // a silent change in either direction fails.
    let dir = tempfile::tempdir().expect("temp dir");
    let escapee = dir.path().join("escapee.pid");
    let perl = dir.path().join("escape.pl");
    let mut file = std::fs::File::create(&perl).expect("write fixture");
    file.write_all(
        br#"use POSIX ();
exit 0 if fork;
POSIX::setsid();
open(my $f, '>', $ENV{'ESCAPEE'}) or exit 1;
print $f $$;
close $f;
sleep 300;
"#,
    )
    .expect("write fixture");
    drop(file);

    let log = dir.path().join("launcher.log");
    let mut launched = launch(
        "perl \"$PERLSCRIPT\"; sleep 300",
        &[
            ("PERLSCRIPT", perl.to_str().expect("utf-8 path")),
            ("ESCAPEE", escapee.to_str().expect("utf-8 path")),
        ],
        &[],
        Some(&log),
    );
    assert!(
        wait_until(Duration::from_secs(15), || read_pid(&escapee).is_some()),
        "the fixture never escaped its process group (is perl installed?)"
    );
    let escaped = read_pid(&escapee).expect("a pid");

    drop(launched.child().stdin.take());
    assert!(
        wait_until(Duration::from_secs(20), || launched
            .child()
            .try_wait()
            .ok()
            .flatten()
            .is_some()),
        "the launcher never exited after its stdin closed"
    );
    let stderr = std::fs::read_to_string(&log).unwrap_or_default();
    let process_group_only = stderr.contains("no cgroup v2 leaf available");

    if process_group_only {
        // The documented residue. Asserted, not tolerated: if a future change
        // makes this path contain the escapee, this test fails and the memo's
        // risk entry has to be retired deliberately.
        assert!(
            pid_is_alive(escaped),
            "a setsid() escapee died without a cgroup leaf; the documented residue changed"
        );
        let _ = Command::new("/bin/kill")
            .args(["-9", &escaped.to_string()])
            .status();
    } else {
        assert!(
            wait_until(Duration::from_secs(10), || !pid_is_alive(escaped)),
            "the cgroup leaf did not kill a setsid() escapee"
        );
    }
}

/// A `SIGTERM` must take the whole server tree down.
///
/// This is the signal `PR_SET_PDEATHSIG` delivers when a `SIGKILL`ed adapter
/// dies, and it is also what an operator or a supervisor sends. Without the
/// handler in `launch::run` the default disposition kills the launcher
/// instantly, `Contained::terminate` never runs, and the grandchild below
/// survives — which is exactly what this test fails on.
#[test]
fn a_sigterm_tears_the_server_tree_down() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pidfile = dir.path().join("grandchild.pid");
    let mut launched = launch(
        "sleep 300 & echo $! > \"$PIDFILE\"; sleep 300",
        &[("PIDFILE", pidfile.to_str().expect("utf-8 path"))],
        &[],
        None,
    );

    assert!(
        wait_until(Duration::from_secs(10), || read_pid(&pidfile).is_some()),
        "the fixture never reported its grandchild"
    );
    let grandchild = read_pid(&pidfile).expect("a pid");
    assert!(pid_is_alive(grandchild), "the grandchild should be running");

    let launcher = launched.child().id() as i32;
    assert!(
        Command::new("/bin/kill")
            .args(["-TERM", &launcher.to_string()])
            .status()
            .expect("kill runs")
            .success(),
        "the launcher must accept a SIGTERM"
    );

    assert!(
        wait_until(Duration::from_secs(10), || !pid_is_alive(grandchild)),
        "the grandchild outlived a SIGTERMed launcher: no signal handler tore the scope down"
    );
    let status = launched.take().wait().expect("the launcher exits");
    assert!(
        !status.success(),
        "a signalled launcher must not report success: {status:?}"
    );
}

/// The Linux half of the same guard, through the mechanism that actually
/// happens in production: the adapter is `SIGKILL`ed, so nothing closes our
/// stdin and only `PR_SET_PDEATHSIG` reports the loss.
///
/// The launcher's stdin is a fifo it holds read-write, so it can never reach
/// EOF; the only path left to the teardown is the signal handler.
#[cfg(target_os = "linux")]
#[test]
fn the_child_tree_dies_when_an_interposed_parent_is_sigkilled() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pidfile = dir.path().join("grandchild.pid");
    let fifo = dir.path().join("stdin.fifo");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false),
        "mkfifo must create the launcher's stdin"
    );

    let script = dir.path().join("parent.sh");
    std::fs::write(
        &script,
        "exec 3<> \"$FIFO\"\n\
         \"$LAUNCHER\" launch --server fixture --set PIDFILE=\"$PIDFILE\" -- \
         /bin/sh -c 'sleep 300 & echo $! > \"$PIDFILE\"; sleep 300' <&3 &\n\
         wait\n",
    )
    .expect("write the interposed parent");

    let mut parent = Command::new("/bin/sh")
        .arg(&script)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("LAUNCHER", LAUNCHER)
        .env("FIFO", &fifo)
        .env("PIDFILE", &pidfile)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the interposed parent starts");

    assert!(
        wait_until(Duration::from_secs(15), || read_pid(&pidfile).is_some()),
        "the fixture never reported its grandchild"
    );
    let grandchild = read_pid(&pidfile).expect("a pid");
    assert!(pid_is_alive(grandchild), "the grandchild should be running");

    parent.kill().expect("SIGKILL the interposed parent");
    let _ = parent.wait();

    assert!(
        wait_until(Duration::from_secs(15), || !pid_is_alive(grandchild)),
        "the grandchild outlived a SIGKILLed adapter: PR_SET_PDEATHSIG only killed the launcher"
    );
}

/// A server that never drains its stdin must not wedge the launcher: the
/// adapter's death is still observed and the scope still goes away.
///
/// The 96 KiB below is more than any pipe buffer, so with the relay halves
/// coupled in one loop — a read of our stdin followed by an unbounded
/// `write_all` to the server's — the launcher parks in that write and never
/// sees its own EOF. This test then hangs instead of passing.
#[test]
fn a_server_that_never_reads_its_stdin_does_not_wedge_the_launcher() {
    let dir = tempfile::tempdir().expect("temp dir");
    let pidfile = dir.path().join("grandchild.pid");
    let mut launched = launch(
        "sleep 300 & echo $! > \"$PIDFILE\"; sleep 300",
        &[("PIDFILE", pidfile.to_str().expect("utf-8 path"))],
        &[],
        None,
    );

    assert!(
        wait_until(Duration::from_secs(10), || read_pid(&pidfile).is_some()),
        "the fixture never reported its grandchild"
    );
    let grandchild = read_pid(&pidfile).expect("a pid");

    {
        let stdin = launched.child().stdin.as_mut().expect("stdin is piped");
        stdin
            .write_all(&vec![b'x'; 96 * 1024])
            .expect("the launcher absorbs a frame burst");
        stdin.flush().expect("flush");
    }
    drop(launched.child().stdin.take());

    assert!(
        wait_until(Duration::from_secs(20), || !pid_is_alive(grandchild)),
        "the launcher never noticed its own stdin EOF behind a server that stopped reading"
    );
}

/// A server that closes its stdout but keeps running has ended the reply path:
/// every frame forwarded from then on, including a mutating tool call, reaches
/// a server that can never answer it. The launcher must stop and say so rather
/// than keep relaying and then report success.
#[test]
fn a_server_that_closes_its_stdout_ends_the_launch() {
    let dir = tempfile::tempdir().expect("temp dir");
    let log = dir.path().join("launcher.log");
    let mut launched = launch("exec 1>&-; sleep 300", &[], &[], Some(&log));

    assert!(
        wait_until(Duration::from_secs(20), || launched
            .child()
            .try_wait()
            .ok()
            .flatten()
            .is_some()),
        "the launcher kept running against a server that had closed its stdout"
    );
    let status = launched.take().wait().expect("the launcher exits");
    assert!(
        !status.success(),
        "a dead reply path must not be reported as success: {status:?}"
    );
    let stderr = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        stderr.contains("closed its stdout"),
        "the failure must name the dead reply path: {stderr}"
    );
}

#[test]
fn a_relative_command_is_refused_by_the_binary() {
    let output = Command::new(LAUNCHER)
        .args(["launch", "--server", "fixture", "--", "sh", "-c", "true"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("launcher runs");
    assert!(!output.status.success(), "a bare command name must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("absolute path"),
        "the refusal must say why: {stderr}"
    );
}

#[test]
fn an_unresolvable_reference_fails_the_launch() {
    // A server declared with a credential must not start without it: failing
    // later, inside the server, is a failure the operator cannot read.
    let output = Command::new(LAUNCHER)
        .args([
            "launch",
            "--server",
            "fixture",
            "--secret",
            "TOKEN=mcp:absent-token",
            "--",
            "/bin/sh",
            "-c",
            "true",
        ])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("launcher runs");
    assert!(!output.status.success(), "a missing credential must fail");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TOKEN"),
        "the refusal must name the variable: {stderr}"
    );
}

#[test]
fn the_launcher_binary_is_where_the_bundle_expects_it() {
    assert!(
        PathBuf::from(LAUNCHER).is_file(),
        "the sidecar binary must exist for the bundle step"
    );
}
