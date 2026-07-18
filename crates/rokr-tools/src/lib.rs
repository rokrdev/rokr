//! The `Tool` trait and the core tool implementations (read, write, edit, bash, glob, grep, ls).
//!
//! See `CONTRIBUTING.md`'s "Extension points" section and
//! `docs/adr/0004-agent-tool-loop.md`: tools are a one-file-each contributor
//! extension point, executed by the core loop in `rokr-core` and fed results
//! back to the model. `docs/adr/0005-permission-model.md` requires gated
//! tools (write, edit, bash) to expose a side-effect-free preview from the
//! start, ahead of the permission machinery that will call it.

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod ls;
pub mod read;
pub mod write;

/// Errors returned while executing or previewing a [`Tool`].
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// Input JSON did not match the tool's expected shape.
    #[error("invalid input for tool: {0}")]
    InvalidInput(String),

    /// Filesystem or subprocess I/O failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// The tool ran but could not complete the requested operation (e.g. an
    /// `edit` whose `old_str` was not found).
    #[error("{0}")]
    ExecutionFailed(String),
}

/// A model-callable tool: a stable name, a model-facing description, a JSON
/// schema describing its input shape, and an `execute` method that performs
/// the tool's effect and returns its output as text (or a typed error).
///
/// One file per tool (`read.rs`, `write.rs`, ...) per `CONTRIBUTING.md`.
pub trait Tool {
    /// Stable, model-facing tool name (e.g. `"read"`).
    fn name(&self) -> &'static str;

    /// Human/model-facing description of what the tool does.
    fn description(&self) -> &'static str;

    /// JSON schema describing the shape of `execute`'s `input` argument.
    fn input_schema(&self) -> serde_json::Value;

    /// Perform the tool's effect and return its output as text, or a typed
    /// error.
    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError>;
}

/// A [`Tool`] whose side effects must be gated behind user permission
/// (`docs/adr/0005-permission-model.md`). Implemented by `write`, `edit`,
/// and `bash`. `preview` computes a human-readable description of what
/// `execute` would do, with zero filesystem or process side effects, so a
/// permission prompt can show it before the user grants access.
pub trait PreviewableTool: Tool {
    /// Describe what `execute(input)` would do, without doing it.
    fn preview(&self, input: serde_json::Value) -> Result<String, ToolError>;
}
