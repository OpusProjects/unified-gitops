use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::domain::api::Api;
use crate::domain::api_key::{ApiKeyDef, ApiKeyRole};
use crate::domain::credential::{Credential, CredentialRules};
use crate::domain::project::GitProject;
use crate::domain::webhook::Webhook;

// Which files make up a configuration directory, and the rule an item name or
// a path segment must meet: web-rs's, the same rule the config API applies.
pub use web_rs::config::{REQUIRED_CONFIG_FILE, config_files, is_config_path, is_safe_id};

pub struct AppConfig {
    pub server: ServerConfig,
    pub projects_config: ProjectsConfig,
    pub secrets_config: SecretsConfig,
    pub config_api: ConfigApiConfig,
    pub credentials: HashMap<String, Credential>,
    pub projects: HashMap<String, GitProject>,
    pub api_keys: HashMap<String, ApiKeyDef>,
    pub webhooks: HashMap<String, Webhook>,
    pub apis: HashMap<String, Api>,
}

fn vault_as_login(who: &str, credential_id: &str) -> String {
    format!(
        "{who} uses credential '{credential_id}', which is a Vault connection: name the \
         credential that reads from it (its vault:) instead"
    )
}

// HTTP server configuration — config.yaml
//
// `deny_unknown_fields`, here and on every other struct that parses a config
// file: a key the schema does not know is a typo, and serde's default is to
// silently drop it and silently apply the field's default. For most settings
// that is an application checked on an interval nobody chose; for a security
// setting (`metrics_require_auth`) it is failing open. Refusing to start,
// naming the key, is strictly better in every case.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,

    // The browser side: the login form, browser sessions, and the web UI
    // (assets_dir). OFF by default and restart-only: it decides which
    // middlewares and routes are in the router, and the router is built once.
    #[serde(default)]
    pub ui: UiConfig,

    // Origins allowed for CORS. Empty (the default) = no CORS headers at all,
    // which is right for server-to-server consumers. ["*"] = any origin.
    #[serde(default)]
    pub cors_allowed_origins: Vec<String>,

    // Require an API key on /metrics. false (the default) keeps it public
    // alongside the health probes, which is what Prometheus scrape configs
    // expect. true is for a shared network: the exposition labels every
    // application id.
    #[serde(default)]
    pub metrics_require_auth: bool,

    // How long shutdown waits for in-flight runs (an apply is never cut in the
    // middle of an operation) before exiting anyway.
    #[serde(default = "default_shutdown_grace_seconds")]
    pub shutdown_grace_seconds: u64,

    // Largest request body accepted, in bytes, on every route. Size it for the
    // biggest config directory a pipeline pushes in one PUT.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,

    // The address people reach this instance at
    // (https://unified-gitops.example.com). The process only knows the port
    // it listens on; the real address is the ingress's. Optional; shown on
    // the Configuration page.
    #[serde(default)]
    pub url: Option<String>,
}

fn default_shutdown_grace_seconds() -> u64 {
    20
}

fn default_max_body_bytes() -> usize {
    2 * 1024 * 1024
}

// Secrets behavior — config.yaml, `secrets:` section (optional, deprecated).
// The struct lives with the chain it configures, in secrets-rs.
pub use secrets_rs::SecretsConfig;

// Where git projects are cloned — config.yaml, `projects:` section (optional)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectsConfig {
    // Working directory for checkouts: one subdirectory per project id
    #[serde(default = "default_projects_dir")]
    pub dir: String,
}

impl Default for ProjectsConfig {
    fn default() -> Self {
        Self {
            dir: default_projects_dir(),
        }
    }
}

fn default_projects_dir() -> String {
    "projects".to_string()
}

// Writing this directory over the API — config.yaml, `config_api:` section
// (optional). OFF by default: turning it on means an admin key can rewrite
// every file the loader reads, api_keys.yaml included.
#[derive(Deserialize, Default, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ConfigApiConfig {
    #[serde(default)]
    pub enabled: bool,
}

// The configuration directory is laid out the way AWX config as code is:
// config.yaml at the root holds the server settings, and every other *.yaml /
// *.yml file, at any depth, holds ITEMS under a typed top-level key:
//
//   # projects/infra.yaml
//   projects:
//     - name: infra
//       git_url: https://git.example.com/infra.git
//       ...
//
// The top-level key says what an item is, never the folder: folders are for
// people (by team, by system) and may nest as deep as they like. One item per
// file is the convention; a file may hold several, of several types. `name`
// is the item's identity, as in AWX: what other items reference, what the
// API and the state directory use.

// The top-level keys an item file may use.
pub const SECTIONS: [&str; 5] = ["projects", "credentials", "api_keys", "webhooks", "apis"];

