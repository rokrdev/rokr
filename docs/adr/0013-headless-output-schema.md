# 0013 - Headless output schema

## Status

accepted

## Context

Ticket 54 (headless-print-mode-text-output) gave `rokr -p/--print <prompt>`
a thin text-only slice: print the final assistant text to stdout, exit 0 on
success, exit non-zero otherwise. Nothing about that output was a contract
— it was one string, read by a human at a terminal.

Ticket 55 turns headless output into something scripts and tooling consume:
`--output-format json` prints one result object; `--output-format
stream-json` streams JSONL events terminated by that same object; and
`--permission-mode`/`--dangerously-skip-permissions` decide how a gated
tool call (`bash`, `write`, `edit`, an MCP tool call) is resolved with no
human present to approve it interactively. The moment headless output is
parsed by another program instead of read by a person, its shape is a
public contract with its own compatibility obligations — the same
reasoning ADR 0002/0010 apply to the on-disk config file, and ADR
0011/0012 apply to the MCP/hooks crate boundaries.

Four things needed settling before writing any code: what the result
object's fields are and why exactly those eight; how versioning discipline
for that contract is recorded, given it's not read from disk the way
`rokr.json` is; what the exit-code contract is; and what `--permission-mode`
means for each of the three tool-call shapes (`bash` commands, write/edit
diffs, MCP tool calls) with nobody available to grant permission
interactively.

## Decision

**The result-object schema (schema v1)**: exactly eight fields --
`subtype` (`success | error_max_turns | error_permission`), `session_id`,
`result`, `is_error`, `usage` (`input_tokens`/`output_tokens`/
`cache_read_tokens`/`cache_write_tokens`, mirroring `rokr_core::Usage`),
`cost_usd`, `num_turns`, `duration_ms`. `--output-format json` prints
exactly one of these, serialized with `serde_json::to_string`.
`--output-format stream-json` prints zero or more JSONL "event" lines
(`{"type": "user" | "assistant" | "system", "message": <rokr_core::Message>}`,
one per transcript entry from this run) followed by the identical result
object as the terminating line.

**"v1" is documented, not carried on the wire**: unlike `rokr.json`
(ADR 0002/0010), there is no `schema_version` field inside the JSON object
itself. The versioning discipline lives here (this ADR) and in a doc
comment on `rokr_app::result_schema::ResultObject`: adding a field is a
compatible v1 change (existing consumers that only read known keys are
unaffected); removing a field, renaming one, or changing an existing
field's meaning/type is a breaking change that must be called out in this
ADR's revision history (or a superseding ADR) and treated as "v2" in
spirit, even with no wire marker to bump. A 9th `schema_version` field was
considered and rejected — see Considered Options.

**`cost_usd` is a provisional placeholder**: always `0.0` today. The field
is present now (so a consumer parsing this schema doesn't need a later,
breaking addition just to gain the key) but its value is meaningless until
ticket 57 (cost-command-and-headless-reporting) lands real per-provider
pricing math. This is documented as a known placeholder, not silently
shipped as if it were real.

**Exit codes**: `0` success, `1` an agent/runtime error occurred during the
run (`subtype: error_max_turns` or `error_permission` carries which one —
not a separate exit code per subtype), `2` CLI misuse (e.g.
`--permission-mode bypass` without the paired
`--dangerously-skip-permissions` flag) caught before any session/provider
setup runs.

**Permission-mode semantics**: `--permission-mode deny` (the default when
the flag is absent) denies every gated tool call. `accept-edits` grants
only a write/edit (`PermissionPayload::Diff`) call; `bash` commands and MCP
tool calls stay denied under `accept-edits` too, since there is no human in
headless to review an arbitrary shell command or MCP call the way the TUI's
permission prompt does. `bypass` grants every gated call unconditionally,
but ONLY when the operator has also passed the explicit, unsafely-named
`--dangerously-skip-permissions` flag — `--permission-mode bypass` alone is
rejected as CLI misuse (exit 2). Every gated-call decision (denied by any
mode, or granted) still runs through the exact same `PreToolUse` hook path
the TUI uses first — this ticket changes nothing about hook ordering.

