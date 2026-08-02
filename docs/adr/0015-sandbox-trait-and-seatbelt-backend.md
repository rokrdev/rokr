# 0015 - Sandbox trait and macOS Seatbelt backend

## Status

accepted

## Context

PRD phase 8 ("Sandboxing") calls for tool execution -- `bash` above all,
but any `PreviewableTool` with real filesystem/process side effects -- to
run under a deny-by-default confinement rather than the ambient
permissions of whatever user account rokr happens to run as: filesystem
writes restricted to the current workspace, network access denied unless
explicitly granted. Ticket 68 is the first of at least two tickets that
get there: it establishes the `Sandbox` abstraction and produces a real,
inspectable profile string for the macOS backend, but does not yet wrap
any subprocess in it. Ticket 69 (not yet started) is expected to take the
string this ticket produces and actually invoke `sandbox-exec` (or an
equivalent) around `BashTool`'s `execute`. Splitting the work this way
means the security-relevant part -- what the profile actually says, deny
default vs. allow-list -- gets its own unit-test suite with zero process-
spawning risk, before any code touches a real subprocess boundary.

The only sandboxing primitive readily available on macOS without a third-
party dependency is Seatbelt, invoked via the `sandbox-exec` CLI and its
S-expression profile language (`(version 1)`, `(deny default)`,
`(allow file-write* (subpath "..."))`, etc.). Apple has marked
`sandbox-exec` deprecated since macOS 10.12 with no public replacement
API ever shipped; every other application-level sandboxing tool on macOS
(Chrome, several dev-tool sandboxes) still relies on it for exactly that
reason. This ADR accepts that risk explicitly rather than silently: the
trait boundary (`Sandbox`) is what allows swapping the backend out later
if Apple removes `sandbox-exec` or a maintained alternative appears,
without every caller needing to change.

## Decision

**1. Introduce a `Sandbox` trait in `crates/rokr-tools/src/sandbox.rs`
whose only method, `profile_for(workspace_root, grants) -> String`, is a
pure function.** No subprocess spawning, no `sandbox-exec` invocation, no
filesystem access beyond reading the `Path` argument's own text. This
ticket's acceptance criterion is entirely about the profile CONTENT being
correct (deny-by-default, writes scoped to `workspace_root`, network
denied unless granted) and that is fully testable as string assertions
against `profile_for`'s return value -- there is no reason to give this
ticket's tests, or its implementation, any dependency on macOS actually
being the host OS, a real `sandbox-exec` binary being present, or a
subprocess actually running. Wiring `profile_for`'s output into a real
`sandbox-exec`-wrapped `Command` is deliberately left to ticket 69, which
will need its own (necessarily macOS-only, subprocess-spawning)
integration tests.

**2. `SeatbeltSandbox` implements `Sandbox` using Seatbelt's profile
language, accepting `sandbox-exec`'s deprecated status as tolerable for
this phase.** The profile always opens with `(version 1)` and
`(deny default)`, then layers `(allow file-write* (subpath "<workspace_root>"))`
to scope writes to the workspace, and conditionally appends
`(allow network*)` only when `grants.network` is `true`. Every allowance
is additive on top of the default deny -- there is no code path that
constructs a profile without the leading `(deny default)` line, so an
implementation bug can at worst omit an allow (fail closed, breaking a
legitimate tool call) rather than omit the deny (fail open, the actually
dangerous direction).

**3. `Grants` is a one-field struct (`network: bool`) -- ASSUMPTION,
flagged for review.** The PRD's sandboxing section discusses network as
the primary grant-able permission beyond filesystem, and today's
permission model has no representation for a network grant to plumb in
regardless: `rokr_core::PermissionPayload::Command(String)` (the payload
`bash` calls go through) carries only the literal command string, no
flag or field a permission-decision callback could use to populate
`Grants { network: true }`. Concretely, this means ticket 69's caller,
wiring a real `sandbox-exec`-wrapped `BashTool::execute`, has no source
of truth to construct anything other than `Grants { network: false }`
until a later ticket extends `PermissionPayload` (or introduces a
parallel channel) with a way for the user or a policy to say "this
command may reach the network." `Grants` is kept to the one field the
current call chain can ever populate, rather than speculatively adding
fields (a workspace-root override list, specific host allow-lists, etc.)
this phase has no caller for yet -- extending it is a small, additive
change when a real caller and a real permission surface exist together.

