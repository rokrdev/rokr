# 0019 - Read-only git permission carve-out

## Status

accepted (human approved 2026-08-05)

## Context

Phase 9's git-workflow PRD (`.workflow/docs/phase-9-git-workflow.md`,
"Problem Statement") names a real friction point: every git invocation --
including a harmless, read-only one like `git status` or `git log` -- has
to clear the exact same `bash` permission prompt as a destructive one. A
user who wants rokr to routinely orient itself with `git status` mid-session
either grants `bash` blanket session-wide (over-broad -- it also covers
every mutating command) or re-approves an identical, harmless invocation
dozens of times a session (friction with no safety payoff, since these
commands cannot mutate anything). User story 2 asks for `status`/`diff`/
`log`/`show`/`blame` to stop prompting; user story 3 asks, just as firmly,
that anything not unambiguously one of those five keep prompting, so the
convenience never quietly widens into something that can mutate the
repository; user story 15 asks that headless get the identical carve-out,
not a second implementation of it.

`PermissionPolicy::resolve` (`crates/rokr-app/src/permission_policy.rs`,
ADR 0016) is where this has to land. ADR 0016 decision 1 made `resolve`
the single, pure, TUI-free entry point every caller -- the TUI's own
`request_permission` closure in `runner.rs`, and, forwarded through
`SubagentTool`, `subagent::run_subagent`'s `tagged_request_permission`
closure -- goes through for every permission decision, specifically so
the precedence rules governing `Allow`/`Deny`/`Prompt` are a property of
one function's test suite rather than something every call site has to
re-derive independently. ADR 0016's own "Amendment (ticket 72)" section
records what happens when that invariant slips: pre-ship review finding
F-005 caught `HeadlessPermissionRequester::request` independently
re-deriving the same `Deny`/`Bypass`/`AcceptEdits` dispatch inline,
entirely bypassing `resolve` -- undetected pre-ship because the two
regression tests guarding it only asserted on externally observable
outcomes, which happened to match by construction while the logic was a
faithful copy. A second review pass, R-002, found a narrower instance of
the identical class of bug one layer downstream, inside `resolve`'s own
callers this time (`Deny` and `Prompt` sharing a match arm that routed
both through the same human-facing callback). Both corrections cost a
dedicated regression test apiece
(`deny_mode_bash_call_never_reaches_request_only_note_denied_without_prompt`,
`subagent_deny_mode_never_reaches_request_permission_callback`) precisely
because "the observable behavior still matches" is not sufficient proof
that a single decision point was actually consulted.

A read-only-git carve-out built as a check upstream of `resolve` --
"if the command looks like read-only git, skip calling `resolve` at all"
-- is exactly this same hazard in a new shape: a second place a permission
decision gets made, which can drift from `resolve`'s precedence rules the
moment either side changes without the other. The architect's ruling,
recorded verbatim in the PRD's "Read-only git permission carve-out"
section and treated here as settled, not re-litigated, is that the
carve-out lives inside `resolve` itself. `resolve` not looking at command
content before this ADR was a scope limitation -- nothing in ADR 0016's
Decision section forbids it, the ticket that produced it (71) simply had
no caller that could supply a command string yet -- not an architectural
boundary this ADR is crossing.

## Decision

**1. `PermissionPolicy::resolve` gains a `command: Option<&str>`
parameter, threaded from every call site.** The two production call
sites that construct a `PermissionRequest` for a `bash` tool call --
`runner.rs`'s `request_permission` closure (the parent session's path)
and `subagent.rs`'s `tagged_request_permission` closure (the subagent
path) -- extract the command text from `PermissionPayload::Command`
*before* that payload is consumed elsewhere in the closure, and pass
`Some(command)`. Every other payload shape (`Diff`, `ToolCall`) passes
`None`, as does every existing unit test in `permission_policy.rs`'s own
test module that isn't specifically exercising the carve-out. This is the
first time `resolve` looks at anything about *what* a tool call would do,
rather than only *which* tool and under *what* mode -- a deliberate,
narrow widening of its inputs, not a departure from its role as the one
place every caller goes through.

**2. Exact precedence, grant first, carve-out fifth:**

1. A prior session grant (`SessionGrants::is_granted`) for `tool_name`
   returns `Allow`, unconditionally -- unchanged from ADR 0016, and
   unaffected by this ADR: the grant check runs before `command` is ever
   consulted.
