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
        http::applications::list_applications,
        http::applications::get_application,
        http::applications::refresh_application,
        http::applications::sync_application,
        http::applications::rollback_application,
        http::applications::preview_rollback,
        http::applications::get_application_state,
        http::search::search_objects,
        http::connectors::list_connectors,
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
        http::applications::ApplicationSummary,
        http::applications::ApplicationDetail,
        http::applications::PlanInfo,
        http::applications::RollbackInfo,
        http::applications::RollbackRequest,
        crate::domain::run::Rollback,
        crate::domain::run::PlanSnapshot,
        crate::domain::plan::Operation,
        crate::domain::plan::Change,
        crate::domain::plan::OpKind,
        crate::domain::run::RunRecord,
        crate::domain::run::AppliedOperation,
        crate::domain::run::SyncStatus,
        crate::domain::run::Trigger,
        crate::domain::run::RunMode,
        crate::domain::run::RunResult,
        http::connectors::ConnectorInfo,
        crate::domain::connector::ConnectorType,
        crate::ports::state_store::Snapshot,
        crate::application::search::FoundObject,
        crate::ports::state_store::SnapshotInfo,
        http::projects::ProjectInfo,
        http::projects::ProjectUsers,
        http::projects::ProjectSyncResult,
        http::credentials::CredentialInfo,
        http::credentials::FileKeyInfo,
        http::credentials::CredentialUsersInfo,
        http::settings::Settings,
        http::settings::ServerSettings,
        http::settings::UiSettings,
        http::settings::StateSettings,
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
        http::config::ValidationResult,
        http::config::WriteResult,
    )),
    tags(
        (name = "Health", description = "Liveness and readiness probes"),
        (name = "Applications", description = "Each live system and its desired state in git: status, the pending plan, run history and the local copy of the system's current settings; refresh compares, sync applies"),
        (name = "Connectors", description = "The scripts that talk to each kind of system, and the applications using them (admin-only)"),
        (name = "Projects", description = "Git project checkouts (admin-only operational routes)"),
        (name = "Credentials", description = "Configured credentials: where each one's secrets come from, whether they resolve now, and what uses it (admin-only; never a secret value)"),
        (name = "Auth", description = "Who the caller is, and the ways in: LDAP login and API keys (read-only; declared in git)"),
        (name = "Configuration", description = "The configuration directory over HTTP (admin-only): read it, validate a proposed change exactly as --check-config would, write it atomically, and reload it into the running process. Off unless config_api.enabled is set")
    ),
    // No explicit version: utoipa takes it from Cargo.toml (CARGO_PKG_VERSION)
    info(
        title = "Unified GitOps",
        description = "GitOps for anything with an API: desired state in git, reconciled through connector scripts"
    )
)]
pub struct ApiDoc;
