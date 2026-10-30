use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::domain::credential::Credential;
use crate::domain::project::GitProject;
use crate::domain::sync_health::SyncHealthRegistry;
use crate::domain::webhook::Webhook;
use crate::ports;

// The part of the configuration a running process can adopt without being
// restarted — everything that is only ever READ, per request or per tick.
//
// It used to be five plain fields on AppState, which made it structurally
// impossible to change any of it without a new process. Grouping it behind
// one Arc is what makes a reload a single pointer swap: a request that is
// already running keeps the snapshot it started with (so a pull cannot see
// half of an old configuration and half of a new one), and the next request
// picks up the new one.
pub struct RuntimeConfig {
    pub projects: HashMap<String, GitProject>,
    // Carried but never resolved from here — the secrets chain holds its own
    // copy. It lives in the snapshot so a reload can say WHICH credential
    // changed.
    pub credentials: HashMap<String, Credential>,
    // Read after every pull, so they reload like projects.
    pub webhooks: HashMap<String, Webhook>,
    // Read per run, so they reload like webhooks.
    pub apis: HashMap<String, crate::domain::api::Api>,
    // Carried for the same reason as `credentials`: the chain is rebuilt from
    // it on every reload.
    pub secrets: crate::config::SecretsConfig,
    // Whether /metrics needs an API key, read per scrape so it reloads.
    pub metrics_require_auth: bool,
    // Largest request body accepted, read per request.
    pub max_body_bytes: usize,
    // Browser origins allowed by CORS, read per request.
    pub cors_allowed_origins: Vec<String>,
    // Read at shutdown, so it reloads trivially.
    pub shutdown_grace_seconds: u64,
    // `server.url`: this instance's address, read per request, so it reloads.
    pub url: Option<String>,
    // LDAP login. Read per login, so the directory and the rules reload; a
    // change also ends the sessions opened under the old rules.
    pub ldap: Option<crate::domain::ldap::LdapConfig>,
    // `server.ui.home`: where a sign-in with no page to return to lands.
    pub ui_home: String,
}

// Bumped on every reload, and watched by the scheduler: the tasks of the
// previous generation are told to finish and a fresh set is spawned from the
// new configuration. A watch channel rather than a callback because that is
// already how shutdown reaches those tasks — one wait point, two reasons to
// wake up.
pub struct ReloadNotifier {
    tx: tokio::sync::watch::Sender<u64>,
}

impl ReloadNotifier {
    pub fn new() -> Self {
        Self {
            tx: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.tx.subscribe()
    }

    pub fn generation(&self) -> u64 {
        *self.tx.borrow()
    }

    // send_replace rather than send: a process with no scheduler listening
    // (every test that builds a router) must still be able to reload, and
    // send() reports "no receivers" as an error.
    pub fn bump(&self) -> u64 {
        let next = self.generation() + 1;
        self.tx.send_replace(next);
        next
    }
}

impl Default for ReloadNotifier {
    fn default() -> Self {
        Self::new()
    }
}

// The shared application state: the ports (as Arc<dyn Trait>, so handlers
// depend on the interface, not the implementation) plus the configuration
// loaded at startup and, where the configuration API is enabled, replaced
// under the running process by a reload.
//
// Arc = Atomic Reference Counted — a reference-counted pointer shared across
// threads; each axum handler receives a cheap clone of the same AppState.
pub struct AppState {
    pub secrets: Arc<dyn ports::secrets::SecretsPort>,
    pub git: Arc<dyn ports::git::GitPort>,
    pub venv: Arc<dyn ports::venv::VenvPort>,
    // The same object as `secrets` above, held once more as the narrow trait
    // that can REPLACE the chain: a reload rebuilds it from the new
    // credentials, and nothing else in the process has to know it happened.
    pub secrets_swap: Arc<dyn ports::secrets::SecretsSwapPort>,
    // The reloadable configuration. Private: reads go through config(), which
    // hands out an Arc snapshot rather than a lock guard — a handler holding a
    // guard across an await would block every reload behind its slowest
    // pull.
    // pub(crate) only so AppBuilder can fill it — everything else reads it
    // through config() and writes it through swap_config().
    pub(crate) config: RwLock<Arc<RuntimeConfig>>,
    // Where the configuration directory is, and the settings the live process
    // was built from. Both only exist to answer a reload honestly: what is on
    // disk, and which of its keys this process cannot adopt without a restart.
    pub config_store: Option<Arc<ports::config_store::ConfigStorePort>>,
    pub(crate) live_settings: RwLock<Option<crate::config::RestartOnlySettings>>,
    // The restart-only keys the last APPLIED reload changed, kept so /metrics
    // can export the count at scrape time: a pod running on a configuration
    // it could only partially adopt is otherwise visible only to whoever
    // queries GET /api/v1/config on that one pod. Empty at boot (the process
    // was just built from the directory) and replaced whole on every reload,
    // so a follow-up reload that reverts the change clears it.
    pub(crate) restart_pending: RwLock<Vec<String>>,
    pub reload: ReloadNotifier,
    // The checkout root. Restart-only: the boot clones, the snapshot of script
    // paths every execution resolves, and the project tasks all take it from
    // here, and moving it under a running process would strand every checkout
    // that is already on disk.
    pub projects_dir: PathBuf,

