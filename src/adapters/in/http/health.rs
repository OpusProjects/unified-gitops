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

// One pull of a project, newest first in its `history`: what the health
// above sums up, pull by pull. The same shape unified-api reports for its
// projects' pulls, so one UI table draws both.
#[derive(Serialize, ToSchema)]
pub struct RunInfo {
    /// When it started, unix seconds
    pub started_at: u64,
    pub duration_ms: u64,
    /// "scheduled", "manual" (Pull, the API), "check" (before an
    /// application's check or sync) or "startup" (the clones at boot, a
    /// project added by a reload)
    pub trigger: &'static str,
    /// Who asked, for a manual pull: the API key's name or the signed-in user
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// Why it failed; absent when it worked
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The commit the pull found the checkout at, and left it at
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl From<crate::domain::sync_health::RunRecord> for RunInfo {
    fn from(run: crate::domain::sync_health::RunRecord) -> Self {
        Self {
            started_at: run.started_at,
            duration_ms: run.duration_ms,
            trigger: run.trigger.name(),
            by: run.by,
            error: run.error,
            revision_before: run.revision_before,
            revision: run.revision,
        }
    }
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
