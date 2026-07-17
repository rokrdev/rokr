# 0008 - Render loop architecture

## Status

accepted

## Context

Performance is a first-class requirement (<50ms first paint, <150ms to
interactive, per `docs/PLAN.md`). Agentic tools stream model output and tool
results continuously, which can easily degrade into redrawing far more often
than the terminal or the user can perceive, wasting CPU and hurting
responsiveness.

## Decision

A single event loop drives the application. The render thread only draws and
handles input — it does not perform IO (per ADR 0007). Target frame budget is
~16ms. Redraws happen only on state change, never on a fixed timer with no
change. Streaming deltas (model tokens, tool output) arrive via channel and
are coalesced per frame — the UI never redraws per-token. Large tool output
is streamed and truncated before rendering rather than rendered in full.

## Considered Options

### Single event loop, coalesced per-frame redraws, IO fully off the render thread

- Pro: bounds worst-case render latency independent of how fast the model or
  tools produce output; directly supports the <50ms/<150ms targets;
  straightforward to reason about since there's one loop, one frame budget.
- Con: requires a coalescing/batching layer between the channels carrying
  streamed deltas and the render step, adding some implementation complexity
  over naive "redraw on every message."

### Redraw immediately on every incoming message (token, tool output line, etc.)

- Pro: simplest possible implementation, no coalescing logic needed.
- Con: token-level streaming from a fast model or a chatty tool (e.g. bash
  output) could trigger redraws far faster than the terminal can display or
  the user can perceive, burning CPU and risking missed frame budgets.

## Consequences

`rokr-tui` needs a coalescing step between "channel has new data" and "time
to redraw" — e.g. draining all pending channel messages before each frame
rather than redrawing per message. Tool implementations that produce large
output (per ADR 0004) must truncate before handing output to the render
path; full untruncated output belongs in session storage (`rokr-session`),
not necessarily on screen.
