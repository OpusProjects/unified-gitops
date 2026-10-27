use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// Configuration shape of one API key — api_keys.yaml.
//
// The SECRET is never in the YAML: `env_vars.key` names the environment
// variable that holds it, so the config file can live in git while the value comes from the
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

    // Environment variable that holds the secret value of this key, as
    // `key: VAR` — the same shape as a credential's env_vars. `key` is the
    // only field an API key has; validation refuses any other.
    #[serde(default)]
    pub env_vars: HashMap<String, String>,

    // DEPRECATED: the same variable as a bare `env: VAR`. Still accepted,
    // with a notice at startup; exactly one of the two may be set.
    #[serde(default)]
    pub env: Option<String>,

    // admin = everything; restricted = only the applications listed
    #[serde(default)]
    pub role: ApiKeyRole,

    // Application ids this key may see, refresh and sync (restricted keys
    // only). Connectors, projects, credentials and the configuration
    // stay admin-only: they describe every system, not one application.
    #[serde(default)]
    pub applications: Vec<String>,
}

impl ApiKeyDef {
    // The environment variable holding the secret, whichever form names it.
    // Everything that reads the key goes through here, so the two forms
    // cannot drift apart. None only for a definition validation refuses.
    pub fn env_var(&self) -> Option<&str> {
        self.env_vars
            .get("key")
            .or(self.env.as_ref())
            .map(String::as_str)
    }

    // What the strict load refuses: a field other than `key`, and anything
    // but exactly one place naming the variable — two would leave one
    // silently ignored, none would leave a key with no secret.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        let mut unknown: Vec<&String> = self.env_vars.keys().filter(|f| *f != "key").collect();
        unknown.sort();
        for field in unknown {
            errors.push(format!(
                "API key '{id}': env_vars has field '{field}' — the only field is 'key'"
            ));
        }
        match (self.env_vars.contains_key("key"), self.env.is_some()) {
            (true, true) => errors.push(format!(
                "API key '{id}' sets both env_vars.key and env — keep env_vars.key"
            )),
            (false, false) => errors.push(format!(
                "API key '{id}' names no variable for its secret — set env_vars: {{key: VAR}}"
            )),
            _ => {}
        }
        errors
    }

    // The notice for a key still written as `env: VAR`, naming what to write.
    pub fn deprecation(&self, id: &str) -> Option<String> {
        let var = self.env.as_ref()?;
        Some(format!(
            "API key '{id}': env is deprecated and will be refused in a later release; \
             write env_vars: {{key: {var}}}"
        ))
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(yaml: &str) -> ApiKeyDef {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn env_vars_key_names_the_variable() {
        let k = key("env_vars:\n  key: UNIFIED_GITOPS_KEY_CENTRAL\nrole: admin\n");
        assert_eq!(k.env_var(), Some("UNIFIED_GITOPS_KEY_CENTRAL"));
        assert!(k.errors("key-central").is_empty());
        assert_eq!(k.deprecation("key-central"), None);
    }

    #[test]
    fn the_old_env_loads_with_a_notice() {
        let k = key("env: OLD_VAR\n");
        assert_eq!(k.env_var(), Some("OLD_VAR"));
        assert!(k.errors("key-old").is_empty());
        assert_eq!(
            k.deprecation("key-old").as_deref(),
            Some(
                "API key 'key-old': env is deprecated and will be refused in a later release; write env_vars: {key: OLD_VAR}"
            )
        );
    }

    #[test]
    fn a_field_other_than_key_is_refused_naming_it() {
        let errors = key("env_vars:\n  key: A\n  token: B\n").errors("k");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("'token'"), "{errors:?}");
    }

    #[test]
    fn both_or_neither_variable_is_refused() {
        let both = key("env_vars:\n  key: A\nenv: B\n").errors("k");
        assert!(both.iter().any(|e| e.contains("both")), "{both:?}");
        let neither = key("role: admin\n").errors("k");
        assert!(
            neither.iter().any(|e| e.contains("no variable")),
            "{neither:?}"
        );
    }
}
