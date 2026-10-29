use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use dashmap::DashMap;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::domain::app::Application;
use crate::domain::connector::{Connector, PROTOCOL_VERSION};
use crate::domain::desired;
use crate::domain::plan::{self, OpKind, Operation};
use crate::domain::project::GitProject;
use crate::domain::run::{
    AppStateRegistry, AppliedOperation, PlanSnapshot, RunMode, RunRecord, RunResult, SyncStatus,
    Trigger, now,
};
use crate::domain::sync_health::SyncHealthRegistry;
use crate::ports::connector::{ConnectorCall, ConnectorPort};
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;
use crate::ports::state_store::StateStorePort;

// The configuration a run reads: borrowed from one RuntimeConfig snapshot, so
// a reload in the middle of a run cannot mix an old application with a new
// connector.
pub struct Catalog<'a> {
    pub applications: &'a HashMap<String, Application>,
    pub connectors: &'a HashMap<String, Connector>,
    pub projects: &'a HashMap<String, GitProject>,
    pub webhooks: &'a HashMap<String, crate::domain::webhook::Webhook>,
    // server.url, for links in webhooks.
    pub url: Option<&'a str>,
}

// The ports and registries a run uses.
pub struct Reconciler<'a> {
    pub connector: &'a dyn ConnectorPort,
    pub git: &'a dyn GitPort,
    pub secrets: &'a dyn SecretsPort,
    pub store: &'a dyn StateStorePort,
    pub states: &'a AppStateRegistry,
    pub health: &'a SyncHealthRegistry,
    pub locks: &'a Locks,
    pub projects_dir: &'a Path,
    // For pulling the application's project before a run.
    pub venv: &'a dyn crate::ports::venv::VenvPort,
    pub project_health: &'a SyncHealthRegistry,
    // What each connector last described, for search.
    pub described: &'a crate::domain::connector::DescribedKinds,
    // Queues what a run has to tell (application::webhooks).
    pub dispatcher: &'a crate::application::webhooks::Dispatcher,
}

// Pull the application's project, then reconcile. What every run should do —
// the schedule and a person pressing Refresh or Sync alike: a Sync right after
// a commit must apply that commit, not the one the last periodic pull saw.
//
// A failed pull is recorded on the project (its sync_health) and the run goes
// on against the checkout already on disk: the systems drift whether or not
// git answers, and the run's revision says which commit it actually read.
//
// `by` is who asked for a manual run (None for the schedule): the pull is a
// `check` in the project's history, and a recorded one says who caused it.
pub async fn pull_and_reconcile(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    mode: RunMode,
    trigger: Trigger,
    by: Option<String>,
) -> Result<RunRecord, String> {
    if let Some(app) = catalog.applications.get(app_id)
        && let Some(project) = catalog.projects.get(&app.project)
        && let Err(e) = crate::application::projects::sync_project(
            r.git,
            r.secrets,
            r.venv,
            r.project_health,
            &app.project,
            project,
            r.projects_dir,
            Some(crate::application::projects::Announce {
                dispatcher: r.dispatcher,
                catalog,
            }),
            crate::application::projects::RunOrigin {
                trigger: crate::domain::sync_health::RunTrigger::Check,
                by,
            },
        )
        .await
    {
        tracing::warn!(application = %app_id, project = %app.project, error = %e, "Pull failed — reconciling against the checkout on disk");
    }
    reconcile(r, catalog, app_id, mode, trigger).await
}

// One lock per application: it never runs twice at once (the schedule and a
// person pressing Sync) — the same rule unified-api's SyncCoordinator keeps
// per source. Two applications pointed at the same system are not serialised
// against each other; each owns only the kinds and keys its files declare.
#[derive(Default)]
pub struct Locks {
    inner: DashMap<String, Arc<Mutex<()>>>,
}

impl Locks {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, key: String) -> Arc<Mutex<()>> {
        self.inner.entry(key).or_default().clone()
    }
}

