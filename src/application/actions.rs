use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::{Map, Value, json};

use crate::RuntimeConfig;
use crate::application::http::{Call, Credentials, Engine, Payload, describe, parse_body};
use crate::application::webhooks::{Dispatcher, Job};
use crate::domain::api::{self, Api, Per};
use crate::domain::http::HttpSpec;
use crate::domain::run::{Kind, Run, RunLog, Standing, Step};
use crate::domain::script::{Script, Stdin};
use crate::domain::template::Scope;
use crate::domain::trigger::On;
use crate::domain::unix_now as now;
use crate::domain::webhook::{Event, Webhook};
use crate::ports::git::GitPort;
use crate::ports::http_sender::HttpSenderPort;
use crate::ports::process::{Execution, ProcessPort};
use crate::ports::secrets::SecretsPort;

// The use case "act on a change": when a pull brings commits, the items
// watching that project (an API to call, a script to run) whose trigger
// selects a changed file run, one at a time per item; so do those on a
// schedule, at startup, or asked by hand. Each run is recorded, and what it
// did is told to the project's webhooks.

// What a run needs from outside, set once by `start`.
pub struct Ports {
    pub secrets: Arc<dyn SecretsPort>,
    pub http: Arc<dyn HttpSenderPort>,
    pub git: Arc<dyn GitPort>,
    pub dispatcher: Arc<Dispatcher>,
    pub process: Arc<dyn ProcessPort>,
    pub projects_dir: PathBuf,
}

// One reason to run an item.
#[derive(Debug, Clone)]
pub struct Request {
    pub kind: Kind,
    pub item: String,
    pub trigger: On,
    pub by: Option<String>,
    // The files a change touched; None: every file the trigger selects, as
    // the checkout has them now.
    pub files: Option<Vec<String>>,
    pub previous_revision: Option<String>,
    pub dry_run: bool,
}

#[derive(Default)]
pub struct Runner {
    ports: OnceLock<Arc<Ports>>,
    pub log: Arc<RunLog>,
    // One lock per item: two runs of one item never interleave.
    locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    // Changes waiting out an item's debounce: the files gathered so far
    // (None once any change could not list its files), and the commit the
    // first one started from.
    debounced: DashMap<String, (Option<BTreeSet<String>>, Option<String>)>,
}

impl Runner {
    pub fn new() -> Self {
        Self::default()
    }

    // Once: later calls do nothing. Until it is called (a test that builds
    // the app without starting it) runs are refused.
    pub fn start(&self, ports: Ports) {
        let _ = self.ports.set(Arc::new(ports));
    }

    fn ports(&self) -> Result<Arc<Ports>, String> {
        self.ports
            .get()
            .cloned()
            .ok_or_else(|| "runs have not started".to_string())
    }

