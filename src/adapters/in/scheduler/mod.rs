use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::{Duration, Interval, MissedTickBehavior, interval};
use tracing::{debug, error, info};

use crate::AppState;
use crate::application::projects::{Announce, sync_project};
use crate::application::reconcile::pull_and_reconcile;
use crate::domain::project::GitProject;
use crate::domain::run::{RunMode, RunResult, Trigger};
use crate::domain::sync_health::SyncHealthRegistry;
use crate::ports::git::GitPort;
use crate::ports::secrets::SecretsPort;

// The scheduler is another "driving adapter", just like HTTP handlers:
// it triggers the same use cases from application/, just by time instead
// of by request. There is no business logic here — only timers and logs.

// Every loop below awaits its work inline, so a run that outlasts its interval
// leaves ticks behind it. tokio's default (`Burst`) then fires those ticks
// back-to-back to catch up: a sync that took an hour on a ten-minute interval
// is followed by five more with no pause between them — hammering, at exactly
// the moment the thing being synced is already struggling.
//
// `Skip` drops the ticks that were missed and resumes on the original schedule,
// so a slow run costs the runs it displaced and nothing more. `Delay` would
// also avoid the burst, but it shifts every later tick by the overrun, and a
// source configured to sync on the hour should stay on the hour.
fn ticker(interval_seconds: u64) -> Interval {
    let mut ticker = interval(Duration::from_secs(interval_seconds));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker
}

// The widest spread a task's schedule is shifted by, and the ceiling on how
// many intervals a failing task backs off to.
const MAX_JITTER_SECONDS: u64 = 30;
const MAX_BACKOFF_INTERVALS: u32 = 8;

// Parse a source's `schedule` cron expression. Standard 5-field cron with an
// OPTIONAL leading seconds field (6 fields), evaluated in UTC — containers
// run UTC and a schedule that shifts with the host's timezone database is a
// surprise nobody asked for. Config validation calls this at startup, so a
// bad expression fails the deploy naming the source, never the first tick.
pub(crate) fn parse_cron(expression: &str) -> Result<croner::Cron, String> {
    croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Optional)
        .build()
        .parse(expression)
        .map_err(|e| e.to_string())
}

// How long until the schedule's next occurrence after `now`. None when the
// pattern has no future occurrence at all (croner gives up past year 5000 —
// practically a misconfiguration).
fn next_cron_delay(cron: &croner::Cron, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let next = cron.find_next_occurrence(&now, false).ok()?;
    (next - now).to_std().ok()
}

// How a source's sync task paces itself: a fixed interval (jittered, ticks
// with Skip semantics) or a cron schedule (exact times, deliberately
// unjittered — "on the hour" was chosen by a person, and shifting it would be
// a schedule change). Backoff works identically in both: a failing source
// lets occurrences pass, 1, 2, 4, up to 8 apart.
// The Cron variant is boxed for size (a parsed pattern is ~300 bytes, an
// interval is 8): one Cadence exists per scheduled source, so this is about
// clippy's variant-size lint, not memory that matters.
#[derive(Clone)]
enum Cadence {
    Interval(u64),
    Cron(Box<croner::Cron>),
}