**`stream-json` is assembled post-hoc, not delivered live**: there is no
live event-streaming hook available out of `rokr_core::run_tool_loop`
without changing its signature (out of this ticket's file scope). Headless
is always a single, fresh submission with an empty starting transcript, so
after `SessionRunner::run_submission` returns, the same
`Arc<Mutex<Vec<Message>>>` transcript handed into it holds exactly this
run's real exchange — genuine data, just read back after completion rather
than pushed incrementally as it happens.

## Considered Options

### Eight documented fields, no wire `schema_version` (chosen)

- Pro: the schema stays exactly as compact as the ticket's acceptance
  criterion specifies; a consumer parsing known keys is forward-compatible
  with additive changes for free (no version-branching logic needed for
  the common case); versioning discipline is recorded once, here, rather
  than duplicated as a value a consumer would have to branch on anyway for
  a still-small, single-version schema.
- Con: a consumer can't programmatically ask "which schema version is
  this?" — it has to trust the ADR/doc-comment discipline instead of a
  runtime check. Accepted: `rokr.json`'s `version` field earns its keep by
  driving an actual load-time migration path (ADR 0002); headless output
  is a one-shot stdout stream with no analogous "load and migrate" step
  for a field to drive.

### Add a `schema_version` field to the wire object

- Pro: a consumer can branch on it directly instead of trusting external
  documentation; matches `rokr.json`'s existing pattern.
- Con: rejected for this ticket specifically — the acceptance line already
  enumerates a fixed field list, and the ticket's own text disagrees with
  itself about the count (its mandated unit-test name says "seven", the
  acceptance sentence lists eight; see this ticket's implementation
  report). Adding a ninth field to resolve a versioning question would
  make that count mismatch worse, not better, for no benefit yet — there
  is exactly one schema version in existence. Revisit if/when a genuine v2
  is proposed and a consumer needs to distinguish the two at runtime.

### `error_max_turns` backed by a real turn-count cap

- Pro: the subtype would mean exactly what its name says, reachable in
  practice, not just a documented-but-dormant enum variant.
- Con: rejected as out of scope — `rokr_core::run_tool_loop` has no
  turn-count cap today, and adding one is a `rokr-core` change outside
  this ticket's `files-touched`. `error_max_turns` is kept in the schema
  (the ticket's `## Context` names exactly these three subtype values) and
  used today as the closest-fit bucket for any `run_submission` failure
  that isn't the tracked permission-denial case — a future ticket that
  adds a real cap can make it precisely accurate without another schema
  change, since the field already exists.

### `accept-edits` also grants `bash`/MCP tool calls

- Pro: closer to "accept edits and let the agent actually get work done"
  in the way a human operator watching a bash command's output might.
- Con: rejected — a `bash` command or MCP tool call can do far more than
  edit a file (network access, arbitrary execution, data exfiltration via
  an HTTP-transport MCP server, per ADR 0011's `mcp_http_origins` origin
  callout); granting those unconditionally under a flag named for "edits"
  would be a silent scope-creep past what the flag's name promises. Only
  `bypass` (paired with the explicit unsafe flag) grants those.

## Consequences

- `crates/rokr-app/src/result_schema.rs` (new) owns `ResultObject`,
  `Subtype`, and `UsageObject`; `crates/rokr-app/src/headless.rs` owns the
  format/permission-mode dispatch and the exit-code mapping described
  above, fully unit-testable without spawning the `rokr` binary (only the
  end-to-end acceptance tests in `crates/rokr/tests/headless_test.rs` spawn
  it).
- Ticket 57 (cost-command-and-headless-reporting) changes `cost_usd`'s
  *value* only — the field's presence and position in the schema are
  already settled by this ADR, so that ticket is not a schema change.
- Ticket 58 (eval-case-runner-and-deterministic-assertions), per the
  original PRD's ticket graph, can assert against this exact schema
  without re-deriving it.
- A future ticket that threads a genuine per-event sink through
  `SessionRunner`/`run_tool_loop` (rather than replaying the completed
  transcript) can upgrade `stream-json` from post-hoc to truly live
  without changing this ADR's schema — only the delivery timing of the
  event lines, not their shape or the terminating result object.