    fn lock(&self, kind: Kind, item: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            &self
                .locks
                .entry(format!("{}/{item}", kind.as_str()))
                .or_default(),
        )
    }

    // Runs the item now and answers how it went: what a manual run and a
    // scheduled one do. Waits behind a run of the same item in progress.
    pub async fn run(&self, config: &Arc<RuntimeConfig>, request: Request) -> Result<Run, String> {
        let ports = self.ports()?;
        let lock = self.lock(request.kind, &request.item);
        let _held = lock.lock().await;
        let run = match request.kind {
            Kind::Api => {
                let api = config
                    .apis
                    .get(&request.item)
                    .ok_or_else(|| format!("API '{}' is not configured", request.item))?;
                run_api(&ports, api, &request).await
            }
            Kind::Script => {
                let script = config
                    .scripts
                    .get(&request.item)
                    .ok_or_else(|| format!("Script '{}' is not configured", request.item))?;
                run_script(&ports, script, &request).await
            }
        };
        let standing = self.log.record(run.clone());
        tell(&ports.dispatcher, config, &run, standing);
        Ok(run)
    }

    // In the background, so a pull is never held by what it starts.
    pub fn spawn(self: &Arc<Self>, config: Arc<RuntimeConfig>, request: Request) {
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let item = request.item.clone();
            if let Err(e) = runner.run(&config, request).await {
                tracing::warn!(item = %item, error = %e, "run could not start");
            }
        });
    }

    // After a pull that moved a checkout: every item of that project that
    // runs on a change, and whose trigger selects one of `files` (None: git
    // could not list them, and every item runs).
    pub fn after_commit(
        self: &Arc<Self>,
        config: &Arc<RuntimeConfig>,
        project: &str,
        previous_revision: &str,
        files: Option<&[String]>,
    ) {
        for item in items(config) {
            if !(item.enabled && item.project == project && item.trigger.fires_on(On::Change)) {
                continue;
            }
            let filter = item.trigger.filter();
            let selected: Option<Vec<String>> =
                files.map(|f| filter.select(f).into_iter().cloned().collect());
            if selected.as_ref().is_some_and(Vec::is_empty) {
                continue;
            }
            self.change(
                config,
                item.kind,
                item.id,
                item.trigger.debounce_seconds,
                selected,
                previous_revision,
            );
        }
    }

    fn change(
        self: &Arc<Self>,
        config: &Arc<RuntimeConfig>,
        kind: Kind,
        item: &str,
        debounce_seconds: u64,
        files: Option<Vec<String>>,
        previous_revision: &str,
    ) {
        let item_id = item.to_string();
        let request = move |files: Option<Vec<String>>, from: Option<String>| Request {
            kind,
            item: item_id.clone(),
            trigger: On::Change,
            by: None,
            files,
            previous_revision: from,
            dry_run: false,
        };
        if debounce_seconds == 0 {
            self.spawn(
                Arc::clone(config),
                request(files, Some(previous_revision.to_string())),
            );
            return;
        }
        let key = format!("{}/{item}", kind.as_str());
        let first = !self.debounced.contains_key(&key);
        {
            let mut entry = self
                .debounced
                .entry(key.clone())
                .or_insert_with(|| (Some(BTreeSet::new()), Some(previous_revision.to_string())));
            match (&mut entry.0, files) {
                (Some(gathered), Some(files)) => gathered.extend(files),
                (gathered, _) => *gathered = None,
            }
        }
        if !first {
            return;
        }
        let runner = Arc::clone(self);
        let config = Arc::clone(config);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(debounce_seconds)).await;
            if let Some((_, (files, from))) = runner.debounced.remove(&key) {
                // The configuration of the moment the wait ends: a reload in
                // between is what the run should see.
                let request = request(files.map(|f| f.into_iter().collect()), from);
                runner.spawn(config, request);
            }
        });
    }

    // At startup, after the first pulls: every item that runs then.
    pub fn at_startup(self: &Arc<Self>, config: &Arc<RuntimeConfig>) {
        for item in items(config) {
            if item.enabled && item.trigger.fires_on(On::Startup) {
                self.spawn(
                    Arc::clone(config),
                    scheduled(item.kind, item.id, On::Startup),
                );
            }
        }
    }
}

// Every item that acts, whichever its kind, by what the runner reads of it;
// sorted, so runs start in a stable order.
pub struct Item<'a> {
    pub kind: Kind,
    pub id: &'a str,
    pub project: &'a str,
    pub enabled: bool,
    pub trigger: &'a crate::domain::trigger::Trigger,
}

pub fn items(config: &RuntimeConfig) -> Vec<Item<'_>> {
    let mut all: Vec<Item<'_>> = config
        .apis
        .iter()
        .map(|(id, a)| Item {
            kind: Kind::Api,
            id,
            project: &a.project,
            enabled: a.enabled,
            trigger: &a.trigger,
        })
        .chain(config.scripts.iter().map(|(id, s)| Item {
            kind: Kind::Script,
            id,
            project: &s.project,
            enabled: s.enabled,
            trigger: &s.trigger,
        }))
        .collect();
    all.sort_by(|a, b| (a.kind, a.id).cmp(&(b.kind, b.id)));
    all
}

