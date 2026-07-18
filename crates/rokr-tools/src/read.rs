//! The `read` tool: reads a file's contents from the filesystem.

use serde::Deserialize;

use crate::{Tool, ToolError};

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
}

/// Reads the full contents of a file as UTF-8 text.
pub struct ReadTool;

impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read the full contents of a file as UTF-8 text."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: ReadInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contents = std::fs::read_to_string(&input.path)?;
        Ok(contents)
    }
}

#[cfg(test)]
mod tests {
    use super::ReadTool;
    use crate::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn read_returns_file_contents() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("hello.txt");
        std::fs::write(&file_path, "hello from disk").unwrap();

        let tool = ReadTool;
        let output = tool
            .execute(json!({ "path": file_path.to_string_lossy() }))
            .await
            .expect("reading an existing tempfile should succeed");

        assert_eq!(output, "hello from disk");
    }
}
