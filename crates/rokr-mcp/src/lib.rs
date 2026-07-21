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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// The Streamable HTTP `McpClientPort` implementation (ticket 48,
/// mcp-http-transport, stretch scope): connects to a remote MCP server
/// over HTTP instead of spawning a subprocess. Same `RunningService`
/// field shape as `RmcpStdioClient` -- `rmcp`'s `RunningService<R, S>` is
/// generic over the role/handler, not the transport, so both clients hold
/// the identical type once `initialize` completes; only how each gets
/// there (`connect` below vs. `spawn` above) differs.
pub struct RmcpHttpClient {
    service: rmcp::service::RunningService<rmcp::RoleClient, ()>,
}

impl RmcpHttpClient {
    /// Connects to `url` and completes the MCP `initialize` handshake,
    /// sending `headers` as literal HTTP headers on every request. Static
    /// bearer/env-token auth only (the caller resolves the token value
    /// into `headers` itself) -- no OAuth 2.1 (PRD "Out of Scope"), so
    /// this is a plain header pass-through, not a token-refresh flow. `()`
    /// as the client-side handler mirrors `RmcpStdioClient::spawn`'s
    /// reasoning -- see its doc comment.
    pub async fn connect(
        url: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<Self, McpClientError> {
        let mut custom_headers = std::collections::HashMap::new();
        for (name, value) in headers {
            let header_name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|err| {
                McpClientError::Spawn(format!("invalid HTTP header name {name:?}: {err}"))
            })?;
            let header_value = http::HeaderValue::from_str(value).map_err(|err| {
                McpClientError::Spawn(format!("invalid HTTP header value for {name:?}: {err}"))
            })?;
            custom_headers.insert(header_name, header_value);
        }
        let config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
            url.to_string(),
        )
        .custom_headers(custom_headers);
        let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
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
/// writer of `status`/`tools`/`joined`; every reader (tool-set assembly in
/// `main.rs`, ticket 51's `/mcp` command) only ever reads through the
/// accessor methods below.
pub struct McpServerHandle {
    pub name: String,
    status: Arc<std::sync::Mutex<McpServerStatus>>,
    tools: Arc<std::sync::Mutex<Vec<Arc<McpTool>>>>,
    /// PC-1 ruling (supersedes ticket 46's whole-session `OnceLock`
    /// freeze): this server's FROZEN tool contribution to every session's
    /// assembled snapshot -- `None` until this server has joined at least
    /// once, `Some(tools)` from then on. Written exactly by
    /// `run_lifecycle`'s success path, at most once per invocation: the
    /// first time this server reaches `Ready` (whether that's the initial
    /// spawn's bounded retry or, after `Degraded`, an explicit
    /// `reconnect()`), overwriting any previous value. Deliberately NOT
    /// cleared by a transport-level `Degraded` transition (F-003, set
    /// directly by `McpTool::execute_boxed` on a failed call) or by
    /// `reconnect()` itself before the new attempt succeeds -- a
    /// mid-session tool call failure or a reconnect attempt still in
    /// flight must never retroactively un-join tools already contributed
    /// to the session snapshot.
    joined: Arc<std::sync::Mutex<Option<Vec<Arc<McpTool>>>>>,
    /// F-002: bumped by `reconnect()` before it re-spawns the lifecycle
    /// task. `run_lifecycle` captures the generation value current AT THE
    /// MOMENT IT WAS SPAWNED and re-checks it against this live counter
    /// immediately before every `status`/`tools`/`joined` write; a
    /// mismatch means a newer `reconnect()` has since superseded this
    /// task, which then abandons silently instead of clobbering the newer
    /// task's state with a stale result.
    generation: Arc<AtomicU64>,
    /// Ticket 51 (mcp-hooks-introspection), `/mcp reconnect`: re-invokes
    /// this server's connect+`list_tools` retry loop from a fresh
    /// `Starting` state. Boxed/type-erased (rather than a second generic
    /// parameter on this struct) so `McpServerHandle` stays a plain,
    /// non-generic type usable in `Vec<McpServerHandle>` regardless of
    /// which connector produced it -- `reconnect` below is the only
    /// caller.
    restart: Arc<dyn Fn() + Send + Sync>,
}

impl McpServerHandle {
    pub fn status(&self) -> McpServerStatus {
        self.status.lock().unwrap().clone()
    }

    /// The current LIVE tool list: empty before `Ready`, and permanently
    /// empty for a `Degraded` server. This is NOT what a session's tool-set
    /// snapshot is built from (see `joined`/`snapshot_tools` below) --
    /// it's informational, reflecting this handle's state right now.
    /// Cloned out (cheap -- `Arc<McpTool>` per entry) rather than borrowed,
    /// so a caller doesn't need to hold this handle's lock.
    pub fn tools(&self) -> Vec<Arc<McpTool>> {
        self.tools.lock().unwrap().clone()
    }

    /// PC-1 ruling: this server's FROZEN contribution to the session
    /// snapshot -- `None` if it has never joined (never reached `Ready`),
    /// `Some(tools)` from its first join onward. See `joined`'s doc comment
    /// on the struct for exactly when this is written.
    pub fn joined(&self) -> Option<Vec<Arc<McpTool>>> {
        self.joined.lock().unwrap().clone()
    }