// The settings a running process cannot adopt without being restarted, pulled
// out of AppConfig as plain comparable values.
//
// A reload swaps what it can (projects, credentials, api keys, webhooks, the
// shutdown grace, the metrics auth flag, CORS, the body limit) and has to say
// something honest about the rest: the listener is already bound and the
// checkouts are already on disk. Silently
// ignoring those
// keys is how a pipeline comes to believe it changed a port it did not
// change — so they are diffed and NAMED in the reload report instead.
// The browsable API's settings.
#[derive(Deserialize, Clone, PartialEq, Debug)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    #[serde(default)]
    pub enabled: bool,

    // How long a browser session lives. Fixed when the routes are built (the
    // login handler receives it as an extension), so it is restart-only and
    // listed in RestartOnlySettings: a reload that changes it says so.
    #[serde(default = "default_session_ttl_seconds")]
    pub session_ttl_seconds: u64,

    // Log in with a directory account instead of pasting an API key. Read per
    // login from the configuration snapshot, so changing the server or the
    // access rules is a reload — which also ends the LDAP sessions opened
    // under the old rules (application::config::apply).
    #[serde(default)]
    pub ldap: Option<crate::domain::ldap::LdapConfig>,

    // Where a sign-in lands when nothing asked for a page: someone opening
    // /api/v1/login directly, or signing in again after signing out. Unset,
    // it is "/" when this instance serves the UI (`assets_dir`) and the API's
    // own browsable pages otherwise. Read per login, so it reloads.
    #[serde(default)]
    pub home: Option<String>,

    // A built unified-ui (the directory holding its index.html), served at
    // "/": the UI and the API on one address, no proxy in front. The image
    // ships one and sets this. Restart-only: the router is built once.
    #[serde(default)]
    pub assets_dir: Option<String>,
}

impl UiConfig {
    pub fn home(&self) -> &str {
        match (&self.home, &self.assets_dir) {
            (Some(home), _) => home,
            (None, Some(_)) => "/",
            (None, None) => "/api/v1/",
        }
    }
}

fn default_session_ttl_seconds() -> u64 {
    3600
}

// A path on this origin, safe to redirect to: web-rs's rule, the same the
// sign-in form applies to `next`.
pub use web_rs::url::is_local_path;

// By hand rather than derived: a derived Default gives a session TTL of zero
// (serde's default only applies when parsing), so a UiConfig built in code
// issued sessions that were expired the moment they were created.
impl Default for UiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_ttl_seconds: default_session_ttl_seconds(),
            ldap: None,
            home: None,
            assets_dir: None,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct RestartOnlySettings {
    pub host: String,
    pub port: u16,
    pub projects_dir: String,
    pub config_api_enabled: bool,
    pub ui_enabled: bool,
    pub session_ttl_seconds: u64,
    pub ui_assets_dir: Option<String>,
}

impl RestartOnlySettings {
    pub fn from_config(cfg: &AppConfig) -> Self {
        Self {
            host: cfg.server.host.clone(),
            port: cfg.server.port,
            projects_dir: cfg.projects_config.dir.clone(),
            config_api_enabled: cfg.config_api.enabled,
            ui_enabled: cfg.server.ui.enabled,
            session_ttl_seconds: cfg.server.ui.session_ttl_seconds,
            ui_assets_dir: cfg.server.ui.assets_dir.clone(),
        }
    }

    // The config.yaml keys that differ, named as they are written in the file
    // so the answer can be pasted into a search.
    pub fn changed_keys(&self, other: &Self) -> Vec<String> {
        let mut keys = Vec::new();
        let mut check = |differs: bool, key: &str| {
            if differs {
                keys.push(key.to_string());
            }
        };
        check(self.host != other.host, "server.host");
        check(self.port != other.port, "server.port");
        check(self.projects_dir != other.projects_dir, "projects.dir");
        check(
            self.config_api_enabled != other.config_api_enabled,
            "config_api.enabled",
        );
        check(self.ui_enabled != other.ui_enabled, "server.ui.enabled");
        check(
            self.session_ttl_seconds != other.session_ttl_seconds,
            "server.ui.session_ttl_seconds",
        );
        check(
            self.ui_assets_dir != other.ui_assets_dir,
            "server.ui.assets_dir",
        );
        keys
    }
}

// Intermediate struct to parse config.yaml (server + optional sections)
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerFile {
    server: ServerConfig,
    #[serde(default)]
    projects: ProjectsConfig,
    #[serde(default)]
    secrets: SecretsConfig,
    #[serde(default)]
    config_api: ConfigApiConfig,
}

