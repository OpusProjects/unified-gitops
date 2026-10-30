use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use dashmap::DashMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};
use utoipa::ToSchema;

use crate::domain::unix_now as now;
use crate::domain::webhook::{
    Auth, AuthScheme, Event, HttpTarget, JobTemplateTarget, Webhook, WebhookType,
};
use crate::ports::git::CommitRange;
use crate::ports::http_sender::{HttpSenderPort, OutboundRequest, OutboundResponse, SendError};
use crate::ports::secrets::SecretsPort;

// The use case "tell someone what happened to the checkout of a project".
//
// After every pull, the project's event (if any) is queued for the webhooks
// that want it; a worker sends them, so a slow receiver never holds up a
// pull.

// How much of a failure a message carries.
const MESSAGE_ERROR_CHARS: usize = 500;

// The message a "Send test" sends: the real shape, marked as a test.
pub fn test_message(webhook_id: &str, base_url: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("event".into(), json!("test"));
    m.insert("project".into(), Value::Null);
    m.insert(
        "description".into(),
        json!(format!("Test of webhook {webhook_id}")),
    );
    for key in ["revision", "url"] {
        m.insert(key.into(), Value::Null);
    }
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
    use crate::domain::webhook::{TemplatePart, template_parts};
    let parts: Vec<TemplatePart<'_>> = template_parts(template).collect();
    let mut out = String::with_capacity(template.len());
    for (i, part) in parts.iter().enumerate() {
        match part {
            TemplatePart::Text(text) => out.push_str(text),
            TemplatePart::Field(name) => {
                // Inside quotes, an absent value is an empty string; outside
                // them, JSON's null, so `{"reverted": {{reverted}}}` stays
                // valid when there is nothing to say.
                let quoted = out.ends_with('"')
                    && matches!(parts.get(i + 1), Some(TemplatePart::Text(t)) if t.starts_with('"'));
                out.push_str(&placeholder_value(name, message, quoted));
            }
        }
    }
    out
}

fn placeholder_value(name: &str, message: &Map<String, Value>, quoted: bool) -> String {
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
        Value::Null if quoted => String::new(),
        Value::Null => "null".to_string(),
        Value::String(s) => {
            let quoted = Value::String(s).to_string();
            quoted[1..quoted.len() - 1].to_string()
        }
        other => other.to_string(),
    }
}

// The request one webhook sends for one message. Pure: the credentials
// arrive resolved, and nothing here touches the network.
pub fn build_request(
    webhook: &Webhook,
    message: &Map<String, Value>,
    credentials: &HashMap<String, String>,
) -> Result<OutboundRequest, String> {
    let credentials = Fields {
        values: credentials,
        credential_ids: webhook.credentials(),
    };
    match webhook.webhook_type {
        WebhookType::Webhook => {
            let target = webhook
                .webhook
                .as_ref()
                .ok_or("type webhook needs a 'webhook:' block")?;
            webhook_request(target, message, &credentials)
        }
        WebhookType::Ascender | WebhookType::Awx => {
            let target = webhook
                .ascender
                .as_ref()
                .or(webhook.awx.as_ref())
                .ok_or("a job template webhook needs its block")?;
            job_template_request(target, message, &credentials)
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

// The webhook's resolved credentials, read by bare field name. A bare name
// is delivered only when exactly one of the credentials has the field
// (application::credentials), so a field two of them share is absent here
// though each has it: that is told apart from a field none has, which would
// otherwise read as "the credential has no 'token' field" about a token that
// is plainly configured. Refused at load where the configuration names the
// fields; a whole Vault secret or JSON file is only known here.
struct Fields<'a> {
    values: &'a HashMap<String, String>,
    credential_ids: &'a [String],
}

impl<'a> Fields<'a> {
    // The field, possibly empty; None when no credential has it.
    fn optional(&self, name: &str) -> Result<Option<&'a str>, String> {
        if let Some(value) = self.values.get(name) {
            return Ok(Some(value));
        }
        use crate::domain::credential::{delivery_id, variable_part};
        let mut holders: Vec<&String> = Vec::new();
        for id in self.credential_ids {
            // In variable form, as the merge compares them: a `Token` field
            // is delivered as `<id>_Token` and shared with a `token` all the
            // same.
            let under_id = variable_part(&format!("{}_{name}", delivery_id(id)));
            if !holders.contains(&id) && self.values.keys().any(|k| variable_part(k) == under_id) {
                holders.push(id);
            }
        }
        if holders.len() > 1 {
            let named: Vec<String> = holders.iter().map(|id| format!("'{id}'")).collect();
            return Err(format!(
                "credentials {} all have a '{name}' field, so it is delivered under neither's \
                 bare name: keep '{name}' in one of them",
                named.join(", ")
            ));
        }
        Ok(None)
    }

    // The field, which must be there and not empty.
    fn required(&self, name: &str) -> Result<&'a str, String> {
        self.optional(name)?
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("the credential has no '{name}' field"))
    }
}

