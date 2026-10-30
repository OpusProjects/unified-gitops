use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::domain::api::{FILE_FIELDS, OnFailure, Per};
use crate::domain::http::Retry;
use crate::domain::template::{self, Allowed};
use crate::domain::trigger::Trigger;
use crate::domain::webhook::EVENT_FIELDS;

// A script item: when the watched project changes, run a file from a
// repository, with what changed in its environment.
//
//   scripts:
//     - name: reload-dns
//       project: infra              # watched: its changes run it
//       trigger: {paths: [dns/**]}
//       credential: cred-dns
//       script:
//         project: ops-scripts      # where the file lives; default: infra
//         file: reload-dns.sh
//         args: [--zone, example.com]
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub project: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub trigger: Trigger,
    // Its fields reach the script as CREDENTIAL_<FIELD> variables.
    #[serde(default)]
    pub credential: Option<String>,
    // One execution; past it the script is told to stop (SIGTERM), then
    // killed.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    // Executions in all for one file (or commit), the first included. A
    // script is run once unless it asks for more: running it again is only
    // safe when it is written to be.
    #[serde(default = "run_once")]
    pub retry: Retry,
    #[serde(default)]
    pub on_failure: OnFailure,
    pub script: Command,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    // The project holding the file; left out, the watched one.
    #[serde(default)]
    pub project: Option<String>,
    // Relative to that project's checkout.
    pub file: String,
    // Templates over $event (and $file, run per file). Passed as they are:
    // no shell reads them, so nothing needs quoting.
    #[serde(default)]
    pub args: Vec<String>,
    // A program the file is given to (bash, python3), for a file committed
    // without its executable bit or its shebang. Left out, the file runs
    // itself.
    #[serde(default)]
    pub interpreter: Option<String>,
    // Where it runs, relative to the WATCHED checkout; left out, its root.
    #[serde(default)]
    pub working_dir: Option<String>,
    // More variables, templated. Not secret: a secret belongs in the
    // credential.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "per_commit")]
    pub per: Per,
    // What the script reads on stdin: nothing, or the event as JSON.
    #[serde(default)]
    pub stdin: Stdin,
    #[serde(default = "default_success_codes")]
    pub success_exit_codes: Vec<i32>,
    // Between the SIGTERM a timeout sends and the SIGKILL after it.
    #[serde(default = "default_kill_grace")]
    pub kill_grace_seconds: u64,
    // Output kept per execution, from its end: where the error usually is.
    #[serde(default = "default_max_output")]
    pub max_output_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Stdin {
    #[default]
    None,
    Event,
}

fn yes() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    300
}

fn run_once() -> Retry {
    Retry {
        attempts: 1,
        ..Retry::default()
    }
}

fn per_commit() -> Per {
    Per::Commit
}

fn default_success_codes() -> Vec<i32> {
    vec![0]
}

fn default_kill_grace() -> u64 {
    10
}

fn default_max_output() -> usize {
    64 * 1024
}

const MAX_TIMEOUT_SECONDS: u64 = 24 * 3600;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

// The variables the service sets itself; `env` may not take them.
pub const RESERVED_PREFIXES: [&str; 2] = ["GITOPS_", "CREDENTIAL_"];

impl Command {
    // The project the file is in.
    pub fn project_of<'a>(&'a self, watched: &'a str) -> &'a str {
        self.project.as_deref().unwrap_or(watched)
    }
}

// A path inside a checkout: relative, never climbing out.
pub fn path_error(path: &str) -> Option<&'static str> {
    if path.trim().is_empty() {
        Some("is empty")
    } else if path.starts_with('/') || path.starts_with('\\') {
        Some("must be relative to the checkout")
    } else if path.split(['/', '\\']).any(|s| s == "..") {
        Some("must stay inside the checkout ('..' is not allowed)")
    } else {
        None
    }
}

fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl Script {
    pub fn errors(&self, id: &str, parse_cron: impl Fn(&str) -> Result<(), String>) -> Vec<String> {
        let mut errors = self.trigger.errors(parse_cron);
        let command = &self.script;
        if let Some(why) = path_error(&command.file) {
            errors.push(format!("script.file '{}' {why}", command.file));
        }
        if let Some(dir) = &command.working_dir
            && let Some(why) = path_error(dir)
        {
            errors.push(format!("script.working_dir '{dir}' {why}"));
        }
        if let Some(interpreter) = &command.interpreter
            && (interpreter.trim().is_empty() || interpreter.contains(char::is_whitespace))
        {
            errors.push(format!(
                "script.interpreter '{interpreter}' must be one program (bash, python3, \
                 /usr/bin/pwsh); its arguments belong in args"
            ));
        }
        let file_fields: &[(&str, &[&str])] = &[("event", &EVENT_FIELDS), ("file", &FILE_FIELDS)];
        let allowed = match command.per {
            Per::File => Allowed {
                roots: &["event", "file"],
                fields: file_fields,
            },
            Per::Commit => Allowed {
                roots: &["event"],
                fields: file_fields,
            },
        };
        for (i, arg) in command.args.iter().enumerate() {
            errors.extend(template::string_errors(
                &format!("script.args[{i}]"),
                arg,
                &allowed,
            ));
        }
        for (name, value) in &command.env {
            if !is_env_name(name) {
                errors.push(format!(
                    "script.env '{name}' is not a variable name (letters, digits, '_')"
                ));
            }
            if RESERVED_PREFIXES.iter().any(|p| name.starts_with(p)) {
                errors.push(format!(
                    "script.env '{name}': GITOPS_* and CREDENTIAL_* are set by the service"
                ));
            }
            errors.extend(template::string_errors(
                &format!("script.env.{name}"),
                value,
                &allowed,
            ));
        }
        if command.success_exit_codes.is_empty() {
            errors.push("script.success_exit_codes names no code".to_string());
        }
        if command.max_output_bytes == 0 || command.max_output_bytes > MAX_OUTPUT_BYTES {
            errors.push(format!(
                "script.max_output_bytes must be between 1 and {MAX_OUTPUT_BYTES}"
            ));
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > MAX_TIMEOUT_SECONDS {
            errors.push(format!(
                "timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}"
            ));
        }
        errors.extend(crate::domain::http::retry_errors(&self.retry));
        errors
            .into_iter()
            .map(|e| format!("Script '{id}': {e}"))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Script {
        serde_saphyr::from_str(yaml).unwrap()
    }

    fn errors(yaml: &str) -> String {
        parse(yaml).errors("s", |_| Ok(())).join("\n")
    }

    #[test]
    fn a_script_needs_a_project_and_a_file() {
        let s = parse("project: infra\nscript:\n  file: reload.sh\n");
        assert_eq!(errors("project: infra\nscript:\n  file: reload.sh\n"), "");
        assert_eq!(s.script.project_of(&s.project), "infra");
        assert_eq!(s.script.per, Per::Commit);
        assert_eq!(s.retry.attempts, 1, "a script runs once unless it asks");
        assert_eq!(s.script.success_exit_codes, [0]);
    }

    #[test]
    fn every_option_parses() {
        let yaml = "description: DNS\nenabled: false\nproject: infra\nlabels: {team: net}\ntrigger: {paths: [dns/**], on: [change, manual]}\ncredential: cred-dns\ntimeout_seconds: 60\nretry: {attempts: 3, backoff: fixed, initial_delay_seconds: 5}\non_failure: stop\nscript:\n  project: ops-scripts\n  file: bin/reload.py\n  args: [--zone, $file.stem, --commit, $event.revision_short]\n  interpreter: python3\n  working_dir: dns\n  env: {ZONE_DIR: $file.dir, MODE: apply}\n  per: file\n  stdin: event\n  success_exit_codes: [0, 3]\n  kill_grace_seconds: 2\n  max_output_bytes: 4096\n";
        assert_eq!(errors(yaml), "");
        assert_eq!(parse(yaml).script.project_of("infra"), "ops-scripts");
    }

    #[test]
    fn what_cannot_work_is_named() {
        let e = errors(
            "project: p\nscript:\n  file: ../escape.sh\n  working_dir: /etc\n  interpreter: bash -x\n  args: [$file.path]\n  env: {GITOPS_X: a, 9BAD: b}\n  success_exit_codes: []\n  max_output_bytes: 0\n",
        );
        for expected in [
            "script.file '../escape.sh' must stay inside",
            "script.working_dir '/etc' must be relative",
            "script.interpreter 'bash -x' must be one program",
            "script.args[0] names $file.path",
            "GITOPS_* and CREDENTIAL_* are set by the service",
            "'9BAD' is not a variable name",
            "success_exit_codes names no code",
            "max_output_bytes must be",
        ] {
            assert!(e.contains(expected), "missing {expected:?} in {e}");
        }
        assert!(serde_saphyr::from_str::<Script>("project: p\nscript:\n  path: x\n").is_err());
        assert!(
            errors("project: p\ntimeout_seconds: 0\nscript:\n  file: x\n")
                .contains("timeout_seconds")
        );
    }
}
