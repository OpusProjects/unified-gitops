use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::webhooks::{self, Delivery, Job};
use crate::domain::webhook::{Event, WebhookType};

// Webhooks: which events go where, and how the last send went.
// Admin-only, like credentials: a webhook can name any project, and its
// destination says where this instance reaches out to.

#[derive(Serialize, ToSchema)]
pub struct WebhookSummary {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub webhook_type: WebhookType,
    pub events: Vec<Event>,
    /// Every project, present and future (`projects: all`)
    pub all_projects: bool,
    /// The projects it listens to, when it names them; empty with
    /// all_projects false = none
    pub projects: Vec<String>,
    /// Where it sends: scheme and host only, never a path or query, which
    /// can carry a receiver's secret
    pub destination: String,
    /// ascender and awx: the job template (or workflow job template) it starts
    pub job_template: Option<u64>,
    pub workflow_job_template: Option<u64>,
    pub credentials: Vec<String>,
    /// How long one attempt may take, and how many times a send is retried
    pub timeout_seconds: u64,
    pub retries: u32,
    /// type webhook: its HTTP method, how the credential is sent, the names
    /// of its fixed headers (never their values), whether it sends a template
    /// or the default message, and the header its signature goes in, if signed
    pub request: Option<RequestInfo>,
    /// The last send since this process started, if any
    pub last: Option<Delivery>,
}

#[derive(Serialize, ToSchema)]
pub struct RequestInfo {
    pub method: String,
    /// bearer, basic, none, or header (the credential in `auth_header`)
    pub auth: String,
    pub auth_header: Option<String>,
    pub headers: Vec<String>,
    /// true: a body template; false: the default message
    pub templated: bool,
    pub signature_header: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/v1/webhooks",
    tag = "Webhooks",
    responses(
        (status = 200, description = "Every webhook: its events, projects and destination, and its last send", body = Vec<WebhookSummary>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_webhooks(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<WebhookSummary>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let mut out: Vec<WebhookSummary> = config
        .webhooks
        .iter()
        .map(|(id, n)| info(&state, id, n))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

// One webhook as the list shows it.
fn info(state: &AppState, id: &str, n: &crate::domain::webhook::Webhook) -> WebhookSummary {
    let target = n.ascender.as_ref().or(n.awx.as_ref());
    let mut projects = n.projects.named().to_vec();
    projects.sort();
    WebhookSummary {
        name: id.to_string(),
        description: n.description.clone(),
        webhook_type: n.webhook_type,
        events: n.events.clone(),
        all_projects: n.projects.is_all(),
        projects,
        destination: n.destination(),
        job_template: target.and_then(|t| t.job_template),
        workflow_job_template: target.and_then(|t| t.workflow_job_template),
        credentials: n.credentials().to_vec(),
        timeout_seconds: n
            .webhook
            .as_ref()
            .map(|w| w.timeout_seconds)
            .or(target.map(|t| t.timeout_seconds))
            .unwrap_or_default(),
        retries: n
            .webhook
            .as_ref()
            .map(|w| w.retries)
            .or(target.map(|t| t.retries))
            .unwrap_or_default(),
        request: n.webhook.as_ref().map(|w| {
            use crate::domain::webhook::{Auth, AuthScheme};
            let (auth, auth_header) = match &w.auth {
                Auth::Header(h) => ("header", Some(h.header.clone())),
                Auth::Scheme(AuthScheme::Basic) => ("basic", None),
                Auth::Scheme(AuthScheme::None) => ("none", None),
                Auth::Scheme(AuthScheme::Bearer) => ("bearer", None),
                // Left out: a bearer token when there is a credential.
                Auth::Default if w.credentials.is_empty() => ("none", None),
                Auth::Default => ("bearer", None),
            };
            RequestInfo {
                method: w.method.clone(),
                auth: auth.to_string(),
                auth_header,
                headers: w.headers.keys().cloned().collect(),
                templated: w.body.is_some(),
                signature_header: w.sign.as_ref().map(|s| s.header.clone()),
            }
        }),
        last: state.dispatcher.log.last(id),
    }
}

#[derive(Serialize, ToSchema)]
pub struct WebhookDetail {
    #[serde(flatten)]
    pub summary: WebhookSummary,
    /// Its sends since this process started, newest first, at most 50.
    /// Retries of one message count as one send
    pub history: Vec<Delivery>,
}

#[utoipa::path(
    get,
    path = "/api/v1/webhooks/{id}",
    tag = "Webhooks",
    params(("id" = String, Path, description = "Webhook name")),
    responses(
        (status = 200, description = "One webhook, with its recent sends", body = WebhookDetail),
        (status = 403, description = "API key is not admin", body = ErrorBody),
        (status = 404, description = "No such webhook", body = ErrorBody)
    )
)]
pub async fn get_webhook(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<WebhookDetail>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let Some(n) = config.webhooks.get(&id) else {
        return Err(ApiError::not_found(format!(
            "webhook '{id}' is not configured"
        )));
    };
    Ok(Json(WebhookDetail {
        summary: info(&state, &id, n),
        history: state.dispatcher.log.history(&id),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/webhooks/{id}/test",
    tag = "Webhooks",
    params(("id" = String, Path, description = "Webhook name")),
    responses(
        (status = 200, description = "Sent once, now, without retrying: the outcome, also kept as the webhook's last send", body = Delivery),
        (status = 403, description = "API key is not admin", body = ErrorBody),
        (status = 404, description = "No such webhook", body = ErrorBody)
    )
)]
pub async fn test_webhook(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Delivery>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let Some(webhook) = config.webhooks.get(&id) else {
        return Err(ApiError::not_found(format!(
            "webhook '{id}' is not configured"
        )));
    };
    let mut message = webhooks::test_message(&id, config.url.as_deref());
    message.insert("webhook".into(), serde_json::json!(id));
    let delivery = webhooks::deliver(
        &*state.secrets,
        &*state.http,
        &state.dispatcher.log,
        Job {
            webhook_id: id.clone(),
            webhook: webhook.clone(),
            message,
        },
        None,
    )
    .await;
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        "webhook_test",
        &id,
        if delivery.ok { "success" } else { "error" },
    );
    Ok(Json(delivery))
}
