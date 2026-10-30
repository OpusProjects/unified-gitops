use std::collections::VecDeque;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::trigger::On;

// What acted: an API item or a script item.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Api,
    Script,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Api => "api",
            Kind::Script => "script",
        }
    }

    // The configuration section and the API route: `apis`, `scripts`.
    pub fn section(self) -> &'static str {
        match self {
            Kind::Api => "apis",
            Kind::Script => "scripts",
        }
    }
}

// One run of an item: why, at which commit, how each step went.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Run {
    pub item: String,
    pub kind: Kind,
    pub project: String,
    pub trigger: On,
    /// The API key's name or the signed-in user, for a manual run
    pub by: Option<String>,
    /// The commit the checkout was at when the run started
    pub revision: Option<String>,
    /// A script's: the commit of the project its file is in
    pub script_revision: Option<String>,
    /// A dry run: what would be sent, with nothing sent
    pub dry_run: bool,
    /// Unix seconds
    pub started_at: u64,
    pub duration_ms: u64,
    pub ok: bool,
    /// Why the run could not start or stopped, beyond its steps' errors
    pub error: Option<String>,
    pub steps: Vec<Step>,
}

// One request an API run made, or one execution of a script.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct Step {
    /// The file it was about, for a run per file
    pub file: Option<String>,
    /// What it did: send, create, update, delete, skip (nothing to change),
    /// or for a script, run
    pub action: String,
    pub method: Option<String>,
    /// Scheme, host and path: never the query, which can hold a credential
    pub url: Option<String>,
    pub status: Option<u16>,
    /// A script's exit code
    pub exit_code: Option<i32>,
    pub ok: bool,
    pub attempts: u32,
    /// With compare: the fields that differed, by name, never their values
    pub changed_fields: Vec<String>,
    pub error: Option<String>,
    /// A script's output, its end kept when it is long
    pub output: Option<String>,
    /// A dry run's body, as it would be sent
    pub body: Option<Value>,
    pub duration_ms: u64,
}

// How many runs each item keeps. In memory: a restart starts it over.
pub const RUN_HISTORY: usize = 50;

// Every item's recent runs, newest first, and which items are failing (for
// the recovered events).
#[derive(Default)]
pub struct RunLog {
    runs: DashMap<String, VecDeque<Run>>,
    failing: DashMap<String, ()>,
}

impl RunLog {
    fn key(kind: Kind, item: &str) -> String {
        format!("{}/{item}", kind.as_str())
    }

    pub fn last(&self, kind: Kind, item: &str) -> Option<Run> {
        self.runs
            .get(&Self::key(kind, item))
            .and_then(|h| h.front().cloned())
    }

    pub fn history(&self, kind: Kind, item: &str) -> Vec<Run> {
        self.runs
            .get(&Self::key(kind, item))
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    // Records a run and says how it changes the item's standing: whether it
    // was failing before this one. A dry run changes nothing.
    pub fn record(&self, run: Run) -> Standing {
        let key = Self::key(run.kind, &run.item);
        let standing = if run.dry_run {
            Standing::Unchanged
        } else if run.ok {
            if self.failing.remove(&key).is_some() {
                Standing::Recovered
            } else {
                Standing::Unchanged
            }
        } else {
            self.failing.insert(key.clone(), ());
            Standing::Unchanged
        };
        let mut history = self.runs.entry(key).or_default();
        history.push_front(run);
        history.truncate(RUN_HISTORY);
        standing
    }

    pub fn is_failing(&self, kind: Kind, item: &str) -> bool {
        self.failing.contains_key(&Self::key(kind, item))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Unchanged,
    // The first run that worked after one or more that failed.
    Recovered,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(ok: bool, dry_run: bool) -> Run {
        Run {
            item: "a".into(),
            kind: Kind::Api,
            project: "p".into(),
            trigger: On::Change,
            by: None,
            revision: None,
            script_revision: None,
            dry_run,
            started_at: 0,
            duration_ms: 0,
            ok,
            error: None,
            steps: vec![],
        }
    }

    #[test]
    fn a_failure_then_a_success_is_a_recovery_once() {
        let log = RunLog::default();
        assert_eq!(log.record(run(true, false)), Standing::Unchanged);
        assert_eq!(log.record(run(false, false)), Standing::Unchanged);
        assert!(log.is_failing(Kind::Api, "a"));
        assert_eq!(
            log.record(run(true, true)),
            Standing::Unchanged,
            "a dry run tells nothing"
        );
        assert_eq!(log.record(run(true, false)), Standing::Recovered);
        assert_eq!(log.record(run(true, false)), Standing::Unchanged);
        assert_eq!(log.history(Kind::Api, "a").len(), 5);
        assert!(log.last(Kind::Script, "a").is_none());
        assert_eq!(Kind::Script.section(), "scripts");
    }
}
