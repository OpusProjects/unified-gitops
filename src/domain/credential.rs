use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialType {
    UsernamePassword,
    Token,
    SshKey,
}

// Reference to a credential — comes from configuration YAML.
// Does not store secrets, only knows WHERE to read them from the environment.
// The infrastructure (ESO, docker secrets, .env) is responsible for injecting them.
// Unknown keys are config typos: fail startup naming the key instead of
// silently applying a default (the policy is explained once, in config.rs).
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Credential {
    // What it is, for people: shown under its name in the UI. The name itself
    // is the item's identity (like AWX), taken by the loader.
    #[serde(default)]
    pub description: Option<String>,

    #[serde(rename = "type")]
    pub credential_type: CredentialType,

    // KV v2 path (under `secrets.vault.mount`) to read this credential from
    // Vault. When set, `secret_keys` maps our names to fields of that secret
    // (empty = every field verbatim) and env_vars/env_prefix/secret_file are
    // not consulted. Requires the `secrets.vault:` block in config.yaml —
    // validated at startup, so a vault_path without a Vault fails the deploy
    // rather than the first sync.
    #[serde(default)]
    pub vault_path: Option<String>,

    // Our field name → the FULL name of the env var holding it
    // ex: {"password": "LDAP_PASSWORD"} → reads LDAP_PASSWORD, delivers `password`
    #[serde(default)]
    pub env_vars: HashMap<String, String>,

    // DEPRECATED (env_vars says the same thing in one place): env vars
    // prefix, joined to each `secret_keys` value with `_`
    // ex: "SECTION9" → reads SECTION9_USERNAME, SECTION9_PASSWORD
    // Still accepted, with a notice at startup naming the env_vars to write.
    pub env_prefix: Option<String>,

    // Path to a JSON file with secrets — ex: "/run/secrets/section9-api.json"
    pub secret_file: Option<String>,

    // Mapping: our name → field name in the Vault secret or the JSON file
    // (or the env var suffix under the deprecated env_prefix)
    // ex: {"username": "USERNAME", "password": "PASSWORD"}
    #[serde(default)]
    pub secret_keys: HashMap<String, String>,

    // Paths to files that the script consumes directly (SSH keys, certificates, etc.)
    // ex: {"ssh_key": "/run/secrets/id_rsa"} → CREDENTIAL_SSH_KEY_PATH=/run/secrets/id_rsa
    #[serde(default)]
    pub file_keys: HashMap<String, String>,
}

impl CredentialType {
    // As written after `type:` in credentials.yaml.
    pub fn name(&self) -> &'static str {
        match self {
            CredentialType::UsernamePassword => "username_password",
            CredentialType::Token => "token",
            CredentialType::SshKey => "ssh_key",
        }
    }
}

impl Credential {
    // Where the secret values are read from, in the order the resolver
    // chain decides it: a Vault path wins over everything, then env vars
    // (env_vars, or the deprecated env_prefix), then a JSON file. "files" is
    // a credential made only of file_keys (paths handed to the script, an
    // SSH key, a certificate).
    pub fn backend(&self) -> &'static str {
        if self.vault_path.is_some() {
            "vault"
        } else if !self.env_vars.is_empty() || self.env_prefix.is_some() {
            "env"
        } else if self.secret_file.is_some() {
            "file"
        } else {
            "files"
        }
    }

    // The environment variables an env-backed credential reads, sorted and
    // spelled exactly as the env resolver spells them: the env_vars values,
    // or PREFIX_FIELD for each secret key under the deprecated env_prefix.
    // Empty for any other backend. Not called `env_vars`: that is the field.
    pub fn env_var_names(&self) -> Vec<String> {
        if self.vault_path.is_some() {
            return Vec::new();
        }
        let mut vars: Vec<String> = if !self.env_vars.is_empty() {
            self.env_vars.values().cloned().collect()
        } else if let Some(prefix) = &self.env_prefix {
            self.secret_keys
                .values()
                .map(|suffix| format!("{}_{}", prefix, suffix))
                .collect()
        } else {
            Vec::new()
        };
        vars.sort();
        vars
    }

    // What the strict load refuses about this credential. Two ways of
    // naming env vars at once would leave one silently ignored; secret_keys
    // next to env_vars, with no Vault path or file for them to name fields
    // of, would mean nothing at all.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.env_vars.is_empty() {
            return errors;
        }
        if self.env_prefix.is_some() {
            errors.push(format!(
                "Credential '{id}' sets both env_vars and env_prefix — keep env_vars"
            ));
        }
        if !self.secret_keys.is_empty() && self.vault_path.is_none() && self.secret_file.is_none() {
            errors.push(format!(
                "Credential '{id}' sets env_vars and secret_keys — secret_keys name fields \
                 of a vault_path or a secret_file, and this credential has neither"
            ));
        }
        errors
    }

    // The notice for a credential still written in the deprecated env_prefix
    // form, naming the exact env_vars to write in its place. Not an error:
    // configuration repositories move to env_vars after the release that
    // introduced it, so both forms load until a later one refuses the old.
    pub fn deprecation(&self, id: &str) -> Option<String> {
        let prefix = self.env_prefix.as_ref()?;
        if self.vault_path.is_some() || self.secret_keys.is_empty() {
            return Some(format!(
                "credential '{id}': env_prefix is deprecated and reads nothing here; remove it"
            ));
        }
        // Sorted, so the suggestion is the same on every start.
        let mut fields: Vec<(&String, &String)> = self.secret_keys.iter().collect();
        fields.sort();
        let pairs: Vec<String> = fields
            .into_iter()
            .map(|(field, suffix)| format!("{field}: {prefix}_{suffix}"))
            .collect();
        Some(format!(
            "credential '{id}': env_prefix + secret_keys is deprecated and will be refused \
             in a later release; write env_vars: {{{}}}",
            pairs.join(", ")
        ))
    }
}

