use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use dashmap::DashMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};
use utoipa::ToSchema;

use crate::domain::notification::{
    Auth, AuthScheme, Event, JobTemplateTarget, JobTrigger, Notification, NotificationType,
    WebhookTarget,
};
use crate::domain::run::{Baseline, RunMode, RunRecord, RunResult, SyncStatus, Trigger, now};
use crate::ports::notifier::{HttpSenderPort, OutboundRequest, OutboundResponse};
use crate::ports::secrets::SecretsPort;

// The use case "tell someone what happened to an application".
//
// After every run, `detect` compares where the application stood before it
// (its Baseline) with what the run found and did, and names at most one
// event: one message per run, never "drift" and "synced" for the same
// change. Notifications that want that event are queued; a worker sends
// them, so a slow receiver never holds up a run.

// Why a sync changed something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    // A commit the system did not have yet.
    Commit,
    // Something changed on the system outside git, at a commit already applied.
    Drift,
    // An older commit, applied on purpose.
    Rollback,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Cause::Commit => "commit",
            Cause::Drift => "drift",
            Cause::Rollback => "rollback",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Detected {
    pub event: Event,
    pub cause: Option<Cause>,
    // drift_detected only: whether this same run put the system back.
    pub reverted: Option<bool>,
}

impl Detected {
    fn new(event: Event, cause: Option<Cause>, reverted: Option<bool>) -> Self {
        Self {
            event,
            cause,
            reverted,
        }
    }
}

// The event one run produced, if any. Events that describe a state
// (out_of_sync, sync_failed, back_in_sync, an unreverted drift) fire when the
// state is entered, not on every check that finds it still there; events that
// describe something done (sync_succeeded, a reverted drift) fire every time.
//
// A baseline of Unknown (first run since the process started: history is in
// memory) fires only failures and applied changes, so a restart does not
// report every pending difference again.
pub fn detect(before: &Baseline, record: &RunRecord, status: SyncStatus) -> Option<Detected> {
    let at_synced_commit = record.revision.is_some() && record.revision == before.synced_revision;

    if matches!(record.result, RunResult::Failed | RunResult::Partial)
        || status == SyncStatus::Failed
    {
        return (before.status != SyncStatus::Failed)
            .then(|| Detected::new(Event::SyncFailed, None, None));
    }

    let applied = record.applied.iter().filter(|a| a.ok).count();
    if record.mode == RunMode::Apply && applied > 0 {
        let cause = if record.trigger == Trigger::Rollback {
            Cause::Rollback
        } else if at_synced_commit {
            Cause::Drift
        } else {
            Cause::Commit
        };
        return Some(match cause {
            Cause::Drift => Detected::new(Event::DriftDetected, Some(cause), Some(true)),
            _ => Detected::new(Event::SyncSucceeded, Some(cause), None),
        });
    }

    match status {
        SyncStatus::OutOfSync if at_synced_commit => (before.status == SyncStatus::Synced)
            .then(|| Detected::new(Event::DriftDetected, Some(Cause::Drift), Some(false))),
        SyncStatus::OutOfSync => matches!(before.status, SyncStatus::Synced | SyncStatus::Failed)
            .then(|| Detected::new(Event::OutOfSync, Some(Cause::Commit), None)),
        SyncStatus::Synced => matches!(before.status, SyncStatus::Failed | SyncStatus::OutOfSync)
            .then(|| Detected::new(Event::BackInSync, None, None)),
        _ => None,
    }
}

// How many error lines a message carries, and how long each may be: enough
// to say why, not a script's whole stderr.
const MESSAGE_ERRORS: usize = 5;
const MESSAGE_ERROR_CHARS: usize = 500;

pub struct RunOutcome<'a> {
    pub application: &'a str,
    pub description: Option<&'a str>,
    // server.url, for a link to the application in the UI.
    pub base_url: Option<&'a str>,
    pub before: &'a Baseline,
    pub status: SyncStatus,
    pub record: &'a RunRecord,
    pub detected: &'a Detected,
}

