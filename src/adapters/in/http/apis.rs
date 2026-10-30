use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::{IntoParams, ToSchema};

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::actions::Request;
use crate::domain::api::{Api, OnFailure, Per};
use crate::domain::run::{Kind, Run};
use crate::domain::trigger::On;

// API items: what they watch, where they send, and how their runs went. A
// restricted caller sees and runs those of its projects; another's reads as
// absent.

#[derive(Serialize, ToSchema)]
pub struct ApiSummary {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    /// The watched project
    pub project: String,
    pub labels: std::collections::BTreeMap<String, String>,
    /// The files whose change runs it; empty for every file
    pub paths: Vec<String>,
    pub ignore_paths: Vec<String>,
    /// What runs it: change, startup, schedule, manual
    pub on: Vec<On>,
    pub schedule: Option<String>,
    pub debounce_seconds: u64,
    /// Scheme and host only: never a path or query, which can carry a secret
    pub destination: String,
    pub method: String,
    /// none, bearer, basic, header, query, oauth2 or login
    pub auth: String,
    pub per: Per,
    pub upsert: bool,
    pub compare: bool,
    pub deletes: bool,
    pub on_failure: OnFailure,
    pub credentials: Vec<String>,
    pub timeout_seconds: u64,
    pub attempts: u32,
    /// The last run since this process started, if any
    pub last: Option<Run>,
}

#[derive(Serialize, ToSchema)]
pub struct ApiDetail {
    #[serde(flatten)]
    pub summary: ApiSummary,
    /// Its runs since this process started, newest first, at most 50
    pub history: Vec<Run>,
}

fn summary(state: &AppState, id: &str, a: &Api) -> ApiSummary {
    let http = &a.api.http;
    ApiSummary {
        name: id.to_string(),
        description: a.description.clone(),
        enabled: a.enabled,
        project: a.project.clone(),
        labels: a.labels.clone(),
        paths: a.trigger.paths.clone(),
        ignore_paths: a.trigger.ignore_paths.clone(),
        on: a.trigger.on.clone(),
        schedule: a.trigger.schedule.clone(),
        debounce_seconds: a.trigger.debounce_seconds,
        destination: a.destination(),
        method: http.method_or("PUT").to_string(),
        auth: match &http.auth {
            Some(auth) => auth.name(),
            None if a.credential.is_some() => "bearer",
            None => "none",
        }
        .to_string(),
        per: a.api.per,
        upsert: a.api.upsert.is_some(),
        compare: a.api.compare.is_some(),
        deletes: a.api.on_delete.is_some(),
        on_failure: a.on_failure,
        credentials: a.credentials().into_iter().cloned().collect(),
        timeout_seconds: a.timeout_seconds,
        attempts: a.retry.attempts,
        last: state.runner.log.last(Kind::Api, id),
    }
}

// The item, if the caller may see it.
fn visible<'a>(
    config: &'a crate::RuntimeConfig,
    auth: &AuthContext,
    id: &str,
) -> Result<&'a Api, ApiError> {
    config
        .apis
        .get(id)
        .filter(|a| auth.permissions.allows_project(&a.project))
        .ok_or_else(|| ApiError::not_found(format!("API '{id}' is not configured")))
}

#[utoipa::path(
    get,
    path = "/api/v1/apis",
    tag = "APIs",
    responses(
        (status = 200, description = "Every API item the caller may see: what it watches, where it sends, its last run", body = Vec<ApiSummary>)
    )
)]
pub async fn list_apis(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<Vec<ApiSummary>> {
    let config = state.config();
    let mut out: Vec<ApiSummary> = config
        .apis
        .iter()
        .filter(|(_, a)| auth.permissions.allows_project(&a.project))
        .map(|(id, a)| summary(&state, id, a))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

#[utoipa::path(
    get,
    path = "/api/v1/apis/{id}",
    tag = "APIs",
    params(("id" = String, Path, description = "API item name")),
    responses(
        (status = 200, description = "One API item, with its recent runs", body = ApiDetail),
        (status = 404, description = "No such API item, or not the caller's", body = ErrorBody)
    )
)]
pub async fn get_api(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ApiDetail>, ApiError> {
    let config = state.config();
    let api = visible(&config, &auth, &id)?;
    Ok(Json(ApiDetail {
        summary: summary(&state, &id, api),
        history: state.runner.log.history(Kind::Api, &id),
    }))
}

#[derive(Deserialize, IntoParams)]
pub struct RunParams {
    /// Build every request and read what compare and upsert read, but send
    /// nothing that writes: the run's steps carry the bodies
    #[serde(default)]
    pub dry_run: bool,
}

#[utoipa::path(
    post,
    path = "/api/v1/apis/{id}/run",
    tag = "APIs",
    params(("id" = String, Path, description = "API item name"), RunParams),
    responses(
        (status = 200, description = "Run now, over every file its trigger selects; the run, also kept in its history", body = Run),
        (status = 404, description = "No such API item, or not the caller's", body = ErrorBody),
        (status = 409, description = "It does not run by hand (trigger.on has no manual)", body = ErrorBody)
    )
)]
pub async fn run_api(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<RunParams>,
) -> Result<Json<Run>, ApiError> {
    let config = state.config();
    let api = visible(&config, &auth, &id)?;
    // A dry run sends nothing that writes: always allowed.
    if !params.dry_run && !api.trigger.fires_on(On::Manual) {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            format!("API '{id}' does not run by hand: add manual to its trigger.on"),
        ));
    }
    let request = Request {
        kind: Kind::Api,
        item: id.clone(),
        trigger: On::Manual,
        by: Some(auth.key_name.as_deref().unwrap_or("open").to_string()),
        files: None,
        previous_revision: None,
        dry_run: params.dry_run,
    };
    let run = state
        .runner
        .run(&config, request)
        .await
        .map_err(ApiError::internal)?;
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        if params.dry_run {
            "api_dry_run"
        } else {
            "api_run"
        },
        &id,
        if run.ok { "success" } else { "error" },
    );
    Ok(Json(run))
}
