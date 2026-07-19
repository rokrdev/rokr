//! The `bash` tool: runs a shell command as a subprocess and captures its output.

use serde::Deserialize;

use crate::{Preview, PreviewableTool, Tool, ToolError};

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
}

/// Runs `command` in a shell (`sh -c`) as a real subprocess and returns its
/// captured stdout. Gated per `docs/adr/0005-permission-model.md`: see
/// [`PreviewableTool::preview`] for the side-effect-free description shown
/// before permission is granted.
pub struct BashTool;

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

        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&input.command)
            .output()
            .await?;

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

        let tool = BashTool;
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
        let tool = BashTool;

        let output = tool
            .execute(json!({ "command": "echo hello-from-subprocess" }))
            .await
            .expect("running a real, harmless subprocess should succeed");

        assert_eq!(output.trim(), "hello-from-subprocess");
    }
}