// The default message: what happened, to which application, at which commit,
// and which objects changed (kind, key, operation and the NAMES of changed
// fields). Never a field's value: a value can be a secret, and the message
// leaves this process.
pub fn message(o: &RunOutcome<'_>) -> Map<String, Value> {
    let record = o.record;
    let fields_of = |kind: &str, key: &str| -> Vec<String> {
        record
            .operations
            .iter()
            .find(|op| op.kind == kind && op.key == key)
            .map(|op| op.changes.iter().map(|c| c.field.clone()).collect())
            .unwrap_or_default()
    };
    let changes: Vec<Value> = if record.mode == RunMode::Apply && !record.applied.is_empty() {
        record
            .applied
            .iter()
            .map(|a| {
                json!({
                    "kind": a.kind,
                    "key": a.key,
                    "op": a.op.as_str(),
                    "fields": fields_of(&a.kind, &a.key),
                    "ok": a.ok,
                })
            })
            .collect()
    } else {
        record
            .operations
            .iter()
            .map(|op| {
                json!({
                    "kind": op.kind,
                    "key": op.key,
                    "op": op.op.as_str(),
                    "fields": op.changes.iter().map(|c| c.field.clone()).collect::<Vec<_>>(),
                })
            })
            .collect()
    };
    let errors: Vec<String> = record
        .errors
        .iter()
        .take(MESSAGE_ERRORS)
        .map(|e| e.chars().take(MESSAGE_ERROR_CHARS).collect())
        .collect();
    let mut m = Map::new();
    m.insert("event".into(), json!(o.detected.event.as_str()));
    m.insert("application".into(), json!(o.application));
    m.insert("description".into(), json!(o.description));
    m.insert("status".into(), json!(o.status));
    m.insert("previous_status".into(), json!(o.before.status));
    m.insert("cause".into(), json!(o.detected.cause.map(Cause::as_str)));
    m.insert("reverted".into(), json!(o.detected.reverted));
    m.insert("revision".into(), json!(record.revision));
    m.insert(
        "applied".into(),
        json!(record.applied.iter().filter(|a| a.ok).count()),
    );
    m.insert("changes".into(), Value::Array(changes));
    m.insert("errors".into(), json!(errors));
    m.insert("at".into(), json!(rfc3339(record.finished_at)));
    m.insert(
        "url".into(),
        json!(
            o.base_url
                .map(|base| format!("{base}/applications/{}", o.application))
        ),
    );
    m
}

// The message a "Send test" sends: the real shape, marked as a test.
pub fn test_message(notification_id: &str, base_url: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("event".into(), json!("test"));
    m.insert("application".into(), Value::Null);
    m.insert(
        "description".into(),
        json!(format!("Test of notification {notification_id}")),
    );
    for key in [
        "status",
        "previous_status",
        "cause",
        "reverted",
        "revision",
        "url",
    ] {
        m.insert(key.into(), Value::Null);
    }
    m.insert("applied".into(), json!(0));
    m.insert("changes".into(), json!([]));
    m.insert("errors".into(), json!([]));
    m.insert("at".into(), json!(rfc3339(now())));
    if let Some(base) = base_url {
        m.insert("url".into(), json!(base));
    }
    m
}

fn rfc3339(unix_seconds: u64) -> String {
    chrono::DateTime::from_timestamp(unix_seconds as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

// Fills {{field}} placeholders from the message. Text is written escaped for
// a JSON string (the template writes the quotes); lists, numbers and
// booleans as JSON; an absent value as nothing.
pub fn render(template: &str, message: &Map<String, Value>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(len) = rest[start + 2..].find("}}") else {
            break;
        };
        out.push_str(&rest[..start]);
        let name = rest[start + 2..start + 2 + len].trim();
        out.push_str(&placeholder_value(name, message));
        rest = &rest[start + 2 + len + 2..];
    }
    out.push_str(rest);
    out
}

fn placeholder_value(name: &str, message: &Map<String, Value>) -> String {
    let value = if name == "revision_short" {
        message
            .get("revision")
            .and_then(Value::as_str)
            .map(|r| Value::String(r.chars().take(7).collect()))
            .unwrap_or(Value::Null)
    } else {
        message.get(name).cloned().unwrap_or(Value::Null)
    };
    match value {
        Value::Null => String::new(),
        Value::String(s) => {
            let quoted = Value::String(s).to_string();
            quoted[1..quoted.len() - 1].to_string()
        }
        other => other.to_string(),
    }
}

// The request one notification sends for one message. Pure: the credentials
// arrive resolved, and nothing here touches the network.
pub fn build_request(
    notification: &Notification,
    message: &Map<String, Value>,
    credentials: &HashMap<String, String>,
) -> Result<OutboundRequest, String> {
    match notification.notification_type {
        NotificationType::Webhook => {
            let target = notification
                .webhook
                .as_ref()
                .ok_or("type webhook needs a 'webhook:' block")?;
            webhook_request(target, message, credentials)
        }
        NotificationType::Ascender | NotificationType::Awx => {
            let target = notification
                .ascender
                .as_ref()
                .or(notification.awx.as_ref())
                .ok_or("a job template notification needs its block")?;
            job_template_request(target, message, credentials)
        }
    }
}

fn base_headers(event: &str) -> Vec<(String, String)> {
    vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "User-Agent".into(),
            format!("unified-gitops/{}", env!("CARGO_PKG_VERSION")),
        ),
        ("X-Unified-Gitops-Event".into(), event.to_string()),
    ]
}

