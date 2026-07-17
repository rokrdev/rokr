# 0007 - Async runtime and concurrency

## Status

accepted

## Context

rokr must perform network IO (provider calls), file IO (tools, config,
sessions), and terminal rendering concurrently without the render loop ever
stalling (see ADR 0008's frame budget). We need a concurrency model decided
before the tool loop and TUI are built on top of it.

## Decision

Use the tokio multi-threaded runtime with an explicit ownership model:
network and IO work runs on tokio tasks; each shared resource has exactly one
owner; communication between the UI thread and worker tasks happens via
message passing (`mpsc`/`watch` channels). No shared `Mutex` on hot paths.

## Considered Options

### tokio tasks + channel-based ownership, no hot-path Mutex

- Pro: matches Rust's ownership model directly; avoids lock contention and
  priority-inversion risks on the render path; failure modes are easier to
  reason about since each resource has one clear owner.
- Con: requires more upfront design of channel protocols between the UI and
  worker tasks compared to reaching for shared state.

### Shared state behind `Mutex`/`RwLock`

- Pro: simpler mental model for small cases, less initial channel-plumbing.
- Con: lock contention directly threatens the ~16ms render frame budget in
  ADR 0008; encourages accidental blocking of the render thread; harder to
  reason about ownership as the codebase grows.

## Consequences

Every crate that performs IO (`rokr-provider`, `rokr-tools`, `rokr-config`,
`rokr-session`) must expose async, channel-friendly APIs rather than
returning shared, lockable state. `rokr-tui` never awaits IO directly on the
render path — it only reads from channels.
