use std::collections::HashMap;

use crate::domain::app::Application;
use crate::domain::credential::{Credential, delivery_id, variable_part};
use crate::domain::ldap::LdapConfig;
use crate::domain::project::GitProject;
use crate::domain::webhook::Webhook;
use crate::ports::secrets::{SecretsError, SecretsPort};

// Resolves a list of credential_ids against the SecretsPort and combines
// all key-value pairs into a single HashMap.
//
// Receives &[String] (only the ids) and not an entire Source: it doesn't need more,
// and this way the scheduler doesn't have to fabricate a fake Source to call it.
//
// Every field is delivered under `<id>_<field>` (domain::credential::delivery_id:
// `cred-portainer-ldap`'s `password` → `portainer_ldap_password`), and also
// as the plain `<field>` ONLY when exactly one of the credentials has it.
// Two credentials with the same field used to overwrite each other in the
// flat map, silently and in list order; now the ambiguous flat name is
// simply absent and each value stays reachable under its id. One
// credential (a git checkout, a token webhook) keeps its plain names, which
// is what the built-in consumers read (`username`, `token`, `ssh_key_path`).
//
// A resolution failure HALTS the use case and propagates to the caller. Previously it
// was swallowed with a warn! and continued with partial or empty credentials —
// the sync would later fail with a confusing connector error, or worse,
// "worked" without the expected authentication.
pub async fn resolve_credentials(
    secrets: &dyn SecretsPort,
    credential_ids: &[String],
) -> Result<HashMap<String, String>, SecretsError> {
    let mut resolved: Vec<(&String, HashMap<String, String>)> = Vec::new();

    for credential_id in credential_ids {
        // The same credential named twice is still one credential: it must
        // not make its own fields look shared.
        if resolved.iter().any(|(id, _)| *id == credential_id) {
            continue;
        }
        match secrets.resolve(credential_id).await {
            Ok(creds) => resolved.push((credential_id, creds)),
            Err(e) => {
                return Err(SecretsError {
                    message: format!("credential '{}': {}", credential_id, e.message),
                });
            }
        }
    }

    merge(resolved)
}

fn merge(
    resolved: Vec<(&String, HashMap<String, String>)>,
) -> Result<HashMap<String, String>, SecretsError> {
    // How many of these credentials have each field, compared as the
    // variable name it becomes (`TOKEN` and `token` are one CREDENTIAL_TOKEN):
    // the flat name is delivered only where the answer is one.
    let mut holders: HashMap<String, usize> = HashMap::new();
    for (_, fields) in &resolved {
        for field in fields.keys() {
            *holders.entry(variable_part(field)).or_default() += 1;
        }
    }

    // Every name delivered, keyed by the variable it becomes, with where it
    // came from: two different values never land on one variable. That is
    // an error naming both, not a silent last-one-wins.
    let mut all_credentials = HashMap::new();
    let mut origin: HashMap<String, (String, String)> = HashMap::new();
    // A closure that mutably borrows the two maps above: it has to be
    // declared `mut` because calling it changes what it captured.
    let mut deliver = |name: String, value: &String, credential_id: &str, field: &str| {
        let variable = variable_part(&name);
        match origin.get(&variable) {
            Some((other, other_field)) if other != credential_id || other_field != field => {
                return Err(SecretsError {
                    message: format!(
                        "credentials '{other}' (field '{other_field}') and '{credential_id}' \
                         (field '{field}') would both reach the connector as CREDENTIAL_{} — \
                         rename one of the fields",
                        variable.to_uppercase()
                    ),
                });
            }
            _ => {}
        }
        origin.insert(variable, (credential_id.to_string(), field.to_string()));
        all_credentials.insert(name, value.clone());
        Ok(())
    };
    for (credential_id, fields) in &resolved {
        let prefix = format!("{}_", delivery_id(credential_id));
        for (field, value) in fields {
            let variable = variable_part(field);
            let unshared = holders[&variable] == 1;
            // A field already named after its credential (`portainer_url` on
            // cred-portainer, the hand-made form that predates this) is not
            // prefixed twice: a script scanning CREDENTIAL_<SYSTEM>_<SITE>_*
            // would otherwise find a second, phantom site. Compared in
            // variable form, so `PORTAINER_URL` counts as named after it too.
            // Only an unshared field with something after the prefix:
            // `ssh_key_path` on cred-ssh, shared with another credential, is
            // a field like any other.
            let hand_made =
                unshared && variable.len() > prefix.len() && variable.starts_with(&prefix);
            if hand_made {
                deliver(field.clone(), value, credential_id, field)?;
                continue;
            }
            deliver(format!("{prefix}{field}"), value, credential_id, field)?;
            if unshared {
                deliver(field.clone(), value, credential_id, field)?;
            }
        }
    }
    Ok(all_credentials)
}

