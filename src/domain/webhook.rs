use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// A webhook: which events, from which applications and projects, and where
// they go.
// The `type` names the receiver; its settings live in the block of the same
// name (`webhook:`, `ascender:`, `awx:`), so a block that belongs to another
// type is a load error rather than settings silently ignored, and adding a
// type later is a new block, with nothing to rename.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "type")]
    pub webhook_type: WebhookType,
    pub events: Vec<Event>,
    // `all`, or the applications by name. Left out or `[]`: none, as an API
    // key or an LDAP rule with no applications reaches none.
    #[serde(default)]
    pub applications: Selection,
    // `all`, or projects by name: their own events (a pull that brought
    // commits, or failed), and the events of every application reading from
    // them, including applications added later. Combined with
    // `applications`.
    #[serde(default)]
    pub projects: Selection,
    #[serde(default)]
    pub webhook: Option<HttpTarget>,
    #[serde(default)]
    pub ascender: Option<JobTemplateTarget>,
    #[serde(default)]
    pub awx: Option<JobTemplateTarget>,
}

// Which applications (or projects) a webhook listens to: the word `all`, or
// a list.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Selection {
    All(AllWord),
    Named(Vec<String>),
}

// The one word `applications:` and `projects:` take instead of a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllWord {
    All,
}

impl Default for Selection {
    fn default() -> Self {
        Selection::Named(Vec::new())
    }
}

impl Selection {
    pub fn is_all(&self) -> bool {
        matches!(self, Selection::All(_))
    }

    // The names listed; empty for `all`.
    pub fn named(&self) -> &[String] {
        match self {
            Selection::All(_) => &[],
            Selection::Named(names) => names,
        }
    }

    pub fn includes(&self, name: &str) -> bool {
        match self {
            Selection::All(_) => true,
            Selection::Named(names) => names.iter().any(|a| a == name),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Selection::Named(names) if names.is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum WebhookType {
    // Any HTTP receiver: its URL, method, headers, body and authentication.
    Webhook,
    // A job template in Ascender, through its own webhook receiver.
    Ascender,
    // The same in AWX. The routes are Ascender's today; they are separate
    // types so neither carries the other's name, and either can follow its
    // own product when they part.
    Awx,
}

impl WebhookType {
    pub fn as_str(self) -> &'static str {
        match self {
            WebhookType::Webhook => "webhook",
            WebhookType::Ascender => "ascender",
            WebhookType::Awx => "awx",
        }
    }
}

// What happened, as a webhook names it: to an application (one run produces
// at most one, application::webhooks::detect), or to a project's checkout
// (one pull produces at most one, application::webhooks::detect_pull).
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
    // A pull brought new commits into a project's checkout.
    CommitPulled,
    // A project's repository could not be pulled: the applications reading
    // from it stay on the commit they have.
    PullFailed,
    // Pulls work again after failing.
    PullRecovered,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::DriftDetected => "drift_detected",
            Event::OutOfSync => "out_of_sync",
            Event::SyncSucceeded => "sync_succeeded",
            Event::SyncFailed => "sync_failed",
            Event::BackInSync => "back_in_sync",
            Event::CommitPulled => "commit_pulled",
            Event::PullFailed => "pull_failed",
            Event::PullRecovered => "pull_recovered",
        }
    }

    // About a project rather than one application: only `projects:` can
    // select it.
    pub fn is_project_event(self) -> bool {
        matches!(
            self,
            Event::CommitPulled | Event::PullFailed | Event::PullRecovered
        )
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpTarget {
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
    Header(HeaderAuth),
    // `auth:` left out: a bearer token when there is a credential, nothing
    // when there is none.
    #[default]
    #[serde(skip)]
    Default,
}

// `auth: {header: Name}`: the credential's token in a header of that name.
// Strict like every other block: `{header: X, value: y}` is a typo, not a
// setting to ignore.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeaderAuth {
    pub header: String,
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

// A job template (or workflow job template) to start, in Ascender or AWX,
// through the template's own webhook receiver in GitLab mode: its key in
// X-Gitlab-Token, no user needed, and any event accepted. The playbook reads
// the message as <prefix>_webhook_payload (ascender_, awx_ or tower_).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobTemplateTarget {
    // The controller's address, without /api.
    pub url: String,
    #[serde(default)]
    pub job_template: Option<u64>,
    #[serde(default)]
    pub workflow_job_template: Option<u64>,
    // One credential, whose `token` is the template's webhook key.
    #[serde(default)]
    pub credentials: Vec<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default = "default_retries")]
    pub retries: u32,
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

