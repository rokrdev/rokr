//! Hook event/payload types and the subprocess executor (ticket 49,
//! hooks-tracer-bullet; PRD `phase-6-mcp-hooks.md`, "Hooks"). Deliberately
//! free of any dependency on `rokr-core` or any other `rokr` crate (see
//! `Cargo.toml`'s doc comment and `docs/adr/0012-hooks-execution-trust-model.md`)
//! -- this crate only knows how to describe a hook event as JSON and run a
//! hook command against it; `crates/rokr/src/main.rs` is what bridges its
//! [`HookResult`] into `rokr-core`'s `PreToolHookOutcome` seam.

use std::process::Stdio;
use std::time::Duration;

/// The lifecycle moment a hook attaches to (PRD "Hooks", v1 event list).
/// Only [`HookPayload::PreToolUse`] is wired end-to-end by this ticket (49);
/// the rest are named now so ticket 50 (hooks-remaining-events-and-config)
/// has a stable, exhaustive set to build its `hooks` config schema against
/// instead of growing this enum's call sites piecemeal later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    UserPromptSubmit,
    Stop,
    SessionEnd,
}

/// The JSON payload delivered on a hook's STDIN -- never string-interpolated
/// into the hook's command line itself (the injection guard; see
/// [`execute_hook`]'s doc comment and the ADR). `#[serde(tag = "event")]`
/// puts the event's name in the payload as its own top-level `"event"`
/// field, alongside whichever fields that event variant carries, so a hook
/// script can dispatch on `.event` without rokr needing a second envelope
/// type. Only the `PreToolUse` variant is populated by this ticket; shaped
/// as an enum (rather than one flat struct with optional fields) so ticket
/// 50 can add a variant per remaining v1 event additively, without touching
/// this one's fields.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "event")]
pub enum HookPayload {
    PreToolUse {
        tool_name: String,
        tool_input: serde_json::Value,
    },
}

/// Outcome of running a hook subprocess to completion (or timing out),
/// covering every branch of the PRD's exit-code contract:
/// - exit `0` -> [`HookResult::Success`] (`stdout`, for `UserPromptSubmit`/
///   `SessionStart` context injection in a later ticket; unused by
///   `PreToolUse`).
/// - exit `2` -> [`HookResult::Blocked`] (blocking veto; `stderr` is the
///   message to surface, mirroring an interactive permission rejection).
/// - any other nonzero exit, a signal, or a timeout -> [`HookResult::NonBlockingFailure`]
///   (`message` describes what happened; the caller should surface it but
///   let the loop continue exactly as if the hook hadn't run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookResult {
    Success { stdout: String },
    Blocked { stderr: String },
    NonBlockingFailure { message: String },
}

/// Default per-hook timeout (PRD "Hooks": "60 seconds by default per hook
/// invocation, configurable per hook entry"). The "configurable per hook"
/// half of that sentence is ticket 50's `hooks` config schema (a
/// `timeout_ms` field per entry); this ticket only wires the default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Whether `matcher` (a glob against the tool name, PRD "Hooks" matcher
/// shape) matches `tool_name`. Only `"*"` (match everything) and an exact
/// literal match are implemented -- a stub for ticket 50
/// (hooks-remaining-events-and-config), which is expected to replace this
/// with real glob syntax (`bash*`, `mcp__*`, ...) once the `hooks` config
/// schema exists to drive it. Not exercised by this ticket's tests (no
/// config/matcher wiring lands until ticket 50); kept minimal rather than
/// building glob matching nothing calls yet.
pub fn matches_tool_name(matcher: &str, tool_name: &str) -> bool {
    matcher == "*" || matcher == tool_name
}

