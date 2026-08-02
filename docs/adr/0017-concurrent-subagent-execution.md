# 0017 - Concurrent subagent execution

## Status

accepted

## Context

Ticket 30 (agent-as-tool) gave `rokr-app` a `SubagentTool`: invoking it runs
a fresh, synchronous `rokr_core::run_tool_loop` to completion against a
subagent's own prompt and a read-only tool subset, then returns only the
subagent's final text. `run_tool_loop`'s own batch-dispatch loop (the `for
(id, name, input) in tool_uses` loop in `crates/rokr-core/src/lib.rs`) has
always awaited every `ToolUse` in a reply strictly sequentially, in
original order. That is the right default for most tools -- `bash`,
`write`, `edit` all have real side effects a user is being asked to permit
one at a time -- but it is a real cost for `subagent`: a single assistant
reply naming two independent subagent tasks (e.g. "research X" and
"research Y") pays the FULL wall-clock cost of both round trips back to
back, even though the two subagent calls share no state and cannot
interfere with each other (the depth-1 cap means a subagent's own tool set
excludes `subagent` itself, so there is no cross-call mutation risk to
worry about).

Ticket 73 (concurrent-subagent-fan-out) makes multiple `subagent` calls in
one batch run with genuine wall-clock overlap, while leaving every other
tool's dispatch (gated or not) exactly as sequential as it already was.

This ADR is essentially "how to get real concurrency here without
violating ADR 0009's `Send` constraint" -- see ADR 0009's own
"AFIT `Send` edge case to watch for" paragraph, which flags exactly this
scenario as something a later phase would need to solve without
`tokio::spawn`.

## Decision

**1. `ExecutableTool` gains a `fn concurrent_safe(&self) -> bool { false }`
default method; `rokr-app`'s `SubagentTool` is the only override.** Every
built-in tool (`read`/`glob`/`grep`/`ls`/`websearch`/`bash`/`write`/`edit`/
`webfetch`, all wired through the `impl_executable_tool!`/
`impl_executable_tool_gated!` macros) keeps the `false` default untouched.
This mirrors `ExecutableTool::preview`'s existing default-method shape
(`None` unless a tool overrides it) -- a trait method, not a special case
bolted onto `run_tool_loop`'s dispatch code. The alternative --
`run_tool_loop` string-matching on `name == "subagent"` -- was rejected
outright per this ticket's own hard constraint: it would make `rokr-core`
implicitly aware that a specific tool name in a crate it doesn't even
depend on (`rokr-app`) is special, the same "ad hoc, scattered decision
logic" smell ADR 0016 (`PermissionPolicy::resolve`) was written to avoid
for permission decisions. A trait method keeps the decision where the
capability actually lives: only a tool implementation itself knows whether
its own execution is safe to overlap with another concurrent-safe call.

**2. Same-task concurrency via `futures::future::join_all`, never
`tokio::spawn`.** ADR 0009 already establishes why: native `async fn`
futures are only provably `Send` when the compiler sees the concrete,
monomorphized type, not across an abstract boundary such as a generic
`P: Provider` bound or (here) `dyn ExecutableTool`. `run_tool_loop` holds
its tool set as `tools: &[&dyn ExecutableTool]` -- a trait-object
boundary by construction -- so a batch of borrowed `&dyn ExecutableTool`
futures can never be proven `Send + 'static`, which is exactly what
`tokio::spawn` requires. `futures::future::join_all` sidesteps this
entirely: it polls a collection of futures concurrently WITHIN the current
task, requiring only that they share one `Future::Output` type -- no
`Send`, no `'static`, no spawning onto the runtime's task scheduler at
all. This is genuine wall-clock concurrency (both calls' `.await` points
are polled in the same turn of the executor, so a slow call does not block
a fast one behind it), just not parallelism across OS threads for this
specific batch -- which is the right trade for I/O-bound provider round
trips, not CPU-bound work.

**3. Batching/ordering scheme: partition the batch into two groups by
`concurrent_safe()`, run each group in original relative order, merge by
original index.** Concretely, in `run_tool_loop`'s dispatch code:

- The per-call pipeline (`PreToolUse` hook, then unless vetoed
  preview/permission-gate/execute, then `PostToolUse` hook) is factored
  into one reusable closure, `dispatch_one`, so both groups below share
  the exact same logic -- ticket 73 changes nothing about what happens to
  an individual call, only how many calls are in flight at once.
