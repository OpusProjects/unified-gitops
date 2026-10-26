use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use utoipa::ToSchema;

use crate::domain::plan::{OpKind, Operation};

// How an application stands against its system, as of the last check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    // Never checked yet (just started, or its checkout is not there yet).
    Unknown,
    // Every declared object exists as declared.
    Synced,
    // The last check found operations to do: new commits not applied yet, or
    // drift someone made by hand.
    OutOfSync,
    // The last check or apply could not finish: invalid desired state, a
    // connector that failed, a system that did not answer.
    Failed,
    // Rolled back to an older commit: deliberately not what git says, and
    // automatic syncs paused until the next manual Sync. Shown over synced
    // and out of sync (which compare with a commit the system no longer
    // follows), never over failed.
    RolledBack,
}

impl SyncStatus {
    pub fn with_rollback(self, rolled_back: bool) -> SyncStatus {
        match self {
            SyncStatus::Failed => SyncStatus::Failed,
            _ if rolled_back => SyncStatus::RolledBack,
            other => other,
        }
    }
}

// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    // The periodic check (and, with auto_sync, the apply it led to).
    Schedule,
    // A person or a pipeline asked: POST .../sync or .../refresh.
    Manual,
    // POST .../rollback: an older commit's desired state, applied once.
    Rollback,
}

// An application rolled back to an older commit. While it stands, a check
// never applies (auto_sync or not): git still says HEAD, and applying it
// would undo the rollback. The next manual Sync applies HEAD and ends it.
#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize, ToSchema)]
pub struct Rollback {
    // The commit whose desired state was applied.
    pub revision: String,
    // Unix seconds.
    pub at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    // Compare only: list, diff, report.
    Plan,
    // Compare, then carry the operations out.
    Apply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RunResult {
    // Plan: nothing to do. Apply: every operation succeeded (or none needed).
    Succeeded,
    // Plan only: there are operations, nothing was changed.
    OutOfSync,
    // Apply: some operations succeeded, at least one failed. The next run
    // lists again and picks up from wherever the system really is.
    Partial,
    Failed,
}

// One operation as carried out.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AppliedOperation {
    pub kind: String,
    pub key: String,
    pub op: OpKind,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// One reconcile, kept in the application's history: which commit, what it
// found, what it did. Rollback is `git revert`; this is how you find which
// commit to revert.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RunRecord {
    pub id: u64,
    pub trigger: Trigger,
    pub mode: RunMode,
    pub result: RunResult,
    // Unix seconds.
    pub started_at: u64,
    pub finished_at: u64,
    // The commit of the checkout the run read; None when it could not tell.
    pub revision: Option<String>,
    // Operations the plan found.
    pub planned: usize,
    // Those operations themselves, each with its fields' old and new values,
    // so a past run's changes can be read as a diff: what it found (a check)
    // or set out to apply (a sync). At most RECORDED_OPERATIONS; `planned`
    // keeps the full count.
    pub operations: Vec<crate::domain::plan::Operation>,
    // What an apply did, operation by operation. Empty for a plan.
    pub applied: Vec<AppliedOperation>,
    // Why the run failed, or which kind could not be listed.
    pub errors: Vec<String>,
}

// The last comparison, kept whole so the API can show the pending diff.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PlanSnapshot {
    pub checked_at: u64,
    pub revision: Option<String>,
    pub operations: Vec<Operation>,
    // Objects on the system git does not declare (prune off).
    pub unmanaged: Vec<Operation>,
    pub in_sync: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct AppState {
    pub status: SyncStatus,
    pub revision: Option<String>,
    pub plan: Option<PlanSnapshot>,
    // Most recent first.
    pub history: Vec<RunRecord>,
}

// How many runs each application keeps. In memory only, like sync health: a
// restart starts the history over. Enough to see the last day of hourly
// checks plus the applies in between.
const HISTORY: usize = 50;

// How many operations one run keeps: enough to read any ordinary change, and
// a bound on what a run that found thousands holds in memory, times HISTORY.
pub const RECORDED_OPERATIONS: usize = 200;

#[derive(Default)]
struct Entry {
    status: Option<SyncStatus>,
    revision: Option<String>,
    plan: Option<PlanSnapshot>,
    history: VecDeque<RunRecord>,
}

// Every application's state, behind one lock. Writes happen once per run, so
// a single RwLock is plenty; reads (the API) are cheap clones.
#[derive(Default)]
pub struct AppStateRegistry {
    entries: RwLock<HashMap<String, Entry>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl AppStateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next_run_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1
    }

    pub fn record(
        &self,
        app_id: &str,
        status: SyncStatus,
        plan: Option<PlanSnapshot>,
        run: RunRecord,
    ) {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(app_id.to_string()).or_default();
        entry.status = Some(status);
        if run.revision.is_some() {
            entry.revision = run.revision.clone();
        }
        if plan.is_some() {
            entry.plan = plan;
        }
        entry.history.push_front(run);
        entry.history.truncate(HISTORY);
    }

    pub fn get(&self, app_id: &str) -> AppState {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        match entries.get(app_id) {
            Some(e) => AppState {
                status: e.status.unwrap_or(SyncStatus::Unknown),
                revision: e.revision.clone(),
                plan: e.plan.clone(),
                history: e.history.iter().cloned().collect(),
            },
            None => AppState {
                status: SyncStatus::Unknown,
                revision: None,
                plan: None,
                history: Vec::new(),
            },
        }
    }

    pub fn status(&self, app_id: &str) -> SyncStatus {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        entries
            .get(app_id)
            .and_then(|e| e.status)
            .unwrap_or(SyncStatus::Unknown)
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
