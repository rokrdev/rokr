# 0009 - Provider trait location

## Status

accepted

## Context

`rokr-core`'s `single_turn` orchestration (ticket 05, Phase 1's single-turn
wiring) must be generic over the `Provider` trait to send a message and
receive a reply without depending on any concrete provider implementation.
ADR 0003 originally declared the `Provider` trait in `rokr-provider`, with
`rokr-core` staying provider-agnostic per ADR 0006. But ADR 0006 (and the
crate dependency graph it operates under) already fixes `rokr-provider ->
rokr-core` as the dependency edge: `rokr-provider`'s adapters convert
`rokr_core::Message` to and from their own wire DTOs, so `rokr-provider`
depends on `rokr-core`. If the `Provider` trait itself stayed in
`rokr-provider`, `rokr-core::single_turn` would need to depend on
`rokr-provider` to be generic over it — the reverse edge — creating a
dependency cycle between the two crates.

## Decision

`Provider` is declared in `rokr-core` as a port (in the hexagonal /
ports-and-adapters sense): the trait `rokr-core`'s own orchestration logic
depends on, without depending on any concrete adapter. Concrete
implementations continue to live in `rokr-provider`, one module per
provider, as ADR 0003 originally decided. `rokr-provider` re-exports
`pub use rokr_core::Provider;` so existing call sites
(`rokr_provider::Provider`) keep working unchanged.

This ADR refines and supersedes ADR 0003 on the trait-location point only.
ADR 0003's other commitments — one module per provider implementation, the
OpenAI-compatible implementation shipping in Phase 1 and Anthropic in Phase
4, and the trait as a designated contributor extension point — stand
unchanged.

## Considered Options

### `Provider` trait in `rokr-core`, impls + re-export in `rokr-provider` (chosen)

- Pro: `rokr-core::single_turn` can be generic over `Provider` without a
  dependency cycle; `rokr-provider -> rokr-core` (fixed by ADR 0006) stays
  the only edge between the two crates; existing `rokr_provider::Provider`
  call sites are unaffected via the re-export.
- Con: the trait's "home" crate no longer matches the crate contributors
  think of first when adding a provider (`rokr-provider`); mitigated by the
  re-export and by `CONTRIBUTING.md` pointing contributors at
  `rokr-provider` for implementation work.

### Keep `Provider` in `rokr-provider`, have `rokr-core` depend on `rokr-provider`

- Pro: matches ADR 0003 literally, no new crate boundary to explain.
- Con: `rokr-provider` already depends on `rokr-core` (ADR 0006 — adapters
  convert `Message` at the edge); adding the reverse edge creates a cycle,
  which Cargo does not allow. Rejected outright, not just on style grounds.

### Extract a third crate (e.g. `rokr-provider-api`) to hold just the trait

- Pro: keeps `rokr-core` and `rokr-provider` both "clean" of the other's
  concerns; mirrors how some Rust ecosystems split a `-core`/`-api` crate
  from implementations.
- Con: a new crate for a single trait is more ceremony than Phase 1 needs,
  and complicates the "crates are the contribution map" principle
  (CONTRIBUTING.md) by adding a crate that isn't an obvious extension
  point. Rejected as premature for a single-trait boundary; revisit only if
  more port traits accumulate.

## Consequences

- `rokr-core/src/lib.rs` carries the `Provider` trait definition and a doc
  comment explaining the port/adapter split; `rokr-provider/src/lib.rs`
  re-exports it with a doc comment pointing back here.
- Contributors adding a new provider still work entirely inside
  `rokr-provider` (new module, `impl Provider for ...`); the trait's
  declaration site in `rokr-core` is an implementation detail they don't
  need to touch.
- Phase 4's session-scoped model switching (multiple providers live within
  one session) is expected to use enum dispatch (e.g. an `AnyProvider` enum
  wrapping each concrete provider) at the session boundary, not a boxed
  `dyn Provider`.
- The trait keeps native `async fn` (Rust's async-fn-in-traits, AFIT); no
  boxed `dyn Provider` variant is introduced by this ADR. If a future phase
  requires genuinely open/runtime-selected providers behind a trait object,
  `trait-variant`'s `#[trait_variant::make(Provider: Send)]` (or an
  explicit `-> impl Future<Output = ...> + Send` return type) should be
  introduced then, not preemptively.
- **AFIT `Send` edge case to watch for**: native `async fn` futures are
  automatically `Send` when the concrete type is known (as with
  `single_turn<P: Provider>`'s monomorphized call sites today). But
  spawning that future onto `tokio::spawn` from behind a generic
  `P: Provider` bound or a `dyn Provider` trait object requires the future
  to be provably `Send`, which AFIT does not guarantee across an abstract
  boundary. If a later phase needs `tokio::spawn` behind such a boundary,
  add `#[trait_variant::make(Provider: Send)]` to the trait (or hand-write
  `-> impl Future<Output = Result<Message, Self::Error>> + Send`) at that
  point — not now, since Phase 1's `rokr-tui` event loop already spawns the
  *concrete* `submit` closure, not a generic-over-`Provider` future.
