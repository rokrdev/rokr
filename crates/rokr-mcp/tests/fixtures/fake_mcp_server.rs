//! Fake stdio MCP server fixture (ticket 44, mcp-tracer-bullet).
//!
//! A hand-rolled, minimal MCP JSON-RPC-over-stdio server: just enough of
//! the 2025-06-18 MCP spec's `initialize` handshake, `tools/list`, and
//! `tools/call` to prove the real `rmcp`-backed client in
//! `rokr_mcp::RmcpStdioClient` round-trips against a real subprocess
//! speaking the real wire protocol. Deliberately NOT built on `rmcp`'s own
//! server machinery (`ServerHandler`/`#[tool_router]`) -- the "no
//! hand-rolled JSON-RPC" constraint (`docs/adr/0011-rokr-mcp-crate-boundary.md`)
//! is about rokr-mcp's production *client* path never reimplementing the
//! wire protocol `rmcp` already implements; a throwaway test double playing
//! the server role is exactly the kind of dependency-free fixture a spike
//! wants, and keeps this crate's `rmcp` feature surface client-only.
//!
//! Wire format: newline-delimited JSON-RPC 2.0 objects, one per line, read
//! from stdin and written to stdout -- the `transport-io`/`transport-async-rw`
//! shape `rmcp`'s stdio transports expect on both ends.
//!
//! Exposes exactly one tool, `echo`, whose `tools/call` response text is
//! the fixed marker string in [`FIXED_RESPONSE_TEXT`] -- the acceptance
//! test asserts this exact string appears in the TUI's rendered final
//! reply, proving the text traveled real-subprocess -> `rmcp` client ->
//! `McpTool::execute_boxed` -> `ToolResult` -> the provider's next request
//! -> the model's reply -> the render, not a stub anywhere along that path.

use std::io::{BufRead, Write};

/// The exact text `tools/call` returns for the `echo` tool. Distinctive
/// enough that it can't plausibly appear in rendered TUI chrome or a
/// wiremock-scripted assistant reply by accident.
pub const FIXED_RESPONSE_TEXT: &str = "fake-mcp-server-echo-response-9f3c2a";

/// The one tool this fixture exposes by default, matched by name in
/// `tools/call`. Overridable via `FAKE_MCP_SERVER_TOOL_NAME` (ticket 46,
/// mcp-namespace-multi-server-freeze) so a test can stand up two servers
/// that both expose a tool with the SAME raw name, proving namespacing
/// keeps them distinguishable -- see this fixture's `tool_name()` below.
const TOOL_NAME: &str = "echo";

/// Ticket 46: the effective tool name for this run, read once so every
/// `tools/list`/`tools/call` branch below agrees on it.
fn tool_name() -> String {
    std::env::var("FAKE_MCP_SERVER_TOOL_NAME").unwrap_or_else(|_| TOOL_NAME.to_string())
}

fn main() {
    // Ticket 45 (mcp-config-and-lifecycle) acceptance test support: when
    // set, exits immediately, before reading or responding to any stdin
    // line at all -- so the client's `initialize` request never gets a
    // response and the handshake genuinely fails, the same way a truly
    // broken server would. Deliberately an abrupt exit rather than a faked
    // JSON-RPC error response for `initialize`, since that's a more
    // realistic failure shape and simpler to produce. Not part of the
    // wire-protocol surface this fixture otherwise exercises -- purely a
    // test-control knob, read via `env` (ticket 45's per-server config
    // `stdio.env` map is exactly what threads this through).
    if std::env::var_os("FAKE_MCP_SERVER_FAIL_INIT").is_some() {
        std::process::exit(1);
    }

    // Ticket 46 (mcp-namespace-multi-server-freeze) acceptance test
    // support: when set, blocks BEFORE reading (or responding to) any
    // stdin line -- including `initialize` -- until the given path exists
    // on disk. This lets a test hold a server in `Starting` (never
    // `Ready`) for as long as it wants, then deterministically release it
    // by creating the file, to prove a session's already-frozen MCP tool
    // snapshot is untouched by a server that only becomes `Ready` after
    // that snapshot was taken. A poll loop (not a filesystem watch) to
    // keep this fixture dependency-free.
    if let Ok(gate_path) = std::env::var("FAKE_MCP_SERVER_READY_GATE_FILE") {
        while !std::path::Path::new(&gate_path).exists() {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }

    let tool_name = tool_name();
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = request.get("id").cloned();

        // Notifications (no "id") never get a response, per JSON-RPC 2.0 --
        // `notifications/initialized` is the only one the client sends this
        // fixture needs to tolerate.
        let Some(id) = id else {
            continue;
        };

        let result = match method {
            "initialize" => serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "fake-mcp-server", "version": "0.1.0" }
            }),
            "tools/list" => serde_json::json!({
                "tools": [
                    {
                        "name": tool_name,
                        "description": "Echoes back a fixed marker string.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "message": { "type": "string" }
                            }
                        }
                    }
                ]
            }),
            "tools/call" => {
                let requested_tool = request
                    .get("params")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or("");
                if requested_tool == tool_name {
                    serde_json::json!({
                        "content": [
                            { "type": "text", "text": FIXED_RESPONSE_TEXT }
                        ],
                        "isError": false
                    })
                } else {
                    serde_json::json!({
                        "content": [
                            { "type": "text", "text": format!("unknown tool: {requested_tool}") }
                        ],
                        "isError": true
                    })
                }
            }
            other => {
                let error = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": format!("method not found: {other}") }
                });
                let _ = writeln!(stdout, "{error}");
                let _ = stdout.flush();
                continue;
            }
        };

        let response = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        });
        if writeln!(stdout, "{response}").is_err() {
            break;
        }
        if stdout.flush().is_err() {
            break;
        }
    }
}
