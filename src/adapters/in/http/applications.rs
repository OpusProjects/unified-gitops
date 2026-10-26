use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::adapters::r#in::http::health::SyncHealthInfo;
use crate::application::reconcile::pull_and_reconcile;
use crate::domain::plan::Operation;
use crate::domain::run::{RunMode, RunRecord, SyncStatus, Trigger, now};
use crate::ports::state_store::{Snapshot, SnapshotInfo};

// Applications: one live system each, the desired state for it in git, and
// how the two stand. Every caller may read the applications its key grants,
// their local copies included; refresh and sync are writes, so a restricted
// key may run them only on its own.

#[derive(Serialize, ToSchema)]
pub struct ApplicationSummary {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    /// The connector that talks to the system
    pub connector: String,
    /// Credentials handed to the connector (ids, never values)
    pub credentials: Vec<String>,
    /// Names of the config keys handed to the connector (values not shown: they
    /// are not secret, but a URL with a token in it is a mistake to echo)
    pub config_keys: Vec<String>,
    pub project: String,
    /// Folder inside the project checkout holding the desired-state YAML
    pub path: String,
    /// Applies automatically when out of sync (false: only reports it)
    pub auto_sync: bool,
    /// Deletes objects on the system that git does not declare
    pub prune: bool,
    pub sync_interval_seconds: u64,
    pub status: SyncStatus,
    /// The commit the last run read
    pub revision: Option<String>,
    /// Seconds since the last run (check or sync, failed or not); null if never
    pub last_checked_age_seconds: Option<u64>,
    /// Operations the last comparison found still to do
    pub pending_operations: usize,
    /// Objects on the system that git does not declare (prune off)
    pub unmanaged: usize,
    /// Declared objects already exactly as declared
    pub in_sync: usize,
    /// Present while the application is rolled back to an older commit:
    /// checks do not apply (auto_sync is paused) until the next manual sync
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<RollbackInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_health: Option<SyncHealthInfo>,
}

#[derive(Serialize, ToSchema)]
pub struct RollbackInfo {
    /// The commit whose desired state was applied
    pub revision: String,
    /// Seconds since the rollback
    pub age_seconds: u64,
}

#[derive(Deserialize, ToSchema)]
pub struct RollbackRequest {
    /// The commit to roll back to: its id, full or abbreviated (4+ hex characters)
    pub revision: String,
}

#[derive(Serialize, ToSchema)]
pub struct PlanInfo {
    pub checked_age_seconds: u64,
    pub revision: Option<String>,
    pub operations: Vec<Operation>,
    pub unmanaged: Vec<Operation>,
    pub in_sync: usize,
    pub errors: Vec<String>,
}

#[derive(Serialize, ToSchema)]
pub struct ApplicationDetail {
    #[serde(flatten)]
    pub summary: ApplicationSummary,
    /// The last comparison, whole: what a sync would do now
    pub plan: Option<PlanInfo>,
    /// The last runs, most recent first (kept in memory, up to 50)
    pub history: Vec<RunRecord>,
    /// The local copy of the system's current settings, per kind
    pub state: Vec<SnapshotInfo>,
}

fn summary(
    state: &AppState,
    id: &str,
    app: &crate::domain::app::Application,
) -> ApplicationSummary {
    let run_state = state.runs.get(id);
    let plan = run_state.plan.as_ref();
    let mut config_keys: Vec<String> = app.config.keys().cloned().collect();
    config_keys.sort();
    ApplicationSummary {
        name: id.to_string(),
        description: app.description.clone(),
        connector: app.connector.clone(),
        credentials: app.credentials.clone(),
        config_keys,
        project: app.project.clone(),
        path: app.path.clone(),
        auto_sync: app.auto_sync,
        prune: app.prune,
        sync_interval_seconds: app.sync_interval_seconds,
        status: run_state
            .status
            .with_rollback(state.store.rollback(id).is_some()),
        revision: run_state.revision.clone(),
        // The latest run of any kind, not the latest plan: a run that failed
        // before it could plan (invalid desired state) was still a check.
        last_checked_age_seconds: run_state
            .history
            .first()
            .map(|r| now().saturating_sub(r.finished_at)),
        pending_operations: plan.map(|p| p.operations.len()).unwrap_or(0),
        unmanaged: plan.map(|p| p.unmanaged.len()).unwrap_or(0),
        in_sync: plan.map(|p| p.in_sync).unwrap_or(0),
        rollback: state.store.rollback(id).map(|r| RollbackInfo {
            revision: r.revision,
            age_seconds: now().saturating_sub(r.at),
        }),
        sync_health: state.app_health.get(id).map(Into::into),
    }
}