pub async fn reconcile(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    mode: RunMode,
    trigger: Trigger,
) -> Result<RunRecord, String> {
    let app = catalog
        .applications
        .get(app_id)
        .ok_or_else(|| format!("application '{app_id}' is not configured"))?;
    let app_lock = r.locks.get(app_id.to_string());
    let _running = app_lock.lock().await;
    let mode = effective_mode(mode, trigger, r.store.rollback(app_id).is_some());
    let checkout = r.projects_dir.join(&app.project);
    // Shared with other readers, exclusive of pulls: the commit recorded and
    // the files read must be the same tree (see projects::checkout_lock).
    // Released as soon as the desired state is read.
    let reading = crate::application::projects::checkout_lock(&checkout)
        .read_owned()
        .await;
    let revision = r.git.revision(&checkout).await;
    let tree = r.git.tree(&checkout, app.folder()).await;
    let record = reconcile_tree(
        r,
        catalog,
        app_id,
        app,
        &checkout,
        Some(reading),
        (revision, tree),
        mode,
        trigger,
    )
    .await;
    // A Sync someone asked for applies HEAD again: whatever a rollback had
    // put back is replaced, so the rollback (and its pause) is over.
    if mode == RunMode::Apply
        && trigger == Trigger::Manual
        && record.result != RunResult::Failed
        && r.store.rollback(app_id).is_some()
        && let Err(e) = r.store.set_rollback(app_id, None)
    {
        tracing::warn!(application = %app_id, error = %e, "could not clear the rollback");
    }
    Ok(record)
}

// A scheduled apply never runs over a rollback: git still says HEAD, and
// applying it would undo what the rollback put back. Decided by the run, under
// the application's lock: a check that looked for the rollback before waiting
// for the lock (held by that very rollback) used to find none, wait, and apply
// HEAD right after it.
fn effective_mode(mode: RunMode, trigger: Trigger, rolled_back: bool) -> RunMode {
    if mode == RunMode::Apply && trigger == Trigger::Schedule && rolled_back {
        RunMode::Plan
    } else {
        mode
    }
}

// Applies the desired state of an older commit, once: the checkout is left
// at HEAD, the commit's folder is exported beside it and reconciled from
// there. Then the application is marked rolled back, which stops checks from
// applying HEAD over it (see Rollback).
pub async fn rollback(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    revision: &str,
) -> Result<RunRecord, String> {
    let app = catalog
        .applications
        .get(app_id)
        .ok_or_else(|| format!("application '{app_id}' is not configured"))?;
    let app_lock = r.locks.get(app_id.to_string());
    let _running = app_lock.lock().await;
    // Kept until the run is over: dropping it removes the exported folder.
    let (exported, sha) = export_tree(r, catalog, app_id, app, revision).await?;
    let tree = exported.0.clone();
    let record = reconcile_tree(
        r,
        catalog,
        app_id,
        app,
        &tree,
        None,
        // An exported folder: no tree to compare drift with, and a rollback
        // is never measured against anyway.
        (Some(sha.clone()), None),
        RunMode::Apply,
        Trigger::Rollback,
    )
    .await;
    // Anything applied means the system now holds (some of) the older
    // state: pause, or the next check would put HEAD straight back. A run
    // whose every operation failed changed nothing, and pausing would only
    // stop the checks for no reason.
    if record.result != RunResult::Failed || record.applied.iter().any(|a| a.ok) {
        let mark = crate::domain::run::Rollback {
            revision: sha,
            at: now(),
        };
        r.store
            .set_rollback(app_id, Some(&mark))
            .map_err(|e| format!("rolled back, but could not record it: {e}"))?;
    }
    Ok(record)
}

// A commit's folder, exported beside the checkouts; removed when dropped.
struct ExportedTree(std::path::PathBuf);

