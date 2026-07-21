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
    /// Spawns `command args...` as a child process (with `env` applied on
    /// top of the inherited environment -- ticket 45's config-driven
    /// per-server `env` map) and completes the MCP `initialize` handshake
    /// over its stdio. `()` as the client-side handler (rather than a
    /// custom `ClientHandler` impl) is `rmcp`'s own pattern for a client
    /// with no server-initiated callbacks to answer -- this crate's
    /// tools-only client needs none.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &std::collections::HashMap<String, String>,
    ) -> Result<Self, McpClientError> {
        let mut process = tokio::process::Command::new(command);
        process.args(args);
        process.envs(env);
        let transport = rmcp::transport::TokioChildProcess::new(process)
            .map_err(|err| McpClientError::Spawn(err.to_string()))?;
        let service = rmcp::ServiceExt::serve((), transport)
            .await
            .map_err(|err| McpClientError::Initialize(err.to_string()))?;
        Ok(Self { service })
    }
}

/// Lifecycle status of one configured MCP server (ticket 45,
/// mcp-config-and-lifecycle). Kept introspectable (not a plain
/// success/failure bool) so ticket 51's `/mcp` listing can report it
/// without redesigning this type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerStatus {
    /// Spawned and/or being taken through `initialize` -- not yet
    /// contributing any tools.
    Starting,
    /// `initialize` and `tools/list` both succeeded; `McpServerHandle::tools`
    /// reflects this server's current tool set.
    Ready,
    /// Every connect attempt in the bounded retry failed; this server
    /// contributes zero tools until a future manual reconnect (ticket 51).
    Degraded { reason: String },
}

/// Shared, introspectable state for one configured MCP server: current
/// status plus the tools it contributes once ready. The background
/// lifecycle task (spawned by `spawn_server_with_connector`) is the sole
/// writer; every reader (tool-set assembly in `main.rs`, eventually ticket
/// 51's `/mcp` command) only ever reads through the accessor methods below.
pub struct McpServerHandle {
    pub name: String,
    status: Arc<std::sync::Mutex<McpServerStatus>>,
    tools: Arc<std::sync::Mutex<Vec<Arc<McpTool>>>>,
}

impl McpServerHandle {
    pub fn status(&self) -> McpServerStatus {
        self.status.lock().unwrap().clone()
    }

    /// The current tool snapshot: empty before `Ready`, and permanently
    /// empty for a `Degraded` server. Cloned out (cheap -- `Arc<McpTool>`
    /// per entry) rather than borrowed, so a caller can build a tool-set
    /// snapshot without holding this handle's lock.
    pub fn tools(&self) -> Vec<Arc<McpTool>> {
        self.tools.lock().unwrap().clone()
    }
}


/// PRD "MCP caching and session semantics": builds one session's MCP
/// tool-set snapshot from every configured server's CURRENT tools (empty
/// for a server still `Starting` or permanently `Degraded`), sorted
/// deterministically by `(server, tool)` -- not handle/server registration
/// order, which is neither meaningful nor stable -- so the resulting
/// tool-spec order (and therefore the cached prompt prefix) never shuffles
/// between two snapshots taken over the same ready set. Pure and
/// side-effect-free: calling this again reflects whatever the handles look
/// like AT THAT MOMENT. Freezing the result for the lifetime of a session
/// (calling this exactly once and reusing the owned `Vec` afterwards,
/// never re-snapshotting turn-to-turn) is the CALLER's job -- `main.rs`
/// (ticket 46, mcp-namespace-multi-server-freeze) does that with a
/// `OnceLock`.
pub fn snapshot_tools(handles: &[McpServerHandle]) -> Vec<Arc<McpTool>> {
    let mut tools: Vec<Arc<McpTool>> = handles.iter().flat_map(|handle| handle.tools()).collect();
    tools.sort_by(|a, b| (&a.server, &a.tool_name).cmp(&(&b.server, &b.tool_name)));
    tools
}

/// Bounded retry count for a server's connect+list_tools attempt (PRD "MCP
/// lifecycle": "bounded retry with backoff... then the server is marked
/// degraded", "no unbounded retry loop"). Ticket 51 adds a manual `/mcp
/// reconnect` for after this is exhausted.
const MAX_CONNECT_ATTEMPTS: u32 = 3;

fn backoff_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(200 * attempt as u64)
}

