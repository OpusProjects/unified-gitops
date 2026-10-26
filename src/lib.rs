pub mod adapters;
pub mod application;
pub mod config;
pub mod domain;
pub mod ports;
mod state;

// Re-export: the rest of the code (and tests) continue using crate::AppState
pub use state::{AppState, ReloadNotifier, RuntimeConfig};

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;

use adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use adapters::out::connectors::process::ProcessConnector;
use adapters::out::git::cli::CliGit;
use adapters::out::secrets::mock::MockSecrets;
use adapters::out::secrets::reloadable::ReloadableSecrets;
use adapters::out::state::fs::FsStateStore;
use domain::app::Application;
use domain::connector::Connector;
use domain::credential::Credential;
use domain::project::GitProject;
use ports::secrets::SecretsPort;

// AppBuilder is the composition root: the ONLY place where concrete adapters
// are chosen to fill AppState ports. Production swaps secrets (EnvSecrets
// instead of MockSecrets) and adds the API key; tests use defaults. Builder
// pattern: each method consumes self and returns it, so calls can be chained
// and build() finalizes the construction.
pub struct AppBuilder {
    applications: HashMap<String, Application>,
    connectors: HashMap<String, Connector>,
    projects: HashMap<String, GitProject>,
    credentials: HashMap<String, Credential>,
    secrets_settings: config::SecretsConfig,
    projects_dir: std::path::PathBuf,
    ui: crate::config::UiConfig,
    secrets: Arc<dyn SecretsPort>,
    // Some = the configuration API is enabled: the directory can be read and
    // written over HTTP, and a reload can swap what this builder is about to
    // freeze into AppState.
    config_store: Option<Arc<dyn ports::config_store::ConfigStorePort>>,
    live_settings: Option<config::RestartOnlySettings>,
    api_keys: Vec<ResolvedApiKey>,
    directory: Arc<dyn ports::directory::DirectoryPort>,
    connector: Arc<dyn ports::connector::ConnectorPort>,
    git: Arc<dyn ports::git::GitPort>,
    state_dir: std::path::PathBuf,
    project_health: Arc<domain::sync_health::SyncHealthRegistry>,
    cors_allowed_origins: Vec<String>,
    metrics_require_auth: bool,
    shutdown_grace_seconds: u64,
    max_body_bytes: usize,
    url: Option<String>,
}

impl AppBuilder {
    pub fn new() -> Self {
        Self {
            applications: HashMap::new(),
            connectors: HashMap::new(),
            projects: HashMap::new(),
            credentials: HashMap::new(),
            secrets_settings: config::SecretsConfig::default(),
            projects_dir: std::path::PathBuf::from("projects"),
            ui: crate::config::UiConfig::default(),
            // MockSecrets by default: in tests there is no secrets store
            secrets: Arc::new(MockSecrets::new()),
            config_store: None,
            live_settings: None,
            api_keys: Vec::new(),
            // The real adapter even in tests: it is only called when a test
            // configures LDAP, and those tests swap in a double.
            directory: Arc::new(adapters::out::directory::ldap::LdapDirectory),
            connector: Arc::new(ProcessConnector::new()),
            git: Arc::new(CliGit::new()),
            state_dir: std::path::PathBuf::from("state"),
            project_health: Arc::new(domain::sync_health::SyncHealthRegistry::new()),
            cors_allowed_origins: Vec::new(),
            metrics_require_auth: false,
            shutdown_grace_seconds: 20,
            // axum's own default, mirrored like the three above
            max_body_bytes: 2 * 1024 * 1024,
            url: None,
        }
    }

    // The browsable HTML API's settings. Off by default, like the config it
    // mirrors — a test that wants the browser surface asks for it.
    pub fn ui(mut self, ui: crate::config::UiConfig) -> Self {
        self.ui = ui;
        self
    }

    pub fn applications(mut self, applications: HashMap<String, Application>) -> Self {
        self.applications = applications;
        self
    }

    pub fn connectors(mut self, connectors: HashMap<String, Connector>) -> Self {
        self.connectors = connectors;
        self
    }

