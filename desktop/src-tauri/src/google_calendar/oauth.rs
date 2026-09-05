//! The authorization request of T11 decision 1 and the checks that must pass
//! before a binding is written (decisions 1 and 2).
//!
//! PKCE protects the authorization code, not its origin, so the callback is
//! only accepted when a CSPRNG `state` comes back equal, and the ID token is
//! only accepted when a CSPRNG `nonce` comes back inside it. An exchange
//! without a refresh token, or short of the requested scopes, writes no
//! binding at all.
//!
//! Signature verification is a seam, not a default:
//! [`IdTokenSignatureVerifier`] has no implementation in this module, so no
//! path here can produce validated claims without one being supplied. Wiring
//! Google's JWKS to it is part of the connect flow (see the module docs of
//! [`super`] for the slice boundary).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use sha2::{Digest as _, Sha256};

use super::redact::{constant_time_eq, Redacted};

/// The scopes the integration requests (T11 decision 1).
///
/// `calendar.events` already grants read and write, so neither
/// `calendar.events.readonly` nor `calendarList` is requested.
pub const SCOPES: &[&str] = &[
    "openid",
    "email",
    "https://www.googleapis.com/auth/calendar.events",
];

/// Google's issuer values, both spellings.
pub const ISSUERS: &[&str] = &["https://accounts.google.com", "accounts.google.com"];

/// Bytes of entropy behind each of `state`, `nonce` and the PKCE verifier.
pub const ENTROPY_BYTES: usize = 32;

/// Largest callback query string accepted, in bytes.
pub const MAX_QUERY_BYTES: usize = 4096;
/// Largest number of query parameters accepted in a callback.
pub const MAX_QUERY_PARAMS: usize = 16;
/// Largest single query value accepted, in bytes.
pub const MAX_QUERY_VALUE_BYTES: usize = 2048;
/// Largest ID token accepted, in bytes.
pub const MAX_ID_TOKEN_BYTES: usize = 8192;

/// Why entropy could not be drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntropyError(pub String);

impl std::fmt::Display for EntropyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "entropy source: {}", self.0)
    }
}

impl std::error::Error for EntropyError {}

/// Draw `ENTROPY_BYTES` of OS entropy, base64url-encoded without padding.
///
/// # Errors
/// Returns [`EntropyError`] when the OS entropy source fails. A failure is
/// surfaced, never replaced with a weaker source.
pub fn random_token() -> Result<String, EntropyError> {
    let mut buffer = [0u8; ENTROPY_BYTES];
    getrandom::getrandom(&mut buffer).map_err(|error| EntropyError(error.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(buffer))
}

/// A PKCE verifier and the challenge derived from it.
#[derive(Debug, Clone)]
pub struct PkcePair {
    /// The verifier, sent only on the token exchange.
    pub verifier: Redacted<String>,
    /// The S256 challenge, sent on the authorization request.
    pub challenge: String,
}

impl PkcePair {
    /// Generate a fresh pair.
    ///
    /// # Errors
    /// Returns [`EntropyError`] when the OS entropy source fails.
    pub fn generate() -> Result<Self, EntropyError> {
        let verifier = random_token()?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier: Redacted::new(verifier),
            challenge,
        })
    }
}

/// One authorization request in flight.
#[derive(Debug)]
pub struct AuthRequest {
    /// The Cloud project's installed-app client id.
    pub client_id: String,
    /// The loopback redirect the listener bound.
    pub redirect_uri: String,
    /// CSPRNG `state`, checked on the callback.
    pub state: Redacted<String>,
    /// CSPRNG `nonce`, checked inside the ID token.
    pub nonce: Redacted<String>,
    /// The PKCE pair for this flow.
    pub pkce: PkcePair,
    /// Whether to force the consent screen. Set when no refresh token is held,
    /// because Google returns one only on a consented grant.
    pub prompt_consent: bool,
}

impl AuthRequest {
    /// Start a flow against `client_id`, redirecting to `redirect_uri`.
    ///
    /// # Errors
    /// Returns [`EntropyError`] when the OS entropy source fails.
    pub fn new(
        client_id: impl Into<String>,
        redirect_uri: impl Into<String>,
        prompt_consent: bool,
    ) -> Result<Self, EntropyError> {
        Ok(Self {
            client_id: client_id.into(),
            redirect_uri: redirect_uri.into(),
            state: Redacted::new(random_token()?),
            nonce: Redacted::new(random_token()?),
            pkce: PkcePair::generate()?,
            prompt_consent,
        })
    }

    /// The authorization URL to open in the browser.
    pub fn authorization_url(&self) -> String {
        let mut url =
            String::from("https://accounts.google.com/o/oauth2/v2/auth?response_type=code");
        let mut push = |key: &str, value: &str| {
            url.push('&');
            url.push_str(key);
            url.push('=');
            url.push_str(&encode_component(value));
        };
        push("client_id", &self.client_id);
        push("redirect_uri", &self.redirect_uri);
        push("scope", &SCOPES.join(" "));
        push("state", self.state.expose());
        push("nonce", self.nonce.expose());
        push("code_challenge", &self.pkce.challenge);
        push("code_challenge_method", "S256");
        push("access_type", "offline");
        if self.prompt_consent {
            push("prompt", "consent");
        }
        url
    }

