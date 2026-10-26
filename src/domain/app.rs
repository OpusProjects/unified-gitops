use serde::{Deserialize, Serialize};

// An application is one live system and the desired state for it: the
// connection (which connector, its config and credentials) and a folder of
// desired state in a git project, reconciled as one unit.
//
// The connection lives HERE, in the configuration admins own, and never in
// the git folder teams edit: whoever can merge to the desired-state repository
// must not be able to point the writes, and the credentials with them, at
// another system.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Application {
    // What it is, for people: shown under its name in the UI. The name itself
    // is the item's identity (like AWX), taken by the loader.
    #[serde(default)]
    pub description: Option<String>,

    // The connector that talks to this system: "powerscale", "ascender".
    pub connector: String,

    // Resolved per run through the secrets chain and handed to the script as
    // CREDENTIAL_<FIELD>, exactly as unified-api hands them to connectors.
    #[serde(default)]
    pub credentials: Vec<String>,

    // Free-form: whatever the connector needs to reach the system (a URL, a
    // cluster name). Passed as APPLICATION_CONFIG, JSON. Not secret — anything
    // secret belongs in a credential.
    #[serde(default)]
    pub config: serde_json::Map<String, serde_json::Value>,

    // The git checkout (a project) and the folder inside it. Every
    // `*.yaml` / `*.yml` file under the folder, recursively, is desired state.
    pub project: String,
    #[serde(default = "default_path")]
    pub path: String,

    // How often desired state is compared with the live systems. That
    // comparison is what notices drift: someone changing an object by hand
    // shows up as OutOfSync at the next check, git commit or not.
    #[serde(default = "default_interval_seconds")]
    pub sync_interval_seconds: u64,

    // Apply automatically when out of sync, or only report it and wait for a
    // person to press Sync. Off by default: turning on automatic writes to a
    // production system is a decision, not a default.
    #[serde(default)]
    pub auto_sync: bool,

    // Delete objects that exist on the system but are absent from git. Off by
    // default and the most dangerous switch here: with it off, a removed line
    // in git leaves the object alone (and the plan lists it as "would delete").
    #[serde(default)]
    pub prune: bool,
}

fn default_path() -> String {
    ".".to_string()
}

fn default_interval_seconds() -> u64 {
    300
}

impl Application {
    // The folder as a path inside the checkout, never empty: `path: ""` means
    // the repository's root like ".", but an empty pathspec is refused by
    // `git archive`, which made a rollback of such an application fail.
    pub fn folder(&self) -> &str {
        match self.path.trim_end_matches('/') {
            "" => ".",
            folder => folder,
        }
    }

    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.sync_interval_seconds == 0 {
            errors.push(format!(
                "application '{id}' sync_interval_seconds must be at least 1"
            ));
        }
        // A path that climbs out of the checkout would read another
        // project's files as this application's desired state.
        if std::path::Path::new(&self.path).is_absolute()
            || self.path.split('/').any(|part| part == "..")
        {
            errors.push(format!(
                "application '{id}' path '{}' must stay inside its project checkout",
                self.path
            ));
        }
        errors
    }
}
