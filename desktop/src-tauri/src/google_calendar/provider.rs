//! Native-only Google OAuth transport and pinned RS256 verifier.
use super::{
    oauth::{AuthRequest, IdTokenSignatureVerifier},
    redact::Redacted,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::Deserialize;
use std::{io::Read, time::Duration};

const MAX_RESPONSE: usize = 64 * 1024;
pub struct Config {
    pub client_id: String,
    pub client_secret: Option<Redacted<String>>,
}
impl Config {
    pub fn load() -> Result<Self, String> {
        let id = std::env::var("BUZZ_GOOGLE_CALENDAR_CLIENT_ID")
            .ok()
            .or_else(|| option_env!("BUZZ_GOOGLE_CALENDAR_CLIENT_ID").map(str::to_string))
            .unwrap_or_default();
        if id.is_empty() {
            return Err(
                "Google Calendar is unavailable until a Buzz Desktop OAuth client is configured"
                    .into(),
            );
        }
        if id.len() > 256
            || !id.ends_with(".apps.googleusercontent.com")
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
        {
            return Err("invalid Google Calendar desktop client configuration".into());
        }
        let secret = std::env::var("BUZZ_GOOGLE_CALENDAR_CLIENT_SECRET")
            .ok()
            .or_else(|| option_env!("BUZZ_GOOGLE_CALENDAR_CLIENT_SECRET").map(str::to_string));
        if secret.as_ref().is_some_and(|s| s.len() > 1024) {
            return Err("invalid Google Calendar desktop client configuration".into());
        }
        Ok(Self {
            client_id: id,
            client_secret: secret.map(Redacted::new),
        })
    }
}
#[derive(Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub expires_in: i64,
    pub token_type: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
    pub scope: Option<String>,
}
impl Tokens {
    fn validate(self) -> Result<Self, String> {
        if !self.token_type.eq_ignore_ascii_case("bearer")
            || self.access_token.is_empty()
            || self.access_token.len() > 8192
            || !(1..=86400).contains(&self.expires_in)
            || self
                .refresh_token
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 8192)
        {
            return Err("Google returned an invalid token response".into());
        }
        Ok(self)
    }
}
#[derive(Debug)]
pub struct TokenFailure {
    pub state: super::failure::FailureState,
    pub detail: String,
}
impl std::fmt::Display for TokenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}
impl From<String> for TokenFailure {
    fn from(detail: String) -> Self {
        Self {
            state: super::failure::FailureState::AppError,
            detail,
        }
    }
}
impl From<&str> for TokenFailure {
    fn from(detail: &str) -> Self {
        detail.to_string().into()
    }
}
impl TokenFailure {
    fn transient() -> Self {
        Self {
            state: super::failure::classify_token(None, ""),
            detail: "Google authorization connection is temporarily unavailable".into(),
        }
    }
}
pub struct Provider(reqwest::blocking::Client);
impl Provider {
    pub fn new() -> Result<Self, String> {
        Ok(Self(
            reqwest::blocking::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(20))
                .build()
                .map_err(|_| "cannot initialize Google connection")?,
        ))
    }
    fn body(response: reqwest::blocking::Response) -> Result<Vec<u8>, String> {
        if !response.status().is_success() {
            return Err(format!(
                "Google authorization request failed (HTTP {})",
                response.status().as_u16()
            ));
        }
        let mut bytes = Vec::new();
        response
            .take((MAX_RESPONSE + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| "Google response could not be read")?;
        if bytes.len() > MAX_RESPONSE {
            return Err("Google response exceeded byte cap".into());
        }
        Ok(bytes)
    }
    fn form(&self, config: &Config, fields: &[(&str, &str)]) -> Result<Tokens, TokenFailure> {
        let mut values = fields.to_vec();
        values.push(("client_id", &config.client_id));
        if let Some(secret) = &config.client_secret {
            values.push(("client_secret", secret.expose()));
        }
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(values)
            .finish();
        let response = self
            .0
            .post("https://oauth2.googleapis.com/token")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .map_err(|_| TokenFailure::transient())?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response
            .take((MAX_RESPONSE + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| TokenFailure::transient())?;
        if bytes.len() > MAX_RESPONSE {
            return Err("Google token response exceeded byte cap".into());
        }
        if status != 200 {
            let reason = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            return Err(TokenFailure{state:super::failure::classify_token(Some(status),&reason),detail:format!("Google authorization request failed (HTTP {status}); reconnect if access was revoked")});
        }
        serde_json::from_slice::<Tokens>(&bytes)
            .map_err(|_| TokenFailure::from("invalid Google token response"))?
            .validate()
            .map_err(TokenFailure::from)
    }
    pub fn exchange(
        &self,
        config: &Config,
        request: &AuthRequest,
        code: &Redacted<String>,
    ) -> Result<Tokens, String> {
        self.form(
            config,
            &[
                ("grant_type", "authorization_code"),
                ("code", code.expose()),
                ("redirect_uri", &request.redirect_uri),
                ("code_verifier", request.pkce.verifier.expose()),
            ],
        )
        .map_err(|error| error.to_string())
    }
    pub fn refresh(
        &self,
        config: &Config,
        refresh: &Redacted<String>,
    ) -> Result<Tokens, TokenFailure> {
        self.form(
            config,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh.expose()),
            ],
        )
    }
    pub fn subject(&self, token: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct User {
            sub: String,
        }
        let response = self
            .0
            .get("https://openidconnect.googleapis.com/v1/userinfo")
            .bearer_auth(token)
            .send()
            .map_err(|_| "Google identity check failed")?;
        let user: User = serde_json::from_slice(&Self::body(response)?)
            .map_err(|_| "invalid Google identity response")?;
        if user.sub.is_empty() || user.sub.len() > 256 {
            return Err("invalid Google identity subject".into());
        }
        Ok(user.sub)
    }
    pub fn verifier(&self) -> Result<GoogleVerifier, String> {
        let response = self
            .0
            .get("https://www.googleapis.com/oauth2/v3/certs")
            .send()
            .map_err(|_| "Google signing keys unavailable")?;
        GoogleVerifier::parse(&Self::body(response)?)
    }
    pub fn revoke(&self, token: &Redacted<String>) -> Option<u16> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token.expose())
            .finish();
        self.0
            .post("https://oauth2.googleapis.com/revoke")
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .ok()
            .map(|r| r.status().as_u16())
    }
}
#[derive(Deserialize)]
struct Jwk {
    kid: String,
    kty: String,
    alg: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    n: String,
    e: String,
}
pub struct GoogleVerifier(Vec<Jwk>);
impl GoogleVerifier {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        #[derive(Deserialize)]
        struct Keys {
            keys: Vec<Jwk>,
        }
        if bytes.len() > MAX_RESPONSE {
            return Err("signing keys exceed byte cap".into());
        }
        let keys: Keys =
            serde_json::from_slice(bytes).map_err(|_| "invalid Google signing keys")?;
        if keys.keys.is_empty() || keys.keys.len() > 16 {
            return Err("invalid signing key count".into());
        }
        Ok(Self(keys.keys))
    }
}
impl IdTokenSignatureVerifier for GoogleVerifier {
    fn verify(&self, input: &str, signature: &[u8], kid: Option<&str>) -> Result<(), String> {
        let key = self
            .0
            .iter()
            .find(|k| {
                Some(k.kid.as_str()) == kid
                    && k.kty == "RSA"
                    && k.alg.as_deref() == Some("RS256")
                    && k.usage.as_deref() == Some("sig")
            })
            .ok_or("unknown Google signing key")?;
        let decoding = jsonwebtoken::DecodingKey::from_rsa_components(&key.n, &key.e)
            .map_err(|_| "invalid RSA signing key")?;
        if !jsonwebtoken::crypto::verify(
            &URL_SAFE_NO_PAD.encode(signature),
            input.as_bytes(),
            &decoding,
            jsonwebtoken::Algorithm::RS256,
        )
        .map_err(|_| "invalid ID token signature")?
        {
            return Err("invalid ID token signature".into());
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calendar_google_verifier_rejects_unknown_keys_and_forged_signatures() {
        let verifier=GoogleVerifier::parse(br#"{"keys":[{"kid":"k","kty":"RSA","alg":"RS256","use":"sig","n":"AQAB","e":"AQAB"}]}"#).unwrap();
        assert!(verifier
            .verify("header.payload", &[1, 2, 3], Some("k"))
            .is_err());
        assert!(verifier
            .verify("header.payload", &[1, 2, 3], Some("other"))
            .is_err());
    }
}

#[cfg(test)]
mod signature_regression {
    use super::*;
    // Synthetic public key/signature; the temporary private key was discarded.
    #[test]
    fn calendar_google_verifier_checks_a_valid_rsa_signature_and_tampering() {
        let verifier=GoogleVerifier::parse(br#"{"keys":[{"kid":"synthetic","kty":"RSA","alg":"RS256","use":"sig","n":"mM2yxraN06gy1l_LcDBXDsoYeIqDr1-ZOxjolL0B6edgN0Ih_gPFiFrJV728Zh01pfg7NyqIaDZ-GmunSMsElnzgxFaPfsOEQmV6sz5jMtbvhJn3sipvyERQNUfWe3ANOGsYfE6f3ItNZ9ELoVhc2VCco0OghnaqmshGU5ZZuCFkyKGRyOZfVlEEWPIxU-Te5wan-a-wVrbyj4Sn4rtirDOiMVHDmVACvvTwurkcnOzdAL0m04p3brquBBXazKN2TeMIQGZqZQjmFifopVpoR9gNFGuwPlLwGqjePZ69bDPk6E1EGuj0-qdOJeT_zOCff7cKt_yugVrYGm6eG4O1Qw","e":"AQAB"}]}"#).unwrap();
        let signature=URL_SAFE_NO_PAD.decode("hcKPFw81hNMHm1OhrQxusmodj_vtNo5RE1GX_HkUWT-d9v6M-6VAszUq83m_aySkRZ_iZM3N2_M6bRjWUtyuUnGtqQMVOyIn9ZVBnu0txCLo-dy1xlT1mESE2J-X_NpeH0b4AYVyz8COJnLHjZV01laQ76ZijHColrYRMb4jpj6nEt7ZnMwE5P5KTKFGcVcuAChEmnj90qiJHivVD6rMjh6IMtTuCT39unEcBQQbuBpEpxXi_hTjfKUmLVIir_loxhFJyEG-PyaRGEbbfiA0q_MNLR3OdBq70QU84uIgJUb41UFOPyWjQHZH3j1W7NkStgDwQVEkFZ5IK9E67KqSqg").unwrap();
        assert!(verifier
            .verify("header.payload", &signature, Some("synthetic"))
            .is_ok());
        assert!(verifier
            .verify("header.changed", &signature, Some("synthetic"))
            .is_err());
    }
}
