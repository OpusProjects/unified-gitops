use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::domain::template::{self, Allowed, Condition};

// One HTTP request as an item writes it, shared by every block that sends
// one (a webhook's `webhook:`, an API's `api:`): where, how it
// authenticates and signs, which answers count, and how it travels (TLS,
// proxy, redirects, rate). What the request carries is the block's own.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HttpSpec {
    // A template: `$event.project` and the like are written into it.
    pub url: String,
    // Upper case; each block says its default.
    #[serde(default)]
    pub method: Option<String>,
    // Fixed headers, templated. Not secret: a secret belongs in a credential.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    // Query parameters added to the URL, templated and encoded.
    #[serde(default)]
    pub query: BTreeMap<String, String>,
    // Left out: a bearer token when the item has a credential with a
    // `token`, nothing otherwise.
    #[serde(default)]
    pub auth: Option<Auth>,
    #[serde(default)]
    pub signature: Option<Signature>,
    // The answers that count as success. Default: any 2xx.
    #[serde(default = "default_success")]
    pub success_status: Vec<StatusRange>,
    // The answers worth trying again (with Retry-After honoured). Default:
    // 429 and any 5xx. Anything else is final: a 4xx will not fix itself.
    #[serde(default = "default_retry_on")]
    pub retry_on_status: Vec<StatusRange>,
    // Off by default: a redirect would resend the body, credentials
    // included, to wherever the receiver points.
    #[serde(default)]
    pub follow_redirects: bool,
    #[serde(default)]
    pub tls: Option<Tls>,
    // A proxy URL (http://, https://, socks5://), or `none` to bypass the
    // one the environment sets. Left out: the environment's
    // (HTTPS_PROXY, NO_PROXY).
    #[serde(default)]
    pub proxy: Option<String>,
    // At most this many requests per window, across every send of the item.
    #[serde(default)]
    pub rate_limit: Option<RateLimit>,
}

// The keys of HttpSpec: a block holds these beside its own, and is split on
// them before either half is read (see `split`).
pub const HTTP_FIELDS: [&str; 12] = [
    "url",
    "method",
    "headers",
    "query",
    "auth",
    "signature",
    "success_status",
    "retry_on_status",
    "follow_redirects",
    "tls",
    "proxy",
    "rate_limit",
];

// A block's map, split into what HttpSpec reads and what the block reads
// itself; each half is then parsed strictly, so a key neither knows is
// still an error, named by the half that got it.
pub fn split<'de, D, Own>(deserializer: D) -> Result<(HttpSpec, Own), D::Error>
where
    D: serde::Deserializer<'de>,
    Own: serde::de::DeserializeOwned,
{
    use serde::de::Error;
    let mut own = Map::<String, Value>::deserialize(deserializer)?;
    let mut http = Map::new();
    for field in HTTP_FIELDS {
        if let Some(value) = own.remove(field) {
            http.insert(field.to_string(), value);
        }
    }
    let http = serde_json::from_value(Value::Object(http)).map_err(D::Error::custom)?;
    let own = serde_json::from_value(Value::Object(own)).map_err(D::Error::custom)?;
    Ok((http, own))
}