    /// Ticket 51 (mcp-hooks-introspection), `/mcp reconnect`: resets this
    /// server back to `Starting` and re-spawns its connect+`list_tools`
    /// retry loop from attempt 1, fencing off the OLD lifecycle task first
    /// (F-002) by bumping `generation` -- any write that in-flight task
    /// still attempts afterward is silently discarded rather than racing
    /// the new one. The exhausted bounded retry's backoff state lived
    /// entirely on the stack of the now-superseded `run_lifecycle` task --
    /// spawning a fresh one from attempt 1 IS clearing it; there is no
    /// separate counter to reset. Also clears the LIVE tool snapshot
    /// (`tools`), though a `Degraded` server already has none.
    ///
    /// Deliberately does NOT touch `joined` -- PC-1's per-server frozen
    /// session contribution is only overwritten by a NEW successful
    /// `Ready` from the freshly-spawned lifecycle task below (a "rejoin"),
    /// never blanked out just because a reconnect attempt STARTED; a
    /// session that already saw this server's tools keeps seeing them
    /// while the reconnect is still in flight.
    ///
    /// Callable regardless of current status -- a caller may restrict
    /// which states it allows reconnecting from (`main.rs`'s `/mcp
    /// reconnect` refuses anything but `Degraded`, PC-1/F-002's "auto-join
    /// is once-per-server, re-entry only via explicit reconnect" guard),
    /// this method itself has no opinion and always proceeds.
    pub fn reconnect(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        *self.status.lock().unwrap() = McpServerStatus::Starting;
        self.tools.lock().unwrap().clear();
        (*self.restart)();
    }
}


/// PC-1 ruling (supersedes ticket 46's whole-session `OnceLock` freeze;
/// PRD "MCP caching and session semantics"): builds the CURRENT joined
/// tool-set from every server's frozen per-server contribution
/// (`McpServerHandle::joined` -- `None`/not-yet-joined servers contribute
/// nothing), sorted deterministically by `(server, tool)` -- not handle/
/// server registration order, which is neither meaningful nor stable -- so
/// ordering is a pure function of WHICH servers have joined, never of
/// timing. A server's contribution here can only ever GROW the set (a new
/// join) or stay the same turn-to-turn -- it never shrinks or reorders an
/// already-joined server's tools, since `joined` itself is monotonic (see
/// its doc comment). Callers (`main.rs`'s `submit`) call this fresh on
/// every turn rather than caching the result themselves -- unlike ticket
/// 46's now-superseded `OnceLock`, re-calling this is cheap (no I/O, just
/// iterating handles) and always safe, since the underlying `joined`
/// state it reads is itself already the frozen/monotonic part.
///
/// F-004: sanitization can make two distinct (server, tool) pairs collide
/// on the same qualified `mcp__<server>__<tool>` name (e.g. servers "my
/// server" and "my_server" both sanitize to "my_server"; or server "a__b"
/// + tool "c" vs. server "a" + tool "b__c" both produce "mcp__a__b__c").
/// After sorting, the FIRST (server, tool) pair to claim a qualified name
/// wins; every later colliding entry is dropped and reported via
/// `notice_tx` rather than silently shadowing the first (which is what
/// "last write wins" `ToolSpec` lookup by name would otherwise do).
pub fn snapshot_tools(
    handles: &[McpServerHandle],
    notice_tx: &std::sync::mpsc::Sender<String>,
) -> Vec<Arc<McpTool>> {
    let mut tools: Vec<Arc<McpTool>> = handles
        .iter()
        .filter_map(|handle| handle.joined())
        .flatten()
        .collect();
    tools.sort_by(|a, b| (&a.server, &a.tool_name).cmp(&(&b.server, &b.tool_name)));

    let mut seen_qualified_names = std::collections::HashSet::new();
    tools.retain(|tool| {
        if seen_qualified_names.insert(tool.qualified_name.clone()) {
            true
        } else {
            let _ = notice_tx.send(format!(
                "MCP tool name collision: '{}' (server '{}', tool '{}') dropped -- \
                 a different server/tool pair already claimed this qualified name",
                tool.qualified_name, tool.server, tool.tool_name
            ));
            false
        }
    });
    tools
}

/// Bounded retry count for a server's connect+list_tools attempt (PRD "MCP
/// lifecycle": "bounded retry with backoff... then the server is marked
/// degraded", "no unbounded retry loop"). Ticket 51 adds a manual `/mcp
/// reconnect` for after this is exhausted.
const MAX_CONNECT_ATTEMPTS: u32 = 3;

/// F-001: per-attempt bound on `connect()`/`list_tools()` each, so a
/// hung `initialize` handshake (a server that accepts the connection but
/// never replies) can't leave a server stuck in `Starting` forever -- a
/// timed-out attempt now counts as a failed attempt (`last_error` set to
/// "timed out during initialize") and proceeds through the same
/// retry/backoff/degrade path a connection-refused or protocol-error
/// failure already did.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
///
/// Ticket 51 (mcp-hooks-introspection), `/mcp reconnect`: `F: Clone` (new
/// bound) lets this build a `spawn_lifecycle` closure that re-invokes
/// `run_lifecycle` on demand, stored on the returned handle as
/// `McpServerHandle::reconnect`'s restart hook. Free for every real
/// connector today (`spawn_stdio_server`/`spawn_http_server` below capture
/// only `String`/`Vec<String>`/`HashMap<String, String>`, all `Clone`, so
/// the closures rustc derives for them are automatically `Clone` too).
/// `notice_tx` is wrapped in `Arc<Mutex<_>>` internally (rather than
/// changing this function's own parameter type) because
/// `std::sync::mpsc::Sender` is `Send` but not `Sync`, and it needs to be
/// re-cloned on every reconnect from inside a closure that itself must be
/// `Sync` (`McpServerHandle` flows through `Arc<Vec<McpServerHandle>>` in
/// `main.rs`, which requires every field to stay `Send + Sync`).
pub fn spawn_server_with_connector<F, Fut>(
    name: String,
    connect: F,
    notice_tx: std::sync::mpsc::Sender<String>,
    auto_approve: Arc<Vec<String>>,
) -> McpServerHandle
where
    F: Fn() -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Arc<dyn McpClientPort>, McpClientError>> + Send + 'static,
{
    let status = Arc::new(std::sync::Mutex::new(McpServerStatus::Starting));
    let tools = Arc::new(std::sync::Mutex::new(Vec::new()));
    let joined = Arc::new(std::sync::Mutex::new(None));
    // F-002: generation 0 is the initial spawn below; `reconnect()` bumps
    // this before re-invoking `spawn_lifecycle`, fencing off whichever
    // task was already in flight.
    let generation = Arc::new(AtomicU64::new(0));
    let notice_tx = Arc::new(std::sync::Mutex::new(notice_tx));

    let spawn_lifecycle: Arc<dyn Fn() + Send + Sync> = {
        let name = name.clone();
        let connect = connect.clone();
        let status = Arc::clone(&status);
        let tools = Arc::clone(&tools);
        let joined = Arc::clone(&joined);
        let generation = Arc::clone(&generation);
        let notice_tx = Arc::clone(&notice_tx);
        let auto_approve = Arc::clone(&auto_approve);
        Arc::new(move || {
            let sender = notice_tx.lock().unwrap().clone();
            // F-002: read the CURRENT generation synchronously, before the
            // task is even spawned, so this invocation's task is
            // permanently stamped with the generation active at the
            // moment it was started -- a later `reconnect()` bumping the
            // live counter afterward is exactly what makes THIS task
            // stale.
            let my_generation = generation.load(Ordering::SeqCst);
            tokio::spawn(run_lifecycle(
                name.clone(),
                connect.clone(),
                Arc::clone(&status),
                Arc::clone(&tools),
                Arc::clone(&joined),
                sender,
                Arc::clone(&auto_approve),
                Arc::clone(&generation),
                my_generation,
            ));
        })
    };

    (*spawn_lifecycle)();

    McpServerHandle {
        name,
        status,
        tools,
        joined,
        generation,
        restart: spawn_lifecycle,
    }
}

/// The background task body `spawn_server_with_connector` spawns: a bounded
/// retry loop over connect+`list_tools`, publishing `Ready`+tools (and, PC-1,
/// joining the session snapshot) on success, or `Degraded`+a one-line
/// notice once attempts are exhausted. Never panics on a connect/list_tools
/// failure -- a server that errors, exits immediately, or times out (F-001)
/// degrades this one server, never the rest of the process (PRD "MCP
/// lifecycle": "never crashes or blocks the rest of rokr").
///
/// F-002: `generation`/`my_generation` fence every write this task makes
/// against `status`/`tools_out`/`joined` -- `my_generation` is the value
/// `generation` held at the moment this task was spawned (by the initial
/// spawn or a `reconnect()`); if `generation`'s LIVE value has since moved
/// on (a newer `reconnect()` happened), this task abandons silently right
/// before each write rather than racing a newer task for the last write.
///
/// PC-1 ruling: on a successful `Ready`, this server's tool contribution
/// joins `joined` unconditionally -- there is no separate "already joined,
/// skip" check needed, because this function's success branch runs at most
/// once per invocation (it `return`s immediately after), and invocations
/// only ever happen at the initial spawn or an explicit `reconnect()` --
/// never as an automatic re-run after `Ready`. That's exactly "auto-join is
/// once-per-server, first Ready only, re-entry only via explicit
/// reconnect": nothing here re-triggers itself.
async fn run_lifecycle<F, Fut>(
    name: String,
    connect: F,
    status: Arc<std::sync::Mutex<McpServerStatus>>,
    tools_out: Arc<std::sync::Mutex<Vec<Arc<McpTool>>>>,
    joined: Arc<std::sync::Mutex<Option<Vec<Arc<McpTool>>>>>,
    notice_tx: std::sync::mpsc::Sender<String>,
    auto_approve: Arc<Vec<String>>,
    generation: Arc<AtomicU64>,
    my_generation: u64,
) where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<Arc<dyn McpClientPort>, McpClientError>>,
{
    let mut last_error = String::new();
    // F-003: shared across every `McpTool` this invocation builds, so a
    // transport-level failure on ANY of this server's tools degrades the
    // same `status` this lifecycle task itself writes to (and reuses the
    // same notice channel).
    let tool_notice_tx = Arc::new(std::sync::Mutex::new(notice_tx.clone()));

    for attempt in 1..=MAX_CONNECT_ATTEMPTS {
        // F-001: each attempt's connect+list_tools is individually bounded
        // -- a hung `initialize` handshake can no longer leave this task
        // stuck forever; it times out, counts as a failed attempt, and the
        // loop proceeds exactly as it would after any other failure.
        let attempt_result = match tokio::time::timeout(CONNECT_TIMEOUT, connect()).await {
            Ok(Ok(client)) => {
                match tokio::time::timeout(CONNECT_TIMEOUT, client.list_tools()).await {
                    Ok(Ok(defs)) => Ok((client, defs)),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(_elapsed) => Err("timed out during initialize".to_string()),
                }
            }
            Ok(Err(err)) => Err(err.to_string()),
            Err(_elapsed) => Err("timed out during initialize".to_string()),
        };

        match attempt_result {
            Ok((client, defs)) => {
                if generation.load(Ordering::SeqCst) != my_generation {
                    // F-002: a newer reconnect superseded this task while
                    // connect/list_tools was in flight -- abandon before
                    // writing anything.
                    return;
                }
                let built: Vec<Arc<McpTool>> = defs
                    .into_iter()
                    .map(|def| {
                        Arc::new(McpTool::new(
                            client.clone(),
                            name.clone(),
                            def,
                            auto_approve.clone(),
                            Arc::clone(&status),
                            Arc::clone(&tool_notice_tx),
                        ))
                    })
                    .collect();
                *tools_out.lock().unwrap() = built.clone();
                *status.lock().unwrap() = McpServerStatus::Ready;
                // PC-1: joins (or rejoins, on a reconnect success) this
                // server's frozen session contribution.
                *joined.lock().unwrap() = Some(built);
                return;
            }
            Err(err) => last_error = err,
        }

        if attempt < MAX_CONNECT_ATTEMPTS {
            tokio::time::sleep(backoff_delay(attempt)).await;
        }
    }

    if generation.load(Ordering::SeqCst) != my_generation {
        // F-002: superseded before the bounded retry even finished
        // exhausting -- don't report a stale failure over a newer task's
        // in-progress or already-successful attempt.
        return;
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
    auto_approve: Vec<String>,
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
    spawn_server_with_connector(name, connect, notice_tx, Arc::new(auto_approve))
}

/// Production entry point for a Streamable HTTP MCP server (ticket 48,
/// mcp-http-transport, stretch scope): mirrors `spawn_stdio_server` above,
/// substituting `RmcpHttpClient::connect` for `RmcpStdioClient::spawn` --
/// same lifecycle/retry/status machinery either way, since
/// `spawn_server_with_connector` is generic over the connector, not the
/// transport.
pub fn spawn_http_server(
    name: String,
    url: String,
    headers: std::collections::HashMap<String, String>,
    notice_tx: std::sync::mpsc::Sender<String>,
    auto_approve: Vec<String>,
) -> McpServerHandle {
    let connect = move || {
        let url = url.clone();
        let headers = headers.clone();
        async move {
            let client = RmcpHttpClient::connect(&url, &headers).await?;
            Ok(Arc::new(client) as Arc<dyn McpClientPort>)
        }
    };
    spawn_server_with_connector(name, connect, notice_tx, Arc::new(auto_approve))
}

/// Shared `list_tools`/`call_tool` logic for every `McpClientPort` impl
/// backed by a real `rmcp` `RunningService` (ticket 48, mcp-http-transport:
/// factored out once `RmcpHttpClient` needed the exact same body
/// `RmcpStdioClient` already had -- `RunningService<R, S>` is generic over
/// the role/handler, not the transport, so this works unchanged for both
/// stdio and HTTP).
async fn list_tools_via(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
) -> Result<Vec<McpToolDef>, McpClientError> {
    // `list_all_tools` (rather than the single-page `list_tools`) pages
    // through `nextCursor` automatically -- this fixture and v1's
    // one-server wiring never paginate, but there's no reason to
    // hand-roll cursor-following when `rmcp` already provides it.
    let tools = service
        .peer()
        .list_all_tools()
        .await
        .map_err(|err| McpClientError::Request(err.to_string()))?;
    Ok(tools
        .into_iter()
        .map(|tool| McpToolDef {
            name: tool.name.to_string(),
            description: tool.description.map(|d| d.to_string()).unwrap_or_default(),
            input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
        })
        .collect())
}

async fn call_tool_via(
    service: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    name: &str,
    arguments: serde_json::Value,
) -> Result<RawCallResult, McpClientError> {
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
    let result = service
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
}

impl McpClientPort for RmcpStdioClient {
    fn list_tools<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<McpToolDef>, McpClientError>> + Send + 'a>> {
        Box::pin(list_tools_via(&self.service))
    }

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<RawCallResult, McpClientError>> + Send + 'a>> {
        Box::pin(call_tool_via(&self.service, name, arguments))
    }
}

