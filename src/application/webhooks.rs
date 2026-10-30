use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Semaphore, mpsc};
use utoipa::ToSchema;

use crate::application::http::{Call, Credentials, Engine, Payload};
use crate::domain::template::Scope;
use crate::domain::unix_now as now;
use crate::domain::webhook::{ContentType, Event, Webhook};
use crate::ports::git::CommitRange;
use crate::ports::http_sender::HttpSenderPort;
use crate::ports::secrets::SecretsPort;

// The use case "tell someone what happened to the checkout of a project".
//
// After every pull, the project's event (if any) is queued for the webhooks
// that want it; a worker sends them through application::http, so a slow
// receiver never holds up a pull.

// How much of a failure a message carries.
const MESSAGE_ERROR_CHARS: usize = 500;

// The message a "Send test" sends: the real shape, marked as a test.
pub fn test_message(webhook: &Webhook, base_url: Option<&str>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("event".into(), json!("test"));
    m.insert("project".into(), json!(webhook.project));
    m.insert("description".into(), json!("A test send"));
    for key in ["revision", "revision_short", "url"] {
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

// What one webhook sends for one message: its body, written as its
// content_type says, and the event's name in a header. Pure.
pub fn payload(webhook: &Webhook, message: &Map<String, Value>) -> Result<Payload, String> {
    let request = &webhook.webhook;
    let method = request.http.method_or("POST").to_string();
    let event = message.get("event").and_then(Value::as_str).unwrap_or("");
    let headers = vec![("X-Unified-Gitops-Event".to_string(), event.to_string())];
    if method == "GET" || method == "HEAD" {
        return Ok(Payload {
            method,
            body: None,
            headers,
        });
    }
    let scope = Scope::new().with("event", Value::Object(message.clone()));
    let content = match &request.body {
        None => Value::Object(message.clone()),
        Some(template) => scope.render(template),
    };
    let bytes = match request.content_type {
        ContentType::Json => serde_json::to_vec(&content).map_err(|e| e.to_string())?,
        ContentType::Form => {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            if let Value::Object(fields) = &content {
                for (name, value) in fields {
                    form.append_pair(name, &crate::domain::template::as_text(value));
                }
            }
            form.finish().into_bytes()
        }
        ContentType::Text => crate::domain::template::as_text(&content).into_bytes(),
    };
    Ok(Payload {
        method,
        body: Some((bytes, request.content_type.mime().to_string())),
        headers,
    })
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

// One message for one webhook, its definition as of the pull that made
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
// Messages being sent at once. A receiver that is down holds its slot
// through its retries: eight of those at once and the others wait, which is
// what bounds the load this instance puts on a receiver that is failing.
const CONCURRENT: usize = 8;

pub struct Dispatcher {
    tx: mpsc::Sender<Job>,
    // Taken by start(); None once the worker runs.
    rx: std::sync::Mutex<Option<mpsc::Receiver<Job>>>,
    pub log: Arc<DeliveryLog>,
    // Messages queued or being sent: what QUEUE bounds and drain waits for.
    pending: Arc<std::sync::atomic::AtomicUsize>,
    // Which projects' pulls are failing, as told so far.
    pub pulls: PullStates,
    // Tokens and rate limits, shared by every send.
    pub engine: Arc<Engine>,
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
            engine: Arc::new(Engine::new()),
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
        let engine = Arc::clone(&self.engine);
        tokio::spawn(async move {
            let slots = Arc::new(Semaphore::new(CONCURRENT));
            while let Some(job) = rx.recv().await {
                let (secrets, http, log, slots, pending, engine) = (
                    Arc::clone(&secrets),
                    Arc::clone(&http),
                    Arc::clone(&log),
                    Arc::clone(&slots),
                    Arc::clone(&pending),
                    Arc::clone(&engine),
                );
                tokio::spawn(async move {
                    let _slot = slots.acquire().await;
                    deliver(&*secrets, &*http, &engine, &log, job, Mode::Queued).await;
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

// How a send is made: from the queue, with the webhook's retries and its
// wait; or by "Send test", once, without waiting for what it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Queued,
    Test,
}

// Sends one message, records the outcome and returns it.
pub async fn deliver(
    secrets: &dyn SecretsPort,
    http: &dyn HttpSenderPort,
    engine: &Engine,
    log: &DeliveryLog,
    job: Job,
    mode: Mode,
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

    let webhook = &job.webhook;
    let spec = &webhook.webhook.http;
    let prepared = match Credentials::resolve(secrets, webhook.credential.as_ref(), spec).await {
        Ok(credentials) => payload(webhook, &job.message).map(|p| (credentials, p)),
        Err(e) => Err(e),
    };
    match prepared {
        Err(e) => delivery.error = Some(e),
        Ok((credentials, payload)) => {
            let call = Call {
                item: &job.webhook_id,
                spec,
                credentials: &credentials,
                timeout: Duration::from_secs(webhook.timeout_seconds),
                retry: match mode {
                    Mode::Queued => webhook.retry,
                    Mode::Test => crate::domain::http::Retry {
                        attempts: 1,
                        ..webhook.retry
                    },
                },
            };
            let scope = Scope::new().with("event", Value::Object(job.message.clone()));
            let outcome = engine.send(http, &call, &scope, &payload).await;
            delivery.attempts = outcome.attempts;
            delivery.status = outcome.status;
            delivery.ok = outcome.ok;
            delivery.error = outcome.error;
            if let (true, Mode::Queued, Some(wait), Some(response)) =
                (delivery.ok, mode, &webhook.webhook.wait, &outcome.response)
                && let Err(e) = engine.wait(http, &call, &scope, wait, response).await
            {
                delivery.ok = false;
                delivery.error = Some(e);
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
    m.insert(
        "revision_short".into(),
        json!(o.revision.map(|r| r.chars().take(7).collect::<String>())),
    );
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
// project (for a commit, one whose trigger selects a file it touched).
pub fn after_pull(
    dispatcher: &Dispatcher,
    webhooks: &std::collections::HashMap<String, Webhook>,
    outcome: &PullOutcome<'_>,
) {
    let event = outcome.event.event();
    let files = match &outcome.event {
        PullEvent::CommitPulled { range, .. } => range.and_then(|r| r.paths.as_deref()),
        _ => None,
    };
    let mut wanted: Vec<(&String, &Webhook)> = webhooks
        .iter()
        .filter(|(_, n)| n.wants(event, outcome.project, files))
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
    use crate::ports::http_sender::{OutboundRequest, OutboundResponse, SendFuture};

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
        pull_message(&PullOutcome {
            project: "prj",
            description: Some("Infra"),
            base_url: Some("https://gitops.example.com"),
            revision: Some("0123456789abcdef"),
            event: PullEvent::CommitPulled {
                from: "fedcba9876543210",
                range: Some(&range),
            },
        })
    }

    fn webhook(yaml: &str) -> Webhook {
        serde_saphyr::from_str(&format!("project: prj\nevents: [commit_pulled]\n{yaml}")).unwrap()
    }

    #[test]
    fn a_commit_message_names_commits_and_files_never_contents() {
        let m = outcome_message();
        assert_eq!(m["event"], json!("commit_pulled"));
        assert_eq!(m["revision_short"], json!("0123456"));
        assert_eq!(m["commits"][0]["author"], json!("Jane"));
        assert_eq!(m["files"], json!(["users/jane.json"]));
        assert_eq!(m["more_commits"], json!(false));
        assert_eq!(m["url"], json!("https://gitops.example.com/projects/prj"));
        // Every field a message has is one a template may name.
        for key in m.keys() {
            assert!(
                crate::domain::webhook::EVENT_FIELDS.contains(&key.as_str()),
                "{key}"
            );
        }
    }

    #[test]
    fn the_body_is_shaped_by_its_template_and_content_type() {
        let m = outcome_message();
        let plain = payload(&webhook("webhook:\n  url: https://x\n"), &m).unwrap();
        let (bytes, mime) = plain.body.unwrap();
        assert_eq!(mime, "application/json");
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap(),
            Value::Object(m.clone())
        );
        assert_eq!(plain.headers[0].1, "commit_pulled");

        let shaped = payload(
            &webhook("webhook:\n  url: https://x\n  body:\n    limit: vcenter\n    extra_vars: {gitops: $event, files: $event.files}\n    text: \"$event.project at $event.revision_short\"\n"),
            &m,
        )
        .unwrap();
        let body: Value = serde_json::from_slice(&shaped.body.unwrap().0).unwrap();
        assert_eq!(body["limit"], "vcenter");
        assert_eq!(body["extra_vars"]["gitops"]["project"], "prj");
        assert_eq!(body["extra_vars"]["files"], json!(["users/jane.json"]));
        assert_eq!(body["text"], "prj at 0123456");

        let form = payload(
            &webhook("webhook:\n  url: https://x\n  content_type: form\n  body: {project: $event.project, files: $event.files}\n"),
            &m,
        )
        .unwrap();
        let (bytes, mime) = form.body.unwrap();
        assert_eq!(mime, "application/x-www-form-urlencoded");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "files=%5B%22users%2Fjane.json%22%5D&project=prj"
        );

        let text = payload(
            &webhook("webhook:\n  url: https://x\n  content_type: text\n  body: \"$event.project changed\"\n"),
            &m,
        )
        .unwrap();
        assert_eq!(text.body.unwrap().0, b"prj changed");

        let get = payload(&webhook("webhook:\n  url: https://x\n  method: GET\n"), &m).unwrap();
        assert!(get.body.is_none());
    }

    struct Answer(u16, &'static str);

    impl HttpSenderPort for Answer {
        fn send(&self, _request: OutboundRequest) -> SendFuture<'_> {
            let response = OutboundResponse {
                status: self.0,
                body: self.1.into(),
                headers: vec![],
            };
            Box::pin(async move { Ok(response) })
        }
    }

    #[tokio::test]
    async fn a_delivery_is_recorded_with_how_it_went() {
        let log = DeliveryLog::default();
        let engine = Engine::new();
        let secrets = crate::adapters::out::secrets::mock::MockSecrets::new();
        let job = |yaml: &str| Job {
            webhook_id: "w".into(),
            webhook: webhook(yaml),
            message: outcome_message(),
        };
        let d = deliver(
            &secrets,
            &Answer(200, ""),
            &engine,
            &log,
            job("webhook:\n  url: https://x\n"),
            Mode::Queued,
        )
        .await;
        assert!(d.ok && d.attempts == 1, "{d:?}");
        let d = deliver(
            &secrets,
            &Answer(500, "down"),
            &engine,
            &log,
            job("retry: {attempts: 3, initial_delay_seconds: 0}\nwebhook:\n  url: https://x\n"),
            Mode::Test,
        )
        .await;
        assert_eq!((d.ok, d.attempts), (false, 1), "a test tries once");
        assert_eq!(d.error.as_deref(), Some("HTTP 500: down"));
        // An unresolvable credential is told, not sent.
        let d = deliver(
            &secrets,
            &Answer(200, ""),
            &engine,
            &log,
            job("credential: cred-x\nwebhook:\n  url: https://x\n"),
            Mode::Queued,
        )
        .await;
        assert!(!d.ok && d.attempts == 0 && d.error.unwrap().contains("cred-x"));
        // A wait that fails makes the send fail.
        let d = deliver(
            &secrets,
            &Answer(200, r#"{"status": "failed"}"#),
            &engine,
            &log,
            job("webhook:\n  url: https://x\n  wait:\n    url: https://x/job\n    until: $poll.status == successful\n    fail_if: $poll.status == failed\n"),
            Mode::Queued,
        )
        .await;
        assert_eq!(d.error.as_deref(), Some("wait: $poll.status is 'failed'"));
        assert_eq!(log.history("w").len(), 4);
        assert_eq!(log.last("w").unwrap().event, "commit_pulled");
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
