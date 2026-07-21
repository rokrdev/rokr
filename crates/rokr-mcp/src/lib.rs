//! `rokr-mcp`: an `rmcp`-backed MCP stdio client, wrapped behind an
//! internal trait, plus the `McpTool` adapter that exposes a single MCP
//! tool call as an `rokr_core::ExecutableTool` (ticket 44, mcp-tracer-bullet;
//! see `docs/adr/0011-rokr-mcp-crate-boundary.md`).
//!
//! Depends on `rokr-core` only -- never `rokr-tools`, `rokr-tui`, or
//! `rokr-provider` -- so an MCP tool call is, from the rest of rokr's
//! perspective, just another `&dyn ExecutableTool` entry.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use rokr_core::{ExecutableTool, PermissionPayload, ToolSpec};

/// A single MCP tool as reported by a server's `tools/list`, already
/// stripped of `rmcp`'s wire types.
#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// One content item from a `tools/call` response, flattened out of
/// `rmcp`'s `ContentBlock` enum. Non-text content (images,
/// embedded resources) collapses to `NonText` -- v1 renders a placeholder
/// note for it rather than the real payload (PRD "MCP caching and session
/// semantics").
#[derive(Debug, Clone)]
pub enum RawContentItem {
    Text(String),
    NonText,
}

/// A `tools/call` result, already stripped of `rmcp`'s wire types: the raw
/// content items (not yet flattened to a single string -- that's
/// `McpTool::execute_boxed`'s job, see its doc comment for why) and the
/// server-reported `isError` flag.
#[derive(Debug, Clone)]
pub struct RawCallResult {
    pub content: Vec<RawContentItem>,
    pub is_error: bool,
}

/// Errors from the `McpClientPort` boundary -- spawning the server
/// subprocess, the `initialize` handshake, or a request/response
/// round-trip.
#[derive(Debug, thiserror::Error)]
pub enum McpClientError {
    #[error("failed to start MCP server: {0}")]
    Spawn(String),
    #[error("MCP initialize handshake failed: {0}")]
    Initialize(String),
    #[error("MCP request failed: {0}")]
    Request(String),
}

/// Thin internal seam hiding `rmcp`'s pre-1.0 client API from the rest of
/// `rokr-mcp`'s public surface (and therefore all of rokr) --
/// `docs/adr/0011-rokr-mcp-crate-boundary.md`. `RmcpStdioClient` is the
/// only production implementation; tests substitute a fake so
/// `McpTool`'s flatten/error-mapping logic is verifiable without a real
/// subprocess.
pub trait McpClientPort: Send + Sync {
    fn list_tools<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<McpToolDef>, McpClientError>> + Send + 'a>>;

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<RawCallResult, McpClientError>> + Send + 'a>>;
}

/// The only production `McpClientPort`: spawns a stdio MCP server
/// subprocess, completes the `initialize` handshake, and round-trips
/// `list_tools`/`call_tool` through `rmcp`'s pinned client API. No `rmcp`
/// type appears in this crate's public surface -- everything crossing the
/// `McpClientPort` boundary is converted to/from `rokr-mcp`'s own types
/// right here.
pub struct RmcpStdioClient {
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

impl RmcpStdioClient {
    /// Spawns `command args...` as a child process and completes the MCP
    /// `initialize` handshake over its stdio. `()` as the client-side
    /// handler (rather than a custom `ClientHandler` impl) is `rmcp`'s own
    /// pattern for a client with no server-initiated callbacks to answer --
    /// this tracer bullet's tools-only client needs none.
    pub async fn spawn(command: &str, args: &[String]) -> Result<Self, McpClientError> {
        let mut process = tokio::process::Command::new(command);
        process.args(args);
        let transport = rmcp::transport::TokioChildProcess::new(process)
            .map_err(|err| McpClientError::Spawn(err.to_string()))?;
        let service = rmcp::ServiceExt::serve((), transport)
            .await
            .map_err(|err| McpClientError::Initialize(err.to_string()))?;
        Ok(Self { service })
    }
}

impl McpClientPort for RmcpStdioClient {
    fn list_tools<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<McpToolDef>, McpClientError>> + Send + 'a>> {
        Box::pin(async move {
            // `list_all_tools` (rather than the single-page `list_tools`)
            // pages through `nextCursor` automatically -- this fixture and
            // v1's one-server wiring never paginate, but there's no reason
            // to hand-roll cursor-following when `rmcp` already provides it.
            let tools = self
                .service
                .peer()
                .list_all_tools()
                .await
                .map_err(|err| McpClientError::Request(err.to_string()))?;
            Ok(tools
                .into_iter()
                .map(|tool| McpToolDef {
                    name: tool.name.to_string(),
                    description: tool
                        .description
                        .map(|d| d.to_string())
                        .unwrap_or_default(),
                    input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
                })
                .collect())
        })
    }

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<RawCallResult, McpClientError>> + Send + 'a>> {
        Box::pin(async move {
            let arguments = match arguments {
                serde_json::Value::Object(map) => Some(map),
                serde_json::Value::Null => None,
                other => {
                    let mut map = serde_json::Map::new();
                    map.insert("value".to_string(), other);
                    Some(map)
                }
            };
            let mut params = rmcp::model::CallToolRequestParams::new(name.to_string());
            if let Some(arguments) = arguments {
                params = params.with_arguments(arguments);
            }
            let result = self
                .service
                .peer()
                .call_tool(params)
                .await
                .map_err(|err| McpClientError::Request(err.to_string()))?;
            let content = result
                .content
                .into_iter()
                .map(|item| match item {
                    rmcp::model::ContentBlock::Text(text) => RawContentItem::Text(text.text),
                    _ => RawContentItem::NonText,
                })
                .collect();
            Ok(RawCallResult {
                content,
                is_error: result.is_error.unwrap_or(false),
            })
        })
    }
}

