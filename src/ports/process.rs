use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

// One execution of a script, built whole by application::actions: the
// adapter only runs it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Execution {
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    // Added to a scrubbed environment: the child sees these and the few host
    // variables a script legitimately needs, never the service's own.
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
    // Between SIGTERM and SIGKILL when the timeout passes.
    pub kill_grace: Duration,
    // Output kept, from its end.
    pub max_output_bytes: usize,
    // A project virtualenv's bin directory, put first on PATH.
    pub venv_bin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Exited {
    // None when a signal ended it.
    pub exit_code: Option<i32>,
    // stdout and stderr as they came, the end kept when too long.
    pub output: String,
    pub timed_out: bool,
}

pub type ExecFuture<'a> = Pin<Box<dyn Future<Output = Result<Exited, String>> + Send + 'a>>;

// Runs scripts. Err: it could not start (no such file, not executable);
// anything it did once started, a failure included, is Ok.
pub trait ProcessPort: Send + Sync {
    fn run(&self, execution: Execution) -> ExecFuture<'_>;
}