// Sets a header, replacing any of the same name (headers are not case
// sensitive): one header, one value.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value));
}

// A header value HTTP can carry: no line breaks or other control characters,
// which a secret read from a file often ends with. Refused here, before the
// request: it is the configuration's to fix, and retrying would not.
fn check_headers(headers: &[(String, String)]) -> Result<(), String> {
    for (name, value) in headers {
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(format!(
                "the value for header '{name}' has a line break or a control character \
                 (a credential read from a file with a trailing newline?)"
            ));
        }
    }
    Ok(())
}

fn webhook_request(
    target: &HttpTarget,
    message: &Map<String, Value>,
    credentials: &Fields,
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
        Auth::Scheme(AuthScheme::Bearer) => set_header(
            &mut headers,
            "Authorization",
            format!("Bearer {}", credentials.required("token")?),
        ),
        Auth::Default => {
            if let Some(token) = credentials.optional("token")?.filter(|t| !t.is_empty()) {
                set_header(&mut headers, "Authorization", format!("Bearer {token}"));
            }
        }
        Auth::Scheme(AuthScheme::Basic) => {
            // An empty password is allowed: some APIs take a key as the
            // username and nothing after the colon.
            let password = credentials.optional("password")?.unwrap_or("");
            let pair = format!("{}:{password}", credentials.required("username")?);
            set_header(
                &mut headers,
                "Authorization",
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(pair)
                ),
            );
        }
        Auth::Header(crate::domain::webhook::HeaderAuth { header }) => {
            set_header(
                &mut headers,
                header,
                credentials.required("token")?.to_string(),
            );
        }
        Auth::Scheme(AuthScheme::None) => {}
    }
    if let Some(sign) = &target.sign {
        let secret = credentials.required(&sign.secret_field)?;
        let digest = hmac_sha256(secret.as_bytes(), &body);
        set_header(
            &mut headers,
            &sign.header,
            format!("{}{}", sign.prefix, hex(&digest)),
        );
    }
    check_headers(&headers)?;
    Ok(OutboundRequest {
        method: target.method.clone(),
        url: target.url.clone(),
        headers,
        body,
        timeout: Duration::from_secs(target.timeout_seconds),
    })
}

// Ascender and AWX speak the same routes: the message goes to the template's
// own receiver in GitLab mode (its key, compared as is, and any event name;
// the playbook reads the message as <prefix>_webhook_payload).
fn job_template_request(
    target: &JobTemplateTarget,
    message: &Map<String, Value>,
    credentials: &Fields,
) -> Result<OutboundRequest, String> {
    let token = credentials.required("token")?;
    let event = message.get("event").and_then(Value::as_str).unwrap_or("");
    let mut headers = base_headers(event);
    headers.push(("X-Gitlab-Token".into(), token.to_string()));
    headers.push(("X-Gitlab-Event".into(), format!("unified-gitops {event}")));
    check_headers(&headers)?;
    Ok(OutboundRequest {
        method: "POST".into(),
        url: target.endpoint(),
        headers,
        body: serde_json::to_vec(message).map_err(|e| e.to_string())?,
        timeout: Duration::from_secs(target.timeout_seconds),
    })
}

// What an answer means: any 2xx is taken (a controller answers 202, "Job
// queued.", or the same for a message it already had); anything else is why
// it was not, with the start of what the receiver said.
pub fn interpret(response: &OutboundResponse) -> Result<(), String> {
    if (200..300).contains(&response.status) {
        return Ok(());
    }
    let excerpt: String = response.body.chars().take(300).collect();
    Err(format!("HTTP {}: {}", response.status, excerpt.trim()))
}

