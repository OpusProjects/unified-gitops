use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use dashmap::DashMap;
use tokio::sync::RwLock;

use crate::application::reconcile::Catalog;
use crate::application::webhooks::{Dispatcher, PullEvent, PullOutcome, after_pull};
use crate::domain::project::GitProject;
use crate::domain::sync_health::{RunRecord, RunTrigger, SyncHealthRegistry};
use crate::domain::webhook::Event;
use crate::ports::git::{CommitRange, GitPort};
use crate::ports::secrets::SecretsPort;
use crate::ports::venv::{VenvOutcome, VenvPort};

// The use case "bring a project checkout up to date": resolve its credential
// (if any), let the GitPort clone or update the directory, and — for projects
// that opted in — bring the Python virtualenv in step with the checkout's
// requirements.txt. Shared by the boot sequence in main and the periodic
// scheduler task, like sync/enrich.
//
// Health is recorded here, at the one place every project sync passes through,
// for the same reason sync_source records sync health: a checkout stuck on a
// stale commit because every pull fails used to be a log line per interval and
// nothing an operator could query or alert on. A broken requirements.txt is
// the same kind of invisible without this: the scripts would fail at import
// time, one confusing sync error per source, with the actual cause nowhere.
// One lock per checkout directory. Several things write to the same
// checkout: every application on the project pulls it before its run, the
// project's own task pulls it, a reload clones what is missing, a rollback
// fetches history into it. Unserialised, two `fetch` + `reset --hard` collide
// (`index.lock`, "shallow file changed"), a project is cloned twice, and a
// run reading its desired state while another pull rewrites the files can
// read a torn tree: a file briefly missing is, with prune, a delete of every
// object it declares.
//
// Writers (pulls, history fetches) take it exclusively; a run takes it
// shared, only while it reads the commit and its desired state, so a pull
// waits for those reads and never for a whole apply.
//
// Keyed by the directory rather than the project id, and process-wide
// (OnceLock: initialised on first use, then shared), because the directory is
// what is shared: boot clones run before AppState exists, and every caller
// already has the path.
pub fn checkout_lock(dir: &Path) -> Arc<RwLock<()>> {
    static LOCKS: OnceLock<DashMap<PathBuf, Arc<RwLock<()>>>> = OnceLock::new();
    LOCKS
        .get_or_init(DashMap::new)
        .entry(dir.to_path_buf())
        .or_default()
        .clone()
}

// Where a pull tells what it did (application::webhooks): every caller that
// has the configuration passes it. Without it (tests) a pull tells nothing.
#[derive(Clone, Copy)]
pub struct Announce<'a> {
    pub dispatcher: &'a Dispatcher,
    pub catalog: &'a Catalog<'a>,
}

// A pull that moved the checkout: from which commit, to which, and what came
// in between (None when it was not asked for, or git could not say).
struct Moved {
    from: String,
    to: String,
    range: Option<CommitRange>,
}

#[allow(clippy::too_many_arguments)]
// What started a pull and who asked, for the project's history.
#[derive(Debug, Clone)]
pub struct RunOrigin {
    pub trigger: RunTrigger,
    // The API key's name or the signed-in user, for a manual pull.
    pub by: Option<String>,
}

