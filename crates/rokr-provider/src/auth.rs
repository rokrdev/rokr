//! OAuth 2.0 Authorization Code flow with PKCE (RFC 7636), token storage
//! (OS keychain via the `keyring` crate, falling back to a
//! `0600`-permissioned file), and `Auth` resolution order (config, then
//! keychain/file, then the existing `ROKR_*_API_KEY` env var path).
//!
//! SECURITY: token values (`Auth::OAuth`'s `access_token`/`refresh_token`,
//! `Auth::ApiKey`'s inner string) must never be logged, printed, or included
//! in any error message. `Auth`'s `Debug` impl below is hand-written to
//! redact these fields for exactly that reason.
//!
//! The default OAuth endpoints/client_id below are UNVERIFIED placeholders
//! -- see [`OAuthEndpoints`]'s doc comment.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A resolved authentication credential for a provider: either a plain API
/// key (today's `ROKR_*_API_KEY` path) or a previously-completed OAuth
/// grant.
///
/// `Serialize`/`Deserialize` are used only for on-disk/keychain persistence
/// of the `OAuth` variant ([`FileTokenStore`], [`KeychainTokenStore`]) --
/// never for logging.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum Auth {
    ApiKey(String),
    OAuth {
        access_token: String,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        refresh_token: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none", default)]
        expires_at: Option<u64>,
    },
}

// Hand-written, not derived: redacts secret fields so a stray `{:?}` in a
// log line or panic message never leaks a token value.
impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::ApiKey(_) => f.debug_tuple("ApiKey").field(&"<redacted>").finish(),
            Auth::OAuth { expires_at, .. } => f
                .debug_struct("OAuth")
                .field("access_token", &"<redacted>")
                .field("refresh_token", &"<redacted>")
                .field("expires_at", expires_at)
                .finish(),
        }
    }
}

/// Abstracts token persistence so tests never touch a real OS keychain.
pub trait TokenStore {
    fn load(&self) -> Result<Option<Auth>, TokenStoreError>;
    fn save(&self, auth: &Auth) -> Result<(), TokenStoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("token store unavailable: {0}")]
    Unavailable(String),
    #[error("token store io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("token store serialization error: {0}")]
    Serde(#[from] serde_json::Error),
}

/// Resolves `Auth` in documented precedence order: an explicit config auth
/// block first (today this is always `None` -- `rokr-config`'s `Config` has
/// no `auth` field yet, see ticket 31's design notes), then a stored
/// keychain/file OAuth token, then the existing `ROKR_*_API_KEY` env var.
/// Stops at the first hit; later sources are never consulted once an
/// earlier one resolves.
///
pub fn resolve_auth(
    config_auth: Option<Auth>,
    token_store: &dyn TokenStore,
    env_var: &'static str,
) -> Option<Auth> {
    if let Some(auth) = config_auth {
        return Some(auth);
    }

    if let Ok(Some(auth)) = token_store.load() {
        return Some(auth);
    }

    std::env::var(env_var).ok().map(Auth::ApiKey)
}

/// Persists a token to a JSON file. Parent directories are created as
/// needed; on unix, permissions are set to `0600` after every write so the
/// token is never left world/group-readable on disk.
pub struct FileTokenStore {
    path: std::path::PathBuf,
}

impl FileTokenStore {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self { path }
    }
}

