# 0016 - Permission mode policy layer

## Status

accepted

## Context

ADR 0005 established that permission gating ships alongside the tool
loop, not bolted on later, and deferred allowlist/auto-accept modes and
OS-level sandboxing to Phase 8. Ticket 55 delivered the headless half of
that deferral: `crates/rokr-app/src/cli.rs`'s `PermissionMode` enum
(`Deny` / `AcceptEdits` / `Bypass`), driven by `--permission-mode` and
`--dangerously-skip-permissions` for non-interactive (`--print`) runs.
That enum answers "what should headless mode do," but nothing yet
answers the more general question both the TUI and headless driver need:
given a mode, a tool name, and whatever the user has already agreed to
this session, should a specific tool call be allowed, denied, or does a
human need to be asked?

Today that question is answered ad hoc at the `rokr_core::run_tool_loop`
call site: only a gated tool call (`preview()` returns `Some(_)`) reaches
the `request_permission: F` callback at all (`F: Fn(PermissionRequest) ->
Fut, Fut: Future<Output = bool>`), and headless mode's
`--dangerously-skip-permissions` / `PermissionMode` handling lives
entangled with that wiring. There is no reusable, TUI-free place a
"remember this tool for the rest of the session" grant (the interactive
allowlist ADR 0005 deferred) can be recorded and checked before ever
reaching `request_permission` -- and no single place that proves, with a
unit test and zero TUI rendering, that `Bypass` really does allow
everything, `Deny` really does deny before any prompt, and so on.

Ticket 71 extracts that decision into `crates/rokr-app/src/
permission_policy.rs`: a pure `PermissionPolicy::resolve` function and a
`SessionGrants` accumulator, with no dependency on `rokr-tui` or on
`rokr_core::run_tool_loop` itself. This is explicitly an *extension* of
ticket 55's `PermissionMode`, not a second, parallel concept -- per the
PRD's phase 8 workstream 2 language, "the same concept... not a second,
parallel notion." `resolve` takes `crate::cli::PermissionMode` directly
as its `mode` parameter; there is no new `permission_policy::Mode`-style
enum duplicating `Deny`/`AcceptEdits`/`Bypass`.

Wiring `PermissionPolicy::resolve`'s `Resolution::Prompt` outcome into
the real `request_permission` callback -- and populating `SessionGrants`
interactively from the TUI's "remember for this session" affordance -- is
ticket 72's job (`tui-session-allowlist-grant`), not this one. This
ticket's acceptance bar is the policy function's own unit-test suite,
matching how ticket 63's `CommandRegistry` was tested directly against
its public API before anything wired it into a caller.

## Decision

**1. `PermissionPolicy::resolve(mode, tool_name, path, grants) ->
Resolution` is a pure function with no I/O and no TUI dependency.**
`Resolution` is a three-variant enum (`Allow`, `Deny`, `Prompt`).
`Allow`/`Deny` are terminal -- the caller acts on them directly. Only
`Resolution::Prompt` is meant to ever reach `rokr_core::run_tool_loop`'s
`request_permission` callback; `resolve` itself has no knowledge that
callback exists. This mirrors ADR 0015's `Sandbox::profile_for`: the
security-relevant decision logic is a pure function with a
string-assertion-grade (here, enum-assertion-grade) test suite, fully
decoupled from wherever it eventually gets called from a real subprocess
or a real TUI event loop.

**2. Resolution follows a fixed precedence order, grants checked first:**

1. If `grants` has a prior grant recorded for `tool_name`, return
   `Allow` -- **regardless of `mode`**, including `Deny`. A grant is a
   record of something the user already explicitly agreed to for this
   session; re-litigating it against the ambient mode on every
   subsequent call would defeat the point of "remember this for the
   session."
2. Else if `mode == Bypass`, return `Allow` for any tool.
3. Else if `mode == Deny`, return `Deny`.
4. Else if `mode == AcceptEdits`, return `Allow` only for `tool_name ==
   "write"` or `"edit"`, `Prompt` for anything else.