    /// Check a callback query against this request.
    ///
    /// # Errors
    /// Returns [`CallbackError`] when the query is over a bound, carries
    /// Google's `error`, is missing `state` or `code`, or the `state` is not
    /// equal to the one this request generated.
    pub fn verify_callback(&self, query: &str) -> Result<Redacted<String>, CallbackError> {
        verify_callback_query(query, self.state.expose())
    }
}

/// Percent-encode one query component.
fn encode_component(raw: &str) -> String {
    percent_encoding::utf8_percent_encode(raw, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Why a callback was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackError {
    /// The query was over [`MAX_QUERY_BYTES`], carried more than
    /// [`MAX_QUERY_PARAMS`] parameters, or held an over-long value.
    OverBound(&'static str),
    /// Google reported an error. The value is its `error` parameter, capped.
    Reported(String),
    /// A required parameter was absent.
    Missing(&'static str),
    /// The `state` did not equal the one this flow generated.
    StateMismatch,
}

impl std::fmt::Display for CallbackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallbackError::OverBound(what) => write!(f, "authorization callback exceeded {what}"),
            CallbackError::Reported(error) => {
                write!(f, "authorization was refused: {error}")
            }
            CallbackError::Missing(field) => {
                write!(f, "authorization callback is missing `{field}`")
            }
            CallbackError::StateMismatch => {
                write!(f, "authorization callback carried an unexpected state")
            }
        }
    }
}

impl std::error::Error for CallbackError {}

/// Check a callback query string against `expected_state`.
///
/// The bounds are checked before parsing, so an oversized callback costs one
/// length comparison. The returned code is redacted: it is a credential until
/// it is exchanged.
///
/// # Errors
/// See [`CallbackError`].
pub fn verify_callback_query(
    query: &str,
    expected_state: &str,
) -> Result<Redacted<String>, CallbackError> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(CallbackError::OverBound("the query byte cap"));
    }
    let pairs: Vec<(&str, &str)> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (key, value),
            None => (pair, ""),
        })
        .collect();
    if pairs.len() > MAX_QUERY_PARAMS {
        return Err(CallbackError::OverBound("the query parameter cap"));
    }
    if pairs
        .iter()
        .any(|(_, value)| value.len() > MAX_QUERY_VALUE_BYTES)
    {
        return Err(CallbackError::OverBound("the query value cap"));
    }
    let find = |name: &str| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| decode_component(value))
    };

    if let Some(error) = find("error") {
        return Err(CallbackError::Reported(
            error.chars().take(128).collect::<String>(),
        ));
    }
    let state = find("state").ok_or(CallbackError::Missing("state"))?;
    if !constant_time_eq(&state, expected_state) {
        return Err(CallbackError::StateMismatch);
    }
    let code = find("code").ok_or(CallbackError::Missing("code"))?;
    if code.is_empty() {
        return Err(CallbackError::Missing("code"));
    }
    Ok(Redacted::new(code))
}

/// Percent-decode one query component, leaving an undecodable value as-is.
fn decode_component(raw: &str) -> String {
    let replaced = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&replaced)
        .decode_utf8()
        .map(|decoded| decoded.into_owned())
        .unwrap_or(replaced)
}

/// Why a token response cannot become a binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    /// Google returned no refresh token, so the grant cannot outlive the
    /// access token.
    NoRefreshToken,
    /// A requested scope was not granted.
    ScopeMissing(String),
    /// The response carried no ID token, so no identity can be bound.
    NoIdToken,
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeError::NoRefreshToken => {
                write!(f, "the token exchange returned no refresh token")
            }
            ExchangeError::ScopeMissing(scope) => write!(f, "the scope `{scope}` was not granted"),
            ExchangeError::NoIdToken => write!(f, "the token exchange returned no ID token"),
        }
    }
}

impl std::error::Error for ExchangeError {}

/// Check that an exchange carried everything a binding needs.
///
/// # Errors
/// Returns [`ExchangeError`] when the refresh token or the ID token is absent,
/// or when any of [`SCOPES`] was not granted.
pub fn check_exchange(
    granted_scopes: &[String],
    has_refresh_token: bool,
    has_id_token: bool,
) -> Result<(), ExchangeError> {
    if !has_refresh_token {
        return Err(ExchangeError::NoRefreshToken);
    }
    if !has_id_token {
        return Err(ExchangeError::NoIdToken);
    }
    for scope in SCOPES {
        if !granted_scopes.iter().any(|granted| granted == scope) {
            return Err(ExchangeError::ScopeMissing((*scope).to_string()));
        }
    }
    Ok(())
}

