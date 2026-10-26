use serde::{Deserialize, Serialize};

// Configuration shape of one API key — api_keys.yaml.
//
// The SECRET is never in the YAML: `env` names the environment variable that
// holds it, so the config file can live in git while the value comes from the
// deployment (a k8s Secret, Vault via ESO, a .env file...). Rotating a key is
// therefore an external process: swap the env var's value and restart — no
// config or code change.
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
// Here the stakes are highest: this file is the access-control list, and a
// silently dropped key means a permission the operator believes is in force
// and is not.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ApiKeyDef {
    // What it is, for people: shown under its name in the UI. The name itself
    // is the item's identity (like AWX), taken by the loader.
    #[serde(default)]
    pub description: Option<String>,

    // Environment variable that holds the secret value of this key
    pub env: String,

    // admin = everything; restricted = only the applications listed
    #[serde(default)]
    pub role: ApiKeyRole,

    // Application ids this key may see, refresh and sync (restricted keys
    // only). Connectors, projects, credentials and the configuration
    // stay admin-only: they describe every system, not one application.
    #[serde(default)]
    pub applications: Vec<String>,
}

// `rename_all = "lowercase"` lets the YAML say `role: admin` instead of
// `role: Admin` — config files shouldn't care about Rust naming style.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyRole {
    Admin,
    // Restricted is the default: forgetting `role:` must never silently
    // hand out admin. #[default] marks which variant Default::default() picks.
    #[default]
    Restricted,
}