impl TokenStore for FileTokenStore {
    fn load(&self) -> Result<Option<Auth>, TokenStoreError> {
        match std::fs::read_to_string(&self.path) {
            Ok(contents) => Ok(Some(serde_json::from_str(&contents)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn save(&self, auth: &Auth) -> Result<(), TokenStoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string(auth)?;
        std::fs::write(&self.path, json)?;

        // Unix-only: Windows has no equivalent permission-bit model, so the
        // 0600 hardening is best-effort/skipped there (matches the
        // `#[cfg(unix)]` precedent already in this workspace, e.g.
        // rokr-config's `load_project_context_does_not_fall_back_when_agents_md_is_unreadable`).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

/// Tries `keychain` first; on ANY error (or empty result, for `load`) falls
/// back to `fallback` (a [`FileTokenStore`] in practice).
pub struct KeychainWithFileFallback<K: TokenStore, F: TokenStore> {
    keychain: K,
    fallback: F,
}

impl<K: TokenStore, F: TokenStore> KeychainWithFileFallback<K, F> {
    pub fn new(keychain: K, fallback: F) -> Self {
        Self { keychain, fallback }
    }
}

impl<K: TokenStore, F: TokenStore> TokenStore for KeychainWithFileFallback<K, F> {
    fn load(&self) -> Result<Option<Auth>, TokenStoreError> {
        match self.keychain.load() {
            Ok(Some(auth)) => Ok(Some(auth)),
            Ok(None) => self.fallback.load(),
            Err(_) => self.fallback.load(),
        }
    }

    fn save(&self, auth: &Auth) -> Result<(), TokenStoreError> {
        match self.keychain.save(auth) {
            Ok(()) => Ok(()),
            Err(_) => self.fallback.save(auth),
        }
    }
}

/// Real OS-keychain-backed token store, via the `keyring` crate's `v1`
/// compatibility API (macOS Keychain Services / Windows Credential Manager
/// / *nix Secret Service, depending on platform). The JSON-serialized
/// [`Auth`] is stored as the keychain entry's secret.
pub struct KeychainTokenStore {
    service: String,
    account: String,
}

impl KeychainTokenStore {
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, TokenStoreError> {
        keyring::Entry::new(&self.service, &self.account)
            .map_err(|err| TokenStoreError::Unavailable(err.to_string()))
    }
}

impl TokenStore for KeychainTokenStore {
    fn load(&self) -> Result<Option<Auth>, TokenStoreError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(json) => Ok(Some(serde_json::from_str(&json)?)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(TokenStoreError::Unavailable(err.to_string())),
        }
    }

    fn save(&self, auth: &Auth) -> Result<(), TokenStoreError> {
        let entry = self.entry()?;
        let json = serde_json::to_string(auth)?;
        entry
            .set_password(&json)
            .map_err(|err| TokenStoreError::Unavailable(err.to_string()))
    }
}

/// Env var (any value, not just `"1"`/`"true"`) that skips the real OS
/// keychain entirely and stores/reads only the `{config_dir}/oauth_token.json`
/// file. This exists so automated tests (and this crate's own CI) never
/// touch the real OS keychain -- on macOS in particular, a process's first
/// keychain access can trigger an interactive permission dialog, which
/// would hang a non-interactive test run. Not a general user-facing knob;
/// it's a testability escape hatch.
pub const ENV_FORCE_FILE_STORE: &str = "ROKR_AUTH_FORCE_FILE_STORE";

const KEYCHAIN_SERVICE: &str = "rokr";
const KEYCHAIN_ACCOUNT: &str = "anthropic-oauth";

/// Builds the token store used both by `rokr auth login` and by provider
/// construction's [`resolve_auth`] call, so both go through the identical
/// keychain/file resolution path. See [`ENV_FORCE_FILE_STORE`] for the
/// test-only escape hatch this checks.
pub fn default_token_store(config_dir: &std::path::Path) -> Box<dyn TokenStore> {
    let fallback = FileTokenStore::new(config_dir.join("oauth_token.json"));
    if std::env::var_os(ENV_FORCE_FILE_STORE).is_some() {
        Box::new(fallback)
    } else {
        let keychain = KeychainTokenStore::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
        Box::new(KeychainWithFileFallback::new(keychain, fallback))
    }
}

// ---------------------------------------------------------------------
// PKCE (RFC 7636) helpers
// ---------------------------------------------------------------------

/// Base64url alphabet (RFC 4648 sec. 5), no padding.
const BASE64URL_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Hand-rolled base64url-no-pad encoder (avoids adding a `base64` dependency
/// for a few lines of clearly-correct, RFC-4648-table-driven code; verified
/// against the RFC 7636 Appendix B.1 test vector in this module's tests).
fn base64url_no_pad(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;

        out.push(BASE64URL_ALPHABET[((n >> 18) & 0x3F) as usize] as char);
        out.push(BASE64URL_ALPHABET[((n >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(BASE64URL_ALPHABET[((n >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(BASE64URL_ALPHABET[(n & 0x3F) as usize] as char);
        }
    }
    out
}

/// Generates a cryptographically random PKCE code verifier: 32 random bytes,
/// base64url-no-pad encoded, yielding exactly 43 characters -- the minimum
/// of RFC 7636's required 43-128 character range (using more random bytes
/// would only push the length further into that range, not out of it).
pub fn generate_code_verifier() -> String {
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    base64url_no_pad(&bytes)
}

/// Computes the PKCE `S256` code challenge for `verifier`: SHA-256 the
/// verifier, base64url-no-pad encode the digest (RFC 7636 sec. 4.2). `S256`
/// is used rather than `plain` because `plain` sends the verifier itself as
/// the challenge, giving no protection if the authorization request URL (or
/// referrer headers on the auth server's own site) leaks the challenge to a
/// party other than the one holding the verifier; `S256` means possession
/// of the challenge alone never reveals or substitutes for the verifier.
pub fn code_challenge_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    base64url_no_pad(&digest)
}

/// Generates a cryptographically random, opaque CSRF-protection token for
/// the OAuth `state` parameter.
pub fn generate_state() -> String {
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    base64url_no_pad(&bytes)
}

// ---------------------------------------------------------------------
// OAuth endpoint configuration
// ---------------------------------------------------------------------

pub const ENV_AUTHORIZE_URL: &str = "ROKR_OAUTH_AUTHORIZE_URL";
pub const ENV_TOKEN_URL: &str = "ROKR_OAUTH_TOKEN_URL";
pub const ENV_CLIENT_ID: &str = "ROKR_OAUTH_CLIENT_ID";

/// **UNVERIFIED PLACEHOLDERS.** These default authorize/token URLs and
/// client_id are plausible-looking guesses, not confirmed against the real
/// Anthropic OAuth service -- this environment has no way to validate them
/// live. A human must confirm the real values (or override via
/// [`ENV_AUTHORIZE_URL`]/[`ENV_TOKEN_URL`]/[`ENV_CLIENT_ID`]) before `rokr
/// auth login` will work against production Anthropic infrastructure.
const DEFAULT_AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const DEFAULT_TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
const DEFAULT_CLIENT_ID: &str = "rokr-cli";

/// Endpoint/client_id configuration for the OAuth flow. See
/// `from_env_or_default`'s doc comment for the unverified-placeholder
/// caveat.
pub struct OAuthEndpoints {
    pub authorize_url: String,
    pub token_url: String,
    pub client_id: String,
}

impl OAuthEndpoints {
    /// Reads [`ENV_AUTHORIZE_URL`]/[`ENV_TOKEN_URL`]/[`ENV_CLIENT_ID`],
    /// falling back to unverified placeholder defaults when unset -- see
    /// this module's top-level doc comment and the `DEFAULT_*` constants'
    /// doc comment.
    pub fn from_env_or_default() -> Self {
        Self {
            authorize_url: std::env::var(ENV_AUTHORIZE_URL)
                .unwrap_or_else(|_| DEFAULT_AUTHORIZE_URL.to_string()),
            token_url: std::env::var(ENV_TOKEN_URL)
                .unwrap_or_else(|_| DEFAULT_TOKEN_URL.to_string()),
            client_id: std::env::var(ENV_CLIENT_ID)
                .unwrap_or_else(|_| DEFAULT_CLIENT_ID.to_string()),
        }
    }
}

// ---------------------------------------------------------------------
// Loopback + manual-paste PKCE login flow
// ---------------------------------------------------------------------

/// Env var (any value) that forces the manual-code-paste fallback instead
/// of attempting the `127.0.0.1` loopback redirect. Also forced
/// automatically if the loopback listener fails to bind. This is the
/// deterministic hook automated tests use, since the loopback path needs a
/// real browser and a real local HTTP redirect to complete interactively.
pub const ENV_NO_BROWSER: &str = "ROKR_AUTH_NO_BROWSER";

/// Out-of-band redirect URI used for the manual-paste path -- a documented
/// value some OAuth implementations recognize for non-loopback,
/// non-interactive redirect (RFC 8252 sec. 7.3 mentions it as a common
/// convention, though it is not itself an IETF-standardized value).
/// ASSUMPTION: not confirmed the real Anthropic token endpoint accepts or
/// requires this exact value; flagged for human verification alongside the
/// endpoint URLs.
const MANUAL_REDIRECT_URI: &str = "urn:ietf:wg:oauth:2.0:oob";

#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token exchange failed with status {0}")]
    TokenExchangeFailed(u16),
    #[error("callback state did not match the expected value (possible CSRF)")]
    StateMismatch,
    #[error("malformed authorization callback")]
    MalformedCallback,
    #[error(transparent)]
    TokenStore(#[from] TokenStoreError),
}

/// Percent-encodes `s` for use as a URL query parameter value (RFC 3986
/// unreserved characters pass through unchanged, everything else becomes
/// `%XX`). Hand-rolled for the same reason as `base64url_no_pad`: a few
/// clearly-correct lines beat a new dependency for this crate's needs.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(query: &str) -> std::collections::HashMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| {
            if pair.is_empty() {
                return None;
            }
            let mut parts = pair.splitn(2, '=');
            let key = parts.next()?;
            let value = parts.next().unwrap_or("");
            Some((percent_decode(key), percent_decode(value)))
        })
        .collect()
}

/// Reads a single HTTP request line off `stream` (the request line and
/// headers only -- no body is expected on a GET callback), extracts `code`
/// and `state` from the request-line's query string, verifies `state`
/// matches `expected_state` (the CSRF check -- a real comparison, not
/// skipped), and writes a minimal success response back before returning
/// the authorization code.
async fn receive_code_via_loopback(
    listener: tokio::net::TcpListener,
    expected_state: &str,
) -> Result<String, LoginError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (stream, _) = listener.accept().await?;
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;

    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or(LoginError::MalformedCallback)?;
    let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_query(query);

    let code = params
        .get("code")
        .cloned()
        .ok_or(LoginError::MalformedCallback)?;
    let state = params.get("state").cloned().unwrap_or_default();

    if state != expected_state {
        return Err(LoginError::StateMismatch);
    }

    let body = "You can close this window and return to the terminal.";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = reader.into_inner();
    stream.write_all(response.as_bytes()).await?;
    let _ = stream.shutdown().await;

    Ok(code)
}

/// Reads one line from stdin in the format `<code>#<state>` (split on the
/// LAST `#`, since an authorization code is not expected to contain one),
/// verifies `state` (the same real CSRF check as the loopback path -- not
/// skipped just because this is the fallback path), and returns the code.
fn receive_code_via_stdin(expected_state: &str) -> Result<String, LoginError> {
    use std::io::BufRead;

    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin.lock().read_line(&mut line)?;
    let line = line.trim();

    let (code, state) = line
        .rsplit_once('#')
        .ok_or(LoginError::MalformedCallback)?;

    if state != expected_state {
        return Err(LoginError::StateMismatch);
    }

    Ok(code.to_string())
}

#[derive(Deserialize)]
struct TokenExchangeResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

async fn exchange_code(
    endpoints: &OAuthEndpoints,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<TokenExchangeResponse, LoginError> {
    let client = reqwest::Client::new();
    let response = client
        .post(&endpoints.token_url)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &endpoints.client_id),
            ("code_verifier", verifier),
        ])
        .send()
        .await?;

    if !response.status().is_success() {
        // Deliberately discards the response body: some servers echo
        // request parameters back in error bodies, and this must never
        // risk surfacing a secret in an error path. Only the status code
        // is preserved.
        return Err(LoginError::TokenExchangeFailed(response.status().as_u16()));
    }

    Ok(response.json::<TokenExchangeResponse>().await?)
}

/// Runs the OAuth 2.0 Authorization Code + PKCE flow end to end: builds the
/// verifier/challenge/state, prints the authorization URL (always -- this
/// is both how the manual-paste path is completed by a human and how
/// automated tests observe `state`), attempts the `127.0.0.1` loopback
/// redirect unless [`ENV_NO_BROWSER`] is set or the bind fails (in which
/// case it falls back to reading a pasted `<code>#<state>` line from
/// stdin), exchanges the resulting code for a token, and saves it via
/// `token_store`.
///
/// Never logs or prints the access/refresh token value anywhere -- only a
/// generic "Login successful; token stored." on success.
pub async fn login(token_store: &dyn TokenStore) -> Result<(), LoginError> {
    let endpoints = OAuthEndpoints::from_env_or_default();
    let verifier = generate_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    let state = generate_state();

    let force_manual = std::env::var_os(ENV_NO_BROWSER).is_some();

    let (redirect_uri, listener) = if force_manual {
        (MANUAL_REDIRECT_URI.to_string(), None)
    } else {
        match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => match listener.local_addr() {
                Ok(addr) => (format!("http://127.0.0.1:{}/callback", addr.port()), Some(listener)),
                Err(_) => (MANUAL_REDIRECT_URI.to_string(), None),
            },
            Err(_) => (MANUAL_REDIRECT_URI.to_string(), None),
        }
    };

    let auth_url = format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state={}",
        endpoints.authorize_url,
        urlencode(&endpoints.client_id),
        urlencode(&redirect_uri),
        urlencode(&challenge),
        urlencode(&state),
    );

    // Always printed: this is both the manual-paste UX (a human clicks or
    // copies it) and the hook automated tests use to extract `state` from
    // the child process's stdout.
    println!("{auth_url}");

    let code = match listener {
        Some(listener) => {
            // Best-effort only: the URL is already printed above, so a
            // failure to launch a browser must not abort the flow.
            let _ = open::that(&auth_url);
            receive_code_via_loopback(listener, &state).await?
        }
        None => receive_code_via_stdin(&state)?,
    };

    let response = exchange_code(&endpoints, &code, &verifier, &redirect_uri).await?;

    let expires_at = response.expires_in.map(|secs_from_now| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        now + secs_from_now
    });

    let auth = Auth::OAuth {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at,
    };

    token_store.save(&auth)?;

    println!("Login successful; token stored.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_GUARD: Mutex<()> = Mutex::new(());

    const TEST_ENV_VAR: &str = "ROKR_TEST_AUTH_RESOLUTION_ENV_VAR";

    struct FakeTokenStore {
        value: Option<Auth>,
    }

    impl TokenStore for FakeTokenStore {
        fn load(&self) -> Result<Option<Auth>, TokenStoreError> {
            Ok(self.value.clone())
        }
        fn save(&self, _auth: &Auth) -> Result<(), TokenStoreError> {
            Ok(())
        }
    }

    /// Proves the keychain/file token store is never even consulted when
    /// `config_auth` is `Some` -- panics if `load`/`save` is called.
    struct PanicIfCalledTokenStore;

    impl TokenStore for PanicIfCalledTokenStore {
        fn load(&self) -> Result<Option<Auth>, TokenStoreError> {
            panic!("token store should not be consulted when config_auth is Some");
        }
        fn save(&self, _auth: &Auth) -> Result<(), TokenStoreError> {
            panic!("token store save should not be called by resolve_auth");
        }
    }

    #[test]
    fn auth_resolution_order_prefers_config_then_keychain_then_env_var() {
        let _lock = ENV_GUARD.lock().unwrap();

        // Level 1: config_auth Some -> returned immediately, keychain/env
        // never consulted (the fake store panics if it's called at all).
        std::env::set_var(TEST_ENV_VAR, "env-value-should-not-win");
        let config_auth = Some(Auth::ApiKey("config-value".to_string()));
        let panics_if_called = PanicIfCalledTokenStore;
        let resolved = resolve_auth(config_auth, &panics_if_called, TEST_ENV_VAR);
        assert_eq!(resolved, Some(Auth::ApiKey("config-value".to_string())));

        // Level 2: config_auth None, keychain has a value -> keychain wins
        // over the env var even though the env var is set to a DIFFERENT
        // distinguishable value.
        let keychain_value = Auth::OAuth {
            access_token: "keychain-token".to_string(),
            refresh_token: None,
            expires_at: None,
        };
        let store_with_value = FakeTokenStore {
            value: Some(keychain_value.clone()),
        };
        let resolved = resolve_auth(None, &store_with_value, TEST_ENV_VAR);
        assert_eq!(resolved, Some(keychain_value));

        // Level 3: config_auth None, keychain empty -> falls back to env var.
        let empty_store = FakeTokenStore { value: None };
        let resolved = resolve_auth(None, &empty_store, TEST_ENV_VAR);
        assert_eq!(
            resolved,
            Some(Auth::ApiKey("env-value-should-not-win".to_string()))
        );

        std::env::remove_var(TEST_ENV_VAR);

        // Level 4: nothing set anywhere -> None.
        let resolved = resolve_auth(None, &empty_store, TEST_ENV_VAR);
        assert_eq!(resolved, None);
    }

    /// A fake keychain backend that always fails, so the composite store's
    /// fallback-to-file behavior is exercised without touching a real OS
    /// keychain.
    struct AlwaysUnavailableTokenStore;

    impl TokenStore for AlwaysUnavailableTokenStore {
        fn load(&self) -> Result<Option<Auth>, TokenStoreError> {
            Err(TokenStoreError::Unavailable(
                "fake: no keychain in test".to_string(),
            ))
        }
        fn save(&self, _auth: &Auth) -> Result<(), TokenStoreError> {
            Err(TokenStoreError::Unavailable(
                "fake: no keychain in test".to_string(),
            ))
        }
    }

    #[test]
    #[cfg(unix)]
    fn oauth_token_falls_back_to_0600_file_when_keychain_unavailable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("oauth_token.json");
        let fallback = FileTokenStore::new(file_path.clone());
        let composite = KeychainWithFileFallback::new(AlwaysUnavailableTokenStore, fallback);

        let auth = Auth::OAuth {
            access_token: "test-access-token".to_string(),
            refresh_token: Some("test-refresh-token".to_string()),
            expires_at: Some(1_234_567_890),
        };

        composite
            .save(&auth)
            .expect("save should fall back to the file store and succeed");

        assert!(
            file_path.exists(),
            "expected fallback file to exist at {file_path:?}"
        );

        let metadata = std::fs::metadata(&file_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected fallback file permissions to be 0600, got {mode:o}"
        );

        let loaded = composite
            .load()
            .expect("load should fall back to the file store");
        assert_eq!(loaded, Some(auth));
    }

    /// Sanity check for the hand-rolled base64url encoder and SHA-256
    /// challenge computation against RFC 7636 Appendix B.1's worked
    /// example, independent of the two named unit tests above.
    #[test]
    fn code_challenge_s256_matches_rfc7636_appendix_b1_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let expected_challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert_eq!(code_challenge_s256(verifier), expected_challenge);
    }

    #[test]
    fn generate_code_verifier_is_within_rfc7636_length_bounds() {
        let verifier = generate_code_verifier();
        assert!(
            (43..=128).contains(&verifier.len()),
            "verifier length {} outside RFC 7636's 43-128 range",
            verifier.len()
        );
    }
}
