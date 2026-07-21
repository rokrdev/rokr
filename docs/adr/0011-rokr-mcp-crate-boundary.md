# 0011 - rokr-mcp crate boundary

## Status

accepted

## Context

Phase 6 (`.workflow/docs/phase-6-mcp-hooks.md`) opens rokr up to
user-configured MCP tool servers. The model needs to call an MCP tool
through the exact same gated tool loop (`rokr_core::run_tool_loop`) it
already uses for `bash`/`write`/`edit`, so an MCP tool call has to end up
as another `&dyn rokr_core::ExecutableTool` entry in the tools slice
`crates/rokr/src/main.rs` assembles — no parallel dispatch path.

Two boundary questions had to be settled before writing any MCP code:
where does the MCP client live in the crate graph, and how much of
`rmcp` (the official Rust MCP SDK, chosen over hand-rolling the wire
protocol) is allowed to leak past that boundary. `rmcp` is pre-1.0: its
client API has changed shape across recent releases (confirmed directly
against its published source for the version pinned below — the type
`Content = Annotated<RawContent>` documented for `rmcp`'s current release
does not exist in 2.2.0, which uses a flat `ContentBlock` enum instead),
so treating it as a stable foundation the rest of rokr can depend on
directly would tie every future `rmcp` upgrade to a rokr-wide review.

`ExecutableTool::name` also had a lifetime problem: it returned
`&'static str`, which every built-in tool satisfies trivially (all are
`&'static str` literals via `rokr_tools::Tool::name`), but an MCP tool's
model-facing name is namespaced per server at runtime
(`mcp__<server>__<tool>`, PRD "Namespacing") and has to live in an owned
`String` on the adapter — it cannot be `'static`.

## Decision

**Crate boundary**: `rokr-mcp` is a new crate depending on `rokr-core`
only — never `rokr-tools`, `rokr-tui`, or `rokr-provider`. It owns the
`rmcp`-backed client wrapper, the stdio transport, and the `McpTool`
adapter. From every other crate's perspective, an MCP tool is just
another `&dyn rokr_core::ExecutableTool`; nothing outside `rokr-mcp` knows
`rmcp` exists.

**`rmcp` pinned to an exact version**: `rmcp = "=2.2.0"` in
`crates/rokr-mcp/Cargo.toml`, with only the `client` and
`transport-child-process` features enabled (this crate is never an MCP
*server*). An exact pin (rather than a caret range) means an `rmcp`
upgrade is a deliberate, reviewed `Cargo.toml` change, not something that
silently shifts underneath a `cargo update` — appropriate for a pre-1.0
dependency whose API has already been observed to change shape between
versions. The pin was validated, not assumed: `crates/rokr-mcp/tests/fixtures/fake_mcp_server.rs`
is a hand-rolled, minimal MCP JSON-RPC-over-stdio server (`initialize`,
`tools/list`, `tools/call` only) that `RmcpStdioClient` was round-tripped
against directly (spawn → initialize → list_tools → call_tool →
flattened, correctly-`isError`-mapped result) before any of this landed
in `crates/rokr/src/main.rs`.

**`rmcp` wrapped behind an internal trait**: `McpClientPort` (in
`crates/rokr-mcp/src/lib.rs`) is the only seam `McpTool` talks to —
`list_tools`/`call_tool`, returning `rokr-mcp`'s own `McpToolDef` /
`RawCallResult` / `RawContentItem` types, never an `rmcp` type. Only
`RmcpStdioClient` (the production implementation) touches `rmcp` directly;
unit tests substitute a fake `McpClientPort` to exercise `McpTool`'s
flatten-and-map logic without a real subprocess. This is the same
reasoning as ADR 0009's port/adapter split for `Provider`, applied to a
dependency this codebase doesn't control the API of at all, rather than
one it does.

