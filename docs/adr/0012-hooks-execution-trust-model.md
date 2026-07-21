# 0012 - Hooks execution and trust model

## Status

accepted

## Context

Phase 6's "Hooks" track (`.workflow/docs/phase-6-mcp-hooks.md`) lets a rokr
user attach arbitrary shell commands to points in the agent lifecycle --
starting with `PreToolUse`, which can veto a tool call outright before it
ever runs (e.g. hard-blocking `rm -rf` regardless of what the model or the
user approves interactively). A hook command is, by construction, an
external process the rokr binary spawns and feeds live data from the
running session: the tool the model chose to call and the exact arguments
it chose to call it with. That data can contain anything a model (or, once
MCP tools exist, a remote MCP server) decides to put there, including
literal shell metacharacters. If that data were ever used to build the
hook's command line, a hook configured for something as narrow as "log
every bash call" would become an arbitrary-command-execution primitive
driven by untrusted model output.

Ticket 49 (hooks-tracer-bullet) is the first slice to actually run a hook
subprocess and wire its result into `run_tool_loop`'s permission gate. This
ADR records the four load-bearing decisions that shape everything the
hooks track builds on top of, once they became concrete rather than
theoretical -- mirroring `docs/adr/0009-provider-trait-location.md`, a
boundary decision written down mid-phase rather than speculatively up
front.

## Decision

**1. JSON-stdin-only execution contract.** A hook is a shell command,
executed via `sh -c <command>` where `<command>` is exactly the
user-configured string -- nothing else is ever appended, substituted, or
interpolated into it. The event payload (tool name, tool input, and
whatever other fields a later event variant carries) is serialized to JSON
and piped to the subprocess's STDIN, then the STDIN pipe is closed so the
hook sees a clean EOF once the payload is fully written. This is the entire
injection guard: `rokr-hooks::execute_hook`'s command-line construction
never touches the payload, so no value the model or an MCP server produces
-- however adversarial -- can ever execute as part of the hook's own
command. A hook script that wants the data uses ordinary JSON parsing
(`jq`, `python -c 'import json,sys; ...'`, etc.) to read it back off its own
stdin.

**2. User-scope-only trust boundary.** Per the PRD's "Config schema"
section, both `mcp` and `hooks` are loaded from the user-scope config file
only (`~/.config/rokr/rokr.json`) -- never from any project-local config,
which doesn't exist as a concept for rokr yet and is explicitly deferred to
Phase 7 as its own, separately-designed trust boundary. This ticket has no
config loader to enforce that yet (the tracer bullet's interim
`ROKR_PRETOOLUSE_HOOK` env-var wiring, see Consequences, is opt-in and
process-scoped, not file-scoped), but the intent is fixed now: `git clone
&& rokr` must never be able to make a hook command run that the user
running rokr didn't themselves put in their own user-scope config. Ticket
50's config loader is expected to read `hooks` from that one file and
nowhere else, exactly as `mcp` already does per ADR 0011's lineage.

**3. Exit-code contract.** A hook's exit code is the entire signal rokr
reads back, per the PRD's "Hooks" section: `0` is success (for
`PreToolUse` this means "allow", with `stdout` reserved for a later event's
context-injection use, unused here); `2` is a **blocking** deny -- `stderr`
becomes the message surfaced back through the loop, in `PreToolUse`'s case
as the error `ToolResult` content, byte-identical in shape to what an
interactive permission rejection already produces; any other nonzero exit,
or the process being killed by a signal, is a **non-blocking** failure --
surfaced (a one-line notice) but treated as if the hook had never run, so a
broken or misconfigured hook script degrades to "no hooks configured"
rather than wedging every subsequent tool call.

**4. Timeout / non-blocking-failure guarantee.** Every hook invocation runs
under `tokio::time::timeout` (default 60s, a per-hook override arrives with
ticket 50's config schema) racing `Child::wait_with_output`. A hook that
outlives its timeout is treated exactly like any other non-blocking
failure -- the loop continues, never waiting on or being wedged by a
runaway process. `Child::kill_on_drop(true)` is what actually reaps the
process: `wait_with_output(self)` owns the `Child`, so when
`tokio::time::timeout`'s future is dropped on expiry, the owned `Child`
drops with it and tokio sends the kill signal as part of that drop -- no
separate "kill after timeout" step to get wrong. This closes the same class
of hazard ADR 0007/0008 already guard against for the render thread: a hook
blocks the agent-loop future it's attached to (by design -- `PreToolUse`
must be able to veto before the tool runs), but can never block anything
else, and can never block *that* future indefinitely either.

## Considered Options

### JSON on STDIN, command line never touches payload data (chosen)

- Pro: injection-proof by construction -- there is no code path where
  payload bytes reach the shell's parser, so no escaping/quoting scheme can
  be gotten wrong. Matches the shape of how CI systems and git hooks
  already pass structured event data to hook scripts (env vars for small
  scalars, stdin for a full payload), so it's a familiar contract to write
  a hook against.