impl AppConfig {
    // Fails with every problem at once (see validate_errors), so one
    // --check-config run — or one rejected API write — reports the whole list.
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        let errors = self.validate_errors();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ConfigErrors::new(errors).into())
        }
    }

    pub fn validate_errors(&self) -> Vec<String> {
        let mut errors: Vec<String> = Vec::new();

        if let Some(url) = &self.server.url
            && !(url.starts_with("https://") || url.starts_with("http://"))
        {
            errors.push(format!(
                "server.url '{url}' must be an http:// or https:// address"
            ));
        }

        // A cron schedule must parse, and a project cannot have two cadences.
        for (id, project) in &self.projects {
            // The id names the checkout's folder (and its venv's): `../x` or
            // an absolute name would clone and `reset --hard` outside
            // projects.dir.
            if !is_safe_id(id) {
                errors.push(format!(
                    "Project '{id}' must use only letters, digits, '-', '_' and '.' \
                     (it names a folder in projects.dir)"
                ));
            }
            if let Some(expression) = &project.schedule {
                if let Err(e) = crate::adapters::r#in::scheduler::parse_cron(expression) {
                    errors.push(format!(
                        "Project '{id}' has an invalid cron schedule '{expression}': {e}"
                    ));
                }
                if project.sync_interval_seconds.is_some_and(|secs| secs > 0) {
                    errors.push(format!(
                        "Project '{id}' sets both schedule and sync_interval_seconds — pick one"
                    ));
                }
            }
            errors.extend(project.errors(id));
            if let Some(cred_id) = project.credential_id() {
                if !self.credentials.contains_key(cred_id) {
                    errors.push(format!(
                        "Project '{id}' references unknown credential '{cred_id}'"
                    ));
                } else if self.is_vault(cred_id) {
                    errors.push(vault_as_login(&format!("Project '{id}'"), cred_id));
                }
            }
        }

        // Restricted API keys must reference existing projects — a typo'd
        // id would otherwise just deny access with no explanation.
        for (id, key) in &self.api_keys {
            errors.extend(key.errors(id));
            if key.role == ApiKeyRole::Restricted {
                for project_id in &key.projects {
                    if !self.projects.contains_key(project_id) {
                        errors.push(format!(
                            "API key '{id}' references unknown project '{project_id}'"
                        ));
                    }
                }
            }
        }

        // Webhooks: their own settings, then the project they watch and the
        // credentials they send with.
        for (id, webhook) in &self.webhooks {
            errors.extend(webhook.errors(id));
            if !self.projects.contains_key(&webhook.project) {
                errors.push(format!(
                    "Webhook '{id}' references unknown project '{}'",
                    webhook.project
                ));
            }
            for cred in webhook.credentials() {
                if !self.credentials.contains_key(cred) {
                    errors.push(format!(
                        "Webhook '{id}' references unknown credential '{cred}'"
                    ));
                } else if self.is_vault(cred) {
                    errors.push(vault_as_login(&format!("Webhook '{id}'"), cred));
                }
            }
        }

        // APIs: their own settings, then the project they watch and the
        // credentials they call with.
        for (id, api) in &self.apis {
            errors.extend(api.errors(id, |expression| {
                crate::adapters::r#in::scheduler::parse_cron(expression).map(|_| ())
            }));
            if !self.projects.contains_key(&api.project) {
                errors.push(format!(
                    "API '{id}' references unknown project '{}'",
                    api.project
                ));
            }
            for cred in api.credentials() {
                if !self.credentials.contains_key(cred) {
                    errors.push(format!("API '{id}' references unknown credential '{cred}'"));
                } else if self.is_vault(cred) {
                    errors.push(vault_as_login(&format!("API '{id}'"), cred));
                }
            }
        }

        // The deprecated config.yaml Vault: a 0 bound fails every request.
        if self
            .secrets_config
            .vault
            .as_ref()
            .is_some_and(|v| v.timeout_seconds == 0)
        {
            errors.push(
                "secrets.vault.timeout_seconds 0 would fail every request to that Vault — \
                 give it a bound in seconds (default 10)"
                    .to_string(),
            );
        }

        for (id, credential) in &self.credentials {
            errors.extend(credential.errors(id));
        }

        // A vault_path needs a Vault to read it from: the `type: vault`
        // credential it names, or (deprecated) config.yaml's secrets.vault
        // when it names none.
        for (id, credential) in &self.credentials {
            if credential.vault_path.is_none() {
                continue;
            }
            match &credential.vault {
                Some(vault) if !self.credentials.contains_key(vault) => errors.push(format!(
                    "Credential '{id}' references unknown vault '{vault}'"
                )),
                Some(vault) if !self.is_vault(vault) => errors.push(format!(
                    "Credential '{id}' names '{vault}' as its vault, which is not a \
                     `type: vault` credential"
                )),
                Some(_) => {}
                None if self.secrets_config.vault.is_none() => errors.push(format!(
                    "Credential '{id}' sets vault_path but names no vault to read it from — \
                     write vault: <a type: vault credential>"
                )),
                None => {}
            }
        }

        if let Some(ldap) = &self.server.ui.ldap {
            errors.extend(self.ldap_errors(ldap));
        }

        if let Some(home) = &self.server.ui.home
            && !is_local_path(home)
        {
            errors.push(format!(
                "server.ui.home '{home}' must be a path on this server, such as \"/\" — \
                 a sign-in redirects there, and another host would make it an open redirect"
            ));
        }

        if self.server.ui.assets_dir.is_some() && !self.server.ui.enabled {
            errors.push(
                "server.ui.assets_dir is set but server.ui.enabled is false — the UI \
                 signs in through the login page, which only exists with the UI enabled"
                    .to_string(),
            );
        }

        errors
    }

    // Forms that still load but are on their way out, one notice per item,
    // sorted: credentials in the env_prefix + secret_keys form or with a
    // vault_path and no `vault:`, API keys with a bare `env`, projects with a
    // singular `credential`, and config.yaml's secrets block. Reported at startup and by --check-config, never
    // refused — a configuration repository moves to the new forms after the
    // release that introduced them ships, not in the same deploy.
    pub fn deprecations(&self) -> Vec<String> {
        let secrets = &self.secrets_config;
        let block = (secrets.cache_ttl_seconds.is_some() || secrets.vault.is_some()).then(|| {
            "config.yaml: the secrets block is deprecated and will be refused in a later \
             release — declare a Vault as a `type: vault` credential (address, mount, \
             env_vars: {token: <VAR>} or kubernetes_role, cache_ttl_seconds) and name it with \
             vault: on the credentials that read from it; env and file credentials need no cache"
                .to_string()
        });
        let mut notices: Vec<String> = self
            .credentials
            .iter()
            .filter_map(|(id, c)| c.deprecation(id))
            .chain(self.api_keys.iter().filter_map(|(id, k)| k.deprecation(id)))
            .chain(self.projects.iter().filter_map(|(id, p)| p.deprecation(id)))
            .chain(block)
            .collect();
        notices.sort();
        notices
    }

    fn is_vault(&self, credential_id: &str) -> bool {
        self.credentials
            .get(credential_id)
            .is_some_and(|c| c.credential_type == crate::domain::credential::CredentialType::Vault)
    }

    // Everything about `server.ui.ldap` that can be known without contacting
    // the directory, so a bad push fails validation instead of every login.
    fn ldap_errors(&self, ldap: &crate::domain::ldap::LdapConfig) -> Vec<String> {
        let mut errors = Vec::new();

        // The login form and the session are the browsable API's; without it
        // there is nothing for LDAP to log anybody into.
        if !self.server.ui.enabled {
            errors.push(
                "server.ui.ldap is set but server.ui.enabled is false — LDAP login is \
                 part of the browsable UI"
                    .to_string(),
            );
        }

        let url = ldap.url.to_ascii_lowercase();
        if url.starts_with("ldap://") {
            if !ldap.starttls {
                errors.push(format!(
                    "server.ui.ldap.url '{}' is plain LDAP — every password would cross \
                     the network in the clear; use ldaps:// or set starttls: true",
                    ldap.url
                ));
            }
        } else if url.starts_with("ldaps://") {
            if ldap.starttls {
                errors.push(
                    "server.ui.ldap: starttls applies to ldap:// URLs; an ldaps:// \
                     connection is already TLS"
                        .to_string(),
                );
            }
        } else {
            errors.push(format!(
                "server.ui.ldap.url '{}' must start with ldaps:// or ldap://",
                ldap.url
            ));
        }

        if !self.credentials.contains_key(&ldap.bind_credential) {
            errors.push(format!(
                "server.ui.ldap.bind_credential references unknown credential '{}'",
                ldap.bind_credential
            ));
        } else if self.is_vault(&ldap.bind_credential) {
            errors.push(vault_as_login(
                "server.ui.ldap.bind_credential",
                &ldap.bind_credential,
            ));
        }
        if !ldap.user_filter.contains("{username}") {
            errors.push(format!(
                "server.ui.ldap.user_filter '{}' has no {{username}} placeholder — it \
                 would match the same entry whoever logs in",
                ldap.user_filter
            ));
        }
        if ldap.access.is_empty() {
            errors.push(
                "server.ui.ldap.access is empty — nobody could log in; name the groups \
                 or users who may"
                    .to_string(),
            );
        }
        for rule in &ldap.access {
            if rule.groups.is_empty() && rule.users.is_empty() {
                errors.push(format!(
                    "server.ui.ldap.access '{}' names no groups and no users",
                    rule.name
                ));
            }
            for id in &rule.projects {
                if !self.projects.contains_key(id) {
                    errors.push(format!(
                        "server.ui.ldap.access '{}' references unknown project '{}'",
                        rule.name, id
                    ));
                }
            }
        }
        errors
    }
}

