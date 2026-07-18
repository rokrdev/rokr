//! The `edit` tool: replaces an exact string match within an existing file.

use serde::Deserialize;

use crate::{PreviewableTool, Tool, ToolError};

#[derive(Debug, Deserialize)]
struct EditInput {
    path: String,
    old_str: String,
    new_str: String,
}

/// Replaces the first exact occurrence of `old_str` with `new_str` in the
/// file at `path`. Returns a typed error, leaving the file untouched, if
/// `old_str` does not occur in the file. Gated per
/// `docs/adr/0005-permission-model.md`: see [`PreviewableTool::preview`] for
/// the side-effect-free description shown before permission is granted.
pub struct EditTool;

impl EditTool {
    fn apply(
        contents: &str,
        old_str: &str,
        new_str: &str,
        path: &str,
    ) -> Result<String, ToolError> {
        if !contents.contains(old_str) {
            return Err(ToolError::ExecutionFailed(format!(
                "old_str not found in {path}"
            )));
        }
        Ok(contents.replacen(old_str, new_str, 1))
    }
}

impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Replace the first exact occurrence of old_str with new_str in a file."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit." },
                "old_str": { "type": "string", "description": "Exact text to find." },
                "new_str": { "type": "string", "description": "Text to replace it with." }
            },
            "required": ["path", "old_str", "new_str"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: EditInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contents = std::fs::read_to_string(&input.path)?;
        let updated = Self::apply(&contents, &input.old_str, &input.new_str, &input.path)?;
        std::fs::write(&input.path, &updated)?;
        Ok(format!("edited {}", input.path))
    }
}

impl PreviewableTool for EditTool {
    fn preview(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: EditInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contents = std::fs::read_to_string(&input.path)?;
        Self::apply(&contents, &input.old_str, &input.new_str, &input.path)?;
        Ok(format!(
            "in {}, replace:\n- {}\n+ {}",
            input.path, input.old_str, input.new_str
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::EditTool;
    use crate::{PreviewableTool, Tool};
    use serde_json::json;

    #[test]
    fn edit_preview_does_not_touch_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("greeting.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let tool = EditTool;
        let preview = tool
            .preview(json!({
                "path": file_path.to_string_lossy(),
                "old_str": "world",
                "new_str": "rokr"
            }))
            .expect("preview should succeed without touching the filesystem");

        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            contents, "hello world",
            "preview must not modify the file on disk: preview was {preview}"
        );
        assert!(preview.contains("world") && preview.contains("rokr"));
    }

    #[tokio::test]
    async fn edit_replaces_matched_text_in_file() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("greeting.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let tool = EditTool;
        tool.execute(json!({
            "path": file_path.to_string_lossy(),
            "old_str": "world",
            "new_str": "rokr"
        }))
        .await
        .expect("editing an existing match should succeed");

        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(contents, "hello rokr");
    }

    #[tokio::test]
    async fn edit_returns_typed_error_when_old_str_not_found() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("greeting.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let tool = EditTool;
        let result = tool
            .execute(json!({
                "path": file_path.to_string_lossy(),
                "old_str": "not-present",
                "new_str": "rokr"
            }))
            .await;

        assert!(
            result.is_err(),
            "editing a non-matching old_str must return a typed error, not silently succeed"
        );

        let contents = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            contents, "hello world",
            "file must be untouched when old_str is not found"
        );
    }
}