// The id a credential's fields are also delivered under: its name
// lowercased, a leading `cred-`/`cred_` dropped (every credential has one by
// convention, so it would only add noise), and anything outside [a-z0-9]
// made `_` — the result becomes part of an env var name (CREDENTIAL_<ID>_<FIELD>).
pub fn delivery_id(name: &str) -> String {
    let lower = name.to_lowercase();
    let stripped = lower
        .strip_prefix("cred-")
        .or_else(|| lower.strip_prefix("cred_"))
        .unwrap_or(&lower);
    stripped
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(yaml: &str) -> Credential {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn deprecated_prefix_vars_are_spelled_as_the_resolver_reads_them() {
        let c = credential(
            "type: username_password\nenv_prefix: D42\nsecret_keys:\n  username: USER\n  password: PASS\n",
        );
        assert_eq!(c.backend(), "env");
        assert_eq!(c.env_var_names(), vec!["D42_PASS", "D42_USER"]);
    }

    #[test]
    fn env_vars_name_the_full_variables() {
        let c = credential(
            "type: username_password\nenv_vars:\n  username: LDAP_USER\n  password: LDAP_PASSWORD\n",
        );
        assert_eq!(c.backend(), "env");
        assert_eq!(c.env_var_names(), vec!["LDAP_PASSWORD", "LDAP_USER"]);
        assert!(c.errors("cred-ldap").is_empty());
        assert_eq!(c.deprecation("cred-ldap"), None);
    }

    #[test]
    fn a_vault_path_wins_and_reads_no_env_vars() {
        let c = credential("type: token\nvault_path: pcd/github\nenv_vars:\n  token: GH_TOKEN\n");
        assert_eq!(c.backend(), "vault");
        assert!(c.env_var_names().is_empty());
    }

    #[test]
    fn env_vars_and_env_prefix_together_are_refused_naming_both() {
        let c = credential("type: token\nenv_vars:\n  token: GH_TOKEN\nenv_prefix: GH\n");
        let errors = c.errors("cred-gh");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("env_vars") && errors[0].contains("env_prefix"));
    }

    #[test]
    fn secret_keys_next_to_env_vars_need_a_vault_path_or_a_file() {
        let yaml = "type: token\nenv_vars:\n  token: GH_TOKEN\nsecret_keys:\n  token: TOKEN\n";
        let errors = credential(yaml).errors("cred-gh");
        assert!(
            errors.iter().any(|e| e.contains("secret_keys")),
            "{errors:?}"
        );
        let with_file = format!("{yaml}secret_file: /run/secrets/gh.json\n");
        assert!(credential(&with_file).errors("cred-gh").is_empty());
    }

    #[test]
    fn the_prefix_form_loads_with_a_notice_naming_the_env_vars_to_write() {
        let c = credential(
            "type: username_password\nenv_prefix: X\nsecret_keys:\n  username: USERNAME\n  password: PASSWORD\n",
        );
        assert!(c.errors("cred-x").is_empty());
        assert_eq!(
            c.deprecation("cred-x").as_deref(),
            Some(
                "credential 'cred-x': env_prefix + secret_keys is deprecated and will be refused in a later release; write env_vars: \
                 {password: X_PASSWORD, username: X_USERNAME}"
            )
        );
    }

    #[test]
    fn delivery_ids_drop_the_cred_prefix_and_anything_outside_a_z0_9() {
        assert_eq!(delivery_id("cred-portainer-ldap"), "portainer_ldap");
        assert_eq!(delivery_id("cred_isilon"), "isilon");
        assert_eq!(delivery_id("CRED-Isilon.Prod"), "isilon_prod");
        assert_eq!(delivery_id("github-token"), "github_token");
        // Only a LEADING prefix is dropped.
        assert_eq!(delivery_id("my-cred-x"), "my_cred_x");
    }
}
