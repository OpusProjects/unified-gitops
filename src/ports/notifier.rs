use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

// One HTTP request, built whole by application::notify: the adapter only
// carries it, so what a notification sends is decided (and tested) in one
// place, without a network.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboundResponse {
    pub status: u16,
    // At most a few KB: enough to read an error or a job id.
    pub body: String,
}

pub type SendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<OutboundResponse, String>> + Send + 'a>>;

// Sends notifications. Err is a request that got no answer (refused, timed
// out, TLS); any answer, error statuses included, is Ok, and the caller
// decides what it means for its receiver.
pub trait HttpSenderPort: Send + Sync {
    fn send(&self, request: OutboundRequest) -> SendFuture<'_>;
}
