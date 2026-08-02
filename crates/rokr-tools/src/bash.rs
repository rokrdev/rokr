//! The `bash` tool: runs a shell command as a subprocess and captures its output.

use std::path::PathBuf;

use serde::Deserialize;

use crate::sandbox::{Grants, Sandbox, SeatbeltSandbox};
use crate::{Preview, PreviewableTool, Tool, ToolError};

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
}

/// Runs `command` in a shell (`sh -c`) as a real subprocess and returns its
/// captured stdout. Gated per `docs/adr/0005-permission-model.md`: see
/// [`PreviewableTool::preview`] for the side-effect-free description shown
/// before permission is granted.
///
/// On macOS the subprocess is wrapped through `sandbox-exec` with a
/// `SeatbeltSandbox` profile confined to `workspace_root` (ticket 69,
/// `docs/adr/0015-sandbox-trait-and-seatbelt-backend.md`): network is always
/// denied (ADR 0015 decision 3 -- no grant knob exists for it yet). On other
/// platforms `execute` falls back to the pre-ticket-69 unsandboxed behavior;
/// tracked as a known gap for this phase, not fixed here.
pub struct BashTool {
    workspace_root: PathBuf,
}

impl BashTool {
    /// Builds a `BashTool` confined to `workspace_root`. Canonicalizes
    /// `workspace_root` here (rather than leaving it to callers) so every
    /// caller gets it for free: Seatbelt's `(subpath "...")` profile rule
    /// matches on the resolved real path, and on macOS `workspace_root`
    /// candidates can arrive as symlinks (e.g. `std::env::temp_dir()`'s
    /// `/var/...` is a symlink to `/private/var/...`) -- an uncanonicalized
    /// root would make in-workspace writes spuriously fail. Falls back to
    /// the given path unchanged if canonicalization fails (e.g. the root
    /// doesn't exist yet), rather than erroring out of construction.
    pub fn new(workspace_root: PathBuf) -> Self {
        let workspace_root =
            std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
        Self { workspace_root }
    }
}

impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command and return its captured stdout."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to run." }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: BashInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        // Ticket 69: on macOS, wrap the subprocess through `sandbox-exec`
        // with a `SeatbeltSandbox` profile confined to `workspace_root`.
        // Network is always denied -- ADR 0015 decision 3, deliberate, no
        // grant knob exists for it yet. Non-macOS falls back to the bare
        // `sh -c` invocation: Seatbelt is this phase's only backend, so
        // there's no confinement on other platforms yet (known gap, not
        // fixed here). `cfg!()` rather than `#[cfg(...)]` on the whole
        // method so both branches are always compiled and checked -- no
        // dead-code path, no per-platform duplication of the
        // error-formatting/stdout-capture tail below.
        let output = if cfg!(target_os = "macos") {
            // F-006: `SeatbeltSandbox::profile_for`'s own output is now a
            // COMPLETE, runnable profile (exec-enabling rules included) --
            // no more string concatenation at this call site.
            let profile =
                SeatbeltSandbox.profile_for(&self.workspace_root, &Grants { network: false })?;
            tokio::process::Command::new("sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("sh")
                .arg("-c")
                .arg(&input.command)
                .output()
                .await
                // F-007: a failure to even SPAWN `sandbox-exec` (not
                // installed / not on `PATH`) must not silently fall through
                // to unsandboxed execution -- that would drop every
                // confinement guarantee this whole module exists for.
                // Refusing to run at all is the safe failure mode.
                .map_err(map_sandbox_exec_spawn_error)?
        } else {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&input.command)
                .output()
                .await?
        };

        if !output.status.success() {
            let mut message = format!(
                "command `{}` exited with status {}: {}",
                input.command,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            // F-007: on macOS, a sandboxed command's failure is otherwise
            // indistinguishable from an ordinary command failure -- confusing
            // when debugging (e.g. "did my command have a bug, or did the
            // sandbox block it?"). Names both the confinement boundary and
            // the word "sandbox" explicitly.
            if cfg!(target_os = "macos") {
                message.push_str(&format!(
                    " (ran under the rokr sandbox: writes confined to {}, network denied)",
                    self.workspace_root.display()
                ));
            }
            return Err(ToolError::ExecutionFailed(message));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

fn map_sandbox_exec_spawn_error(err: std::io::Error) -> ToolError {
    // Trivial cleanup (post-round-1 re-critique): ONLY `NotFound` actually
    // means "sandbox-exec not available" -- any other spawn error kind
    // (e.g. permission denied) means something else went wrong, and
    // swallowing it into the generic "not available" message would hide
    // the real cause from the caller. `NotFound` alone gets the
    // specific/actionable message; every other kind passes the real
    // `io::Error` through in its own message instead.
    if err.kind() == std::io::ErrorKind::NotFound {
        ToolError::ExecutionFailed(
            "sandbox-exec not available; refusing to run unsandboxed".to_string(),
        )
    } else {
        ToolError::ExecutionFailed(format!("failed to spawn sandbox-exec: {err}"))
    }
}

impl PreviewableTool for BashTool {
    fn preview(&self, input: serde_json::Value) -> Result<Preview, ToolError> {
        let input: BashInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        Ok(Preview::Command(input.command))
    }
}

#[cfg(test)]
mod tests {
    use super::BashTool;
    use crate::{Preview, PreviewableTool, Tool};
    use serde_json::json;

    #[test]
    fn bash_preview_does_not_spawn_a_process() {
        let temp = tempfile::tempdir().unwrap();
        let marker_path = temp.path().join("side-effect-marker");
        let command = format!("touch {}", marker_path.to_string_lossy());

        let tool = BashTool::new(temp.path().to_path_buf());
        let preview = tool
            .preview(json!({ "command": command }))
            .expect("preview should succeed without spawning a process");

        assert!(
            !marker_path.exists(),
            "preview must not execute the command: {preview:?}"
        );
        assert_eq!(
            preview,
            Preview::Command(command),
            "preview should describe the literal command"
        );
    }

    #[tokio::test]
    async fn bash_executes_real_subprocess_and_returns_stdout() {
        let temp = tempfile::tempdir().unwrap();
        let tool = BashTool::new(temp.path().to_path_buf());

        let output = tool
            .execute(json!({ "command": "echo hello-from-subprocess" }))
            .await
            .expect("running a real, harmless subprocess should succeed");

        assert_eq!(output.trim(), "hello-from-subprocess");
    }

    /// Ticket 69: proves `execute` actually routes the subprocess through
    /// `sandbox-exec`, not just that it "works" -- a command that attempts
    /// to write outside `workspace_root` must be blocked by the Seatbelt
    /// profile. Against the pre-ticket-69 bare `sh -c` implementation this
    /// write would succeed (both the `Err` assertion and the file-absence
    /// assertion below would fail), which is what makes this a real RED,
    /// not a tautology.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bash_execute_wraps_command_through_sandbox_exec_profile() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("pwned.txt");

        let tool = BashTool::new(workspace.path().to_path_buf());
        let command = format!("echo pwned > {}", target.to_string_lossy());
        let result = tool.execute(json!({ "command": command })).await;

        assert!(
            result.is_err(),
            "writing outside workspace_root should be blocked by the sandbox profile, got: {result:?}"
        );
        assert!(
            !target.exists(),
            "the out-of-workspace file must not have been created: {}",
            target.display()
        );
    }

    /// F-007 (pre-ship review, minor): a sandbox denial must be
    /// distinguishable from an ordinary command failure in the error text --
    /// names both the workspace root the command was confined to and the
    /// word "sandbox".
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn bash_execute_sandbox_denial_error_names_workspace_root_and_sandbox() {
        let workspace = tempfile::tempdir().unwrap();
        let canonical_workspace = workspace.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("pwned.txt");

        let tool = BashTool::new(workspace.path().to_path_buf());
        let command = format!("echo pwned > {}", target.to_string_lossy());
        let result = tool.execute(json!({ "command": command })).await;

        let crate::ToolError::ExecutionFailed(message) =
            result.expect_err("writing outside workspace_root should be blocked")
        else {
            panic!("expected ToolError::ExecutionFailed");
        };
        assert!(
            message.contains(&canonical_workspace.to_string_lossy().into_owned()),
            "expected the error message to name the workspace root {canonical_workspace:?}, got: {message}"
        );
        assert!(
            message.to_lowercase().contains("sandbox"),
            "expected the error message to mention 'sandbox', got: {message}"
        );
    }

    /// F-007: a failure to even SPAWN `sandbox-exec` (e.g. not installed /
    /// not on `PATH`) must be mapped to an explicit, actionable error
    /// naming `sandbox-exec` -- NOT silently fall through to unsandboxed
    /// execution, and not a generic `Io` error either. Exercises the pure
    /// mapping function directly (rather than trying to make the real
    /// `sandbox-exec` binary disappear from a real macOS system, which
    /// isn't something a test can safely arrange) with a synthetic
    /// `NotFound` error matching exactly what `execvp` reports when a
    /// binary isn't on `PATH`.
    #[cfg(target_os = "macos")]
    #[test]
    fn map_sandbox_exec_spawn_error_names_sandbox_exec_explicitly() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");

        let result = super::map_sandbox_exec_spawn_error(io_err);

        match result {
            crate::ToolError::ExecutionFailed(message) => {
                assert!(
                    message.contains("sandbox-exec"),
                    "expected the error to explicitly name sandbox-exec, got: {message}"
                );
            }
            other => panic!("expected ToolError::ExecutionFailed, got {other:?}"),
        }
    }

    /// Trivial cleanup (post-round-1 re-critique): a spawn error kind OTHER
    /// than `NotFound` (e.g. permission denied) must NOT be collapsed into
    /// the generic "not available" message -- that message specifically
    /// means "the binary isn't there," which isn't what happened. The real
    /// `io::Error` must still be visible in the mapped message instead.
    #[cfg(target_os = "macos")]
    #[test]
    fn map_sandbox_exec_spawn_error_passes_through_non_not_found_errors() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied");

        let result = super::map_sandbox_exec_spawn_error(io_err);

        match result {
            crate::ToolError::ExecutionFailed(message) => {
                assert!(
                    !message.contains("not available"),
                    "a non-NotFound spawn error must not be reported as \"not available\", \
                     got: {message}"
                );
                assert!(
                    message.contains("permission denied"),
                    "expected the real io::Error text to be visible in the mapped message, \
                     got: {message}"
                );
            }
            other => panic!("expected ToolError::ExecutionFailed, got {other:?}"),
        }
    }
}