fn field<'a>(credentials: &'a HashMap<String, String>, name: &str) -> Result<&'a str, String> {
    credentials
        .get(name)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("the credential has no '{name}' field"))
}

fn webhook_request(
    target: &WebhookTarget,
    message: &Map<String, Value>,
    credentials: &HashMap<String, String>,
) -> Result<OutboundRequest, String> {
    let body = match &target.body {
        Some(template) => render(template, message).into_bytes(),
        None => serde_json::to_vec(message).map_err(|e| e.to_string())?,
    };
    let event = message.get("event").and_then(Value::as_str).unwrap_or("");
    let mut headers = base_headers(event);
    for (name, value) in &target.headers {
        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        headers.push((name.clone(), value.clone()));
    }
    match &target.auth {
        Auth::Scheme(AuthScheme::Bearer) => headers.push((
            "Authorization".into(),
            format!("Bearer {}", field(credentials, "token")?),
        )),
        Auth::Default => {
            if let Some(token) = credentials.get("token").filter(|t| !t.is_empty()) {
                headers.push(("Authorization".into(), format!("Bearer {token}")));
            }
        }
        Auth::Scheme(AuthScheme::Basic) => {
            let pair = format!(
                "{}:{}",
                field(credentials, "username")?,
                field(credentials, "password")?
            );
            headers.push((
                "Authorization".into(),
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(pair)
                ),
            ));
        }
        Auth::Header { header } => {
            headers.push((header.clone(), field(credentials, "token")?.to_string()));
        }
        Auth::Scheme(AuthScheme::None) => {}
    }
    if let Some(sign) = &target.sign {
        let secret = field(credentials, &sign.secret_field)?;
        let digest = hmac_sha256(secret.as_bytes(), &body);
        headers.push((
            sign.header.clone(),
            format!("{}{}", sign.prefix, hex(&digest)),
        ));
    }
    Ok(OutboundRequest {
        method: target.method.clone(),
        url: target.url.clone(),
        headers,
        body,
        timeout: Duration::from_secs(target.timeout_seconds),
    })
}

// Ascender and AWX speak the same routes. `webhook` posts the message to the
// template's own receiver in GitLab mode (its key, compared as is, and any
// event name; the playbook reads the message as <prefix>_webhook_payload).
// `launch` starts it as a user, the message under extra_vars.unified_gitops.
fn job_template_request(
    target: &JobTemplateTarget,
    message: &Map<String, Value>,
    credentials: &HashMap<String, String>,
) -> Result<OutboundRequest, String> {
    let token = field(credentials, "token")?;
    let event = message.get("event").and_then(Value::as_str).unwrap_or("");
    let mut headers = base_headers(event);
    let body = match target.trigger {
        JobTrigger::Webhook => {
            headers.push(("X-Gitlab-Token".into(), token.to_string()));
            headers.push(("X-Gitlab-Event".into(), format!("unified-gitops {event}")));
            serde_json::to_vec(message).map_err(|e| e.to_string())?
        }
        JobTrigger::Launch => {
            headers.push(("Authorization".into(), format!("Bearer {token}")));
            let mut extra_vars = target.extra_vars.clone();
            extra_vars.insert("unified_gitops".into(), Value::Object(message.clone()));
            serde_json::to_vec(&json!({ "extra_vars": extra_vars })).map_err(|e| e.to_string())?
        }
    };
    Ok(OutboundRequest {
        method: "POST".into(),
        url: target.endpoint(),
        headers,
        body,
        timeout: Duration::from_secs(target.timeout_seconds),
    })
}