5. Otherwise, return `Prompt`.

Grants overriding `Deny` is the one precedence rule that isn't obvious
from mode semantics alone, so it is called out explicitly here and
covered by its own test
(`a_prior_session_grant_for_a_tool_resolves_allow_without_prompting_again`,
which asserts the grant beats `Deny` specifically, not just that the
grant resolves to `Allow` in isolation).

**3. `SessionGrants` is tool-name keyed only -- ASSUMPTION, flagged as a
deliberate scope cut, same shape as ADR 0015's `Grants { network: bool }`
cut.** It wraps a `HashSet<String>` of tool names with a `grant(&mut
self, tool_name)` recorder and an internal membership query `resolve`
calls. `resolve`'s public signature already accepts a `path:
Option<&Path>` parameter -- the ticket's acceptance criterion fixes that
shape -- but nothing in this ticket's semantics branches on it; it is
unused (`#[allow(unused_variables)]`, documented in `resolve`'s doc
comment) rather than removed, because a natural follow-on ticket may want
path-keyed grants (e.g. "always allow `write` to `src/generated/*`"
without granting `write` everywhere) and this shape doesn't foreclose
that. Building path-keyed grant storage now, with no caller that needs
it, would be exactly the kind of speculative flexibility ADR 0015's
`Grants` decision (and this codebase's general practice) argues against.

## Considered Options

### Pure `resolve` function + tool-name-keyed `SessionGrants`, wiring deferred to ticket 72 (chosen)

- Pro: the precedence logic (the actual security-relevant behavior) is
  unit-tested in complete isolation -- no TUI event loop, no
  `run_tool_loop` closure, no async runtime required to prove `Bypass`
  allows everything or `Deny` denies before any prompt.
- Pro: reuses `crate::cli::PermissionMode` directly rather than
  introducing a parallel enum, so headless and interactive callers share
  one source of truth for what the three modes mean -- exactly what ADR
  0005 and ticket 55 set up for extension.
- Con: `PermissionPolicy` does nothing to a real tool call yet -- a
  reader could mistake ticket 71 landing for the allowlist prompt being
  wired up. Mitigated by this ADR and the module doc comment both
  stating explicitly that connecting `Resolution::Prompt` to
  `request_permission`, and populating `SessionGrants` from a real user
  action, is ticket 72's job.

### Wire `resolve` into `run_tool_loop`'s `request_permission` callback directly in this ticket

- Pro: would deliver an end-to-end interactive allowlist in one ticket
  rather than two.
- Con: rejected. Conflates policy-decision correctness with callback
  wiring and TUI-side grant recording into one vertical slice, the same
  problem ADR 0015 rejected for `Sandbox` + `sandbox-exec` invocation.
  Testing the wired-up version would require either mocking
  `run_tool_loop`'s closure shape (weak signal on whether the precedence
  logic itself is right) or driving it through TUI/headless integration
  tests for what is fundamentally a pure-function question.

### Introduce a new, parallel `permission_policy::Mode` enum instead of reusing `cli::PermissionMode`

- Pro: would decouple the policy module from `rokr-app`'s CLI-parsing
  module (`ValueEnum` derive, clap-specific concerns).
- Con: rejected outright. This is precisely the "second, parallel
  notion" the PRD's phase 8 workstream 2 language warns against, and
  ticket 55 already made `PermissionMode` `pub` and re-exported from
  `crate::lib` for exactly this kind of reuse. Two enums with identical
  variants inevitably drift (a new mode added to one and not the other)
  and give calling code an ambiguous choice about which to construct
  from `--permission-mode`.

### Make `SessionGrants` path-keyed now, since `resolve` already takes a `path` parameter

- Pro: would use the `path` parameter for something rather than leaving
  it unused this ticket.