// Everything wrong with a configuration directory, as items.
//
// Loading fails in two shapes — a file that does not parse (one error) and a
// directory that parses but does not hang together (one per broken
// reference). ConfigErrors holds either as a list; it is web-rs's, since the
// config API reports the same list.
pub use web_rs::config::ConfigErrors;

pub fn load_config(config_dir: &str) -> Result<AppConfig, Box<dyn std::error::Error>> {
    Ok(load_config_detailed(Path::new(config_dir))?)
}

// The same load, keeping the errors as a list. `load_config` is the thin
// wrapper over it that main and the tests already use.
pub fn load_config_detailed(dir: &Path) -> Result<AppConfig, ConfigErrors> {
    // config.yaml is mandatory — without server config we cannot start
    let server_file: ServerFile = load_yaml_file(&dir.join(REQUIRED_CONFIG_FILE))
        .map_err(|e| ConfigErrors::single(format!("{REQUIRED_CONFIG_FILE}: {e}")))?;

    // Every other file holds items. All of them are read before anything is
    // reported, so a push with three typos in three files names all three.
    let mut errors = Vec::new();
    let mut entries: HashMap<&str, Vec<Entry>> = HashMap::new();
    let files = config_files(dir).map_err(ConfigErrors::single)?;
    for file in files.iter().filter(|f| *f != REQUIRED_CONFIG_FILE) {
        if !is_config_path(file) {
            errors.push(format!(
                "{file}: rename it — configuration paths may only use letters, digits, \
                 '-', '_' and '.', in segments that do not start with a dot"
            ));
            continue;
        }
        match fs::read_to_string(dir.join(file)) {
            Ok(text) => read_items(file, &text, &mut entries, &mut errors),
            Err(e) => errors.push(format!("{file}: {e}")),
        }
    }
    let mut section = |name: &str| entries.remove(name).unwrap_or_default();
    let (c, pr, ak, nt, ap) = (
        section("credentials"),
        section("projects"),
        section("api_keys"),
        section("webhooks"),
        section("apis"),
    );
    let credentials = take("credential", c, &mut errors);
    let projects = take("project", pr, &mut errors);
    let api_keys = take("API key", ak, &mut errors);
    let webhooks = take("webhook", nt, &mut errors);
    let apis = take("API", ap, &mut errors);
    if !errors.is_empty() {
        // Cross-references are only worth checking once every item parsed:
        // otherwise one typo'd credential also reports every project using it.
        return Err(ConfigErrors::new(errors));
    }

    let config = AppConfig {
        server: server_file.server,
        projects_config: server_file.projects,
        secrets_config: server_file.secrets,
        config_api: server_file.config_api,
        credentials,
        projects,
        api_keys,
        webhooks,
        apis,
    };

    let errors = config.validate_errors();
    if errors.is_empty() {
        Ok(config)
    } else {
        Err(ConfigErrors::new(errors))
    }
}

