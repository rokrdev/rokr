# 0006 - Message and content-block model

## Status

accepted

## Context

Prompt caching is rokr's headline feature (Phase 3), but Phase 1 only ships
an OpenAI-compatible provider, whose native request/response shape is flat
messages with no notion of cache breakpoints. We need to decide now whether
the core message model matches the first provider we ship, or the future
caching-capable shape we know we need.

## Decision

Core message types in `rokr-core` model Anthropic-style structured content
blocks with `cache_control` breakpoints from day one, even though only an
OpenAI-compatible provider ships first. The OpenAI provider is responsible
for adapting to and from this shape at the edge (in `rokr-provider`), not the
other way around.

## Considered Options

### Anthropic-style content blocks with cache_control, from day one

- Pro: Phase 3's caching work and Phase 4's Anthropic provider both plug into
  a core model that already fits their shape; no core rewrite needed later.
- Con: Phase 1's OpenAI-compatible provider must do adaptation work
  immediately even though it can't use cache_control yet, which is upfront
  complexity for a feature not yet active.

### Flat OpenAI-shaped messages in core, adapt later for caching/Anthropic

- Pro: simplest possible core model for Phase 1's needs.
- Con: caching is described in `docs/PLAN.md` as the headline feature of the
  product; retrofitting content blocks and cache breakpoints into the core
  message model later would touch every crate that consumes messages
  (`rokr-tui`, `rokr-tools`, `rokr-session`), not just the provider layer.

## Consequences

`rokr-provider`'s OpenAI-compatible implementation carries the cost of
shape-adaptation starting in Phase 1. In exchange, Phase 3 (caching) and
Phase 4 (Anthropic provider) integrate against a core model that was already
built for them.