// A per-task offset added before the first tick, so every task does not fire
// at the same instant — at boot that meant every source gathering at once
// (and, since tokio intervals keep their phase, colliding again at every
// common multiple forever).
//
// Deterministic (a hash of the id) rather than random: the same config spreads
// the same way on every boot, which makes load patterns reproducible, and it
// needs no RNG dependency. Capped at MAX_JITTER_SECONDS and at the interval
// itself — spreading a 10-second source across 30 seconds would be a schedule
// change, not a jitter.
fn startup_jitter(id: &str, interval_seconds: u64) -> Duration {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let window_ms = interval_seconds.min(MAX_JITTER_SECONDS) * 1000;
    if window_ms == 0 {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    Duration::from_millis(hasher.finish() % window_ms)
}

// How many ticks a task sits out after a failure: 0 after the first (retry on
// the very next tick — most failures are transient), then exponentially more,
// capped at MAX_BACKOFF_INTERVALS. A failing source used to hammer its
// struggling system at exactly sync_interval_seconds forever; now the attempts
// land 1, 2, 4, 8, 8... intervals apart while `sync_health` carries the streak.
//
// Ticks rather than clock math: the ticker already owns the schedule (with
// Skip semantics), so backing off is just letting some ticks pass — attempts
// stay aligned to the configured cadence instead of drifting.
fn ticks_to_skip(consecutive_failures: u32) -> u32 {
    if consecutive_failures == 0 {
        return 0;
    }
    2u32.saturating_pow(consecutive_failures - 1)
        .min(MAX_BACKOFF_INTERVALS)
        - 1
}

// Spawn a periodic task through a supervisor: a panic in the body is counted,
// logged, and the body is restarted after `restart_delay` — instead of the
// tokio default, where the task dies silently and (for a sync task) that
// source simply stops syncing until someone notices the data went stale.
//
// `factory` builds a fresh body per (re)start, which is why it is a closure
// and not a future. A body that RETURNS is done on purpose (shutdown) and is
// not restarted; only a panic is.
fn spawn_supervised<F, Fut>(
    task: String,
    restart_delay: Duration,
    factory: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            match tokio::spawn(factory()).await {
                Ok(()) => return,
                Err(e) if e.is_panic() => {
                    metrics::counter!(
                        "unified_gitops_scheduler_task_panics_total",
                        "task" => task.clone(),
                    )
                    .increment(1);
                    error!(task = %task, "Scheduler task panicked — restarting it");
                    tokio::time::sleep(restart_delay).await;
                }
                // Cancelled (runtime shutting down): nothing to restart into
                Err(_) => return,
            }
        }
    })
}

// One scheduled check of an application, with the backoff bookkeeping. The
// project is pulled first (inside pull_and_reconcile), so a commit is picked up
// at the next check, not at the project's own pull interval.
async fn scheduled_reconcile(
    state: &AppState,
    app_id: &str,
    consecutive_failures: &mut u32,
    skip: &mut u32,
) {
    let config = state.config();
    let Some(app) = config.applications.get(app_id) else {
        return;
    };

    // auto_sync applies what a check finds; otherwise the check only reports.
    // Whether a rollback pauses it is the run's to decide, under the
    // application's lock (reconcile::effective_mode): read here, before the
    // pull and the wait for that lock, it could be stale by the time the run
    // starts.
    let mode = if app.auto_sync {
        RunMode::Apply
    } else {
        RunMode::Plan
    };
    let failed = match pull_and_reconcile(
        &state.reconciler(),
        &config.catalog(),
        app_id,
        mode,
        Trigger::Schedule,
    )
    .await
    {
        Ok(record) => matches!(record.result, RunResult::Failed | RunResult::Partial),
        Err(e) => {
            error!(application = %app_id, error = %e, "Reconcile could not start");
            true
        }
    };

    if failed {
        *consecutive_failures = consecutive_failures.saturating_add(1);
        *skip = ticks_to_skip(*consecutive_failures);
    } else {
        *consecutive_failures = 0;
    }
}

// One scheduled project pull with the backoff bookkeeping, shared by the
// interval and cron loops so the two cannot drift.
#[allow(clippy::too_many_arguments)]
async fn scheduled_project_pull(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    venv: &dyn crate::ports::venv::VenvPort,
    health: &SyncHealthRegistry,
    project_id: &str,
    project: &GitProject,
    projects_dir: &std::path::Path,
    // The configuration of the moment, for what the pull tells.
    state: &AppState,
    consecutive_failures: &mut u32,
    skip: &mut u32,
) {
    let config = state.config();
    let catalog = config.catalog();
    match sync_project(
        git,
        secrets,
        venv,
        health,
        project_id,
        project,
        projects_dir,
        Some(Announce {
            dispatcher: &state.dispatcher,
            catalog: &catalog,
        }),
        crate::application::projects::RunOrigin::of(
            crate::domain::sync_health::RunTrigger::Scheduled,
        ),
    )
    .await
    {
        Ok(()) => {
            *consecutive_failures = 0;
            info!(project = %project_id, "Project updated");
        }
        Err(e) => {
            *consecutive_failures = consecutive_failures.saturating_add(1);
            *skip = ticks_to_skip(*consecutive_failures);
            error!(
                project = %project_id,
                error = %e,
                next_attempt_in_occurrences = *skip + 1,
                "Project update failed"
            );
        }
    }
}

