use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Instant;

// Why a source's data looks the way it does. The cache entry says how OLD the
// data is; this says whether anything is still successfully refreshing it, and
// what went wrong if not. Without it a failing connector is indistinguishable
// from a long sync interval: both look like a dataset slowly getting older.
//
// Ages rather than wall-clock timestamps, because that is what the rest of the
// API reports (age_seconds everywhere) and because Instant is monotonic — it
// cannot be dragged backwards by an NTP correction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncHealth {
    /// Seconds since a sync was last attempted, successful or not
    pub last_attempt_age_seconds: Option<u64>,
    /// Seconds since the last sync that succeeded
    pub last_success_age_seconds: Option<u64>,
    /// The error from the most recent attempt, if it failed
    pub last_error: Option<String>,
    /// Failed attempts since the last success — 0 while healthy
    pub consecutive_failures: u32,
}

// What started a pull: the timer, a person (Pull, the API), a check or sync
// of an application (each pulls its project first), or the process itself
// (the clones at boot, a project added by a reload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunTrigger {
    Scheduled,
    Manual,
    Check,
    Startup,
}

impl RunTrigger {
    pub fn name(self) -> &'static str {
        match self {
            RunTrigger::Scheduled => "scheduled",
            RunTrigger::Manual => "manual",
            RunTrigger::Check => "check",
            RunTrigger::Startup => "startup",
        }
    }
}

// One pull, as the history keeps it: when, why and by whom, how it ended,
// and the commit it moved the checkout from and to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// When it started, unix seconds
    pub started_at: u64,
    pub duration_ms: u64,
    pub trigger: RunTrigger,
    /// Who asked, for a manual pull: the API key's name or the signed-in user
    pub by: Option<String>,
    /// Why it failed; None when it worked
    pub error: Option<String>,
    pub revision_before: Option<String>,
    pub revision: Option<String>,
}

// How many pulls each project keeps. In memory only, like the health above
// and an application's run history: a restart starts it over.
pub const HISTORY: usize = 50;

// An error is kept whole up to this length: git's stderr can be long.
const HISTORY_ERROR: usize = 2000;

// What is stored internally: Instants, converted to ages when read.
#[derive(Debug)]
struct Record {
    last_attempt: Instant,
    last_success: Option<Instant>,
    last_error: Option<String>,
    consecutive_failures: u32,
}

// Shared, mutable, keyed by whatever the owning registry tracks: source ids
// for syncs, enricher ids for enrichment runs, project ids for git pulls, and
// a single well-known key for the cache snapshot task. One registry instance
// per kind (see AppState), so the id spaces cannot collide.
//
// It lives outside the cache on purpose: the interesting case is a source with
// NO cache entry — one that has never synced successfully — and a registry
// keyed independently keeps that record instead of having nowhere to put it.
// The same reasoning extends to the other kinds: a periodic task that is
// failing leaves nothing behind BUT this record.
//
// A std Mutex rather than the DashMap used by the cache: writes happen once per
// sync attempt (seconds apart at best), so there is no contention to design
// for, and this keeps the domain layer free of extra dependencies.
#[derive(Debug, Default)]
pub struct SyncHealthRegistry {
    inner: Mutex<HashMap<String, Record>>,
    // The last runs of each id, newest first: what the health above sums up
    // (the last attempt, the streak), one row per run.
    history: Mutex<HashMap<String, VecDeque<RunRecord>>>,
}