    // Browser sessions for the browsable API. Empty and unused when
    // `ui.enabled` is false — the routes that would create one are not in the
    // router at all.
    pub sessions: Arc<crate::adapters::r#in::http::session::SessionStore>,
    // Checks LDAP logins. Unused unless `server.ui.ldap` is configured.
    pub directory: Arc<dyn ports::directory::DirectoryPort>,

    // The browsable HTML API's settings. Static like `projects_dir`, and for
    // the same reason: it decides which middlewares and routes are in the
    // router, and the router is built once (config::RestartOnlySettings).
    pub ui: crate::config::UiConfig,
    // Last attempt / success / error / streak per project pull (keyed by
    // project id).
    pub project_health: Arc<SyncHealthRegistry>,
    // When this process's app was built: how long it has been running, for
    // the Configuration page ("did it restart?").
    pub started: std::time::Instant,
    // Recent failed LDAP logins and the cap on concurrent ones (see
    // application::login::LoginThrottle).
    pub login_throttle: Arc<crate::application::login::LoginThrottle>,
    // Queues what runs have to tell, and sends it (application::webhooks).
    pub dispatcher: Arc<crate::application::webhooks::Dispatcher>,
    // What sends it: an HTTP client in production, a recorder in tests.
    pub http: Arc<dyn ports::http_sender::HttpSenderPort>,
    // Runs the items that act on a change (application::actions).
    pub runner: Arc<crate::application::actions::Runner>,
}

impl AppState {
    // The configuration as of right now. An Arc clone, not a borrow: the
    // caller may hold it across await points, and while it does, a concurrent
    // reload swaps the pointer without disturbing it.
    pub fn config(&self) -> Arc<RuntimeConfig> {
        Arc::clone(&self.config.read().expect("runtime config lock"))
    }

    // Install a new configuration, returning the one it replaced so the caller
    // can report what actually changed. Does NOT notify the scheduler — that
    // is a separate step (see ReloadNotifier), because a reload swaps several
    // things and the tasks should restart once, at the end, against all of it.
    pub fn swap_config(&self, next: Arc<RuntimeConfig>) -> Arc<RuntimeConfig> {
        let mut guard = self.config.write().expect("runtime config lock");
        std::mem::replace(&mut guard, next)
    }

    // What the LIVE process was built from, for the keys a reload cannot
    // apply. None in tests and wherever main did not record it.
    pub fn live_settings(&self) -> Option<crate::config::RestartOnlySettings> {
        self.live_settings
            .read()
            .expect("live settings lock")
            .clone()
    }

    pub fn set_live_settings(&self, settings: crate::config::RestartOnlySettings) {
        *self.live_settings.write().expect("live settings lock") = Some(settings);
    }

    // The restart-only keys still awaiting a restart, as of the last applied
    // reload. GET /api/v1/config reports them by name; /metrics as a count.
    pub fn restart_pending(&self) -> Vec<String> {
        self.restart_pending
            .read()
            .expect("restart pending lock")
            .clone()
    }

    pub fn set_restart_pending(&self, keys: Vec<String>) {
        *self.restart_pending.write().expect("restart pending lock") = keys;
    }

    // Starts sending queued webhooks. Once; main calls it beside the
    // scheduler, and so does a test that wants to see what was sent.
    pub fn start_webhooks(&self) {
        self.dispatcher
            .start(Arc::clone(&self.secrets), Arc::clone(&self.http));
        self.runner.start(crate::application::actions::Ports {
            secrets: Arc::clone(&self.secrets),
            http: Arc::clone(&self.http),
            git: Arc::clone(&self.git),
            dispatcher: Arc::clone(&self.dispatcher),
            projects_dir: self.projects_dir.clone(),
        });
    }

    // What a pull tells, and to whom: the configuration of this moment.
    pub fn announce<'a>(
        &'a self,
        config: &'a Arc<RuntimeConfig>,
    ) -> crate::application::projects::Announce<'a> {
        crate::application::projects::Announce {
            dispatcher: &self.dispatcher,
            runner: &self.runner,
            config,
        }
    }
}
