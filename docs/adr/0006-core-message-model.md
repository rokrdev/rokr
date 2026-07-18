# 0006 - Core message model

## Status

accepted

## Context

ADR 0003 defines a provider-agnostic `Provider` trait in `rokr-provider` and
states that adaptation work happens "at the edge" — each provider converts
between rokr's own message representation and that provider's wire format. It
defers the definition of that representation to this ADR.

The model must satisfy competing constraints:

- **Provider-agnostic.** OpenAI-compatible (Phase 1) and Anthropic (Phase 4)
  have different message shapes: OpenAI uses a `tool` role and historically a
  string `content`; Anthropic uses `user`/`assistant` roles only, an array of
  typed content blocks, a top-level `system` parameter, and `cache_control`
  annotations. Neither wire format may leak into `rokr-core`.
- **Additive growth.** Phase 1 needs only single-turn chat (one user message,
  one assistant text reply). Later phases add tool calls/results (Phase 2),
  prompt caching (Phase 3), sessions (Phase 4-5), and images (Phase 8). None
  of these may force a breaking change to the core types.
- **Simplicity now.** Per the project's "simplest solution" rule, Phase 1 code
  should stay small — but the PLAN's "content blocks from day one" principle
  explicitly overrides a naive plain-string model to avoid a later rewrite.

## Decision

Define the message model in `rokr-core` as a uniform triple of *role +
ordered content blocks*, with content blocks modelled as a Rust enum whose
Phase 1 surface is a single `Text` variant. Every message — system, user, or
assistant — has the same shape.

```rust
/// Who authored a message. Kept minimal and provider-neutral:
/// providers map these onto their own conventions at the edge
/// (e.g. Anthropic lifts `System` to the top-level `system` param;
/// OpenAI tool results map onto the `tool` role, not a rokr role).
pub enum Role {
    System,
    User,
    Assistant,
}

/// A single unit of content within a message. This enum is the
/// designated extension point for the message model: new modalities
/// and turn types (tool calls, images, thinking) are added as
/// variants, never by changing `Message` or `Role`.
pub enum ContentBlock {
    Text {
        text: String,
        /// Caching hint. Always `None` until Phase 3; present now so
        /// caching is additive rather than a struct change.
        cache_control: Option<CacheControl>,
    },
    // Phase 2:  ToolUse { id, name, input }
    //           ToolResult { tool_use_id, content, is_error }
    // Phase 8:  Image { source, cache_control }
}

pub struct CacheControl {
    pub kind: CacheControlKind,
}

pub enum CacheControlKind {
    Ephemeral,
}

/// A message is always role + an ordered list of blocks, uniformly.
/// The Anthropic-style "content may be a bare string" shortcut is a
/// wire-format concern owned by the provider adapter, not the model.
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self { /* ... */ }
    pub fn system_text(text: impl Into<String>) -> Self { /* ... */ }
    // assistant_text, plus block accessors for the render layer
}
```

Two rules make this work:

1. **Adapters own the wire format.** `rokr-core` types derive
   `serde::{Serialize, Deserialize}` only for rokr's *own* persistence
   (sessions, checkpoints — Phase 4-5), in a rokr-native shape. Each provider
   module in `rokr-provider` owns its own request/response DTO structs and
   converts `Message` ↔ DTO. The core carries **no** provider-specific serde
   attributes. In Phase 1 this means the OpenAI adapter does real conversion
   work even though its format is close to ours — that symmetry is deliberate
   (see Consequences).

2. **`ContentBlock` is *not* `#[non_exhaustive]`.** Adding a variant should
   force a compile error in every provider adapter's `match`, so no provider
   can silently drop a block type it does not yet handle. Because core and
   providers live in one workspace and version together, cross-crate
   exhaustiveness is a safety feature, not a burden.

## Considered Options

### Content blocks with a single `Text` variant now (chosen)

- Pro: matches the PLAN's "content blocks from day one" principle; tool
  calls, images, and thinking blocks are additive enum variants; `cache_control`
  has a home before caching exists; Anthropic's array-of-blocks maps directly.
- Con: more ceremony than a string for Phase 1's single-turn chat; the
  OpenAI adapter must convert blocks ↔ string even though a string would have
  sufficed today.

### Plain `String` content, refactor to blocks later

- Pro: minimal Phase 1 code.
- Con: the PLAN explicitly rejects this — tool results, images, and per-block
  `cache_control` cannot be expressed, so Phase 2/3/8 would each require a
  breaking change to `Message` and a rewrite of the agent loop that consumes
  it. Trades a few lines now for a core rewrite later.

### Core types double as the OpenAI wire format

- Pro: zero conversion code in the Phase 1 provider.
- Con: bakes OpenAI's quirks (the `tool` role, legacy `function_call`, bare
  string content) into `rokr-core`, making Anthropic the "odd one out" and
  violating ADR 0003's "adaptation at the edge." Rejected: the core must be
  neutral, with *both* providers converting.

### A separate `Tool` role instead of tool-result content blocks

- Pro: mirrors OpenAI's wire shape directly.
- Con: only one provider has a tool role; Anthropic models tool results as
  content blocks inside a user message. Keeping `Role` at three variants and
  representing tool results as blocks stays neutral and lets each adapter map
  as needed. Rejected.

## Consequences

- Phase 1 pays a small, bounded cost: a `ContentBlock` enum with one variant,
  an always-`None` `cache_control` field, and an OpenAI adapter that converts
  to/from bare strings. This is the "upfront design cost" ADR 0003 anticipated.
- Growth is additive. Phase 2 adds `ToolUse`/`ToolResult` variants; Phase 3
  populates `cache_control`; Phase 8 adds `Image`. None touch `Message` or
  `Role`. When a variant is added, every provider `match` fails to compile
  until updated — the intended guardrail.
- **Streaming (later phases) does not touch this model.** Streaming is a
  transport concern of the `Provider` trait: it yields deltas that accumulate
  into a final `Message`. The message model is the settled result, not the
  wire chunking.
- Sessions and checkpoints are `Vec<Message>` plus metadata; persistence uses
  the rokr-native serde derives, insulated from any provider's format changes.
- The render layer in the TUI reads `ContentBlock`s directly, so it is ready
  for richer content (tool call cards, images) without a model change.
