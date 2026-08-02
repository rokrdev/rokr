//! Ticket 69 (bash-command-sandbox-confinement) acceptance tests: the
//! shared adversarial fixture file every later sandboxed entry point
//! (ticket 75) reuses. Exercises `rokr_tools::bash::BashTool` wrapped
//! through `SeatbeltSandbox` end to end -- real `sandbox-exec` subprocess,
//! no mocks, per this project's PRD Testing Decisions. macOS-only: Seatbelt
//! is this phase's only backend.
//!
//! This crate deliberately has no `tempfile` dev-dependency (see
//! `cli_test.rs`'s own `unique_temp_dir` helper) -- `unique_temp_dir` below
//! duplicates that pattern rather than adding one.

#![cfg(target_os = "macos")]

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use rokr_tools::bash::BashTool;
use rokr_tools::Tool;
use serde_json::json;

/// Creates a fresh, uniquely-named directory under the system temp dir.
/// Mirrors `cli_test.rs`/`headless_test.rs`'s own helper of the same name.
fn unique_temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "rokr-sandbox-fixtures-{label}-{}-{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Acceptance clause 1: "a command attempting an out-of-workspace write is
/// blocked". Workspace root A, target file in sibling dir B outside it --
/// the write must fail and the target file must never exist.
#[tokio::test]
async fn out_of_workspace_write_attempt_is_blocked() {
    let workspace = unique_temp_dir("workspace-a");
    let outside = unique_temp_dir("workspace-b");
    let target = outside.join("pwned.txt");

    let tool = BashTool::new(workspace.clone());
    let command = format!("echo pwned > {}", target.to_string_lossy());
    let result = tool.execute(json!({ "command": command })).await;

    assert!(
        result.is_err(),
        "writing outside workspace_root ({}) into sibling dir ({}) should be blocked, got: {result:?}",
        workspace.display(),
        outside.display()
    );
    assert!(
        !target.exists(),
        "the out-of-workspace file must not have been created: {}",
        target.display()
    );
}

/// Acceptance clause 2: "a command attempting a network connection is
/// blocked". Seatbelt denies the `connect()`/socket syscall itself, so this
/// fails near-instantly regardless of whether the test runner actually has
/// internet access -- a short `-m` timeout keeps it robust offline too.
#[tokio::test]
async fn network_connection_attempt_is_blocked() {
    let workspace = unique_temp_dir("network-workspace");

    let tool = BashTool::new(workspace);
    let result = tool
        .execute(json!({ "command": "curl -s -m 3 http://example.com" }))
        .await;

    assert!(
        result.is_err(),
        "a network connection attempt should be blocked by the sandbox profile, got: {result:?}"
    );
}

/// Acceptance clause 3: "a command that only touches in-workspace files
/// with no network use succeeds and returns identical output to the
/// unsandboxed baseline". Baseline is a bare `sh -c` invocation of the
/// exact same command, spawned directly in this test (not through
/// `BashTool`), diffed against the sandboxed `BashTool::execute` result.
#[tokio::test]
async fn in_workspace_no_network_command_succeeds_unimpeded() {
    let workspace = unique_temp_dir("in-workspace");
    const COMMAND: &str = "echo hello-from-sandboxed-subprocess";

    let tool = BashTool::new(workspace);
    let sandboxed = tool
        .execute(json!({ "command": COMMAND }))
        .await
        .expect("an in-workspace, no-network command should succeed under the sandbox");

    let baseline = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(COMMAND)
        .output()
        .await
        .expect("baseline unsandboxed subprocess should run");
    assert!(baseline.status.success());
    let baseline_stdout = String::from_utf8_lossy(&baseline.stdout).into_owned();

    assert_eq!(
        sandboxed, baseline_stdout,
        "sandboxed output should be byte-identical to the unsandboxed baseline"
    );
}
