use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

// A connector is one script that knows one kind of system (an Isilon, an
// Ascender, a key-value store). It is deliberately dumb: it can LIST what
// exists and APPLY one operation. Deciding what to change — the diff, drift,
// pruning, ordering — is the core's job, so every connector gets those for
// free and none of them reinvents it.
//
// The configuration only says how to RUN it. What it manages — the types of
// object, how each is identified, what goes first — is the script's to say,
// in `describe`, the way a Kubernetes API server describes its own resources:
// the knowledge lives next to the code that handles the objects, once, rather
// than repeated (and drifting) in every instance's configuration.
//
// The contract, as the script sees it (see docs/connectors.md):
//   <script> describe              → {"protocol": 1, "kinds": [{"kind", "key", "depends_on"}]}
//   <script> list  --kind <kind>   → JSON array of the objects that exist now
//   <script> apply --kind <kind>   → stdin {"op": "create|update|delete", "object": {...}}
// with APPLICATION_CONFIG (JSON) and CREDENTIAL_* in a scrubbed environment, and
// UNIFIED_ALLOW_WRITE=1 only on apply.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Connector {
    // What it is, for people: shown under its name in the UI. The name itself
    // is the item's identity (like AWX), taken by the loader.
    #[serde(default)]
    pub description: Option<String>,

    // What kind of connector this is. Only `script` so far, and the default:
    // the key is there so a connector reads the same as unified-api's (whose
    // connectors are `script` or `ssh`), and so another type can be added
    // here later without changing every file.
    #[serde(default, rename = "type")]
    pub connector_type: ConnectorType,

    // The script lives in a project checkout (resolved per execution, as
    // unified-api resolves connector scripts), or anywhere on disk when no
    // project is named — an absolute path baked into the image, for example.
    // `project` + `path` means the same everywhere: a file or folder in that
    // project's checkout (an application's desired state is one too).
    #[serde(default)]
    pub project: Option<String>,
    pub path: String,

    // Every describe, list or apply is bounded: a hung script is killed, not
    // waited on.
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
}

fn default_timeout_seconds() -> u64 {
    120
}

// `rename_all = "lowercase"` lets the YAML say `type: script`. An unknown
// value fails the load naming the variants serde expected.
#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum ConnectorType {
    // A program run once per describe, list and apply (docs/connectors.md).
    #[default]
    Script,
}

impl Connector {
    // The validation errors config.rs reports for this connector.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.path.trim().is_empty() {
            errors.push(format!("connector '{id}' has an empty path"));
        }
        if self.timeout_seconds == 0 {
            errors.push(format!(
                "connector '{id}' timeout_seconds must be at least 1"
            ));
        }
        errors
    }
}

// One type of object a connector manages, as it describes it.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct KindDecl {
    pub kind: String,
    // The field that identifies an object of this kind (`path` for an NFS
    // export, `name` for a job template). Desired and actual objects are
    // matched on it; it is never itself "changed".
    pub key: String,
    // Kinds of the same connector that must be applied first (a quota before
    // the export on it). Deletions run in the reverse order.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

// What `describe` answers: the contract version, and what the connector
// manages. Checked at the start of every run, before anything is listed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ConnectorDescription {
    pub protocol: u32,
    #[serde(default)]
    pub kinds: Vec<KindDecl>,
}

pub const PROTOCOL_VERSION: u32 = 1;

// A connector's kinds, validated: keyed by name (BTreeMap: a stable order in
// plans and messages).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Kinds(BTreeMap<String, KindDecl>);

impl ConnectorDescription {
    // The kinds, or every problem with them at once. A kind's name becomes a
    // file name in the state directory, so it must be a safe id.
    pub fn kinds(&self, connector: &str) -> Result<Kinds, Vec<String>> {
        let mut errors = Vec::new();
        let mut kinds = BTreeMap::new();
        if self.kinds.is_empty() {
            errors.push(format!("connector '{connector}' describes no kinds"));
        }
        for decl in &self.kinds {
            if !crate::config::is_safe_id(&decl.kind) {
                errors.push(format!(
                    "connector '{connector}' kind '{}' must use only letters, digits, \
                     '-', '_' and '.' (it names a file in the state directory)",
                    decl.kind
                ));
            }
            if decl.key.trim().is_empty() {
                errors.push(format!(
                    "connector '{connector}' kind '{}' has an empty key",
                    decl.kind
                ));
            }
            if kinds.insert(decl.kind.clone(), decl.clone()).is_some() {
                errors.push(format!(
                    "connector '{connector}' describes kind '{}' twice",
                    decl.kind
                ));
            }
        }
        for decl in &self.kinds {
            for dep in &decl.depends_on {
                if dep == &decl.kind {
                    errors.push(format!(
                        "connector '{connector}' kind '{}' depends on itself",
                        decl.kind
                    ));
                } else if !kinds.contains_key(dep) {
                    errors.push(format!(
                        "connector '{connector}' kind '{}' depends on unknown kind '{dep}'",
                        decl.kind
                    ));
                }
            }
        }
        let kinds = Kinds(kinds);
        if errors.is_empty() && kinds.has_cycle() {
            errors.push(format!("connector '{connector}' has a depends_on cycle"));
        }
        if errors.is_empty() {
            Ok(kinds)
        } else {
            Err(errors)
        }
    }
}