- Con: a hook author who forgets to read stdin (or reads it incorrectly)
  gets no payload and no error -- the failure mode is "my hook always sees
  nothing useful," discoverable in normal use, not a silent security gap.

### Environment variables for payload fields

- Pro: trivially readable from any shell script (`$TOOL_NAME`,
  `$TOOL_INPUT_JSON`) with no JSON parsing required for simple cases.
- Con: still requires the *value* of an env var to be set to
  attacker-influenced data, and a hook script that then uses that env var
  inside its OWN subsequent shell calls (e.g. `sh -c "grep $TOOL_INPUT_JSON
  file"`) reintroduces the exact injection hazard this ADR exists to close
  -- just moved one level down, into every hook author's own script instead
  of rokr's executor. Rejected: the whole point is to make the safe path
  the only path, not to make it available alongside an unsafe one.

### String-interpolating the payload into the command line, with escaping

- Pro: none identified that outweigh the con below.
- Con: this is exactly the hazard described in Context. Shell-escaping a
  JSON blob correctly for `sh -c` across every value a model or MCP tool
  might produce (nested quotes, backticks, `$(...)`, newlines, NUL bytes in
  degenerate cases) is a well-known class of bug with a long history of
  getting it wrong; STDIN delivery sidesteps the entire problem rather than
  trying to solve it correctly. Rejected outright.

### Blocking (kill on timeout) vs. detach-and-abandon on timeout

- Pro (detach): simpler -- just stop awaiting the process and move on,
  never issue a kill.
- Con (detach): an abandoned hook process keeps running indefinitely,
  consuming resources and potentially still holding open file descriptors
  or a lock the next hook invocation (or the tool the hook was supposed to
  gate) depends on; on repeated timeouts this leaks processes without
  bound. Rejected -- `kill_on_drop` costs nothing extra to wire (it falls
  out of `wait_with_output` owning the `Child`) and closes the leak.

## Consequences

- `rokr-hooks` depends on nothing in the `rokr-*` workspace (not even
  `rokr-core`) -- it is a standalone event/payload-types-plus-subprocess-
  executor crate, matching `rokr-mcp`'s own "depend on the minimum, expose
  a thin seam" shape from ADR 0011, just with an even smaller dependency
  footprint since it doesn't need to speak MCP's wire protocol.
- `rokr-core::run_tool_loop` gains an `Option<&PreToolHookCallback<'_>>`
  parameter -- a boxed `dyn Fn` behind a reference, not a second generic
  type parameter alongside `request_permission`'s `F`. `request_permission`
  is REQUIRED at every call site, so a generic closure type works cleanly
  there; this hook callback is OPTIONAL (most call sites, e.g. the
  subagent loop in `crates/rokr/src/subagent.rs`, pass `None`), and a
  generic `Option<F>` parameter would force even those call sites to name a
  concrete "no-op" closure type just to spell `None::<F>`. The non-generic
  trait-object type sidesteps that at the cost of one boxed-future
  allocation per tool call when a hook IS configured -- judged an
  acceptable trade for a still-rarely-exercised path. `rokr-core` itself
  never depends on `rokr-hooks`: `crates/rokr/src/main.rs` is the only
  place that knows both `rokr_hooks::HookResult` and
  `rokr_core::PreToolHookOutcome`, and maps one down to the other.
- This same `PreToolHookCallback` shape is deliberately reused (not
  reshaped) when ticket 50 adds a `PostToolHookCallback` parameter
  alongside it, per the PRD's "Core seam" note -- landing that is expected
  to be a pure addition to `run_tool_loop`'s parameter list, not a
  redesign.
- Ticket 49's own wiring in `main.rs` is intentionally throwaway: a single
  `ROKR_PRETOOLUSE_HOOK` env var holding one hook's command line, read
  fresh per tool-call attempt, with no matcher and no support for more than
  one hook. This mirrors ticket 44's `ROKR_MCP_SERVER` interim pattern
  (superseded wholesale by ticket 45's `mcp` config schema) -- ticket 50
  replaces this env var wholesale with the real `hooks` config schema
  (per-event hook lists, the tool-name glob matcher, per-hook
  `timeout_ms`), loaded exclusively from user-scope config per this ADR's
  trust-boundary decision above.
- A hook author who wants to actually deny a call must remember `exit 2`
  specifically -- any other nonzero exit degrades to non-blocking. This is
  a real ergonomic sharp edge (a typo'd exit code silently stops blocking
  anything), accepted here because it's the exact contract the PRD
  specifies and because it mirrors how `PreToolUse`/`PostToolUse` hooks
  work in comparable prior art (e.g. Claude Code's own hooks); documenting
  it prominently in hook-authoring docs is future scope (`/hooks`
  introspection, ticket 52-equivalent), not this ADR's job.