// How a request authenticates. One word (`none`, `bearer`, `basic`) or a
// mapping with a `type`.
#[derive(Debug, Clone, PartialEq)]
pub enum Auth {
    None,
    // Authorization: Bearer <token>
    Bearer,
    // Authorization: Basic base64(<username>:<password>)
    Basic,
    // The credential's `token` in a header of this name (X-API-Key).
    Header { name: String },
    // The credential's `token` as a query parameter of this name.
    Query { name: String },
    // OAuth 2.0 client credentials: the credential's client_id and
    // client_secret buy a token at token_url, kept until it expires.
    OAuth2(OAuth2),
    // A login call first: its answer holds a token, sent on the request.
    Login(Login),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuth2 {
    pub token_url: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub audience: Option<String>,
    // How the client authenticates at token_url: HTTP basic (RFC 6749's
    // recommendation) or client_id and client_secret in the form body.
    #[serde(default)]
    pub client_auth: ClientAuth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientAuth {
    #[default]
    Basic,
    Body,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Login {
    pub url: String,
    #[serde(default = "default_post")]
    pub method: String,
    // A template over $credential: {Username: $credential.username, ...}.
    #[serde(default)]
    pub body: Option<Value>,
    // Where the token is in the answer: `$response.jwt`, or a header's
    // value as `$headers.x-auth-token`.
    pub token: String,
    #[serde(default = "default_auth_header")]
    pub header: String,
    #[serde(default = "default_bearer_prefix")]
    pub prefix: String,
    // How long a token is used before logging in again; a 401 logs in again
    // sooner.
    #[serde(default = "default_login_ttl")]
    pub ttl_seconds: u64,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AuthForm {
    None,
    Bearer,
    Basic,
    Header { name: String },
    Query { name: String },
    Oauth2(OAuth2Form),
    Login(LoginForm),
}

// The variants' bodies, named apart so `type` is the only extra key.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OAuth2Form {
    token_url: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    audience: Option<String>,
    #[serde(default)]
    client_auth: ClientAuth,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginForm {
    url: String,
    #[serde(default = "default_post")]
    method: String,
    #[serde(default)]
    body: Option<Value>,
    token: String,
    #[serde(default = "default_auth_header")]
    header: String,
    #[serde(default = "default_bearer_prefix")]
    prefix: String,
    #[serde(default = "default_login_ttl")]
    ttl_seconds: u64,
}

impl<'de> Deserialize<'de> for Auth {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let value = Value::deserialize(deserializer)?;
        let value = match value {
            // The one-word form is the mapping with only its type.
            Value::String(word) => serde_json::json!({ "type": word }),
            other => other,
        };
        // serde lets a variant without fields ignore extra keys; a key here
        // is still a typo, not a setting.
        if let Value::Object(map) = &value
            && matches!(
                map.get("type").and_then(Value::as_str),
                Some("none" | "bearer" | "basic")
            )
            && let Some(extra) = map.keys().find(|k| *k != "type")
        {
            return Err(D::Error::custom(format!(
                "unknown field `{extra}`: auth {} takes no settings",
                map["type"].as_str().unwrap_or_default()
            )));
        }
        let form: AuthForm = serde_json::from_value(value).map_err(D::Error::custom)?;
        Ok(match form {
            AuthForm::None => Auth::None,
            AuthForm::Bearer => Auth::Bearer,
            AuthForm::Basic => Auth::Basic,
            AuthForm::Header { name } => Auth::Header { name },
            AuthForm::Query { name } => Auth::Query { name },
            AuthForm::Oauth2(f) => Auth::OAuth2(OAuth2 {
                token_url: f.token_url,
                scopes: f.scopes,
                audience: f.audience,
                client_auth: f.client_auth,
            }),
            AuthForm::Login(f) => Auth::Login(Login {
                url: f.url,
                method: f.method,
                body: f.body,
                token: f.token,
                header: f.header,
                prefix: f.prefix,
                ttl_seconds: f.ttl_seconds,
            }),
        })
    }
}

impl Auth {
    pub fn name(&self) -> &'static str {
        match self {
            Auth::None => "none",
            Auth::Bearer => "bearer",
            Auth::Basic => "basic",
            Auth::Header { .. } => "header",
            Auth::Query { .. } => "query",
            Auth::OAuth2(_) => "oauth2",
            Auth::Login(_) => "login",
        }
    }

    // The header this auth writes, if it writes one.
    pub fn header(&self) -> Option<&str> {
        match self {
            Auth::Bearer | Auth::Basic | Auth::OAuth2(_) => Some("Authorization"),
            Auth::Header { name } => Some(name),
            Auth::Login(login) => Some(&login.header),
            Auth::None | Auth::Query { .. } => None,
        }
    }
}

// An HMAC of what is sent, for receivers that check signatures (GitHub,
// Stripe, Svix, most webhook receivers): the digest, encoded, after a
// prefix, in a header. With `timestamp_header`, the signed text is
// `<unix seconds>.<body>` and the seconds go in that header, so a receiver
// can refuse a replay.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signature {
    // The credential holding the secret; left out, the item's.
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default = "default_secret_field")]
    pub field: String,
    #[serde(default)]
    pub algorithm: Algorithm,
    #[serde(default = "default_signature_header")]
    pub header: String,
    // Left out: `sha256=` or `sha512=`, after the algorithm.
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub encoding: Encoding,
    #[serde(default)]
    pub timestamp_header: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Algorithm {
    #[default]
    Sha256,
    Sha512,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Encoding {
    #[default]
    Hex,
    Base64,
}

impl Signature {
    pub fn prefix(&self) -> &str {
        match (&self.prefix, self.algorithm) {
            (Some(prefix), _) => prefix,
            (None, Algorithm::Sha256) => "sha256=",
            (None, Algorithm::Sha512) => "sha512=",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    // A PEM bundle of the CAs to trust, in place of the system's: an
    // internal CA.
    #[serde(default)]
    pub ca_file: Option<String>,
    // Mutual TLS: a credential whose `cert` and `key` fields hold the
    // client certificate and its key, in PEM.
    #[serde(default)]
    pub client_credential: Option<String>,
    // Accept any certificate. For a lab; refused together with ca_file.
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    pub requests: u32,
    pub per_seconds: u64,
}

// How a failed send is tried again: attempts in all, the first included,
// waiting initial_delay_seconds, then (exponential) twice that each time,
// never more than max_delay_seconds. A Retry-After answer wins, capped the
// same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retry {
    #[serde(default = "default_attempts")]
    pub attempts: u32,
    #[serde(default)]
    pub backoff: Backoff,
    #[serde(default = "default_initial_delay")]
    pub initial_delay_seconds: u64,
    #[serde(default = "default_max_delay")]
    pub max_delay_seconds: u64,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: default_attempts(),
            backoff: Backoff::default(),
            initial_delay_seconds: default_initial_delay(),
            max_delay_seconds: default_max_delay(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backoff {
    Fixed,
    #[default]
    Exponential,
}

impl Retry {
    // The wait before attempt `next` (2 is the first retry).
    pub fn delay(&self, next: u32) -> std::time::Duration {
        let seconds = match self.backoff {
            Backoff::Fixed => self.initial_delay_seconds,
            Backoff::Exponential => self
                .initial_delay_seconds
                .saturating_mul(1u64 << next.saturating_sub(2).min(20)),
        };
        std::time::Duration::from_secs(seconds.min(self.max_delay_seconds))
    }

    fn errors(&self, at: &str) -> Vec<String> {
        let mut errors = Vec::new();
        if self.attempts == 0 || self.attempts > MAX_ATTEMPTS {
            errors.push(format!(
                "{at}.attempts must be between 1 and {MAX_ATTEMPTS} (the first try included)"
            ));
        }
        if self.max_delay_seconds > MAX_DELAY_SECONDS {
            errors.push(format!(
                "{at}.max_delay_seconds must be at most {MAX_DELAY_SECONDS}"
            ));
        }
        if self.initial_delay_seconds > self.max_delay_seconds {
            errors.push(format!(
                "{at}.initial_delay_seconds is above max_delay_seconds"
            ));
        }
        errors
    }
}

// Following what a request started (an AWX job, a pipeline, a deployment):
// after it succeeds, `url` is asked every interval until `until` holds
// (success) or `fail_if` does (failure), or timeout_seconds pass. The URL
// may name $response (what the request answered); the conditions read
// $poll (what the URL answers now).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wait {
    pub url: String,
    #[serde(default = "default_get")]
    pub method: String,
    #[serde(deserialize_with = "condition")]
    pub until: Condition,
    #[serde(default, deserialize_with = "optional_condition")]
    pub fail_if: Option<Condition>,
    #[serde(default = "default_poll_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_poll_timeout")]
    pub timeout_seconds: u64,
}

fn condition<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Condition, D::Error> {
    use serde::de::Error;
    Condition::parse(&String::deserialize(d)?).map_err(D::Error::custom)
}

fn optional_condition<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Condition>, D::Error> {
    condition(d).map(Some)
}

// A status, or a range of them: 204, "200-299", "2xx".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusRange {
    pub from: u16,
    pub to: u16,
}

impl StatusRange {
    pub fn contains(&self, status: u16) -> bool {
        (self.from..=self.to).contains(&status)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let number = |t: &str| {
            t.trim()
                .parse::<u16>()
                .ok()
                .filter(|n| (100..=599).contains(n))
                .ok_or_else(|| format!("'{text}' is not an HTTP status (100-599)"))
        };
        if let Some(class) = text.strip_suffix("xx").or_else(|| text.strip_suffix("XX")) {
            let digit = number(&format!("{class}00"))?;
            return Ok(Self {
                from: digit,
                to: digit + 99,
            });
        }
        match text.split_once('-') {
            Some((a, b)) => {
                let (from, to) = (number(a)?, number(b)?);
                if from > to {
                    return Err(format!("'{text}' ends before it starts"));
                }
                Ok(Self { from, to })
            }
            None => {
                let n = number(text)?;
                Ok(Self { from: n, to: n })
            }
        }
    }
}

impl<'de> Deserialize<'de> for StatusRange {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match Value::deserialize(deserializer)? {
            Value::Number(n) => StatusRange::parse(&n.to_string()),
            Value::String(s) => StatusRange::parse(&s),
            other => Err(format!("'{other}' is not an HTTP status")),
        }
        .map_err(D::Error::custom)
    }
}

pub fn in_ranges(ranges: &[StatusRange], status: u16) -> bool {
    ranges.iter().any(|r| r.contains(status))
}

fn default_success() -> Vec<StatusRange> {
    vec![StatusRange { from: 200, to: 299 }]
}

fn default_retry_on() -> Vec<StatusRange> {
    vec![
        StatusRange { from: 429, to: 429 },
        StatusRange { from: 500, to: 599 },
    ]
}

fn default_post() -> String {
    "POST".to_string()
}

fn default_get() -> String {
    "GET".to_string()
}

fn default_auth_header() -> String {
    "Authorization".to_string()
}

fn default_bearer_prefix() -> String {
    "Bearer ".to_string()
}

fn default_login_ttl() -> u64 {
    300
}

fn default_secret_field() -> String {
    "secret".to_string()
}

fn default_signature_header() -> String {
    "X-Hub-Signature-256".to_string()
}

fn default_attempts() -> u32 {
    4
}

fn default_initial_delay() -> u64 {
    1
}

fn default_max_delay() -> u64 {
    60
}

fn default_poll_interval() -> u64 {
    10
}

fn default_poll_timeout() -> u64 {
    3600
}

pub const METHODS: [&str; 6] = ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD"];
const MAX_ATTEMPTS: u32 = 11;
const MAX_DELAY_SECONDS: u64 = 3600;
// Longest one request may take: a receiver that never answers must not hold
// a send (and what queues behind it) for long.
pub const MAX_TIMEOUT_SECONDS: u64 = 300;
const MAX_POLL_SECONDS: u64 = 24 * 3600;

// What the templates of one request may name, by setting.
pub struct Contexts<'a> {
    // url, headers, query and the block's own body.
    pub request: Allowed<'a>,
}

