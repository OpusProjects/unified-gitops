use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::http::{self, HttpSpec, Retry, Wait};
use crate::domain::template::{self, Allowed};
use crate::domain::trigger::PathFilter;

// A webhook: which events of which project, and the HTTP request that tells
// them. Generic: any receiver that takes an HTTP request, shaped by
// `webhook.body` into whatever that receiver expects.
//
//   webhooks:
//     - name: notify
//       project: infra
//       events: [commit_pulled, pull_failed]
//       trigger: {paths: [vcenter/**]}
//       credential: cred-hook
//       webhook:
//         url: https://hooks.example.com/x
//         body: {text: "$event.project: $event.event"}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Webhook {
    #[serde(default)]
    pub description: Option<String>,
    // Off: kept in the configuration, never sent.
    #[serde(default = "yes")]
    pub enabled: bool,
    // The watched project whose events it tells.
    pub project: String,
    // Free-form, for finding items in the UI and the API.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub events: Vec<Event>,
    // For commit_pulled: only commits touching these files are told.
    #[serde(default)]
    pub trigger: PathFilter,
    // Read per send: `auth` and `signature` say which fields go where.
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub retry: Retry,
    pub webhook: WebhookRequest,
}

// The request: the shared HTTP settings plus what a webhook sends.
#[derive(Debug, Clone, PartialEq)]
pub struct WebhookRequest {
    pub http: HttpSpec,
    pub content_type: ContentType,
    // A template over $event; left out, the event message as it is.
    pub body: Option<Value>,
    pub wait: Option<Wait>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebhookOwn {
    #[serde(default)]
    content_type: ContentType,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    wait: Option<Wait>,
}

impl<'de> Deserialize<'de> for WebhookRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (http, own): (HttpSpec, WebhookOwn) = http::split(deserializer)?;
        Ok(Self {
            http,
            content_type: own.content_type,
            body: own.body,
            wait: own.wait,
        })
    }
}

// How the body is written. json: as JSON. form: a mapping, URL-encoded
// (application/x-www-form-urlencoded), nested values as JSON. text: the
// body's text as it is (text/plain).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    #[default]
    Json,
    Form,
    Text,
}

impl ContentType {
    pub fn mime(self) -> &'static str {
        match self {
            ContentType::Json => "application/json",
            ContentType::Form => "application/x-www-form-urlencoded",
            ContentType::Text => "text/plain; charset=utf-8",
        }
    }
}

// What happened to a project's checkout, as a webhook names it (one pull
// produces at most one, application::webhooks::detect_pull).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Event {
    // A pull brought new commits into a project's checkout.
    CommitPulled,
    // A project's repository could not be pulled: the checkout stays on the
    // commit it has.
    PullFailed,
    // Pulls work again after failing.
    PullRecovered,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::CommitPulled => "commit_pulled",
            Event::PullFailed => "pull_failed",
            Event::PullRecovered => "pull_recovered",
        }
    }
}

fn yes() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    30
}

// The fields of an event message, which `$event.<field>` may name: every
// event has the envelope, and each adds its own (null where it has none).
// Checked at load, so a typo fails the push, not a send.
pub const EVENT_FIELDS: [&str; 16] = [
    "event",
    "project",
    "description",
    "revision",
    "revision_short",
    "errors",
    "at",
    "url",
    "webhook",
    "previous_revision",
    "commits",
    "more_commits",
    "forced",
    "files",
    "consecutive_failures",
    "failing_seconds",
];

const EVENT_CONTEXT: Allowed<'static> = Allowed {
    roots: &["event"],
    fields: &[("event", &EVENT_FIELDS)],
};

