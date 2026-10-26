use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};

// Connectors: how each is run, and who uses it. Admin-only: a connector
// describes what this instance can write to on every system, not one
// application. What a connector manages is the script's to describe, at run
// time, not configuration to show here.

#[derive(Serialize, ToSchema)]
pub struct ConnectorInfo {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    /// What kind of connector: `script` (a program run per describe, list and apply)
    #[serde(rename = "type")]
    pub connector_type: crate::domain::connector::ConnectorType,
    pub project: Option<String>,
    /// The script: relative to the project's checkout when it has one,
    /// otherwise a path on this instance's disk
    pub path: String,
    pub timeout_seconds: u64,
    /// Applications that use this connector
    pub used_by: Vec<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/connectors",
    tag = "Connectors",
    responses(
        (status = 200, description = "Every connector: its script, where it is read from, its timeout, and the applications using it", body = Vec<ConnectorInfo>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_connectors(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<ConnectorInfo>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let mut out: Vec<ConnectorInfo> = config
        .connectors
        .iter()
        .map(|(id, connector)| {
            let mut used_by: Vec<String> = config
                .applications
                .iter()
                .filter(|(_, a)| &a.connector == id)
                .map(|(a, _)| a.clone())
                .collect();
            used_by.sort();
            ConnectorInfo {
                name: id.clone(),
                description: connector.description.clone(),
                connector_type: connector.connector_type,
                project: connector.project.clone(),
                path: connector.path.clone(),
                timeout_seconds: connector.timeout_seconds,
                used_by,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}