/// Adapter: a single MCP tool exposed to `rokr-core`'s tool loop as an
/// `ExecutableTool`. Hand-implemented (not `impl_executable_tool!`)
/// because it wraps a foreign client, not a `rokr_tools::Tool` -- ticket
/// 44's doc comment.
pub struct McpTool {
    client: Arc<dyn McpClientPort>,
    server: String,
    tool_name: String,
    qualified_name: String,
    description: String,
    input_schema: serde_json::Value,
}

impl McpTool {
    pub fn new(client: Arc<dyn McpClientPort>, server: impl Into<String>, def: McpToolDef) -> Self {
        let server = server.into();
        let qualified_name = qualified_name(&server, &def.name);
        Self {
            client,
            server,
            tool_name: def.name,
            qualified_name,
            description: def.description,
            input_schema: def.input_schema,
        }
    }
}

/// PRD "Namespacing": an MCP tool is exposed to the model as
/// `mcp__<server>__<tool>` so two servers can each expose a same-named
/// tool without colliding.
pub fn qualified_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize(server), sanitize(tool))
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// Flattens a `tools/call` result's content items into a single string
/// (PRD "MCP caching and session semantics": rmcp content items are
/// flattened to text and joined; non-text content becomes a truncated
/// placeholder note). Joined with newlines so multi-part text content
/// stays readable rather than running together.
fn flatten_content(content: &[RawContentItem]) -> String {
    content
        .iter()
        .map(|item| match item {
            RawContentItem::Text(text) => text.clone(),
            RawContentItem::NonText => "[non-text content omitted]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ExecutableTool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn to_tool_spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.qualified_name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
            cache_control: None,
        }
    }

    fn execute_boxed<'a>(
        &'a self,
        input: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, rokr_core::ToolError>> + Send + 'a>> {
        Box::pin(async move {
            match self.client.call_tool(&self.tool_name, input).await {
                Ok(result) if result.is_error => {
                    Err(rokr_core::ToolError::ExecutionFailed(flatten_content(
                        &result.content,
                    )))
                }
                Ok(result) => Ok(flatten_content(&result.content)),
                Err(err) => Err(rokr_core::ToolError::ExecutionFailed(err.to_string())),
            }
        })
    }

    fn preview(
        &self,
        input: serde_json::Value,
    ) -> Option<Result<PermissionPayload, rokr_core::ToolError>> {
        Some(Ok(PermissionPayload::Command(format!(
            "MCP tool call: {}::{} {}",
            self.server, self.tool_name, input
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeClient {
        content: Vec<RawContentItem>,
        is_error: bool,
    }

    impl McpClientPort for FakeClient {
        fn list_tools<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<McpToolDef>, McpClientError>> + Send + 'a>>
        {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn call_tool<'a>(
            &'a self,
            _name: &'a str,
            _arguments: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = Result<RawCallResult, McpClientError>> + Send + 'a>>
        {
            let content = self.content.clone();
            let is_error = self.is_error;
            Box::pin(async move { Ok(RawCallResult { content, is_error }) })
        }
    }

    #[tokio::test]
    async fn mcp_tool_flattens_text_content_and_maps_is_error() {
        let client = Arc::new(FakeClient {
            content: vec![
                RawContentItem::Text("first part".to_string()),
                RawContentItem::NonText,
                RawContentItem::Text("second part".to_string()),
            ],
            is_error: true,
        });
        let tool = McpTool::new(
            client,
            "srv",
            McpToolDef {
                name: "echo".to_string(),
                description: "d".to_string(),
                input_schema: serde_json::json!({}),
            },
        );

        let result = tool.execute_boxed(serde_json::json!({})).await;

        match result {
            Err(err) => {
                let text = err.to_string();
                assert!(text.contains("first part"), "got: {text}");
                assert!(text.contains("second part"), "got: {text}");
                assert!(
                    text.contains("non-text"),
                    "expected a placeholder note for the non-text item, got: {text}"
                );
            }
            Ok(text) => panic!("expected an error result (isError: true), got Ok({text})"),
        }
    }
}