impl HttpSpec {
    pub fn method_or<'a>(&'a self, default: &'a str) -> &'a str {
        self.method.as_deref().unwrap_or(default)
    }

    // The credentials this request reads besides the item's own.
    pub fn extra_credentials(&self) -> Vec<&String> {
        let mut ids = Vec::new();
        if let Some(id) = self.signature.as_ref().and_then(|s| s.credential.as_ref()) {
            ids.push(id);
        }
        if let Some(id) = self.tls.as_ref().and_then(|t| t.client_credential.as_ref()) {
            ids.push(id);
        }
        ids
    }

    // Everything wrong with this request on its own. `at` names the block
    // (`webhook`, `api`), `has_credential` whether the item names one.
    pub fn errors(&self, at: &str, has_credential: bool, contexts: &Contexts<'_>) -> Vec<String> {
        let mut errors = Vec::new();
        errors.extend(url_errors(&format!("{at}.url"), &self.url));
        errors.extend(template::string_errors(
            &format!("{at}.url"),
            &self.url,
            &contexts.request,
        ));
        if let Some(method) = &self.method
            && !METHODS.contains(&method.as_str())
        {
            errors.push(format!(
                "{at}.method '{method}' must be one of {} (upper case)",
                METHODS.join(", ")
            ));
        }
        for (name, value) in &self.headers {
            if !is_header_name(name) {
                errors.push(format!("{at}.headers '{name}' is not a header name"));
            }
            errors.extend(template::string_errors(
                &format!("{at}.headers.{name}"),
                value,
                &contexts.request,
            ));
        }
        for (name, value) in &self.query {
            if name.is_empty() {
                errors.push(format!("{at}.query has an empty parameter name"));
            }
            errors.extend(template::string_errors(
                &format!("{at}.query.{name}"),
                value,
                &contexts.request,
            ));
        }
        errors.extend(self.auth_errors(at, has_credential));
        if let Some(sign) = &self.signature {
            for (key, header) in [
                ("header", Some(&sign.header)),
                ("timestamp_header", sign.timestamp_header.as_ref()),
            ] {
                if let Some(header) = header
                    && !is_header_name(header)
                {
                    errors.push(format!(
                        "{at}.signature.{key} '{header}' is not a header name"
                    ));
                }
            }
            if sign.credential.is_none() && !has_credential {
                errors.push(format!(
                    "{at}.signature needs a credential holding its secret: the item's \
                     `credential`, or signature.credential"
                ));
            }
        }
        // One header, one source: a fixed header with the name the auth or
        // the signature writes would be sent twice.
        let mut written: Vec<(&str, &str)> = Vec::new();
        if let Some(header) = self.auth.as_ref().and_then(Auth::header) {
            written.push((header, "auth"));
        } else if self.auth.is_none() && has_credential {
            written.push(("Authorization", "auth (a bearer token, by default)"));
        }
        if let Some(sign) = &self.signature {
            written.push((&sign.header, "signature"));
            if let Some(ts) = &sign.timestamp_header {
                written.push((ts, "signature.timestamp_header"));
            }
        }
        for (i, (name, by)) in written.iter().enumerate() {
            if self.headers.keys().any(|h| h.eq_ignore_ascii_case(name)) {
                errors.push(format!(
                    "{at}.headers sets '{name}', which {by} writes: set it in one place"
                ));
            }
            if let Some((_, other)) = written[..i]
                .iter()
                .find(|(other, _)| other.eq_ignore_ascii_case(name))
            {
                errors.push(format!(
                    "{at}: {other} and {by} both write '{name}': give them different headers"
                ));
            }
        }
        if self.success_status.is_empty() {
            errors.push(format!("{at}.success_status names no status"));
        }
        if let Some(tls) = &self.tls
            && tls.insecure_skip_verify
            && tls.ca_file.is_some()
        {
            errors.push(format!(
                "{at}.tls sets ca_file and insecure_skip_verify: trust the CA, or nothing"
            ));
        }
        if let Some(proxy) = &self.proxy
            && proxy != "none"
            && !["http://", "https://", "socks5://", "socks5h://"]
                .iter()
                .any(|s| proxy.starts_with(s))
        {
            errors.push(format!(
                "{at}.proxy must be an http://, https:// or socks5:// URL, or none"
            ));
        }
        if let Some(limit) = &self.rate_limit
            && (limit.requests == 0 || limit.per_seconds == 0)
        {
            errors.push(format!(
                "{at}.rate_limit needs at least one request per at least one second"
            ));
        }
        errors
    }

