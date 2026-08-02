//! The `edit` tool: replaces an exact string match within an existing file.

use std::path::PathBuf;

use serde::Deserialize;

use crate::sandbox;
use crate::{Preview, PreviewableTool, Tool, ToolError};

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
///
/// Ticket 70 (write-edit-path-confinement): `execute` rejects any `path`
/// that resolves outside `workspace_root` with `ToolError::ExecutionFailed`
/// before touching the filesystem at all (gated ahead of even the initial
/// read, not just the write -- see `execute`'s doc comment below).
pub struct EditTool {
    workspace_root: PathBuf,
}

impl EditTool {
    /// Builds an `EditTool` confined to `workspace_root`. Canonicalizes
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

    /// Validates that `old_str` occurs in the file at `path` (without
    /// touching the filesystem), then returns the raw `(old_str, new_str)`
    /// pair for a permission preview to render as a partial diff. Unlike
    /// `write`'s preview (whole file before/after), `edit`'s diff-review must
    /// show only the targeted changed region, so this deliberately returns
    /// the snippet itself rather than the file's full before/after content.
    pub fn diff_snippet(
        path: &str,
        old_str: &str,
        new_str: &str,
    ) -> Result<(String, String), ToolError> {
        let contents = std::fs::read_to_string(path)?;
        Self::apply(&contents, old_str, new_str, path)?;
        Ok((old_str.to_string(), new_str.to_string()))
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
        // Gated ahead of even the initial read, not just the write: gating
        // only the write would still leak whether an out-of-workspace file
        // exists (and its error kind) via the read's error, so the
        // confinement check runs before any filesystem access at all.
        if !sandbox::path_is_within_workspace(
            std::path::Path::new(&input.path),
            &self.workspace_root,
        ) {
            return Err(ToolError::ExecutionFailed(format!(
                "path outside workspace root: {}",
                input.path
            )));
        }
        let contents = std::fs::read_to_string(&input.path)?;
        let updated = Self::apply(&contents, &input.old_str, &input.new_str, &input.path)?;
        std::fs::write(&input.path, &updated)?;
        Ok(format!("edited {}", input.path))
    }
}

impl PreviewableTool for EditTool {
    fn preview(&self, input: serde_json::Value) -> Result<Preview, ToolError> {
        let input: EditInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let (old, new) = Self::diff_snippet(&input.path, &input.old_str, &input.new_str)?;
        Ok(Preview::Diff {
            path: input.path,
            old,
            new,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::EditTool;
    use crate::{Preview, PreviewableTool, Tool, ToolError};
    use serde_json::json;

    #[test]
    fn edit_preview_does_not_touch_filesystem() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("greeting.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let tool = EditTool::new(temp.path().to_path_buf());
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
            "preview must not modify the file on disk: preview was {preview:?}"
        );
        assert_eq!(
            preview,
            Preview::Diff {
                path: file_path.to_string_lossy().into_owned(),
                old: "world".to_string(),
                new: "rokr".to_string()
            }
        );
    }

    #[test]
    fn edit_preview_shows_targeted_replacement_only() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("multi.txt");
        std::fs::write(&file_path, "line1\nline2\nline3\n").unwrap();

        let (old, new) =
            EditTool::diff_snippet(&file_path.to_string_lossy(), "line2", "replaced_line2")
                .expect("diff_snippet should succeed when old_str is present in the file");

        assert_eq!(old, "line2");
        assert_eq!(new, "replaced_line2");
        assert!(
            !old.contains("line1") && !old.contains("line3"),
            "diff snippet must contain only the targeted region, not unrelated file lines, got old: {old:?}"
        );
        assert!(
            !new.contains("line1") && !new.contains("line3"),
            "diff snippet must contain only the targeted region, not unrelated file lines, got new: {new:?}"
        );
    }

    #[tokio::test]
    async fn edit_replaces_matched_text_in_file() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("greeting.txt");
        std::fs::write(&file_path, "hello world").unwrap();

        let tool = EditTool::new(temp.path().to_path_buf());
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

        let tool = EditTool::new(temp.path().to_path_buf());
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

    /// Ticket 70 (write-edit-path-confinement): an `edit` target that
    /// resolves outside `workspace_root` must be rejected with a typed
    /// `ExecutionFailed` error before any filesystem access happens -- the
    /// file is pre-created with known content so a "file not found" error
    /// can't be mistaken for the confinement check firing, and the content
    /// must remain untouched.
    #[tokio::test]
    async fn edit_execute_rejects_path_outside_workspace_root() {
        let workspace_root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("greeting.txt");
        std::fs::write(&target, "original content").unwrap();

        let tool = EditTool::new(workspace_root.path().to_path_buf());
        let result = tool
            .execute(json!({
                "path": target.to_string_lossy(),
                "old_str": "original",
                "new_str": "changed"
            }))
            .await;

        assert!(
            matches!(result, Err(ToolError::ExecutionFailed(_))),
            "expected Err(ToolError::ExecutionFailed(_)) for out-of-workspace path, got {result:?}"
        );

        let contents = std::fs::read_to_string(&target).unwrap();
        assert_eq!(
            contents, "original content",
            "file outside workspace root must be untouched by the confinement check"
        );
    }
}