// Every start_* function takes the shutdown receiver and returns the spawned
// handles: on SIGTERM main flips the watch, each body returns at its next
// wait point (a running sync is finished, never cut mid-write), and main joins
// the handles before the final snapshot — so the snapshot cannot serialize a
// cache a sync task is still mutating.
// The scheduler as something that can be REBUILT, not only started.
//
// Every periodic task captures the source (or enricher, or project) it was
// spawned for, which is exactly what makes it cheap — and exactly what makes
// it wrong the moment the configuration changes underneath it. So a reload
// does not try to talk to the running tasks: it replaces them. One generation
// of tasks is told to stop, a new generation is spawned from the new
// snapshot, and the difference is invisible to everything else.
//
// The outgoing generation is NOT waited for here. Its tasks stop at their
// next wait point, which for a task in the middle of a run means after that
// run finishes — an apply is never cut between operations. It is safe to
// overlap because each application holds its own lock for the whole run
// (application::reconcile::Locks): the outgoing task's last run and the
// incoming task's first cannot interleave.
//
// Shutdown still drains everything, outgoing generations included, because
// main's final cache snapshot may not run while any task can still write.
pub fn start_supervisor(
    state: Arc<AppState>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut reload = state.reload.subscribe();
        // Generations that have been told to stop and are finishing their
        // in-flight work. Pruned on every reload so a long-lived process does
        // not accumulate handles of tasks that ended hours ago.
        let mut draining: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        loop {
            if *shutdown.borrow() {
                return;
            }

            let generation = state.reload.generation();
            let (stop, stop_rx) = watch::channel(false);

            let mut handles = start_application_tasks(Arc::clone(&state), stop_rx.clone());
            handles.extend(start_project_tasks(Arc::clone(&state), stop_rx.clone()));
            // A project that arrived WITH this reload has no checkout yet, and
            // the scripts of any source pointing into it would be missing
            // until its first periodic pull — which for a project without an
            // interval is never. Boot already clones (main), so this is only
            // for what a reload added.
            if generation > 0 {
                handles.extend(clone_missing_checkouts(Arc::clone(&state)));
            }

            info!(generation, tasks = handles.len(), "Scheduler tasks started");

            tokio::select! {
                _ = shutdown.changed() => {
                    let _ = stop.send(true);
                    draining.extend(handles);
                    for handle in draining {
                        let _ = handle.await;
                    }
                    return;
                }
                _ = reload.changed() => {
                    let _ = stop.send(true);
                    draining.retain(|handle| !handle.is_finished());
                    draining.extend(handles);
                }
            }
        }
    })
}

// The project tasks, wired from the state rather than from main's locals —
// which is what lets a reload restart them with a different projects.yaml.
fn start_project_tasks(
    state: Arc<AppState>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let projects = state.config().projects.clone();
    if projects.is_empty() {
        return Vec::new();
    }
    start_project_sync_tasks(state, projects, shutdown)
}

// One clone attempt per configured project that has no checkout on disk,
// concurrently, each bounded by its own timeout_seconds (applied inside
// sync_project). A project that is already checked out is left alone: its
// periodic task will pull it, and re-cloning on every reload would throw away
// a working tree for nothing.
fn clone_missing_checkouts(state: Arc<AppState>) -> Vec<tokio::task::JoinHandle<()>> {
    let projects = state.config().projects.clone();
    let mut handles = Vec::new();

    for (project_id, project) in projects {
        let state = Arc::clone(&state);
        handles.push(tokio::spawn(async move {
            let checkout = state.projects_dir.join(&project_id).join(".git");
            if tokio::fs::try_exists(&checkout).await.unwrap_or(false) {
                return;
            }
            info!(project = %project_id, "New project — cloning its checkout");
            let config = state.config();
            let catalog = config.catalog();
            match sync_project(
                &*state.git,
                &*state.secrets,
                &*state.venv,
                &state.project_health,
                &project_id,
                &project,
                &state.projects_dir,
                Some(Announce {
                    dispatcher: &state.dispatcher,
                    catalog: &catalog,
                }),
                crate::application::projects::RunOrigin::of(
                    crate::domain::sync_health::RunTrigger::Startup,
                ),
            )
            .await
            {
                Ok(()) => info!(project = %project_id, "Project checkout ready"),
                Err(e) => error!(project = %project_id, error = %e, "Project sync failed"),
            }
        }));
    }

    handles
}