    fn auth_errors(&self, at: &str, has_credential: bool) -> Vec<String> {
        let mut errors = Vec::new();
        let Some(auth) = &self.auth else {
            return errors;
        };
        let needs_credential = !matches!(auth, Auth::None);
        if needs_credential && !has_credential {
            errors.push(format!(
                "{at}.auth {} needs the item's `credential`",
                auth.name()
            ));
        }
        match auth {
            Auth::Header { name } if !is_header_name(name) => {
                errors.push(format!("{at}.auth header '{name}' is not a header name"))
            }
            Auth::Query { name } if name.is_empty() => {
                errors.push(format!("{at}.auth query needs a parameter name"))
            }
            Auth::OAuth2(oauth) => errors.extend(url_errors(
                &format!("{at}.auth.token_url"),
                &oauth.token_url,
            )),
            Auth::Login(login) => {
                errors.extend(url_errors(&format!("{at}.auth.url"), &login.url));
                if !METHODS.contains(&login.method.as_str()) {
                    errors.push(format!(
                        "{at}.auth.method '{}' must be one of {}",
                        login.method,
                        METHODS.join(", ")
                    ));
                }
                if !is_header_name(&login.header) {
                    errors.push(format!(
                        "{at}.auth.header '{}' is not a header name",
                        login.header
                    ));
                }
                if let Some(body) = &login.body {
                    errors.extend(template::errors(
                        &format!("{at}.auth.body"),
                        body,
                        &Allowed {
                            roots: &["credential"],
                            fields: &[],
                        },
                    ));
                }
                let token_vars = template::string_vars(&login.token);
                if token_vars.is_empty() {
                    errors.push(format!(
                        "{at}.auth.token '{}' must name where the token is: $response.<field> \
                         or $headers.<name>",
                        login.token
                    ));
                }
                errors.extend(template::string_errors(
                    &format!("{at}.auth.token"),
                    &login.token,
                    &Allowed {
                        roots: &["response", "headers"],
                        fields: &[],
                    },
                ));
                if login.ttl_seconds == 0 {
                    errors.push(format!("{at}.auth.ttl_seconds must be at least 1"));
                }
            }
            _ => {}
        }
        errors
    }
}

