use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::apis::RunParams;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::actions::Request;
use crate::domain::api::{OnFailure, Per};
use crate::domain::run::{Kind, Run};
use crate::domain::script::{Script, Stdin};
use crate::domain::trigger::On;

// Script items: what they watch, which file they run, and how their runs
// went. Scoped like APIs: a restricted caller sees and runs those of its
// projects.

#[derive(Serialize, ToSchema)]
pub struct ScriptSummary {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    /// The watched project
    pub project: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub paths: Vec<String>,
    pub ignore_paths: Vec<String>,
    pub on: Vec<On>,
    pub schedule: Option<String>,
    pub debounce_seconds: u64,
    /// The project the file is in
    pub script_project: String,
    pub file: String,
    /// The arguments as written (templates, not rendered)
    pub args: Vec<String>,
    pub interpreter: Option<String>,
    pub working_dir: Option<String>,
    /// The names of the variables it sets, never their values
    pub env: Vec<String>,
    pub per: Per,
    pub stdin: Stdin,
    pub success_exit_codes: Vec<i32>,
    pub on_failure: OnFailure,
    pub credential: Option<String>,
    pub timeout_seconds: u64,
    pub attempts: u32,
    pub last: Option<Run>,
}

#[derive(Serialize, ToSchema)]
pub struct ScriptDetail {
    #[serde(flatten)]
    pub summary: ScriptSummary,
    /// Its runs since this process started, newest first, at most 50
    pub history: Vec<Run>,
}

fn summary(state: &AppState, id: &str, s: &Script) -> ScriptSummary {
    let c = &s.script;
    ScriptSummary {
        name: id.to_string(),
        description: s.description.clone(),
        enabled: s.enabled,
        project: s.project.clone(),
        labels: s.labels.clone(),
        paths: s.trigger.paths.clone(),
        ignore_paths: s.trigger.ignore_paths.clone(),
        on: s.trigger.on.clone(),
        schedule: s.trigger.schedule.clone(),
        debounce_seconds: s.trigger.debounce_seconds,
        script_project: c.project_of(&s.project).to_string(),
        file: c.file.clone(),
        args: c.args.clone(),
        interpreter: c.interpreter.clone(),
        working_dir: c.working_dir.clone(),
        env: c.env.keys().cloned().collect(),
        per: c.per,
        stdin: c.stdin,
        success_exit_codes: c.success_exit_codes.clone(),
        on_failure: s.on_failure,
        credential: s.credential.clone(),
        timeout_seconds: s.timeout_seconds,
        attempts: s.retry.attempts,
        last: state.runner.log.last(Kind::Script, id),
    }
}

fn visible<'a>(
    config: &'a crate::RuntimeConfig,
    auth: &AuthContext,
    id: &str,
) -> Result<&'a Script, ApiError> {
    config
        .scripts
        .get(id)
        .filter(|s| auth.permissions.allows_project(&s.project))
        .ok_or_else(|| ApiError::not_found(format!("Script '{id}' is not configured")))
}

#[utoipa::path(
    get,
    path = "/api/v1/scripts",
    tag = "Scripts",
    responses(
        (status = 200, description = "Every script item the caller may see: what it watches, what it runs, its last run", body = Vec<ScriptSummary>)
    )
)]
pub async fn list_scripts(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Json<Vec<ScriptSummary>> {
    let config = state.config();
    let mut out: Vec<ScriptSummary> = config
        .scripts
        .iter()
        .filter(|(_, s)| auth.permissions.allows_project(&s.project))
        .map(|(id, s)| summary(&state, id, s))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Json(out)
}

#[utoipa::path(
    get,
    path = "/api/v1/scripts/{id}",
    tag = "Scripts",
    params(("id" = String, Path, description = "Script item name")),
    responses(
        (status = 200, description = "One script item, with its recent runs and their output", body = ScriptDetail),
        (status = 404, description = "No such script item, or not the caller's", body = ErrorBody)
    )
)]
pub async fn get_script(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<ScriptDetail>, ApiError> {
    let config = state.config();
    let script = visible(&config, &auth, &id)?;
    Ok(Json(ScriptDetail {
        summary: summary(&state, &id, script),
        history: state.runner.log.history(Kind::Script, &id),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/scripts/{id}/run",
    tag = "Scripts",
    params(("id" = String, Path, description = "Script item name"), RunParams),
    responses(
        (status = 200, description = "Run now, over every file its trigger selects; the run, also kept in its history. A dry run runs nothing and says what would run", body = Run),
        (status = 404, description = "No such script item, or not the caller's", body = ErrorBody),
        (status = 409, description = "It does not run by hand (trigger.on has no manual)", body = ErrorBody)
    )
)]
pub async fn run_script(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(params): Query<RunParams>,
) -> Result<Json<Run>, ApiError> {
    let config = state.config();
    let script = visible(&config, &auth, &id)?;
    if !params.dry_run && !script.trigger.fires_on(On::Manual) {
        return Err(ApiError::new(
            axum::http::StatusCode::CONFLICT,
            format!("Script '{id}' does not run by hand: add manual to its trigger.on"),
        ));
    }
    let request = Request {
        kind: Kind::Script,
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
            "script_dry_run"
        } else {
            "script_run"
        },
        &id,
        if run.ok { "success" } else { "error" },
    );
    Ok(Json(run))
}