// One task per application: check every sync_interval_seconds, jittered so
// applications sharing an interval do not hit their systems in lockstep,
// backing off 1, 2, 4, 8 intervals while it keeps failing.
pub fn start_application_tasks(
    state: Arc<AppState>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut handles = Vec::new();
    let config = state.config();

    for (app_id, app) in &config.applications {
        let interval_secs = app.sync_interval_seconds.max(1);
        info!(application = %app_id, interval_secs, auto_sync = app.auto_sync, "Application scheduled");

        let task_state = Arc::clone(&state);
        let task_app_id = app_id.clone();
        let task_shutdown = shutdown.clone();

        handles.push(spawn_supervised(
            format!("application:{app_id}"),
            Duration::from_secs(interval_secs),
            move || {
                let state = Arc::clone(&task_state);
                let app_id = task_app_id.clone();
                let mut shutdown = task_shutdown.clone();
                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(startup_jitter(&app_id, interval_secs)) => {}
                        _ = shutdown.changed() => return,
                    }
                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;
                    let mut ticker = ticker(interval_secs);
                    loop {
                        tokio::select! {
                            _ = ticker.tick() => {}
                            _ = shutdown.changed() => return,
                        }
                        if skip > 0 {
                            skip -= 1;
                            debug!(application = %app_id, ticks_left = skip, "Backing off, tick skipped");
                            continue;
                        }
                        scheduled_reconcile(&state, &app_id, &mut consecutive_failures, &mut skip).await;
                    }
                }
            },
        ));
    }
    handles
}