impl McpClientPort for RmcpHttpClient {
    fn list_tools<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<McpToolDef>, McpClientError>> + Send + 'a>> {
        Box::pin(list_tools_via(&self.service))
    }

    fn call_tool<'a>(
        &'a self,
        name: &'a str,
        arguments: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<RawCallResult, McpClientError>> + Send + 'a>> {
        Box::pin(call_tool_via(&self.service, name, arguments))
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
    /// This server's `auto_approve` allowlist (ticket 47,
    /// mcp-permission-polish), of UNQUALIFIED tool names -- see
    /// `rokr_config::McpServerConfig::auto_approve`'s doc comment for why
    /// unqualified. Shared (`Arc`) across every `McpTool` for the same
    /// server rather than cloned per tool, since it's set once at server
    /// configuration time and never mutated.
    auto_approve: Arc<Vec<String>>,
    /// F-003: the SAME `status` cell `McpServerHandle`/`run_lifecycle`
    /// share for this server. `Ready` was previously sticky -- once a
    /// server reached `Ready`, nothing ever moved it out of that state
    /// again, so a server that died mid-session kept reporting
    /// `state=connected` via `/mcp` while every call silently errored.
    /// `execute_boxed` below writes `Degraded` here on a transport-level
    /// failure, so the NEXT `/mcp` (or reconnect attempt) reflects reality.
    status: Arc<std::sync::Mutex<McpServerStatus>>,
    /// F-003: shared notice sender (`Arc<Mutex<_>>` since
    /// `std::sync::mpsc::Sender` is `Send` but not `Sync`, and `McpTool`
    /// must be `Sync` -- `ExecutableTool: Send + Sync`) for the one-line
    /// degrade notice `execute_boxed` sends alongside the `status` write.
    notice_tx: Arc<std::sync::Mutex<std::sync::mpsc::Sender<String>>>,
}