The "no hand-rolled JSON-RPC" rule this decision enforces is about
`rokr-mcp`'s *production client path* specifically: reimplementing what
`rmcp` already does correctly, just to avoid depending on it, would be
strictly worse (more surface area to get the spec's edge cases wrong, for
no boundary benefit `McpClientPort` doesn't already provide). It does not
extend to the *test fixture* — `fake_mcp_server.rs` hand-rolls the server
side of the same protocol on purpose, so the round-trip validates the
real `rmcp` client against a real subprocess without depending on
`rmcp`'s server-side macros (`server`/`macros` features, `schemars`, ...)
that this crate has no other reason to pull in.

**`ExecutableTool::name` relaxed from `&'static str` to `&str`**
(`crates/rokr-core/src/lib.rs`): the only edit this phase makes to shared
`rokr-core` code (per the PRD, also the single sync point the hooks track
waits on). Every existing built-in tool's `&'static str` name still
coerces to `&str` unchanged — `impl_executable_tool!` and
`impl_executable_tool_gated!` needed no changes, and `run_tool_loop`'s
`tool.name() == name.as_str()` lookup only ever compared by value. The
relaxation exists solely so `McpTool::name` can return a borrow of its
own `qualified_name: String` field instead of requiring a `'static`
leak (`Box::leak` or similar) just to satisfy a signature no built-in
tool actually needed the strictness of.

**`rokr_core::ToolError` re-export**: `ExecutableTool::execute_boxed`'s
signature is fixed as `Result<String, rokr_tools::ToolError>` — a type
from `rokr-tools`, which `rokr-mcp` is not allowed to depend on directly.
`rokr-core` already depends on `rokr-tools` (for the built-in tool
macros), so `crates/rokr-core/src/lib.rs` adds `pub use
rokr_tools::ToolError;` alongside the `name` relaxation. This adds no new
dependency edge — it only makes a type that already flows through
`ExecutableTool`'s public API nameable from a crate that only depends on
`rokr-core`.

## Considered Options

### `rokr-mcp` depends on `rokr-core` only, `rmcp` behind `McpClientPort` (chosen)

- Pro: an MCP tool call is indistinguishable from a built-in tool call to
  every crate except `rokr-mcp` itself; an `rmcp` upgrade (even a
  breaking one) is contained to one crate and, so long as
  `McpClientPort`'s shape doesn't need to change, invisible to
  `McpTool`'s own tests.
- Con: every `rmcp` type crossing the boundary needs an explicit
  conversion into a `rokr-mcp`-owned type (`McpToolDef`, `RawCallResult`,
  `RawContentItem`) — more boilerplate than re-exporting `rmcp`'s types
  directly. Accepted: the conversion is small and localized to
  `RmcpStdioClient`, and it's exactly what keeps `rmcp` from leaking.

### `rokr-mcp` also depends on `rokr-tools`, no `ToolError` re-export

- Pro: no re-export needed in `rokr-core`; `rokr-mcp` names
  `rokr_tools::ToolError` directly like every built-in tool crate does.
- Con: rejected outright — the architecture decision behind this ticket
  is explicit that `rokr-mcp` depends on `rokr-core` only, "never
  `rokr-tools`". A narrow re-export costs one line in a file already
  being edited for the `name` relaxation; a second dependency edge is a
  standing architectural fact.

### Expose `rmcp` types directly through `McpTool`'s public API

- Pro: no `McpClientPort` trait, no conversion layer, less code up front.
- Con: rejected — this is precisely the coupling the PRD calls out
  `rmcp`'s pre-1.0 status as a reason to avoid ("wrapped behind a thin
  internal trait so the rest of rokr never touches `rmcp`'s pre-1.0 API
  surface directly"). It would also make `McpTool`'s flatten/error-mapping
  logic untestable without a real subprocess, since there'd be no seam to
  substitute a fake behind.

### Loosen the `rmcp` version pin to a caret range (`"2"`)

- Pro: transparently picks up patch/minor fixes without a manual bump.
- Con: rejected for a pre-1.0 dependency specifically — a caret range on
  `2.x` still allows `rmcp` to ship a breaking change under semver's
  pre-1.0 convention (where the leading `0`-major or, per some
  ecosystems' informal convention for young libraries, minor-version
  bumps carry breaking changes) without rokr's build ever flagging it
  until something fails to compile or, worse, silently changes behavior.
  An exact pin makes every `rmcp` upgrade a visible, single-line diff.

## Consequences

- Ticket 45 (mcp-config-and-lifecycle) and later MCP tickets add real
  config-driven, multi-server, background-task lifecycle on top of this
  boundary without touching it — `McpClientPort` and `McpTool` don't
  change shape; only how many `RmcpStdioClient`s get constructed, and how
  their startup is scheduled, changes.
- Ticket 49 (hooks track) can rely on `ExecutableTool::name`'s `&str`
  signature already being settled by this ADR — no further `rokr-core`
  edits are needed for hooks to reach the same call sites.
- `crates/rokr/src/main.rs`'s interim `ROKR_MCP_SERVER` env-var wiring
  (ticket 44) is explicitly throwaway: single server, hardcoded name
  `"interim"`, spawned inline before the TUI starts rather than as a
  background task. Ticket 45 replaces it wholesale with the `mcp` config
  schema and background lifecycle the PRD describes ("MCP lifecycle" —
  spawn/init off the render path, bounded reconnect, `/mcp` status). This
  ADR's crate-boundary decision is what ticket 45 builds on; the env-var
  wiring itself is not part of the decision being recorded here.
- `crates/rokr/Cargo.toml` gained a `rokr-mcp` path dependency as part of
  this ticket. This is not listed in ticket 44's `files-touched`
  frontmatter — it is a mechanical consequence of the boundary decision
  above (the `rokr` binary cannot call into a crate it doesn't depend on)
  and is called out explicitly in the ticket's implementation report
  rather than made silently.
