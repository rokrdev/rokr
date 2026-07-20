//! The `websearch` tool: delegates a search query to the active provider's
//! native, server-side search capability (e.g. Anthropic's web-search server
//! tool) and returns its results verbatim.
//!
//! Unlike `webfetch`, this tool ships no client-side scraper of its own —
//! per the PRD ("day one parallel" build-order note, `websearch` trade-offs
//! section), if the active provider lacks a native search capability the
//! tool should be *absent* from the tool set for that session rather than
//! falling back to a degraded client-side implementation. [`NativeSearchCapability`]
//! is the local hook this file defines for that delegation; [`for_capability`]
//! is the "tool set construction" helper that omits the tool entirely when no
//! capability is supplied. `rokr-tools` does not depend on `rokr-core`
//! (see `webfetch.rs`'s note on the same constraint), so this trait is
//! self-contained here rather than sharing a type with `rokr-core::Provider`'s
//! `native_search_capable` hook — `crates/rokr/src/main.rs` is the only place
//! that sees both and bridges them (ticket 28: websearch-tool).

use std::sync::Arc;

use serde::Deserialize;

use crate::{Tool, ToolError};

#[derive(Debug, Deserialize)]
struct WebsearchInput {
    query: String,
}

/// A provider's native, server-side search capability. Implemented for a
/// real provider adapter elsewhere (out of scope for this ticket — no
/// implementation exists yet); scripted with a fake in this file's own
/// tests so `websearch`'s delegation behavior stays testable independent of
/// any concrete adapter (per the PRD's "day one parallel" note).
pub trait NativeSearchCapability: Send + Sync {
    /// Runs `query` against the provider's native search and returns its
    /// results as text, verbatim — `WebsearchTool::execute` performs no
    /// post-processing of this string.
    fn search(&self, query: &str) -> Result<String, ToolError>;
}

/// Delegates its query to a [`NativeSearchCapability`] and returns the
/// result verbatim. Never implements `PreviewableTool`: unlike `webfetch`,
/// there is no local side effect for the user to approve — the request is
/// entirely the provider's.
pub struct WebsearchTool {
    capability: Arc<dyn NativeSearchCapability>,
}

impl WebsearchTool {
    pub fn new(capability: Arc<dyn NativeSearchCapability>) -> Self {
        Self { capability }
    }
}

/// Builds the `websearch` tool for a session's tool set from an optional
/// native search capability. `None` in, `None` out: the tool is omitted
/// entirely when the active provider has no native search capability,
/// rather than falling back to a client-side scraper.
pub fn for_capability(
    capability: Option<Arc<dyn NativeSearchCapability>>,
) -> Option<WebsearchTool> {
    capability.map(WebsearchTool::new)
}

impl Tool for WebsearchTool {
    fn name(&self) -> &'static str {
        "websearch"
    }

    fn description(&self) -> &'static str {
        "Search the web using the active provider's native search capability."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query." }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<String, ToolError> {
        let input: WebsearchInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;

        self.capability.search(&input.query)
    }
}

#[cfg(test)]
mod tests {
    use super::{for_capability, NativeSearchCapability, WebsearchTool};
    use crate::{Tool, ToolError};
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    #[test]
    fn websearch_tool_absent_when_provider_lacks_native_search_capability() {
        let tool = for_capability(None);

        assert!(
            tool.is_none(),
            "websearch must be absent from the tool set when the active provider reports no \
             native search capability"
        );
    }

    /// Records the last query it was asked to search, and returns a fixed
    /// canned response, so the test can assert both that the exact query
    /// reached the capability and that the capability's output comes back
    /// unmodified.
    struct ScriptedCapability {
        response: String,
        received_query: Mutex<Option<String>>,
    }

    impl NativeSearchCapability for ScriptedCapability {
        fn search(&self, query: &str) -> Result<String, ToolError> {
            *self.received_query.lock().unwrap() = Some(query.to_string());
            Ok(self.response.clone())
        }
    }

    #[tokio::test]
    async fn websearch_tool_delegates_query_to_provider_native_search_capability() {
        let capability = Arc::new(ScriptedCapability {
            response: "scripted search results, verbatim".to_string(),
            received_query: Mutex::new(None),
        });

        let tool: WebsearchTool = for_capability(Some(capability.clone()))
            .expect("websearch must be present when a native search capability is supplied");

        let output = tool
            .execute(json!({ "query": "rust async traits" }))
            .await
            .expect("execute should succeed when the capability succeeds");

        assert_eq!(
            output, "scripted search results, verbatim",
            "execute must return the capability's search results verbatim, with no client-side \
             scraping or post-processing"
        );
        assert_eq!(
            capability.received_query.lock().unwrap().as_deref(),
            Some("rust async traits"),
            "the tool must delegate the exact query to the provider's native search capability"
        );
    }
}