impl McpTool {
    pub fn new(
        client: Arc<dyn McpClientPort>,
        server: impl Into<String>,
        def: McpToolDef,
        auto_approve: Arc<Vec<String>>,
        status: Arc<std::sync::Mutex<McpServerStatus>>,
        notice_tx: Arc<std::sync::Mutex<std::sync::mpsc::Sender<String>>>,
    ) -> Self {
        let server = server.into();
        let qualified_name = qualified_name(&server, &def.name);
        Self {
            client,
            server,
            tool_name: def.name,
            qualified_name,
            description: def.description,
            input_schema: def.input_schema,
            auto_approve,
            status,
            notice_tx,
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
                    // Tool-level `isError` (the SERVER ran the call and
                    // reported a failure) must NOT degrade the server --
                    // the transport itself is fine; only a
                    // `McpClientError::Request` below (the round-trip
                    // itself failing) means this server is unreachable.
                    Err(rokr_core::ToolError::ExecutionFailed(flatten_content(
                        &result.content,
                    )))
                }
                Ok(result) => Ok(flatten_content(&result.content)),
                Err(err @ McpClientError::Request(_)) => {
                    // F-003: `Ready` was sticky -- a dead server mid-session
                    // kept reporting `state=connected` via `/mcp` forever
                    // while every call kept failing. A transport-level
                    // error here means THIS server, not just this one
                    // call, is unreachable, so it degrades the shared
                    // `status` cell (visible to `/mcp` and to any future
                    // `/mcp reconnect` gate) and emits a one-line notice --
                    // matching `run_lifecycle`'s own degrade-notice shape.
                    // PC-1 interaction: this does NOT touch
                    // `McpServerHandle::joined` -- the server's
                    // already-joined tools stay frozen in every session's
                    // snapshot; only the live `status` (and therefore
                    // whether `/mcp reconnect` will even accept a retry)
                    // changes.
                    let reason = err.to_string();
                    *self.status.lock().unwrap() = McpServerStatus::Degraded {
                        reason: reason.clone(),
                    };
                    let notice_tx = self.notice_tx.lock().unwrap().clone();
                    let _ = notice_tx.send(format!(
                        "MCP server '{}' degraded: {reason}",
                        self.server
                    ));
                    Err(rokr_core::ToolError::ExecutionFailed(reason))
                }
                Err(err) => Err(rokr_core::ToolError::ExecutionFailed(err.to_string())),
            }
        })
    }

    fn preview(
        &self,
        input: serde_json::Value,
    ) -> Option<Result<PermissionPayload, rokr_core::ToolError>> {
        // Ticket 47 (mcp-permission-polish), PRD "MCP permissions": a tool
        // on its server's `auto_approve` list is checked by UNQUALIFIED
        // name (`self.tool_name`, not `self.qualified_name`) -- see
        // `rokr_config::McpServerConfig::auto_approve`'s doc comment.
        // Returning `None` here (rather than `Some(Ok(_))`) is what makes
        // it ungated: `run_tool_loop`'s existing `preview() -> None` =>
        // execute-directly-no-permission-check semantics already does the
        // rest, so this needs no change to the loop itself.
        if self.auto_approve.iter().any(|name| name == &self.tool_name) {
            return None;
        }

        let input_pretty =
            serde_json::to_string_pretty(&input).unwrap_or_else(|_| input.to_string());
        Some(Ok(PermissionPayload::ToolCall {
            server: self.server.clone(),
            tool: self.tool_name.clone(),
            input_pretty,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeClient {
        content: Vec<RawContentItem>,
        is_error: bool,
        /// ITEM 0 / F-004 test support: `list_tools`'s canned response.
        /// Defaults to empty (`..Default::default()` at existing call
        /// sites that only care about `call_tool` behavior); a test
        /// exercising the join/snapshot path sets this explicitly.
        tools: Vec<McpToolDef>,
    }

    impl McpClientPort for FakeClient {
        fn list_tools<'a>(
            &'a self,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<McpToolDef>, McpClientError>> + Send + 'a>>
        {
            let tools = self.tools.clone();
            Box::pin(async move { Ok(tools) })
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

    /// F-003 test support: a throwaway `status`/`notice_tx` pair for tests
    /// that build a bare `McpTool` directly (rather than through
    /// `spawn_server_with_connector`/`run_lifecycle`) and don't care about
    /// wiring it to a real `McpServerHandle`.
    fn fresh_status_and_notice() -> (
        Arc<std::sync::Mutex<McpServerStatus>>,
        Arc<std::sync::Mutex<std::sync::mpsc::Sender<String>>>,
    ) {
        let status = Arc::new(std::sync::Mutex::new(McpServerStatus::Ready));
        let (notice_tx, _notice_rx) = std::sync::mpsc::channel::<String>();
        (status, Arc::new(std::sync::Mutex::new(notice_tx)))
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
                let client: Arc<dyn McpClientPort> = Arc::new(FakeClient::default());
                Ok(client)
            }
        };

        let (notice_tx, _notice_rx) = std::sync::mpsc::channel::<String>();
        let before = Instant::now();
        let handle = spawn_server_with_connector(
            "srv".to_string(),
            connect,
            notice_tx,
            Arc::new(Vec::new()),
        );

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

    /// Ticket 51 (mcp-hooks-introspection), `/mcp reconnect`: a server that
    /// has exhausted its bounded retry (`MAX_CONNECT_ATTEMPTS` failures,
    /// now `Degraded`) can be reconnected -- `McpServerHandle::reconnect`
    /// must reset status to `Starting` SYNCHRONOUSLY (before any `.await`,
    /// proving the degraded/backoff state is cleared immediately rather
    /// than lingering until some later attempt happens to succeed), then
    /// re-run the connect+`list_tools` retry loop from attempt 1 -- proven
    /// here by a connector that succeeds on the very first call after
    /// `reconnect()`, reaching `Ready` promptly (well under the wall-clock
    /// backoff the exhausted first loop already paid), with the total
    /// attempt count landing at exactly `MAX_CONNECT_ATTEMPTS + 1` (3
    /// exhausted attempts, then 1 fresh successful one) -- not some higher
    /// number that would mean the old loop's attempt counter carried over.
    #[tokio::test]
    async fn reconnect_resets_degraded_server_to_retrying_and_clears_backoff_state() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        let attempts = Arc::new(AtomicUsize::new(0));
        let should_succeed = Arc::new(AtomicBool::new(false));
        let connect = {
            let attempts = Arc::clone(&attempts);
            let should_succeed = Arc::clone(&should_succeed);
            move || {
                let attempts = Arc::clone(&attempts);
                let should_succeed = Arc::clone(&should_succeed);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    if should_succeed.load(Ordering::SeqCst) {
                        let client: Arc<dyn McpClientPort> = Arc::new(FakeClient::default());
                        Ok(client)
                    } else {
                        Err(McpClientError::Spawn("boom".to_string()))
                    }
                }
            }
        };

        let (notice_tx, _notice_rx) = std::sync::mpsc::channel::<String>();
        let handle = spawn_server_with_connector(
            "srv".to_string(),
            connect,
            notice_tx,
            Arc::new(Vec::new()),
        );

        // Let the bounded retry (3 attempts, with backoff between them)
        // exhaust and land on Degraded.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            matches!(handle.status(), McpServerStatus::Degraded { .. }),
            "expected Degraded after the bounded retry exhausted, got {:?}",
            handle.status()
        );
        assert_eq!(attempts.load(Ordering::SeqCst), MAX_CONNECT_ATTEMPTS as usize);

        should_succeed.store(true, Ordering::SeqCst);
        handle.reconnect();

        assert_eq!(
            handle.status(),
            McpServerStatus::Starting,
            "reconnect must reset status to Starting synchronously"
        );

        let before = Instant::now();
        loop {
            if handle.status() == McpServerStatus::Ready {
                break;
            }
            assert!(
                before.elapsed() < Duration::from_millis(500),
                "reconnect did not reach Ready promptly -- stale backoff state from the \
                 exhausted retry loop appears to have carried over, got {:?}",
                handle.status()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        assert_eq!(
            attempts.load(Ordering::SeqCst),
            MAX_CONNECT_ATTEMPTS as usize + 1,
            "expected the reconnect's retry loop to start counting from attempt 1, not \
             continue from the exhausted loop's leftover count"
        );
    }

    /// F-001 done-when: a connector whose future never resolves (a hung
    /// `initialize` handshake) must not leave the server stuck in
    /// `Starting` forever -- each attempt's `connect()` is bounded by
    /// `CONNECT_TIMEOUT`, so the bounded retry still exhausts and reaches
    /// `Degraded` with a notice, on the SAME schedule a real
    /// connection-refused failure would. Paused/advanceable virtual time
    /// (`start_paused = true`) lets this cover `CONNECT_TIMEOUT *
    /// MAX_CONNECT_ATTEMPTS` of virtual time without a real multi-minute
    /// wait -- tokio auto-advances the paused clock to the next timer
    /// deadline whenever every task is blocked on one.
    #[tokio::test(start_paused = true)]
    async fn hung_connect_times_out_and_still_reaches_degraded_with_notice() {
        let connect = || async {
            // Never resolves -- simulates a server that accepts the
            // connection but never replies to `initialize`.
            std::future::pending::<Result<Arc<dyn McpClientPort>, McpClientError>>().await
        };

        let (notice_tx, notice_rx) = std::sync::mpsc::channel::<String>();
        let handle = spawn_server_with_connector(
            "hung".to_string(),
            connect,
            notice_tx,
            Arc::new(Vec::new()),
        );

        // Under start_paused virtual time, this auto-fast-forwards through
        // every CONNECT_TIMEOUT + backoff interval as soon as every task is
        // blocked on a timer -- resolves promptly in real wall-clock time.
        tokio::time::sleep(CONNECT_TIMEOUT * (MAX_CONNECT_ATTEMPTS + 1)).await;

        assert!(
            matches!(handle.status(), McpServerStatus::Degraded { .. }),
            "expected a permanently-hung connect to still reach Degraded within the bounded \
             retry window, got {:?}",
            handle.status()
        );

        let notice = notice_rx
            .try_recv()
            .expect("expected a degrade notice to have been sent");
        assert!(
            notice.contains("hung"),
            "expected the notice to name the server, got: {notice:?}"
        );
    }

    /// F-002 done-when: `reconnect()` mid-retry must fence off the
    /// already-in-flight OLD lifecycle task so ONLY the newer task's
    /// outcome is ever observable, even if the old task's own attempt
    /// later resolves SUCCESSFULLY (racing after the newer task already
    /// wrote `Ready`). Deterministic (gate-controlled, not sleep-raced):
    /// the very first `connect()` call ever (the original spawn's attempt
    /// 1) blocks on a gate the test holds closed until well after
    /// `reconnect()`'s own freshly-spawned task has already reached
    /// `Ready`; every OTHER call succeeds immediately. Critically, the two
    /// outcomes are DISTINGUISHABLE (different tool names) -- proving
    /// fencing actually discarded the stale write, not just that both
    /// writes happened to agree.
    #[tokio::test]
    async fn reconnect_mid_retry_fences_off_stale_task_so_only_newer_task_state_survives() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let (stale_gate_tx, stale_gate_rx) = tokio::sync::oneshot::channel::<()>();
        let stale_gate_rx = Arc::new(std::sync::Mutex::new(Some(stale_gate_rx)));
        let call_count = Arc::new(AtomicUsize::new(0));
        let connect = {
            let stale_gate_rx = Arc::clone(&stale_gate_rx);
            let call_count = Arc::clone(&call_count);
            move || {
                let stale_gate_rx = Arc::clone(&stale_gate_rx);
                let call_count = Arc::clone(&call_count);
                async move {
                    let n = call_count.fetch_add(1, Ordering::SeqCst);
                    if n == 0 {
                        // The very first call ever (the original spawn's
                        // attempt 1): blocks until the test explicitly
                        // releases it -- well after reconnect()'s own task
                        // has already reached Ready -- then succeeds with a
                        // tool distinctly named "stale_tool".
                        let rx = stale_gate_rx.lock().unwrap().take().expect("gate taken once");
                        let _ = rx.await;
                        let client: Arc<dyn McpClientPort> = Arc::new(FakeClient {
                            tools: vec![McpToolDef {
                                name: "stale_tool".to_string(),
                                description: String::new(),
                                input_schema: serde_json::json!({}),
                            }],
                            ..Default::default()
                        });
                        Ok(client)
                    } else {
                        // Every later call (the reconnect-spawned task's
                        // own first attempt) succeeds immediately with a
                        // distinctly named "fresh_tool".
                        let client: Arc<dyn McpClientPort> = Arc::new(FakeClient {
                            tools: vec![McpToolDef {
                                name: "fresh_tool".to_string(),
                                description: String::new(),
                                input_schema: serde_json::json!({}),
                            }],
                            ..Default::default()
                        });
                        Ok(client)
                    }
                }
            }
        };

        let (notice_tx, _notice_rx) = std::sync::mpsc::channel::<String>();
        let handle = spawn_server_with_connector(
            "srv".to_string(),
            connect,
            notice_tx,
            Arc::new(Vec::new()),
        );

        // Give the first (stale) attempt a moment to actually start and
        // block on the gate.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(handle.status(), McpServerStatus::Starting);

        // Bumps generation and spawns a NEW lifecycle task, whose first
        // connect() call (n >= 1, gate already taken) succeeds immediately
        // with "fresh_tool".
        handle.reconnect();

        let ready_deadline = std::time::Instant::now() + Duration::from_millis(500);
        while handle.status() != McpServerStatus::Ready {
            assert!(
                std::time::Instant::now() < ready_deadline,
                "expected the newer task to reach Ready promptly, got {:?}",
                handle.status()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let joined_after_new_ready = handle.joined().expect("expected the server to have joined");
        assert_eq!(
            joined_after_new_ready.len(),
            1,
            "expected exactly one tool from the newer task's join"
        );
        assert_eq!(joined_after_new_ready[0].name(), qualified_name("srv", "fresh_tool"));

        // NOW release the stale task's gate -- its call resolves
        // successfully with "stale_tool", strictly AFTER the newer task
        // already joined with "fresh_tool".
        let _ = stale_gate_tx.send(());
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(
            handle.status(),
            McpServerStatus::Ready,
            "the stale task's late-resolving success must not have altered status"
        );
        let joined_after_stale_resolves =
            handle.joined().expect("expected the server to still be joined");
        assert_eq!(
            joined_after_stale_resolves
                .iter()
                .map(|t| t.name().to_string())
                .collect::<Vec<_>>(),
            vec![qualified_name("srv", "fresh_tool")],
            "the stale (pre-reconnect) task's late success must not have overwritten the \
             newer task's joined contribution with 'stale_tool'"
        );
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
            ..Default::default()
        });
        let (status, notice_tx) = fresh_status_and_notice();
        let tool = McpTool::new(
            client,
            "srv",
            McpToolDef {
                name: "echo".to_string(),
                description: "d".to_string(),
                input_schema: serde_json::json!({}),
            },
            Arc::new(Vec::new()),
            status,
            notice_tx,
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

    /// A `FakeClient` whose `call_tool` always fails at the TRANSPORT
    /// level (`McpClientError::Request`), for F-003's degrade test --
    /// distinct from `is_error: true` in `FakeClient` above, which is a
    /// tool-level (`isError` in the wire response) failure that must NOT
    /// degrade the server.
    struct TransportFailingClient;

    impl McpClientPort for TransportFailingClient {
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
            Box::pin(async {
                Err(McpClientError::Request(
                    "connection reset by peer".to_string(),
                ))
            })
        }
    }

    /// F-003 done-when: a transport-level `Err(McpClientError::Request(_))`
    /// from `call_tool` -- e.g. the server process died mid-session -- must
    /// degrade the SHARED `status` cell (not leave `Ready` sticky) and emit
    /// a one-line notice, so `/mcp` and a future `/mcp reconnect` both see
    /// reality instead of a server that looks connected forever while every
    /// call silently errors.
    #[tokio::test]
    async fn transport_level_call_tool_error_degrades_shared_status_and_sends_notice() {
        let client: Arc<dyn McpClientPort> = Arc::new(TransportFailingClient);
        let status = Arc::new(std::sync::Mutex::new(McpServerStatus::Ready));
        let (notice_tx, notice_rx) = std::sync::mpsc::channel::<String>();
        let notice_tx = Arc::new(std::sync::Mutex::new(notice_tx));
        let tool = McpTool::new(
            client,
            "flaky-server",
            McpToolDef {
                name: "echo".to_string(),
                description: "d".to_string(),
                input_schema: serde_json::json!({}),
            },
            Arc::new(Vec::new()),
            Arc::clone(&status),
            notice_tx,
        );

        let result = tool.execute_boxed(serde_json::json!({})).await;
        assert!(result.is_err(), "expected the transport error to surface as a tool error");

        assert!(
            matches!(*status.lock().unwrap(), McpServerStatus::Degraded { .. }),
            "expected the shared status cell to be Degraded after a transport-level call_tool \
             error, got: {:?}",
            *status.lock().unwrap()
        );

        let notice = notice_rx
            .try_recv()
            .expect("expected a one-line degrade notice to have been sent");
        assert!(
            notice.contains("flaky-server"),
            "expected the notice to name the degraded server, got: {notice:?}"
        );
    }

    /// F-003: a tool-level `isError: true` result (the server ran the call
    /// and reported failure) must NOT degrade the server -- the transport
    /// itself is fine. Reuses `FakeClient`'s existing `is_error` knob
    /// (distinct from `TransportFailingClient` above).
    #[tokio::test]
    async fn tool_level_is_error_does_not_degrade_shared_status() {
        let client: Arc<dyn McpClientPort> = Arc::new(FakeClient {
            content: vec![RawContentItem::Text("failed on purpose".to_string())],
            is_error: true,
            ..Default::default()
        });
        let status = Arc::new(std::sync::Mutex::new(McpServerStatus::Ready));
        let (notice_tx, notice_rx) = std::sync::mpsc::channel::<String>();
        let notice_tx = Arc::new(std::sync::Mutex::new(notice_tx));
        let tool = McpTool::new(
            client,
            "srv",
            McpToolDef {
                name: "echo".to_string(),
                description: "d".to_string(),
                input_schema: serde_json::json!({}),
            },
            Arc::new(Vec::new()),
            Arc::clone(&status),
            notice_tx,
        );

        let result = tool.execute_boxed(serde_json::json!({})).await;
        assert!(result.is_err(), "expected the isError result to surface as a tool error");

        assert_eq!(
            *status.lock().unwrap(),
            McpServerStatus::Ready,
            "a tool-level isError must not degrade the server status"
        );
        assert!(
            notice_rx.try_recv().is_err(),
            "a tool-level isError must not send a degrade notice"
        );
    }


    /// Ticket 48 (mcp-http-transport): a minimal wiremock-backed fake
    /// Streamable HTTP MCP server. Mirrors
    /// `tests/fixtures/fake_mcp_server.rs`'s stdio fixture's JSON-RPC
    /// result shapes for `initialize`/`tools/list` exactly, but replies
    /// over HTTP instead of stdio -- proving `RmcpHttpClient` speaks the
    /// same wire protocol `RmcpStdioClient` does, just over a different
    /// transport. Only handles POST requests: with
    /// `StreamableHttpClientTransportConfig::allow_stateless` defaulted to
    /// `true` and no `Mcp-Session-Id` response header set below, the real
    /// `rmcp` client never opens a GET/SSE stream (gated on a session id
    /// being present), so a GET handler isn't needed.
    struct FakeHttpMcpResponder;

    impl wiremock::Respond for FakeHttpMcpResponder {
        fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
            let body: serde_json::Value = request.body_json().unwrap_or(serde_json::Value::Null);
            let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let Some(id) = body.get("id").cloned() else {
                // A notification (e.g. `notifications/initialized`) has no
                // "id" and gets no JSON-RPC reply -- 202 Accepted with no
                // body is what `post_message`'s reqwest impl treats as
                // `StreamableHttpPostResponse::Accepted`.
                return wiremock::ResponseTemplate::new(202);
            };
            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "fake-http-mcp-server", "version": "0.1.0" }
                }),
                "tools/list" => serde_json::json!({
                    "tools": [
                        {
                            "name": "echo",
                            "description": "Echoes back a fixed marker string.",
                            "inputSchema": { "type": "object", "properties": {} }
                        }
                    ]
                }),
                _ => serde_json::json!({}),
            };
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
        }
    }

    /// Ticket 48 (mcp-http-transport) unit test: every HTTP request
    /// `RmcpHttpClient` makes over the course of `initialize` +
    /// `notifications/initialized` + `tools/list` carries the configured
    /// static header -- the ONE mock mounted below requires it on every
    /// match, so if any request in that sequence were missing it, that
    /// request would 404 (no mock matches), the client would error, and
    /// `connect`/`list_tools` would return `Err` instead of the tool list
    /// asserted below.
    #[tokio::test]
    async fn http_transport_sends_static_token_header_on_every_request() {
        use wiremock::matchers::{header, method};
        use wiremock::{Mock, MockServer};

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(header("authorization", "Bearer test-token-abc"))
            .respond_with(FakeHttpMcpResponder)
            .mount(&mock_server)
            .await;

        let mut headers = std::collections::HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer test-token-abc".to_string());

        let client = RmcpHttpClient::connect(&mock_server.uri(), &headers)
            .await
            .expect("expected the HTTP MCP client to connect and complete initialize");

        let tools = client
            .list_tools()
            .await
            .expect("expected list_tools to succeed");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
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
        let client: Arc<dyn McpClientPort> = Arc::new(FakeClient::default());
        let (status, notice_tx) = fresh_status_and_notice();
        let tool = McpTool::new(
            client,
            "my server",
            McpToolDef {
                name: "search:tool".to_string(),
                description: "d".to_string(),
                input_schema: serde_json::json!({}),
            },
            Arc::new(Vec::new()),
            status,
            notice_tx,
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

    /// PC-1 ruling (supersedes ticket 46's whole-session `OnceLock`
    /// freeze/this test's original semantics): `snapshot_tools` assembles
    /// from every handle's FROZEN `joined` contribution, sorted
    /// deterministically by (server, tool) -- not handle/tool
    /// registration order. A handle's LIVE `tools` mutating (e.g. a
    /// reconnect attempt still in flight, before it reaches `Ready` and
    /// rejoins) never reaches into the assembled set, since that reads
    /// only `joined`. Unlike ticket 46's now-superseded whole-session
    /// freeze, though, a server joining for the FIRST time between two
    /// calls DOES show up in the later one -- there's no session-wide
    /// cache to go stale; ordering is a pure function of which servers
    /// have joined as of THIS call, recomputed fresh every time.
    #[test]
    fn joined_snapshot_sorted_by_server_then_tool_ignores_live_churn_but_reflects_new_joins() {
        fn make_joined_handle(name: &str, joined_tool_names: &[&str]) -> McpServerHandle {
            let client: Arc<dyn McpClientPort> = Arc::new(FakeClient::default());
            let (status, notice_tx) = fresh_status_and_notice();
            let tools = joined_tool_names
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
                        Arc::new(Vec::new()),
                        Arc::clone(&status),
                        Arc::clone(&notice_tx),
                    ))
                })
                .collect::<Vec<_>>();
            McpServerHandle {
                name: name.to_string(),
                status,
                tools: Arc::new(std::sync::Mutex::new(tools.clone())),
                joined: Arc::new(std::sync::Mutex::new(Some(tools))),
                generation: Arc::new(AtomicU64::new(0)),
                restart: Arc::new(|| {}),
            }
        }

        // Deliberately out of (server, tool) order, and with "server_a"
        // registered AFTER "server_b", to prove the snapshot re-sorts
        // rather than trusting handle/tool iteration order.
        let mut handles = vec![
            make_joined_handle("server_b", &["search", "alpha"]),
            make_joined_handle("server_a", &["zzz", "search"]),
        ];

        let (notice_tx, _notice_rx) = std::sync::mpsc::channel::<String>();
        let snapshot_1 = snapshot_tools(&handles, &notice_tx);
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

        // A handle's LIVE tool list changing must not affect the joined
        // snapshot, which reads only the frozen `joined` field, never
        // `tools`.
        *handles[0].tools.lock().unwrap() = vec![];
        let snapshot_2 = snapshot_tools(&handles, &notice_tx);
        let names_2: Vec<&str> = snapshot_2.iter().map(|t| t.name()).collect();
        assert_eq!(
            names_1, names_2,
            "a handle's LIVE tool list mutating must not affect the joined snapshot"
        );

        // A server joining for the FIRST time (added to the handle list,
        // as an initial spawn's first Ready would) appears in the very
        // next call, in deterministic sorted position -- no session-wide
        // freeze holds it back.
        handles.push(make_joined_handle("server_c", &["late"]));
        let snapshot_3 = snapshot_tools(&handles, &notice_tx);
        let names_3: Vec<&str> = snapshot_3.iter().map(|t| t.name()).collect();
        assert_eq!(
            names_3,
            vec![
                qualified_name("server_a", "search"),
                qualified_name("server_a", "zzz"),
                qualified_name("server_b", "alpha"),
                qualified_name("server_b", "search"),
                qualified_name("server_c", "late"),
            ],
            "a newly-joined server must appear, in deterministic sorted position, on the next call"
        );
    }

    /// F-004: two servers whose names sanitize to the SAME string ("my
    /// server" and "my_server" both become "my_server"), each contributing
    /// a tool with the identical raw name, collide on the same qualified
    /// `mcp__my_server__search` -- the snapshot must drop the later
    /// duplicate (keeping exactly one, per (server, tool) sort order) and
    /// report the drop via a notice, rather than silently producing two
    /// `ToolSpec`s with the same name (which would route every call to
    /// whichever one a naive by-name lookup finds first).
    #[test]
    fn snapshot_drops_duplicate_qualified_name_and_emits_collision_notice() {
        fn make_joined_handle(server: &str, tool_name: &str) -> McpServerHandle {
            let client: Arc<dyn McpClientPort> = Arc::new(FakeClient::default());
            let (status, notice_tx) = fresh_status_and_notice();
            let tool = Arc::new(McpTool::new(
                client,
                server.to_string(),
                McpToolDef {
                    name: tool_name.to_string(),
                    description: String::new(),
                    input_schema: serde_json::json!({}),
                },
                Arc::new(Vec::new()),
                status.clone(),
                notice_tx,
            ));
            McpServerHandle {
                name: server.to_string(),
                status,
                tools: Arc::new(std::sync::Mutex::new(vec![tool.clone()])),
                joined: Arc::new(std::sync::Mutex::new(Some(vec![tool]))),
                generation: Arc::new(AtomicU64::new(0)),
                restart: Arc::new(|| {}),
            }
        }

        let handles = vec![
            make_joined_handle("my server", "search"),
            make_joined_handle("my_server", "search"),
        ];

        let (notice_tx, notice_rx) = std::sync::mpsc::channel::<String>();
        let snapshot = snapshot_tools(&handles, &notice_tx);

        let names: Vec<&str> = snapshot.iter().map(|t| t.name()).collect();
        assert_eq!(
            names,
            vec![qualified_name("my_server", "search")],
            "expected exactly one survivor of the colliding qualified name, got: {names:?}"
        );

        let notice = notice_rx
            .try_recv()
            .expect("expected a collision notice to have been sent");
        assert!(
            notice.contains("collision") || notice.contains("duplicate"),
            "expected the notice to describe a name collision, got: {notice:?}"
        );
    }
}

