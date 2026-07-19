//! The `glob` tool: lists files under a directory whose names match a wildcard pattern.

use serde::Deserialize;

use crate::{Tool, ToolError};

#[derive(Debug, Deserialize)]
struct GlobInput {
    path: String,
    pattern: String,
}

/// Lists the direct entries of `path` whose names match `pattern`.
/// `pattern` supports `*` (matches any sequence of characters, including
/// none); no other wildcard syntax is supported.
pub struct GlobTool;

/// Matches `name` against `pattern`, where `*` matches any run of
/// characters (including empty). Classic greedy two-pointer wildcard match.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();

    let (mut pi, mut ni) = (0, 0);
    let (mut star_idx, mut match_idx) = (None, 0);

    while ni < name.len() {
        if pi < pattern.len() && (pattern[pi] == '*' || pattern[pi] == name[ni]) {
            if pattern[pi] == '*' {
                star_idx = Some(pi);
                match_idx = ni;
                pi += 1;
            } else {
                pi += 1;
                ni += 1;
            }
        } else if let Some(s) = star_idx {
            pi = s + 1;
            match_idx += 1;
            ni = match_idx;
        } else {
            return false;
        }
    }

    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }

    pi == pattern.len()
}

impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "List the entries of a directory whose names match a `*` wildcard pattern."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to search." },
                "pattern": { "type": "string", "description": "Wildcard pattern, e.g. `*.txt`." }
            },
            "required": ["path", "pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: GlobInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        let mut matches = Vec::new();
        for entry in std::fs::read_dir(&input.path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if wildcard_match(&input.pattern, &name) {
                matches.push(name);
            }
        }
        matches.sort();

        Ok(matches.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::GlobTool;
    use crate::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn glob_matches_files_by_wildcard_pattern() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("keep.txt"), "").unwrap();
        std::fs::write(temp.path().join("also-keep.txt"), "").unwrap();
        std::fs::write(temp.path().join("skip.md"), "").unwrap();

        let tool = GlobTool;
        let output = tool
            .execute(json!({
                "path": temp.path().to_string_lossy(),
                "pattern": "*.txt"
            }))
            .await
            .expect("globbing a real tempdir should succeed");

        assert!(output.contains("keep.txt"));
        assert!(output.contains("also-keep.txt"));
        assert!(!output.contains("skip.md"));
    }

    #[tokio::test]
    async fn glob_returns_typed_error_for_missing_directory() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("does-not-exist");

        let tool = GlobTool;
        let result = tool
            .execute(json!({ "path": missing.to_string_lossy(), "pattern": "*.txt" }))
            .await;

        assert!(
            result.is_err(),
            "globbing a missing directory must return a typed error"
        );
    }
}