// The fields a body template may name: the keys of every message (an
// application's or a project's; a field a message does not have is null),
// plus a short revision. Checked at load, so a typo fails the push, not a
// send.
pub const TEMPLATE_FIELDS: [&str; 23] = [
    "event",
    "application",
    "project",
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
    "webhook",
    "previous_revision",
    "commits",
    "more_commits",
    "forced",
    "applications",
    "consecutive_failures",
    "failing_seconds",
];

const METHODS: [&str; 3] = ["POST", "PUT", "PATCH"];

// Longest a send may take: a receiver that never answers must not hold the
// sender (and every webhook queued behind it) for long.
const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_RETRIES: u32 = 10;

impl Webhook {
    // Everything wrong with this item on its own; references to applications
    // and credentials are checked by the loader, which knows them.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        let at = |what: String| format!("Webhook '{id}': {what}");
        if self.events.is_empty() {
            errors.push(at("needs at least one event".to_string()));
        }
        // A project event reaches only the projects a webhook names: with
        // none, it could never be sent.
        let project_events: Vec<&str> = self
            .events
            .iter()
            .filter(|e| e.is_project_event())
            .map(|e| e.as_str())
            .collect();
        if !project_events.is_empty() && self.projects.is_empty() {
            errors.push(at(format!(
                "{} {} about projects: name them in 'projects:'",
                project_events.join(", "),
                if project_events.len() == 1 {
                    "is"
                } else {
                    "are"
                }
            )));
        }
        let blocks = [
            ("webhook", self.webhook.is_some()),
            ("ascender", self.ascender.is_some()),
            ("awx", self.awx.is_some()),
        ];
        let wanted = self.webhook_type.as_str();
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

    // The credentials this webhook names, whichever its type.
    pub fn credentials(&self) -> &[String] {
        if let Some(webhook) = &self.webhook {
            return &webhook.credentials;
        }
        match (&self.ascender, &self.awx) {
            (Some(target), _) | (None, Some(target)) => &target.credentials,
            _ => &[],
        }
    }

    // Whether this webhook wants `event` from `application`, which reads
    // from `project`.
    pub fn wants(&self, event: Event, application: &str, project: &str) -> bool {
        !event.is_project_event()
            && self.events.contains(&event)
            && (self.applications.includes(application) || self.projects.includes(project))
    }

