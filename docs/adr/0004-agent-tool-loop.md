# 0004 - Agent tool loop

## Status

accepted

## Context

Agentic coding tools are defined by their ability to let a model call tools,
observe results, and iterate. This loop is the product's core value
proposition and needs an architectural home before Phase 2 implementation
starts.

## Decision

The core loop lives in `rokr-core`: the model emits tool calls, they are
executed via the `Tool` trait defined in `rokr-tools`, results are fed back
to the model, and the cycle repeats until the model signals completion.
Tools are a one-file-each contributor extension point. This loop is the
product's heart and is sequenced as Phase 2, immediately after the Phase 1
skeleton proves config/TUI/provider/render wiring.

## Considered Options

### Loop owned by `rokr-core`, tools as a trait in `rokr-tools`

- Pro: clean separation between "how the conversation progresses" (core) and
  "what a tool does" (tools crate); new tools are additive; matches the
  provider abstraction pattern in ADR 0003.
- Con: requires a stable `Tool` trait contract early, before all tool
  requirements (e.g. permission gating, streaming output) are fully known.

### Loop owned by `rokr-tui`

- Pro: tighter coupling to rendering could simplify streaming updates.
- Con: conflates UI concerns with agent orchestration; makes a future
  headless mode (Phase 7) much harder since the loop would be entangled with
  the TUI.

## Consequences

Because the loop lives in `rokr-core` independent of `rokr-tui`, headless
mode (Phase 7) can reuse the same loop without a rendering dependency. Tool
authors only need to implement the `Tool` trait and do not need to
understand the render loop or provider adaptation code.