2. `Bypass` mode returns `Allow` for any tool -- unchanged.
3. `Deny` mode returns `Deny` -- unchanged, and, critically, checked
   *before* the carve-out. A read-only git command issued under `Deny`
   mode must still resolve to `Deny`. This is a required regression test
   (`deny_mode_beats_read_only_git_carveout_regression`), not an optional
   nice-to-have, precisely because a carve-out that could override an
   explicit deny would defeat the reason `Deny` mode exists.
4. **New: if `tool_name == "bash"` and `command` classifies as read-only
   git (`crate::git_readonly::is_read_only_git`), return `Allow`.** This
   step only runs once grant/Bypass/Deny have all already passed through
   without resolving anything -- it never gets a chance to override any of
   them.
5. `AcceptEdits` mode returns `Allow` for `write`/`edit` only, `Prompt`
   otherwise -- unchanged, and now sitting *below* the carve-out: a
   read-only git command under `AcceptEdits` is allowed by step 4 before
   `AcceptEdits`'s own `write`/`edit`-only rule ever gets consulted (which
   would otherwise have prompted, since `bash` isn't `write` or `edit`).
6. Otherwise, `Prompt` -- unchanged. This is the only outcome the
   carve-out ever converts to `Allow`; every other outcome above is
   reached, and returned, exactly as it was before this ADR.

The invariant worth stating plainly: **the carve-out only ever turns a
would-be `Prompt` into `Allow`.** It never runs before a grant is
checked, never runs under `Bypass` (already `Allow`, redundant but
harmless), and never runs under `Deny` (already terminal at step 3). A
non-`bash` tool, or a `bash` command that fails classification, falls
through step 4 unchanged and reaches whatever step 5/6 would have
produced anyway.

**3. Classifier: `crate::git_readonly::is_read_only_git(command: &str) ->
bool`, pure, no I/O, no side effects.** Its conservatism spec is fixed by
this ADR, not left to implementer judgment:

1. **Any shell metacharacter present anywhere in the command disqualifies
   it outright:** `; & | < > (newline) ( ) { } ` (backtick) `$ * ? [ ] '
   " \ # ~ =`. `=` is deliberately in this list: it means `--flag=value`
   forms are rejected at this step, before the classifier ever reaches
   the flag allowlist below -- so an allowlisted flag must appear as its
   own whitespace-split token (`--stat`, not `--stat=full`).
2. **The command must whitespace-split to the literal token `git`
   immediately followed by a subcommand token that is exactly one of
   `status`, `diff`, `log`, `show`, `blame`**, with no global options
   permitted between `git` and the subcommand. This is what defeats
   `-c`/`-C`/`--git-dir`/`--work-tree`/`--exec-path`/`--upload-pack`-style
   redirection: whatever token sits immediately after `git` must be
   exactly one of the five subcommands, full stop, whether that token is
   a global option flag or anything else.
3. **Every remaining token must be on a fixed flag allowlist** (`-p -s
   --stat --numstat --name-only --name-status --oneline --graph
   --decorate --no-color --short --porcelain -n --max-count --author
   --since --until --grep`, plus the pattern `-<digits>`, e.g. `-5`,
   `-10`). Non-dash positional tokens -- paths, revisions, or values for
   flags like `--author`/`--since` -- pass through unchecked; the
   allowlist only constrains dash-prefixed tokens.
4. **Any ambiguity, anywhere in this process, fails closed to `Prompt` --
   never to `Allow`.** An unrecognized flag, a missing subcommand, an
   empty command, `git` with nothing after it: all of these classify
   `false`, which step 4 of the precedence chain above turns into "fall
   through to whatever the mode would otherwise have produced" -- at
   worst, an extra prompt; never a silent allow.

**4. Two residual risks are accepted, documented, and explicitly not
mitigated in v1:**

- **A PATH-spoofed `git` binary could defeat the classifier** -- the
  classifier only inspects the command string, not what `git` on `PATH`
  actually resolves to or does. This is accepted because planting a
  spoofed `git` binary somewhere on `PATH` already requires a prior
  gated write the sandbox still contains (ADR 0015); this carve-out does
  not create a new way to get that write past the sandbox, only a new
  thing a session that already has one could additionally do.
- **Positional arguments can disclose which paths exist** -- `git show
  some/secret/path` reveals, via its exit code and output, whether that
  path exists in the repo, without a prompt. This is accepted as
  read-only information disclosure no worse than what the existing
  `read` tool already permits unprompted today.