// What an answer means for this receiver: Ok with the job it started, if it
// says; Err with why it did not do what was asked.
pub fn interpret(
    notification: &Notification,
    response: &OutboundResponse,
) -> Result<Option<String>, String> {
    if !(200..300).contains(&response.status) {
        let excerpt: String = response.body.chars().take(300).collect();
        return Err(format!("HTTP {}: {}", response.status, excerpt.trim()));
    }
    let target = notification.ascender.as_ref().or(notification.awx.as_ref());
    let Some(target) = target else {
        return Ok(None);
    };
    let answer: Value = serde_json::from_str(&response.body).unwrap_or(Value::Null);
    match target.trigger {
        JobTrigger::Launch => {
            // Without "Prompt on launch" for variables (or a survey), the
            // controller drops extra_vars and says so only here: the job
            // would run without the event.
            if answer
                .get("ignored_fields")
                .and_then(|f| f.get("extra_vars"))
                .is_some()
            {
                return Err(
                    "the job template ignored extra_vars: tick \"Prompt on launch\" \
                     for its variables"
                        .to_string(),
                );
            }
            Ok(answer
                .get("job")
                .or_else(|| answer.get("workflow_job"))
                .or_else(|| answer.get("id"))
                .map(|id| id.to_string()))
        }
        JobTrigger::Webhook => Ok(None),
    }
}

// Worth sending again: no answer at all, or a receiver that is overloaded or
// failing. A 4xx is this side's mistake (a wrong key, a wrong template) and
// will not fix itself.
fn retryable(result: &Result<OutboundResponse, String>) -> bool {
    match result {
        Err(_) => true,
        Ok(response) => response.status == 429 || response.status >= 500,
    }
}

// HMAC-SHA256 (RFC 2104) over sha2, already in the tree for config ETags.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        padded[..digest.len()].copy_from_slice(&digest);
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let inner_key: Vec<u8> = padded.iter().map(|b| b ^ 0x36).collect();
    let outer_key: Vec<u8> = padded.iter().map(|b| b ^ 0x5c).collect();
    let inner = Sha256::new()
        .chain_update(&inner_key)
        .chain_update(message)
        .finalize();
    let outer = Sha256::new()
        .chain_update(&outer_key)
        .chain_update(inner)
        .finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&outer);
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// One send of a notification, for the API and the UI.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Delivery {
    pub event: String,
    pub application: Option<String>,
    // Unix seconds.
    pub at: u64,
    pub ok: bool,
    pub attempts: u32,
    // The receiver's HTTP status, when it answered.
    pub status: Option<u16>,
    // The job the controller started, when it says (launch).
    pub job: Option<String>,
    pub error: Option<String>,
}

// How many sends each notification keeps. In memory, like run history: a
// restart starts it over.
pub const DELIVERY_HISTORY: usize = 50;

// Each notification's recent sends, newest first.
#[derive(Default)]
pub struct DeliveryLog {
    sends: DashMap<String, std::collections::VecDeque<Delivery>>,
}

impl DeliveryLog {
    pub fn last(&self, notification_id: &str) -> Option<Delivery> {
        self.sends
            .get(notification_id)
            .and_then(|h| h.front().cloned())
    }

    pub fn history(&self, notification_id: &str) -> Vec<Delivery> {
        self.sends
            .get(notification_id)
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn record(&self, notification_id: &str, delivery: Delivery) {
        let mut history = self.sends.entry(notification_id.to_string()).or_default();
        history.push_front(delivery);
        history.truncate(DELIVERY_HISTORY);
    }
}

// One message for one notification, its definition as of the run that made
// it: a reload while it waits does not change where it goes.
pub struct Job {
    pub notification_id: String,
    pub notification: Notification,
    pub message: Map<String, Value>,
}

// Bounded: a receiver that is down for hours must not grow memory without
// limit. A full queue drops the new message, counted and logged.
const QUEUE: usize = 1000;
// Sends in flight at once, so one slow receiver does not hold up the others.
const CONCURRENT: usize = 8;
// Longest wait between two attempts.
const MAX_BACKOFF_SECONDS: u64 = 60;

pub struct Dispatcher {
    tx: mpsc::Sender<Job>,
    // Taken by start(); None once the worker runs.
    rx: std::sync::Mutex<Option<mpsc::Receiver<Job>>>,
    pub log: Arc<DeliveryLog>,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(QUEUE);
        Self {
            tx,
            rx: std::sync::Mutex::new(Some(rx)),
            log: Arc::new(DeliveryLog::default()),
        }
    }

    pub fn enqueue(&self, job: Job) {
        if let Err(e) = self.tx.try_send(job) {
            let job = match e {
                mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job) => {
                    job
                }
            };
            let event = job
                .message
                .get("event")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            tracing::warn!(notification = %job.notification_id, event = %event, "notification queue full; message dropped");
            count(&job.notification_id, &event, "dropped");
        }
    }

    // Starts the worker that sends what is queued. Once: later calls do
    // nothing. Separate from construction so building the app needs no
    // runtime; main starts it beside the scheduler, and tests that send do.
    pub fn start(&self, secrets: Arc<dyn SecretsPort>, http: Arc<dyn HttpSenderPort>) {
        let Some(mut rx) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let log = Arc::clone(&self.log);
        tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(CONCURRENT));
            while let Some(job) = rx.recv().await {
                let Ok(slot) = Arc::clone(&slots).acquire_owned().await else {
                    return;
                };
                let (secrets, http, log) =
                    (Arc::clone(&secrets), Arc::clone(&http), Arc::clone(&log));
                tokio::spawn(async move {
                    let _slot = slot;
                    deliver(&*secrets, &*http, &log, job, true).await;
                });
            }
        });
    }
}

