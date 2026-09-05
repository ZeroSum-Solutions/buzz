//! Windows containment tests, through the shipped `buzz-mcp-launch` binary.
//!
//! `launch/windows.rs` checks the return value of every Job Object call and
//! aborts the launch before a server exists when one fails. That is a claim no
//! other job can test: `tests/launcher.rs` is `#![cfg(unix)]`, and every other
//! lane compiles this module without running it. These tests drive the real
//! binary through each `BUZZ_MCP_LAUNCH_FAULT` value and assert the abort is
//! real — the server never ran — and that the ordinary path still reports the
//! server's own exit code, which the deliberately-unclosed job handle is there
//! to preserve.

#![cfg(windows)]

use std::process::{Command, Output};

const LAUNCHER: &str = env!("CARGO_BIN_EXE_buzz-mcp-launch");

/// Printed by the server fixture. Its absence in the launcher's stdout is how
/// these tests prove no server process ran.
const RAN: &str = "BUZZ-SERVER-RAN";

/// Every fault the Windows containment path injects, with the Win32 call each
/// one makes fail.
const FAULTS: &[(&str, &str)] = &[
    ("job-create", "CreateJobObjectW"),
    ("job-set-information", "SetInformationJobObject"),
    ("job-assign", "AssignProcessToJobObject"),
];

fn system_root() -> String {
    std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string())
}

fn cmd_exe() -> String {
    format!(r"{}\System32\cmd.exe", system_root())
}

/// Run the launcher over `cmd.exe /C <script>`, with `fault` in its own
/// environment when given.
fn launch(script: &str, fault: Option<&str>) -> Output {
    let mut command = Command::new(LAUNCHER);
    command
        .args(["launch", "--server", "fixture", "--"])
        .arg(cmd_exe())
        .args(["/C", script])
        .env_clear()
        .env("SystemRoot", system_root())
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("RUST_LOG", "info");
    if let Some(value) = fault {
        command.env("BUZZ_MCP_LAUNCH_FAULT", value);
    }
    command.output().expect("launcher runs")
}

#[test]
fn job_object_failure_leaves_no_process() {
    // The control: with no fault the same invocation does start the server, so
    // the assertions below are about the abort and not about a fixture that
    // never worked.
    let ok = launch(&format!("echo {RAN}"), None);
    assert!(
        ok.status.success(),
        "the control launch failed: {}",
        String::from_utf8_lossy(&ok.stderr)
    );
    assert!(
        String::from_utf8_lossy(&ok.stdout).contains(RAN),
        "the control launch never ran the server"
    );

    for (fault, call) in FAULTS {
        let output = launch(&format!("echo {RAN}"), Some(fault));
        assert!(
            !output.status.success(),
            "{fault}: a containment failure must fail the launch"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("containment") && stderr.contains(call),
            "{fault}: the operator must be told which call failed: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(RAN),
            "{fault}: the server ran even though containment failed"
        );
    }
}

#[test]
fn a_clean_exit_reports_the_server_exit_code() {
    // The launcher is inside its own kill-on-close job, so closing the job
    // handle at teardown would kill the supervisor before it could report
    // this. `Contained::terminate` therefore holds the handle to process exit;
    // this test fails if that ever changes back.
    let output = launch("exit 7", None);
    assert_eq!(
        output.status.code(),
        Some(7),
        "the launcher lost the server's exit code: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
