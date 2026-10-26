use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// A notification: which events, from which applications, and where they go.
// The `type` names the receiver; its settings live in the block of the same
// name (`webhook:`, `ascender:`, `awx:`), so a block that belongs to another
// type is a load error rather than settings silently ignored, and adding a
// type later is a new block, with nothing to rename.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notification {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub notification_type: NotificationType,
    pub events: Vec<Event>,
    // Empty = every application.
    #[serde(default)]
    pub applications: Vec<String>,
    #[serde(default)]
    pub webhook: Option<WebhookTarget>,
    #[serde(default)]
    pub ascender: Option<JobTemplateTarget>,
    #[serde(default)]
    pub awx: Option<JobTemplateTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NotificationType {
    // Any HTTP receiver: its URL, method, headers, body and authentication.
    Webhook,
    // A job template in Ascender, through its webhook receiver or its launch API.
    Ascender,
    // The same in AWX. The routes are Ascender's today; they are separate
    // types so neither carries the other's name, and either can follow its
    // own product when they part.
    Awx,
}

impl NotificationType {
    pub fn as_str(self) -> &'static str {
        match self {
            NotificationType::Webhook => "webhook",
            NotificationType::Ascender => "ascender",
            NotificationType::Awx => "awx",
        }
    }
}

// What happened to an application, as a notification names it. One run
// produces at most one event (application::notify::detect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    // The system differs from a commit already applied: someone changed it
    // outside git. With auto_sync it is reverted in the same run.
    DriftDetected,
    // A new commit is waiting to be applied (auto_sync off).
    OutOfSync,
    // A sync applied at least one change from git (a commit or a rollback).
    SyncSucceeded,
    // A check or a sync could not finish, or some of its operations failed.
    SyncFailed,
    // In sync again after a failure or a difference.
    BackInSync,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::DriftDetected => "drift_detected",
            Event::OutOfSync => "out_of_sync",
            Event::SyncSucceeded => "sync_succeeded",
            Event::SyncFailed => "sync_failed",
            Event::BackInSync => "back_in_sync",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookTarget {
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    // Resolved per send; `auth` says which of their fields go where.
    #[serde(default)]
    pub credentials: Vec<String>,
    #[serde(default)]
    pub auth: Auth,
    // Fixed headers. Not secret: anything secret belongs in a credential.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    // A template with {{field}} placeholders; None sends the default message.
    #[serde(default)]
    pub body: Option<String>,
    // An HMAC-SHA256 of the body, for receivers that check signatures.
    #[serde(default)]
    pub sign: Option<Signing>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
}

