# 0005 - Permission model

## Status

accepted

## Context

Once tools can execute arbitrary actions (writing files, running shell
commands), rokr needs a permission model so users aren't surprised by side
effects. This needs to be decided alongside the tool loop (ADR 0004) rather
than bolted on afterward.

## Decision

Permission prompts ship in the same phase as tools (Phase 2), not later.
Gating is per-tool. The Plan agent is restricted to a read-only tool set
(read, grep, glob, ls); the Build agent gets the full tool set (adds write,
edit, bash). Allowlist/auto-accept modes and OS-level sandboxing are deferred
to Phase 8, which hardens this baseline model.

## Considered Options

### Permissions ship with tools (Phase 2)

- Pro: no window where tools exist but are unguarded; permission gating is
  designed alongside the `Tool` trait rather than retrofitted onto it.
- Con: adds scope to Phase 2's already-substantial deliverable (the full tool
  loop plus core tools).

### Permissions deferred to a later phase

- Pro: smaller Phase 2 scope.
- Con: ships a version of rokr where agents can execute bash/write/edit with
  no user confirmation step, which is an unacceptable default for a coding
  agent; retrofitting gating onto an already-built tool loop is riskier than
  building it in from the start.

## Consequences

Every tool implementation must be written against a permission-aware calling
convention from the first tool onward, rather than having gating layered on
top later. Phase 8's sandboxing and allowlist work extends this baseline
rather than introducing permission checks for the first time.
