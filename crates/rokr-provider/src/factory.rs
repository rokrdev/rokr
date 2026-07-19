//! Composes provider construction: given an already-resolved [`Auth`]
//! credential, selects the correct backend (mirroring `AnyProvider`'s
//! existing selection logic) and wraps it in the resilience decorator
//! (ticket 32, PRD Phase 4 "Further Notes" -- provider factory seam).
//!
//! Auth resolution itself (`auth::resolve_auth`, which needs a
//! `&dyn TokenStore`) stays the caller's job, not this function's --
//! callers already have the token store on hand (it's how they resolve
//! auth in the first place), and keeping it out of this function's
//! signature makes `build_provider` trivially unit-testable with a
//! directly-constructed `Auth` value, no filesystem/keychain involved.

use crate::auth::Auth;
use crate::resilience::RetryPolicy;
use crate::{AnthropicProvider, AnyProvider, ResilientProvider};

/// The result of [`build_provider`]: the concrete, backend-selected
/// provider (`selected`) alongside that same provider wrapped in the
/// resilience decorator (`resilient`), ready for the send path.
///
/// Two fields rather than returning `ResilientProvider<AnyProvider>` alone:
/// `ResilientProvider<P>` is deliberately not `Clone` and exposes no
/// accessor to its wrapped inner value (see `resilience.rs`), so a caller
/// that also needs the plain `AnyProvider` (main.rs's shared session state,
/// read by both `/model` and the `subagent` tool, which per ADR 0009 must
/// hold a concrete `AnyProvider`, not a generic `Provider` bound) cannot
/// recover it from the wrapped value alone. Returning both, built from one
/// selection pass, avoids re-running selection twice.
pub struct BuiltProvider {
    pub selected: AnyProvider,
    pub resilient: ResilientProvider<AnyProvider>,
}

/// Selects a backend given `resolved_auth` (see this module's doc comment
/// for why auth resolution itself is the caller's job) and wraps it in
/// [`ResilientProvider`] under `retry_policy`.
///
/// Mirrors `main.rs`'s prior `construct_provider`: a resolved
/// `Auth::OAuth` credential builds an `AnthropicProvider` directly from the
/// `ROKR_ANTHROPIC_BASE_URL`/`ROKR_ANTHROPIC_MODEL` env vars and the OAuth
/// access token (see `AnthropicProvider::new`'s KNOWN LIMITATION note,
/// carried over unchanged from ticket 31: OAuth and API-key credentials
/// still flow through the same constructor parameter). Anything else
/// (`None`, or a resolved `Auth::ApiKey`) falls through to the unchanged
/// `AnyProvider::from_env()` path, preserving `construct_provider`'s exact
/// behavior byte-for-byte, quirks included.
pub fn build_provider(
    resolved_auth: Option<Auth>,
    retry_policy: RetryPolicy,
) -> Result<BuiltProvider, String> {
    let selected = match resolved_auth {
        Some(Auth::OAuth { access_token, .. }) => {
            let base_url = std::env::var(crate::anthropic::ENV_BASE_URL).map_err(|_| {
                format!(
                    "missing required environment variable: {}",
                    crate::anthropic::ENV_BASE_URL
                )
            })?;
            let model = std::env::var(crate::anthropic::ENV_MODEL).map_err(|_| {
                format!(
                    "missing required environment variable: {}",
                    crate::anthropic::ENV_MODEL
                )
            })?;
            AnyProvider::Anthropic(AnthropicProvider::new(base_url, model, access_token))
        }
        _ => AnyProvider::from_env().map_err(|err| err.to_string())?,
    };

    let resilient = ResilientProvider::with_policy(selected.clone(), retry_policy);

    Ok(BuiltProvider { selected, resilient })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rokr_core::Provider;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Given a resolved OAuth credential, `build_provider` must (a) select
    /// the Anthropic backend (matching `construct_provider`'s existing
    /// OAuth branch) and (b) return a provider that actually retries a
    /// retryable failure -- not just a value that happens to type-check as
    /// `ResilientProvider<AnyProvider>`. The mock server fails the first
    /// two attempts with a retryable 503, then succeeds; if `build_provider`
    /// wired up the resilience decorator, `.send()` on `built.resilient`
    /// must still succeed, and the mock must have recorded all three
    /// attempts.
    #[tokio::test]
    async fn build_provider_selects_configured_backend_and_wraps_in_resilience_decorator() {
        let _lock = ENV_GUARD.lock().unwrap();

        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .mount(&mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_test",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "ok"}],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            })))
            .mount(&mock_server)
            .await;

        std::env::set_var(crate::anthropic::ENV_BASE_URL, mock_server.uri());
        std::env::set_var(crate::anthropic::ENV_MODEL, "claude-3-5-sonnet-20241022");

        let fast_policy = RetryPolicy {
            max_attempts: 5,
            base_delay: std::time::Duration::from_millis(1),
            max_delay: std::time::Duration::from_millis(5),
            max_elapsed: std::time::Duration::from_secs(5),
        };

        let resolved_auth = Some(Auth::OAuth {
            access_token: "test-access-token".to_string(),
            refresh_token: None,
            expires_at: None,
        });

        let built = build_provider(resolved_auth, fast_policy)
            .expect("build_provider should succeed given a resolved OAuth credential and valid env vars");

        assert!(
            matches!(built.selected, AnyProvider::Anthropic(_)),
            "a resolved OAuth credential should select the Anthropic backend, matching construct_provider's existing OAuth branch"
        );

        let messages = vec![rokr_core::Message::user_text("hello")];
        let result = built.resilient.send(&messages, &[]).await;

        std::env::remove_var(crate::anthropic::ENV_BASE_URL);
        std::env::remove_var(crate::anthropic::ENV_MODEL);

        result.expect(
            "build_provider's returned resilient provider should retry the retryable 503s \
             and ultimately succeed once the mock starts returning 200 -- if this fails, \
             build_provider is not actually wrapping the selected provider in the resilience \
             decorator",
        );

        assert_eq!(
            mock_server.received_requests().await.unwrap().len(),
            3,
            "expected exactly 3 attempts against the mock (2 retried 503s + 1 successful 200), \
             proving build_provider's returned provider is genuinely resilience-wrapped, not just \
             typed as ResilientProvider<AnyProvider>"
        );
    }
}
