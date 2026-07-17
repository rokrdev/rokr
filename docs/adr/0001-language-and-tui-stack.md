# 0001 - Language and TUI stack

## Status

accepted

## Context

rokr is a greenfield agentic coding CLI in the same space as Claude Code,
opencode, and Crush. We need to pick a language and TUI framework before any
other work can start. The choice affects distribution model, performance
ceiling, and the pool of contributors we can draw on.

## Decision

Build rokr in Rust, using ratatui for the terminal UI and tokio as the async
runtime.

## Considered Options

### Rust + ratatui + tokio

- Pro: highest performance ceiling of the options considered; compiles to a
  single static binary with no runtime to install; ratatui is a mature,
  actively maintained TUI framework with a large widget ecosystem.
- Con: smaller contributor pool than Go or TypeScript; steeper learning curve
  for new contributors; longer compile times during development.

### Go + bubbletea (Crush's stack)

- Pro: fast compilation, simple concurrency model, easy single-binary
  distribution, proven in Crush.
- Con: lower performance ceiling than Rust for CPU-bound work; garbage
  collector introduces latency variance that works against the render-loop
  budget in ADR 0008.

### TypeScript + ink (opencode-style)

- Pro: largest contributor pool, fastest iteration speed, rich ecosystem.
- Con: requires a Node runtime or bundling step for distribution; weakest
  performance ceiling of the three; React-style reconciliation adds overhead
  that works against the <50ms first paint target.

## Consequences

Every other crate and tool in this project inherits Rust's ownership model
and tokio's async primitives. Contributors need Rust familiarity. In
exchange, we get single-binary distribution and headroom to hit the
performance targets in `docs/PLAN.md` without fighting the language.