impl Wait {
    pub fn errors(&self, at: &str, event_fields: &[&str]) -> Vec<String> {
        let mut errors = Vec::new();
        let url_context = Allowed {
            roots: &["event", "response"],
            fields: &[("event", event_fields)],
        };
        if !(self.url.starts_with("https://")
            || self.url.starts_with("http://")
            || self.url.starts_with('$'))
        {
            errors.push(format!(
                "{at}.wait.url must be an http:// or https:// address, or start with $response \
                 (a URL the request answered)"
            ));
        }
        errors.extend(template::string_errors(
            &format!("{at}.wait.url"),
            &self.url,
            &url_context,
        ));
        if !METHODS.contains(&self.method.as_str()) {
            errors.push(format!(
                "{at}.wait.method '{}' must be one of {}",
                self.method,
                METHODS.join(", ")
            ));
        }
        for (key, condition) in [
            ("until", Some(&self.until)),
            ("fail_if", self.fail_if.as_ref()),
        ] {
            if let Some(c) = condition
                && c.path[0] != "poll"
            {
                errors.push(format!(
                    "{at}.wait.{key} reads ${}: a condition reads $poll, what the URL answers",
                    c.path.join(".")
                ));
            }
        }
        if self.interval_seconds == 0 {
            errors.push(format!("{at}.wait.interval_seconds must be at least 1"));
        }
        if self.timeout_seconds == 0 || self.timeout_seconds > MAX_POLL_SECONDS {
            errors.push(format!(
                "{at}.wait.timeout_seconds must be between 1 and {MAX_POLL_SECONDS}"
            ));
        }
        errors
    }
}

