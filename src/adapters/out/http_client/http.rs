use dashmap::DashMap;

use crate::ports::http_sender::{
    HttpSenderPort, OutboundRequest, OutboundResponse, Proxy, SendError, SendFuture, Transport,
};

// How much of an answer is kept: a receiver's error message, a job's URL, a
// login's token; not whatever page a misconfigured URL returns.
const KEPT_BODY_BYTES: usize = 64 * 1024;

// HttpSenderPort over reqwest (rustls), the client Vault already uses. One
// client per Transport, built on first use and kept: a client holds its
// connection pool, so building one per request would reconnect every time.
pub struct ReqwestSender {
    clients: DashMap<Transport, reqwest::Client>,
}

impl ReqwestSender {
    pub fn new() -> Self {
        Self {
            clients: DashMap::new(),
        }
    }

    fn client(&self, transport: &Transport) -> Result<reqwest::Client, SendError> {
        if let Some(client) = self.clients.get(transport) {
            return Ok(client.clone());
        }
        let refused = |what: &str, e: reqwest::Error| SendError {
            message: format!("{what}: {}", without_url(&e)),
            retryable: false,
        };
        let mut builder = reqwest::Client::builder().redirect(if transport.follow_redirects {
            reqwest::redirect::Policy::limited(10)
        } else {
            reqwest::redirect::Policy::none()
        });
        builder = match &transport.proxy {
            Proxy::Environment => builder,
            Proxy::None => builder.no_proxy(),
            Proxy::Url(url) => {
                builder.proxy(reqwest::Proxy::all(url.as_str()).map_err(|e| refused("proxy", e))?)
            }
        };
        if let Some(pem) = &transport.ca_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| refused("tls.ca_file", e))?;
            // Trusting an empty list would refuse every server, and say only
            // that the certificate was unknown.
            if certificates.is_empty() {
                return Err(SendError {
                    message: "tls.ca_file holds no PEM certificate".to_string(),
                    retryable: false,
                });
            }
            builder = builder.tls_built_in_root_certs(false);
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        if let Some(pem) = &transport.identity_pem {
            builder = builder.identity(
                reqwest::Identity::from_pem(pem)
                    .map_err(|e| refused("tls.client_credential", e))?,
            );
        }
        if transport.insecure_skip_verify {
            builder = builder.danger_accept_invalid_certs(true);
        }
        let client = builder.build().map_err(|e| refused("client", e))?;
        self.clients.insert(transport.clone(), client.clone());
        Ok(client)
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
            let method =
                reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| SendError {
                    message: format!("method {}: {e}", request.method),
                    retryable: false,
                })?;
            let mut builder = self
                .client(&request.transport)?
                .request(method, &request.url)
                .timeout(request.timeout)
                .body(request.body);
            for (name, value) in &request.headers {
                builder = builder.header(name, value);
            }
            // The error's text, never the request: its headers carry the credential.
            let response = builder.send().await.map_err(|e| SendError {
                message: without_url(&e),
                // Not buildable (a malformed URL or header): the same next time.
                retryable: !e.is_builder(),
            })?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
                })
                .collect();
            let bytes = response.bytes().await.unwrap_or_default();
            let kept = &bytes[..bytes.len().min(KEPT_BODY_BYTES)];
            Ok(OutboundResponse {
                status,
                body: String::from_utf8_lossy(kept).into_owned(),
                headers,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // A local server: /go redirects to /here, which answers with a header.
    async fn server() -> String {
        let app = axum::Router::new()
            .route(
                "/go",
                axum::routing::get(|| async { axum::response::Redirect::temporary("/here") }),
            )
            .route(
                "/here",
                axum::routing::get(|| async { ([("x-answer", "yes")], "arrived") }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn get(url: String, transport: Transport) -> OutboundRequest {
        OutboundRequest {
            method: "GET".into(),
            url,
            timeout: Duration::from_secs(5),
            transport,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn redirects_are_followed_only_when_asked() {
        let base = server().await;
        let sender = ReqwestSender::new();
        let stay = Transport {
            proxy: Proxy::None,
            ..Default::default()
        };
        let answer = sender.send(get(format!("{base}/go"), stay)).await.unwrap();
        assert_eq!(answer.status, 307);
        let follow = Transport {
            follow_redirects: true,
            proxy: Proxy::None,
            ..Default::default()
        };
        let answer = sender
            .send(get(format!("{base}/go"), follow.clone()))
            .await
            .unwrap();
        assert_eq!((answer.status, answer.body.as_str()), (200, "arrived"));
        assert_eq!(answer.header("X-Answer"), Some("yes"));
        // The client is kept for the next request with the same transport.
        sender
            .send(get(format!("{base}/here"), follow))
            .await
            .unwrap();
        assert_eq!(sender.clients.len(), 2);
    }

    // A port nothing listens on: refused at once.
    async fn closed() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn a_transport_that_cannot_be_built_is_not_retried() {
        let sender = ReqwestSender::new();
        let nowhere = closed().await;
        for transport in [
            Transport {
                ca_pem: Some(b"not a certificate".to_vec()),
                ..Default::default()
            },
            Transport {
                identity_pem: Some(b"not a key".to_vec()),
                ..Default::default()
            },
            Transport {
                proxy: Proxy::Url("::not a url::".into()),
                ..Default::default()
            },
        ] {
            let e = sender
                .send(get(nowhere.clone(), transport))
                .await
                .unwrap_err();
            assert!(!e.retryable, "{e:?}");
        }
        let lab = Transport {
            insecure_skip_verify: true,
            proxy: Proxy::Url(nowhere.trim_end_matches('/').to_string()),
            ..Default::default()
        };
        let e = sender.send(get(nowhere.clone(), lab)).await.unwrap_err();
        assert!(e.retryable, "{e:?}");
        let e = sender
            .send(OutboundRequest {
                method: "NOT A METHOD".into(),
                ..get(nowhere.clone(), Transport::default())
            })
            .await
            .unwrap_err();
        assert!(!e.retryable);
    }
}
