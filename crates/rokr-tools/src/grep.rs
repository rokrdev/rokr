//! The `grep` tool: searches a file's lines for a literal substring.

use serde::Deserialize;

use crate::{Tool, ToolError};

#[derive(Debug, Deserialize)]
struct GrepInput {
    pattern: String,
    path: String,
}

/// Searches `path` for lines containing the literal substring `pattern`,
/// returning matching lines prefixed with their 1-based line number.
pub struct GrepTool;

impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search a file's lines for a literal substring."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Literal substring to search for." },
                "path": { "type": "string", "description": "File to search." }
            },
            "required": ["pattern", "path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: GrepInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let contents = std::fs::read_to_string(&input.path)?;

        let matches: Vec<String> = contents
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(&input.pattern))
            .map(|(i, line)| format!("{}:{}", i + 1, line))
            .collect();

        Ok(matches.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::GrepTool;
    use crate::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn grep_returns_matching_lines() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("log.txt");
        std::fs::write(&file_path, "line one\nline two has needle\nline three\n").unwrap();

        let tool = GrepTool;
        let output = tool
            .execute(json!({
                "pattern": "needle",
                "path": file_path.to_string_lossy()
            }))
            .await
            .expect("grepping a real tempfile should succeed");

        assert!(output.contains("line two has needle"));
        assert!(!output.contains("line one"));
        assert!(!output.contains("line three"));
    }

    #[tokio::test]
    async fn grep_returns_typed_error_for_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist.txt");

        let tool = GrepTool;
        let result = tool
            .execute(json!({ "pattern": "needle", "path": missing.to_string_lossy() }))
            .await;

        assert!(
            result.is_err(),
            "grepping a missing file must return a typed error"
        );
    }
}
