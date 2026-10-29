// Credential and CredentialType live in unified-secrets (the crate every
// secrets adapter reads them from, shared with unified-api); this module
// re-exports them so the rest of the service keeps naming
// `crate::domain::credential::Credential`.
pub use unified_secrets::credential::{Credential, CredentialType, delivery_id, variable_part};

// What this service checks about a credential when it loads its
// configuration, in its own words. A trait because Credential is a type from
// another crate: Rust only lets a crate add inherent methods to its own types,
// so the service's rules arrive as an extension trait instead — bring
// `CredentialRules` into scope and `credential.errors(id)` works as before.
pub trait CredentialRules {
    fn declared_fields(&self) -> Vec<String>;
    fn errors(&self, id: &str) -> Vec<String>;
    fn deprecation(&self, id: &str) -> Option<String>;
}

impl CredentialRules for Credential {
    // The fields this credential delivers, as far as its configuration says:
    // the env_vars keys, the secret_keys keys (of the deprecated env_prefix, a
    // file or a Vault secret) and `<key>_path` per file_key. A file or a
    // Vault secret read without secret_keys delivers every field it holds,
    // which only the file or the Vault knows, so those are not listed.
    // Sorted, so a message naming them is stable.
    fn declared_fields(&self) -> Vec<String> {
        let mut fields: Vec<String> = if !self.env_vars.is_empty() {
            self.env_vars.keys().cloned().collect()
        } else {
            self.secret_keys.keys().cloned().collect()
        };
        fields.extend(self.file_keys.keys().map(|key| format!("{key}_path")));
        fields.sort();
        fields.dedup();
        fields
    }

