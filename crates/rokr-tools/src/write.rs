//! The `write` tool: writes content to a file, creating or overwriting it.

use serde::Deserialize;

use crate::{PreviewableTool, Tool, ToolError};

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

/// Writes `content` to `path`, creating the file if it does not exist and
/// overwriting it if it does. Gated per `docs/adr/0005-permission-model.md`:
/// see [`PreviewableTool::preview`] for the side-effect-free description
/// shown before permission is granted.
pub struct WriteTool;

impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Write content to a file, creating or overwriting it."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write." },
                "content": { "type": "string", "description": "Content to write to the file." }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: WriteInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        std::fs::write(&input.path, &input.content)?;
        Ok(format!(
            "wrote {} bytes to {}",
            input.content.len(),
            input.path
        ))
    }
}

impl PreviewableTool for WriteTool {
    fn preview(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: WriteInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        Ok(format!(
            "write {} bytes to {}:\n{}",
            input.content.len(),
            input.path,
            input.content
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::WriteTool;
    use crate::PreviewableTool;
    use serde_json::json;

    #[test]
    fn write_preview_does_not_touch_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("new-file.txt");
        assert!(!file_path.exists(), "precondition: file must not exist yet");

        let tool = WriteTool;
        let preview = tool
            .preview(json!({
                "path": file_path.to_string_lossy(),
                "content": "new content"
            }))
            .expect("preview should succeed without touching the filesystem");

        assert!(
            !file_path.exists(),
            "preview must not create the file: {preview}"
        );
        assert!(
            preview.contains("new content"),
            "preview should describe the content that would be written: {preview}"
        );
    }
}
