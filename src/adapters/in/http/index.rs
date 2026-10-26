use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;

// The entry point of the API: every collection, by name, as a path.
//
// Modelled on AWX's /api/v2/, and deliberately just as boring — a flat map of
// name to path. It is what turns the API from a set of addresses you have to
// know into something you can start reading at the root and follow, whether
// "you" is a person in a browser or `jq` in a pipeline.
//
// Paths, never absolute URLs: this process does not reliably know its own
// external origin (an ingress, a port-forward, another instance federating
// through it), and a confidently wrong URL is worse than a relative one the
// caller resolves against whatever address it used to get here.

#[derive(Serialize, ToSchema)]
pub struct ApiIndex {
    /// The running version, same source as the OpenAPI spec's
    pub version: &'static str,
    /// Desired state and how it stands against the live systems
    pub applications: &'static str,
    /// Find an object on the systems by key or value: `?q=`
    pub search: &'static str,
    /// Admin-only; absent for a restricted key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connectors: Option<&'static str>,
    /// Admin-only; absent for a restricted key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhooks: Option<&'static str>,
    /// Admin-only route; absent for a restricted key
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<&'static str>,
    /// Admin-only, and only when config_api.enabled is on
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<&'static str>,
    /// Who this caller is, and how anyone can log in
    pub auth: &'static str,
    /// Configured credentials and whether they resolve (admin-only)
    pub credentials: &'static str,
    /// The configuration in force, defaults included (admin-only)
    pub settings: &'static str,
    pub health: &'static str,
    pub ready: &'static str,
    pub metrics: &'static str,
    pub docs: &'static str,
}

#[utoipa::path(
    get,
    path = "/api/v1/",
    tag = "Index",
    responses(
        (status = 200, description = "Every collection this key may read, as paths", body = ApiIndex)
    )
)]
pub async fn api_index(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<ApiIndex> {
    // A link the caller cannot follow is worse than no link: it invites a
    // click that can only 403. So the two admin-only collections are present
    // exactly when this key could actually read them — which also makes the
    // index an honest answer to "what am I allowed to see?".
    let admin = auth.permissions.is_admin();

    Json(ApiIndex {
        version: env!("CARGO_PKG_VERSION"),
        applications: "/api/v1/applications",
        search: "/api/v1/search",
        connectors: admin.then_some("/api/v1/connectors"),
        webhooks: admin.then_some("/api/v1/webhooks"),
        projects: admin.then_some("/api/v1/projects"),
        // Same condition the config handlers enforce: the routes exist in the
        // router either way, and answer 403 when no store is configured.
        config: (admin && state.config_store.is_some()).then_some("/api/v1/config"),
        auth: "/api/v1/auth",
        credentials: "/api/v1/credentials",
        settings: "/api/v1/settings",
        health: "/healthz",
        ready: "/readyz",
        metrics: "/metrics",
        docs: "/swagger-ui/",
    })
}