// Sends one message, with retries when `retry` is set, records the outcome
// and returns it. The worker's path, and the "Send test" button's (which
// answers at once, without retrying).
pub async fn deliver(
    secrets: &dyn SecretsPort,
    http: &dyn HttpSenderPort,
    log: &DeliveryLog,
    job: Job,
    retry: bool,
) -> Delivery {
    let started = Instant::now();
    let event = job
        .message
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let application = job
        .message
        .get("application")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut delivery = Delivery {
        event: event.clone(),
        application,
        at: now(),
        ok: false,
        attempts: 0,
        status: None,
        job: None,
        error: None,
    };

    let request = match resolve(secrets, job.notification.credentials()).await {
        Ok(credentials) => build_request(&job.notification, &job.message, &credentials),
        Err(e) => Err(e),
    };
    match request {
        Err(e) => delivery.error = Some(e),
        Ok(request) => {
            let retries = if retry {
                retries_of(&job.notification)
            } else {
                0
            };
            loop {
                delivery.attempts += 1;
                let result = http.send(request.clone()).await;
                delivery.status = result.as_ref().ok().map(|r| r.status);
                let again = retryable(&result);
                match result.and_then(|response| interpret(&job.notification, &response)) {
                    Ok(started_job) => {
                        delivery.ok = true;
                        delivery.job = started_job;
                        delivery.error = None;
                        break;
                    }
                    Err(e) => delivery.error = Some(e),
                }
                if !again || delivery.attempts > retries {
                    break;
                }
                let wait = (1u64 << delivery.attempts.min(6)).min(MAX_BACKOFF_SECONDS);
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }

    let outcome = if delivery.ok { "success" } else { "error" };
    count(&job.notification_id, &event, outcome);
    metrics::histogram!(
        "unified_gitops_notification_duration_seconds",
        "notification" => job.notification_id.clone()
    )
    .record(started.elapsed().as_secs_f64());
    if delivery.ok {
        tracing::info!(notification = %job.notification_id, event = %event, attempts = delivery.attempts, "notification sent");
    } else {
        tracing::warn!(
            notification = %job.notification_id,
            event = %event,
            attempts = delivery.attempts,
            error = delivery.error.as_deref().unwrap_or(""),
            "notification failed"
        );
    }
    log.record(&job.notification_id, delivery.clone());
    delivery
}

fn retries_of(notification: &Notification) -> u32 {
    notification
        .webhook
        .as_ref()
        .map(|w| w.retries)
        .or_else(|| notification.ascender.as_ref().map(|t| t.retries))
        .or_else(|| notification.awx.as_ref().map(|t| t.retries))
        .unwrap_or(0)
}

async fn resolve(
    secrets: &dyn SecretsPort,
    ids: &[String],
) -> Result<HashMap<String, String>, String> {
    crate::application::credentials::resolve_credentials(secrets, ids)
        .await
        .map_err(|e| e.message)
}

fn count(notification_id: &str, event: &str, outcome: &'static str) {
    metrics::counter!(
        "unified_gitops_notifications_total",
        "notification" => notification_id.to_string(),
        "event" => event.to_string(),
        "outcome" => outcome
    )
    .increment(1);
}

// After a run: names its event, if any, and queues it for every notification
// that wants it from this application.
pub fn after_run(
    dispatcher: &Dispatcher,
    notifications: &HashMap<String, Notification>,
    outcome: &RunOutcome<'_>,
) {
    if notifications.is_empty() {
        return;
    }
    let event = outcome.detected.event;
    let mut wanted: Vec<(&String, &Notification)> = notifications
        .iter()
        .filter(|(_, n)| n.wants(event, outcome.application))
        .collect();
    if wanted.is_empty() {
        return;
    }
    wanted.sort_by(|a, b| a.0.cmp(b.0));
    let base = message(outcome);
    for (id, notification) in wanted {
        let mut m = base.clone();
        m.insert("notification".into(), json!(id));
        dispatcher.enqueue(Job {
            notification_id: id.clone(),
            notification: notification.clone(),
            message: m,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::plan::{Change, OpKind, Operation};
    use crate::domain::run::AppliedOperation;

    fn baseline(status: SyncStatus, synced: Option<&str>) -> Baseline {
        Baseline {
            status,
            synced_revision: synced.map(str::to_string),
        }
    }

    fn record(mode: RunMode, trigger: Trigger, result: RunResult, revision: &str) -> RunRecord {
        RunRecord {
            id: 1,
            trigger,
            mode,
            result,
            started_at: 0,
            finished_at: 1_700_000_000,
            revision: Some(revision.to_string()),
            planned: 0,
            operations: Vec::new(),
            applied: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn applied(ok: bool) -> AppliedOperation {
        AppliedOperation {
            kind: "user".into(),
            key: "jane".into(),
            op: OpKind::Update,
            ok,
            error: None,
        }
    }

    fn event(d: Option<Detected>) -> Option<(Event, Option<Cause>, Option<bool>)> {
        d.map(|d| (d.event, d.cause, d.reverted))
    }

    #[test]
    fn a_change_from_git_applied_is_sync_succeeded_with_cause_commit() {
        let mut r = record(RunMode::Apply, Trigger::Schedule, RunResult::Succeeded, "b");
        r.applied.push(applied(true));
        let d = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &r,
            SyncStatus::Synced,
        );
        assert_eq!(
            event(d),
            Some((Event::SyncSucceeded, Some(Cause::Commit), None))
        );
    }

    #[test]
    fn a_change_applied_at_the_synced_commit_is_reverted_drift_only() {
        let mut r = record(RunMode::Apply, Trigger::Schedule, RunResult::Succeeded, "a");
        r.applied.push(applied(true));
        let d = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &r,
            SyncStatus::Synced,
        );
        assert_eq!(
            event(d),
            Some((Event::DriftDetected, Some(Cause::Drift), Some(true)))
        );
    }

    #[test]
    fn drift_found_without_auto_sync_fires_once() {
        let r = record(RunMode::Plan, Trigger::Schedule, RunResult::OutOfSync, "a");
        let first = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &r,
            SyncStatus::OutOfSync,
        );
        assert_eq!(
            event(first),
            Some((Event::DriftDetected, Some(Cause::Drift), Some(false)))
        );
        let again = detect(
            &baseline(SyncStatus::OutOfSync, Some("a")),
            &r,
            SyncStatus::OutOfSync,
        );
        assert_eq!(again, None, "still drifted: not a new event");
    }

    #[test]
    fn a_new_commit_waiting_is_out_of_sync_once() {
        let r = record(RunMode::Plan, Trigger::Schedule, RunResult::OutOfSync, "b");
        let first = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &r,
            SyncStatus::OutOfSync,
        );
        assert_eq!(
            event(first),
            Some((Event::OutOfSync, Some(Cause::Commit), None))
        );
        let again = detect(
            &baseline(SyncStatus::OutOfSync, Some("a")),
            &r,
            SyncStatus::OutOfSync,
        );
        assert_eq!(again, None);
    }

    #[test]
    fn failures_fire_on_entering_the_failed_state() {
        let r = record(RunMode::Apply, Trigger::Schedule, RunResult::Failed, "a");
        let first = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &r,
            SyncStatus::Failed,
        );
        assert_eq!(event(first), Some((Event::SyncFailed, None, None)));
        let again = detect(
            &baseline(SyncStatus::Failed, Some("a")),
            &r,
            SyncStatus::Failed,
        );
        assert_eq!(again, None);
        let mut partial = record(RunMode::Apply, Trigger::Schedule, RunResult::Partial, "b");
        partial.applied.push(applied(true));
        partial.applied.push(applied(false));
        let d = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &partial,
            SyncStatus::Failed,
        );
        assert_eq!(
            event(d),
            Some((Event::SyncFailed, None, None)),
            "partial is a failure, once"
        );
    }

    #[test]
    fn recovering_is_back_in_sync() {
        let r = record(RunMode::Plan, Trigger::Schedule, RunResult::Succeeded, "a");
        let d = detect(
            &baseline(SyncStatus::Failed, Some("a")),
            &r,
            SyncStatus::Synced,
        );
        assert_eq!(event(d), Some((Event::BackInSync, None, None)));
        let steady = detect(
            &baseline(SyncStatus::Synced, Some("a")),
            &r,
            SyncStatus::Synced,
        );
        assert_eq!(steady, None);
    }

    #[test]
    fn after_a_restart_only_failures_and_applied_changes_fire() {
        let unknown = baseline(SyncStatus::Unknown, None);
        let pending = record(RunMode::Plan, Trigger::Schedule, RunResult::OutOfSync, "a");
        assert_eq!(detect(&unknown, &pending, SyncStatus::OutOfSync), None);
        let synced = record(RunMode::Plan, Trigger::Schedule, RunResult::Succeeded, "a");
        assert_eq!(detect(&unknown, &synced, SyncStatus::Synced), None);
        let failed = record(RunMode::Plan, Trigger::Schedule, RunResult::Failed, "a");
        assert!(detect(&unknown, &failed, SyncStatus::Failed).is_some());
    }

    #[test]
    fn a_rollback_applied_is_sync_succeeded_with_cause_rollback() {
        let mut r = record(
            RunMode::Apply,
            Trigger::Rollback,
            RunResult::Succeeded,
            "old",
        );
        r.applied.push(applied(true));
        let d = detect(
            &baseline(SyncStatus::Synced, Some("new")),
            &r,
            SyncStatus::Synced,
        );
        assert_eq!(
            event(d),
            Some((Event::SyncSucceeded, Some(Cause::Rollback), None))
        );
    }

    fn outcome_message() -> Map<String, Value> {
        let mut r = record(
            RunMode::Apply,
            Trigger::Schedule,
            RunResult::Succeeded,
            "0123456789abcdef",
        );
        r.operations.push(Operation {
            kind: "user".into(),
            key: "jane".into(),
            op: OpKind::Update,
            object: json!({"Username": "jane", "Password": "hunter2"}),
            changes: vec![Change {
                field: "Password".into(),
                from: json!("old-secret"),
                to: json!("hunter2"),
            }],
            file: None,
        });
        r.applied.push(applied(true));
        let before = baseline(SyncStatus::Synced, Some("a"));
        let detected = Detected::new(Event::SyncSucceeded, Some(Cause::Commit), None);
        message(&RunOutcome {
            application: "portainer",
            description: Some("Portainer"),
            base_url: Some("https://gitops.example.com"),
            before: &before,
            status: SyncStatus::Synced,
            record: &r,
            detected: &detected,
        })
    }

    #[test]
    fn a_message_names_changed_fields_but_never_their_values() {
        let m = outcome_message();
        let text = serde_json::to_string(&m).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("old-secret"),
            "{text}"
        );
        assert_eq!(m["changes"][0]["fields"], json!(["Password"]));
        assert_eq!(m["cause"], json!("commit"));
        assert_eq!(m["applied"], json!(1));
        assert_eq!(m["at"], json!("2023-11-14T22:13:20Z"));
        assert_eq!(
            m["url"],
            json!("https://gitops.example.com/applications/portainer")
        );
    }

    #[test]
    fn render_escapes_text_and_writes_json_for_lists() {
        let mut m = outcome_message();
        m.insert("description".into(), json!("say \"hi\""));
        let body = render(
            r#"{"d": "{{description}}", "c": {{changes}}, "r": "{{revision_short}}", "x": "{{reverted}}"}"#,
            &m,
        );
        let parsed: Value = serde_json::from_str(&body).expect(&body);
        assert_eq!(parsed["d"], json!("say \"hi\""));
        assert_eq!(parsed["c"][0]["key"], json!("jane"));
        assert_eq!(parsed["r"], json!("0123456"));
        assert_eq!(parsed["x"], json!(""));
    }

    fn notification(yaml: &str) -> Notification {
        serde_saphyr::from_str(yaml).unwrap()
    }

    fn header<'a>(request: &'a OutboundRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn a_webhook_puts_the_credential_where_auth_says() {
        let creds = HashMap::from([("token".to_string(), "t0k".to_string())]);
        let m = outcome_message();
        let bearer = notification(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n",
        );
        let request = build_request(&bearer, &m, &creds).unwrap();
        assert_eq!(header(&request, "Authorization"), Some("Bearer t0k"));
        assert_eq!(header(&request, "Content-Type"), Some("application/json"));
        let parsed: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(parsed["application"], json!("portainer"));

        let named = notification(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n  auth: {header: X-Gitlab-Token}\n  headers: {X-Gitlab-Event: unified-gitops}\n",
        );
        let request = build_request(&named, &m, &creds).unwrap();
        assert_eq!(header(&request, "X-Gitlab-Token"), Some("t0k"));
        assert_eq!(header(&request, "Authorization"), None);
        assert_eq!(header(&request, "X-Gitlab-Event"), Some("unified-gitops"));

        let basic = notification(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n  auth: basic\n",
        );
        let err = build_request(&basic, &m, &creds).unwrap_err();
        assert!(err.contains("'username'"), "{err}");
    }

    #[test]
    fn a_signed_webhook_carries_the_hmac_of_its_body() {
        let creds = HashMap::from([("secret".to_string(), "Jefe".to_string())]);
        let signed = notification(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n  auth: none\n  body: 'what do ya want for nothing?'\n  sign: {}\n",
        );
        let request = build_request(&signed, &outcome_message(), &creds).unwrap();
        // RFC 4231, test case 2.
        assert_eq!(
            header(&request, "X-Hub-Signature-256"),
            Some("sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843")
        );
    }

    #[test]
    fn a_job_template_webhook_uses_the_gitlab_receiver_and_its_key() {
        let creds = HashMap::from([("token".to_string(), "jt-key".to_string())]);
        let n = notification(
            "type: ascender\nevents: [drift_detected]\nascender:\n  url: https://ascender.example.com\n  job_template: 12\n  credentials: [k]\n",
        );
        let request = build_request(&n, &outcome_message(), &creds).unwrap();
        assert_eq!(
            request.url,
            "https://ascender.example.com/api/v2/job_templates/12/gitlab/"
        );
        assert_eq!(header(&request, "X-Gitlab-Token"), Some("jt-key"));
        assert_eq!(
            header(&request, "X-Gitlab-Event"),
            Some("unified-gitops sync_succeeded")
        );
        assert_eq!(header(&request, "Authorization"), None);
        let parsed: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(parsed["event"], json!("sync_succeeded"));
    }

    #[test]
    fn a_launch_sends_the_message_under_extra_vars() {
        let creds = HashMap::from([("token".to_string(), "user-token".to_string())]);
        let n = notification(
            "type: awx\nevents: [drift_detected]\nawx:\n  url: https://awx.example.com\n  job_template: 5\n  trigger: launch\n  credentials: [t]\n  extra_vars: {report: drift}\n",
        );
        let request = build_request(&n, &outcome_message(), &creds).unwrap();
        assert_eq!(
            request.url,
            "https://awx.example.com/api/v2/job_templates/5/launch/"
        );
        assert_eq!(header(&request, "Authorization"), Some("Bearer user-token"));
        let parsed: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(parsed["extra_vars"]["report"], json!("drift"));
        assert_eq!(
            parsed["extra_vars"]["unified_gitops"]["application"],
            json!("portainer")
        );
    }

    #[test]
    fn a_launch_whose_variables_were_ignored_is_a_failure() {
        let n = notification(
            "type: ascender\nevents: [drift_detected]\nascender:\n  url: https://a.example.com\n  job_template: 5\n  trigger: launch\n  credentials: [t]\n",
        );
        let ignored = OutboundResponse {
            status: 201,
            body: r#"{"job": 77, "ignored_fields": {"extra_vars": {"unified_gitops": {}}}}"#.into(),
        };
        assert!(
            interpret(&n, &ignored)
                .unwrap_err()
                .contains("Prompt on launch")
        );
        let started = OutboundResponse {
            status: 201,
            body: r#"{"job": 77, "id": 77}"#.into(),
        };
        assert_eq!(interpret(&n, &started).unwrap(), Some("77".to_string()));
        let refused = OutboundResponse {
            status: 403,
            body: r#"{"detail": "You do not have permission"}"#.into(),
        };
        assert!(interpret(&n, &refused).unwrap_err().starts_with("HTTP 403"));
    }

    #[test]
    fn the_log_keeps_the_newest_sends_first_and_bounded() {
        let log = DeliveryLog::default();
        for i in 0..(DELIVERY_HISTORY as u64 + 5) {
            log.record(
                "n",
                Delivery {
                    event: "test".into(),
                    application: None,
                    at: i,
                    ok: true,
                    attempts: 1,
                    status: Some(202),
                    job: None,
                    error: None,
                },
            );
        }
        let history = log.history("n");
        assert_eq!(history.len(), DELIVERY_HISTORY);
        assert_eq!(history[0].at, DELIVERY_HISTORY as u64 + 4);
        assert_eq!(log.last("n").unwrap().at, history[0].at);
        assert!(log.history("other").is_empty());
    }

    #[test]
    fn only_unanswered_or_server_failures_are_retried() {
        assert!(retryable(&Err("timed out".into())));
        assert!(retryable(&Ok(OutboundResponse {
            status: 503,
            body: String::new()
        })));
        assert!(retryable(&Ok(OutboundResponse {
            status: 429,
            body: String::new()
        })));
        assert!(!retryable(&Ok(OutboundResponse {
            status: 401,
            body: String::new()
        })));
    }
}