- Con: rejected. No caller exists yet that can supply a meaningful path
  policy (ticket 72's interactive grant flow, the actual producer of
  session grants, is out of scope here), so path-keyed storage would be
  untested surface area with no real consumer -- the same reasoning ADR
  0015 gave for keeping `Grants` to its one populatable field rather than
  speculatively widening it.

## Consequences

- `crates/rokr-app/src/permission_policy.rs` adds `pub mod
  permission_policy;` to `crates/rokr-app/src/lib.rs`'s module list,
  alongside the other one-file-per-concern modules (`cli`, `commands`,
  `headless`, `result_schema`, `runner`, `subagent`, `upgrade`).
- `PermissionPolicy::resolve` and `SessionGrants` are safe to call from
  anywhere (unit tests, a future TUI event handler, a future headless
  call site) with no side effects and no dependency on `rokr-tui` or
  `rokr_core::run_tool_loop` -- callers decide what to do with
  `Resolution::Allow`/`Deny`/`Prompt` themselves.
- Ticket 72 has a concrete, tested contract to build against:
  `PermissionPolicy::resolve(mode, tool_name, path, grants) ->
  Resolution`, plus the explicit reminder (decision 3) that
  `SessionGrants` can only be populated by tool name until a later
  ticket gives it a path-keyed grant path and a real producer for one.
- Because grants are checked first and override even `Deny`, any future
  caller populating `SessionGrants` must treat `grant()` as a
  security-relevant action (equivalent to a standing allowlist entry for
  that tool name), not a cosmetic "don't ask again this turn" convenience
  -- this is the behavior
  `a_prior_session_grant_for_a_tool_resolves_allow_without_prompting_again`
  exists to pin down.

## Amendment (ticket 72)

