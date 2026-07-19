# Architect Brief — Phase 3

## Goal

Give rokr context-management discipline: stable, cache-friendly context
assembly; auto-compaction with a manual `/compact` fallback; a cheap repo
map for orientation; and `@`-file mentions for direct context injection.

Phase 1 and 2 proved the skeleton and gave the agent real tools, but every
turn still resends the full context with no caching discipline, no token
accounting, and no compaction, so long sessions get slow, expensive, and
eventually overflow the context window. Phase 3 fixes this at the
foundation — a provider-agnostic assembly module with stable-prefix
ordering and cache-breakpoint hints — and then builds compaction, the repo
map, and mentions on top of it. Real cache-hit validation against a live
provider wire contract is deliberately deferred to Phase 4 alongside the
Anthropic provider; this phase's payoff is prefix-stability discipline.

## Key decisions (binding)

- **Context assembly owned by `rokr-core`**: a new module owns segment
  ordering and breakpoint placement — tools → static system (agent prompt +
  project context file) → repo map (its own segment) → transcript. The
  refactor introducing this module must be behavior-preserving: identical
  wire output before and after, verified before any caching behavior is
  layered on.
- **Cache-breakpoint hints are provider-agnostic and modeled in
  `rokr-core`**: `cache_control` extends to tool-result/tool-use blocks and
  a breakpoint marker on tool specs; `CacheControl` gains a TTL hint
  (short vs. long-lived) so multi-hour caching stays expressible without
  being provider-specific. Placement: after tools, after the static system
  segment, and a rolling breakpoint on the conversation tail.
- **Provider adapters translate or ignore breakpoints; usage lands in
  `rokr-provider`**: the OpenAI-compatible adapter is a deliberate no-op on
  cache directives (implicit prefix caching already covers it) — explicit
  emission is Anthropic-only, arriving Phase 4. Provider send now returns
  usage (input/output/cache-read/cache-write tokens) alongside the reply;
  this is a breaking trait change, sequenced first.
- **Auto-compaction + `/compact`**: token accounting driven by
  provider-reported usage (chars/4 estimate only before first usage
  arrives); context-window size is a config value, not derived from model
  name; default trigger at 0.7 of window. Compaction is a summarization
  call that rewrites only older middle turns into one summary message,
  preserving the static prefix and the last user turn + last tool cycle
  verbatim; runs off the render thread; failure leaves the transcript
  intact with a notice.
- **Repo map owned by `rokr-tools`**: gitignore-aware file tree (not symbol
  extraction — tree-sitter/LSP is a later phase), built once per session,
  cached in memory, injected as its own breakpointed segment, regenerated
  only on `/compact`. Implemented as a plain function, not a `Tool` impl —
  it's orientation infrastructure, not a model-invoked action, so it stays
  outside the permission machinery.
- **Config compaction settings are additive-optional, not a version bump**
  (ADR 0010, amending ADR 0002): `context_window_size` and
  `auto_compact_threshold` land as `serde`-defaulted fields on the existing
  `version: 1` schema. An existing config missing them loads with runtime
  defaults (0.7 threshold, a sane window size) and is never rewritten — no
  migration, no write-back. A version bump + migration is reserved for an
  actual breaking change to the config shape, decided when that change is
  proposed.
- **Command seam owned by `rokr-tui`**: `/compact` routes through a new
  command-handling path (input starting with `/` goes to a handler, not
  the normal submit path); the TUI itself stays unaware of what any given
  command means. This seam is built to be extended by a later phase's
  broader slash-command surface, not replaced by it.
- **`@`-file mentions resolved at submit time**: parsed into delimited text
  segment(s) inside the user's own turn — never a synthetic tool result,
  since at least one supported provider rejects an orphan tool-role
  message. Reuses the existing read tool's read path for consistent size/
  encoding/truncation behavior. Lands in the dynamic tail, below every
  cache breakpoint. Autocomplete/fuzzy-match is explicitly deferred.

## Constraints

- Never block the render loop — compaction, repo-map generation, and
  mention resolution all run off the render thread; failures degrade
  gracefully rather than stalling input.
- Performance stays first-class: the repo map and mention resolution are
  bounded by explicit token/size budgets, not left to grow unbounded.
- Config is a versioned contract, refined by ADR 0010: only a breaking
  change to the config shape is a version bump requiring an explicit
  migration write-back. An additive-optional field (sane default, old
  files remain fully valid) is a `serde`-defaulted field with no version
  change and no rewrite of existing files — this phase's compaction
  settings are the first case of that.
- Crate boundaries are the contribution map: context assembly and cache
  modeling stay in `rokr-core`; the repo map stays in `rokr-tools`; the
  command seam stays in `rokr-tui`; usage plumbing stays in `rokr-provider`.
  No feature reaches across a boundary to implement another crate's
  responsibility.
- The OpenAI-compatible adapter must not emit `cache_control` directives on
  the wire — it stays a deliberate no-op this phase; explicit emission is
  Anthropic-only and arrives in Phase 4.
- The context-assembly refactor must land as behavior-preserving on its
  own, with identical wire output verified before any caching, compaction,
  repo-map, or mention behavior is built on top of it.

## Build order for Phase 3

1. Enabler: model + provider plumbing — cache-control TTL hint, breakpoints
   on tool-result/tool-use blocks and tool specs, usage returned from
   provider send. Breaking changes land first so nothing downstream
   migrates twice.
2. Enabler: context-assembly refactor in `rokr-core` — segmented, ordered
   assembly with named breakpoint slots; behavior-preserving, verified by
   asserting identical wire output before and after.
3. Caching activation — populate breakpoint hints per the placement policy;
   OpenAI adapter no-op confirmed.
4. Repo map in `rokr-tools` — generation, budgeting, injection as its own
   segment.
5. `@`-mentions — parsing, resolution, injection into the dynamic tail.
6. Config compaction settings — `context_window_size` and
   `auto_compact_threshold` as additive-optional fields (ADR 0010); no
   version bump, no migration. Independent of the enablers; can land
   anytime.
7. Auto-compaction + `/compact` — summarization mechanism and TUI command
   seam, consuming the settings from (6).

Dependency note: 1 → 2 → {3, 4, 5}; slices 3, 4, and 5 all depend on 2 and
are parallelizable against each other. (6) has no dependencies and can run
alongside 1/2. Compaction (7) depends on 1, 2, 3, and 6, and is sequenced
last.

## Out of scope

- The Anthropic provider and real, validated cache-hit measurement (Phase
  4) — this phase only guarantees prefix stability, not a measured hit
  rate against a live wire contract.
- Autocomplete or fuzzy matching for `@`-mentions (a later input-experience
  phase).
- Tree-sitter or language-server-based symbol maps (a much later phase,
  after any LSP integration).
- Durable, disk-backed session persistence — this phase's compaction and
  caching operate only on the in-memory running transcript.
- webfetch/websearch tools, MCP support, and hooks (later phases).
- Deduplication optimization for repeated or overlapping mentions.
