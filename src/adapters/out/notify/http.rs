use crate::ports::notifier::{HttpSenderPort, OutboundRequest, OutboundResponse, SendFuture};

// How much of an answer is kept: a receiver's error message or a job id, not
// whatever page a misconfigured URL returns.
const KEPT_BODY_BYTES: usize = 4096;

// HttpSenderPort over reqwest (rustls), the client Vault already uses.
pub struct ReqwestSender {
    client: reqwest::Client,
}

impl ReqwestSender {
    pub fn new() -> Self {
        Self {
            // A redirect would resend the body, credentials included, to
            // wherever the receiver points: never followed.
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
        }
    }
}

impl Default for ReqwestSender {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpSenderPort for ReqwestSender {
    fn send(&self, request: OutboundRequest) -> SendFuture<'_> {
        Box::pin(async move {
            let method = reqwest::Method::from_bytes(request.method.as_bytes())
                .map_err(|e| format!("method {}: {e}", request.method))?;
            let mut builder = self
                .client
                .request(method, &request.url)
                .timeout(request.timeout)
                .body(request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            // The error's text, never the request: its headers carry the credential.
            let response = builder.send().await.map_err(|e| without_url(&e))?;
            let status = response.status().as_u16();
            let bytes = response.bytes().await.unwrap_or_default();
            let kept = &bytes[..bytes.len().min(KEPT_BODY_BYTES)];
            Ok(OutboundResponse {
                status,
                body: String::from_utf8_lossy(kept).into_owned(),
            })
        })
    }
}

// reqwest's error, without the URL it carries: a webhook URL can hold its
// secret in its path or query.
fn without_url(error: &reqwest::Error) -> String {
    let kind = if error.is_timeout() {
        "timed out"
    } else if error.is_connect() {
        "could not connect"
    } else {
        "request failed"
    };
    let mut cause = String::new();
    let mut source = std::error::Error::source(error);
    while let Some(inner) = source {
        cause = inner.to_string();
        source = inner.source();
    }
    if cause.is_empty() {
        kind.to_string()
    } else {
        format!("{kind}: {cause}")
    }
}
