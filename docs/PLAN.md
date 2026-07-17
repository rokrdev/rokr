# rokr Roadmap

## Phases

1. **Phase 1** — TUI (Header / View / Prompt sections) + JSON config at
   `~/.config/rokr/rokr.json` (created on first run, `"version": 1`) +
   OpenAI-compatible provider (url/model/api_key via env vars) + Plan & Build
   agent prompts scaffolded to `~/.config/rokr/agents/*.md` + single-turn
   model call rendered in the TUI.

2. **Phase 2** — Agent tool loop (model → tool call → execute → result →
   iterate) + core tools (read, write, edit, bash, glob, grep, ls, webfetch,
   websearch — the agent needs docs access) + permission prompts. Plan agent
   = read-only tool set; Build agent = full tool set. Permissions ship WITH
   tools, same phase. Write/edit tool calls render as a diff review UI with
   accept/reject, pairing naturally with the permission prompts above.
   Project context files (AGENTS.md, plus a rokr-specific override file) are
   auto-loaded from the project root into the system prompt.

3. **Phase 3** — Cache optimization (cache_control breakpoints, ≥1h prompt
   caching, following Claude/Crush best practices) + auto-compaction at a
   configurable threshold (e.g. 0.7) + `/compact` command. A gitignore-aware
   repo map gives the agent cheap orientation in a codebase, and @-file
   mentions in the prompt UI let the user inject specific files into
   context directly.

4. **Phase 4** — Anthropic provider + subagents (user-defined, own prompts,
   navigable) + session-scoped model search/switch (does not persist to
   config). Provider resilience (retries with exponential backoff,
   rate-limit handling, provider failover) is designed into the `Provider`
   trait and implemented in this phase. OAuth login for subscription plans
   (Claude Pro/Max-style) supplements API-key auth.

5. **Phase 5** — Session management: resume, search older sessions, jump
   between sessions with a swap warning; mouse support; status line (session
   time, context % used). Undo/checkpoints snapshot files before agent
   edits, tied to session turns, with a rollback command; $EDITOR
   integration, multi-line prompt editing, and prompt history round out the
   input experience.

6. **Phase 6** — MCP support + hooks.

7. **Phase 7 — Adoption layer** — headless mode (`rokr -p "..."`, stdin/stdout,
   `--output-format json`), custom slash commands + skills, cost/token
   analytics, persistent memory files (user scope and project scope), and an
   agent eval harness built on headless mode for testing agent quality in
   CI. Distribution rounds out this phase: install script, Homebrew,
   `rokr upgrade` self-update, and shell completions.

8. **Phase 8 — Advanced execution & DX** — OS-level sandboxing, parallel
   subagents, git workflow integration, LSP integration (optional, last),
   and image support (clipboard paste to vision-capable models — the
   content-block model already supports this, per ADR 0006).

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