**5. Cross-reference.** This ADR amends ADR 0016 in the same spirit ADR
0016's own "Amendment (ticket 72)" section amended itself, and the way
ADR 0018's "Amendments" section documented its relationship to ADR 0014:
a short, dedicated section appended to ADR 0016
(`docs/adr/0016-permission-mode-policy-layer.md`, "Amendment (ADR 0019)")
points back here for the full rationale and classifier spec rather than
duplicating either.

## Considered Options

### Carve-out lives inside `PermissionPolicy::resolve` (chosen)

- Pro: preserves ADR 0016's single-entry-point invariant -- one function,
  one precedence table, one test suite proves the whole chain including
  the new step, exactly as `resolve`'s existing tests already prove
  `Bypass`/`Deny`/`AcceptEdits`.
- Pro: every caller of `resolve` gets the carve-out automatically, with
  no separate wiring needed per call site beyond threading `command`
  through -- the same reason threading `permission_mode` through
  `SessionRunner` (ADR 0016's F-005 fix) was preferred over each call
  site re-deriving mode dispatch independently.
- Con: `resolve`'s signature grows again (a `command: Option<&str>`
  parameter, on top of the `path: Option<&Path>` it already carries
  unused) -- a real but small cost, and one every existing call site
  (14 of them, 12 in `permission_policy.rs`'s own tests, one each in
  `runner.rs` and `subagent.rs`) has to update, mostly to `None`.

### Upstream bypass: check read-only-git before ever calling `resolve`

- Pro: would avoid touching `resolve`'s signature at all -- the check
  could live entirely at each call site.
- Con: rejected. This is precisely the dual-resolver shape ADR 0016's
  F-005 correction documents: a second place a permission decision gets
  made, which drifts the instant one side (say, `resolve`'s own `Deny`
  handling) changes without the upstream check changing to match. A
  read-only git command under `Deny` mode would be exactly one missed
  update away from silently executing -- the identical failure mode
  F-005 caught, on a different pair of code paths.

### A wider or looser classifier (e.g. permitting `--flag=value`, or more subcommands)

- Pro: would reduce the odds of a legitimate read-only invocation still
  hitting the prompt (e.g. `git log --author=alice`), reducing friction
  further.
- Con: rejected. Fail-closed is the entire point of this carve-out: a
  false `Allow` is a security bug (an unintended command executes with
  no human in the loop), while a false `Prompt` is minor, recoverable
  friction (the user clicks once more than strictly necessary). Widening
  the allowlist or accepting `=`-joined flags trades a bounded, cheap
  cost for an unbounded one, for a convenience gain user story 3
  explicitly asks not to be made. Widening the allowlist later, if real
  usage shows the current one is too narrow, is noted as deferred future
  work in the PRD, not decided here.

## Consequences

- A new module, `crates/rokr-app/src/git_readonly.rs`, holds
  `is_read_only_git` and its unit tests -- pure, enum/bool-assertion
  grade, zero I/O, matching `permission_policy.rs`'s existing test style.
  `crates/rokr-app/src/lib.rs`'s module list gains `pub mod
  git_readonly;`.
- `PermissionPolicy::resolve`'s signature changes from `resolve(mode,
  tool_name, path, grants)` to `resolve(mode, tool_name, path, command,
  grants)`; all 14 existing call sites (12 in `permission_policy.rs`'s
  own tests, one each in `runner.rs` and `subagent.rs`) are updated,
  passing `None` unless the test specifically exercises the carve-out.
- `runner.rs`'s `request_permission` closure and `subagent.rs`'s
  `tagged_request_permission` closure each gain a small extraction step
  -- reading the command string out of `PermissionPayload::Command`
  *before* that payload is consumed elsewhere -- so the raw command text
  reaches `resolve` on the `bash` path in both the parent session and the
  subagent path, satisfying user story 15's "identical to the interactive
  TUI" requirement for headless without a second implementation.
- A read-only `git status`/`diff`/`log`/`show`/`blame` invocation, issued
  by the model through the `bash` tool, now executes with no permission
  prompt in every mode except `Deny` (where it still denies) and except
  when a metacharacter, global option, or unrecognized flag makes the
  classifier fail closed (where it still prompts, exactly as before this
  ADR).
- Widening the flag allowlist or the recognized subcommand set, should
  real usage show the current one too narrow, is left as explicitly
  deferred future work (per the PRD's Further Notes) and would need its
  own review, not a silent broadening of this ADR's fixed spec.
