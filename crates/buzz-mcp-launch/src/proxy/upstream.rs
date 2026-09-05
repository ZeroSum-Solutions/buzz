//! The proxy's upstream boundary: which URLs are allowed, which header carries
//! the credential, and what the transport refuses (memo decision 4).

use url::Url;

use buzz_secret_store::sentinel::scan_query_pairs;

/// Why an upstream URL is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    /// The string is not a URL.
    #[error("upstream url is not a url: {0}")]
    NotAUrl(String),
    /// The scheme is neither `https` nor a loopback `http`.
    #[error("upstream url must be https (http is allowed only for a loopback host)")]
    InsecureScheme,
    /// The URL carries userinfo (`https://user:pass@host`).
    #[error("upstream url must not carry userinfo")]
    Userinfo,
    /// The URL has no host.
    #[error("upstream url has no host")]
    NoHost,
    /// The query string is credential-shaped.
    #[error("upstream url {0}")]
    CredentialInQuery(String),
    /// The URL is longer than [`MAX_URL_LEN`].
    #[error("upstream url is {0} bytes, over the {MAX_URL_LEN}-byte cap")]
    TooLong(usize),
}

/// Longest accepted upstream URL, in bytes.
pub const MAX_URL_LEN: usize = 2048;

/// Whether `host` is a loopback host, the single exception to https-only.
///
/// Matched on the parsed host rather than the string: `host_str()` renders an
/// IPv6 literal with brackets, and `127.0.0.2` is loopback too.
fn is_loopback(host: url::Host<&str>) -> bool {
    match host {
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
        url::Host::Domain(name) => name.eq_ignore_ascii_case("localhost"),
    }
}

/// Validate an upstream URL against every decision-4 rule.
///
/// # Errors
/// The [`UpstreamError`] naming the first rule breached.
pub fn validate_upstream(raw: &str) -> Result<Url, UpstreamError> {
    if raw.len() > MAX_URL_LEN {
        return Err(UpstreamError::TooLong(raw.len()));
    }
    let url = Url::parse(raw).map_err(|e| UpstreamError::NotAUrl(e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UpstreamError::Userinfo);
    }
    let host = url.host().ok_or(UpstreamError::NoHost)?;
    match url.scheme() {
        "https" => {}
        "http" if is_loopback(host) => {}
        _ => return Err(UpstreamError::InsecureScheme),
    }
    if let Some(hit) = scan_query_pairs(
        url.query_pairs()
            .map(|(name, value)| {
                // `query_pairs` yields Cow; leak-free borrow via an owned pair below.
                (name.into_owned(), value.into_owned())
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str())),
    ) {
        return Err(UpstreamError::CredentialInQuery(hit.reason));
    }
    Ok(url)
}

/// The enumerated set of auth schemes, and the pinned header each uses.
///
/// The header name never comes from operator free text: a free-text name is a
/// way to redirect a resolved credential into a header the upstream echoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthScheme {
    /// `Authorization: Bearer <secret>`.
    Bearer,
    /// `Authorization: token <secret>` (GitHub's spelling).
    Token,
    /// `Authorization: Basic <secret>`, with a pre-encoded secret.
    Basic,
    /// `X-Api-Key: <secret>`.
    ApiKey,
}

impl AuthScheme {
    /// The pinned header name for this scheme.
    pub fn header_name(self) -> &'static str {
        match self {
            Self::Bearer | Self::Token | Self::Basic => "authorization",
            Self::ApiKey => "x-api-key",
        }
    }

    /// The header value carrying `secret`.
    pub fn header_value(self, secret: &str) -> String {
        match self {
            Self::Bearer => format!("Bearer {secret}"),
            Self::Token => format!("token {secret}"),
            Self::Basic => format!("Basic {secret}"),
            Self::ApiKey => secret.to_string(),
        }
    }
}

/// Why an auth scheme string is refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not a supported auth scheme (expected bearer, token, basic or api-key)")]
pub struct UnknownAuthScheme(pub String);

impl std::str::FromStr for AuthScheme {
    type Err = UnknownAuthScheme;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.to_ascii_lowercase().as_str() {
            "bearer" => Ok(Self::Bearer),
            "token" => Ok(Self::Token),
            "basic" => Ok(Self::Basic),
            "api-key" | "api_key" | "apikey" => Ok(Self::ApiKey),
            other => Err(UnknownAuthScheme(other.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn https_is_required_except_on_loopback() {
        assert!(validate_upstream("https://api.example.com/mcp").is_ok());
        assert!(validate_upstream("http://127.0.0.1:8080/mcp").is_ok());
        assert!(validate_upstream("http://localhost:8080/mcp").is_ok());
        assert!(validate_upstream("http://[::1]:8080/mcp").is_ok());
        assert_eq!(
            validate_upstream("http://api.example.com/mcp"),
            Err(UpstreamError::InsecureScheme)
        );
        assert_eq!(
            validate_upstream("ftp://api.example.com/mcp"),
            Err(UpstreamError::InsecureScheme)
        );
    }

    #[test]
    fn userinfo_and_credential_queries_are_refused() {
        assert_eq!(
            validate_upstream("https://user:pass@api.example.com/mcp"),
            Err(UpstreamError::Userinfo)
        );
        assert_eq!(
            validate_upstream("https://user@api.example.com/mcp"),
            Err(UpstreamError::Userinfo)
        );
        let err = validate_upstream("https://api.example.com/mcp?access_token=abc123")
            .expect_err("a credential query must be refused");
        assert!(
            matches!(err, UpstreamError::CredentialInQuery(_)),
            "{err:?}"
        );
        assert!(validate_upstream("https://api.example.com/mcp?workspace=acme").is_ok());
    }

    #[test]
    fn the_url_is_capped() {
        let long = format!("https://api.example.com/{}", "a".repeat(MAX_URL_LEN));
        assert_eq!(
            validate_upstream(&long),
            Err(UpstreamError::TooLong(long.len()))
        );
    }

    #[test]
    fn header_names_come_from_the_pinned_map_only() {
        assert_eq!(AuthScheme::Bearer.header_name(), "authorization");
        assert_eq!(AuthScheme::ApiKey.header_name(), "x-api-key");
        assert_eq!(AuthScheme::Bearer.header_value("s3cret"), "Bearer s3cret");
        assert_eq!(AuthScheme::Token.header_value("s3cret"), "token s3cret");
        assert_eq!(AuthScheme::ApiKey.header_value("s3cret"), "s3cret");
        assert_eq!(AuthScheme::from_str("BEARER"), Ok(AuthScheme::Bearer));
        assert_eq!(
            AuthScheme::from_str("x-custom-header"),
            Err(UnknownAuthScheme("x-custom-header".to_string()))
        );
    }
}