// A run of every file the trigger selects: at startup, on a schedule, by
// hand.
pub fn scheduled(kind: Kind, item: &str, trigger: On) -> Request {
    Request {
        kind,
        item: item.to_string(),
        trigger,
        by: None,
        files: None,
        previous_revision: None,
        dry_run: false,
    }
}

// Files under `root` the filter selects, as paths relative to it, without
// git's own folder.
fn files_in(root: &Path, filter: &crate::domain::trigger::PathFilter) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_name() == ".git" {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(root, &path, out),
                Ok(t) if t.is_file() => {
                    if let Ok(relative) = path.strip_prefix(root) {
                        out.push(relative.to_string_lossy().replace('\\', "/"));
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.retain(|f| filter.selects(f));
    out.sort();
    out
}

// What $event is for an item's run: the commit, the files and why.
fn event_value(
    project: &str,
    request: &Request,
    revision: Option<&str>,
    files: &[String],
) -> Value {
    json!({
        "event": request.trigger.as_str(),
        "project": project,
        "trigger": request.trigger.as_str(),
        "revision": revision,
        "revision_short": revision.map(|r| r.chars().take(7).collect::<String>()),
        "previous_revision": request.previous_revision,
        "files": files,
        "at": rfc3339(now()),
    })
}

fn rfc3339(unix_seconds: u64) -> String {
    chrono::DateTime::from_timestamp(unix_seconds as i64, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

// A URL as a record keeps it: no user info, no query.
fn shown(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    crate::domain::project::without_userinfo(without_query)
}

async fn run_api(ports: &Ports, api: &Api, request: &Request) -> Run {
    let started = Instant::now();
    let checkout = ports.projects_dir.join(&api.project);
    let revision = ports.git.revision(&checkout).await;
    let mut run = Run {
        item: request.item.clone(),
        kind: Kind::Api,
        project: api.project.clone(),
        trigger: request.trigger,
        by: request.by.clone(),
        revision: revision.clone(),
        script_revision: None,
        dry_run: request.dry_run,
        started_at: now(),
        duration_ms: 0,
        ok: false,
        error: None,
        steps: Vec::new(),
    };
    let finish = |mut run: Run| {
        run.ok = run.error.is_none() && run.steps.iter().all(|s| s.ok);
        run.duration_ms = started.elapsed().as_millis() as u64;
        count(&run);
        run
    };
    if !api.enabled && !request.dry_run && request.trigger != On::Manual {
        run.error = Some("disabled".to_string());
        return finish(run);
    }
    let credentials =
        match Credentials::resolve(&*ports.secrets, api.credential.as_ref(), &api.api.http).await {
            Ok(c) => c,
            Err(e) => {
                run.error = Some(e);
                return finish(run);
            }
        };
    let filter = api.trigger.filter();
    let files: Vec<String> = match &request.files {
        Some(files) => files.clone(),
        None => {
            let root = checkout.clone();
            tokio::task::spawn_blocking(move || files_in(&root, &filter))
                .await
                .unwrap_or_default()
        }
    };
    let engine = &ports.dispatcher.engine;
    let call = Call {
        item: &request.item,
        spec: &api.api.http,
        credentials: &credentials,
        timeout: Duration::from_secs(api.timeout_seconds),
        retry: api.retry,
    };
    let event = event_value(&api.project, request, revision.as_deref(), &files);
    let ctx = Ctx {
        http: &*ports.http,
        engine,
        call: &call,
        dry_run: request.dry_run,
    };
    match api.api.per {
        Per::Commit => {
            let scope = Scope::new().with("event", event);
            run.steps.push(ctx.commit(api, &scope).await);
        }
        Per::File => {
            let refs: Vec<&str> = files.iter().map(String::as_str).collect();
            for file in api.api.ordered(&refs) {
                let step = ctx.file(api, &checkout, file, &event).await;
                let failed = !step.ok;
                run.steps.push(step);
                if failed && api.on_failure == api::OnFailure::Stop {
                    run.error = Some(format!("stopped after {file} failed (on_failure: stop)"));
                    break;
                }
            }
        }
    }
    finish(run)
}

// What one API run sends through.
struct Ctx<'a> {
    http: &'a dyn HttpSenderPort,
    engine: &'a Engine,
    call: &'a Call<'a>,
    dry_run: bool,
}

impl Ctx<'_> {
    async fn commit(&self, api: &Api, scope: &Scope) -> Step {
        let body = api.api.body.as_ref().map(|b| scope.render(b));
        let method = api.api.http.method_or("POST").to_string();
        self.send(api, &method, &api.api.http.url, scope, body, None)
            .await
    }

    async fn file(&self, api: &Api, checkout: &Path, file: &str, event: &Value) -> Step {
        let path = checkout.join(file);
        let request = &api.api;
        let bytes = tokio::fs::read(&path).await;
        let Ok(bytes) = bytes else {
            // Gone from the checkout: the commit removed it.
            let scope = Scope::new()
                .with("event", event.clone())
                .with("file", api::file_value(file, "removed"));
            return match &request.on_delete {
                Some(step) => {
                    let (method, url) = step.resolve("DELETE", &request.http);
                    let mut s = self
                        .send(api, &method, &url, &scope, None, Some(file))
                        .await;
                    s.action = "delete".into();
                    s
                }
                None => Step {
                    file: Some(file.to_string()),
                    action: "skip".into(),
                    ok: true,
                    error: Some("removed; no on_delete".into()),
                    ..Step::default()
                },
            };
        };
        let status = if event["previous_revision"].is_null() {
            "present"
        } else {
            "changed"
        };
        let doc = match request.read(file, &bytes) {
            Ok(doc) => doc,
            Err(e) => {
                return Step {
                    file: Some(file.to_string()),
                    action: "read".into(),
                    error: Some(e),
                    ..Step::default()
                };
            }
        };
        let scope = Scope::new()
            .with("event", event.clone())
            .with("file", api::file_value(file, status))
            .with("doc", doc);
        let body = match &request.body {
            Some(template) => scope.render(template),
            None => scope.lookup(&["doc"]),
        };

        // Ask first, when the item compares or upserts: one GET answers both
        // when they read the same URL.
        let read_url = request
            .compare
            .as_ref()
            .and_then(|c| c.url.clone())
            .or_else(|| request.upsert.as_ref().map(|u| u.exists(&request.http).1))
            .unwrap_or_else(|| request.http.url.clone());
        let mut changed_fields = Vec::new();
        let mut exists: Option<bool> = None;
        if request.compare.is_some() || request.upsert.is_some() {
            let read = self.read(&read_url, &scope).await;
            match read {
                Err(step) => return step.with_file(file),
                Ok(None) => exists = Some(false),
                Ok(Some(live)) => {
                    exists = Some(true);
                    if let Some(compare) = &request.compare {
                        if api::covers(&live, &body, &compare.ignore_fields) {
                            return Step {
                                file: Some(file.to_string()),
                                action: "skip".into(),
                                ok: true,
                                ..Step::default()
                            };
                        }
                        changed_fields =
                            api::differing_fields(&live, &body, &compare.ignore_fields);
                    }
                }
            }
            // An upsert whose exists URL differs from compare's asks again.
            if let (Some(upsert), Some(compare_url)) = (
                &request.upsert,
                request.compare.as_ref().and_then(|c| c.url.as_ref()),
            ) && upsert.exists(&request.http).1 != *compare_url
            {
                match self.read(&upsert.exists(&request.http).1, &scope).await {
                    Err(step) => return step.with_file(file),
                    Ok(found) => exists = Some(found.is_some()),
                }
            }
        }
        let (action, method, url) = match (&request.upsert, exists) {
            (Some(upsert), Some(true)) => {
                let (m, u) = upsert.update(&request.http);
                ("update", m, u)
            }
            (Some(upsert), _) => {
                let (m, u) = upsert.create(&request.http);
                ("create", m, u)
            }
            (None, _) => (
                "send",
                request.http.method_or("PUT").to_string(),
                request.http.url.clone(),
            ),
        };
        let mut step = self
            .send(api, &method, &url, &scope, Some(body), Some(file))
            .await;
        step.action = action.into();
        step.changed_fields = changed_fields;
        step
    }

    // The live object at `url`: None when the API says it is not there (404).
    async fn read(&self, url: &str, scope: &Scope) -> Result<Option<Value>, Box<Step>> {
        let spec = HttpSpec {
            url: url.to_string(),
            success_status: vec![crate::domain::http::StatusRange { from: 200, to: 299 }],
            signature: None,
            ..self.call.spec.clone()
        };
        let call = Call {
            spec: &spec,
            ..*self.call
        };
        let payload = Payload {
            method: "GET".into(),
            body: None,
            headers: Vec::new(),
        };
        let outcome = self.engine.send(self.http, &call, scope, &payload).await;
        match (&outcome.response, outcome.ok) {
            (Some(response), true) => Ok(Some(parse_body(response))),
            (Some(response), false) if response.status == 404 => Ok(None),
            _ => Err(Box::new(Step {
                action: "read".into(),
                method: Some("GET".into()),
                url: Some(shown(&scope.render_text(url))),
                status: outcome.status,
                attempts: outcome.attempts,
                error: Some(
                    outcome
                        .response
                        .as_ref()
                        .map(describe)
                        .or(outcome.error)
                        .unwrap_or_default(),
                ),
                ..Step::default()
            })),
        }
    }

    async fn send(
        &self,
        api: &Api,
        method: &str,
        url: &str,
        scope: &Scope,
        body: Option<Value>,
        file: Option<&str>,
    ) -> Step {
        let started = Instant::now();
        let spec = HttpSpec {
            url: url.to_string(),
            ..self.call.spec.clone()
        };
        let call = Call {
            spec: &spec,
            ..*self.call
        };
        let mut step = Step {
            file: file.map(str::to_string),
            action: "send".into(),
            method: Some(method.to_string()),
            url: Some(shown(&scope.render_text(url))),
            ..Step::default()
        };
        let sends_body = !matches!(method, "GET" | "HEAD" | "DELETE");
        let encoded = match body.as_ref().filter(|_| sends_body) {
            Some(Value::String(text)) => Some((
                text.clone().into_bytes(),
                api.api.content_type_for(&Value::String(String::new())),
            )),
            Some(value) => match serde_json::to_vec(value) {
                Ok(bytes) => Some((bytes, api.api.content_type_for(value))),
                Err(e) => {
                    step.error = Some(e.to_string());
                    return step;
                }
            },
            None => None,
        };
        if self.dry_run {
            step.ok = true;
            step.body = body.filter(|_| sends_body);
            // What would be sent, built whole: a template or a credential
            // that would fail the send fails the dry run too.
            let payload = Payload {
                method: method.to_string(),
                body: encoded,
                headers: Vec::new(),
            };
            if let Err(e) = self.engine.build(self.http, &call, scope, &payload).await {
                step.ok = false;
                step.error = Some(e);
            }
            return step;
        }
        let payload = Payload {
            method: method.to_string(),
            body: encoded,
            headers: Vec::new(),
        };
        let outcome = self.engine.send(self.http, &call, scope, &payload).await;
        step.ok = outcome.ok;
        step.status = outcome.status;
        step.attempts = outcome.attempts;
        step.error = outcome.error;
        step.duration_ms = started.elapsed().as_millis() as u64;
        step
    }
}

trait WithFile {
    fn with_file(self, file: &str) -> Step;
}

impl WithFile for Box<Step> {
    fn with_file(self, file: &str) -> Step {
        let mut step = *self;
        step.file = Some(file.to_string());
        step
    }
}

fn count(run: &Run) {
    if run.dry_run {
        return;
    }
    metrics::counter!(
        "unified_gitops_runs_total",
        "kind" => run.kind.as_str(),
        "item" => run.item.clone(),
        "outcome" => if run.ok { "success" } else { "error" }
    )
    .increment(1);
    metrics::histogram!(
        "unified_gitops_run_duration_seconds",
        "kind" => run.kind.as_str(),
        "item" => run.item.clone()
    )
    .record(run.duration_ms as f64 / 1000.0);
}

// How a run's steps are summed up in a message: what each did, never a
// body or an output.
const MESSAGE_STEPS: usize = 50;
const MESSAGE_ERRORS: usize = 5;

pub fn run_message(
    run: &Run,
    event: Event,
    description: Option<&str>,
    base_url: Option<&str>,
) -> Map<String, Value> {
    let steps: Vec<Value> = run
        .steps
        .iter()
        .take(MESSAGE_STEPS)
        .map(|s| {
            json!({
                "file": s.file,
                "action": s.action,
                "status": s.status,
                "exit_code": s.exit_code,
                "ok": s.ok,
                "error": s.error,
            })
        })
        .collect();
    let errors: Vec<String> = run
        .error
        .iter()
        .chain(
            run.steps
                .iter()
                .filter_map(|s| s.error.as_ref().filter(|_| !s.ok)),
        )
        .take(MESSAGE_ERRORS)
        .map(|e| e.chars().take(500).collect())
        .collect();
    let mut m = Map::new();
    m.insert("event".into(), json!(event.as_str()));
    m.insert("project".into(), json!(run.project));
    m.insert("description".into(), json!(description));
    m.insert("revision".into(), json!(run.revision));
    m.insert(
        "revision_short".into(),
        json!(
            run.revision
                .as_ref()
                .map(|r| r.chars().take(7).collect::<String>())
        ),
    );
    m.insert("item".into(), json!(run.item));
    m.insert("kind".into(), json!(run.kind.as_str()));
    m.insert("trigger".into(), json!(run.trigger.as_str()));
    m.insert("ok".into(), json!(run.ok));
    m.insert("steps".into(), Value::Array(steps));
    m.insert("errors".into(), json!(errors));
    m.insert("duration_ms".into(), json!(run.duration_ms));
    m.insert("at".into(), json!(rfc3339(now())));
    m.insert(
        "url".into(),
        json!(base_url.map(|b| format!("{b}/{}/{}", run.kind.section(), run.item))),
    );
    m
}

// After a run: its event, to every webhook of the project that wants it.
fn tell(dispatcher: &Dispatcher, config: &RuntimeConfig, run: &Run, standing: Standing) {
    if run.dry_run {
        return;
    }
    let (succeeded, failed, recovered) = match run.kind {
        Kind::Api => (Event::ApiSucceeded, Event::ApiFailed, Event::ApiRecovered),
        Kind::Script => (
            Event::ScriptSucceeded,
            Event::ScriptFailed,
            Event::ScriptRecovered,
        ),
    };
    let mut events = vec![if run.ok { succeeded } else { failed }];
    if standing == Standing::Recovered {
        events.push(recovered);
    }
    let description = match run.kind {
        Kind::Api => config
            .apis
            .get(&run.item)
            .and_then(|a| a.description.as_deref()),
        Kind::Script => config
            .scripts
            .get(&run.item)
            .and_then(|s| s.description.as_deref()),
    };
    let mut wanted: Vec<(&String, &Webhook, Event)> = Vec::new();
    for event in events {
        for (id, webhook) in &config.webhooks {
            if webhook.wants(event, &run.project, None) {
                wanted.push((id, webhook, event));
            }
        }
    }
    wanted.sort_by(|a, b| (a.0, a.2.as_str()).cmp(&(b.0, b.2.as_str())));
    for (id, webhook, event) in wanted {
        let mut m = run_message(run, event, description, config.url.as_deref());
        m.insert("webhook".into(), json!(id));
        dispatcher.enqueue(Job {
            webhook_id: id.clone(),
            webhook: webhook.clone(),
            message: m,
        });
    }
}

// The items each project's change would start, for the API: which APIs
// and scripts watch it.
pub fn watching(config: &RuntimeConfig, project: &str) -> HashMap<Kind, Vec<String>> {
    let mut out: HashMap<Kind, Vec<String>> =
        HashMap::from([(Kind::Api, vec![]), (Kind::Script, vec![])]);
    for item in items(config) {
        if item.project == project {
            out.entry(item.kind).or_default().push(item.id.to_string());
        }
    }
    out
}

// A credential's fields as a script reads them: CREDENTIAL_<FIELD>, the
// name upper case and anything but letters and digits as '_'.
pub fn credential_variable(field: &str) -> String {
    let name: String = field
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("CREDENTIAL_{name}")
}

// added, changed or removed, as the checkout has the file now.
fn status_of(checkout: &Path, file: &str, previous: Option<&String>) -> &'static str {
    match (checkout.join(file).exists(), previous) {
        (false, _) => "removed",
        (true, Some(_)) => "changed",
        (true, None) => "present",
    }
}

async fn run_script(ports: &Ports, script: &Script, request: &Request) -> Run {
    let started = Instant::now();
    let command = &script.script;
    let watched = ports.projects_dir.join(&script.project);
    let home_project = command.project_of(&script.project);
    let home = ports.projects_dir.join(home_project);
    let mut run = Run {
        item: request.item.clone(),
        kind: Kind::Script,
        project: script.project.clone(),
        trigger: request.trigger,
        by: request.by.clone(),
        revision: ports.git.revision(&watched).await,
        script_revision: ports.git.revision(&home).await,
        dry_run: request.dry_run,
        started_at: now(),
        duration_ms: 0,
        ok: false,
        error: None,
        steps: Vec::new(),
    };
    let finish = |mut run: Run| {
        run.ok = run.error.is_none() && run.steps.iter().all(|s| s.ok);
        run.duration_ms = started.elapsed().as_millis() as u64;
        count(&run);
        run
    };
    let file = home.join(&command.file);
    if !file.is_file() {
        run.error = Some(format!(
            "script.file '{}' is not in project '{home_project}' (has it been pulled?)",
            command.file
        ));
        return finish(run);
    }
    let mut secrets: Vec<(String, String)> = Vec::new();
    if let Some(id) = &script.credential {
        match ports.secrets.resolve(id).await {
            Ok(fields) => {
                secrets = fields
                    .into_iter()
                    .map(|(field, value)| (credential_variable(&field), value))
                    .collect();
                secrets.sort();
            }
            Err(e) => {
                run.error = Some(format!("credential '{id}': {}", e.message));
                return finish(run);
            }
        }
    }
    let filter = script.trigger.filter();
    let files: Vec<String> = match &request.files {
        Some(files) => files.clone(),
        None => {
            let root = watched.clone();
            tokio::task::spawn_blocking(move || files_in(&root, &filter))
                .await
                .unwrap_or_default()
        }
    };
    let event = event_value(&script.project, request, run.revision.as_deref(), &files);
    let fixed = vec![
        ("GITOPS_ITEM".to_string(), request.item.clone()),
        ("GITOPS_PROJECT".to_string(), script.project.clone()),
        (
            "GITOPS_TRIGGER".to_string(),
            request.trigger.as_str().to_string(),
        ),
        (
            "GITOPS_CHECKOUT".to_string(),
            watched.to_string_lossy().into_owned(),
        ),
        (
            "GITOPS_COMMIT".to_string(),
            run.revision.clone().unwrap_or_default(),
        ),
        (
            "GITOPS_PREVIOUS_COMMIT".to_string(),
            request.previous_revision.clone().unwrap_or_default(),
        ),
        (
            "GITOPS_SCRIPT_COMMIT".to_string(),
            run.script_revision.clone().unwrap_or_default(),
        ),
        ("GITOPS_CHANGED_FILES".to_string(), files.join("\n")),
    ];
    let venv_bin = {
        let bin = ports
            .projects_dir
            .join(crate::ports::venv::VENVS_DIR)
            .join(home_project)
            .join("bin");
        bin.is_dir().then(|| bin.to_string_lossy().into_owned())
    };
    let working_dir = match &command.working_dir {
        Some(dir) => watched.join(dir),
        None => watched.clone(),
    };
    let units: Vec<Option<&str>> = match command.per {
        Per::Commit => vec![None],
        Per::File => files.iter().map(|f| Some(f.as_str())).collect(),
    };
    for unit in units {
        let mut scope = Scope::new().with("event", event.clone());
        let mut vars = fixed.clone();
        if let Some(file) = unit {
            let status = status_of(&watched, file, request.previous_revision.as_ref());
            scope.set("file", api::file_value(file, status));
            vars.push(("GITOPS_FILE".to_string(), file.to_string()));
            vars.push(("GITOPS_FILE_STATUS".to_string(), status.to_string()));
        }
        vars.extend(
            command
                .env
                .iter()
                .map(|(k, v)| (k.clone(), scope.render_text(v))),
        );
        vars.extend(secrets.iter().cloned());
        let rendered: Vec<String> = command.args.iter().map(|a| scope.render_text(a)).collect();
        let file_path = file.to_string_lossy().into_owned();
        let (program, args) = match &command.interpreter {
            Some(interpreter) => (
                interpreter.clone(),
                std::iter::once(file_path).chain(rendered).collect(),
            ),
            None => (file_path, rendered),
        };
        let execution = Execution {
            program,
            args,
            working_dir: working_dir.clone(),
            env: vars,
            stdin: (command.stdin == Stdin::Event)
                .then(|| serde_json::to_vec(&scope.lookup(&["event"])).unwrap_or_default()),
            timeout: Duration::from_secs(script.timeout_seconds),
            kill_grace: Duration::from_secs(command.kill_grace_seconds),
            max_output_bytes: command.max_output_bytes,
            venv_bin: venv_bin.clone(),
        };
        let step = execute(ports, script, &execution, unit, request.dry_run).await;
        let failed = !step.ok;
        run.steps.push(step);
        if failed && script.on_failure == api::OnFailure::Stop {
            if let Some(file) = unit {
                run.error = Some(format!("stopped after {file} failed (on_failure: stop)"));
            }
            break;
        }
    }
    finish(run)
}

async fn execute(
    ports: &Ports,
    script: &Script,
    execution: &Execution,
    file: Option<&str>,
    dry_run: bool,
) -> Step {
    let started = Instant::now();
    let mut step = Step {
        file: file.map(str::to_string),
        action: "run".into(),
        ..Step::default()
    };
    if dry_run {
        // What would run, and the names of the variables it would get: never
        // their values, a credential's among them.
        let mut names: Vec<&str> = execution.env.iter().map(|(k, _)| k.as_str()).collect();
        names.sort();
        step.ok = true;
        step.output = Some(format!(
            "would run: {} {}\nin: {}\nwith: {}",
            execution.program,
            execution.args.join(" "),
            execution.working_dir.display(),
            names.join(", ")
        ));
        return step;
    }
    loop {
        step.attempts += 1;
        match ports.process.run(execution.clone()).await {
            Err(e) => {
                step.ok = false;
                step.error = Some(e);
                step.exit_code = None;
            }
            Ok(exited) => {
                let ok = !exited.timed_out
                    && exited
                        .exit_code
                        .is_some_and(|c| script.script.success_exit_codes.contains(&c));
                step.exit_code = exited.exit_code;
                step.output = Some(exited.output);
                step.ok = ok;
                step.error = if ok {
                    None
                } else if exited.timed_out {
                    Some(format!("timed out after {}s", script.timeout_seconds))
                } else {
                    Some(match exited.exit_code {
                        Some(code) => format!("exit code {code}"),
                        None => "ended by a signal".to_string(),
                    })
                };
            }
        }
        if step.ok || step.attempts >= script.retry.attempts {
            break;
        }
        tokio::time::sleep(script.retry.delay(step.attempts + 1)).await;
    }
    step.duration_ms = started.elapsed().as_millis() as u64;
    step
}