- Every `(index, id, name, input)` in the batch is classified by looking
  up the named tool and calling `concurrent_safe()` on it (a tool name
  with no match in `tools` -- the existing "unknown tool" error case --
  is treated as non-concurrent; there is no tool instance to ask, and
  `dispatch_one` already produces the correct error `ToolResult` for it
  either way).
- The non-concurrent-safe group is awaited one at a time, in original
  order, via a plain `for` loop -- byte-for-byte the pre-ticket-73
  behavior for every tool that doesn't override `concurrent_safe()`.
- The concurrent-safe group is awaited together via
  `futures::future::join_all`, each future tagged with its original
  index.
- A `Vec<Option<ContentBlock>>` sized to the batch, indexed by ORIGINAL
  position, is filled in by whichever group produces each entry
  (regardless of completion order within the concurrent group); the final
  `Vec<ContentBlock>` handed to `transcript.push` is built by unwrapping
  every slot in order. This is what guarantees `ToolResult` order in the
  transcript always matches the original `tool_uses` order the model
  asked for, independent of which calls ran concurrently or how fast each
  one finished.

This was the simplest scheme that satisfies the ticket's ordering
requirement without touching the non-concurrent path's semantics at all.
The two groups are not interleaved with each other in wall-clock time
(sequential group runs to completion before the concurrent group starts,
in this implementation) -- nothing in the ticket requires temporal
interleaving BETWEEN the two groups, only genuine overlap WITHIN the
concurrent-safe group and correct final ordering, both of which this
scheme delivers.

**4. Scope boundary: permission-prompt serialization for concurrent
subagents is explicitly deferred, not solved here.** `SubagentTool` is not
a `PreviewableTool` (its `preview` stays the trait default, `None`), so a
top-level `subagent` call itself never reaches `request_permission` --
but a subagent's OWN internal tool calls can (e.g. if a future ticket
widens a subagent's tool subset beyond read-only). If two concurrent
subagents both hit a gated tool at the same moment, today's
`request_permission` callback has no defined behavior for "two prompts
want the user's attention at once" -- that is a real, distinct problem
(prompt ordering/queuing at the TUI or headless layer) left to a
follow-on ticket. This ticket's own test scenarios are deliberately
prompt-free (`SubagentTool`'s depth-1 read-only tool subset has no gated
tools in it today, and the acceptance test's mocked provider replies with
plain text, never a nested `ToolUse`), specifically so a permission-prompt
race can never make this ticket's tests flaky -- see decision 5 below for
where that constraint mattered concretely.

**5. Acceptance test 2's harness deviates from the originally sketched
design.** The plan was a `wiremock::Respond` impl blocking on a shared
`std::sync::Barrier` inside `respond()`, mirroring the unit test's
`tokio::sync::Barrier`-based fake tool. That does not work: `wiremock`
0.6's `BareMockServer` (`mock_server/hyper.rs`) handles every incoming
request under a single `tokio::sync::RwLock::write()` held for the entire
`handle_request` call -- exactly where `Respond::respond()` runs. A single
`MockServer` therefore only ever has ONE request "inside `respond()`" at a
time, by wiremock's own design; a 2-party barrier in `respond()` deadlocks
unconditionally regardless of how concurrently the client dispatches,
because the second request can never even reach `respond()` until the
first (stuck on the barrier) releases that lock. This was confirmed
empirically: the barrier-based version of the test hung indefinitely both
BEFORE and AFTER implementing concurrent dispatch in `run_tool_loop`,
proving the harness -- not `run_tool_loop` -- was the problem.

The acceptance test instead proves genuine overlap with timing:
`ResponseTemplate::set_delay` on the mocked response, combined with
`wiremock`'s own documented behavior (in `hyper.rs`) of releasing the
per-request write lock BEFORE awaiting that delay. Two `SubagentTool`
calls awaited concurrently therefore take roughly one delay's worth of
wall-clock time in total; awaited sequentially, roughly two. A
`tokio::time::timeout` bound set strictly between those two figures
reproduces the same `Err(Elapsed(()))` red the barrier design was meant
to produce -- verified by temporarily forcing `concurrent_safe()` to
always return `false` and confirming the test fails with exactly that
error, then restoring the real implementation and confirming it passes,
consistently, across repeated runs.

## Considered Options

### `concurrent_safe()` trait method, `join_all` over the concurrent-safe partition (chosen)

- Pro: the "is this tool safe to overlap" decision lives on the tool
  itself, not as a name string `run_tool_loop` has to know about --
  `rokr-core` stays unaware that `rokr-app`'s `SubagentTool` exists at
  all, exactly like `preview`'s existing default-method precedent.