/// The ID token claims a binding is written from.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct IdTokenClaims {
    /// Issuer.
    pub iss: String,
    /// Audience: this installation's client id.
    pub aud: String,
    /// The stable Google account id an identity binds to.
    pub sub: String,
    /// Expiry, in seconds since the Unix epoch.
    pub exp: i64,
    /// The nonce this flow generated.
    #[serde(default)]
    pub nonce: Option<String>,
    /// The account's email, when the `email` scope returned one.
    #[serde(default)]
    pub email: Option<String>,
}

/// What the claims must say for this flow.
#[derive(Debug, Clone)]
pub struct IdTokenExpectations {
    /// The client id the token must be addressed to.
    pub client_id: String,
    /// The nonce this flow generated.
    pub nonce: Redacted<String>,
    /// Now, in milliseconds since the Unix epoch.
    pub now_ms: i64,
}

/// Verifies an ID token's signature against the issuer's keys.
///
/// Deliberately unimplemented in this module: no path here can produce
/// validated claims without a verifier, so a missing JWKS wiring is a compile
/// error at the call site rather than an unsigned token being trusted.
pub trait IdTokenSignatureVerifier {
    /// Verify the signature over `signing_input` (`header.payload`).
    ///
    /// # Errors
    /// Returns a reason when the signature is absent, malformed or does not
    /// verify against the issuer's current keys.
    fn verify(
        &self,
        signing_input: &str,
        signature: &[u8],
        key_id: Option<&str>,
    ) -> Result<(), String>;
}

/// Why an ID token was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdTokenError {
    /// Over [`MAX_ID_TOKEN_BYTES`], or not three dot-separated segments.
    Malformed(&'static str),
    /// The signature did not verify.
    Signature(String),
    /// The issuer was not one of [`ISSUERS`].
    Issuer,
    /// The audience was not this installation's client id.
    Audience,
    /// The token had expired.
    Expired,
    /// The nonce was absent or not the one this flow generated.
    Nonce,
    /// The `sub` claim was empty.
    Subject,
}

impl std::fmt::Display for IdTokenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdTokenError::Malformed(detail) => write!(f, "the ID token is malformed: {detail}"),
            IdTokenError::Signature(reason) => {
                write!(f, "the ID token signature did not verify: {reason}")
            }
            IdTokenError::Issuer => write!(f, "the ID token came from an unexpected issuer"),
            IdTokenError::Audience => write!(f, "the ID token is addressed to another client"),
            IdTokenError::Expired => write!(f, "the ID token has expired"),
            IdTokenError::Nonce => write!(f, "the ID token did not echo this flow's nonce"),
            IdTokenError::Subject => write!(f, "the ID token carries no subject"),
        }
    }
}

impl std::error::Error for IdTokenError {}

/// Verify an ID token and return its claims.
///
/// The order matters: bounds, then shape, then signature, then claims. Nothing
/// downstream sees claims from a token whose signature did not verify.
///
/// # Errors
/// See [`IdTokenError`].
pub fn verify_id_token(
    raw: &str,
    verifier: &dyn IdTokenSignatureVerifier,
    expectations: &IdTokenExpectations,
) -> Result<IdTokenClaims, IdTokenError> {
    if raw.len() > MAX_ID_TOKEN_BYTES {
        return Err(IdTokenError::Malformed("over the byte cap"));
    }
    let mut segments = raw.split('.');
    let (Some(header), Some(payload), Some(signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(IdTokenError::Malformed("not three segments"));
    };
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| IdTokenError::Malformed("the signature is not base64url"))?;
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header)
        .map_err(|_| IdTokenError::Malformed("the header is not base64url"))?;
    let key_id = serde_json::from_slice::<serde_json::Value>(&header_bytes)
        .ok()
        .and_then(|header| {
            header
                .get("kid")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        });
    let signing_input = &raw[..header.len() + 1 + payload.len()];
    verifier
        .verify(signing_input, &signature_bytes, key_id.as_deref())
        .map_err(IdTokenError::Signature)?;

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| IdTokenError::Malformed("the payload is not base64url"))?;
    let claims: IdTokenClaims = serde_json::from_slice(&payload_bytes)
        .map_err(|_| IdTokenError::Malformed("the payload is not the expected object"))?;

    if !ISSUERS.contains(&claims.iss.as_str()) {
        return Err(IdTokenError::Issuer);
    }
    if claims.aud != expectations.client_id {
        return Err(IdTokenError::Audience);
    }
    if claims.exp.saturating_mul(1000) <= expectations.now_ms {
        return Err(IdTokenError::Expired);
    }
    let echoed = claims.nonce.as_deref().ok_or(IdTokenError::Nonce)?;
    if !constant_time_eq(echoed, expectations.nonce.expose()) {
        return Err(IdTokenError::Nonce);
    }
    if claims.sub.is_empty() {
        return Err(IdTokenError::Subject);
    }
    Ok(claims)
}