#[utoipa::path(
    get,
    path = "/api/v1/applications",
    tag = "Applications",
    responses(
        (status = 200, description = "The applications this key may see, with their sync status", body = Vec<ApplicationSummary>)
    )
)]
pub async fn list_applications(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<Vec<ApplicationSummary>> {
    let config = state.config();
    let mut out: Vec<ApplicationSummary> = config
        .applications
        .iter()
        .filter(|(id, _)| auth.permissions.allows_application(id))
        .map(|(id, app)| summary(&state, id, app))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

fn visible<'a>(
    config: &'a crate::RuntimeConfig,
    auth: &AuthContext,
    id: &str,
) -> Result<&'a crate::domain::app::Application, ApiError> {
    let app = config
        .applications
        .get(id)
        .ok_or_else(|| ApiError::not_found(format!("application '{id}' is not configured")))?;
    if !auth.permissions.allows_application(id) {
        return Err(ApiError::forbidden(format!(
            "this API key may not access application '{id}'"
        )));
    }
    Ok(app)
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}",
    tag = "Applications",
    params(("id" = String, Path, description = "Application identifier")),
    responses(
        (status = 200, description = "The application, its pending plan and its run history", body = ApplicationDetail),
        (status = 403, description = "This key may not access the application", body = ErrorBody),
        (status = 404, description = "Application not configured", body = ErrorBody)
    )
)]
pub async fn get_application(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ApplicationDetail>, ApiError> {
    let config = state.config();
    let app = visible(&config, &auth, &id)?;
    let run_state = state.runs.get(&id);
    let plan = run_state.plan.map(|p| PlanInfo {
        checked_age_seconds: now().saturating_sub(p.checked_at),
        revision: p.revision,
        operations: p.operations,
        unmanaged: p.unmanaged,
        in_sync: p.in_sync,
        errors: p.errors,
    });
    let snapshots = state
        .store
        .list()
        .unwrap_or_default()
        .into_iter()
        .filter(|s| s.application == id)
        .collect();
    Ok(Json(ApplicationDetail {
        summary: summary(&state, &id, app),
        plan,
        history: run_state.history,
        state: snapshots,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}/state/{kind}",
    tag = "Applications",
    params(
        ("id" = String, Path, description = "Application identifier"),
        ("kind" = String, Path, description = "Kind of object")
    ),
    responses(
        (status = 200, description = "The local copy of this kind's objects on the application's system, as of its last check", body = Snapshot),
        (status = 403, description = "This key may not access the application", body = ErrorBody),
        (status = 404, description = "Unknown application, or no copy of this kind yet", body = ErrorBody)
    )
)]
pub async fn get_application_state(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path((id, kind)): Path<(String, String)>,
) -> Result<Json<Snapshot>, ApiError> {
    let config = state.config();
    visible(&config, &auth, &id)?;
    // The ids are checked before they become path segments: the application
    // id against config, the kind here.
    if !crate::config::is_safe_id(&kind) {
        return Err(ApiError::not_found(format!("no kind '{kind}'")));
    }
    match state.store.read(&id, &kind) {
        Ok(Some(snapshot)) => Ok(Json(snapshot)),
        Ok(None) => Err(ApiError::not_found(format!(
            "no copy of {kind} for {id} yet: it has not been checked"
        ))),
        Err(e) => Err(ApiError::internal(e)),
    }
}

async fn run_now(
    state: Arc<AppState>,
    auth: AuthContext,
    headers: axum::http::HeaderMap,
    id: String,
    mode: RunMode,
) -> Result<Json<RunRecord>, ApiError> {
    let config = state.config();
    visible(&config, &auth, &id)?;
    let record = pull_and_reconcile(
        &state.reconciler(),
        &config.catalog(),
        &id,
        mode,
        Trigger::Manual,
    )
    .await
    .map_err(ApiError::not_found)?;
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        match mode {
            RunMode::Plan => "application_refresh",
            RunMode::Apply => "application_sync",
        },
        &id,
        match record.result {
            crate::domain::run::RunResult::Succeeded => "success",
            crate::domain::run::RunResult::OutOfSync => "out_of_sync",
            crate::domain::run::RunResult::Partial => "partial",
            crate::domain::run::RunResult::Failed => "error",
        },
    );
    Ok(Json(record))
}

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/refresh",
    tag = "Applications",
    params(("id" = String, Path, description = "Application identifier")),
    responses(
        (status = 200, description = "Compared desired state with the system now; nothing was changed. The run record says what was found (its errors, if any, say why it could not finish)", body = RunRecord),
        (status = 403, description = "This key may not access the application", body = ErrorBody),
        (status = 404, description = "Application not configured", body = ErrorBody)
    )
)]
pub async fn refresh_application(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<RunRecord>, ApiError> {
    run_now(state, auth, headers, id, RunMode::Plan).await
}

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/sync",
    tag = "Applications",
    params(("id" = String, Path, description = "Application identifier")),
    responses(
        (status = 200, description = "Compared, then applied every operation found, one by one. The run record lists each operation and whether it succeeded; `result` is partial when some failed", body = RunRecord),
        (status = 403, description = "This key may not access the application", body = ErrorBody),
        (status = 404, description = "Application not configured", body = ErrorBody)
    )
)]
pub async fn sync_application(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Json<RunRecord>, ApiError> {
    run_now(state, auth, headers, id, RunMode::Apply).await
}

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/rollback",
    tag = "Applications",
    params(("id" = String, Path, description = "Application identifier")),
    request_body = RollbackRequest,
    responses(
        (status = 200, description = "Applied the desired state of that commit, once, as a sync would. The application is then rolled back: checks stop applying (auto_sync paused) until the next manual sync applies HEAD again", body = RunRecord),
        (status = 400, description = "Not a commit id, or not in the project's history", body = ErrorBody),
        (status = 403, description = "This key may not access the application", body = ErrorBody),
        (status = 404, description = "Application not configured", body = ErrorBody)
    )
)]
pub async fn rollback_application(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    headers: axum::http::HeaderMap,
    Json(body): Json<RollbackRequest>,
) -> Result<Json<RunRecord>, ApiError> {
    let config = state.config();
    visible(&config, &auth, &id)?;
    let result = crate::application::reconcile::rollback(
        &state.reconciler(),
        &config.catalog(),
        &id,
        body.revision.trim(),
    )
    .await;
    let outcome = match &result {
        Ok(record) if record.result == crate::domain::run::RunResult::Succeeded => "success",
        Ok(record) if record.result == crate::domain::run::RunResult::Partial => "partial",
        _ => "error",
    };
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        "application_rollback",
        &format!("{id}@{}", body.revision.trim()),
        outcome,
    );
    result.map(Json).map_err(ApiError::bad_request)
}

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/rollback/preview",
    tag = "Applications",
    params(("id" = String, Path, description = "Application identifier")),
    request_body = RollbackRequest,
    responses(
        (status = 200, description = "What a rollback to that commit would change on the system now, field by field. Nothing is applied or recorded", body = crate::domain::run::PlanSnapshot),
        (status = 400, description = "Not a commit id, not in the project's history, or its desired state does not load", body = ErrorBody),
        (status = 403, description = "This key may not access the application", body = ErrorBody),
        (status = 404, description = "Application not configured", body = ErrorBody)
    )
)]
pub async fn preview_rollback(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<RollbackRequest>,
) -> Result<Json<crate::domain::run::PlanSnapshot>, ApiError> {
    let config = state.config();
    visible(&config, &auth, &id)?;
    crate::application::reconcile::preview_rollback(
        &state.reconciler(),
        &config.catalog(),
        &id,
        body.revision.trim(),
    )
    .await
    .map(Json)
    .map_err(ApiError::bad_request)
}

// Counts for the overview and /metrics: how many applications stand where.
pub fn status_counts(state: &AppState) -> [(SyncStatus, usize); 5] {
    let config = state.config();
    let mut counts = [
        (SyncStatus::Synced, 0),
        (SyncStatus::OutOfSync, 0),
        (SyncStatus::Failed, 0),
        (SyncStatus::Unknown, 0),
        (SyncStatus::RolledBack, 0),
    ];
    for id in config.applications.keys() {
        let s = state.application_status(id);
        if let Some(c) = counts.iter_mut().find(|(k, _)| *k == s) {
            c.1 += 1;
        }
    }
    counts
}