- Pro: `join_all` requires no `Send`/`'static` bound beyond what
  `dispatch_one`'s closure already needs, so it works cleanly behind the
  `&[&dyn ExecutableTool]` boundary ADR 0009 flags as unprovable-`Send`
  for `tokio::spawn`.
- Con: concurrency here is single-task (no OS-thread parallelism for the
  batch) -- acceptable, even preferable, since the work being overlapped
  is I/O-bound provider round trips, not CPU-bound computation.

### Tool-name string match (`name == "subagent"`) in `run_tool_loop`

- Pro: no new trait method, no change to `ExecutableTool`'s surface.
- Con: rejected outright per this ticket's own hard constraint. Makes
  `rokr-core` implicitly coupled to a tool name defined in `rokr-app`, a
  crate `rokr-core` does not and should not depend on -- the exact
  "scattered, ad hoc decision logic" pattern ADR 0016 already argues
  against for a structurally similar problem (permission resolution).
  Also does not generalize: any future concurrent-safe tool would need
  another hardcoded name check.

### `tokio::spawn` the concurrent-safe calls onto the runtime

- Pro: true OS-thread parallelism, not just single-task interleaving.
- Con: rejected. Requires `Send + 'static` on the spawned future; per ADR
  0009, a future built from `&dyn ExecutableTool` (necessarily borrowed,
  necessarily crossing a trait-object boundary) cannot be proven `Send`
  by the compiler no matter how the concrete implementations behave --
  ADR 0009 spells this out as precisely the scenario to avoid spawning
  across. Would also require every `ExecutableTool` implementor's
  `execute_boxed` future to become `'static` (owning, not borrowing,
  everything it touches), a much larger and unrelated redesign.

### Interleave the sequential and concurrent groups strictly by original position (e.g. run everything through one combined pipeline that dispatches each call the instant its predecessor unblocks)

- Pro: would preserve real-time ordering between the two groups, not just
  final result-vector ordering.
- Con: rejected as unnecessary complexity. Nothing in the ticket's
  acceptance criteria asks for temporal interleaving between
  concurrent-safe and non-concurrent-safe calls -- only that (a)
  concurrent-safe calls overlap with EACH OTHER and (b) the final
  `ToolResult` order matches original `tool_uses` order. The chosen
  two-phase scheme satisfies both with far less code and no risk of a
  non-concurrent-safe call's permission prompt racing a concurrent-safe
  call's own execution.

## Consequences

- `crates/rokr-core/src/lib.rs`: `ExecutableTool::concurrent_safe()` is a
  new default method (`false`); `run_tool_loop`'s dispatch loop is
  restructured into a `dispatch_one` closure plus a two-phase
  (sequential-then-concurrent) batch runner, but every existing caller's
  observable behavior for non-concurrent-safe tools -- which remains
  every built-in tool -- is byte-for-byte unchanged; the full existing
  `run_tool_loop` test suite passed unmodified against the new
  implementation.
- `crates/rokr-core/Cargo.toml` gains `futures = { workspace = true }` as
  a real (non-dev) dependency; the workspace root `Cargo.toml` gains
  `futures = "0.3"` to `[workspace.dependencies]`.
- `crates/rokr-app/src/subagent.rs`: `SubagentTool::concurrent_safe()`
  overrides to `true`, the one override in the codebase today. Any future
  tool wanting the same treatment (in `rokr-app` or elsewhere) overrides
  the same method -- no `run_tool_loop` change required.
- A batch mixing gated tools (e.g. `bash`) and `subagent` calls keeps the
  gated tool's permission prompt fully serialized against the rest of the
  sequential group, exactly as before; only calls between multiple
  concurrent-safe tools in the same batch actually overlap.
- Permission-prompt serialization for concurrent subagents remains
  unsolved and is left to a follow-on ticket -- today's `SubagentTool`
  tool subset has no gated tools, so this gap has no live call path yet,
  but widening a subagent's tools to include a gated one before that
  follow-on ticket lands would reopen it.
- `crates/rokr-app/src/subagent.rs`'s acceptance test documents, in its
  own doc comment, exactly why it diverges from a barrier-based harness
  and how its timing-based bound was chosen and verified against a
  deliberately-forced-sequential run -- a future reader modifying
  `wiremock` usage elsewhere in this codebase should be aware a single
  `MockServer` cannot host two truly concurrent in-flight `respond()`
  calls.