impl RunOrigin {
    pub fn of(trigger: RunTrigger) -> Self {
        Self { trigger, by: None }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn sync_project(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    venv: &dyn VenvPort,
    health: &SyncHealthRegistry,
    project_id: &str,
    project: &GitProject,
    projects_dir: &Path,
    announce: Option<Announce<'_>>,
    origin: RunOrigin,
) -> Result<(), String> {
    let started_at = crate::domain::unix_now();
    let start = std::time::Instant::now();
    // The commit before and after, on every pull (`run` reports a move only
    // when there was one): what the history shows the checkout went over.
    let before = git.revision(&projects_dir.join(project_id)).await;
    // Listing commits can take a fetch: only when someone will read them.
    let list_commits = announce.is_some_and(|a| {
        a.catalog
            .webhooks
            .values()
            .any(|w| w.wants_project(Event::CommitPulled, project_id))
    });
    let result = run(
        git,
        secrets,
        venv,
        project_id,
        project,
        projects_dir,
        list_commits,
    )
    .await;

    match &result {
        Ok(_) => health.record_success(project_id),
        Err(error) => health.record_failure(project_id, error),
    }
    health.record_run(
        project_id,
        RunRecord {
            started_at,
            duration_ms: start.elapsed().as_millis() as u64,
            trigger: origin.trigger,
            by: origin.by,
            error: result.as_ref().err().cloned(),
            revision_before: before,
            revision: git.revision(&projects_dir.join(project_id)).await,
        },
    );

    if let Some(announce) = announce {
        let dir = projects_dir.join(project_id);
        let tell = |revision: Option<&str>, event: PullEvent<'_>| {
            after_pull(
                announce.dispatcher,
                announce.catalog.webhooks,
                &PullOutcome {
                    project: project_id,
                    description: project.description.as_deref(),
                    base_url: announce.catalog.url,
                    revision,
                    applications: announce.catalog.applications,
                    event,
                },
            )
        };
        match &result {
            Ok(moved) => {
                if let Some(since) = announce.dispatcher.pulls.succeeded(project_id) {
                    let revision = git.revision(&dir).await;
                    tell(
                        revision.as_deref(),
                        PullEvent::PullRecovered {
                            failing_seconds: crate::domain::run::now().saturating_sub(since),
                        },
                    );
                }
                if let Some(moved) = moved {
                    tell(
                        Some(&moved.to),
                        PullEvent::CommitPulled {
                            from: &moved.from,
                            range: moved.range.as_ref(),
                        },
                    );
                }
            }
            Err(error) => {
                if announce.dispatcher.pulls.failed(project_id) {
                    let revision = git.revision(&dir).await;
                    let consecutive_failures = health
                        .get(project_id)
                        .map(|h| h.consecutive_failures)
                        .unwrap_or(1);
                    tell(
                        revision.as_deref(),
                        PullEvent::PullFailed {
                            error,
                            consecutive_failures,
                        },
                    );
                }
            }
        }
    }

    result.map(|_| ())
}

async fn run(
    git: &dyn GitPort,
    secrets: &dyn SecretsPort,
    venv: &dyn VenvPort,
    project_id: &str,
    project: &GitProject,
    projects_dir: &Path,
    list_commits: bool,
) -> Result<Option<Moved>, String> {
    let credentials = match &project.credential {
        Some(credential_id) => secrets
            .resolve(credential_id)
            .await
            .map_err(|e| format!("credential '{}': {}", credential_id, e.message))?,
        None => Default::default(),
    };

    let dir = projects_dir.join(project_id);
    // Held through the venv step too: it reads the checkout's
    // requirements.txt.
    let lock = checkout_lock(&dir);
    let _writing = lock.write().await;
    // Bounded like every connector/enricher/output run: a git remote that
    // never answers used to hang this future — and with it whatever awaited
    // the sync. Dropping the future on timeout kills the git child
    // (kill_on_drop in the adapter), so nothing keeps running behind our back.
    let operation = if dir.join(".git").exists() {
        "pull"
    } else {
        "clone"
    };
    // A clone has nothing to compare with: only a pull can bring commits.
    let before = match operation {
        "pull" => git.revision(&dir).await,
        _ => None,
    };
    let started = std::time::Instant::now();
    let pulled = match tokio::time::timeout(
        std::time::Duration::from_secs(project.timeout_seconds),
        git.ensure(&dir, project, &credentials),
    )
    .await
    {
        Ok(result) => result.map_err(|e| e.message),
        Err(_elapsed) => Err(format!(
            "git operation timed out after {}s",
            project.timeout_seconds
        )),
    };
    count_git_request(project_id, operation, pulled.is_ok(), started);
    pulled?;

    let after = git.revision(&dir).await;
    let moved = match (before, after) {
        (Some(from), Some(to)) if from != to => {
            let range = if list_commits {
                commits(git, &credentials, project_id, project, &dir, &from, &to).await
            } else {
                None
            };
            Some(Moved { from, to, range })
        }
        _ => None,
    };

    if !project.python_venv {
        return Ok(moved);
    }

    // Its own timeout budget rather than sharing git's: each step is bounded
    // by the same number, and a slow clone must not eat the time a first
    // `pip install` legitimately needs. Dropping on timeout kills the child
    // (kill_on_drop in the adapter), same contract as git.
    match tokio::time::timeout(
        std::time::Duration::from_secs(project.timeout_seconds),
        venv.ensure(projects_dir, project_id),
    )
    .await
    {
        Ok(Ok(outcome)) => {
            if outcome == VenvOutcome::Installed {
                tracing::info!(project = %project_id, "Virtualenv updated");
            }
            Ok(moved)
        }
        Ok(Err(e)) => Err(format!("venv: {}", e.message)),
        Err(_elapsed) => Err(format!(
            "venv install timed out after {}s",
            project.timeout_seconds
        )),
    }
}

// What a pull brought in, for commit_pulled. Bounded by the project's
// timeout like the pull itself; a failure costs the message its commit list,
// never the pull.
#[allow(clippy::too_many_arguments)]
async fn commits(
    git: &dyn GitPort,
    credentials: &std::collections::HashMap<String, String>,
    project_id: &str,
    project: &GitProject,
    dir: &Path,
    from: &str,
    to: &str,
) -> Option<CommitRange> {
    let started = std::time::Instant::now();
    let listed = tokio::time::timeout(
        std::time::Duration::from_secs(project.timeout_seconds),
        git.commits(
            dir,
            project,
            credentials,
            from,
            to,
            crate::application::webhooks::MESSAGE_COMMITS,
        ),
    )
    .await;
    match listed {
        Ok(Ok(range)) => {
            if range.fetched {
                count_git_request(project_id, "fetch", true, started);
            }
            Some(range)
        }
        Ok(Err(e)) => {
            tracing::warn!(project = %project_id, error = %e.message, "Could not list the commits pulled");
            None
        }
        Err(_elapsed) => {
            tracing::warn!(project = %project_id, "Listing the commits pulled timed out");
            None
        }
    }
}

// Every request this instance makes to a git server, as count_call in
// application::reconcile counts those to the systems: a clone or a pull per
// project, and the fetch a rollback makes (application::reconcile). What the
// Overview's repositories chart reads.
pub fn count_git_request(
    project_id: &str,
    operation: &'static str,
    ok: bool,
    started: std::time::Instant,
) {
    let outcome = if ok { "success" } else { "error" };
    metrics::counter!(
        "unified_gitops_git_requests_total",
        "project" => project_id.to_string(),
        "operation" => operation,
        "outcome" => outcome
    )
    .increment(1);
    metrics::histogram!(
        "unified_gitops_git_request_duration_seconds",
        "project" => project_id.to_string(),
        "operation" => operation
    )
    .record(started.elapsed().as_secs_f64());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::out::secrets::mock::MockSecrets;
    use crate::ports::git::{GitError, GitFuture};
    use std::collections::HashMap;

    use crate::ports::venv::{VenvError, VenvFuture};

    // A VenvPort that counts its calls and answers what it is told to.
    #[derive(Default)]
    struct StubVenv {
        calls: std::sync::atomic::AtomicUsize,
        fail_with: Option<String>,
    }

    impl VenvPort for StubVenv {
        fn ensure(&self, _projects_dir: &Path, _project_id: &str) -> VenvFuture<'_> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let fail_with = self.fail_with.clone();
            Box::pin(async move {
                match fail_with {
                    Some(message) => Err(VenvError { message }),
                    None => Ok(VenvOutcome::Installed),
                }
            })
        }
    }