// How a credential reaches the receiver. Written as one word (`bearer`,
// `basic`, `none`) or as `{header: Name}`.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(untagged)]
pub enum Auth {
    Scheme(AuthScheme),
    Header {
        header: String,
    },
    // `auth:` left out: a bearer token when there is a credential, nothing
    // when there is none.
    #[default]
    #[serde(skip)]
    Default,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthScheme {
    // Authorization: Bearer <token>
    Bearer,
    // Authorization: Basic base64(<username>:<password>)
    Basic,
    // Nothing: the credentials are only there for the body or a signature.
    None,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signing {
    #[serde(default = "default_signature_header")]
    pub header: String,
    // Written before the hex digest, as GitHub writes `sha256=`.
    #[serde(default = "default_signature_prefix")]
    pub prefix: String,
    // The credential field holding the shared secret.
    #[serde(default = "default_signature_field")]
    pub secret_field: String,
}

// A job template (or workflow job template) to start, in Ascender or AWX.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobTemplateTarget {
    // The controller's address, without /api.
    pub url: String,
    #[serde(default)]
    pub job_template: Option<u64>,
    #[serde(default)]
    pub workflow_job_template: Option<u64>,
    #[serde(default)]
    pub trigger: JobTrigger,
    // `webhook`: one credential whose `token` is the template's webhook key.
    // `launch`: one credential whose `token` is a user's OAuth token.
    #[serde(default)]
    pub credentials: Vec<String>,
    // `launch` only: variables sent beside the event, which arrives as
    // `unified_gitops`. The template must prompt for variables on launch.
    #[serde(default)]
    pub extra_vars: serde_json::Map<String, serde_json::Value>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum JobTrigger {
    // The template's own webhook receiver, in GitLab mode: its key in
    // X-Gitlab-Token, no user needed. The playbook reads the event as
    // `<prefix>_webhook_payload`.
    #[default]
    Webhook,
    // POST .../launch/ as a user, with extra_vars.
    Launch,
}

impl JobTrigger {
    pub fn as_str(self) -> &'static str {
        match self {
            JobTrigger::Webhook => "webhook",
            JobTrigger::Launch => "launch",
        }
    }
}

fn default_method() -> String {
    "POST".to_string()
}

fn default_timeout_seconds() -> u64 {
    30
}

fn default_retries() -> u32 {
    3
}

fn default_signature_header() -> String {
    "X-Hub-Signature-256".to_string()
}

fn default_signature_prefix() -> String {
    "sha256=".to_string()
}

fn default_signature_field() -> String {
    "secret".to_string()
}

// The fields a body template may name: the default message's own keys, plus
// a short revision. Checked at load, so a typo fails the push, not a send.
pub const TEMPLATE_FIELDS: [&str; 15] = [
    "event",
    "application",
    "description",
    "status",
    "previous_status",
    "cause",
    "reverted",
    "revision",
    "revision_short",
    "applied",
    "changes",
    "errors",
    "at",
    "url",
    "notification",
];

const METHODS: [&str; 3] = ["POST", "PUT", "PATCH"];

// Longest a send may take: a receiver that never answers must not hold the
// sender (and every notification queued behind it) for long.
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_RETRIES: u32 = 10;

impl Notification {
    // Everything wrong with this item on its own; references to applications
    // and credentials are checked by the loader, which knows them.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        let at = |what: String| format!("Notification '{id}': {what}");
        if self.events.is_empty() {
            errors.push(at("needs at least one event".to_string()));
        }
        let blocks = [
            ("webhook", self.webhook.is_some()),
            ("ascender", self.ascender.is_some()),
            ("awx", self.awx.is_some()),
        ];
        let wanted = self.notification_type.as_str();
        for (block, present) in blocks {
            if block == wanted && !present {
                errors.push(at(format!("type {wanted} needs a '{wanted}:' block")));
            }
            if block != wanted && present {
                errors.push(at(format!(
                    "a '{block}:' block does not belong to type {wanted}"
                )));
            }
        }
        if let Some(webhook) = &self.webhook {
            errors.extend(webhook.errors().into_iter().map(at));
        }
        for target in [&self.ascender, &self.awx].into_iter().flatten() {
            errors.extend(target.errors().into_iter().map(at));
        }
        errors
    }

    // The credentials this notification names, whichever its type.
    pub fn credentials(&self) -> &[String] {
        if let Some(webhook) = &self.webhook {
            return &webhook.credentials;
        }
        match (&self.ascender, &self.awx) {
            (Some(target), _) | (None, Some(target)) => &target.credentials,
            _ => &[],
        }
    }

    // Whether this notification wants `event` from `application`.
    pub fn wants(&self, event: Event, application: &str) -> bool {
        self.events.contains(&event)
            && (self.applications.is_empty() || self.applications.iter().any(|a| a == application))
    }

    // Where it sends, without anything a URL can carry that is secret: a
    // chat webhook's path or query is often the credential itself.
    pub fn destination(&self) -> String {
        let url = match (&self.webhook, &self.ascender, &self.awx) {
            (Some(w), _, _) => &w.url,
            (_, Some(t), _) | (_, _, Some(t)) => &t.url,
            _ => return String::new(),
        };
        origin(url)
    }
}

impl WebhookTarget {
    fn errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        errors.extend(url_errors("webhook.url", &self.url));
        if !METHODS.contains(&self.method.as_str()) {
            errors.push(format!(
                "webhook.method '{}' must be one of {}",
                self.method,
                METHODS.join(", ")
            ));
        }
        if let Some(body) = &self.body {
            errors.extend(template_errors(body));
        }
        let needs_credential = match &self.auth {
            Auth::Scheme(AuthScheme::Bearer | AuthScheme::Basic) | Auth::Header { .. } => true,
            Auth::Scheme(AuthScheme::None) | Auth::Default => false,
        };
        if needs_credential && self.credentials.is_empty() {
            errors.push("webhook.auth needs a credential in webhook.credentials".to_string());
        }
        if let Auth::Header { header } = &self.auth
            && !is_header_name(header)
        {
            errors.push(format!(
                "webhook.auth header '{header}' is not a header name"
            ));
        }
        for name in self.headers.keys() {
            if !is_header_name(name) {
                errors.push(format!("webhook.headers '{name}' is not a header name"));
            }
        }
        if let Some(sign) = &self.sign {
            if !is_header_name(&sign.header) {
                errors.push(format!(
                    "webhook.sign header '{}' is not a header name",
                    sign.header
                ));
            }
            if self.credentials.is_empty() {
                errors.push("webhook.sign needs a credential holding its secret".to_string());
            }
        }
        errors.extend(bounds_errors("webhook", self.timeout_seconds, self.retries));
        errors
    }
}

