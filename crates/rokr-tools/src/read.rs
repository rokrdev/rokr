//! The `read` tool: reads a file's contents from the filesystem.

use serde::Deserialize;

use crate::{Tool, ToolError};

#[derive(Debug, Deserialize)]
struct ReadInput {
    path: String,
}

/// Per-file read cap, in bytes, consistent with `rokr-core::mentions`'
/// `MAX_MENTION_FILE_BYTES`. Kept as an independent local constant for now —
/// sharing the cap between the two paths is a follow-up once both are capped
/// (`rokr-tools` does not depend on `rokr-core`).
const MAX_READ_FILE_BYTES: usize = 64 * 1024;

/// Truncates `contents` to at most `cap` bytes (respecting UTF-8 char
/// boundaries), returning the (possibly truncated) body plus an optional
/// notice string to append when truncation occurred.
fn truncate_to_cap(contents: &str, cap: usize) -> (&str, Option<String>) {
    if contents.len() <= cap {
        return (contents, None);
    }

    let mut boundary = cap;
    while boundary > 0 && !contents.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let body = &contents[..boundary];
    let notice = format!(
        "\n[truncated, showing {} of {} bytes]",
        body.len(),
        contents.len()
    );
    (body, Some(notice))
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
        let (body, notice) = truncate_to_cap(&contents, MAX_READ_FILE_BYTES);
        let mut output = body.to_string();
        if let Some(notice) = notice {
            output.push_str(&notice);
        }
        Ok(output)
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

    #[tokio::test]
    async fn read_tool_truncates_output_exceeding_size_cap_with_notice() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("big.txt");
        let contents = "a".repeat(64 * 1024 + 500);
        std::fs::write(&file_path, &contents).unwrap();

        let tool = ReadTool;
        let output = tool
            .execute(json!({ "path": file_path.to_string_lossy() }))
            .await
            .expect("reading an existing oversized tempfile should succeed");

        assert!(
            output.len() < contents.len(),
            "expected output to be truncated below the original file size"
        );
        assert!(
            output.contains("truncated"),
            "expected output to contain a truncation notice"
        );
    }

    #[tokio::test]
    async fn read_tool_returns_full_contents_when_under_cap() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("small.txt");
        let contents = "b".repeat(1024);
        std::fs::write(&file_path, &contents).unwrap();

        let tool = ReadTool;
        let output = tool
            .execute(json!({ "path": file_path.to_string_lossy() }))
            .await
            .expect("reading an existing under-cap tempfile should succeed");

        assert_eq!(output, contents);
    }
}
