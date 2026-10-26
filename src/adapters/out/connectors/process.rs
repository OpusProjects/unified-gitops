use std::time::Duration;

use serde_json::Value;

use crate::adapters::out::process_env;
use crate::domain::connector::ConnectorDescription;
use crate::ports::connector::{ConnectorCall, ConnectorError, ConnectorFuture, ConnectorPort};

// A connector is a script, run once per subcommand. Same rules as unified-api's
// connectors, which is the point: one way to run a script in the family.
// - The environment is scrubbed (process_env): the script sees the
//   passthrough list plus what is injected here, never the service's API-key
//   secrets or another application's credentials.
// - APPLICATION_CONFIG carries the application's config as JSON, CREDENTIAL_<FIELD> each
//   resolved credential field.
// - UNIFIED_ALLOW_WRITE=1 only on `apply`. A shared script that reads for
//   unified-api and writes for gitops refuses to write without it — and
//   unified-api, whose environment is scrubbed too, never sets it.
// - The timeout KILLS the script (kill_on_drop), it does not abandon it.
pub struct ProcessConnector;

impl Default for ProcessConnector {
    fn default() -> Self {
        Self
    }
}

impl ProcessConnector {
    pub fn new() -> Self {
        Self
    }
}

// Stderr is shown in run errors; a script that dumps a whole traceback should
// not turn one run record into a megabyte.
const STDERR_LIMIT: usize = 2000;

async fn run(
    call: &ConnectorCall,
    args: &[&str],
    stdin: Option<Vec<u8>>,
    write: bool,
) -> Result<Vec<u8>, ConnectorError> {
    let config = serde_json::to_string(&call.config).map_err(|e| ConnectorError {
        message: format!("could not serialize the application config: {e}"),
        stderr: String::new(),
    })?;

    let mut cmd = process_env::scrubbed_command(&call.script_path);
    if let Some(venv_bin) = &call.venv_bin {
        process_env::apply_venv(&mut cmd, venv_bin);
    }
    cmd.kill_on_drop(true);
    cmd.args(args);
    cmd.env("APPLICATION_CONFIG", config);
    if write {
        cmd.env("UNIFIED_ALLOW_WRITE", "1");
    }
    for (key, value) in &call.credentials {
        cmd.env(format!("CREDENTIAL_{}", key.to_uppercase()), value);
    }
    cmd.stdin(if stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let what = format!("{} {}", call.script_path, args.join(" "));
    let fut = async {
        let mut child = cmd.spawn().map_err(|e| ConnectorError {
            message: format!("could not run {what}: {e}"),
            stderr: String::new(),
        })?;
        if let Some(input) = stdin
            && let Some(mut pipe) = child.stdin.take()
        {
            use tokio::io::AsyncWriteExt;
            // A script that exits without reading stdin closes the pipe; that
            // is its own business, and its exit status says how it went.
            let _ = pipe.write_all(&input).await;
            drop(pipe);
        }
        child.wait_with_output().await.map_err(|e| ConnectorError {
            message: format!("{what} could not be waited on: {e}"),
            stderr: String::new(),
        })
    };

    let output = tokio::time::timeout(Duration::from_secs(call.timeout_seconds), fut)
        .await
        .map_err(|_| ConnectorError {
            message: format!(
                "{what} did not finish within {}s and was killed",
                call.timeout_seconds
            ),
            stderr: String::new(),
        })??;

    let mut stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.len() > STDERR_LIMIT {
        let mut cut = STDERR_LIMIT;
        while !stderr.is_char_boundary(cut) {
            cut -= 1;
        }
        stderr.truncate(cut);
        stderr.push('…');
    }
    if !output.status.success() {
        return Err(ConnectorError {
            message: format!("{what} failed with exit code {:?}", output.status.code()),
            stderr,
        });
    }
    Ok(output.stdout)
}

fn parse<T: serde::de::DeserializeOwned>(bytes: &[u8], what: &str) -> Result<T, ConnectorError> {
    serde_json::from_slice(bytes).map_err(|e| ConnectorError {
        message: format!("{what} did not print the JSON the contract asks for: {e}"),
        stderr: String::new(),
    })
}

impl ConnectorPort for ProcessConnector {
    fn describe<'a>(
        &'a self,
        call: &'a ConnectorCall,
    ) -> ConnectorFuture<'a, ConnectorDescription> {
        Box::pin(async move {
            let out = run(call, &["describe"], None, false).await?;
            parse(&out, "describe")
        })
    }

    fn list<'a>(
        &'a self,
        call: &'a ConnectorCall,
        kind: &'a str,
    ) -> ConnectorFuture<'a, Vec<Value>> {
        Box::pin(async move {
            let out = run(call, &["list", "--kind", kind], None, false).await?;
            parse(&out, &format!("list --kind {kind}"))
        })
    }

    fn apply<'a>(
        &'a self,
        call: &'a ConnectorCall,
        kind: &'a str,
        operation: &'a Value,
    ) -> ConnectorFuture<'a, ()> {
        Box::pin(async move {
            let input = serde_json::to_vec(operation).map_err(|e| ConnectorError {
                message: format!("could not serialize the operation: {e}"),
                stderr: String::new(),
            })?;
            run(call, &["apply", "--kind", kind], Some(input), true).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn call(script: &str, timeout: u64) -> ConnectorCall {
        let mut credentials = HashMap::new();
        credentials.insert("password".to_string(), "s3cret".to_string());
        ConnectorCall {
            script_path: script.to_string(),
            timeout_seconds: timeout,
            config: serde_json::Map::new(),
            credentials,
            venv_bin: None,
        }
    }

    // The probe prints, as JSON, the environment variables it was given that
    // matter to the contract — exactly what a real connector would see.
    #[tokio::test]
    async fn only_apply_may_write_and_secrets_stay_scoped() {
        unsafe { std::env::set_var("UNIFIED_GITOPS_TEST_PARENT_SECRET", "leaked") };
        let connector = ProcessConnector::new();
        let c = call("tests/connectors/env_probe.py", 10);

        let listed = connector.list(&c, "probe").await.unwrap();
        let env = listed[0].as_object().unwrap();
        assert_eq!(env["CREDENTIAL_PASSWORD"], "s3cret");
        assert_eq!(env["UNIFIED_ALLOW_WRITE"], Value::Null);
        assert_eq!(env["UNIFIED_GITOPS_TEST_PARENT_SECRET"], Value::Null);

        // apply succeeds only because the probe found the write flag set.
        connector
            .apply(
                &c,
                "probe",
                &serde_json::json!({"op": "create", "object": {}}),
            )
            .await
            .expect("apply should see UNIFIED_ALLOW_WRITE=1");
    }

    #[tokio::test]
    async fn a_hung_connector_is_killed() {
        let started = std::time::Instant::now();
        let err = ProcessConnector::new()
            .list(&call("tests/connectors/hang.py", 1), "x")
            .await
            .unwrap_err();
        assert!(err.message.contains("killed"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
