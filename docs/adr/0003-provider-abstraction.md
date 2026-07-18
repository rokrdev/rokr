# 0003 - Provider abstraction

## Status

superseded by [0009](0009-provider-trait-location.md)

## Context

rokr's core pitch is "bring your own models" — it must talk to more than one
LLM backend, starting with OpenAI-compatible APIs and later adding Anthropic
directly. We need an abstraction boundary that lets contributors add new
providers without touching the agent loop or the TUI.

## Decision

Define a `Provider` trait in `rokr-provider`, with one module per concrete
implementation. The OpenAI-compatible implementation ships in Phase 1; the
Anthropic implementation ships in Phase 4. This trait is a designated
contributor extension point (see `CONTRIBUTING.md`).

## Considered Options

### Trait-based abstraction, one module per provider

- Pro: clean compile-time boundary; new providers are additive (new file, new
  trait impl) rather than invasive; matches the "crates are the contribution
  map" principle at a finer grain.
- Con: trait must be designed to accommodate providers with different
  request/response shapes (e.g. OpenAI vs. Anthropic message formats), which
  requires the core message model to be provider-agnostic (see ADR 0006).

### Provider-specific code paths scattered through `rokr-core`

- Pro: no upfront abstraction cost.
- Con: every new provider means touching the core agent loop; violates the
  extension-point goal and does not scale past two providers.

## Consequences

`rokr-core`'s message and content-block model (ADR 0006) must be shaped so
that the OpenAI-compatible provider is the one doing adaptation work at the
edge, not the core. This is more upfront design cost in Phase 1 but avoids a
rewrite when Anthropic support and later multi-provider features (Phase 4's
session-scoped model switching) land.