impl JobTemplateTarget {
    fn errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        errors.extend(url_errors("url", &self.url));
        if self.url.trim_end_matches('/').ends_with("/api") || self.url.contains("/api/v2") {
            errors.push(format!(
                "url '{}' is the controller's address, without /api",
                self.url
            ));
        }
        match (self.job_template, self.workflow_job_template) {
            (Some(_), Some(_)) => {
                errors.push("set job_template or workflow_job_template, not both".to_string())
            }
            (None, None) => {
                errors.push("needs a job_template or a workflow_job_template".to_string())
            }
            _ => {}
        }
        if self.credentials.len() != 1 {
            errors.push(format!(
                "trigger {} needs exactly one credential, whose token is {}",
                self.trigger.as_str(),
                match self.trigger {
                    JobTrigger::Webhook => "the template's webhook key",
                    JobTrigger::Launch => "a user's token",
                }
            ));
        }
        if self.trigger == JobTrigger::Webhook && !self.extra_vars.is_empty() {
            errors.push(
                "extra_vars only go with trigger launch: a webhook sends the event itself"
                    .to_string(),
            );
        }
        errors.extend(bounds_errors(
            "job template",
            self.timeout_seconds,
            self.retries,
        ));
        errors
    }

    // The route a send goes to.
    pub fn endpoint(&self) -> String {
        let (collection, id) = match (self.job_template, self.workflow_job_template) {
            (Some(id), _) => ("job_templates", id),
            (None, Some(id)) => ("workflow_job_templates", id),
            (None, None) => ("job_templates", 0),
        };
        let action = match self.trigger {
            JobTrigger::Webhook => "gitlab",
            JobTrigger::Launch => "launch",
        };
        format!(
            "{}/api/v2/{collection}/{id}/{action}/",
            self.url.trim_end_matches('/')
        )
    }
}

fn url_errors(field: &str, url: &str) -> Vec<String> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Vec::new()
    } else {
        vec![format!(
            "{field} '{}' must be an http:// or https:// address",
            origin(url)
        )]
    }
}

fn bounds_errors(what: &str, timeout_seconds: u64, retries: u32) -> Vec<String> {
    let mut errors = Vec::new();
    if timeout_seconds == 0 || timeout_seconds > MAX_TIMEOUT_SECONDS {
        errors.push(format!(
            "{what} timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}"
        ));
    }
    if retries > MAX_RETRIES {
        errors.push(format!("{what} retries must be at most {MAX_RETRIES}"));
    }
    errors
}

// Placeholders a body names that no message has.
fn template_errors(body: &str) -> Vec<String> {
    placeholders(body)
        .filter(|name| !TEMPLATE_FIELDS.contains(name))
        .map(|name| {
            format!(
                "webhook.body names '{{{{{name}}}}}', which no message has (one of: {})",
                TEMPLATE_FIELDS.join(", ")
            )
        })
        .collect()
}

// Every {{name}} in a template, in order.
pub fn placeholders(template: &str) -> impl Iterator<Item = &str> {
    template.split("{{").skip(1).filter_map(|rest| {
        rest.split_once("}}")
            .map(|(name, _)| name.trim())
            .filter(|name| !name.is_empty())
    })
}

fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// scheme://host[:port] of a URL, or the text up to its first '/' after the
// scheme; never its user info, path or query.
pub fn origin(url: &str) -> String {
    let (scheme, rest) = url.split_once("://").unwrap_or(("", url));
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    if scheme.is_empty() {
        host.to_string()
    } else {
        format!("{scheme}://{host}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Notification {
        serde_saphyr::from_str(yaml).unwrap()
    }

    #[test]
    fn a_webhook_with_its_block_is_valid() {
        let n = parse(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://alerts.example.com/x\n",
        );
        assert!(n.errors("n").is_empty(), "{:?}", n.errors("n"));
        assert_eq!(n.webhook.unwrap().method, "POST");
    }

    #[test]
    fn the_type_and_its_block_must_agree() {
        let n = parse(
            "type: ascender\nevents: [sync_failed]\nwebhook:\n  url: https://alerts.example.com/x\n",
        );
        let errors = n.errors("n").join("\n");
        assert!(
            errors.contains("type ascender needs a 'ascender:' block"),
            "{errors}"
        );
        assert!(
            errors.contains("'webhook:' block does not belong to type ascender"),
            "{errors}"
        );
    }

    #[test]
    fn an_unknown_event_or_key_fails_to_parse() {
        assert!(
            serde_saphyr::from_str::<Notification>(
                "type: webhook\nevents: [drifted]\nwebhook:\n  url: https://x.example.com\n"
            )
            .is_err()
        );
        assert!(serde_saphyr::from_str::<Notification>(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  token: x\n"
        )
        .is_err());
    }

    #[test]
    fn a_body_may_only_name_message_fields() {
        let n = parse(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  body: '{\"a\": \"{{application}}\", \"b\": {{chnages}}}'\n",
        );
        let errors = n.errors("n").join("\n");
        assert!(errors.contains("'{{chnages}}'"), "{errors}");
        assert!(!errors.contains("{{application}}"), "{errors}");
        assert_eq!(n.errors("n").len(), 1, "{errors}");
    }

    #[test]
    fn auth_takes_a_scheme_or_a_header() {
        let bearer = parse(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  credentials: [c]\n  auth: basic\n",
        );
        assert_eq!(
            bearer.webhook.unwrap().auth,
            Auth::Scheme(AuthScheme::Basic)
        );
        let header = parse(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  credentials: [c]\n  auth: {header: X-Gitlab-Token}\n",
        );
        assert_eq!(
            header.webhook.unwrap().auth,
            Auth::Header {
                header: "X-Gitlab-Token".into()
            }
        );
        let missing = parse(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  auth: bearer\n",
        );
        assert!(missing.errors("n").join("").contains("needs a credential"));
    }

    #[test]
    fn a_job_template_target_names_one_template_and_one_credential() {
        let n = parse(
            "type: ascender\nevents: [drift_detected]\nascender:\n  url: https://ascender.example.com\n  job_template: 7\n  credentials: [k]\n",
        );
        assert!(n.errors("n").is_empty(), "{:?}", n.errors("n"));
        let target = n.ascender.unwrap();
        assert_eq!(target.trigger, JobTrigger::Webhook);
        assert_eq!(
            target.endpoint(),
            "https://ascender.example.com/api/v2/job_templates/7/gitlab/"
        );

        let launch = parse(
            "type: awx\nevents: [drift_detected]\nawx:\n  url: https://awx.example.com/\n  workflow_job_template: 9\n  trigger: launch\n  credentials: [t]\n",
        );
        assert_eq!(
            launch.awx.unwrap().endpoint(),
            "https://awx.example.com/api/v2/workflow_job_templates/9/launch/"
        );

        let wrong = parse(
            "type: ascender\nevents: [drift_detected]\nascender:\n  url: https://ascender.example.com/api/v2\n  job_template: 7\n  workflow_job_template: 8\n  extra_vars: {a: 1}\n",
        );
        let errors = wrong.errors("n").join("\n");
        assert!(errors.contains("without /api"), "{errors}");
        assert!(errors.contains("not both"), "{errors}");
        assert!(errors.contains("exactly one credential"), "{errors}");
        assert!(
            errors.contains("extra_vars only go with trigger launch"),
            "{errors}"
        );
    }

    #[test]
    fn wants_filters_on_event_and_application() {
        let n = parse(
            "type: webhook\nevents: [sync_failed]\napplications: [a]\nwebhook:\n  url: https://x.example.com\n",
        );
        assert!(n.wants(Event::SyncFailed, "a"));
        assert!(!n.wants(Event::SyncFailed, "b"));
        assert!(!n.wants(Event::DriftDetected, "a"));
        let all =
            parse("type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n");
        assert!(all.wants(Event::SyncFailed, "anything"));
    }

    #[test]
    fn the_destination_never_shows_a_path_or_user_info() {
        assert_eq!(
            origin("https://user:pw@hooks.example.com/services/T0/B0/secret?x=1"),
            "https://hooks.example.com"
        );
        assert_eq!(origin("http://127.0.0.1:9000/x"), "http://127.0.0.1:9000");
    }
}
