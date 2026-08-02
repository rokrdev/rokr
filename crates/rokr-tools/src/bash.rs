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
            // `SeatbeltSandbox::profile_for` (ticket 68) is deliberately
            // scoped to ONLY the security-relevant confinement rules --
            // write-scoping and the network deny/allow toggle -- per ADR
            // 0015: no subprocess spawning, no opinion on what a process
            // needs to merely start running under `(deny default)`. A bare
            // `(deny default)` profile blocks `sandbox-exec` from even
            // exec'ing `/bin/sh` (file-read of the binary itself is
            // denied), so running ANY command -- sandboxed or not -- needs
            // three more allowances layered on top here, none of which
            // weaken the confinement `profile_for` establishes:
            // `file-read*` (read the shell/coreutils binaries and their
            // shared libraries), `process-exec*` (actually exec them), and
            // `process-fork` (shells fork for pipelines/subshells, e.g.
            // `echo x | cat`). Verified by hand against real `sandbox-exec`
            // runs that out-of-workspace writes and network connections
            // stay blocked with these three rules present.
            let profile = SeatbeltSandbox.profile_for(&self.workspace_root, &Grants { network: false })
                + "(allow file-read*)\n(allow process-exec*)\n(allow process-fork)\n";
            tokio::process::Command::new("sandbox-exec")
                .arg("-p")
                .arg(&profile)
                .arg("sh")
                .arg("-c")
                .arg(&input.command)
                .output()
                .await?
        } else {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&input.command)
                .output()
                .await?
        };

        if !output.status.success() {
            return Err(ToolError::ExecutionFailed(format!(
                "command `{}` exited with status {}: {}",
                input.command,
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
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
}
