//! Proxy mode: a local stdio MCP server in front of a Streamable HTTP upstream
//! (memo decision 4).
//!
//! No generated JSON or TOML ever holds a secret value. The proxy resolves its
//! one bound secret through the capability of decision 5, at first use, and
//! attaches it as a header from a pinned map. Redirects are disabled, so no
//! origin change can carry the credential; the scheme must be `https` outside
//! loopback; and every inbound frame is bounded before it is parsed.

pub mod codec;
pub mod limits;
pub mod upstream;

use std::sync::Arc;

use buzz_secret_store::{AgentCapability, McpSecretLookup, McpSecretRef, SecretBlobSource};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, OnceCell, Semaphore};
use url::Url;

use codec::{FrameDecoder, FrameError};
use limits::ProxyLimits;
use upstream::AuthScheme;

/// Why the proxy could not start or could not forward a message.
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    /// The HTTP client could not be built.
    #[error("http client: {0}")]
    Client(String),
    /// The credential could not be resolved.
    #[error("credential: {0}")]
    Credential(String),
    /// The upstream refused, timed out, or answered outside the bounds.
    #[error("upstream: {0}")]
    Upstream(String),
    /// The upstream answered with a redirect, which is never followed.
    #[error("upstream returned a {0} redirect; redirects are disabled so the credential cannot follow an origin change")]
    Redirect(u16),
    /// The response body was over the cap.
    #[error("upstream response exceeded the {cap}-byte cap")]
    ResponseTooLarge {
        /// The configured cap.
        cap: usize,
    },
    /// The outbound request body was over the cap.
    #[error("request body is {len} bytes, over the {cap}-byte cap")]
    RequestTooLarge {
        /// Observed length.
        len: usize,
        /// The configured cap.
        cap: usize,
    },
    /// The stdio transport failed or breached its frame bound.
    #[error("stdio transport: {0}")]
    Transport(#[from] FrameError),
    /// Writing a response back to the client failed.
    #[error("stdio write: {0}")]
    Write(String),
}

/// Everything one proxy process needs.
pub struct ProxyConfig<S: SecretBlobSource> {
    /// The validated upstream URL.
    pub url: Url,
    /// The auth scheme, when the upstream needs one.
    pub auth: Option<AuthScheme>,
    /// The bound secret reference, when the upstream needs one.
    pub secret: Option<McpSecretRef>,
    /// The capability that authorizes the secret read.
    pub capability: Option<AgentCapability>,
    /// The secret store to resolve through.
    pub secrets: S,
    /// Resource bounds.
    pub limits: ProxyLimits,
}

/// A running proxy.
pub struct Proxy<S: SecretBlobSource> {
    client: reqwest::Client,
    url: Url,
    auth: Option<AuthScheme>,
    secret: Option<McpSecretRef>,
    capability: Option<AgentCapability>,
    lookup: McpSecretLookup<S>,
    header_value: OnceCell<String>,
    limits: ProxyLimits,
    in_flight: Arc<Semaphore>,
    buffer_budget: Arc<Semaphore>,
}

impl<S: SecretBlobSource> Proxy<S> {
    /// Build a proxy from `config`.
    ///
    /// # Errors
    /// [`ProxyError::Client`] when the HTTP client cannot be constructed.
    pub fn new(config: ProxyConfig<S>) -> Result<Self, ProxyError> {
        let client = reqwest::Client::builder()
            // Redirects are never followed: a 30x is an error, so no origin
            // change can carry the credential.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.limits.connect_timeout)
            .timeout(config.limits.request_timeout)
            .pool_max_idle_per_host(config.limits.max_connections)
            .build()
            .map_err(|e| ProxyError::Client(e.to_string()))?;
        Ok(Self {
            client,
            url: config.url,
            auth: config.auth,
            secret: config.secret,
            capability: config.capability,
            lookup: McpSecretLookup::new(config.secrets),
            header_value: OnceCell::new(),
            in_flight: Arc::new(Semaphore::new(config.limits.max_in_flight)),
            buffer_budget: Arc::new(Semaphore::new(config.limits.max_buffered_bytes)),
            limits: config.limits,
        })
    }

    /// The header value carrying the credential, resolved once on first use.
    async fn auth_header(&self) -> Result<Option<&str>, ProxyError> {
        let (Some(scheme), Some(reference), Some(capability)) =
            (self.auth, self.secret.as_ref(), self.capability.as_ref())
        else {
            return Ok(None);
        };
        let value = self
            .header_value
            .get_or_try_init(|| async {
                let secret = self
                    .lookup
                    .resolve(capability, reference)
                    .map_err(|e| ProxyError::Credential(e.to_string()))?;
                Ok::<String, ProxyError>(scheme.header_value(secret.expose()))
            })
            .await?;
        Ok(Some(value.as_str()))
    }

