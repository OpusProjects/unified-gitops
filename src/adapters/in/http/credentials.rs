use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::credentials::{check, users_of};
use crate::domain::credential::CredentialType;

// The credentials configured on this instance, for the UI's Credentials
// page: what each one is, where its secrets come from, whether they resolve
// right now, and what depends on it.
//
// NEVER a secret value. The page answers "is it there, and who needs it" —
// the first questions when a sync fails with an authentication error, or
// before a credential is rotated or removed — and none of that needs a value.
// What it shows is what credentials.yaml already says (variable names, file
// paths, a Vault path) plus a yes/no. Admin-only all the same, like the
// configuration API that holds the same file: a map of where every secret
// lives is not for every key.

#[derive(Serialize, ToSchema)]
pub struct CredentialInfo {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    /// `username_password`, `token`, `ssh_key` or `vault`
    #[serde(rename = "type")]
    pub credential_type: &'static str,
    /// Where the secrets are read from: `vault`, `env`, `file`, or `files`
    /// (only file paths handed to the script)
    pub backend: &'static str,
    pub vault_path: Option<String>,
    /// The `type: vault` credential a vault_path is read from (absent = the
    /// deprecated config.yaml secrets.vault)
    pub vault: Option<String>,
    /// The environment variables read, for an env-backed credential
    pub env_vars: Vec<String>,
    pub secret_file: Option<String>,
    /// The names the connector receives the secrets under (not their values)
    pub secret_keys: Vec<String>,
    /// Files handed to the script by path (an SSH key, a certificate)
    pub file_keys: Vec<FileKeyInfo>,
    /// Whether the secrets resolve right now, asked as a sync asks
    pub resolves: bool,
    /// Why not: the resolver's message, naming what is missing
    pub error: Option<String>,
    pub used_by: CredentialUsersInfo,
    /// For a `type: vault` credential: where the Vault is, how it logs in and how long it keeps what it read
    pub vault_connection: Option<VaultConnectionInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct VaultConnectionInfo {
    pub address: String,
    /// The KV v2 mount `vault_path` values live under
    pub mount: String,
    /// `token` or `kubernetes`
    pub auth: &'static str,
    pub kubernetes_role: Option<String>,
    /// The variable holding its token, for token auth
    pub token_env: Option<String>,
    pub timeout_seconds: u64,
    /// How long a value read from this Vault is reused, as in effect: the
    /// deprecated config.yaml `secrets.cache_ttl_seconds` while it is set,
    /// else this Vault's own (default 60). 0 = no cache
    pub cache_ttl_seconds: u64,
}

#[derive(Serialize, ToSchema)]
pub struct FileKeyInfo {
    pub name: String,
    pub path: String,
}

#[derive(Serialize, ToSchema)]
pub struct CredentialUsersInfo {
    /// For a `type: vault` credential: the credentials reading from it
    pub credentials: Vec<String>,
    pub projects: Vec<String>,
    /// Webhooks that send with it
    pub webhooks: Vec<String>,
    /// API items that call with it
    pub apis: Vec<String>,
    /// Whether the LDAP login's service account binds with it
    pub ldap: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/credentials",
    tag = "Credentials",
    responses(
        (status = 200, description = "Configured credentials: where their secrets come from, whether they resolve now, and what uses them. Never a secret value", body = Vec<CredentialInfo>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_credentials(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<CredentialInfo>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();

    let mut credentials = Vec::with_capacity(config.credentials.len());
    for (id, credential) in &config.credentials {
        let error = check(state.secrets.as_ref(), id).await;
        let users = users_of(
            id,
            &config.credentials,
            &config.projects,
            &config.webhooks,
            config.ldap.as_ref(),
        );

        // The delivered names: env_vars' fields when that is what is read
        // (backend "env" in the new form), secret_keys' otherwise.
        let delivered = if credential.backend() == "env" && !credential.env_vars.is_empty() {
            &credential.env_vars
        } else {
            &credential.secret_keys
        };
        let mut secret_keys: Vec<String> = delivered.keys().cloned().collect();
        secret_keys.sort();
        let mut file_keys: Vec<FileKeyInfo> = credential
            .file_keys
            .iter()
            .map(|(name, path)| FileKeyInfo {
                name: name.clone(),
                path: path.clone(),
            })
            .collect();
        file_keys.sort_by(|a, b| a.name.cmp(&b.name));

        // The same defaults secrets::build_chain applies.
        let vault_connection =
            (credential.credential_type == CredentialType::Vault).then(|| VaultConnectionInfo {
                address: credential.address.clone().unwrap_or_default(),
                mount: credential
                    .mount
                    .clone()
                    .unwrap_or_else(|| "secret".to_string()),
                auth: if credential.kubernetes_role.is_some() {
                    "kubernetes"
                } else {
                    "token"
                },
                kubernetes_role: credential.kubernetes_role.clone(),
                token_env: credential.env_vars.get("token").cloned(),
                timeout_seconds: credential.timeout_seconds.unwrap_or(10),
                // The cache in effect: the deprecated config.yaml one
                // replaces each Vault's own while it is set.
                cache_ttl_seconds: config
                    .secrets
                    .cache_ttl_seconds
                    .unwrap_or_else(|| credential.cache_ttl_seconds.unwrap_or(60)),
            });

        credentials.push(CredentialInfo {
            name: id.clone(),
            description: credential.description.clone(),
            credential_type: credential.credential_type.name(),
            backend: credential.backend(),
            vault_path: credential.vault_path.clone(),
            vault: credential.vault.clone(),
            env_vars: credential.env_var_names(),
            secret_file: credential.secret_file.clone(),
            secret_keys,
            file_keys,
            resolves: error.is_none(),
            error,
            used_by: CredentialUsersInfo {
                credentials: users.credentials,
                projects: users.projects,
                webhooks: users.webhooks,
                apis: {
                    let mut ids: Vec<String> = config
                        .apis
                        .iter()
                        .filter(|(_, a)| a.credentials().contains(&id))
                        .map(|(a, _)| a.clone())
                        .collect();
                    ids.sort();
                    ids
                },
                ldap: users.ldap,
            },
            vault_connection,
        });
    }

    credentials.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(credentials))
}