Ticket 72 (`tui-session-allowlist-grant`) wires `PermissionPolicy::resolve`'s
`Resolution::Prompt` outcome into the real interactive TUI, and populates
`SessionGrants` from a real user action (pressing `r`, "allow and
remember," at a permission prompt) -- the two things this ADR's Decision
section explicitly deferred to it. Doing so required one signature change
this ADR did not originally anticipate, authorized by the orchestrator as
an amendment to this ticket's own "no new API" scope:

**`PermissionPolicy::resolve`'s `mode` parameter widens from
`crate::cli::PermissionMode` to `Option<PermissionMode>`.** The grant
precedence rule (decision 2, item 1 above) is completely unchanged -- a
prior grant for `tool_name` still returns `Allow` regardless of `mode`,
`None` included. The reason for the change: the interactive TUI has no
ambient permission mode at all. `--permission-mode` /
`--dangerously-skip-permissions` are headless-only flags (per
`crate::cli`'s own doc comment, unchanged by this ticket), so a TUI call
site has nothing meaningful to construct a `PermissionMode` from. Before
this amendment, every call site was forced to pick SOME `PermissionMode`
variant even when none applied, which would have meant either silently
reusing `Deny` (misleading -- the TUI isn't "denying," it's "prompting,
same as it always did") or `AcceptEdits` (wrong -- it would have started
auto-allowing `write`/`edit` with no user in the loop, a real behavior
change nobody asked for). `None` names the TUI's actual situation
directly: no ambient mode, so (absent a grant) always `Prompt` --
identical to today's un-widened TUI behavior for a call the user hasn't
granted yet. `Some(mode)` preserves the original 3-mode behavior
(`Bypass`/`Deny`/`AcceptEdits`) completely unchanged.

**Correction (pre-ship review F-005):** the paragraph above originally
claimed "headless continues to pass `Some(mode)` and is unaffected end to
end," verified only by `accept_edits_permission_mode_grants_file_
writes_but_denies_bash_execution` and `bypass_permission_mode_without_
dangerously_skip_permissions_exits_two_with_stderr_error` staying green.
That claim was **false** at the time it was written: those two tests
staying green only proved headless's OBSERVABLE behavior was unchanged,
not that headless actually called `PermissionPolicy::resolve` with
`Some(mode)` at all. In fact `crate::headless::HeadlessPermissionRequester
::request` never called `resolve` -- it independently re-derived the exact
same `Deny`/`Bypass`/`AcceptEdits` dispatch inline, entirely bypassing this
policy layer. That was exactly the dual-resolver architecture this ADR's
own Decision section (and the PRD) exists to forbid; it went undetected
pre-ship because the two regression tests above only assert on
externally-observable outcomes, which happened to match by construction
(the duplicated logic was a faithful copy at the time).

The actual fix (pre-ship review F-005) threads a new
`permission_mode: Option<PermissionMode>` field through
`crate::runner::SessionRunner` -- `None` for the interactive TUI
(`crates/rokr/src/main.rs`'s `SessionRunner` construction), `Some(mode)`
for headless (`crate::headless::run_result_object`'s `SessionRunner`
construction, the field's ONE non-test `Some(_)` call site). That field is
now the value BOTH of `run_submission`'s `PermissionPolicy::resolve` call
sites use (the parent's own permission closure in `runner.rs`, and --
forwarded through `subagent::SubagentTool` -- the one `subagent::
run_subagent`'s `tagged_request_permission` closure uses), replacing what
were previously two independent hardcoded `None`s.
`HeadlessPermissionRequester` no longer stores or dispatches on `mode` at
all: its `request` method is now reduced to handling whatever
`PermissionPolicy::resolve` did NOT already decide unconditionally. The two
regression tests named above (plus `default_permission_mode_denies_gated_
tool_call_without_flag`, headless's `Deny`-mode case) all still pass
unchanged against this real wiring.

**Correction (post-round-1 re-critique, R-002):** the paragraph above
originally went on to describe `runner.rs`'s and `subagent.rs`'s permission
closures routing BOTH `Resolution::Deny` and `Resolution::Prompt` through
the SAME arm, both invoking `permission.request`/`request_permission` --
with `HeadlessPermissionRequester::request` (always deny-and-record) as the
one leaf both resolutions funneled into. That was a **violation** of this
ADR's own Decision 1 ("only `Resolution::Prompt` is meant to ever reach
`rokr_core::run_tool_loop`'s `request_permission` callback"), not a faithful
implementation of it: routing `Deny` through the human-facing prompt
callback at all is a latent hazard, since any future interactive driver that
respects a permission mode could let a single accidental keypress at the
prompt override an explicit deny-mode denial. It went undetected at the time
because headless's `request` always answers `Deny` regardless of which
`Resolution` sent it there, so the OBSERVABLE outcome (a denied call, `subtype:
error_permission`) was identical whether `Deny` was intercepted upstream or
routed through the callback -- exactly the kind of "tests match by
construction, not by the code actually satisfying the invariant" gap the
F-005 correction above already flagged once for this same ADR, this time one
layer downstream of it.

The actual fix (R-002) adds `PermissionRequester::note_denied_without_prompt(&self)`
(default no-op) to the trait in `runner.rs`, and the mirrored
`subagent::NoteDeniedCallback` type (a boxed, side-effect-only
`Box<dyn Fn() + Send + Sync>`) for `subagent::run_subagent`, which cannot
hold a typed `H: PermissionRequester` directly (that generic is erased at
`SubagentTool`'s boundary, per ADR 0009). Both `runner.rs`'s
`request_permission` closure and `subagent.rs`'s `tagged_request_permission`
now split `Resolution::Deny` and `Resolution::Prompt` into separate match
arms: `Deny` calls `note_denied_without_prompt()`/`(note_denied_without_prompt)()`
and returns `false` directly, WITHOUT ever calling `request`/`request_permission`;
`Prompt` is the only resolution that reaches the human-facing callback.
`HeadlessPermissionRequester::note_denied_without_prompt` overrides the
trait's default no-op to do exactly what its old merged-arm `request` path
used to do for a `Deny` (`self.denied.store(true, SeqCst)`), so
`subtype: error_permission` reporting is unaffected -- headless still denies
and records a `Deny`-mode call the same way observably, just without ever
routing it through the prompt callback to get there. The interactive TUI
(`permission_mode: None`) never hits `Resolution::Deny` in the first place
(`PermissionPolicy::resolve`'s `None` arm only ever yields `Allow` or
`Prompt`), so its default no-op `note_denied_without_prompt` is never
actually invoked in production -- `PermissionHandle` doesn't need to
override it. All headless permission-mode tests (including the two named
above) still pass unchanged; `crates/rokr-app/src/runner.rs`'s and
`subagent.rs`'s own test suites gained dedicated regression tests
(`deny_mode_bash_call_never_reaches_request_only_note_denied_without_prompt`
and `subagent_deny_mode_never_reaches_request_permission_callback`) that
fail with a panic inside a `request`/`request_permission` stub if a future
change ever re-merges the two arms.

### Rejected: add a 4th `PermissionMode` variant (e.g. `Interactive` or
`NoAmbientMode`)

- Pro: keeps `resolve`'s `mode` parameter non-`Option`, so every call site
  is still forced to supply something, and the type alone documents all
  the modes that exist in one enum.
- Con: rejected. `PermissionMode` is `crate::cli::PermissionMode` --
  ticket 55's headless-only enum, driven by the headless-only
  `--permission-mode` flag and documented as such in `cli.rs`'s own doc
  comment. Adding a variant that can never be selected via
  `--permission-mode` (since the TUI has no such flag) would mean
  `cli.rs`'s enum no longer maps 1:1 onto "the modes `--permission-mode`
  accepts," which is exactly the property ADR 0005/ticket 55 established
  and this ADR's original Decision section relied on ("reuses
  `crate::cli::PermissionMode` directly rather than introducing a
  parallel enum"). It would also require touching `crates/rokr-app/src/
  cli.rs`, which ticket 72's scope explicitly excludes.

### Rejected: make `SessionGrants::is_granted` public so callers bypass
`resolve` entirely for the "already granted" check

- Pro: would let a caller check "is this already granted?" without
  needing a `mode` argument at all, sidestepping the `Option` question.
- Con: rejected. This would split the permission decision across two call
  sites -- one path through `is_granted` directly, another through
  `resolve`'s full precedence chain -- reintroducing exactly the "ad hoc,
  scattered decision logic" this ADR's original Context section
  identified as the problem ticket 71 fixed. `resolve` staying the SINGLE
  entry point every caller goes through (this ADR's decision 1) is what
  makes the precedence rule (grant beats even `Deny`) a property of one
  function's tests rather than something every call site has to
  re-derive and potentially get wrong independently.

## Amendment (ADR 0019)

ADR 0019 (`docs/adr/0019-read-only-git-permission-carveout.md`) extends
`PermissionPolicy::resolve` with command-content awareness for the first
time: a new `command: Option<&str>` parameter, threaded from every call
site (`None` except on the `bash` path, where the raw command string is
extracted before its `PermissionPayload` is consumed elsewhere). This is
a narrow widening of `resolve`'s inputs, not a departure from decision 1
above -- `resolve` remains the single entry point every caller goes
through; a read-only-git carve-out checked upstream of `resolve` was
considered and rejected specifically because it would reintroduce the
dual-resolver hazard this ADR's own F-005 correction, above, documents.

The new precedence placement: below `Deny`, above `AcceptEdits` -- a
prior grant, then `Bypass`, then `Deny`, then the read-only-git
carve-out, then `AcceptEdits`, then the default `Prompt`. The carve-out
only ever converts what would otherwise be `Prompt` into `Allow`; it
never overrides `Deny`, the absence of a grant, or `Bypass`. See ADR
0019 for the full rationale, the classifier's exact conservatism spec,
and the accepted residual risks.