impl SyncHealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record_success(&self, source_id: &str) {
        let now = Instant::now();
        self.with_map(|map| {
            map.insert(
                source_id.to_string(),
                Record {
                    last_attempt: now,
                    last_success: Some(now),
                    last_error: None,
                    consecutive_failures: 0,
                },
            );
        });
    }

    pub fn record_failure(&self, source_id: &str, error: &str) {
        let now = Instant::now();
        self.with_map(|map| {
            // A failure keeps the last success: "last worked 6 hours ago,
            // failing since" is the whole point.
            let entry = map.entry(source_id.to_string()).or_insert(Record {
                last_attempt: now,
                last_success: None,
                last_error: None,
                consecutive_failures: 0,
            });
            entry.last_attempt = now;
            entry.last_error = Some(error.to_string());
            entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        });
    }

    pub fn get(&self, source_id: &str) -> Option<SyncHealth> {
        let now = Instant::now();
        self.with_map(|map| {
            map.get(source_id).map(|record| SyncHealth {
                last_attempt_age_seconds: Some(now.duration_since(record.last_attempt).as_secs()),
                last_success_age_seconds: record
                    .last_success
                    .map(|at| now.duration_since(at).as_secs()),
                last_error: record.last_error.clone(),
                consecutive_failures: record.consecutive_failures,
            })
        })
    }

    // A panic elsewhere while this lock was held must not take the API down:
    // health data is a side-channel for humans, so recovering the map from a
    // poisoned lock is strictly better than propagating the panic into every
    // later request.
    fn with_map<T>(&self, f: impl FnOnce(&mut HashMap<String, Record>) -> T) -> T {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut map)
    }

    // Keeps one run in the id's history, newest first, the oldest dropped
    // past HISTORY.
    pub fn record_run(&self, id: &str, mut run: RunRecord) {
        if let Some(error) = &mut run.error
            && error.len() > HISTORY_ERROR
        {
            let mut cut = HISTORY_ERROR;
            while !error.is_char_boundary(cut) {
                cut -= 1;
            }
            error.truncate(cut);
            error.push('…');
        }
        let mut map = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let runs = map.entry(id.to_string()).or_default();
        runs.push_front(run);
        runs.truncate(HISTORY);
    }

    // The id's runs, newest first; empty when it has none yet.
    pub fn history(&self, id: &str) -> Vec<RunRecord> {
        let map = self
            .history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(id)
            .map(|runs| runs.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(started_at: u64) -> RunRecord {
        RunRecord {
            started_at,
            duration_ms: 10,
            trigger: RunTrigger::Scheduled,
            by: None,
            error: None,
            revision_before: None,
            revision: None,
        }
    }

    #[test]
    fn the_history_is_newest_first_and_keeps_the_last_fifty() {
        let registry = SyncHealthRegistry::new();
        assert!(registry.history("prj-a").is_empty());
        for at in 0..60 {
            registry.record_run("prj-a", run(at));
        }
        let history = registry.history("prj-a");
        assert_eq!(history.len(), HISTORY);
        assert_eq!(history[0].started_at, 59, "newest first");
        assert_eq!(history[HISTORY - 1].started_at, 10, "the oldest dropped");
    }

    #[test]
    fn unknown_source_has_no_health() {
        let registry = SyncHealthRegistry::new();
        assert_eq!(registry.get("src-nope"), None);
    }

    #[test]
    fn success_clears_the_error_and_the_failure_count() {
        let registry = SyncHealthRegistry::new();

        registry.record_failure("src-a", "connection refused");
        registry.record_failure("src-a", "connection refused");
        let health = registry.get("src-a").unwrap();
        assert_eq!(health.consecutive_failures, 2);
        assert_eq!(health.last_error.as_deref(), Some("connection refused"));
        assert_eq!(health.last_success_age_seconds, None);

        registry.record_success("src-a");
        let health = registry.get("src-a").unwrap();
        assert_eq!(health.consecutive_failures, 0);
        assert_eq!(health.last_error, None);
        assert!(health.last_success_age_seconds.is_some());
    }

    #[test]
    fn failure_after_success_keeps_the_last_success() {
        let registry = SyncHealthRegistry::new();

        registry.record_success("src-a");
        registry.record_failure("src-a", "script exited 1");

        let health = registry.get("src-a").unwrap();
        // "last worked N seconds ago, failing since" — the reason a failure
        // must not wipe the success timestamp
        assert!(health.last_success_age_seconds.is_some());
        assert_eq!(health.last_error.as_deref(), Some("script exited 1"));
        assert_eq!(health.consecutive_failures, 1);
    }

    #[test]
    fn each_source_is_tracked_separately() {
        let registry = SyncHealthRegistry::new();

        registry.record_failure("src-a", "boom");
        registry.record_success("src-b");

        assert_eq!(registry.get("src-a").unwrap().consecutive_failures, 1);
        assert_eq!(registry.get("src-b").unwrap().consecutive_failures, 0);
    }
}