// What names a credential in its configuration: the applications that pass it
// to their connector, the projects that clone with it, the webhooks that send
// with it, the directory login's service account, and, for a Vault, the
// credentials reading from it. Sorted, so a page and a diff of it are stable.
pub struct CredentialUsers {
    pub credentials: Vec<String>,
    pub applications: Vec<String>,
    pub projects: Vec<String>,
    pub webhooks: Vec<String>,
    pub ldap: bool,
}

pub fn users_of(
    credential_id: &str,
    credentials: &HashMap<String, Credential>,
    applications: &HashMap<String, Application>,
    projects: &HashMap<String, GitProject>,
    webhooks: &HashMap<String, Webhook>,
    ldap: Option<&LdapConfig>,
) -> CredentialUsers {
    let mut app_ids: Vec<String> = applications
        .iter()
        .filter(|(_, app)| app.credentials.iter().any(|c| c == credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    app_ids.sort();
    let mut project_ids: Vec<String> = projects
        .iter()
        .filter(|(_, project)| project.credential_id() == Some(credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    project_ids.sort();
    // Without these, a credential only a webhook sends with looked unused,
    // and safe to remove.
    let mut webhook_ids: Vec<String> = webhooks
        .iter()
        .filter(|(_, webhook)| webhook.credentials().iter().any(|c| c == credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    webhook_ids.sort();
    // A Vault's users are the credentials reading from it (their `vault:`).
    let mut credential_ids: Vec<String> = credentials
        .iter()
        .filter(|(_, c)| c.vault.as_deref() == Some(credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    credential_ids.sort();
    CredentialUsers {
        credentials: credential_ids,
        applications: app_ids,
        projects: project_ids,
        webhooks: webhook_ids,
        ldap: ldap.is_some_and(|l| l.bind_credential == credential_id),
    }
}

// Whether a credential's secrets resolve right now, asked exactly as a sync
// asks (through the same resolver chain and its cache), so "resolves" here
// means a sync will get them. None when it does; otherwise the resolver's
// own message, which names what is missing (the env var, the file, the
// Vault path) and never a value. The values themselves are dropped here.
pub async fn check(secrets: &dyn SecretsPort, credential_id: &str) -> Option<String> {
    secrets
        .resolve(credential_id)
        .await
        .err()
        .map(|e| e.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::secrets::mock::MockSecrets;

    #[tokio::test]
    async fn no_credential_ids_resolve_to_an_empty_map() {
        let secrets = MockSecrets::new();
        let resolved = resolve_credentials(&secrets, &[])
            .await
            .expect("empty is fine");
        assert!(resolved.is_empty());
    }

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[tokio::test]
    async fn every_field_is_also_delivered_under_its_credential_id() {
        let secrets = MockSecrets::new().with("cred-portainer-ldap", [("password", "s3cret")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-portainer-ldap"]))
            .await
            .unwrap();
        assert_eq!(resolved["portainer_ldap_password"], "s3cret");
        // One credential: the plain name is kept for the built-in consumers.
        assert_eq!(resolved["password"], "s3cret");
        assert_eq!(resolved.len(), 2);
    }

    // The hand-made form: the field already carries its credential's name.
    // A script scanning CREDENTIAL_<SYSTEM>_<SITE>_* must see nothing new.
    #[tokio::test]
    async fn a_field_already_named_after_its_credential_is_not_prefixed_twice() {
        let secrets = MockSecrets::new()
            .with("cred-portainer-a", [("portainer_a_url", "https://a")])
            .with("cred-portainer-b", [("portainer_b_url", "https://b")]);
        let resolved =
            resolve_credentials(&secrets, &ids(&["cred-portainer-a", "cred-portainer-b"]))
                .await
                .unwrap();
        let mut names: Vec<&String> = resolved.keys().collect();
        names.sort();
        assert_eq!(names, ["portainer_a_url", "portainer_b_url"]);
    }

    // However the hand-made field is written, it is not prefixed again.
    #[tokio::test]
    async fn the_no_double_prefix_rule_ignores_case() {
        let secrets =
            MockSecrets::new().with("cred-avamar-aa1", [("AVAMAR_AA1_URL", "https://aa1")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-avamar-aa1"]))
            .await
            .unwrap();
        let names: Vec<&String> = resolved.keys().collect();
        assert_eq!(names, ["AVAMAR_AA1_URL"]);
    }

    #[tokio::test]
    async fn the_id_drops_a_cred_prefix_and_anything_outside_a_z0_9() {
        let secrets = MockSecrets::new()
            .with("cred_isilon", [("token", "a")])
            .with("Portainer.Prod", [("username", "b")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred_isilon", "Portainer.Prod"]))
            .await
            .unwrap();
        assert_eq!(resolved["isilon_token"], "a");
        assert_eq!(resolved["portainer_prod_username"], "b");
    }

    #[tokio::test]
    async fn a_field_two_credentials_share_is_only_delivered_under_their_ids() {
        let secrets = MockSecrets::new()
            .with(
                "cred-portainer",
                [("username", "admin"), ("password", "p1")],
            )
            .with("cred-ldap", [("password", "p2")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-portainer", "cred-ldap"]))
            .await
            .unwrap();
        assert!(
            !resolved.contains_key("password"),
            "a shared field must not pick a winner"
        );
        assert_eq!(resolved["portainer_password"], "p1");
        assert_eq!(resolved["ldap_password"], "p2");
        // Unique to one credential: still delivered flat.
        assert_eq!(resolved["username"], "admin");
    }

    // A shared field is a field like any other, even when its name happens
    // to start with the credential's id: never handed out bare.
    #[tokio::test]
    async fn a_shared_field_starting_with_its_id_is_still_prefixed() {
        let secrets = MockSecrets::new()
            .with("cred-ssh", [("ssh_key_path", "/a")])
            .with("cred-ssh-dc4", [("ssh_key_path", "/b")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-ssh", "cred-ssh-dc4"]))
            .await
            .unwrap();
        assert!(!resolved.contains_key("ssh_key_path"));
        assert_eq!(resolved["ssh_ssh_key_path"], "/a");
        assert_eq!(resolved["ssh_dc4_ssh_key_path"], "/b");
    }

    // Two values that would reach one variable fail the resolution, naming
    // both, instead of one silently winning.
    #[tokio::test]
    async fn two_values_on_one_variable_are_an_error() {
        let secrets = MockSecrets::new()
            .with("cred-avamar", [("aa1_url", "old")])
            .with("cred-avamar-aa1", [("url", "new")]);
        let err = resolve_credentials(&secrets, &ids(&["cred-avamar", "cred-avamar-aa1"]))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("CREDENTIAL_AVAMAR_AA1_URL"),
            "{}",
            err.message
        );

        let secrets = MockSecrets::new()
            .with("cred-a", [("TOKEN", "1")])
            .with("cred-b", [("token", "2")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-a", "cred-b"]))
            .await
            .unwrap();
        assert!(!resolved.contains_key("token") && !resolved.contains_key("TOKEN"));
    }

    #[tokio::test]
    async fn a_credential_listed_twice_keeps_its_bare_names() {
        let secrets = MockSecrets::new().with("cred-gh", [("token", "t")]);
        let resolved = resolve_credentials(&secrets, &ids(&["cred-gh", "cred-gh"]))
            .await
            .unwrap();
        assert_eq!(resolved["token"], "t");
        assert_eq!(resolved["gh_token"], "t");
    }

    // The property the 0.6-era rewrite bought: a failing credential HALTS the
    // sync with the credential named, instead of continuing with partial
    // credentials and failing later with a confusing connector error.
    #[tokio::test]
    async fn a_failing_credential_halts_and_names_itself() {
        let secrets = MockSecrets::new();
        let err = resolve_credentials(&secrets, &["cred-ghost".to_string()])
            .await
            .expect_err("mock knows no credentials");
        assert!(
            err.message.contains("cred-ghost"),
            "error was: {}",
            err.message
        );
    }

    // A credential only a webhook sends with is used, not unused.
    #[test]
    fn a_webhook_s_credential_is_used_by_it() {
        let webhook = |yaml: &str| -> Webhook { serde_saphyr::from_str(yaml).unwrap() };
        let webhooks = HashMap::from([
            (
                "ntf-b".to_string(),
                webhook(
                    "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  credentials: [cred-hook]\n",
                ),
            ),
            (
                "ntf-a".to_string(),
                webhook(
                    "type: ascender\nevents: [sync_failed]\nascender:\n  url: https://a.example.com\n  job_template: 1\n  credentials: [cred-hook]\n",
                ),
            ),
            (
                "ntf-c".to_string(),
                webhook(
                    "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n",
                ),
            ),
        ]);
        let users = users_of(
            "cred-hook",
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &webhooks,
            None,
        );
        assert_eq!(users.webhooks, ["ntf-a", "ntf-b"]);
    }

    // A Vault is used by the credentials reading from it, not unused.
    #[test]
    fn a_vaults_users_are_the_credentials_naming_it() {
        let credential = |yaml: &str| -> Credential { serde_saphyr::from_str(yaml).unwrap() };
        let credentials = HashMap::from([
            (
                "cred-vault".to_string(),
                credential("type: vault\naddress: https://vault.example\nenv_vars:\n  token: T\n"),
            ),
            (
                "cred-b".to_string(),
                credential("type: token\nvault: cred-vault\nvault_path: b\n"),
            ),
            (
                "cred-a".to_string(),
                credential("type: token\nvault: cred-vault\nvault_path: a\n"),
            ),
            (
                "cred-env".to_string(),
                credential("type: token\nenv_vars:\n  token: X\n"),
            ),
        ]);
        let users = users_of(
            "cred-vault",
            &credentials,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            None,
        );
        assert_eq!(users.credentials, ["cred-a", "cred-b"]);
        let users = users_of(
            "cred-a",
            &credentials,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            None,
        );
        assert!(users.credentials.is_empty());
    }
}