    // Where the local copy of each system's settings is written.
    pub fn state_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.state_dir = dir;
        self
    }

    // Test seams: a connector port that keeps objects in memory, a git port
    // that needs no remote.
    pub fn connector(mut self, connector: Arc<dyn ports::connector::ConnectorPort>) -> Self {
        self.connector = connector;
        self
    }

    pub fn git(mut self, git: Arc<dyn ports::git::GitPort>) -> Self {
        self.git = git;
        self
    }

    pub fn projects(
        mut self,
        projects: HashMap<String, GitProject>,
        projects_dir: std::path::PathBuf,
    ) -> Self {
        self.projects = projects;
        self.projects_dir = projects_dir;
        self
    }

    pub fn directory(mut self, directory: Arc<dyn ports::directory::DirectoryPort>) -> Self {
        self.directory = directory;
        self
    }

    pub fn secrets(mut self, secrets: Arc<dyn SecretsPort>) -> Self {
        self.secrets = secrets;
        self
    }

    // The credential DEFINITIONS (not their values), and the settings the
    // resolver chain is built from. Both only matter to a reload: it rebuilds
    // the chain from the new ones and reports which credential changed.
    pub fn credentials(
        mut self,
        credentials: HashMap<String, Credential>,
        settings: config::SecretsConfig,
    ) -> Self {
        self.credentials = credentials;
        self.secrets_settings = settings;
        self
    }

    // Everything a loaded AppConfig contributes, in one call.
    //
    // Borrows rather than consumes: main still needs the same config for the
    // things that are NOT part of the app (the listener address, the snapshot
    // path, the boot clones), and a reload needs to build the identical set
    // from a config it did not consume either. Cloning a few maps once at
    // boot is not a cost worth a partially-moved struct.
    pub fn from_config(mut self, cfg: &config::AppConfig) -> Self {
        self.applications = cfg.applications.clone();
        self.connectors = cfg.connectors.clone();
        self.projects = cfg.projects.clone();
        self.credentials = cfg.credentials.clone();
        self.secrets_settings = cfg.secrets_config.clone();
        self.projects_dir = std::path::PathBuf::from(&cfg.projects_config.dir);
        self.state_dir = std::path::PathBuf::from(&cfg.state.dir);
        self.ui = cfg.server.ui.clone();
        self.cors_allowed_origins = cfg.server.cors_allowed_origins.clone();
        self.metrics_require_auth = cfg.server.metrics_require_auth;
        self.shutdown_grace_seconds = cfg.server.shutdown_grace_seconds;
        self.max_body_bytes = cfg.server.max_body_bytes;
        // As a reload reads it (application::config): one spelling, no
        // credentials, so boot and reload agree.
        self.url = cfg
            .server
            .url
            .as_deref()
            .map(|url| crate::domain::project::without_userinfo(url.trim_end_matches('/')));
        // What this process is built from, kept whether or not the config
        // API is on: GET /settings shows the restart-only values in force.
        self.live_settings = Some(config::RestartOnlySettings::from_config(cfg));
        self
    }

    // Turn on the configuration API. `live_settings` is what THIS process was
    // built from, kept so a later reload can name the keys it cannot adopt.
    pub fn config_api(
        mut self,
        store: Arc<dyn ports::config_store::ConfigStorePort>,
        live_settings: config::RestartOnlySettings,
    ) -> Self {
        self.config_store = Some(store);
        self.live_settings = Some(live_settings);
        self
    }

    // Shorthand: one secret = one admin key. Tests and the
    // UNIFIED_GITOPS_KEY path use it.
    pub fn api_key(mut self, api_key: Option<String>) -> Self {
        if let Some(secret) = api_key {
            self.api_keys.push(ResolvedApiKey {
                name: "default".to_string(),
                secret,
                permissions: Permissions::Admin,
            });
        }
        self
    }

    pub fn api_keys(mut self, api_keys: Vec<ResolvedApiKey>) -> Self {
        self.api_keys = api_keys;
        self
    }

    // Project pulls start at boot, before the AppState exists (main clones the
    // checkouts so script paths can be resolved into them). Handing the
    // registry in lets those boot syncs and the periodic task write into the
    // same instance the HTTP layer reads. Defaults to a fresh one for tests.
    pub fn project_health(
        mut self,
        registry: Arc<domain::sync_health::SyncHealthRegistry>,
    ) -> Self {
        self.project_health = registry;
        self
    }

    pub fn cors_allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.cors_allowed_origins = origins;
        self
    }

    pub fn metrics_require_auth(mut self, require_auth: bool) -> Self {
        self.metrics_require_auth = require_auth;
        self
    }

    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.max_body_bytes = bytes;
        self
    }

    pub fn build(self) -> Router<()> {
        let (router, _state) = self.build_with_state();
        router
    }

    // Also returns the AppState: needed by main (to start the scheduler on the
    // same state) and by tests that drive a reconcile directly
    pub fn build_with_state(self) -> (Router<()>, Arc<AppState>) {
        // Install the metrics recorder before anything can record
        adapters::r#in::http::metrics::init();

        // The resolver chain always sits behind the reloadable indirection,
        // in tests as in production: one shape to reason about, and a reload
        // never has to ask whether it is allowed to swap.
        let reloadable = Arc::new(ReloadableSecrets::new(self.secrets));

        let state = Arc::new(AppState {
            connector: self.connector,
            store: Arc::new(FsStateStore::new(self.state_dir)),
            secrets: Arc::clone(&reloadable) as Arc<dyn SecretsPort>,
            git: self.git,
            venv: Arc::new(adapters::out::python::PyVenv::new()),
            secrets_swap: reloadable,
            config: std::sync::RwLock::new(Arc::new(RuntimeConfig {
                applications: self.applications,
                connectors: self.connectors,
                projects: self.projects,
                credentials: self.credentials,
                secrets: self.secrets_settings,
                metrics_require_auth: self.metrics_require_auth,
                cors_allowed_origins: self.cors_allowed_origins,
                max_body_bytes: self.max_body_bytes,
                shutdown_grace_seconds: self.shutdown_grace_seconds,
                url: self.url,
                ldap: self.ui.ldap.clone(),
                ui_home: self.ui.home().to_string(),
            })),
            config_store: self.config_store,
            live_settings: std::sync::RwLock::new(self.live_settings),
            restart_pending: std::sync::RwLock::new(Vec::new()),
            reload: ReloadNotifier::new(),
            projects_dir: self.projects_dir,
            ui: self.ui,
            sessions: std::sync::Arc::new(crate::adapters::r#in::http::session::SessionStore::new()),
            directory: self.directory,
            runs: Arc::new(domain::run::AppStateRegistry::new()),
            app_health: Arc::new(domain::sync_health::SyncHealthRegistry::new()),
            project_health: self.project_health,
            locks: Arc::new(application::reconcile::Locks::new()),
            described: Arc::new(domain::connector::DescribedKinds::new()),
            started: std::time::Instant::now(),
            login_throttle: Arc::new(crate::application::login::LoginThrottle::default()),
        });
        let router = adapters::r#in::http::routes::create_router(
            Arc::clone(&state),
            Arc::new(adapters::r#in::http::auth::ApiKeyRegistry::new(
                self.api_keys,
            )),
        );
        (router, state)
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}
