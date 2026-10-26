use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use serde_json::Value;

use crate::domain::connector::ConnectorDescription;

// What a connector run needs besides its subcommand: where the script is, how
// long it may take, the application's config and credentials, and the project's
// virtualenv when there is one.
#[derive(Debug, Clone)]
pub struct ConnectorCall {
    pub script_path: String,
    pub timeout_seconds: u64,
    pub config: serde_json::Map<String, Value>,
    pub credentials: HashMap<String, String>,
    pub venv_bin: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ConnectorError {
    pub message: String,
    // The script's stderr, trimmed: what it said about why. Shown in the
    // run's errors, never the credentials (they are only in the env).
    pub stderr: String,
}

impl std::fmt::Display for ConnectorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.stderr.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(f, "{}: {}", self.message, self.stderr)
        }
    }
}

pub type ConnectorFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, ConnectorError>> + Send + 'a>>;

// A connector, as the core sees it. The process adapter runs a script; a test
// double keeps objects in memory. The core never knows which.
pub trait ConnectorPort: Send + Sync {
    fn describe<'a>(&'a self, call: &'a ConnectorCall)
    -> ConnectorFuture<'a, ConnectorDescription>;

    // The objects of one kind that exist on the system now.
    fn list<'a>(
        &'a self,
        call: &'a ConnectorCall,
        kind: &'a str,
    ) -> ConnectorFuture<'a, Vec<Value>>;

    // Carries out one operation: {"op": "create|update|delete", "object": {...}}.
    fn apply<'a>(
        &'a self,
        call: &'a ConnectorCall,
        kind: &'a str,
        operation: &'a Value,
    ) -> ConnectorFuture<'a, ()>;
}