// One item as found: which file, its name, and the rest of its keys.
struct Entry {
    file: String,
    name: String,
    body: serde_json::Map<String, serde_json::Value>,
}

// Reads one item file into `entries`, by section. YAML is read with the
// core-schema reader desired state uses (domain::yaml), so `0644` stays a
// string here too, and an error names a position, never the text — a
// configuration file can hold a secret, and the error goes to the log.
fn read_items(
    file: &str,
    text: &str,
    entries: &mut HashMap<&'static str, Vec<Entry>>,
    errors: &mut Vec<String>,
) {
    use serde_json::Value;
    let top = match crate::domain::yaml::to_json(text) {
        Ok(Value::Object(top)) => top,
        // An empty file is a file with nothing in it yet, not an error.
        Ok(Value::Null) => return,
        Ok(_) => {
            errors.push(format!(
                "{file}: expected a mapping with one of: {}",
                SECTIONS.join(", ")
            ));
            return;
        }
        Err(e) => {
            errors.push(format!("{file}: {e}"));
            return;
        }
    };
    for (key, value) in top {
        let Some(section) = SECTIONS.iter().find(|s| **s == key) else {
            errors.push(format!(
                "{file}: unknown key '{key}' (expected one of: {})",
                SECTIONS.join(", ")
            ));
            continue;
        };
        let items = match value {
            Value::Array(items) => items,
            Value::Null => continue,
            _ => {
                errors.push(format!("{file}: '{key}' must be a list of items"));
                continue;
            }
        };
        for (i, item) in items.into_iter().enumerate() {
            let Value::Object(mut body) = item else {
                errors.push(format!("{file}: {key} item {} is not a mapping", i + 1));
                continue;
            };
            let name = match body.remove("name") {
                Some(Value::String(name)) if !name.trim().is_empty() => name,
                _ => {
                    errors.push(format!(
                        "{file}: {key} item {} needs a 'name', its identity",
                        i + 1
                    ));
                    continue;
                }
            };
            entries.entry(section).or_default().push(Entry {
                file: file.to_string(),
                name,
                body,
            });
        }
    }
}

// One section's items, typed. A name defined twice is an error naming both
// files: which one wins would otherwise depend on the order files are read.
fn take<T: serde::de::DeserializeOwned>(
    label: &str,
    entries: Vec<Entry>,
    errors: &mut Vec<String>,
) -> HashMap<String, T> {
    let mut out = HashMap::new();
    let mut seen: HashMap<String, String> = HashMap::new();
    for entry in entries {
        if let Some(first) = seen.get(&entry.name) {
            errors.push(format!(
                "{}: {label} '{}' is already defined in {first}",
                entry.file, entry.name
            ));
            continue;
        }
        seen.insert(entry.name.clone(), entry.file.clone());
        match serde_json::from_value::<T>(serde_json::Value::Object(entry.body)) {
            Ok(item) => {
                out.insert(entry.name, item);
            }
            Err(e) => errors.push(format!("{}: {label} '{}': {e}", entry.file, entry.name)),
        }
    }
    out
}