impl Drop for ExportedTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn export_tree(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    app: &Application,
    revision: &str,
) -> Result<(ExportedTree, String), String> {
    let checkout = r.projects_dir.join(&app.project);
    // Hidden, beside the checkouts, and named per process and attempt so two
    // rollbacks never share one.
    static EXPORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tree = r.projects_dir.join(format!(
        ".rollback-{app_id}-{}-{}",
        std::process::id(),
        EXPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let exported = ExportedTree(tree.clone());

    let project = catalog
        .projects
        .get(&app.project)
        .ok_or_else(|| format!("project '{}' is not configured", app.project))?;
    let credentials = match project.credential_id() {
        Some(id) => r
            .secrets
            .resolve(id)
            .await
            .map_err(|e| format!("credential '{id}': {}", e.message))?,
        None => Default::default(),
    };
    // Exclusive: an older commit may first be fetched into the checkout,
    // which must not run alongside a pull of it.
    let lock = crate::application::projects::checkout_lock(&checkout);
    let _fetching = lock.write().await;
    // Bounded like a pull: fetching history from a remote that never
    // answers must not hold the application's lock forever.
    let started = std::time::Instant::now();
    let fetched = tokio::time::timeout(
        std::time::Duration::from_secs(project.timeout_seconds),
        r.git.export(
            &checkout,
            &project.remote(),
            &credentials,
            revision,
            app.folder(),
            &tree,
        ),
    )
    .await
    .map_err(|_| {
        format!(
            "reading commit {revision} took longer than {} s",
            project.timeout_seconds
        )
    })
    .and_then(|r| r.map_err(|e| e.message));
    crate::application::projects::count_git_request(
        &app.project,
        "fetch",
        fetched.is_ok(),
        started,
    );
    Ok((exported, fetched?))
}

// What a rollback to `revision` would change, without changing anything:
// the commit's desired state compared with what the system has now. Nothing
// is recorded (the application's status and plan stay those of HEAD); only
// the local copies are refreshed, as every listing refreshes them.
pub async fn preview_rollback(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    revision: &str,
) -> Result<PlanSnapshot, String> {
    let app = catalog
        .applications
        .get(app_id)
        .ok_or_else(|| format!("application '{app_id}' is not configured"))?;
    let app_lock = r.locks.get(app_id.to_string());
    let _running = app_lock.lock().await;
    let (tree, sha) = export_tree(r, catalog, app_id, app, revision).await?;
    let mut scratch = RunRecord {
        id: 0,
        trigger: Trigger::Rollback,
        mode: RunMode::Plan,
        result: RunResult::Succeeded,
        started_at: now(),
        finished_at: now(),
        revision: Some(sha.clone()),
        tree: None,
        planned: 0,
        operations: Vec::new(),
        applied: Vec::new(),
        errors: Vec::new(),
    };
    let mut plan = run(
        r,
        catalog,
        app_id,
        app,
        &tree.0,
        None,
        RunMode::Plan,
        &mut scratch,
    )
    .await
    .map_err(|errors| errors.join("; "))?;
    plan.revision = Some(sha);
    Ok(plan)
}

// One run against a tree of desired state: the checkout for every run but a
// rollback, which brings its own. The caller holds the application's lock.
#[allow(clippy::too_many_arguments)]
async fn reconcile_tree(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    app: &Application,
    checkout: &Path,
    reading: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    // The commit and the folder's tree read (GitPort::revision, ::tree).
    (revision, tree): (Option<String>, Option<String>),
    mode: RunMode,
    trigger: Trigger,
) -> RunRecord {
    let started_at = now();
    let mut record = RunRecord {
        id: r.states.next_run_id(),
        trigger,
        mode,
        result: RunResult::Succeeded,
        started_at,
        finished_at: started_at,
        revision: revision.clone(),
        tree,
        planned: 0,
        operations: Vec::new(),
        applied: Vec::new(),
        errors: Vec::new(),
    };

    let outcome = run(
        r,
        catalog,
        app_id,
        app,
        checkout,
        reading,
        mode,
        &mut record,
    )
    .await;
    record.finished_at = now();

    let (status, snapshot) = match outcome {
        Ok(snapshot) => {
            let status = status_of(&record, &snapshot);
            (status, Some(snapshot))
        }
        Err(errors) => {
            record.errors = errors;
            record.result = RunResult::Failed;
            (SyncStatus::Failed, None)
        }
    };
    let snapshot = snapshot.map(|mut s| {
        s.revision = revision.clone();
        s
    });
    // The result is decided; what is kept of it is bounded.
    record.bound();

    match record.result {
        RunResult::Failed | RunResult::Partial => {
            r.health.record_failure(app_id, &record.errors.join("; "))
        }
        _ => r.health.record_success(app_id),
    }
    // Where it stood before this run, read before the run is recorded over it.
    let mut before = r.states.baseline(app_id);
    before.rolled_back = r.store.rollback(app_id).is_some();
    r.states.record(app_id, status, snapshot, record.clone());
    if let Some(detected) = crate::application::webhooks::detect(&before, &record, status) {
        crate::application::webhooks::after_run(
            r.dispatcher,
            catalog.webhooks,
            &crate::application::webhooks::RunOutcome {
                application: app_id,
                project: &app.project,
                description: app.description.as_deref(),
                base_url: catalog.url,
                before: &before,
                status,
                record: &record,
                detected: &detected,
            },
        );
    }
    tracing::info!(
        application = %app_id,
        mode = ?mode,
        result = ?record.result,
        planned = record.planned,
        applied = record.applied.len(),
        revision = record.revision.as_deref().unwrap_or("-"),
        "reconcile finished"
    );
    record
}

fn status_of(record: &RunRecord, snapshot: &PlanSnapshot) -> SyncStatus {
    match record.result {
        RunResult::Failed | RunResult::Partial => SyncStatus::Failed,
        _ if !snapshot.errors.is_empty() => SyncStatus::Failed,
        // After an apply the snapshot is the re-listed state: what is left.
        _ if snapshot.operations.is_empty() => SyncStatus::Synced,
        _ => SyncStatus::OutOfSync,
    }
}

// `reading` is the checkout's shared lock when the tree is a checkout pulls
// can rewrite; it is released once the desired state is read.
#[allow(clippy::too_many_arguments)]
async fn run(
    r: &Reconciler<'_>,
    catalog: &Catalog<'_>,
    app_id: &str,
    app: &Application,
    checkout: &Path,
    reading: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    mode: RunMode,
    record: &mut RunRecord,
) -> Result<PlanSnapshot, Vec<String>> {
    if !catalog.projects.contains_key(&app.project) {
        return Err(vec![format!("project '{}' is not configured", app.project)]);
    }
    let Some(connector) = catalog.connectors.get(&app.connector) else {
        return Err(vec![format!(
            "connector '{}' is not configured",
            app.connector
        )]);
    };
    if !checkout.join(".git").exists() && !checkout.is_dir() {
        return Err(vec![format!(
            "project '{}' has no checkout yet (its first pull has not finished)",
            app.project
        )]);
    }

    // Desired state is read first: a file that does not parse fails the run
    // before any script is started.
    let docs = desired::load_folder(checkout, &app.path)?;
    drop(reading);

    // Credentials are resolved once per run, and the connector must speak this
    // protocol and say what it manages before it is asked for anything.
    // describe reads nothing from the system: it is the script answering
    // about itself.
    let (call, described) = prepare(r, app_id, app, connector)
        .await
        .map_err(|e| vec![e])?;

    // Then validated whole against what the connector manages, before
    // anything is listed or applied: a typo in one file must not reach the
    // system.
    let set = desired::validate(&app.connector, &described, &docs)?;
    // Paths from the repository's root, as a reader finds them in git.
    let folder = app.folder();
    let files: HashMap<(String, String), String> = desired::declared_in(&described, &docs)
        .into_iter()
        .map(|(id, file)| (id, format!("{folder}/{file}")))
        .collect();
    let with_file = |mut op: Operation| {
        op.file = files.get(&(op.kind.clone(), op.key.clone())).cloned();
        op
    };
    let kinds = desired::ordered_kinds(&set, &described);

    let mut operations: Vec<Operation> = Vec::new();
    let mut unmanaged: Vec<Operation> = Vec::new();
    let mut in_sync = 0usize;
    let mut errors: Vec<String> = Vec::new();
    // Kinds whose objects are unknown this run: listing them failed.
    let mut unlisted: Vec<&str> = Vec::new();

    for kind in &kinds {
        let actual = match refresh(r, &call, app_id, &app.connector, kind).await {
            Ok(a) => a,
            Err(e) => {
                errors.push(format!("{kind}: {e}"));
                unlisted.push(kind);
                continue;
            }
        };
        let key = &described
            .get(kind)
            .expect("validated against these kinds")
            .key;
        let diff = plan::diff(kind, key, &set[kind], &actual, app.prune);
        in_sync += diff.in_sync;
        unmanaged.extend(diff.unmanaged);
        operations.extend(diff.operations.into_iter().map(with_file));
    }
    record.planned = operations.len();
    record.operations = operations
        .iter()
        .take(crate::domain::run::RECORDED_OPERATIONS)
        .cloned()
        .collect();

    if mode == RunMode::Plan || operations.is_empty() {
        record.result = if !errors.is_empty() {
            RunResult::Failed
        } else if operations.is_empty() {
            RunResult::Succeeded
        } else {
            RunResult::OutOfSync
        };
        record.errors = errors.clone();
        return Ok(PlanSnapshot {
            checked_at: now(),
            revision: None,
            operations,
            unmanaged,
            in_sync,
            errors,
        });
    }

    // Apply. Creates and updates in dependency order (the order the kinds
    // were walked); deletes after them, in reverse, so a quota is removed
    // only once the export on it is gone.
    let (deletes, writes): (Vec<Operation>, Vec<Operation>) =
        operations.into_iter().partition(|o| o.op == OpKind::Delete);
    let ordered: Vec<Operation> = writes
        .into_iter()
        .chain(deletes.into_iter().rev())
        .collect();
    // Kinds with a delete that failed: their objects may still be there.
    let mut undeleted: Vec<&str> = Vec::new();
    for op in &ordered {
        // Dependencies hold both ways, and a kind whose state is unknown
        // breaks them. Its dependencies are not deleted (a namespace must not
        // go while entries the run could not list, or failed to delete, may
        // still be in it), and its dependents are not written (they would be
        // created against objects the run knows nothing about). Skipped, not
        // applied: the next run lists again and picks up from there.
        let blocked_by = if op.op == OpKind::Delete {
            unlisted
                .iter()
                .chain(&undeleted)
                .find(|k| described.depends_on(k, &op.kind))
        } else {
            unlisted.iter().find(|k| described.depends_on(&op.kind, k))
        };
        if let Some(blocker) = blocked_by {
            record.applied.push(AppliedOperation {
                kind: op.kind.clone(),
                key: op.key.clone(),
                op: op.op,
                ok: false,
                error: Some(format!(
                    "not applied: kind '{blocker}' could not be {}",
                    if unlisted.contains(blocker) {
                        "listed"
                    } else {
                        "fully deleted"
                    }
                )),
            });
            continue;
        }
        let payload = json!({"op": op.op.as_str(), "object": op.object});
        let started = std::time::Instant::now();
        let result = r.connector.apply(&call, &op.kind, &payload).await;
        count_call(app_id, &app.connector, "apply", result.is_ok(), started);
        let ok = result.is_ok();
        if !ok && op.op == OpKind::Delete && !undeleted.contains(&op.kind.as_str()) {
            undeleted.push(&op.kind);
        }
        record.applied.push(AppliedOperation {
            kind: op.kind.clone(),
            key: op.key.clone(),
            op: op.op,
            ok,
            error: result.err().map(|e| e.to_string()),
        });
    }

    // Re-list every kind: the local copy must show what the system has now,
    // and the snapshot shows what is still left to do (an operation the
    // connector accepted but the system normalised differently shows up here).
    let mut remaining: Vec<Operation> = Vec::new();
    let mut left_unmanaged: Vec<Operation> = Vec::new();
    let mut remaining_in_sync = 0usize;
    for kind in &kinds {
        match refresh(r, &call, app_id, &app.connector, kind).await {
            Ok(actual) => {
                let key = &described
                    .get(kind)
                    .expect("validated against these kinds")
                    .key;
                let d = plan::diff(kind, key, &set[kind], &actual, app.prune);
                remaining_in_sync += d.in_sync;
                remaining.extend(d.operations.into_iter().map(with_file));
                left_unmanaged.extend(d.unmanaged);
            }
            Err(e) => errors.push(format!("{kind}: re-list after apply: {e}")),
        }
    }

    let failed = record.applied.iter().filter(|a| !a.ok).count();
    record.result = if failed == 0 && errors.is_empty() {
        RunResult::Succeeded
    } else if failed == record.applied.len() && !record.applied.is_empty() {
        RunResult::Failed
    } else {
        RunResult::Partial
    };
    record.errors = errors.clone();
    record.errors.extend(record.applied.iter().filter_map(|a| {
        a.error
            .as_ref()
            .map(|e| format!("{} {} '{}': {e}", a.op.as_str(), a.kind, a.key))
    }));

    Ok(PlanSnapshot {
        checked_at: now(),
        revision: None,
        operations: remaining,
        unmanaged: left_unmanaged,
        in_sync: remaining_in_sync,
        errors,
    })
}

// Lists the kind on the system and writes the local copy. The diff is taken
// against what the system just said, never against an older file: applying
// against a stale copy would undo changes made since.
async fn refresh(
    r: &Reconciler<'_>,
    call: &ConnectorCall,
    app_id: &str,
    connector_id: &str,
    kind: &str,
) -> Result<Vec<Value>, String> {
    let started = std::time::Instant::now();
    let listed = r.connector.list(call, kind).await;
    count_call(app_id, connector_id, "list", listed.is_ok(), started);
    let actual = listed.map_err(|e| e.to_string())?;
    if let Err(e) = r.store.write(app_id, kind, &actual) {
        // The local copy is for people and for bootstrapping; failing to
        // write it does not make the diff wrong. Logged, not fatal.
        tracing::warn!(application = %app_id, kind = %kind, error = %e, "could not write the state copy");
    }
    Ok(actual)
}

// Builds the call for the application: script path (resolved into its
// project checkout per run, as unified-api resolves connector scripts),
// credentials, venv. Then checks the connector speaks this protocol.
async fn prepare(
    r: &Reconciler<'_>,
    app_id: &str,
    app: &Application,
    connector: &Connector,
) -> Result<(ConnectorCall, crate::domain::connector::Kinds), String> {
    let call = build_call(r, app, connector).await?;
    let started = std::time::Instant::now();
    let described = r.connector.describe(&call).await;
    count_call(
        app_id,
        &app.connector,
        "describe",
        described.is_ok(),
        started,
    );
    let description =
        described.map_err(|e| format!("connector '{}' describe: {e}", app.connector))?;
    if description.protocol != PROTOCOL_VERSION {
        return Err(format!(
            "connector '{}' speaks protocol {}, this version of unified-gitops speaks {}",
            app.connector, description.protocol, PROTOCOL_VERSION
        ));
    }
    let kinds = description
        .kinds(&app.connector)
        .map_err(|errors| errors.join("; "))?;
    r.described.record(&app.connector, kinds.clone());
    Ok((call, kinds))
}

pub async fn build_call(
    r: &Reconciler<'_>,
    app: &Application,
    connector: &Connector,
) -> Result<ConnectorCall, String> {
    let (script_path, venv_bin) = match &connector.project {
        Some(project_id) => (
            crate::application::scripts::resolve_script_path(
                r.projects_dir,
                &app.connector,
                project_id,
                &connector.path,
            ),
            crate::application::scripts::venv_bin_dir(r.projects_dir, project_id),
        ),
        None => (connector.path.clone(), None),
    };
    let credentials =
        crate::application::credentials::resolve_credentials(r.secrets, &app.credentials)
            .await
            .map_err(|e| e.message)?;
    Ok(ConnectorCall {
        script_path,
        timeout_seconds: connector.timeout_seconds,
        config: app.config.clone(),
        credentials,
        venv_bin,
    })
}

// Every call this instance makes to a system, through its connector: the traffic
// unified-gitops originates. Its own HTTP routes are mostly the UI polling it;
// these are what reach the Isilons and Ascenders. `list` reads, `apply`
// writes, `describe` is the handshake each run starts with.
fn count_call(
    app_id: &str,
    connector_id: &str,
    command: &'static str,
    ok: bool,
    started: std::time::Instant,
) {
    let outcome = if ok { "success" } else { "error" };
    metrics::counter!(
        "unified_gitops_connector_calls_total",
        "application" => app_id.to_string(),
        "connector" => connector_id.to_string(),
        "command" => command,
        "outcome" => outcome
    )
    .increment(1);
    metrics::histogram!(
        "unified_gitops_connector_call_duration_seconds",
        "application" => app_id.to_string(),
        "connector" => connector_id.to_string(),
        "command" => command
    )
    .record(started.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rollback_pauses_scheduled_applies_only() {
        use RunMode::{Apply, Plan};
        assert_eq!(effective_mode(Apply, Trigger::Schedule, false), Apply);
        assert_eq!(effective_mode(Apply, Trigger::Schedule, true), Plan);
        assert_eq!(effective_mode(Plan, Trigger::Schedule, false), Plan);
        // A manual Sync is what ends a rollback: it still applies.
        assert_eq!(effective_mode(Apply, Trigger::Manual, true), Apply);
    }
}
