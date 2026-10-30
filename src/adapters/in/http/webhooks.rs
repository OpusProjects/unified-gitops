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
use crate::domain::webhook::Event;

// Webhooks: which events go where, and how the last send went.
// Admin-only, like credentials: a webhook can name any project, and its
// destination says where this instance reaches out to.

#[derive(Serialize, ToSchema)]
pub struct WebhookSummary {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    /// The watched project whose events it tells
    pub project: String,
    pub labels: std::collections::BTreeMap<String, String>,
    pub events: Vec<Event>,
    /// For commit_pulled: the files a commit must touch to be told; empty
    /// for every commit
    pub paths: Vec<String>,
    pub ignore_paths: Vec<String>,
    /// Where it sends: scheme and host only, never a path or query, which
    /// can carry a receiver's secret
    pub destination: String,
    /// Every credential it reads: its own, a signature's, a client
    /// certificate's
    pub credentials: Vec<String>,
    /// How long one attempt may take, and how many attempts a send makes
    pub timeout_seconds: u64,
    pub attempts: u32,
    pub request: RequestInfo,
    /// The last send since this process started, if any
    pub last: Option<Delivery>,
}

// How it sends, without a value that could be secret: header names, never
// their values.
#[derive(Serialize, ToSchema)]
pub struct RequestInfo {
    pub method: String,
    /// none, bearer, basic, header, query, oauth2 or login
    pub auth: String,
    pub headers: Vec<String>,
    pub query: Vec<String>,
    pub content_type: crate::domain::webhook::ContentType,
    /// true: a body template; false: the event message as it is
    pub templated: bool,
    pub signature_header: Option<String>,
    /// Whether it follows what it started until a condition holds
    pub waits: bool,
    pub rate_limit: Option<String>,
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
    let http = &n.webhook.http;
    let auth = match &http.auth {
        Some(auth) => auth.name(),
        // Left out: a bearer token when there is a credential.
        None if n.credential.is_some() => "bearer",
        None => "none",
    };
    WebhookSummary {
        name: id.to_string(),
        description: n.description.clone(),
        enabled: n.enabled,
        project: n.project.clone(),
        labels: n.labels.clone(),
        events: n.events.clone(),
        paths: n.trigger.paths.clone(),
        ignore_paths: n.trigger.ignore_paths.clone(),
        destination: n.destination(),
        credentials: n.credentials().into_iter().cloned().collect(),
        timeout_seconds: n.timeout_seconds,
        attempts: n.retry.attempts,
        request: RequestInfo {
            method: http.method_or("POST").to_string(),
            auth: auth.to_string(),
            headers: http.headers.keys().cloned().collect(),
            query: http.query.keys().cloned().collect(),
            content_type: n.webhook.content_type,
            templated: n.webhook.body.is_some(),
            signature_header: http.signature.as_ref().map(|s| s.header.clone()),
            waits: n.webhook.wait.is_some(),
            rate_limit: http
                .rate_limit
                .map(|r| format!("{} per {}s", r.requests, r.per_seconds)),
        },
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
    let mut message = webhooks::test_message(webhook, config.url.as_deref());
    message.insert("webhook".into(), serde_json::json!(id));
    let delivery = webhooks::deliver(
        &*state.secrets,
        &*state.http,
        &state.dispatcher.engine,
        &state.dispatcher.log,
        Job {
            webhook_id: id.clone(),
            webhook: webhook.clone(),
            message,
        },
        webhooks::Mode::Test,
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
