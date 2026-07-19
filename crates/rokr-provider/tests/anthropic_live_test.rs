//! Live smoke test against the real Anthropic API — validates that prompt
//! caching actually works end-to-end (the wire-shape tests in
//! `anthropic_test.rs` only assert breakpoint *placement*, not that the API
//! honors them). `#[ignore]`-marked; run explicitly:
//!
//! ```sh
//! ROKR_ANTHROPIC_BASE_URL=https://api.anthropic.com \
//! ROKR_ANTHROPIC_MODEL=<a real model id> \
//! ROKR_ANTHROPIC_API_KEY=<key> \
//! cargo test -p rokr-provider --test anthropic_live_test -- --ignored
//! ```
//!
//! ## Empirical findings (researched 2026-07-19, ticket 24)
//!
//! - **Minimum cacheable prefix length** varies by model family/version:
//!   1,024 tokens for the Sonnet family and several Opus versions, up to
//!   4,096 tokens for Claude Haiku 4.5 and some Opus versions (per current
//!   Anthropic docs, `platform.claude.com/docs/en/build-with-claude/prompt-caching`).
//!   Since this test resolves its model from `ROKR_ANTHROPIC_MODEL` at
//!   runtime rather than hardcoding one, the filler prefix below is sized
//!   comfortably above the *highest* documented floor (4,096 tokens) so a
//!   zero-cache-hit result can't be blamed on prefix size no matter which
//!   model is configured.
//! - **Extended (1-hour) TTL beta header**: sources disagree. Current
//!   Anthropic docs state the `ttl: "1h"` `cache_control` field works with no
//!   special header. Other sources (a GitHub issue thread, a "TTL silently
//!   dropped from 1h to 5m" blog post) reference an
//!   `anthropic-beta: extended-cache-ttl-2025-04-11` opt-in header having
//!   been required at some point. Rather than resolve that ambiguity here,
//!   this test sidesteps it entirely by using `CacheControlKind::Ephemeral`
//!   (the standard breakpoint, undisputed across every source — no header
//!   needed), which is sufficient to validate basic cache-read behavior.
//!   `AnthropicProvider`/`send()` do not currently thread through an
//!   `anthropic-beta` header at all; whether extended-TTL support needs that
//!   plumbing is an open question for a follow-up ticket, not this one.

use rokr_core::{CacheControl, CacheControlKind, ContentBlock, Message, Role};
use rokr_provider::{AnthropicProvider, Provider};

#[tokio::test]
#[ignore]
async fn anthropic_live_cache_hit_on_second_send() {
    let provider = AnthropicProvider::from_env().expect(
        "ROKR_ANTHROPIC_BASE_URL, ROKR_ANTHROPIC_MODEL, and ROKR_ANTHROPIC_API_KEY must all be \
         set to run this live test",
    );

    // A short phrase repeated ~500 times comfortably clears the highest
    // documented per-model floor (4,096 tokens), regardless of which model
    // ROKR_ANTHROPIC_MODEL points at.
    let filler = "The rokr agent orchestrates tool calls against a workspace. ".repeat(500);

    let system_message = Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: filler,
            cache_control: Some(CacheControl {
                kind: CacheControlKind::Ephemeral,
            }),
        }],
    };
    let user_message = Message::user_text("Reply with a single word: acknowledged.");

    let messages = vec![system_message, user_message];

    let (_first_response, first_usage) = provider
        .send(&messages, &[])
        .await
        .expect("first live send() should succeed");

    let (_second_response, second_usage) = provider
        .send(&messages, &[])
        .await
        .expect("second live send() should succeed");

    eprintln!("first call usage: {first_usage:?}");
    eprintln!("second call usage: {second_usage:?}");

    assert!(
        second_usage.cache_read_tokens > 0,
        "second send() with an identical cache-eligible prefix should report nonzero \
         cache_read_tokens (got {second_usage:?}); first call usage was {first_usage:?}"
    );
}