impl Webhook {
    // Everything wrong with this item on its own; the project and the
    // credentials it names are checked by the loader, which knows them.
    pub fn errors(&self, id: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.events.is_empty() {
            errors.push("needs at least one event".to_string());
        }
        if !self.trigger.is_everything() && !self.events.contains(&Event::CommitPulled) {
            errors.push(
                "trigger filters the files of a commit: it needs the commit_pulled event"
                    .to_string(),
            );
        }
        errors.extend(self.trigger.errors("trigger"));
        let request = &self.webhook;
        errors.extend(request.http.errors(
            "webhook",
            self.credential.is_some(),
            &http::Contexts {
                request: EVENT_CONTEXT,
            },
        ));
        if let Some(body) = &request.body {
            errors.extend(template::errors("webhook.body", body, &EVENT_CONTEXT));
            match (request.content_type, body) {
                (ContentType::Form, Value::Object(_)) => {}
                (ContentType::Form, _) => errors.push(
                    "webhook.body must be a mapping with content_type form: its keys are the \
                     form's fields"
                        .to_string(),
                ),
                (ContentType::Text, Value::String(_)) => {}
                (ContentType::Text, _) => {
                    errors.push("webhook.body must be text with content_type text".to_string())
                }
                (ContentType::Json, _) => {}
            }
        }
        if request.http.method_or("POST") == "GET" && request.body.is_some() {
            errors.push("webhook.body is set but webhook.method GET sends none".to_string());
        }
        if let Some(wait) = &request.wait {
            errors.extend(wait.errors("webhook", &EVENT_FIELDS));
        }
        errors.extend(http::timeout_errors(self.timeout_seconds));
        errors.extend(http::retry_errors(&self.retry));
        errors
            .into_iter()
            .map(|e| format!("Webhook '{id}': {e}"))
            .collect()
    }

    // Every credential it reads: its own, the signature's, the TLS client
    // certificate's.
    pub fn credentials(&self) -> Vec<&String> {
        self.credential
            .iter()
            .chain(self.webhook.http.extra_credentials())
            .collect()
    }

    // Whether this webhook tells `event` of `project`. A commit is told only
    // when it touched a file the trigger selects; `files` None (git could not
    // list them) is told, rather than silently dropped.
    pub fn wants(&self, event: Event, project: &str, files: Option<&[String]>) -> bool {
        self.enabled
            && self.project == project
            && self.events.contains(&event)
            && (event != Event::CommitPulled
                || self.trigger.is_everything()
                || files.is_none_or(|f| !self.trigger.select(f).is_empty()))
    }