/// Runs `command` as a shell command (`sh -c command` -- never `sh -c
/// "{command} {payload}"` or similar) with `payload` serialized to JSON and
/// piped to the subprocess's STDIN. This is the whole injection guard
/// (`docs/adr/0012-hooks-execution-trust-model.md`): `payload` NEVER
/// touches `command`'s text, so a `tool_input` value containing shell
/// metacharacters (`;`, `` ` ``, `$(...)`, ...) can never execute as part of
/// the hook's command line, no matter what the model or an MCP server
/// produced.
///
/// Interprets the exit code per the PRD's exit-code contract -- see
/// [`HookResult`]'s doc comment for the full mapping. A hook that runs
/// longer than `timeout` is killed (never left to hang the caller) and
/// treated as [`HookResult::NonBlockingFailure`], the same as any other
/// non-blocking failure.
pub async fn execute_hook(command: &str, payload: &HookPayload, timeout: Duration) -> HookResult {
    let payload_json = match serde_json::to_vec(payload) {
        Ok(bytes) => bytes,
        Err(err) => {
            return HookResult::NonBlockingFailure {
                message: format!("failed to serialize hook payload: {err}"),
            }
        }
    };

    let mut child = match tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            return HookResult::NonBlockingFailure {
                message: format!("failed to spawn hook command: {err}"),
            }
        }
    };

    // Written on a separate task, concurrently with reading the child's
    // output below, rather than sequentially before it -- a hook that
    // starts emitting stdout/stderr before it has finished reading stdin
    // could otherwise deadlock against pipe buffer limits with a
    // sequential write-then-read. Not joined: if the child is killed by
    // the timeout below, the write simply fails (broken pipe) and this
    // task ends on its own; nothing downstream depends on its result.
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(&payload_json).await;
            // `stdin` drops here (task end), closing the pipe so the hook
            // sees EOF on its stdin -- the signal that the JSON payload is
            // complete.
        });
    }

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(err)) => {
            return HookResult::NonBlockingFailure {
                message: format!("failed waiting for hook process: {err}"),
            }
        }
        Err(_elapsed) => {
            // `wait_with_output` owns `child` by value; dropping this whole
            // future on timeout drops that owned `child` too, and
            // `kill_on_drop(true)` above makes THAT send a kill -- the hook
            // process is reaped, never left to hang the caller.
            return HookResult::NonBlockingFailure {
                message: format!("hook timed out after {timeout:?}"),
            };
        }
    };

    match output.status.code() {
        Some(0) => HookResult::Success {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        },
        Some(2) => HookResult::Blocked {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Some(code) => HookResult::NonBlockingFailure {
            message: format!(
                "hook exited with status {code} (stderr: {})",
                String::from_utf8_lossy(&output.stderr)
            ),
        },
        None => HookResult::NonBlockingFailure {
            message: "hook process terminated by signal".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a path (not created on disk) under the system temp dir,
    /// unique per test run, used as a side-effect marker: a test writes
    /// shell metacharacters that WOULD create this file if ever executed as
    /// part of a command line, then asserts it was never created.
    fn unique_temp_marker(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rokr-hooks-test-{label}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[tokio::test]
    async fn hook_exit_2_is_treated_as_blocking_and_stderr_becomes_error_result() {
        let payload = HookPayload::PreToolUse {
            tool_name: "bash".to_string(),
            tool_input: serde_json::json!({"command": "rm -rf /"}),
        };

        let result = execute_hook(
            "cat >/dev/null; echo 'blocked: dangerous command' >&2; exit 2",
            &payload,
            DEFAULT_TIMEOUT,
        )
        .await;

        match result {
            HookResult::Blocked { stderr } => {
                assert!(
                    stderr.contains("blocked: dangerous command"),
                    "expected the hook's stderr in the Blocked result, got: {stderr:?}"
                );
            }
            other => panic!("expected HookResult::Blocked for exit 2, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn hook_payload_delivered_as_json_on_stdin_never_interpolated_into_command_line() {
        let marker = unique_temp_marker("no-interpolation");
        let malicious_note = format!("; touch {}; echo pwned", marker.display());
        let malicious_input = serde_json::json!({ "note": malicious_note });
        let payload = HookPayload::PreToolUse {
            tool_name: "bash".to_string(),
            tool_input: malicious_input.clone(),
        };

        // `cat` just echoes whatever arrives on its stdin back to stdout --
        // if the executor ever built the command line by interpolating the
        // payload into it (instead of piping JSON on stdin), the shell
        // metacharacters embedded in `malicious_input` above would execute
        // as part of the command itself; `cat` alone never touches the
        // filesystem, so `marker`'s existence below is a direct signal of
        // whether interpolation happened.
        let result = execute_hook("cat", &payload, DEFAULT_TIMEOUT).await;

        match result {
            HookResult::Success { stdout } => {
                let parsed: serde_json::Value = serde_json::from_str(&stdout)
                    .unwrap_or_else(|err| {
                        panic!("hook stdout should be the JSON payload echoed back, got {stdout:?}: {err}")
                    });
                assert_eq!(parsed["event"], "PreToolUse");
                assert_eq!(parsed["tool_name"], "bash");
                assert_eq!(parsed["tool_input"], malicious_input);
            }
            other => panic!("expected HookResult::Success echoing the JSON payload, got {other:?}"),
        }

        assert!(
            !marker.exists(),
            "payload content must never be interpolated into the command line -- the embedded \
             shell metacharacters must not have executed"
        );
    }

    #[tokio::test]
    async fn hook_exceeding_timeout_treated_as_non_blocking_failure_without_hanging() {
        let payload = HookPayload::PreToolUse {
            tool_name: "bash".to_string(),
            tool_input: serde_json::json!({}),
        };

        let started = std::time::Instant::now();
        let result = execute_hook(
            "cat >/dev/null; sleep 30; exit 0",
            &payload,
            Duration::from_millis(200),
        )
        .await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_secs(5),
            "execute_hook must return promptly once its timeout elapses, not hang until the \
             hook process exits on its own; took {elapsed:?}"
        );
        match result {
            HookResult::NonBlockingFailure { message } => {
                assert!(
                    message.to_lowercase().contains("timeout")
                        || message.to_lowercase().contains("timed out"),
                    "expected the failure message to mention the timeout, got: {message:?}"
                );
            }
            other => panic!("expected HookResult::NonBlockingFailure on timeout, got {other:?}"),
        }
    }
}