// Worth sending again: no answer at all, or a receiver that is overloaded or
// failing. A 4xx is this side's mistake (a wrong key, a wrong template) and
// will not fix itself.
fn retryable(result: &Result<OutboundResponse, SendError>) -> bool {
    match result {
        Err(e) => e.retryable,
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

// One send of a webhook, for the API and the UI.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Delivery {
    pub event: String,
    // The project it was about; none for a test send.
    pub project: Option<String>,
    // Unix seconds.
    pub at: u64,
    pub ok: bool,
    pub attempts: u32,
    // The receiver's HTTP status, when it answered.
    pub status: Option<u16>,
    pub error: Option<String>,
}

// How many sends each webhook keeps. In memory, like run history: a
// restart starts it over.
pub const DELIVERY_HISTORY: usize = 50;

// Each webhook's recent sends, newest first.
#[derive(Default)]
pub struct DeliveryLog {
    sends: DashMap<String, std::collections::VecDeque<Delivery>>,
}

impl DeliveryLog {
    pub fn last(&self, webhook_id: &str) -> Option<Delivery> {
        self.sends.get(webhook_id).and_then(|h| h.front().cloned())
    }

    pub fn history(&self, webhook_id: &str) -> Vec<Delivery> {
        self.sends
            .get(webhook_id)
            .map(|h| h.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn record(&self, webhook_id: &str, delivery: Delivery) {
        let mut history = self.sends.entry(webhook_id.to_string()).or_default();
        history.push_front(delivery);
        history.truncate(DELIVERY_HISTORY);
    }
}

// One message for one webhook, its definition as of the run that made
// it: a reload while it waits does not change where it goes.
pub struct Job {
    pub webhook_id: String,
    pub webhook: Webhook,
    pub message: Map<String, Value>,
}

// Bounded: a receiver that is down for hours must not grow memory without
// limit. Messages waiting or being sent count against it; past it the new
// message is dropped, counted and logged.
const QUEUE: usize = 1000;
// Requests in flight at once. A slot is held for one attempt only, never
// through the wait before a retry, so a receiver that is down slows its own
// messages, not everyone's.
const CONCURRENT: usize = 8;
// Longest wait between two attempts.
const MAX_BACKOFF_SECONDS: u64 = 60;

pub struct Dispatcher {
    tx: mpsc::Sender<Job>,
    // Taken by start(); None once the worker runs.
    rx: std::sync::Mutex<Option<mpsc::Receiver<Job>>>,
    pub log: Arc<DeliveryLog>,
    // Messages queued or being sent: what QUEUE bounds and drain waits for.
    pending: Arc<std::sync::atomic::AtomicUsize>,
    // Which projects' pulls are failing, as told so far.
    pub pulls: PullStates,
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
            pending: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pulls: PullStates::default(),
        }
    }

    pub fn enqueue(&self, job: Job) {
        use std::sync::atomic::Ordering;
        let room = self.pending.load(Ordering::SeqCst) < QUEUE;
        let job = if room {
            self.pending.fetch_add(1, Ordering::SeqCst);
            match self.tx.try_send(job) {
                Ok(()) => return,
                Err(
                    mpsc::error::TrySendError::Full(job) | mpsc::error::TrySendError::Closed(job),
                ) => {
                    self.pending.fetch_sub(1, Ordering::SeqCst);
                    job
                }
            }
        } else {
            job
        };
        let event = job
            .message
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        tracing::warn!(webhook = %job.webhook_id, event = %event, "webhook queue full; message dropped");
        count(&job.webhook_id, &event, "dropped");
    }

    // Starts the worker that sends what is queued. Once: later calls do
    // nothing. Separate from construction so building the app needs no
    // runtime; main starts it beside the scheduler, and tests that send do.
    pub fn start(&self, secrets: Arc<dyn SecretsPort>, http: Arc<dyn HttpSenderPort>) {
        let Some(mut rx) = self.rx.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let log = Arc::clone(&self.log);
        let pending = Arc::clone(&self.pending);
        tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(CONCURRENT));
            while let Some(job) = rx.recv().await {
                let (secrets, http, log, slots, pending) = (
                    Arc::clone(&secrets),
                    Arc::clone(&http),
                    Arc::clone(&log),
                    Arc::clone(&slots),
                    Arc::clone(&pending),
                );
                tokio::spawn(async move {
                    deliver(&*secrets, &*http, &log, job, Some(&slots)).await;
                    pending.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                });
            }
        });
    }

    // Waits for what is queued or being sent, at most `grace`, and answers
    // how many messages were still left. Shutdown calls it after the runs
    // have finished, so the events of the last runs still go out.
    pub async fn drain(&self, grace: Duration) -> usize {
        use std::sync::atomic::Ordering;
        let deadline = Instant::now() + grace;
        while self.pending.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.pending.load(Ordering::SeqCst)
    }
}

