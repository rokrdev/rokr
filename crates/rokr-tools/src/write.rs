//! The `write` tool: writes content to a file, creating or overwriting it.

use std::path::PathBuf;

use serde::Deserialize;

use crate::sandbox;
use crate::{Preview, PreviewableTool, Tool, ToolError};

#[derive(Debug, Deserialize)]
struct WriteInput {
    path: String,
    content: String,
}

/// Writes `content` to `path`, creating the file if it does not exist and
/// overwriting it if it does. Gated per `docs/adr/0005-permission-model.md`:
/// see [`PreviewableTool::preview`] for the side-effect-free description
/// shown before permission is granted.
///
/// Ticket 70 (write-edit-path-confinement): `execute` rejects any `path`
/// that resolves outside `workspace_root` with `ToolError::ExecutionFailed`
/// before touching the filesystem at all -- mirrors ticket 69's `BashTool`
/// confinement, but as a plain in-process check (`write`/`edit` call
/// `std::fs` directly, no subprocess to sandbox).
pub struct WriteTool {
    workspace_root: PathBuf,
}

impl WriteTool {
    /// Builds a `WriteTool` confined to `workspace_root`. Canonicalizes
    /// `workspace_root` here (rather than leaving it to callers) so every
    /// caller gets it for free -- see `BashTool::new`'s doc comment for why
    /// (macOS `/var` -> `/private/var` symlink resolution, mainly). Falls
    /// back to the given path unchanged if canonicalization fails (e.g. the
    /// root doesn't exist yet), rather than erroring out of construction.
    pub fn new(workspace_root: PathBuf) -> Self {
        let workspace_root = std::fs::canonicalize(&workspace_root).unwrap_or(workspace_root);
        Self { workspace_root }
    }
}

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
        if !sandbox::path_is_within_workspace(
            std::path::Path::new(&input.path),
            &self.workspace_root,
        ) {
            return Err(ToolError::ExecutionFailed(format!(
                "path outside workspace root: {}",
                input.path
            )));
        }
        std::fs::write(&input.path, &input.content)?;
        Ok(format!(
            "wrote {} bytes to {}",
            input.content.len(),
            input.path
        ))
    }
}

impl PreviewableTool for WriteTool {
    fn preview(&self, input: serde_json::Value) -> Result<Preview, ToolError> {
        let input: WriteInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let old = match std::fs::read_to_string(&input.path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ToolError::Io(e)),
        };
        Ok(Preview::Diff {
            path: input.path,
            old,
            new: input.content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::WriteTool;
    use crate::{Preview, PreviewableTool, Tool, ToolError};
    use serde_json::json;

    #[test]
    fn write_preview_does_not_touch_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("new-file.txt");
        assert!(!file_path.exists(), "precondition: file must not exist yet");

        let tool = WriteTool::new(temp.path().to_path_buf());
        let preview = tool
            .preview(json!({
                "path": file_path.to_string_lossy(),
                "content": "new content"
            }))
            .expect("preview should succeed without touching the filesystem");

        assert!(
            !file_path.exists(),
            "preview must not create the file: {preview:?}"
        );
        assert_eq!(
            preview,
            Preview::Diff {
                path: file_path.to_string_lossy().into_owned(),
                old: String::new(),
                new: "new content".to_string()
            }
        );
    }

    /// F-001: a file that exists but is unreadable (e.g. non-UTF-8 content)
    /// must not be silently treated the same as a missing file. The old
    /// implementation used `unwrap_or_default()`, which showed a false
    /// "brand new file" diff even though unreadable existing content would
    /// be clobbered by `execute` without ever being shown to the user.
    #[test]
    fn write_preview_errors_on_unreadable_existing_file_instead_of_treating_it_as_new() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("binary-file.bin");
        std::fs::write(&file_path, [0xff, 0xfe]).unwrap();

        let tool = WriteTool::new(temp.path().to_path_buf());
        let result = tool.preview(json!({
            "path": file_path.to_string_lossy(),
            "content": "new content"
        }));

        assert!(
            matches!(result, Err(ToolError::Io(_))),
            "expected Err(ToolError::Io(_)) for unreadable existing file, got {result:?}"
        );
    }

    /// Ticket 70 (write-edit-path-confinement): a `write` target that
    /// resolves outside `workspace_root` must be rejected with a typed
    /// `ExecutionFailed` error before any filesystem write happens -- not
    /// silently written, not a generic `Io` error.
    #[tokio::test]
    async fn write_execute_rejects_path_outside_workspace_root() {
        let workspace_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("pwned.txt");

        let tool = WriteTool::new(workspace_root.path().to_path_buf());
        let result = tool
            .execute(json!({
                "path": target.to_string_lossy(),
                "content": "pwned"
            }))
            .await;

        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(_))),
            "expected Err(ToolError::ExecutionFailed(_)) for out-of-workspace path, got {result:?}"
        );
        assert!(
            !target.exists(),
            "the out-of-workspace file must not have been created: {}",
            target.display()
        );
    }
}
