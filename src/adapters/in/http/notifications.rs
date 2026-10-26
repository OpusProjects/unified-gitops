use axum::Json;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use serde::Serialize;
use std::sync::Arc;
use utoipa::ToSchema;

use crate::AppState;
use crate::adapters::r#in::http::auth::AuthContext;
use crate::adapters::r#in::http::error::{ApiError, ErrorBody};
use crate::application::notify::{self, Delivery, Job};
use crate::domain::notification::{Event, JobTrigger, NotificationType};

// Notifications: which events go where, and how the last send went.
// Admin-only, like connectors: a notification can name any application, and
// its destination says where this instance reaches out to.

#[derive(Serialize, ToSchema)]
pub struct NotificationInfo {
    /// The item's identity, as in the configuration
    pub name: String,
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub notification_type: NotificationType,
    pub events: Vec<Event>,
    /// Every application, present and future (`applications: all`)
    pub all_applications: bool,
    /// The applications it listens to, when it names them; empty with
    /// all_applications false = none
    pub applications: Vec<String>,
    /// Where it sends: scheme and host only, never a path or query, which
    /// can carry a receiver's secret
    pub destination: String,
    /// ascender and awx: the job template (or workflow job template) it starts
    pub job_template: Option<u64>,
    pub workflow_job_template: Option<u64>,
    /// ascender and awx: through the template's webhook receiver or its launch API
    pub trigger: Option<JobTrigger>,
    pub credentials: Vec<String>,
    /// How long one attempt may take, and how many times a send is retried
    pub timeout_seconds: u64,
    pub retries: u32,
    /// webhook: its HTTP method, how the credential is sent, the names of
    /// its fixed headers (never their values), whether it sends a template or
    /// the default message, and the header its signature goes in, if signed
    pub webhook: Option<WebhookInfo>,
    /// ascender and awx, trigger launch: the names of the extra_vars it adds
    /// beside the event (never their values)
    pub extra_vars: Vec<String>,
    /// The last send since this process started, if any
    pub last: Option<Delivery>,
}

#[derive(Serialize, ToSchema)]
pub struct WebhookInfo {
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
    path = "/api/v1/notifications",
    tag = "Notifications",
    responses(
        (status = 200, description = "Every notification: its events, applications and destination, and its last send", body = Vec<NotificationInfo>),
        (status = 403, description = "API key is not admin", body = ErrorBody)
    )
)]
pub async fn list_notifications(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
) -> Result<Json<Vec<NotificationInfo>>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let mut out: Vec<NotificationInfo> = config
        .notifications
        .iter()
        .map(|(id, n)| info(&state, id, n))
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

// One notification as the list shows it.
fn info(
    state: &AppState,
    id: &str,
    n: &crate::domain::notification::Notification,
) -> NotificationInfo {
    let target = n.ascender.as_ref().or(n.awx.as_ref());
    let mut applications = n.applications.named().to_vec();
    applications.sort();
    NotificationInfo {
        name: id.to_string(),
        description: n.description.clone(),
        notification_type: n.notification_type,
        events: n.events.clone(),
        all_applications: n.applications.is_all(),
        applications,
        destination: n.destination(),
        job_template: target.and_then(|t| t.job_template),
        workflow_job_template: target.and_then(|t| t.workflow_job_template),
        trigger: target.map(|t| t.trigger),
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
        webhook: n.webhook.as_ref().map(|w| {
            use crate::domain::notification::{Auth, AuthScheme};
            let (auth, auth_header) = match &w.auth {
                Auth::Header { header } => ("header", Some(header.clone())),
                Auth::Scheme(AuthScheme::Basic) => ("basic", None),
                Auth::Scheme(AuthScheme::None) => ("none", None),
                Auth::Scheme(AuthScheme::Bearer) => ("bearer", None),
                // Left out: a bearer token when there is a credential.
                Auth::Default if w.credentials.is_empty() => ("none", None),
                Auth::Default => ("bearer", None),
            };
            WebhookInfo {
                method: w.method.clone(),
                auth: auth.to_string(),
                auth_header,
                headers: w.headers.keys().cloned().collect(),
                templated: w.body.is_some(),
                signature_header: w.sign.as_ref().map(|s| s.header.clone()),
            }
        }),
        extra_vars: target
            .map(|t| t.extra_vars.keys().cloned().collect())
            .unwrap_or_default(),
        last: state.notifier.log.last(id),
    }
}

#[derive(Serialize, ToSchema)]
pub struct NotificationDetail {
    #[serde(flatten)]
    pub notification: NotificationInfo,
    /// Its sends since this process started, newest first, at most 50.
    /// Retries of one message count as one send
    pub history: Vec<Delivery>,
}

#[utoipa::path(
    get,
    path = "/api/v1/notifications/{id}",
    tag = "Notifications",
    params(("id" = String, Path, description = "Notification name")),
    responses(
        (status = 200, description = "One notification, with its recent sends", body = NotificationDetail),
        (status = 403, description = "API key is not admin", body = ErrorBody),
        (status = 404, description = "No such notification", body = ErrorBody)
    )
)]
pub async fn get_notification(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    Path(id): Path<String>,
) -> Result<Json<NotificationDetail>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let Some(n) = config.notifications.get(&id) else {
        return Err(ApiError::not_found(format!(
            "notification '{id}' is not configured"
        )));
    };
    Ok(Json(NotificationDetail {
        notification: info(&state, &id, n),
        history: state.notifier.log.history(&id),
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/notifications/{id}/test",
    tag = "Notifications",
    params(("id" = String, Path, description = "Notification name")),
    responses(
        (status = 200, description = "Sent once, now, without retrying: the outcome, also kept as the notification's last send", body = Delivery),
        (status = 403, description = "API key is not admin", body = ErrorBody),
        (status = 404, description = "No such notification", body = ErrorBody)
    )
)]
pub async fn test_notification(
    State(state): State<Arc<AppState>>,
    axum::Extension(auth): axum::Extension<AuthContext>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Delivery>, ApiError> {
    if !auth.permissions.is_admin() {
        return Err(ApiError::admin_only());
    }
    let config = state.config();
    let Some(notification) = config.notifications.get(&id) else {
        return Err(ApiError::not_found(format!(
            "notification '{id}' is not configured"
        )));
    };
    let mut message = notify::test_message(&id, config.url.as_deref());
    message.insert("notification".into(), serde_json::json!(id));
    let delivery = notify::deliver(
        &*state.secrets,
        &*state.http,
        &state.notifier.log,
        Job {
            notification_id: id.clone(),
            notification: notification.clone(),
            message,
        },
        false,
    )
    .await;
    crate::adapters::r#in::http::audit::record(
        &auth,
        &headers,
        "notification_test",
        &id,
        if delivery.ok { "success" } else { "error" },
    );
    Ok(Json(delivery))
}
