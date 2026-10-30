use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

// One HTTP request, built whole by application::http: the adapter only
// carries it, so what an item sends is decided (and tested) in one place,
// without a network.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutboundRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
    pub transport: Transport,
}

// How a request travels, as its item configures it. The adapter keeps one
// client per distinct Transport.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Transport {
    pub follow_redirects: bool,
    pub proxy: Proxy,
    // PEM: the CAs to trust instead of the system's.
    pub ca_pem: Option<Vec<u8>>,
    // PEM: a client certificate and its key, for mutual TLS.
    pub identity_pem: Option<Vec<u8>>,
    pub insecure_skip_verify: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Proxy {
    // HTTPS_PROXY, HTTP_PROXY and NO_PROXY, as the environment sets them.
    #[default]
    Environment,
    None,
    Url(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutboundResponse {
    pub status: u16,
    // Enough to read an error, a job's URL or a login's token, not whatever
    // page a misconfigured URL returns.
    pub body: String,
    // Names in lower case.
    pub headers: Vec<(String, String)>,
}

impl OutboundResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

// A request that got no answer. `retryable` is false when it could not even
// be built (a malformed URL): sending it again would fail the same way.
#[derive(Debug, Clone, PartialEq)]
pub struct SendError {
    pub message: String,
    pub retryable: bool,
}

pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OutboundResponse, SendError>> + Send + 'a>>;

// Sends one request. Err is a request that got no answer (refused, timed
// out, TLS, or not buildable); any answer, error statuses included, is Ok,
// and the caller decides what it means for its receiver.
pub trait HttpSenderPort: Send + Sync {
    fn send(&self, request: OutboundRequest) -> SendFuture<'_>;
}