    // Whether this webhook wants `event` from `project` itself.
    pub fn wants_project(&self, event: Event, project: &str) -> bool {
        event.is_project_event() && self.events.contains(&event) && self.projects.includes(project)
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

impl HttpTarget {
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
            Auth::Scheme(AuthScheme::Bearer | AuthScheme::Basic) | Auth::Header(_) => true,
            Auth::Scheme(AuthScheme::None) | Auth::Default => false,
        };
        if needs_credential && self.credentials.is_empty() {
            errors.push("webhook.auth needs a credential in webhook.credentials".to_string());
        }
        if let Auth::Header(HeaderAuth { header }) = &self.auth
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
        // One header, one source: a fixed header with the name the auth or
        // the signature writes would be sent twice.
        let mut written: Vec<(&str, &str)> = Vec::new();
        match &self.auth {
            Auth::Scheme(AuthScheme::Bearer | AuthScheme::Basic) => {
                written.push(("Authorization", "webhook.auth"))
            }
            Auth::Default if !self.credentials.is_empty() => {
                written.push(("Authorization", "webhook.auth"))
            }
            Auth::Header(HeaderAuth { header }) => written.push((header, "webhook.auth")),
            _ => {}
        }
        if let Some(sign) = &self.sign {
            written.push((&sign.header, "webhook.sign"));
        }
        for (i, (name, by)) in written.iter().enumerate() {
            if self.headers.keys().any(|h| h.eq_ignore_ascii_case(name)) {
                errors.push(format!(
                    "webhook.headers sets '{name}', which {by} writes: set it in one place"
                ));
            }
            if written[..i]
                .iter()
                .any(|(other, _)| other.eq_ignore_ascii_case(name))
            {
                errors.push(format!(
                    "webhook.auth and webhook.sign both write '{name}': give them different headers"
                ));
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
                "url '{}…' is the controller's address, without /api",
                origin(&self.url)
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
            errors.push(
                "needs exactly one credential, whose token is the template's webhook key"
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

    // The template's webhook receiver, GitLab mode.
    pub fn endpoint(&self) -> String {
        let (collection, id) = match (self.job_template, self.workflow_job_template) {
            (Some(id), _) => ("job_templates", id),
            (None, Some(id)) => ("workflow_job_templates", id),
            (None, None) => ("job_templates", 0),
        };
        format!(
            "{}/api/v2/{collection}/{id}/gitlab/",
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
    template_parts(template).filter_map(|part| match part {
        TemplatePart::Field(name) => Some(name),
        TemplatePart::Text(_) => None,
    })
}

// A template, split once, the same way for checking it at load and for
// rendering it: text, and the {{field}} placeholders in it. A `{{` with no
// `}}` after it, or with nothing between them, is text.
pub enum TemplatePart<'a> {
    Text(&'a str),
    Field(&'a str),
}

pub fn template_parts(template: &str) -> impl Iterator<Item = TemplatePart<'_>> {
    let mut pieces = template.split("{{");
    let first = pieces.next().map(TemplatePart::Text);
    first.into_iter().chain(pieces.flat_map(|piece| {
        match piece.split_once("}}") {
            Some((name, after)) if !name.trim().is_empty() => [
                Some(TemplatePart::Field(name.trim())),
                Some(TemplatePart::Text(after)),
            ],
            // Not a placeholder: the `{{` is text, and so is what follows it.
            _ => [
                Some(TemplatePart::Text("{{")),
                Some(TemplatePart::Text(piece)),
            ],
        }
        .into_iter()
        .flatten()
    }))
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

    fn parse(yaml: &str) -> Webhook {
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
            serde_saphyr::from_str::<Webhook>(
                "type: webhook\nevents: [drifted]\nwebhook:\n  url: https://x.example.com\n"
            )
            .is_err()
        );
        assert!(serde_saphyr::from_str::<Webhook>(
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
            Auth::Header(HeaderAuth {
                header: "X-Gitlab-Token".into()
            })
        );
        // Strict: a key the header form does not have is an error.
        assert!(serde_saphyr::from_str::<Webhook>(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  credentials: [c]\n  auth: {header: X-Key, value: y}\n"
        )
        .is_err());
        // One header, one source.
        let twice = parse(
            "type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n  credentials: [c]\n  headers: {authorization: x}\n",
        );
        assert!(twice.errors("n").join("").contains("set it in one place"));
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
        assert_eq!(
            target.endpoint(),
            "https://ascender.example.com/api/v2/job_templates/7/gitlab/"
        );

        let workflow = parse(
            "type: awx\nevents: [drift_detected]\nawx:\n  url: https://awx.example.com/\n  workflow_job_template: 9\n  credentials: [k]\n",
        );
        assert_eq!(
            workflow.awx.unwrap().endpoint(),
            "https://awx.example.com/api/v2/workflow_job_templates/9/gitlab/"
        );
        // The launch API is not a way in: trigger and extra_vars are unknown keys.
        assert!(serde_saphyr::from_str::<Webhook>(
            "type: ascender\nevents: [drift_detected]\nascender:\n  url: https://a.example.com\n  job_template: 7\n  credentials: [k]\n  trigger: launch\n"
        )
        .is_err());

        let wrong = parse(
            "type: ascender\nevents: [drift_detected]\nascender:\n  url: https://ascender.example.com/api/v2\n  job_template: 7\n  workflow_job_template: 8\n",
        );
        let errors = wrong.errors("n").join("\n");
        assert!(errors.contains("without /api"), "{errors}");
        assert!(errors.contains("not both"), "{errors}");
        assert!(errors.contains("exactly one credential"), "{errors}");
    }

    #[test]
    fn wants_filters_on_event_and_application() {
        let n = parse(
            "type: webhook\nevents: [sync_failed]\napplications: [a]\nwebhook:\n  url: https://x.example.com\n",
        );
        assert!(n.wants(Event::SyncFailed, "a", "p"));
        assert!(!n.wants(Event::SyncFailed, "b", "p"));
        assert!(!n.wants(Event::DriftDetected, "a", "p"));
        let all = parse(
            "type: webhook\nevents: [sync_failed]\napplications: all\nwebhook:\n  url: https://x.example.com\n",
        );
        assert!(all.applications.is_all());
        assert!(all.wants(Event::SyncFailed, "anything", "p"));
        // Left out or empty: none, as an API key with no applications.
        let none =
            parse("type: webhook\nevents: [sync_failed]\nwebhook:\n  url: https://x.example.com\n");
        assert!(!none.wants(Event::SyncFailed, "anything", "p"));
        let empty = parse(
            "type: webhook\nevents: [sync_failed]\napplications: []\nwebhook:\n  url: https://x.example.com\n",
        );
        assert!(!empty.wants(Event::SyncFailed, "anything", "p"));
        assert!(serde_saphyr::from_str::<Webhook>(
            "type: webhook\nevents: [sync_failed]\napplications: every\nwebhook:\n  url: https://x.example.com\n"
        )
        .is_err());
    }

    #[test]
    fn projects_select_their_applications_and_their_own_events() {
        let n = parse(
            "type: webhook\nevents: [sync_failed, commit_pulled]\nprojects: [p]\napplications: [x]\nwebhook:\n  url: https://x.example.com\n",
        );
        assert!(n.errors("n").is_empty(), "{:?}", n.errors("n"));
        // Every application reading from p, and x wherever it reads from.
        assert!(n.wants(Event::SyncFailed, "any", "p"));
        assert!(n.wants(Event::SyncFailed, "x", "q"));
        assert!(!n.wants(Event::SyncFailed, "any", "q"));
        assert!(n.wants_project(Event::CommitPulled, "p"));
        assert!(!n.wants_project(Event::CommitPulled, "q"));
        // A project event is never an application's, nor the reverse.
        assert!(!n.wants(Event::CommitPulled, "x", "p"));
        assert!(!n.wants_project(Event::SyncFailed, "p"));
        let all = parse(
            "type: webhook\nevents: [pull_failed]\nprojects: all\nwebhook:\n  url: https://x.example.com\n",
        );
        assert!(all.wants_project(Event::PullFailed, "anything"));
    }

    #[test]
    fn a_project_event_needs_projects() {
        let n = parse(
            "type: webhook\nevents: [commit_pulled, pull_failed, sync_failed]\napplications: all\nwebhook:\n  url: https://x.example.com\n",
        );
        let errors = n.errors("n").join("\n");
        assert!(
            errors.contains("commit_pulled, pull_failed are about projects"),
            "{errors}"
        );
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