// The item-level settings every outbound item shares.
pub fn timeout_errors(timeout_seconds: u64) -> Vec<String> {
    if timeout_seconds == 0 || timeout_seconds > MAX_TIMEOUT_SECONDS {
        vec![format!(
            "timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}"
        )]
    } else {
        Vec::new()
    }
}

pub fn retry_errors(retry: &Retry) -> Vec<String> {
    retry.errors("retry")
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

pub fn is_header_name(name: &str) -> bool {
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

    fn spec(yaml: &str) -> HttpSpec {
        serde_saphyr::from_str(yaml).unwrap()
    }

    const NOTHING: Allowed<'static> = Allowed {
        roots: &["event"],
        fields: &[],
    };

    fn errors(yaml: &str, has_credential: bool) -> String {
        spec(yaml)
            .errors(
                "webhook",
                has_credential,
                &Contexts {
                    request: Allowed {
                        roots: &["event"],
                        fields: &[("event", &["project"])],
                    },
                },
            )
            .join("\n")
    }

    #[test]
    fn auth_takes_a_word_or_a_typed_mapping() {
        assert_eq!(
            spec("url: https://x\nauth: basic\n").auth,
            Some(Auth::Basic)
        );
        assert_eq!(
            spec("url: https://x\nauth: {type: header, name: X-API-Key}\n").auth,
            Some(Auth::Header {
                name: "X-API-Key".into()
            })
        );
        let oauth = spec(
            "url: https://x\nauth:\n  type: oauth2\n  token_url: https://idp/token\n  scopes: [a, b]\n",
        );
        assert!(matches!(oauth.auth, Some(Auth::OAuth2(ref o)) if o.scopes == ["a", "b"]));
        let login = spec(
            "url: https://x\nauth:\n  type: login\n  url: https://x/api/auth\n  body: {Username: $credential.username}\n  token: $response.jwt\n",
        );
        assert!(matches!(login.auth, Some(Auth::Login(ref l)) if l.prefix == "Bearer "));
        for bad in [
            "url: https://x\nauth: digest\n",
            "url: https://x\nauth: {type: header}\n",
            "url: https://x\nauth: {type: bearer, value: y}\n",
        ] {
            assert!(serde_saphyr::from_str::<HttpSpec>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn statuses_take_numbers_ranges_and_classes() {
        let s = spec("url: https://x\nsuccess_status: [204, \"200-202\", 3xx]\n");
        assert!(in_ranges(&s.success_status, 204));
        assert!(in_ranges(&s.success_status, 201));
        assert!(in_ranges(&s.success_status, 302));
        assert!(!in_ranges(&s.success_status, 203));
        assert!(in_ranges(&s.retry_on_status, 429) && in_ranges(&s.retry_on_status, 503));
        assert!(
            serde_saphyr::from_str::<HttpSpec>("url: https://x\nsuccess_status: [700]\n").is_err()
        );
        assert!(
            serde_saphyr::from_str::<HttpSpec>("url: https://x\nsuccess_status: [\"300-200\"]\n")
                .is_err()
        );
    }

    #[test]
    fn what_cannot_work_is_named_at_load() {
        let e = errors(
            "url: ftp://x\nmethod: post\nheaders: {Authorization: x, bad header: y, X-P: $event.prjoect}\nauth: bearer\n",
            true,
        );
        for expected in [
            "webhook.url 'ftp://x' must be an http",
            "webhook.method 'post'",
            "'bad header' is not a header name",
            "$event.prjoect",
            "webhook.headers sets 'Authorization', which auth writes",
        ] {
            assert!(e.contains(expected), "missing {expected:?} in {e}");
        }
        let e = errors("url: https://x\nauth: basic\nsignature: {}\n", false);
        assert!(
            e.contains("auth basic needs the item's `credential`"),
            "{e}"
        );
        assert!(e.contains("signature needs a credential"), "{e}");
        let e = errors(
            "url: https://x\ntls: {ca_file: /ca.pem, insecure_skip_verify: true}\nproxy: ftp://p\nrate_limit: {requests: 0, per_seconds: 1}\n",
            false,
        );
        assert!(e.contains("trust the CA, or nothing"), "{e}");
        assert!(e.contains("proxy must be"), "{e}");
        assert!(e.contains("rate_limit needs"), "{e}");
        let e = errors(
            "url: https://x\nauth:\n  type: login\n  url: https://x/login\n  body: {u: $event.project}\n  token: jwt\n",
            true,
        );
        assert!(e.contains("auth.body names $event.project"), "{e}");
        assert!(e.contains("must name where the token is"), "{e}");
        assert_eq!(errors("url: https://x/$event.project\n", false), "");
        let _ = NOTHING.roots;
    }

    #[test]
    fn a_default_bearer_and_a_signature_may_not_share_a_header() {
        let e = errors("url: https://x\nsignature: {header: Authorization}\n", true);
        assert!(e.contains("both write 'Authorization'"), "{e}");
    }

    #[test]
    fn retry_delays_grow_and_cap() {
        let r = Retry {
            attempts: 5,
            backoff: Backoff::Exponential,
            initial_delay_seconds: 2,
            max_delay_seconds: 10,
        };
        let waits: Vec<u64> = (2..=5).map(|n| r.delay(n).as_secs()).collect();
        assert_eq!(waits, [2, 4, 8, 10]);
        let fixed = Retry {
            backoff: Backoff::Fixed,
            ..r
        };
        assert_eq!(fixed.delay(5).as_secs(), 2);
        assert!(!Retry { attempts: 0, ..r }.errors("retry").is_empty());
        assert!(
            !Retry {
                initial_delay_seconds: 20,
                ..r
            }
            .errors("retry")
            .is_empty()
        );
    }

    #[test]
    fn a_wait_reads_poll_and_names_its_url() {
        let w: Wait = serde_saphyr::from_str(
            "url: $response.url\nuntil: $poll.status == successful\nfail_if: $poll.status in [failed, error]\n",
        )
        .unwrap();
        assert!(
            w.errors("webhook", &["project"]).is_empty(),
            "{:?}",
            w.errors("webhook", &[])
        );
        let w: Wait = serde_saphyr::from_str(
            "url: ftp://x/$event.nope\nuntil: $response.status == ok\ninterval_seconds: 0\n",
        )
        .unwrap();
        let e = w.errors("webhook", &["project"]).join("\n");
        assert!(e.contains("wait.url must be"), "{e}");
        assert!(e.contains("$event.nope"), "{e}");
        assert!(e.contains("a condition reads $poll"), "{e}");
        assert!(e.contains("interval_seconds"), "{e}");
        assert!(serde_saphyr::from_str::<Wait>("url: https://x\nuntil: status\n").is_err());
    }

    #[test]
    fn split_hands_each_half_its_keys() {
        #[derive(Deserialize, Debug)]
        #[serde(deny_unknown_fields)]
        struct Own {
            body: Option<Value>,
        }
        let (http, own): (HttpSpec, Own) =
            split(serde_json::json!({"url": "https://x", "body": {"a": 1}})).unwrap();
        assert_eq!(http.url, "https://x");
        assert_eq!(own.body, Some(serde_json::json!({"a": 1})));
        let unknown: Result<(HttpSpec, Own), _> =
            split(serde_json::json!({"url": "https://x", "bdoy": 1}));
        assert!(unknown.unwrap_err().to_string().contains("bdoy"));
    }

    #[test]
    fn the_origin_never_shows_a_path_or_user_info() {
        assert_eq!(
            origin("https://user:pw@hooks.example.com/services/T0/B0/secret?x=1"),
            "https://hooks.example.com"
        );
    }
}