// Periodic re-pull of git project checkouts, one task per project with a
// cadence. Each pull reads the configuration of the moment from the state,
// for what it tells (application::webhooks).
pub fn start_project_sync_tasks(
    state: Arc<AppState>,
    projects: HashMap<String, GitProject>,
    shutdown: watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let git = Arc::clone(&state.git);
    let secrets = Arc::clone(&state.secrets);
    let venv = Arc::clone(&state.venv);
    let health = Arc::clone(&state.project_health);
    let projects_dir = state.projects_dir.clone();
    let mut handles = Vec::new();

    for (project_id, project) in projects {
        let cadence = match (project.schedule.as_deref(), project.sync_interval_seconds) {
            (Some(expression), _) => match parse_cron(expression) {
                Ok(cron) => {
                    info!(project = %project_id, schedule = %expression, "Project scheduled (cron, UTC)");
                    Cadence::Cron(Box::new(cron))
                }
                Err(e) => {
                    error!(project = %project_id, error = %e, "Invalid cron schedule — project will NOT re-pull");
                    continue;
                }
            },
            (None, Some(secs)) if secs > 0 => {
                info!(project = %project_id, interval_secs = secs, "Project scheduled");
                Cadence::Interval(secs)
            }
            _ => continue,
        };
        let restart_delay = match &cadence {
            Cadence::Interval(secs) => Duration::from_secs(*secs),
            Cadence::Cron(_) => Duration::from_secs(60),
        };

        let task_git = Arc::clone(&git);
        let task_secrets = Arc::clone(&secrets);
        let task_venv = Arc::clone(&venv);
        let task_health = Arc::clone(&health);
        let task_projects_dir = projects_dir.clone();
        let task_state = Arc::clone(&state);
        let task_cadence = cadence;
        let task_shutdown = shutdown.clone();

        handles.push(spawn_supervised(
            format!("project:{}", project_id),
            restart_delay,
            move || {
                let git = Arc::clone(&task_git);
                let secrets = Arc::clone(&task_secrets);
                let venv = Arc::clone(&task_venv);
                let health = Arc::clone(&task_health);
                let projects_dir = task_projects_dir.clone();
                let state = Arc::clone(&task_state);
                let project_id = project_id.clone();
                let project = project.clone();
                let cadence = task_cadence.clone();
                let mut shutdown = task_shutdown.clone();

                async move {
                    if *shutdown.borrow() {
                        return;
                    }
                    let mut consecutive_failures: u32 = 0;
                    let mut skip: u32 = 0;

                    match cadence {
                        Cadence::Interval(interval_secs) => {
                            tokio::select! {
                                _ = tokio::time::sleep(startup_jitter(&project_id, interval_secs)) => {}
                                _ = shutdown.changed() => return,
                            }
                            let mut ticker = ticker(interval_secs);
                            // The boot sequence already cloned; skip the immediate first tick
                            ticker.tick().await;

                            loop {
                                tokio::select! {
                                    _ = ticker.tick() => {}
                                    _ = shutdown.changed() => return,
                                }
                                if skip > 0 {
                                    skip -= 1;
                                    debug!(project = %project_id, ticks_left = skip, "Backing off, tick skipped");
                                    continue;
                                }
                                scheduled_project_pull(
                                    &*git,
                                    &*secrets,
                                    &*venv,
                                    &health,
                                    &project_id,
                                    &project,
                                    &projects_dir,
                                    &state,
                                    &mut consecutive_failures,
                                    &mut skip,
                                )
                                .await;
                            }
                        }
                        // No jitter and no first-tick handling for cron: the
                        // first occurrence is in the future by construction,
                        // so the boot clone is never immediately repeated
                        Cadence::Cron(cron) => loop {
                            let Some(delay) = next_cron_delay(&cron, chrono::Utc::now()) else {
                                error!(project = %project_id, "Cron schedule has no future occurrence — project will NOT re-pull");
                                return;
                            };
                            tokio::select! {
                                _ = tokio::time::sleep(delay) => {}
                                _ = shutdown.changed() => return,
                            }
                            if skip > 0 {
                                skip -= 1;
                                debug!(project = %project_id, occurrences_left = skip, "Backing off, occurrence skipped");
                                continue;
                            }
                            scheduled_project_pull(
                                &*git,
                                &*secrets,
                                &*venv,
                                &health,
                                &project_id,
                                &project,
                                &projects_dir,
                                &state,
                                &mut consecutive_failures,
                                &mut skip,
                            )
                            .await;
                        },
                    }
                }
            },
        ));
    }

    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_run_that_overruns_its_interval_does_not_burst() {
        let mut t = ticker(10);
        t.tick().await;
        tokio::time::advance(Duration::from_secs(35)).await;
        let before = tokio::time::Instant::now();
        t.tick().await;
        // Skip: the missed ticks are dropped, not fired back to back.
        t.tick().await;
        assert!(tokio::time::Instant::now() > before);
    }

    #[test]
    fn cron_expressions_parse_in_five_and_six_field_forms() {
        assert!(parse_cron("*/5 * * * *").is_ok());
        assert!(parse_cron("0 */5 * * * *").is_ok());
        assert!(parse_cron("not a cron").is_err());
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let skipped: Vec<u32> = (0..7).map(ticks_to_skip).collect();
        assert_eq!(skipped, vec![0, 0, 1, 3, 7, 7, 7]);
    }

    #[test]
    fn jitter_is_bounded_and_deterministic() {
        let a = startup_jitter("app-a", 300);
        assert_eq!(a, startup_jitter("app-a", 300));
        assert!(a < Duration::from_secs(MAX_JITTER_SECONDS));
        assert!(startup_jitter("app-a", 5) < Duration::from_secs(5));
        assert_eq!(startup_jitter("app-a", 0), Duration::ZERO);
    }

    #[tokio::test]
    async fn a_panicking_task_body_is_restarted() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let runs = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&runs);
        let handle = spawn_supervised("test".into(), Duration::from_millis(1), move || {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    panic!("first run panics");
                }
            }
        });
        handle.await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }
}
