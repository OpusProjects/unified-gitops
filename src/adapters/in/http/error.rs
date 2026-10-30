// One error shape for the whole API: web-rs's ApiError and ErrorBody (the
// `web` repository), shared with unified-api. Every failure is a status and
// {"error": ...}, never a bare StatusCode with an empty body.
pub use web_rs::{ApiError, ErrorBody};
