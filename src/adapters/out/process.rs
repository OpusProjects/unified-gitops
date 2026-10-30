use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::adapters::out::process_env::{apply_venv, scrubbed_command, spawn};
use crate::ports::process::{ExecFuture, Execution, Exited, ProcessPort};

// ProcessPort over tokio's processes. A script runs in its own process
// group, so a timeout stops what it started too: SIGTERM to the group, then
// SIGKILL after the grace.
#[derive(Default)]
pub struct LocalProcess;

impl LocalProcess {
    pub fn new() -> Self {
        Self
    }
}

impl ProcessPort for LocalProcess {
    fn run(&self, execution: Execution) -> ExecFuture<'_> {
        Box::pin(run(execution))
    }
}

async fn run(execution: Execution) -> Result<Exited, String> {
    let mut cmd = scrubbed_command(&execution.program);
    cmd.args(&execution.args)
        .current_dir(&execution.working_dir)
        .envs(execution.env.iter().map(|(k, v)| (k, v)))
        .stdin(if execution.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    if let Some(bin) = &execution.venv_bin {
        apply_venv(&mut cmd, bin);
    }
    let mut child = spawn(&mut cmd)
        .await
        .map_err(|e| format!("could not start {}: {e}", execution.program))?;
    let pid = child.id();

    if let (Some(input), Some(mut stdin)) = (execution.stdin, child.stdin.take()) {
        // A script that never reads its stdin must not block the run: the
        // write gives up when the pipe is closed.
        tokio::spawn(async move {
            let _ = stdin.write_all(&input).await;
        });
    }
    let output = Arc::new(Mutex::new(Tail::new(execution.max_output_bytes)));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(tokio::spawn(collect(out, Arc::clone(&output))));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(tokio::spawn(collect(err, Arc::clone(&output))));
    }

    let (status, timed_out) = match tokio::time::timeout(execution.timeout, child.wait()).await {
        Ok(status) => (status.map_err(|e| e.to_string())?, false),
        Err(_) => {
            signal(pid, libc::SIGTERM);
            let status = match tokio::time::timeout(execution.kill_grace, child.wait()).await {
                Ok(status) => status,
                Err(_) => {
                    signal(pid, libc::SIGKILL);
                    child.wait().await
                }
            };
            (status.map_err(|e| e.to_string())?, true)
        }
    };
    // What the script's own children still hold open is not waited for.
    for reader in readers {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), reader).await;
    }
    let output = output.lock().map(|t| t.text()).unwrap_or_default();
    Ok(Exited {
        exit_code: status.code(),
        output,
        timed_out,
    })
}

// To the whole process group the script leads.
fn signal(pid: Option<u32>, signal: libc::c_int) {
    if let Some(pid) = pid {
        // SAFETY: kill(2) with a negative pid signals that process group;
        // it reads no memory of ours.
        unsafe {
            libc::kill(-(pid as libc::pid_t), signal);
        }
    }
}

async fn collect(mut from: impl AsyncRead + Unpin, into: Arc<Mutex<Tail>>) {
    let mut buffer = [0u8; 8192];
    loop {
        match from.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if let Ok(mut tail) = into.lock() {
                    tail.push(&buffer[..n]);
                }
            }
        }
    }
}

// The last `max` bytes written, and whether anything was dropped before them.
struct Tail {
    bytes: VecDeque<u8>,
    max: usize,
    dropped: bool,
}

impl Tail {
    fn new(max: usize) -> Self {
        Self {
            bytes: VecDeque::new(),
            max,
            dropped: false,
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        if self.bytes.len() > self.max {
            let excess = self.bytes.len() - self.max;
            self.bytes.drain(..excess);
            self.dropped = true;
        }
    }

    fn text(&self) -> String {
        let (a, b) = self.bytes.as_slices();
        let text = String::from_utf8_lossy(&[a, b].concat()).into_owned();
        if self.dropped {
            format!("[…]{text}")
        } else {
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn sh(script: &str) -> Execution {
        Execution {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            working_dir: std::env::temp_dir(),
            timeout: Duration::from_secs(10),
            kill_grace: Duration::from_secs(1),
            max_output_bytes: 1024,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn output_exit_code_and_environment_come_back() {
        let mut e = sh("echo out; echo err >&2; echo \"$GITOPS_ITEM\"; cat; exit 3");
        e.env = vec![("GITOPS_ITEM".into(), "reload".into())];
        e.stdin = Some(b"from stdin".to_vec());
        let done = LocalProcess::new().run(e).await.unwrap();
        assert_eq!(done.exit_code, Some(3));
        assert!(!done.timed_out);
        for line in ["out", "err", "reload", "from stdin"] {
            assert!(done.output.contains(line), "{line} in {:?}", done.output);
        }
    }

    #[tokio::test]
    async fn a_long_output_keeps_its_end() {
        let mut e = sh("i=0; while [ $i -lt 500 ]; do echo line$i; i=$((i+1)); done");
        e.max_output_bytes = 32;
        let done = LocalProcess::new().run(e).await.unwrap();
        assert!(done.output.starts_with("[…]"), "{:?}", done.output);
        assert!(done.output.ends_with("line499\n"), "{:?}", done.output);
    }

    #[tokio::test]
    async fn an_overrun_is_stopped_with_what_it_started() {
        let mut e = sh("trap '' TERM; sleep 30 & wait");
        e.timeout = Duration::from_millis(200);
        e.kill_grace = Duration::from_millis(200);
        let started = Instant::now();
        let done = LocalProcess::new().run(e).await.unwrap();
        assert!(done.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(done.exit_code, None, "ended by a signal");

        let mut polite = sh("sleep 30");
        polite.timeout = Duration::from_millis(100);
        let done = LocalProcess::new().run(polite).await.unwrap();
        assert!(done.timed_out);
    }

    #[tokio::test]
    async fn a_missing_program_is_an_error_and_a_venv_leads_path() {
        let e = Execution {
            program: "/nonexistent/script".into(),
            working_dir: std::env::temp_dir(),
            ..sh("")
        };
        let err = LocalProcess::new().run(e).await.unwrap_err();
        assert!(err.contains("could not start /nonexistent/script"), "{err}");
        let mut e = sh("echo $PATH; echo $VIRTUAL_ENV");
        e.venv_bin = Some("/venvs/prj/bin".into());
        let done = LocalProcess::new().run(e).await.unwrap();
        assert!(
            done.output.starts_with("/venvs/prj/bin:"),
            "{}",
            done.output
        );
        assert!(done.output.contains("/venvs/prj\n"), "{}", done.output);
    }
}
