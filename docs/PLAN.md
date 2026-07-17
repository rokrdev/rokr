# rokr Roadmap

## Phases

1. **Phase 1** — TUI (Header / View / Prompt sections) + JSON config at
   `~/.config/rokr/rokr.json` (created on first run, `"version": 1`) +
   OpenAI-compatible provider (url/model/api_key via env vars) + Plan & Build
   agent prompts scaffolded to `~/.config/rokr/agents/*.md` + single-turn
   model call rendered in the TUI.

2. **Phase 2** — Agent tool loop (model → tool call → execute → result →
   iterate) + core tools (read, write, edit, bash, glob, grep, ls) +
   permission prompts. Plan agent = read-only tool set; Build agent = full
   tool set. Permissions ship WITH tools, same phase.

3. **Phase 3** — Cache optimization (cache_control breakpoints, ≥1h prompt
   caching, following Claude/Crush best practices) + auto-compaction at a
   configurable threshold (e.g. 0.7) + `/compact` command.

4. **Phase 4** — Anthropic provider + subagents (user-defined, own prompts,
   navigable) + session-scoped model search/switch (does not persist to
   config).

5. **Phase 5** — Session management: resume, search older sessions, jump
   between sessions with a swap warning; mouse support; status line (session
   time, context % used).

6. **Phase 6** — MCP support + hooks.

7. **Phase 7 — Adoption layer** — headless mode (`rokr -p "..."`, stdin/stdout,
   `--output-format json`), custom slash commands + skills, cost/token
   analytics.

8. **Phase 8 — Advanced execution & DX** — OS-level sandboxing, parallel
   subagents, git workflow integration, LSP integration (optional, last).

## Design principles

- **Performance is first-class.** Target <50ms first paint, <150ms to
  interactive. Lazy-load everything. No network calls on boot.
- **Never block the render loop.** All IO runs on tokio tasks and
  communicates with the UI via channels; redraws are coalesced, never
  triggered per unit of background work.
- **Content blocks from day one.** Core message types model Anthropic-style
  content blocks with `cache_control` from the start, even before caching or
  the Anthropic provider exist, so later phases don't require a core rewrite.
- **Config schema is versioned from day one.** On-disk config is a public
  contract; every schema change gets a version bump and a migration path.
- **Crates are the contribution map.** Each crate owns one responsibility;
  contributors should be able to find the right crate from the feature they
  want to add.
