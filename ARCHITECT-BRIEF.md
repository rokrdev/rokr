# Architect Brief — Phase 1

## Goal

Ship a minimal but real end-to-end slice: a TUI that loads a versioned JSON
config, talks to a single OpenAI-compatible provider, and renders one
single-turn model response. No tool loop, no permissions, no caching yet —
those are later phases. Phase 1 exists to prove the skeleton (config → TUI →
provider → render) works before any agentic behavior is layered on.

## Key decisions (binding, from the ADRs)

- **Stack**: Rust + ratatui + tokio (ADR 0001). Chosen over Go/bubbletea and
  TypeScript/ink for performance ceiling, single-binary distribution, and
  ratatui's maturity.
- **Config**: JSON at `~/.config/rokr/rokr.json`, created on first run with an
  explicit `"version": 1` field (ADR 0002). Config is a public contract for
  OSS users — schema is versioned from day one, migrations required on bumps.
- **Provider abstraction**: a `Provider` trait in `rokr-provider`, one module
  per implementation (ADR 0003). Phase 1 ships only the OpenAI-compatible
  implementation (url/model/api_key via env vars); Anthropic lands in Phase 4.
- **Message/content-block model**: `rokr-core` models Anthropic-style
  structured content blocks with `cache_control` breakpoints from day one
  (ADR 0006), even though Phase 1 only exercises a single-turn, non-cached
  call through an OpenAI-compatible adapter. This avoids a core rewrite when
  caching becomes the headline feature in Phase 3.
- **Async runtime**: tokio multi-threaded runtime, explicit single-owner
  resources, UI/worker communication via channels, no `Mutex` on hot paths
  (ADR 0007).
- **Render loop**: single event loop; render thread only draws and handles
  input; ~16ms frame budget; redraw only on state change (ADR 0008). Applies
  even to Phase 1's single-turn call — the response arrives over a channel,
  not by blocking the render thread.

## Constraints

- Performance is first-class from the start: target <50ms first paint, <150ms
  to interactive; lazy-load everything; no network calls on boot.
- Never block the render loop — all IO runs on tokio tasks and communicates
  with the UI via channels.
- Crates are the contribution map; keep responsibilities scoped to the crate
  boundaries defined in the workspace `Cargo.toml`.

## Build order for Phase 1

1. TUI skeleton: Header / View / Prompt sections in `rokr-tui`.
2. Config loading in `rokr-config`: read-or-create `~/.config/rokr/rokr.json`
   with `"version": 1`.
3. OpenAI-compatible provider in `rokr-provider` (url/model/api_key via env
   vars).
4. Plan & Build agent prompts scaffolded to `~/.config/rokr/agents/*.md`
   (prompt files only — no tool loop yet).
5. Wire a single-turn model call end-to-end and render the result in the TUI.

## Out of scope

Everything Phase 2 onward: the tool loop and core tools, permissions, prompt
caching and compaction, the Anthropic provider, subagents, session
management, MCP/hooks, headless mode, sandboxing, and any other capability
described in `docs/PLAN.md` beyond Phase 1.
