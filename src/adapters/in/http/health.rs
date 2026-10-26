use axum::Json;
use axum::extract::State;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::domain::run::SyncStatus;
use crate::domain::sync_health::SyncHealth;

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "Health",
    responses(
        (status = 200, description = "Service is alive", body = String)
    )
)]
pub async fn healthz(State(_state): State<Arc<AppState>>) -> &'static str {
    "ok"
}

#[derive(Serialize, ToSchema)]
pub struct ReadyStatus {
    pub ready: bool,
    pub applications_total: usize,
    pub applications_checked: usize,
    /// Applications not compared with their systems yet (their first check
    /// is still to come, or their checkout is not there yet)
    pub applications_pending: Vec<String>,
}

// Ready as soon as the process serves. Unlike unified-api, whose readiness is
// "has data to serve", this service's job is to reach OTHER systems: making it
// unready because a system is down would add pod restarts to that outage and
// take the API that explains it away. The body still says which applications
// have not been checked yet, for whoever is watching a rollout.
#[utoipa::path(
    get,
    path = "/readyz",
    tag = "Health",
    responses(
        (status = 200, description = "Service is ready to serve", body = ReadyStatus)
    )
)]
pub async fn readyz(State(state): State<Arc<AppState>>) -> Json<ReadyStatus> {
    let config = state.config();
    let mut pending: Vec<String> = config
        .applications
        .keys()
        .filter(|id| state.runs.status(id) == SyncStatus::Unknown)
        .cloned()
        .collect();
    pending.sort();
    Json(ReadyStatus {
        ready: true,
        applications_total: config.applications.len(),
        applications_checked: config.applications.len() - pending.len(),
        applications_pending: pending,
    })
}

// Whether periodic work keeps succeeding: last attempt, last success, last
// error, consecutive failures. The same shape unified-api reports for its
// sources, so one UI component draws both.
#[derive(Serialize, ToSchema)]
pub struct SyncHealthInfo {
    pub last_attempt_age_seconds: Option<u64>,
    pub last_success_age_seconds: Option<u64>,
    pub last_error: Option<String>,
    pub consecutive_failures: u32,
}

impl From<SyncHealth> for SyncHealthInfo {
    fn from(health: SyncHealth) -> Self {
        Self {
            last_attempt_age_seconds: health.last_attempt_age_seconds,
            last_success_age_seconds: health.last_success_age_seconds,
            last_error: health.last_error,
            consecutive_failures: health.consecutive_failures,
        }
    }
}