impl Kinds {
    pub fn get(&self, kind: &str) -> Option<&KindDecl> {
        self.0.get(kind)
    }

    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }

    // Whether `kind` depends on `other`, directly or through other kinds
    // (an export on a quota on a zone depends on the zone).
    pub fn depends_on(&self, kind: &str, other: &str) -> bool {
        let mut seen: Vec<&str> = Vec::new();
        let mut todo: Vec<&str> = vec![kind];
        while let Some(next) = todo.pop() {
            let Some(decl) = self.0.get(next) else {
                continue;
            };
            for dep in &decl.depends_on {
                if dep == other {
                    return true;
                }
                if !seen.contains(&dep.as_str()) {
                    seen.push(dep);
                    todo.push(dep);
                }
            }
        }
        false
    }

    // Kinds in an order that respects depends_on: dependencies first.
    // Validation has already refused unknown names and cycles, so a kind is
    // placed once all of its dependencies are.
    pub fn in_order(&self) -> Vec<String> {
        let mut placed: Vec<String> = Vec::new();
        while placed.len() < self.0.len() {
            let before = placed.len();
            for (name, kind) in &self.0 {
                if placed.contains(name) {
                    continue;
                }
                if kind.depends_on.iter().all(|d| placed.contains(d)) {
                    placed.push(name.clone());
                }
            }
            if placed.len() == before {
                // A cycle: validation should have caught it. Fall back to the
                // declared order rather than spinning.
                for name in self.0.keys() {
                    if !placed.contains(name) {
                        placed.push(name.clone());
                    }
                }
            }
        }
        placed
    }

    fn has_cycle(&self) -> bool {
        // Kahn's algorithm: if some kinds can never be placed, there is a cycle.
        let mut done: Vec<&str> = Vec::new();
        loop {
            let ready: Vec<&str> = self
                .0
                .iter()
                .filter(|(n, k)| {
                    !done.contains(&n.as_str())
                        && k.depends_on.iter().all(|d| done.contains(&d.as_str()))
                })
                .map(|(n, _)| n.as_str())
                .collect();
            if ready.is_empty() {
                break;
            }
            done.extend(ready);
        }
        done.len() < self.0.len()
    }
}

// What each connector last described, kept for what has no run of its own to
// ask in: finding an object by key (search) needs each kind's key. Filled at
// every run; empty for a connector no application has used since the start.
#[derive(Default)]
pub struct DescribedKinds(RwLock<HashMap<String, Kinds>>);

impl DescribedKinds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, connector: &str, kinds: Kinds) {
        let mut map = self.0.write().unwrap_or_else(|e| e.into_inner());
        map.insert(connector.to_string(), kinds);
    }

    pub fn get(&self, connector: &str) -> Option<Kinds> {
        let map = self.0.read().unwrap_or_else(|e| e.into_inner());
        map.get(connector).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn described(kinds: &[(&str, &[&str])]) -> ConnectorDescription {
        ConnectorDescription {
            protocol: 1,
            kinds: kinds
                .iter()
                .map(|(n, deps)| KindDecl {
                    kind: n.to_string(),
                    key: "name".into(),
                    depends_on: deps.iter().map(|d| d.to_string()).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn dependencies_come_first() {
        let kinds = described(&[
            ("export", &["quota"]),
            ("quota", &[]),
            ("share", &["export"]),
        ])
        .kinds("p")
        .unwrap();
        assert_eq!(kinds.in_order(), vec!["quota", "export", "share"]);
        assert!(kinds.depends_on("share", "quota"), "through export");
        assert!(kinds.depends_on("export", "quota"));
        assert!(!kinds.depends_on("quota", "export"));
        assert!(!kinds.depends_on("quota", "quota"));
    }

    #[test]
    fn unknown_dependencies_cycles_and_unsafe_names_are_refused() {
        let err = described(&[("a", &["missing"])]).kinds("p").unwrap_err();
        assert!(err[0].contains("unknown kind 'missing'"));
        let err = described(&[("a", &["b"]), ("b", &["a"])])
            .kinds("p")
            .unwrap_err();
        assert!(err[0].contains("cycle"));
        let err = described(&[("../x", &[])]).kinds("p").unwrap_err();
        assert!(err[0].contains("kind '../x'"));
        let err = described(&[]).kinds("p").unwrap_err();
        assert!(err[0].contains("describes no kinds"));
    }
}