    // Where it sends, without anything a URL can carry that is secret: a
    // chat webhook's path or query is often the credential itself.
    pub fn destination(&self) -> String {
        http::origin(&self.webhook.http.url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Webhook {
        serde_saphyr::from_str(yaml).unwrap()
    }

    const BASE: &str = "project: p\nevents: [commit_pulled, pull_failed]\n";

    #[test]
    fn a_webhook_needs_only_a_project_events_and_a_url() {
        let n = parse(&format!("{BASE}webhook:\n  url: https://x.example.com/h\n"));
        assert!(n.errors("n").is_empty(), "{:?}", n.errors("n"));
        assert!(n.enabled);
        assert_eq!(n.webhook.http.method_or("POST"), "POST");
        assert_eq!(n.webhook.content_type, ContentType::Json);
        assert_eq!(n.retry.attempts, 4);
        assert_eq!(n.destination(), "https://x.example.com");
    }

    #[test]
    fn every_option_parses() {
        let n = parse(&format!(
            "{BASE}description: All of it\nenabled: false\nlabels: {{team: ops}}\ntrigger:\n  paths: [vcenter/**]\n  ignore_paths: ['**/*.md']\ncredential: cred-hook\ntimeout_seconds: 10\nretry: {{attempts: 2, backoff: fixed, initial_delay_seconds: 5, max_delay_seconds: 5}}\nwebhook:\n  url: https://jobs.example.com/api/v2/job_templates/42/launch/\n  method: POST\n  headers: {{X-Team: ops}}\n  query: {{dry: 'false'}}\n  auth: bearer\n  signature: {{header: X-Signature, algorithm: sha512, encoding: base64, timestamp_header: X-Timestamp}}\n  success_status: [201]\n  retry_on_status: [503]\n  follow_redirects: true\n  tls: {{ca_file: /etc/ca.pem, client_credential: cred-mtls}}\n  proxy: http://proxy:3128\n  rate_limit: {{requests: 5, per_seconds: 1}}\n  content_type: json\n  body:\n    limit: vcenter\n    extra_vars: {{gitops: $event, site: madrid}}\n  wait:\n    url: https://jobs.example.com$response.url\n    until: $poll.status == successful\n    fail_if: $poll.status in [failed, error, canceled]\n    interval_seconds: 5\n    timeout_seconds: 600\n"
        ));
        assert!(n.errors("n").is_empty(), "{:?}", n.errors("n"));
        assert!(!n.enabled);
        assert_eq!(n.credentials(), ["cred-hook", "cred-mtls"]);
    }

    #[test]
    fn templates_and_shapes_are_checked() {
        let n = parse(&format!(
            "{BASE}webhook:\n  url: https://x.example.com\n  body: {{a: $event.projcet, b: $file}}\n"
        ));
        let errors = n.errors("n").join("\n");
        assert!(errors.contains("$event.projcet"), "{errors}");
        assert!(errors.contains("$file"), "{errors}");
        let n = parse(&format!(
            "{BASE}webhook:\n  url: https://x.example.com\n  content_type: form\n  body: [a]\n"
        ));
        assert!(n.errors("n").join("").contains("must be a mapping"));
        let n = parse(&format!(
            "{BASE}webhook:\n  url: https://x.example.com\n  content_type: text\n  body: {{a: 1}}\n"
        ));
        assert!(n.errors("n").join("").contains("must be text"));
        let n = parse(&format!(
            "{BASE}webhook:\n  url: https://x.example.com\n  method: GET\n  body: x\n"
        ));
        assert!(n.errors("n").join("").contains("GET sends none"));
    }

    #[test]
    fn unknown_keys_fail_in_either_half_of_the_block() {
        for yaml in [
            format!("{BASE}webhook:\n  url: https://x\n  bdoy: x\n"),
            format!("{BASE}webhook:\n  url: https://x\n  tls: {{cafile: x}}\n"),
            format!("{BASE}type: webhook\nwebhook:\n  url: https://x\n"),
            "project: p\nevents: [drift_detected]\nwebhook:\n  url: https://x\n".to_string(),
        ] {
            assert!(serde_saphyr::from_str::<Webhook>(&yaml).is_err(), "{yaml}");
        }
    }

    #[test]
    fn wants_filters_on_project_event_and_files() {
        let n = parse(&format!(
            "{BASE}trigger: {{paths: [users/**]}}\nwebhook:\n  url: https://x\n"
        ));
        let touched =
            |files: &[&str]| -> Vec<String> { files.iter().map(|f| f.to_string()).collect() };
        assert!(n.wants(Event::CommitPulled, "p", Some(&touched(&["users/a.json"]))));
        assert!(!n.wants(Event::CommitPulled, "p", Some(&touched(&["roles/r.json"]))));
        assert!(n.wants(Event::CommitPulled, "p", None));
        assert!(!n.wants(Event::CommitPulled, "q", None));
        assert!(n.wants(Event::PullFailed, "p", None));
        assert!(!n.wants(Event::PullRecovered, "p", None));
        let off = parse(&format!(
            "{BASE}enabled: false\nwebhook:\n  url: https://x\n"
        ));
        assert!(!off.wants(Event::PullFailed, "p", None));
        let no_commits = parse(
            "project: p\nevents: [pull_failed]\ntrigger: {paths: [a]}\nwebhook:\n  url: https://x\n",
        );
        assert!(
            no_commits
                .errors("n")
                .join("")
                .contains("needs the commit_pulled event")
        );
    }
}