    // A GitPort that answers what it is told to, so the test controls the
    // outcome without a git binary or a network.
    struct StubGit {
        fail_with: Option<String>,
    }

    impl GitPort for StubGit {
        fn ensure(
            &self,
            _dir: &Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            let fail_with = self.fail_with.clone();
            Box::pin(async move {
                match fail_with {
                    Some(message) => Err(GitError { message }),
                    None => Ok(()),
                }
            })
        }
    }

    // A GitPort that never answers — the unreachable remote.
    struct HungGit;

    impl GitPort for HungGit {
        fn ensure(
            &self,
            _dir: &Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    fn project() -> GitProject {
        serde_saphyr::from_str("git_url: \"https://example.com/repo.git\"\n")
            .expect("project fixture")
    }

    #[tokio::test]
    async fn a_failed_pull_is_recorded_and_a_later_success_clears_it() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let dir = std::path::PathBuf::from("unused");

        let failing = StubGit {
            fail_with: Some("remote unreachable".to_string()),
        };
        sync_project(
            &failing,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project(),
            &dir,
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect_err("the stub fails");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert_eq!(recorded.last_error.as_deref(), Some("remote unreachable"));
        assert_eq!(recorded.consecutive_failures, 1);

        let working = StubGit { fail_with: None };
        sync_project(
            &working,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project(),
            &dir,
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect("the stub succeeds");

        let recorded = health.get("prj-a").unwrap();
        assert_eq!(recorded.consecutive_failures, 0);
        assert_eq!(recorded.last_error, None);
    }

    // The unreachable remote used to hang this future forever — and boot, the
    // scheduler task or the HTTP request along with it.
    // A checkout whose commit moves on each pull: "aaa" before, "bbb" after.
    struct MovingGit {
        at: std::sync::Mutex<&'static str>,
    }

    impl GitPort for MovingGit {
        fn ensure(
            &self,
            _dir: &Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            *self.at.lock().unwrap() = "bbb";
            Box::pin(async { Ok(()) })
        }

        fn revision(&self, _dir: &Path) -> crate::ports::git::RevisionFuture<'_> {
            let at = self.at.lock().unwrap().to_string();
            Box::pin(async move { Some(at) })
        }
    }

    // Each pull is a row in the project's history: why and by whom, and the
    // commit it moved the checkout from and to.
    #[tokio::test]
    async fn a_pull_is_kept_in_the_history_with_its_commits() {
        let health = SyncHealthRegistry::new();
        let git = MovingGit {
            at: std::sync::Mutex::new("aaa"),
        };
        sync_project(
            &git,
            &MockSecrets::new(),
            &StubVenv::default(),
            &health,
            "prj-a",
            &project(),
            &std::path::PathBuf::from("unused"),
            None,
            RunOrigin {
                trigger: RunTrigger::Manual,
                by: Some("key-ops".into()),
            },
        )
        .await
        .expect("the stub succeeds");

        let history = health.history("prj-a");
        assert_eq!(history.len(), 1);
        let run = &history[0];
        assert_eq!(run.trigger, RunTrigger::Manual);
        assert_eq!(run.by.as_deref(), Some("key-ops"));
        assert_eq!(run.revision_before.as_deref(), Some("aaa"));
        assert_eq!(run.revision.as_deref(), Some("bbb"));
        assert_eq!(run.error, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_git_remote_times_out_and_records_the_failure() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();

        let project: GitProject = serde_saphyr::from_str(
            "git_url: \"https://example.com/repo.git\"\ntimeout_seconds: 30\n",
        )
        .expect("project fixture");

        sync_project(
            &HungGit,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect_err("the hung remote must time out");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("timed out after 30s")),
            "error was: {:?}",
            recorded.last_error
        );
    }

    #[tokio::test]
    async fn an_unresolvable_credential_is_a_recorded_failure_too() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let git = StubGit { fail_with: None };

        let project: GitProject = serde_saphyr::from_str(
            "git_url: \"https://example.com/repo.git\"\ncredential: \"cred-ghost\"\n",
        )
        .expect("project fixture");

        sync_project(
            &git,
            &secrets,
            &StubVenv::default(),
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect_err("the credential does not resolve");

        let recorded = health.get("prj-a").expect("failure must be recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("cred-ghost"))
        );
    }

    // A GitPort that takes a while and counts how many of its calls run at
    // once, to show that pulls of one checkout never overlap.
    #[derive(Default)]
    struct SlowGit {
        running: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
    }

    impl GitPort for SlowGit {
        fn ensure(
            &self,
            _dir: &Path,
            _project: &GitProject,
            _credentials: &HashMap<String, String>,
        ) -> GitFuture<'_> {
            use std::sync::atomic::Ordering::SeqCst;
            Box::pin(async move {
                let now = self.running.fetch_add(1, SeqCst) + 1;
                self.most.fetch_max(now, SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                self.running.fetch_sub(1, SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn pulls_of_one_checkout_never_overlap() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let git = SlowGit::default();
        let venv = StubVenv::default();
        let dir = tempfile::tempdir().unwrap();
        let project = project();

        // Five applications on one project, pulling at once.
        let pulls = (0..5).map(|_| {
            sync_project(
                &git,
                &secrets,
                &venv,
                &health,
                "prj-shared",
                &project,
                dir.path(),
                None,
                RunOrigin::of(RunTrigger::Scheduled),
            )
        });
        for result in futures::future::join_all(pulls).await {
            result.expect("the stub succeeds");
        }
        assert_eq!(git.most.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A reader holding the lock keeps a pull out until it is done.
        let lock = checkout_lock(&dir.path().join("prj-shared"));
        let reading = lock.read().await;
        let pull = sync_project(
            &git,
            &secrets,
            &venv,
            &health,
            "prj-shared",
            &project,
            dir.path(),
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        );
        tokio::pin!(pull);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut pull)
                .await
                .is_err(),
            "a pull must wait for a run reading the checkout"
        );
        drop(reading);
        pull.await.expect("the pull goes on once the read is over");
    }

    #[tokio::test]
    async fn a_project_without_python_venv_never_builds_one() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let venv = StubVenv::default();

        sync_project(
            &StubGit { fail_with: None },
            &secrets,
            &venv,
            &health,
            "prj-a",
            &project(),
            &std::path::PathBuf::from("unused"),
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect("git succeeds");

        assert_eq!(
            venv.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "python_venv defaults to off: no venv work without the opt-in"
        );
    }

    // A broken requirements.txt fails the PROJECT sync, visibly, instead of
    // every script failing at import time with the cause nowhere.
    #[tokio::test]
    async fn a_failing_venv_install_fails_the_sync_and_records_it() {
        let secrets = MockSecrets::new();
        let health = SyncHealthRegistry::new();
        let venv = StubVenv {
            fail_with: Some("pip could not find torch==999".to_string()),
            ..Default::default()
        };
        let project: GitProject = serde_saphyr::from_str(
            "git_url: \"https://example.com/repo.git\"\npython_venv: true\n",
        )
        .expect("project fixture");

        sync_project(
            &StubGit { fail_with: None },
            &secrets,
            &venv,
            &health,
            "prj-a",
            &project,
            &std::path::PathBuf::from("unused"),
            None,
            RunOrigin::of(RunTrigger::Scheduled),
        )
        .await
        .expect_err("the venv step fails");

        assert_eq!(venv.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let recorded = health.get("prj-a").expect("failure recorded");
        assert!(
            recorded
                .last_error
                .as_deref()
                .is_some_and(|e| e.contains("venv") && e.contains("torch==999")),
            "error was: {:?}",
            recorded.last_error
        );
    }
}