## Considered Options

### `Sandbox` trait + pure `profile_for`, subprocess wiring deferred to ticket 69 (chosen)

- Pro: the security-critical logic (what the profile says) is unit-
  tested in full isolation, no macOS-only CI runner or real
  `sandbox-exec` binary required to verify deny-by-default and grant
  semantics are correct.
- Pro: matches the PRD's own phased breakdown and keeps ticket 68's
  vertical slice small and reviewable -- one string-generation
  responsibility, not string generation entangled with process
  spawning and its own failure modes (missing binary, profile syntax
  errors surfacing only at `sandbox-exec` runtime, etc.).
- Con: `SeatbeltSandbox` on its own does nothing to protect a real tool
  call yet -- a reader could mistake ticket 68 landing for the sandbox
  being "on." Mitigated by this ADR and the module doc comment both
  saying explicitly that subprocess wrapping is ticket 69's job.

### Wrap `sandbox-exec` around `BashTool::execute` directly in this ticket

- Pro: would deliver end-to-end protection in one ticket rather than
  two, no intermediate state where `SeatbeltSandbox` exists but isn't
  wired to anything.
- Con: rejected. Conflates two different kinds of correctness (profile
  content vs. subprocess wiring) into one ticket, forcing tests that
  either mock `sandbox-exec` (weak signal on whether the real profile
  syntax is even valid) or actually spawn processes under Seatbelt in
  every CI run (slow, macOS-only, harder to keep deterministic). Also a
  much larger vertical slice than TDD-per-ticket calls for.

### Skip Seatbelt, wait for a non-deprecated macOS sandboxing primitive

- Pro: avoids building on an API Apple could remove.
- Con: rejected. No alternative has shipped since the 10.12 deprecation
  years ago, and every comparable tool still uses `sandbox-exec` for
  lack of one; waiting indefinitely means phase 8 never ships macOS
  sandboxing at all. The `Sandbox` trait exists precisely so this
  choice is revisitable without a call-site rewrite if that changes.

### `Grants` grows fields speculatively (workspace overrides, host allow-lists, etc.)

- Pro: might save a later ticket from having to widen the struct.
- Con: rejected. Nothing in today's call chain -- `PermissionPayload`
  above all -- can populate fields beyond `network` yet; adding them now
  would be untested surface area with no real producer or consumer,
  the opposite of the vertical-slice, no-speculative-flexibility
  approach this codebase otherwise follows (see decision 3).

## Consequences

- `crates/rokr-tools/src/sandbox.rs` adds `pub mod sandbox;` to
  `crates/rokr-tools/src/lib.rs`'s module list, alongside the other
  one-file-per-concern tool/utility modules.
- `SeatbeltSandbox::profile_for` is safe to call from anywhere (tests,
  future non-macOS platforms) without side effects -- it will produce a
  Seatbelt-flavored string even if invoked on a non-macOS host, since
  it does no OS detection itself. Any OS-gating (only actually invoking
  `sandbox-exec` on macOS, choosing a different `Sandbox` impl
  elsewhere) is left to ticket 69's caller.
- Ticket 69 has a concrete, tested contract to build against:
  `Sandbox::profile_for(workspace_root, grants) -> String`, plus the
  explicit reminder (decision 3) that it will only ever be able to pass
  `Grants { network: false }` until the permission model grows a way to
  say otherwise.
- If `sandbox-exec` is ever removed from macOS, only `SeatbeltSandbox`'s
  implementation (and whatever ticket 69 adds around invoking it) needs
  to change -- callers coded against the `Sandbox` trait do not.