    // What the strict load refuses about this credential. A Vault needs an
    // address and exactly one way to log in, and nothing a login has; a
    // login has none of a Vault's settings. A credential reads from one
    // place: two ways of naming env vars at once, or env_vars beside a
    // Vault path, a file or secret_keys, would leave one side silently
    // ignored.
    fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.credential_type == CredentialType::Vault {
            return vault_errors(self, id);
        }
        for (key, set) in [
            ("address", self.address.is_some()),
            ("mount", self.mount.is_some()),
            ("kubernetes_role", self.kubernetes_role.is_some()),
            ("jwt_path", self.jwt_path.is_some()),
            ("timeout_seconds", self.timeout_seconds.is_some()),
            ("cache_ttl_seconds", self.cache_ttl_seconds.is_some()),
        ] {
            if set {
                errors.push(format!(
                    "Credential '{id}' sets {key}, which only a `type: vault` credential uses"
                ));
            }
        }
        if let (Some(vault), None) = (&self.vault, &self.vault_path) {
            errors.push(format!(
                "Credential '{id}' names vault '{vault}' but no vault_path to read there"
            ));
        }
        if self.env_vars.is_empty() {
            // The same rule for the older forms: a Vault path wins over the
            // env prefix, which wins over the file, and the loser was never
            // read, silently.
            let places: Vec<&str> = [
                ("vault_path", self.vault_path.is_some()),
                ("env_prefix", self.env_prefix.is_some()),
                ("secret_file", self.secret_file.is_some()),
            ]
            .into_iter()
            .filter_map(|(key, set)| set.then_some(key))
            .collect();
            if places.len() > 1 {
                errors.push(format!(
                    "Credential '{id}' reads from {} — a credential reads from one place: \
                     keep one",
                    places.join(" and ")
                ));
            }
            return errors;
        }
        if self.env_prefix.is_some() {
            errors.push(format!(
                "Credential '{id}' sets both env_vars and env_prefix — keep env_vars"
            ));
        }
        // env_vars beside a vault_path or a secret_file used to load, and one
        // side was silently never read (Vault wins over env_vars; env_vars
        // over the file).
        for (key, set) in [
            ("vault_path", self.vault_path.is_some()),
            ("secret_file", self.secret_file.is_some()),
            ("secret_keys", !self.secret_keys.is_empty()),
        ] {
            if set {
                errors.push(format!(
                    "Credential '{id}' sets {key} beside env_vars — a credential reads \
                     from one place: env_vars names each variable, so keep it or {key}, \
                     not both"
                ));
            }
        }
        errors
    }

    // The notice for a credential still written in the deprecated env_prefix
    // form, naming the exact env_vars to write in its place. Not an error:
    // configuration repositories move to env_vars after the release that
    // introduced it, so both forms load until a later one refuses the old.
    fn deprecation(&self, id: &str) -> Option<String> {
        if self.vault_path.is_some() && self.vault.is_none() {
            return Some(format!(
                "credential '{id}': a vault_path without `vault:` reads from config.yaml's \
                 secrets.vault, which is deprecated and will be refused in a later release; \
                 declare the Vault as a `type: vault` credential and write vault: <its name>"
            ));
        }
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

fn vault_errors(credential: &Credential, id: &str) -> Vec<String> {
    let mut errors = Vec::new();
    match &credential.address {
        None => errors.push(format!(
            "Credential '{id}' is a Vault but has no address (https://vault.example.com:8200)"
        )),
        Some(a) if !(a.starts_with("https://") || a.starts_with("http://")) => errors.push(
            format!("Credential '{id}': address '{a}' must be an http:// or https:// address"),
        ),
        Some(_) => {}
    }
    for field in credential.env_vars.keys().filter(|f| *f != "token") {
        errors.push(format!(
            "Credential '{id}': env_vars.{field} is not a field of a Vault — the only \
             one is `token`, the variable holding its token"
        ));
    }
    match (
        credential.env_vars.contains_key("token"),
        credential.kubernetes_role.is_some(),
    ) {
        (true, true) => errors.push(format!(
            "Credential '{id}' logs in to Vault both with env_vars.token and \
             kubernetes_role — keep one"
        )),
        (false, false) => errors.push(format!(
            "Credential '{id}' has no way to log in to Vault: set env_vars: {{token: \
             <VAR>}} or kubernetes_role"
        )),
        _ => {}
    }
    for (key, set) in [
        ("vault_path", credential.vault_path.is_some()),
        ("vault", credential.vault.is_some()),
        ("env_prefix", credential.env_prefix.is_some()),
        ("secret_file", credential.secret_file.is_some()),
        ("secret_keys", !credential.secret_keys.is_empty()),
        ("file_keys", !credential.file_keys.is_empty()),
    ] {
        if set {
            errors.push(format!(
                "Credential '{id}' is a Vault and sets {key}, which only the \
                 credentials reading from a Vault use"
            ));
        }
    }
    if credential.timeout_seconds == Some(0) {
        errors.push(format!(
            "Credential '{id}': timeout_seconds 0 would fail every request to this \
             Vault — give it a bound in seconds (default 10)"
        ));
    }
    if credential.jwt_path.is_some() && credential.kubernetes_role.is_none() {
        errors.push(format!(
            "Credential '{id}' sets jwt_path without kubernetes_role — it is only \
             read for Kubernetes auth"
        ));
    }
    errors
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

    // One credential reads from one place: secret_keys beside env_vars is
    // refused, with or without a file or a Vault path for them to name
    // fields of, and so are those beside env_vars on their own (they used
    // to load with one side silently never read).
    #[test]
    fn secret_keys_next_to_env_vars_are_refused_with_or_without_a_file() {
        let yaml = "type: token\nenv_vars:\n  token: GH_TOKEN\nsecret_keys:\n  token: TOKEN\n";
        let errors = credential(yaml).errors("cred-gh");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets secret_keys beside env_vars")),
            "{errors:?}"
        );
        let with_file = format!("{yaml}secret_file: /run/secrets/gh.json\n");
        let errors = credential(&with_file).errors("cred-gh");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets secret_keys beside env_vars")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets secret_file beside env_vars")),
            "{errors:?}"
        );
    }

    #[test]
    fn env_vars_beside_a_file_or_a_vault_path_are_refused() {
        let c = credential(
            "type: token\nenv_vars:\n  token: T\nsecret_file: /run/x.json\nvault_path: a/b\n",
        );
        let errors = c.errors("cred-x");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets vault_path beside env_vars")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.contains("sets secret_file beside env_vars")),
            "{errors:?}"
        );
    }

    #[test]
    fn declared_fields_are_what_the_configuration_names() {
        let env = credential(
            "type: username_password\nenv_vars:\n  username: U\n  password: P\nfile_keys:\n  ssh_key: /run/k\n",
        );
        assert_eq!(
            env.declared_fields(),
            ["password", "ssh_key_path", "username"]
        );
        let prefixed = credential("type: token\nenv_prefix: GH\nsecret_keys:\n  token: TOKEN\n");
        assert_eq!(prefixed.declared_fields(), ["token"]);
        // Every field of the secret: only Vault knows which.
        let whole = credential("type: token\nvault: v\nvault_path: a/b\n");
        assert!(whole.declared_fields().is_empty());
    }

    #[test]
    fn the_older_forms_read_from_one_place_too() {
        let c = credential(
            "type: token\nvault_path: a/b\nenv_prefix: GH\nsecret_file: /f.json\nsecret_keys:\n  token: TOKEN\n",
        );
        let errors = c.errors("cred-x");
        assert!(
            errors
                .iter()
                .any(|e| e.contains("reads from vault_path and env_prefix and secret_file")),
            "{errors:?}"
        );
    }

    #[test]
    fn a_vault_with_a_zero_timeout_is_refused() {
        let c = credential(
            "type: vault\naddress: https://v\nenv_vars:\n  token: T\ntimeout_seconds: 0\n",
        );
        assert!(
            c.errors("cred-v")
                .iter()
                .any(|e| e.contains("timeout_seconds 0"))
        );
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
