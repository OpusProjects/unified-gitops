use std::collections::HashMap;

use crate::domain::app::Application;
use crate::domain::credential::Credential;
use crate::domain::ldap::LdapConfig;
use crate::domain::project::GitProject;
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

    Ok(merge(resolved))
}

fn merge(resolved: Vec<(&String, HashMap<String, String>)>) -> HashMap<String, String> {
    // How many credentials carry each field: the flat name is delivered
    // only where the answer is one.
    let mut holders: HashMap<&str, usize> = HashMap::new();
    for (_, fields) in &resolved {
        for field in fields.keys() {
            *holders.entry(field).or_default() += 1;
        }
    }
    let unique: std::collections::HashSet<String> = holders
        .into_iter()
        .filter(|(_, n)| *n == 1)
        .map(|(field, _)| field.to_string())
        .collect();

    let mut all_credentials = HashMap::new();
    for (credential_id, fields) in resolved {
        let prefix = format!("{}_", crate::domain::credential::delivery_id(credential_id));
        for (field, value) in fields {
            if unique.contains(&field) {
                all_credentials.insert(field.clone(), value.clone());
            }
            // A field already named after its credential (`portainer_url` on
            // cred-portainer, the hand-made form that predates this) is not
            // prefixed twice: a script scanning CREDENTIAL_<SYSTEM>_<SITE>_*
            // would otherwise find a second, phantom site.
            let under_id = if field.starts_with(&prefix) {
                field
            } else {
                format!("{prefix}{field}")
            };
            all_credentials.insert(under_id, value);
        }
    }
    all_credentials
}

// What names a credential in its configuration: the applications that pass it
// to their connector, the projects that clone with it, the directory login's
// service account, and, for a Vault, the credentials reading from it. Sorted, so a page and a diff of it are stable.
pub struct CredentialUsers {
    pub credentials: Vec<String>,
    pub applications: Vec<String>,
    pub projects: Vec<String>,
    pub ldap: bool,
}

pub fn users_of(
    credential_id: &str,
    credentials: &HashMap<String, Credential>,
    applications: &HashMap<String, Application>,
    projects: &HashMap<String, GitProject>,
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
        .filter(|(_, project)| project.credential.as_deref() == Some(credential_id))
        .map(|(id, _)| id.clone())
        .collect();
    project_ids.sort();
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

    #[tokio::test]
    async fn the_same_credential_named_twice_keeps_its_flat_fields() {
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
            None,
        );
        assert_eq!(users.credentials, ["cred-a", "cred-b"]);
        let users = users_of(
            "cred-a",
            &credentials,
            &HashMap::new(),
            &HashMap::new(),
            None,
        );
        assert!(users.credentials.is_empty());
    }
}
