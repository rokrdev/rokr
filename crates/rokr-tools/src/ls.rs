//! The `ls` tool: lists the entries of a directory.

use serde::Deserialize;

use crate::{Tool, ToolError};

#[derive(Debug, Deserialize)]
struct LsInput {
    path: String,
}

/// Lists the entries of a directory, one per line. Directory entries are
/// suffixed with `/`.
pub struct LsTool;

impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "ls"
    }

    fn description(&self) -> &'static str {
        "List the entries of a directory."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: LsInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&input.path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                entries.push(format!("{name}/"));
            } else {
                entries.push(name);
            }
        }
        entries.sort();

        Ok(entries.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::LsTool;
    use crate::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn ls_lists_directory_entries() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("a.txt"), "a").unwrap();
        std::fs::write(temp.path().join("b.txt"), "b").unwrap();
        std::fs::create_dir(temp.path().join("subdir")).unwrap();

        let tool = LsTool;
        let output = tool
            .execute(json!({ "path": temp.path().to_string_lossy() }))
            .await
            .expect("listing a real tempdir should succeed");

        assert!(output.contains("a.txt"));
        assert!(output.contains("b.txt"));
        assert!(output.contains("subdir"));
    }

    #[tokio::test]
    async fn ls_returns_typed_error_for_missing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");

        let tool = LsTool;
        let result = tool
            .execute(json!({ "path": missing.to_string_lossy() }))
            .await;

        assert!(
            result.is_err(),
            "listing a missing directory must return a typed error"
        );
    }
}
