use utoipa::openapi::security::{ApiKey as OpenApiKey, ApiKeyValue, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::adapters::r#in::http;

// Add the security scheme (X-API-Key header) to the generated spec
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.as_mut().unwrap();
        components.add_security_scheme(
            "api_key",
            SecurityScheme::ApiKey(OpenApiKey::Header(ApiKeyValue::new("X-API-Key"))),
        );
    }
}

// The complete OpenAPI spec: utoipa generates it at compile-time from
// #[utoipa::path] attributes of each handler listed here.
// A new handler doesn't appear in Swagger until registered in paths()
// (and its response structs in components/schemas).
#[derive(OpenApi)]
#[openapi(
    modifiers(&SecurityAddon),
    security(
        ("api_key" = [])
    ),
    paths(
        http::index::api_index,
        http::health::healthz,
        http::health::readyz,
        http::metrics::metrics,
        http::apis::list_apis,
        http::apis::get_api,
        http::apis::run_api,
        http::scripts::list_scripts,
        http::scripts::get_script,
        http::scripts::run_script,
        http::webhooks::list_webhooks,
        http::webhooks::get_webhook,
        http::webhooks::test_webhook,
        http::projects::list_projects,
        http::projects::sync_project_now,
        http::credentials::list_credentials,
        http::settings::get_settings,
        http::users::get_auth,
        http::config::get_config,
        http::config::get_config_file,
        http::config::put_config,
        http::config::put_config_file,
        http::config::delete_config_file,
        http::config::validate_config,
        http::config::reload_config,
    ),
    components(schemas(
        http::error::ErrorBody,
        http::index::ApiIndex,
        http::health::ReadyStatus,
        http::health::SyncHealthInfo,
        http::health::RunInfo,
        http::webhooks::WebhookSummary,
        http::webhooks::WebhookDetail,
        http::webhooks::RequestInfo,
        crate::application::webhooks::Delivery,
        http::apis::ApiSummary,
        http::apis::ApiDetail,
        http::scripts::ScriptSummary,
        http::scripts::ScriptDetail,
        crate::domain::script::Stdin,
        crate::domain::run::Run,
        crate::domain::run::Step,
        crate::domain::run::Kind,
        crate::domain::trigger::On,
        crate::domain::api::Per,
        crate::domain::api::OnFailure,
        crate::domain::webhook::Event,
        crate::domain::webhook::ContentType,
        http::projects::ProjectInfo,
        http::projects::ProjectUsers,
        http::projects::ProjectSyncResult,
        http::credentials::CredentialInfo,
        http::credentials::FileKeyInfo,
        http::credentials::CredentialUsersInfo,
        http::credentials::VaultConnectionInfo,
        http::settings::Settings,
        http::settings::ServerSettings,
        http::settings::UiSettings,
        http::settings::ProjectsSettings,
        http::settings::SecretsSettings,
        http::settings::VaultSettings,
        http::settings::ConfigApiSettings,
        http::users::AuthInfo,
        http::users::Me,
        http::users::Methods,
        http::users::LdapInfo,
        http::users::AccessRuleInfo,
        http::users::ApiKeyInfo,
        http::config::ConfigBundle,
        http::config::ConfigFileInfo,
        http::config::ConfigInventory,
        http::config::ConfigRejected,
        http::config::ConfigSummary,
        http::config::DeltaInfo,
        http::config::ReloadInfo,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Webhooks", description = "What project pulls report, sent to any HTTP receiver, with each one's recent sends (admin-only)"),
        (name = "APIs", description = "Calls to an API when a watched project changes: what each watches and sends, its runs, and running it by hand or dry"),
        (name = "Scripts", description = "Scripts run when a watched project changes: what each watches and runs, its runs with their output, and running it by hand or dry"),
        (name = "Projects", description = "The git repositories this instance watches: their checkouts, pulls and health"),
        (name = "Credentials", description = "Configured credentials: where each one's secrets come from, whether they resolve now, and what uses it (admin-only; never a secret value)"),
        (name = "Auth", description = "Who the caller is, and the ways in: LDAP login and API keys (read-only; declared in git)"),
        (name = "Configuration", description = "The configuration directory over HTTP (admin-only): read it, validate a proposed change exactly as --check-config would, write it atomically, and reload it into the running process. Off unless config_api.enabled is set")
    ),
    // No explicit version: utoipa takes it from Cargo.toml (CARGO_PKG_VERSION)
    info(
        title = "Unified GitOps",
        description = "Git-triggered ops automation inside your network"
    )
)]
pub struct ApiDoc;