// Reads and parses a YAML file — fails if it does not exist
fn load_yaml_file<T: serde::de::DeserializeOwned>(
    path: &Path,
) -> Result<T, Box<dyn std::error::Error>> {
    let contents = fs::read_to_string(path)?;
    // No source snippet in errors: a config file can hold a secret, and the
    // error goes to the log. Only `true`/`false` are booleans, as in 0.31.
    let options = serde_saphyr::options! {
        with_snippet: false,
        strict_booleans: true,
    };
    let parsed = serde_saphyr::from_str_with_options(&contents, options)?;
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n",
        )
        .unwrap();
        for (name, body) in files {
            let path = dir.path().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        dir
    }

    fn load_err(dir: &tempfile::TempDir) -> String {
        load_config(dir.path().to_str().unwrap())
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    }

    // server.url is shown back as a link: anything but a web address is a typo.
    #[test]
    fn server_url_must_be_a_web_address() {
        let dir = dir_with(&[(
            "config.yaml",
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  url: \"gitops.example\"\n",
        )]);
        assert!(load_err(&dir).contains("server.url"), "{}", load_err(&dir));
        let dir = dir_with(&[(
            "config.yaml",
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\n  url: \"https://gitops.example\"\n",
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.server.url.as_deref(), Some("https://gitops.example"));
    }

    const PROJECT: &str =
        "projects:\n  - name: prj-state\n    git_url: https://example.com/state.git\n";
    const CREDENTIAL: &str = "credentials:\n  - name: cred-key\n    type: token\n    env_prefix: JT\n    secret_keys:\n      token: KEY\n";

    #[test]
    fn webhooks_load_as_items() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECT),
            ("credentials/cred-key.yaml", CREDENTIAL),
            (
                "webhooks/ntf-drift.yaml",
                "webhooks:\n  - name: ntf-drift\n    project: prj-state\n    events: [commit_pulled]\n    credential: cred-key\n    webhook:\n      url: https://jobs.example.com/launch\n",
            ),
        ]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        let n = &cfg.webhooks["ntf-drift"];
        assert_eq!(n.project, "prj-state");
        assert_eq!(n.credential.as_deref(), Some("cred-key"));
    }

    #[test]
    fn a_webhook_must_name_known_projects_and_credentials() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECT),
            (
                "webhooks/n.yaml",
                "webhooks:\n  - name: n\n    project: prj-nope\n    events: [pull_failed]\n    credential: cred-nope\n    webhook:\n      url: https://hooks.example.com/x\n      tls: {client_credential: cred-mtls-nope}\n",
            ),
        ]);
        let err = load_err(&dir);
        assert!(
            err.contains("Webhook 'n' references unknown project 'prj-nope'"),
            "{err}"
        );
        assert!(
            err.contains("Webhook 'n' references unknown credential 'cred-nope'"),
            "{err}"
        );
        assert!(
            err.contains("Webhook 'n' references unknown credential 'cred-mtls-nope'"),
            "{err}"
        );
    }

    #[test]
    fn items_load_from_any_depth_by_their_top_level_key() {
        let dir = dir_with(&[
            // Folders are for people: an item may live anywhere.
            ("credentials/team-a/key.yaml", CREDENTIAL),
            (
                "projects/team-a/infra/prj.yaml",
                "projects:\n  - name: prj\n    description: The infra repo\n    git_url: https://example.com/infra.git\n",
            ),
            // Hidden entries are not configuration.
            (".git/HEAD.yaml", "nonsense: ["),
            ("projects/.swp.yaml", "nonsense: ["),
            // Neither is anything that is not YAML.
            ("README.md", "# notes"),
        ]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(
            cfg.projects["prj"].description.as_deref(),
            Some("The infra repo")
        );
        assert!(cfg.credentials.contains_key("cred-key"));
    }

    #[test]
    fn a_configmap_mount_of_symlinks_loads() {
        // What the kubelet writes: the real files in a hidden folder, and a
        // link per key at the top.
        let dir = dir_with(&[("..2026_10_25/projects-state.yaml", PROJECT)]);
        std::os::unix::fs::symlink(dir.path().join("..2026_10_25"), dir.path().join("..data"))
            .unwrap();
        std::os::unix::fs::symlink(
            "..data/projects-state.yaml",
            dir.path().join("projects-state.yaml"),
        )
        .unwrap();
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert!(cfg.projects.contains_key("prj-state"));
    }

    #[test]
    fn one_file_may_hold_several_items_and_types() {
        let dir = dir_with(&[(
            "all.yaml",
            &format!(
                "{PROJECT}{}",
                "credentials:\n  - name: a\n    type: token\n    env_prefix: A\n  - name: b\n    type: token\n    env_prefix: B\n"
            ),
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.credentials.len(), 2);
        assert_eq!(cfg.projects.len(), 1);
    }

    #[test]
    fn duplicate_names_unknown_keys_and_missing_names_are_all_named() {
        let dir = dir_with(&[
            ("projects/one.yaml", PROJECT),
            ("projects/two.yaml", PROJECT),
            ("typo.yaml", "projetcs:\n  - name: x\n"),
            (
                "nameless.yaml",
                "projects:\n  - git_url: https://example.com/x.git\n",
            ),
            ("bad field.yaml", "projects: []\n"),
        ]);
        let err = load_err(&dir);
        for expected in [
            "projects/two.yaml: project 'prj-state' is already defined in projects/one.yaml",
            "typo.yaml: unknown key 'projetcs'",
            "nameless.yaml: projects item 1 needs a 'name'",
            "bad field.yaml: rename it",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    #[test]
    fn apis_load_as_items_and_name_what_they_reference() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECT),
            ("credentials/cred-key.yaml", CREDENTIAL),
            (
                "apis/users.yaml",
                "apis:\n  - name: users\n    project: prj-state\n    credential: cred-key\n    trigger: {paths: [users/*.json], on: [change, schedule], schedule: '0 3 * * *'}\n    api:\n      url: https://x/users/$file.stem\n",
            ),
        ]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.apis["users"].project, "prj-state");
        let dir = dir_with(&[(
            "apis.yaml",
            "apis:\n  - name: a\n    project: nope\n    credential: cred-x\n    trigger: {on: [schedule], schedule: 'every day'}\n    api:\n      url: https://x\n",
        )]);
        let err = load_err(&dir);
        for expected in [
            "API 'a' references unknown project 'nope'",
            "API 'a' references unknown credential 'cred-x'",
            "API 'a': trigger.schedule 'every day' is not a cron schedule",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    #[test]
    fn broken_references_are_all_named() {
        let dir = dir_with(&[
            (
                "prj.yaml",
                "projects:\n  - name: prj\n    git_url: https://example.com/x.git\n    credentials: [cred-x]\n",
            ),
            (
                "w.yaml",
                "webhooks:\n  - name: w\n    project: missing\n    events: [pull_failed]\n    credential: cred-y\n    webhook:\n      url: https://hooks.example.com/x\n",
            ),
            (
                "keys.yaml",
                "api_keys:\n  - name: k\n    env_vars:\n      key: K\n    role: restricted\n    projects: [nope]\n",
            ),
        ]);
        let err = load_err(&dir);
        for expected in [
            "Project 'prj' references unknown credential 'cred-x'",
            "Webhook 'w' references unknown project 'missing'",
            "Webhook 'w' references unknown credential 'cred-y'",
            "API key 'k' references unknown project 'nope'",
        ] {
            assert!(err.contains(expected), "missing {expected:?} in {err}");
        }
    }

    // The sections applications and connectors were removed: a file still
    // using them is named, not ignored.
    #[test]
    fn the_removed_sections_are_unknown_keys() {
        let dir = dir_with(&[(
            "app.yaml",
            "applications:\n  - name: a\n    connector: kv\n    project: x\n",
        )]);
        assert!(
            load_err(&dir).contains("app.yaml: unknown key 'applications'"),
            "{}",
            load_err(&dir)
        );
    }

    // A project's name is its checkout's folder: `../x` or an absolute name
    // would clone (and reset --hard) outside projects.dir.
    #[test]
    fn project_names_that_would_escape_projects_dir_are_refused() {
        for name in ["../outside", "/srv/other", ".venvs", "a/b"] {
            let body = format!(
                "projects:\n  - name: \"{name}\"\n    git_url: https://example.com/x.git\n"
            );
            let dir = dir_with(&[("prj.yaml", body.as_str())]);
            let err = load_err(&dir);
            assert!(
                err.contains(&format!("Project '{name}' must use only")),
                "{err}"
            );
        }
    }

    #[test]
    fn unknown_keys_inside_an_item_fail_the_load() {
        let dir = dir_with(&[(
            "prj.yaml",
            "projects:\n  - name: a\n    git_url: https://example.com/x.git\n    credential_ids: []\n",
        )]);
        let err = load_err(&dir);
        assert!(
            err.contains("prj.yaml: project 'a'") && err.contains("credential_ids"),
            "{err}"
        );
    }

    // The old forms load (repositories move after the release ships) and
    // are reported; mixing an old and a new form in one item is refused.
    #[test]
    fn deprecated_env_forms_load_with_notices_and_mixed_forms_are_refused() {
        let dir = dir_with(&[
            (
                "credentials.yaml",
                "credentials:\n  - name: cred-old\n    type: token\n    env_prefix: OLD\n    secret_keys:\n      token: TOKEN\n  - name: cred-new\n    type: token\n    env_vars:\n      token: NEW_TOKEN\n",
            ),
            (
                "api_keys.yaml",
                "api_keys:\n  - name: key-old\n    env: OLD_KEY\n    role: admin\n  - name: key-new\n    env_vars:\n      key: NEW_KEY\n    role: admin\n",
            ),
        ]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("deprecated forms still load");
        assert_eq!(
            cfg.deprecations(),
            vec![
                "API key 'key-old': env is deprecated and will be refused in a later release; \
                 write env_vars: {key: OLD_KEY}",
                "credential 'cred-old': env_prefix + secret_keys is deprecated and will be refused \
                 in a later release; write env_vars: {token: OLD_TOKEN}",
            ]
        );

        let dir = dir_with(&[
            (
                "credentials.yaml",
                "credentials:\n  - name: cred-mixed\n    type: token\n    env_prefix: M\n    env_vars:\n      token: M_TOKEN\n",
            ),
            (
                "api_keys.yaml",
                "api_keys:\n  - name: key-mixed\n    env: A\n    env_vars:\n      key: B\n",
            ),
        ]);
        let err = load_err(&dir);
        assert!(
            err.contains("Credential 'cred-mixed' sets both env_vars and env_prefix")
                && err.contains("API key 'key-mixed' sets both env_vars.key and env"),
            "{err}"
        );
    }

    #[test]
    fn configuration_paths() {
        for ok in ["config.yaml", "a/b/c.yml", "webhooks/team-a/notify.yaml"] {
            assert!(is_config_path(ok), "{ok}");
        }
        for bad in [
            "",
            "a.json",
            "../x.yaml",
            "a/../x.yaml",
            "/abs.yaml",
            "a//b.yaml",
            ".hidden.yaml",
            "a/.git/x.yaml",
            "with space.yaml",
            "a\\b.yaml",
        ] {
            assert!(!is_config_path(bad), "{bad:?}");
        }
    }

    #[test]
    fn restart_only_settings_are_named() {
        let a = dir_with(&[]);
        let b = tempfile::tempdir().unwrap();
        fs::write(
            b.path().join("config.yaml"),
            "server:\n  host: \"127.0.0.1\"\n  port: 9191\nprojects:\n  dir: /var/lib/x\n",
        )
        .unwrap();
        let a = RestartOnlySettings::from_config(&load_config(a.path().to_str().unwrap()).unwrap());
        let b = RestartOnlySettings::from_config(&load_config(b.path().to_str().unwrap()).unwrap());
        assert_eq!(a.changed_keys(&b), vec!["server.port", "projects.dir"]);
    }

    // A vault_path needs a Vault: the one it names, or the deprecated
    // config.yaml block.
    #[test]
    fn a_vault_path_without_a_vault_is_refused() {
        let creds = (
            "credentials.yaml",
            "credentials:\n  - name: cred-v\n    type: token\n    vault_path: team/x\n",
        );
        let err = load_err(&dir_with(&[creds]));
        assert!(
            err.contains("names no vault to read it from"),
            "error was: {err}"
        );

        // The deprecated config.yaml block still resolves it, with a notice
        let dir = dir_with(&[
            creds,
            (
                "config.yaml",
                "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  vault:\n    address: \"http://vault.example:8200\"\n",
            ),
        ]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("a configured Vault validates");
        let notes = cfg.deprecations();
        assert_eq!(notes.len(), 2, "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("credential 'cred-v': a vault_path without `vault:`"))
        );
    }

    // A Vault is a credential: others name it with `vault:`, nothing hands it
    // to a script, and config.yaml needs no secrets block.
    #[test]
    fn a_vault_credential_is_named_by_the_credentials_that_read_from_it() {
        let dir = dir_with(&[(
            "creds.yaml",
            concat!(
                "credentials:\n",
                "  - name: cred-vault\n    type: vault\n    address: https://vault.example:8200\n",
                "    env_vars:\n      token: VAULT_TOKEN\n    cache_ttl_seconds: 300\n",
                "  - name: cred-isilon\n    type: username_password\n    vault: cred-vault\n",
                "    vault_path: infra/isilon\n",
            ),
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.credentials["cred-isilon"].backend(), "vault");
        assert_eq!(cfg.credentials["cred-vault"].backend(), "vault");
        assert!(cfg.deprecations().is_empty(), "{:?}", cfg.deprecations());
    }

    #[test]
    fn a_vault_credential_misused_is_refused_naming_why() {
        let dir = dir_with(&[
            ("projects.yaml", PROJECT),
            (
                "webhooks/w.yaml",
                "webhooks:\n  - name: w\n    project: prj-state\n    events: [commit_pulled]\n    credential: cred-vault\n    webhook:\n      url: https://jobs.example.com/launch\n",
            ),
            (
                "creds.yaml",
                concat!(
                    "credentials:\n",
                    // no address, no way to log in
                    "  - name: cred-vault\n    type: vault\n",
                    // a Vault with a login's fields and a jwt_path for no role
                    "  - name: cred-vault2\n    type: vault\n    address: vault.example\n",
                    "    env_vars:\n      token: T\n      username: U\n    jwt_path: /x\n",
                    "    secret_file: /run/x.json\n",
                    // names a credential that is not a Vault
                    "  - name: cred-a\n    type: token\n    vault: cred-gh\n    vault_path: x\n",
                    "  - name: cred-gh\n    type: token\n    env_vars:\n      token: GH\n",
                    // names a Vault that does not exist
                    "  - name: cred-b\n    type: token\n    vault: cred-nope\n    vault_path: x\n",
                    // vault: with no vault_path
                    "  - name: cred-c\n    type: token\n    vault: cred-vault\n",
                    // a Vault's setting on a login
                    "  - name: cred-odd\n    type: token\n    address: https://x\n    env_vars:\n      token: T\n",
                ),
            ),
        ]);
        let err = load_err(&dir);
        for expected in [
            "is a Vault but has no address",
            "has no way to log in to Vault",
            "address 'vault.example' must be an http:// or https:// address",
            "env_vars.username is not a field of a Vault",
            "is a Vault and sets secret_file",
            "sets jwt_path without kubernetes_role",
            "names 'cred-gh' as its vault, which is not a `type: vault` credential",
            "Credential 'cred-b' references unknown vault 'cred-nope'",
            "Credential 'cred-c' names vault 'cred-vault' but no vault_path",
            "sets address, which only a `type: vault` credential uses",
            "Webhook 'w' uses credential 'cred-vault', which is a Vault connection",
        ] {
            assert!(err.contains(expected), "missing '{expected}' in: {err}");
        }
    }

    // A project names its credential as every item does: credentials, a
    // list. The singular form still loads, deprecated; more than one, or
    // both forms, are refused.
    #[test]
    fn a_project_takes_credentials_like_every_item() {
        let base = |extra: &str| {
            dir_with(&[
                (
                    "cred.yaml",
                    "credentials:\n  - name: cred-gh\n    type: token\n    env_vars:\n      token: GH\n  - name: cred-two\n    type: token\n    env_vars:\n      token: TWO\n",
                ),
                (
                    "projects.yaml",
                    &format!(
                        "projects:\n  - name: prj-test\n    git_url: \"https://example.com/repo.git\"\n{extra}"
                    ),
                ),
            ])
        };

        let dir = base("    credentials:\n      - cred-gh\n");
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.projects["prj-test"].credential_id(), Some("cred-gh"));
        assert!(cfg.deprecations().is_empty(), "{:?}", cfg.deprecations());

        let dir = base("    credential: cred-gh\n");
        let cfg = load_config(dir.path().to_str().unwrap()).expect("the old form loads");
        assert_eq!(cfg.projects["prj-test"].credential_id(), Some("cred-gh"));
        assert_eq!(
            cfg.deprecations(),
            vec![
                "project 'prj-test': credential is deprecated and will be refused in a later \
                 release; write credentials: [cred-gh]"
            ]
        );

        // One credential listed twice is still one.
        let dir = base("    credentials:\n      - cred-gh\n      - cred-gh\n");
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.projects["prj-test"].credential_id(), Some("cred-gh"));

        let dir =
            base("    credentials:\n      - cred-gh\n      - cred-two\n    credential: cred-gh\n");
        let err = load_err(&dir);
        assert!(err.contains("lists 2 credentials"), "{err}");
        assert!(
            err.contains("sets both credentials and credential"),
            "{err}"
        );
    }

    #[test]
    fn the_config_yaml_secrets_block_still_loads_and_says_what_to_write() {
        let dir = dir_with(&[(
            "config.yaml",
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  cache_ttl_seconds: 30\n",
        )]);
        let cfg = load_config(dir.path().to_str().unwrap()).expect("loads");
        assert_eq!(cfg.secrets_config.cache_ttl_seconds, Some(30));
        let notes = cfg.deprecations();
        assert_eq!(notes.len(), 1);
        assert!(
            notes[0].contains("the secrets block is deprecated"),
            "{notes:?}"
        );
    }

    #[test]
    fn the_deprecated_secrets_vault_with_a_zero_timeout_is_refused() {
        let dir = dir_with(&[(
            "config.yaml",
            "server:\n  host: \"127.0.0.1\"\n  port: 9090\nsecrets:\n  vault:\n    address: https://vault.example\n    timeout_seconds: 0\n",
        )]);
        let err = load_err(&dir);
        assert!(err.contains("secrets.vault.timeout_seconds 0"), "{err}");
    }
}