// Sends one message, records the outcome and returns it. With `slots` (the
// worker's path) it retries, holding a slot for each attempt only; without
// (the "Send test" button) it tries once and answers at once.
pub async fn deliver(
    secrets: &dyn SecretsPort,
    http: &dyn HttpSenderPort,
    log: &DeliveryLog,
    job: Job,
    slots: Option<&Semaphore>,
) -> Delivery {
    let started = Instant::now();
    let event = job
        .message
        .get("event")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let project = job
        .message
        .get("project")
        .and_then(Value::as_str)
        .map(str::to_string);
    let mut delivery = Delivery {
        event: event.clone(),
        project,
        at: now(),
        ok: false,
        attempts: 0,
        status: None,
        error: None,
    };

    // A request that cannot be built (a missing credential field, a value
    // HTTP cannot carry) is the configuration's to fix: not retried.
    let request = match resolve(secrets, job.webhook.credentials()).await {
        Ok(credentials) => build_request(&job.webhook, &job.message, &credentials),
        Err(e) => Err(e),
    };
    match request {
        Err(e) => delivery.error = Some(e),
        Ok(request) => {
            let retries = if slots.is_some() {
                retries_of(&job.webhook)
            } else {
                0
            };
            loop {
                delivery.attempts += 1;
                let slot = match slots {
                    Some(slots) => slots.acquire().await.ok(),
                    None => None,
                };
                let result = http.send(request.clone()).await;
                drop(slot);
                delivery.status = result.as_ref().ok().map(|r| r.status);
                let again = retryable(&result);
                match result
                    .map_err(|e| e.message)
                    .and_then(|response| interpret(&response))
                {
                    Ok(()) => {
                        delivery.ok = true;
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
    count(&job.webhook_id, &event, outcome);
    metrics::histogram!(
        "unified_gitops_webhook_duration_seconds",
        "webhook" => job.webhook_id.clone()
    )
    .record(started.elapsed().as_secs_f64());
    if delivery.ok {
        tracing::info!(webhook = %job.webhook_id, event = %event, attempts = delivery.attempts, "webhook sent");
    } else {
        tracing::warn!(
            webhook = %job.webhook_id,
            event = %event,
            attempts = delivery.attempts,
            error = delivery.error.as_deref().unwrap_or(""),
            "webhook failed"
        );
    }
    log.record(&job.webhook_id, delivery.clone());
    delivery
}

fn retries_of(webhook: &Webhook) -> u32 {
    webhook
        .webhook
        .as_ref()
        .map(|w| w.retries)
        .or_else(|| webhook.ascender.as_ref().map(|t| t.retries))
        .or_else(|| webhook.awx.as_ref().map(|t| t.retries))
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

fn count(webhook_id: &str, event: &str, outcome: &'static str) {
    metrics::counter!(
        "unified_gitops_webhooks_total",
        "webhook" => webhook_id.to_string(),
        "event" => event.to_string(),
        "outcome" => outcome
    )
    .increment(1);
}

fn send_to(dispatcher: &Dispatcher, wanted: Vec<(&String, &Webhook)>, base: Map<String, Value>) {
    for (id, webhook) in wanted {
        let mut m = base.clone();
        m.insert("webhook".into(), json!(id));
        dispatcher.enqueue(Job {
            webhook_id: id.clone(),
            webhook: webhook.clone(),
            message: m,
        });
    }
}

// Projects whose pulls are failing, and since when (unix seconds). A failure
// is told when a project starts failing and a success when it stops. Kept apart
// from the project's sync_health on purpose: health counts every attempt,
// this remembers what was TOLD, so a failure before the webhooks could send
// (or before a webhook asked for it) still ends in a recovery that matches.
#[derive(Default)]
pub struct PullStates {
    failing: DashMap<String, u64>,
}

impl PullStates {
    // A pull failed. True when the project was not failing before.
    pub fn failed(&self, project: &str) -> bool {
        match self.failing.entry(project.to_string()) {
            dashmap::Entry::Vacant(entry) => {
                entry.insert(now());
                true
            }
            dashmap::Entry::Occupied(_) => false,
        }
    }

    // A pull worked. Since when it had been failing, if it had.
    pub fn succeeded(&self, project: &str) -> Option<u64> {
        self.failing.remove(project).map(|(_, since)| since)
    }
}

// What one pull of a project has to tell.
pub enum PullEvent<'a> {
    CommitPulled {
        // HEAD before the pull.
        from: &'a str,
        // None when git could not list what came in.
        range: Option<&'a CommitRange>,
    },
    PullFailed {
        error: &'a str,
        consecutive_failures: u32,
    },
    PullRecovered {
        failing_seconds: u64,
    },
}

impl PullEvent<'_> {
    fn event(&self) -> Event {
        match self {
            PullEvent::CommitPulled { .. } => Event::CommitPulled,
            PullEvent::PullFailed { .. } => Event::PullFailed,
            PullEvent::PullRecovered { .. } => Event::PullRecovered,
        }
    }
}

pub struct PullOutcome<'a> {
    pub project: &'a str,
    pub description: Option<&'a str>,
    // server.url, for a link to the project in the UI.
    pub base_url: Option<&'a str>,
    // The commit the checkout is at now.
    pub revision: Option<&'a str>,
    pub event: PullEvent<'a>,
}

// Commits listed in one message, and how much of each subject: enough to
// say what changed, not a changelog.
pub const MESSAGE_COMMITS: usize = 20;
const MESSAGE_SUBJECT_CHARS: usize = 200;
// Changed paths listed in one message: a reformat of the whole repository
// would otherwise make it as large as the tree.
const MESSAGE_FILES: usize = 200;

// A project event's message: the envelope every event has (event, project,
// description, revision, at, url, webhook), with what the event adds. Commit
// authors and subjects leave this process, as in any git host's push
// webhook; file contents never do.
pub fn pull_message(o: &PullOutcome<'_>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("event".into(), json!(o.event.event().as_str()));
    m.insert("project".into(), json!(o.project));
    m.insert("description".into(), json!(o.description));
    m.insert("revision".into(), json!(o.revision));
    match &o.event {
        PullEvent::CommitPulled { from, range } => {
            m.insert("previous_revision".into(), json!(from));
            let commits: Vec<Value> = range
                .map(|r| {
                    r.commits
                        .iter()
                        .take(MESSAGE_COMMITS)
                        .map(|c| {
                            json!({
                                "revision": c.revision,
                                "author": c.author,
                                "message": c.message.chars().take(MESSAGE_SUBJECT_CHARS).collect::<String>(),
                                "at": rfc3339(c.at),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            m.insert("commits".into(), Value::Array(commits));
            m.insert(
                "more_commits".into(),
                json!(range.is_none_or(|r| r.more || r.commits.len() > MESSAGE_COMMITS)),
            );
            m.insert("forced".into(), json!(range.is_some_and(|r| r.forced)));
            // The paths the commits touched; null when git could not list
            // them.
            m.insert(
                "files".into(),
                json!(
                    range
                        .and_then(|r| r.paths.as_deref())
                        .map(|paths| { paths.iter().take(MESSAGE_FILES).collect::<Vec<_>>() })
                ),
            );
        }
        PullEvent::PullFailed {
            error,
            consecutive_failures,
        } => {
            let error: String = error.chars().take(MESSAGE_ERROR_CHARS).collect();
            m.insert("errors".into(), json!([error]));
            m.insert("consecutive_failures".into(), json!(consecutive_failures));
        }
        PullEvent::PullRecovered { failing_seconds } => {
            m.insert("failing_seconds".into(), json!(failing_seconds));
        }
    }
    m.insert("at".into(), json!(rfc3339(now())));
    m.insert(
        "url".into(),
        json!(
            o.base_url
                .map(|base| format!("{base}/projects/{}", o.project))
        ),
    );
    m
}

// After a pull: queues its event for every webhook that wants it from this
// project.
pub fn after_pull(
    dispatcher: &Dispatcher,
    webhooks: &HashMap<String, Webhook>,
    outcome: &PullOutcome<'_>,
) {
    let event = outcome.event.event();
    let mut wanted: Vec<(&String, &Webhook)> = webhooks
        .iter()
        .filter(|(_, n)| n.wants_project(event, outcome.project))
        .collect();
    if wanted.is_empty() {
        return;
    }
    wanted.sort_by(|a, b| a.0.cmp(b.0));
    send_to(dispatcher, wanted, pull_message(outcome));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_value_is_null_outside_quotes_and_empty_inside() {
        let m = outcome_message();
        let body = render(
            r#"{"r": {{previous_revision}}, "s": "{{previous_revision}}"}"#,
            &m,
        );
        let parsed: Value = serde_json::from_str(&body).expect(&body);
        assert_eq!(parsed["r"], Value::Null);
        assert_eq!(parsed["s"], json!(""));
    }

    #[test]
    fn render_reads_placeholders_as_the_load_check_does() {
        // The check sees one placeholder, `project`; so does render:
        // the unclosed `{{a ` is text.
        let template = "x {{a {{project}} y {{}} z";
        let names: Vec<&str> = crate::domain::webhook::placeholders(template).collect();
        assert_eq!(names, ["project"]);
        assert_eq!(render(template, &outcome_message()), "x {{a prj y {{}} z");
    }

    fn outcome_message() -> Map<String, Value> {
        let range = CommitRange {
            commits: vec![crate::ports::git::Commit {
                revision: "0123456789abcdef".into(),
                author: "Jane".into(),
                message: "feat: add jane".into(),
                at: 1_700_000_000,
            }],
            more: false,
            forced: false,
            paths: Some(vec!["users/jane.json".into()]),
            fetched: false,
        };
        let mut m = pull_message(&PullOutcome {
            project: "prj",
            description: Some("Infra"),
            base_url: Some("https://gitops.example.com"),
            revision: Some("0123456789abcdef"),
            event: PullEvent::CommitPulled {
                from: "fedcba9876543210",
                range: Some(&range),
            },
        });
        m.insert("at".into(), json!("2023-11-14T22:13:20Z"));
        m.remove("previous_revision");
        m
    }

    #[test]
    fn a_commit_message_names_commits_and_files_never_contents() {
        let m = outcome_message();
        assert_eq!(m["event"], json!("commit_pulled"));
        assert_eq!(m["commits"][0]["author"], json!("Jane"));
        assert_eq!(m["files"], json!(["users/jane.json"]));
        assert_eq!(m["more_commits"], json!(false));
        assert_eq!(m["url"], json!("https://gitops.example.com/projects/prj"));
    }

    #[test]
    fn render_escapes_text_and_writes_json_for_lists() {
        let mut m = outcome_message();
        m.insert("description".into(), json!("say \"hi\""));
        let body = render(
            r#"{"d": "{{description}}", "c": {{commits}}, "r": "{{revision_short}}", "x": "{{previous_revision}}"}"#,
            &m,
        );
        let parsed: Value = serde_json::from_str(&body).expect(&body);
        assert_eq!(parsed["d"], json!("say \"hi\""));
        assert_eq!(parsed["c"][0]["author"], json!("Jane"));
        assert_eq!(parsed["r"], json!("0123456"));
        assert_eq!(parsed["x"], json!(""));
    }

    fn webhook(yaml: &str) -> Webhook {
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
        let bearer = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n",
        );
        let request = build_request(&bearer, &m, &creds).unwrap();
        assert_eq!(header(&request, "Authorization"), Some("Bearer t0k"));
        assert_eq!(header(&request, "Content-Type"), Some("application/json"));
        let parsed: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(parsed["project"], json!("prj"));

        let named = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n  auth: {header: X-Gitlab-Token}\n  headers: {X-Gitlab-Event: unified-gitops}\n",
        );
        let request = build_request(&named, &m, &creds).unwrap();
        assert_eq!(header(&request, "X-Gitlab-Token"), Some("t0k"));
        assert_eq!(header(&request, "Authorization"), None);
        assert_eq!(header(&request, "X-Gitlab-Event"), Some("unified-gitops"));

        let basic = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n  auth: basic\n",
        );
        let err = build_request(&basic, &m, &creds).unwrap_err();
        assert!(err.contains("'username'"), "{err}");

        // A key as the username and no password: allowed.
        let key_only = HashMap::from([("username".to_string(), "api-key".to_string())]);
        let request = build_request(&basic, &m, &key_only).unwrap();
        assert_eq!(
            header(&request, "Authorization"),
            Some("Basic YXBpLWtleTo=")
        );

        // A token read from a file with its trailing newline: refused before
        // anything is sent.
        let newline = HashMap::from([("token".to_string(), "t0k\n".to_string())]);
        let err = build_request(&bearer, &m, &newline).unwrap_err();
        assert!(err.contains("line break"), "{err}");
        assert!(!err.contains("t0k"), "{err}");
    }

    // Two credentials with a `token` deliver it under neither's bare name:
    // the send says the field is shared rather than missing, and a default
    // auth fails instead of silently sending no Authorization. A field only
    // one of them has is still read.
    #[test]
    fn a_field_two_credentials_share_is_named_shared_not_missing() {
        let creds = HashMap::from([
            ("auth_token".to_string(), "a".to_string()),
            ("sign_token".to_string(), "b".to_string()),
            ("secret".to_string(), "s".to_string()),
            ("sign_secret".to_string(), "s".to_string()),
        ]);
        let m = outcome_message();
        for auth in ["", "  auth: bearer\n", "  auth: {header: X-Key}\n"] {
            let w = webhook(&format!(
                "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [cred-auth, cred-sign]\n{auth}"
            ));
            let err = build_request(&w, &m, &creds).unwrap_err();
            assert!(
                err.contains("credentials 'cred-auth', 'cred-sign' all have a 'token' field"),
                "{auth}: {err}"
            );
        }
        let signed = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [cred-auth, cred-sign]\n  auth: none\n  sign: {secret_field: token}\n",
        );
        let err = build_request(&signed, &m, &creds).unwrap_err();
        assert!(err.contains("all have a 'token' field"), "{err}");
        let signed = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [cred-auth, cred-sign]\n  auth: none\n  sign: {}\n",
        );
        assert!(build_request(&signed, &m, &creds).is_ok());
        // Missing everywhere is still missing.
        let basic = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [cred-auth, cred-sign]\n  auth: basic\n",
        );
        let err = build_request(&basic, &m, &creds).unwrap_err();
        assert_eq!(err, "the credential has no 'username' field");
    }

    #[test]
    fn a_signed_webhook_carries_the_hmac_of_its_body() {
        let creds = HashMap::from([("secret".to_string(), "Jefe".to_string())]);
        let signed = webhook(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com/h\n  credentials: [c]\n  auth: none\n  body: 'what do ya want for nothing?'\n  sign: {}\n",
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
        let n = webhook(
            "type: ascender\nevents: [commit_pulled]\nprojects: all\nascender:\n  url: https://ascender.example.com\n  job_template: 12\n  credentials: [k]\n",
        );
        let request = build_request(&n, &outcome_message(), &creds).unwrap();
        assert_eq!(
            request.url,
            "https://ascender.example.com/api/v2/job_templates/12/gitlab/"
        );
        assert_eq!(header(&request, "X-Gitlab-Token"), Some("jt-key"));
        assert_eq!(
            header(&request, "X-Gitlab-Event"),
            Some("unified-gitops commit_pulled")
        );
        assert_eq!(header(&request, "Authorization"), None);
        let parsed: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(parsed["event"], json!("commit_pulled"));
    }

    #[test]
    fn any_2xx_is_taken_and_anything_else_says_why() {
        let queued = OutboundResponse {
            status: 202,
            body: r#"{"message": "Job queued."}"#.into(),
        };
        assert!(interpret(&queued).is_ok());
        let refused = OutboundResponse {
            status: 403,
            body: r#"{"detail": "You do not have permission"}"#.into(),
        };
        assert!(interpret(&refused).unwrap_err().starts_with("HTTP 403"));
    }

    #[test]
    fn only_unanswered_or_server_failures_are_retried() {
        assert!(retryable(&Err(SendError {
            message: "timed out".into(),
            retryable: true,
        })));
        // A request that could not be built fails the same way every time.
        assert!(!retryable(&Err(SendError {
            message: "builder error".into(),
            retryable: false,
        })));
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

    #[test]
    fn a_failing_project_is_told_once_and_its_recovery_once() {
        let pulls = PullStates::default();
        assert!(pulls.failed("p"));
        assert!(!pulls.failed("p"));
        assert!(pulls.succeeded("p").is_some());
        assert!(pulls.succeeded("p").is_none());
    }
}