/// Spawns a background tokio task that takes one MCP server through
/// connect -> `list_tools`, publishing its status/tools into the returned
/// `McpServerHandle` as it goes. Returns immediately -- the `tokio::spawn`
/// call below is the only thing this function does with `connect`, so
/// nothing here ever awaits server startup on the calling task, matching
/// the PRD's "fully off the render path" requirement (first paint must
/// never wait on any MCP server).
///
/// Generic over `connect` (rather than hardcoding `RmcpStdioClient::spawn`)
/// so this lifecycle/retry/status logic is unit-testable without a real
/// subprocess -- `spawn_stdio_server` below is the production entry point
/// that supplies a real connector.
pub fn spawn_server_with_connector<F, Fut>(
    name: String,
    connect: F,
    notice_tx: std::sync::mpsc::Sender<String>,
) -> McpServerHandle
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Arc<dyn McpClientPort>, McpClientError>> + Send + 'static,
{
    let status = Arc::new(std::sync::Mutex::new(McpServerStatus::Starting));
    let tools = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handle = McpServerHandle {
        name: name.clone(),
        status: Arc::clone(&status),
        tools: Arc::clone(&tools),
    };

    tokio::spawn(run_lifecycle(name, connect, status, tools, notice_tx));

    handle
}

/// The background task body `spawn_server_with_connector` spawns: a bounded
/// retry loop over connect+`list_tools`, publishing `Ready`+tools on
/// success or `Degraded`+a one-line notice once attempts are exhausted.
/// Never panics on a connect/list_tools failure -- a server that errors,
/// exits immediately, or times out degrades this one server, never the
/// rest of the process (PRD "MCP lifecycle": "never crashes or blocks the
/// rest of rokr").
async fn run_lifecycle<F, Fut>(
    name: String,
    connect: F,
    status: Arc<std::sync::Mutex<McpServerStatus>>,
    tools_out: Arc<std::sync::Mutex<Vec<Arc<McpTool>>>>,
    notice_tx: std::sync::mpsc::Sender<String>,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<Arc<dyn McpClientPort>, McpClientError>>,
{
    let mut last_error = String::new();

    for attempt in 1..=MAX_CONNECT_ATTEMPTS {
        match connect().await {
            Ok(client) => match client.list_tools().await {
                Ok(defs) => {
                    let built: Vec<Arc<McpTool>> = defs
                        .into_iter()
                        .map(|def| Arc::new(McpTool::new(client.clone(), name.clone(), def)))
                        .collect();
                    *tools_out.lock().unwrap() = built;
                    *status.lock().unwrap() = McpServerStatus::Ready;
                    return;
                }
                Err(err) => last_error = err.to_string(),
            },
            Err(err) => last_error = err.to_string(),
        }

        if attempt < MAX_CONNECT_ATTEMPTS {
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }

    *status.lock().unwrap() = McpServerStatus::Degraded {
        reason: last_error.clone(),
    };
    let _ = notice_tx.send(format!(
        "MCP server '{name}' failed to start: {last_error}"
    ));
}

/// Production entry point: spawns `command args...` (with `env` applied to
/// the child process) as a stdio MCP server and takes it through the
/// lifecycle above. Replaces ticket 44's inline, single-server
/// `ROKR_MCP_SERVER` env-var wiring in `crates/rokr/src/main.rs`.
pub fn spawn_stdio_server(
    name: String,
    command: String,
    args: Vec<String>,
    env: std::collections::HashMap<String, String>,
    notice_tx: std::sync::mpsc::Sender<String>,
) -> McpServerHandle {
    let connect = move || {
        let command = command.clone();
        let args = args.clone();
        let env = env.clone();
        async move {
            let client = RmcpStdioClient::spawn(&command, &args, &env).await?;
            Ok(Arc::new(client) as Arc<dyn McpClientPort>)
        }
    };
    spawn_server_with_connector(name, connect, notice_tx)
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
    async fn server_spawn_task_never_blocks_before_ready_signal() {
        use std::time::{Duration, Instant};

        let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
        let gate_rx = Arc::new(std::sync::Mutex::new(Some(gate_rx)));
        let connect = move || {
            let gate_rx = Arc::clone(&gate_rx);
            async move {
                // Blocks until the test explicitly releases the gate --
                // proves spawn_server_with_connector doesn't await this
                // future on the calling task, since the assertion right
                // after the call below runs before the gate is ever
                // released.
                let rx = gate_rx.lock().unwrap().take().expect("connect called once");
                let _ = rx.await;
                let client: Arc<dyn McpClientPort> = Arc::new(FakeClient {
                    content: Vec::new(),
                    is_error: false,
                });
                Ok(client)
            }
        };

        let (notice_tx, _notice_rx) = std::sync::mpsc::channel::<String>();
        let before = Instant::now();
        let handle = spawn_server_with_connector("srv".to_string(), connect, notice_tx);

        assert!(
            before.elapsed() < Duration::from_millis(50),
            "spawn_server_with_connector blocked on connect instead of returning immediately"
        );
        assert_eq!(handle.status(), McpServerStatus::Starting);

        let _ = gate_tx.send(());
        // Let the spawned task run to completion now that the gate is open.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(handle.status(), McpServerStatus::Ready);
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


    /// Ticket 46 (mcp-namespace-multi-server-freeze), PRD "Namespacing":
    /// non-alphanumeric characters in EITHER half of `mcp__<server>__<tool>`
    /// are sanitized (not just rejected/left raw), and two servers exposing
    /// the SAME raw tool name still produce two DISTINCT qualified names --
    /// the whole reason namespacing exists. Exercises the real `McpTool`
    /// adapter (not just the free `qualified_name` fn) so this proves what
    /// the model actually sees via `ExecutableTool::name`/`to_tool_spec`.
    #[test]
    fn tool_names_sanitized_and_namespaced_as_mcp_server_tool() {
        let client: Arc<dyn McpClientPort> = Arc::new(FakeClient {
            content: Vec::new(),
            is_error: false,
        });
        let tool = McpTool::new(
            client,
            "my server",
            McpToolDef {
                name: "search:tool".to_string(),
                description: "d".to_string(),
                input_schema: serde_json::json!({}),
            },
        );

        assert_eq!(tool.name(), "mcp__my_server__search_tool");
        assert_eq!(tool.to_tool_spec().name, "mcp__my_server__search_tool");

        // Two servers exposing a tool with the identical raw name must not
        // collide once namespaced.
        assert_ne!(
            qualified_name("server_a", "search"),
            qualified_name("server_b", "search"),
        );
    }

    /// Ticket 46 (mcp-namespace-multi-server-freeze), PRD "MCP caching and
    /// session semantics": a tool-set snapshot assembled from multiple
    /// servers is sorted deterministically by (server, tool) -- not server
    /// registration/iteration order -- and calling the snapshot fn again
    /// against UNCHANGED handles returns a byte-for-byte identical result.
    #[test]
    fn snapshot_sorted_by_server_then_tool_and_stable_across_repeated_calls() {
        fn make_handle(name: &str, tool_names: &[&str]) -> McpServerHandle {
            let client: Arc<dyn McpClientPort> = Arc::new(FakeClient {
                content: Vec::new(),
                is_error: false,
            });
            let tools = tool_names
                .iter()
                .map(|tool_name| {
                    Arc::new(McpTool::new(
                        client.clone(),
                        name.to_string(),
                        McpToolDef {
                            name: tool_name.to_string(),
                            description: String::new(),
                            input_schema: serde_json::json!({}),
                        },
                    ))
                })
                .collect::<Vec<_>>();
            McpServerHandle {
                name: name.to_string(),
                status: Arc::new(std::sync::Mutex::new(McpServerStatus::Ready)),
                tools: Arc::new(std::sync::Mutex::new(tools)),
            }
        }

        // Deliberately out of (server, tool) order, and with "server_a"
        // registered AFTER "server_b", to prove the snapshot re-sorts
        // rather than trusting handle/tool iteration order.
        let handles = vec![
            make_handle("server_b", &["search", "alpha"]),
            make_handle("server_a", &["zzz", "search"]),
        ];

        let snapshot_1 = snapshot_tools(&handles);
        let names_1: Vec<&str> = snapshot_1.iter().map(|t| t.name()).collect();
        assert_eq!(
            names_1,
            vec![
                qualified_name("server_a", "search"),
                qualified_name("server_a", "zzz"),
                qualified_name("server_b", "alpha"),
                qualified_name("server_b", "search"),
            ]
        );

        let snapshot_2 = snapshot_tools(&handles);
        let names_2: Vec<&str> = snapshot_2.iter().map(|t| t.name()).collect();
        assert_eq!(
            names_1, names_2,
            "repeated snapshots over unchanged handles must be byte-for-byte identical"
        );

        // A handle's live tool list changing AFTER a snapshot was taken
        // (e.g. a future reconnect) must never reach back into that
        // already-taken snapshot -- `snapshot_1` returns owned `Arc<McpTool>`
        // clones, not a view into the handle's mutex.
        *handles[0].tools.lock().unwrap() = vec![Arc::new(McpTool::new(
            Arc::new(FakeClient {
                content: Vec::new(),
                is_error: false,
            }),
            "server_b".to_string(),
            McpToolDef {
                name: "changed".to_string(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        ))];
        let names_1_after_mutation: Vec<&str> = snapshot_1.iter().map(|t| t.name()).collect();
        assert_eq!(
            names_1_after_mutation, names_1,
            "an already-taken snapshot must be unaffected by a later handle mutation"
        );
    }
}

