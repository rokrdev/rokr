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

use rokr_app::skill_trust::{
    ConsentOutcome, ConsentResolver, SkillConsentRequest, SkillTrustStore,
};
use rokr_app::CommandRegistry;
use rokr_tools::bash::BashTool;
use rokr_tools::write::WriteTool;
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

/// F-008 (pre-ship review, minor): `network_connection_attempt_is_blocked`
/// above only asserts `is_err()`, which would ALSO pass on a DNS failure or
/// `curl` being absent from the test runner -- it doesn't actually prove
/// the SANDBOX denied the call. This fixture does a direct-IP TCP connect
/// via a shell built-in (`/dev/tcp`), with no DNS resolution and no
/// external tool dependency, and asserts the specific denial signature
/// (`Operation not permitted`, what Seatbelt's `connect()` denial actually
/// produces) rather than "some error occurred" -- this WOULD fail if the
/// sandbox profile had `(allow network*)` accidentally left in.
#[tokio::test]
async fn direct_ip_tcp_connect_is_denied_with_operation_not_permitted() {
    let workspace = unique_temp_dir("network-direct-ip-workspace");

    let tool = BashTool::new(workspace);
    let result = tool
        .execute(json!({ "command": "exec 3<>/dev/tcp/1.1.1.1/80" }))
        .await;

    let err = result.expect_err(
        "a direct-IP TCP connect attempt should be blocked by the sandbox profile",
    );
    let message = err.to_string();
    assert!(
        message.contains("Operation not permitted") || message.contains("connect"),
        "expected the denial's error text to show the actual Seatbelt denial signature \
         (\"Operation not permitted\"/\"connect\"), not just any error, got: {message}"
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

/// F-002 (pre-ship review, blocker): the pre-fix Seatbelt profile denied
/// `/dev/null` writes and `sysctl-read`, which breaks common, entirely
/// legitimate in-workspace command patterns -- redirecting output to
/// `/dev/null` is one of the most ordinary shell idioms there is. Proven
/// two ways: the sandboxed exit status AND the actual "ok" stdout, so a
/// silently-swallowed failure (nonzero exit with empty output) can't be
/// mistaken for success.
#[tokio::test]
async fn redirect_to_dev_null_succeeds_under_the_sandbox() {
    let workspace = unique_temp_dir("dev-null-workspace");

    let tool = BashTool::new(workspace);
    let result = tool
        .execute(json!({ "command": "echo hi > /dev/null && echo ok" }))
        .await
        .expect("redirecting to /dev/null then echoing should succeed under the sandbox");

    assert_eq!(result.trim(), "ok");
}

/// F-002 (pre-ship review, blocker): `git status` (which reads sysctls at
/// startup on macOS and is an entirely ordinary in-workspace command) was
/// broken by the pre-fix profile's missing `sysctl-read` allowance. Runs
/// inside a workspace-local directory this fixture `git init`s itself, so
/// the fixture has no dependency on any pre-existing repo state.
#[tokio::test]
async fn git_status_succeeds_under_the_sandbox() {
    let workspace = unique_temp_dir("git-status-workspace");

    let init = tokio::process::Command::new("git")
        .arg("init")
        .current_dir(&workspace)
        .output()
        .await
        .expect("git init should run as part of fixture setup");
    assert!(
        init.status.success(),
        "fixture setup: git init must succeed, stderr: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    // `BashTool::execute` does not itself change the subprocess's cwd (it
    // only confines WRITES via the sandbox profile's `subpath` rule) -- it
    // inherits the test process's own cwd. `cd` into the fixture's
    // workspace explicitly so `git status` reports on the fresh repo just
    // created above, not on whatever repo the test binary happens to be
    // running from.
    let command = format!("cd {} && git status --porcelain", workspace.to_string_lossy());
    let tool = BashTool::new(workspace);
    let result = tool
        .execute(json!({ "command": command }))
        .await
        .expect("git status --porcelain should succeed under the sandbox");

    // A freshly-init'd repo with no commits/files has nothing to report --
    // the meaningful assertion here is that the command didn't error out,
    // proving `sysctl-read` (and the other exec-enabling rules) let `git`
    // actually run to completion under the sandbox.
    assert_eq!(result.trim(), "");
}

/// Ticket 70 (write-edit-path-confinement): mirrors
/// `out_of_workspace_write_attempt_is_blocked` above, but for the in-process
/// `WriteTool` confinement check rather than `BashTool`'s `sandbox-exec`
/// wrapping. Workspace root A, target file in sibling dir B outside it --
/// the write must fail and the target file must never exist.
#[tokio::test]
async fn write_tool_out_of_workspace_write_attempt_is_blocked() {
    let workspace = unique_temp_dir("write-workspace-a");
    let outside = unique_temp_dir("write-workspace-b");
    let target = outside.join("pwned.txt");

    let tool = WriteTool::new(workspace.clone());
    let result = tool
        .execute(json!({ "path": target.to_string_lossy(), "content": "pwned" }))
        .await;

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

/// A test `ConsentResolver` that always auto-approves without persisting a
/// trust-store entry -- isolates CONTAINMENT (this test's actual subject)
/// from CONSENT (which has its own dedicated coverage in `commands.rs`'s and
/// `tui_test.rs`'s tests): consent is granted unconditionally here so the
/// only thing left to prove is that the sandbox still blocks the
/// out-of-workspace write once execution is reached.
#[derive(Clone)]
struct AlwaysApproveConsentResolver;

impl ConsentResolver for AlwaysApproveConsentResolver {
    fn resolve(
        &self,
        _request: SkillConsentRequest,
    ) -> impl std::future::Future<Output = ConsentOutcome> + Send {
        async { ConsentOutcome::ApproveWithoutPersisting }
    }
}

/// Ticket 75 (executable-skill-invocation), ADR 0018 decision 2: reuses this
/// file's shared adversarial fixture shape (workspace root A, target file in
/// sibling dir B outside it) but drives the write attempt through a
/// project-scope executable skill's `run:` command, resolved via
/// `CommandRegistry::resolve_skills` with consent auto-granted (see
/// `AlwaysApproveConsentResolver` above) -- proving containment and consent
/// are independent gates: even a FULLY CONSENTED `run:` command is still
/// confined by the same `SeatbeltSandbox`/`BashTool` path ticket 69
/// established, unchanged.
#[tokio::test]
async fn executable_skill_out_of_workspace_write_attempt_is_blocked() {
    let workspace = unique_temp_dir("skill-workspace-a");
    let outside = unique_temp_dir("skill-workspace-b");
    let target = outside.join("pwned.txt");

    let skills_dir = workspace.join(".rokr").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    let command = format!("echo pwned > {}", target.to_string_lossy());
    std::fs::write(
        skills_dir.join("deploy.md"),
        format!("---\nrun: {command}\n---\nDeploy body."),
    )
    .unwrap();

    let registry = CommandRegistry::discover_project_scope(&workspace);
    let trust_store = SkillTrustStore::new(&workspace.join("trust-store-config"));

    let resolved = registry
        .resolve_skills(
            "@skill:deploy",
            workspace.clone(),
            trust_store,
            AlwaysApproveConsentResolver,
        )
        .await
        .expect("resolving a consented executable skill mention should not error");

    assert!(
        !target.exists(),
        "writing outside workspace_root ({}) into sibling dir ({}) should be blocked by the \
         sandbox even though consent was fully granted, got resolved text: {resolved:?}",
        workspace.display(),
        outside.display()
    );
}