    /// Forward one JSON-RPC frame upstream and return the response frame.
    ///
    /// `Ok(None)` for a notification, which has no response.
    ///
    /// # Errors
    /// Every bound breach and every upstream failure is its own named variant;
    /// none is downgraded to an empty response.
    pub async fn forward(&self, frame: &[u8]) -> Result<Option<Vec<u8>>, ProxyError> {
        if frame.len() > self.limits.max_request_bytes {
            return Err(ProxyError::RequestTooLarge {
                len: frame.len(),
                cap: self.limits.max_request_bytes,
            });
        }
        let mut request = self
            .client
            .post(self.url.clone())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .body(frame.to_vec());
        if let Some(value) = self.auth_header().await? {
            if let Some(scheme) = self.auth {
                request = request.header(scheme.header_name(), value);
            }
        }

        let response = request
            .send()
            .await
            .map_err(|e| ProxyError::Upstream(e.to_string()))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(ProxyError::Redirect(status.as_u16()));
        }

        let body = self.read_bounded_body(response).await?;
        if !status.is_success() {
            return Err(ProxyError::Upstream(format!(
                "status {status}: {}",
                String::from_utf8_lossy(&body[..body.len().min(512)])
            )));
        }
        if body.is_empty() {
            return Ok(None);
        }
        Ok(Some(body))
    }

    /// Read a response body, refusing one past the cap without buffering it.
    async fn read_bounded_body(
        &self,
        mut response: reqwest::Response,
    ) -> Result<Vec<u8>, ProxyError> {
        let cap = self.limits.max_response_bytes;
        // `Content-Length` is only a hint, so the chunk loop below is what
        // actually enforces the cap: the buffer never grows past it.
        if let Some(len) = response.content_length() {
            if len > cap as u64 {
                return Err(ProxyError::ResponseTooLarge { cap });
            }
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| ProxyError::Upstream(e.to_string()))?
        {
            if body.len().saturating_add(chunk.len()) > cap {
                return Err(ProxyError::ResponseTooLarge { cap });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Drive the stdio transport until end of input.
    ///
    /// Reads pause once the aggregate buffer ceiling is reached rather than
    /// queueing behind it, because the permit for the next frame is acquired
    /// before the frame is read.
    ///
    /// # Errors
    /// Any transport failure, including a frame over the inbound bound. The
    /// transport is not resynchronized after one: a decoder that skipped an
    /// oversized frame would resume on attacker-chosen boundaries.
    pub async fn run<R, W>(self: Arc<Self>, mut input: R, output: W) -> Result<(), ProxyError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin + Send + 'static,
        S: Send + Sync + 'static,
    {
        let mut decoder = FrameDecoder::with_cap(self.limits.max_inbound_frame_bytes);
        let output = Arc::new(Mutex::new(output));
        let mut tasks = Vec::new();

        while let Some(frame) = decoder.next_frame(&mut input).await? {
            if frame.is_empty() {
                continue;
            }
            let reservation = u32::try_from(self.limits.reservation_for(frame.len()))
                .unwrap_or(u32::MAX)
                .min(u32::try_from(self.limits.max_buffered_bytes).unwrap_or(u32::MAX));
            // Both permits are taken before the work starts, so a full budget
            // stalls this read loop instead of growing an unbounded task queue.
            let Ok(budget) = self
                .buffer_budget
                .clone()
                .acquire_many_owned(reservation)
                .await
            else {
                break;
            };
            let Ok(slot) = self.in_flight.clone().acquire_owned().await else {
                break;
            };

            let proxy = Arc::clone(&self);
            let output = Arc::clone(&output);
            tasks.push(tokio::spawn(async move {
                let _budget = budget;
                let _slot = slot;
                let outcome = proxy.forward(&frame).await;
                let reply = match outcome {
                    Ok(Some(body)) => Some(body),
                    Ok(None) => None,
                    Err(error) => {
                        // A failed call is reported to the model as a JSON-RPC
                        // error when it had an id, and to the operator on
                        // stderr either way. It is never dropped silently.
                        tracing::error!(%error, "mcp proxy call failed");
                        json_rpc_error(&frame, &error.to_string())
                    }
                };
                if let Some(mut body) = reply {
                    body.push(b'\n');
                    let mut guard = output.lock().await;
                    if let Err(e) = guard.write_all(&body).await {
                        tracing::error!(error = %e, "mcp proxy could not write a response");
                        return;
                    }
                    if let Err(e) = guard.flush().await {
                        tracing::error!(error = %e, "mcp proxy could not flush a response");
                    }
                }
            }));
        }

        for task in tasks {
            if let Err(e) = task.await {
                tracing::error!(error = %e, "mcp proxy task did not complete");
            }
        }
        Ok(())
    }
}

/// Build a JSON-RPC error response for `frame`, when it carried an id.
fn json_rpc_error(frame: &[u8], message: &str) -> Option<Vec<u8>> {
    let parsed: serde_json::Value = serde_json::from_slice(frame).ok()?;
    let id = parsed.get("id")?.clone();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32000, "message": message },
    });
    serde_json::to_vec(&body).ok()
}
