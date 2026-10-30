use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use dashmap::DashMap;
use serde_json::{Map, Value, json};

use crate::domain::http::{
    Algorithm, Auth, ClientAuth, Encoding, HttpSpec, Login, OAuth2, Retry, Signature, Wait,
    in_ranges,
};
use crate::domain::template::Scope;
use crate::ports::http_sender::{
    HttpSenderPort, OutboundRequest, OutboundResponse, Proxy, SendError, Transport,
};
use crate::ports::secrets::SecretsPort;

// The use case "send one configured HTTP request": every outbound item (a
// webhook, an API call) describes its request with an HttpSpec, and this is
// the one place that turns it into bytes on the wire: the URL and its query,
// the headers, the credential where `auth` says, the signature, the
// transport; then the answer, read against success_status and
// retry_on_status, tried again as `retry` says, and followed with `wait`.

// The credentials one request reads, resolved for this send: the item's, and
// any other a signature or a client certificate names.
#[derive(Debug, Default, Clone)]
pub struct Credentials {
    pub item: Option<(String, HashMap<String, String>)>,
    pub others: HashMap<String, HashMap<String, String>>,
}

impl Credentials {
    pub async fn resolve(
        secrets: &dyn SecretsPort,
        item: Option<&String>,
        spec: &HttpSpec,
    ) -> Result<Self, String> {
        async fn get(
            secrets: &dyn SecretsPort,
            id: &String,
        ) -> Result<HashMap<String, String>, String> {
            secrets
                .resolve(id)
                .await
                .map_err(|e| format!("credential '{id}': {}", e.message))
        }
        let mut out = Credentials::default();
        if let Some(id) = item {
            out.item = Some((id.clone(), get(secrets, id).await?));
        }
        for id in spec.extra_credentials() {
            if !out.others.contains_key(id) && out.item.as_ref().is_none_or(|(i, _)| i != id) {
                out.others.insert(id.clone(), get(secrets, id).await?);
            }
        }
        Ok(out)
    }

    fn of(&self, id: Option<&String>) -> Result<(&str, &HashMap<String, String>), String> {
        match id {
            Some(id) => match &self.item {
                Some((item_id, fields)) if item_id == id => Ok((item_id, fields)),
                _ => self
                    .others
                    .get_key_value(id)
                    .map(|(id, f)| (id.as_str(), f))
                    .ok_or_else(|| format!("credential '{id}' was not resolved")),
            },
            None => self
                .item
                .as_ref()
                .map(|(id, f)| (id.as_str(), f))
                .ok_or_else(|| "no credential".to_string()),
        }
    }

    // A field of the item's credential, present and not empty.
    fn required(&self, field: &str) -> Result<&str, String> {
        field_of(self.of(None)?, field)
    }

    fn optional(&self, field: &str) -> Option<&str> {
        self.item
            .as_ref()
            .and_then(|(_, f)| f.get(field))
            .map(String::as_str)
    }

    // The item credential as $credential, for a login body.
    fn as_value(&self) -> Value {
        self.item
            .as_ref()
            .map(|(_, f)| json!(f))
            .unwrap_or(Value::Null)
    }
}

fn field_of<'a>(
    (id, fields): (&str, &'a HashMap<String, String>),
    field: &str,
) -> Result<&'a str, String> {
    fields
        .get(field)
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("credential '{id}' has no '{field}' field"))
}

// What a send is about: the item it belongs to (tokens and rate limits are
// kept per item) and how long one request may take.
pub struct Call<'a> {
    pub item: &'a str,
    pub spec: &'a HttpSpec,
    pub credentials: &'a Credentials,
    pub timeout: Duration,
    pub retry: Retry,
}

// What the request carries: the method, and the body with its media type.
pub struct Payload {
    pub method: String,
    pub body: Option<(Vec<u8>, String)>,
    // Headers this use case adds (an event name), before the spec's own.
    pub headers: Vec<(String, String)>,
}

// How a send ended, for the item's history.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub ok: bool,
    pub attempts: u32,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub response: Option<OutboundResponse>,
}

// Tokens bought with oauth2 or a login, and the recent sends each item's
// rate_limit counts: per process, shared by every send. In memory: a restart
// logs in again.
#[derive(Default)]
pub struct Engine {
    tokens: DashMap<String, (String, Instant)>,
    windows: DashMap<String, Arc<tokio::sync::Mutex<VecDeque<Instant>>>>,
}

// Never wait longer than this between attempts, whatever Retry-After says.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

impl Engine {
    pub fn new() -> Self {
        Self::default()
    }

    // Sends the request, tries again as `retry` says, and returns how it
    // ended. A request that cannot be built (a missing credential field, a
    // value HTTP cannot carry) is the configuration's to fix: not retried.
    pub async fn send(
        &self,
        http: &dyn HttpSenderPort,
        call: &Call<'_>,
        scope: &Scope,
        payload: &Payload,
    ) -> Outcome {
        let mut outcome = Outcome::default();
        let mut logged_in_again = false;
        loop {
            outcome.attempts += 1;
            self.pace(call).await;
            let request = match self.build(http, call, scope, payload).await {
                Ok(request) => request,
                Err(e) => {
                    outcome.error = Some(e);
                    return outcome;
                }
            };
            let result = http.send(request).await;
            let (again, wait) = match result {
                Err(SendError { message, retryable }) => {
                    outcome.status = None;
                    outcome.error = Some(message);
                    (retryable, None)
                }
                Ok(response) => {
                    outcome.status = Some(response.status);
                    if in_ranges(&call.spec.success_status, response.status) {
                        outcome.ok = true;
                        outcome.error = None;
                        outcome.response = Some(response);
                        return outcome;
                    }
                    outcome.error = Some(describe(&response));
                    // A bought token the receiver no longer takes: buy
                    // another and try at once, once.
                    if response.status == 401 && self.forget_token(call) && !logged_in_again {
                        logged_in_again = true;
                        outcome.response = Some(response);
                        continue;
                    }
                    let again = in_ranges(&call.spec.retry_on_status, response.status);
                    let wait = retry_after(&response);
                    outcome.response = Some(response);
                    (again, wait)
                }
            };
            if !again || outcome.attempts >= call.retry.attempts {
                return outcome;
            }
            let backoff = call.retry.delay(outcome.attempts + 1);
            let max = Duration::from_secs(call.retry.max_delay_seconds).min(MAX_RETRY_AFTER);
            tokio::time::sleep(wait.map_or(backoff, |w| w.min(max).max(backoff))).await;
        }
    }

    // After a request succeeded: ask wait.url until wait.until holds,
    // wait.fail_if does, or the time is up. Errors of one poll (the job's
    // URL briefly unreachable) are tried again until then.
    pub async fn wait(
        &self,
        http: &dyn HttpSenderPort,
        call: &Call<'_>,
        scope: &Scope,
        wait: &Wait,
        response: &OutboundResponse,
    ) -> Result<(), String> {
        let mut scope = scope.clone();
        scope.set("response", parse_body(response));
        let url = scope.render_text(&wait.url);
        let deadline = Instant::now() + Duration::from_secs(wait.timeout_seconds);
        let payload = Payload {
            method: wait.method.clone(),
            body: None,
            headers: Vec::new(),
        };
        let polled = HttpSpec {
            url: url.clone(),
            query: Default::default(),
            signature: None,
            ..call.spec.clone()
        };
        let poll_call = Call {
            spec: &polled,
            retry: Retry {
                attempts: 1,
                ..call.retry
            },
            ..*call
        };
        let mut last;
        loop {
            let outcome = self.send(http, &poll_call, &scope, &payload).await;
            match (&outcome.response, outcome.ok) {
                (Some(answer), true) => {
                    let mut poll_scope = scope.clone();
                    poll_scope.set("poll", parse_body(answer));
                    if let Some(fail) = &wait.fail_if
                        && fail.holds(&poll_scope)
                    {
                        return Err(format!(
                            "wait: ${} is '{}'",
                            fail.path.join("."),
                            crate::domain::template::as_text(
                                &poll_scope.lookup(
                                    &fail.path.iter().map(String::as_str).collect::<Vec<_>>()
                                )
                            )
                        ));
                    }
                    if wait.until.holds(&poll_scope) {
                        return Ok(());
                    }
                    last = format!(
                        "${} is '{}'",
                        wait.until.path.join("."),
                        crate::domain::template::as_text(
                            &poll_scope.lookup(
                                &wait
                                    .until
                                    .path
                                    .iter()
                                    .map(String::as_str)
                                    .collect::<Vec<_>>()
                            )
                        )
                    );
                }
                _ => last = outcome.error.unwrap_or_default(),
            }
            if Instant::now() + Duration::from_secs(wait.interval_seconds) > deadline {
                return Err(format!(
                    "wait: gave up after {}s ({last})",
                    wait.timeout_seconds
                ));
            }
            tokio::time::sleep(Duration::from_secs(wait.interval_seconds)).await;
        }
    }

    // Blocks until the item's rate_limit has room for one more request.
    async fn pace(&self, call: &Call<'_>) {
        let Some(limit) = call.spec.rate_limit else {
            return;
        };
        let window = Duration::from_secs(limit.per_seconds);
        let sent = Arc::clone(&self.windows.entry(call.item.to_string()).or_default());
        loop {
            let mut sent = sent.lock().await;
            let now = Instant::now();
            while sent
                .front()
                .is_some_and(|t| now.duration_since(*t) >= window)
            {
                sent.pop_front();
            }
            if sent.len() < limit.requests as usize {
                sent.push_back(now);
                return;
            }
            let free_at = sent[0] + window;
            drop(sent);
            tokio::time::sleep(free_at.saturating_duration_since(now)).await;
        }
    }

    // True when the item's auth keeps a token, which is now forgotten.
    fn forget_token(&self, call: &Call<'_>) -> bool {
        matches!(call.spec.auth, Some(Auth::OAuth2(_) | Auth::Login(_)))
            && self.tokens.remove(call.item).is_some()
    }

    async fn token(
        &self,
        http: &dyn HttpSenderPort,
        call: &Call<'_>,
        transport: &Transport,
    ) -> Result<String, String> {
        if let Some(entry) = self.tokens.get(call.item)
            && entry.1 > Instant::now()
        {
            return Ok(entry.0.clone());
        }
        let (token, lifetime) = match &call.spec.auth {
            Some(Auth::OAuth2(oauth)) => oauth2_token(http, call, oauth, transport).await?,
            Some(Auth::Login(login)) => login_token(http, call, login, transport).await?,
            _ => return Err("this auth keeps no token".to_string()),
        };
        self.tokens.insert(
            call.item.to_string(),
            (token.clone(), Instant::now() + lifetime),
        );
        Ok(token)
    }

    // The request, built whole. Public for the "what would be sent" views
    // and the tests; a send goes through `send`.
    pub async fn build(
        &self,
        http: &dyn HttpSenderPort,
        call: &Call<'_>,
        scope: &Scope,
        payload: &Payload,
    ) -> Result<OutboundRequest, String> {
        let spec = call.spec;
        let transport = transport(spec, call.credentials)?;
        let mut url =
            url::Url::parse(&scope.render_text(&spec.url)).map_err(|e| format!("url: {e}"))?;
        if !spec.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in &spec.query {
                pairs.append_pair(name, &scope.render_text(value));
            }
        }
        let mut headers = vec![(
            "User-Agent".to_string(),
            format!("unified-gitops/{}", env!("CARGO_PKG_VERSION")),
        )];
        if let Some((_, mime)) = &payload.body {
            set_header(&mut headers, "Content-Type", mime.clone());
        }
        for (name, value) in &payload.headers {
            set_header(&mut headers, name, value.clone());
        }
        for (name, value) in &spec.headers {
            set_header(&mut headers, name, scope.render_text(value));
        }
        let credentials = call.credentials;
        match &spec.auth {
            None => {
                if let Some(token) = credentials.optional("token").filter(|t| !t.is_empty()) {
                    set_header(&mut headers, "Authorization", format!("Bearer {token}"));
                }
            }
            Some(Auth::None) => {}
            Some(Auth::Bearer) => set_header(
                &mut headers,
                "Authorization",
                format!("Bearer {}", credentials.required("token")?),
            ),
            Some(Auth::Basic) => {
                // An empty password is allowed: some APIs take a key as the
                // username and nothing after the colon.
                let password = credentials.optional("password").unwrap_or("");
                let pair = format!("{}:{password}", credentials.required("username")?);
                set_header(
                    &mut headers,
                    "Authorization",
                    format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(pair)
                    ),
                );
            }
            Some(Auth::Header { name }) => set_header(
                &mut headers,
                name,
                credentials.required("token")?.to_string(),
            ),
            Some(Auth::Query { name }) => {
                let token = credentials.required("token")?;
                url.query_pairs_mut().append_pair(name, token);
            }
            Some(Auth::OAuth2(_)) => {
                let token = self.token(http, call, &transport).await?;
                set_header(&mut headers, "Authorization", format!("Bearer {token}"));
            }
            Some(Auth::Login(login)) => {
                let token = self.token(http, call, &transport).await?;
                set_header(
                    &mut headers,
                    &login.header,
                    format!("{}{token}", login.prefix),
                );
            }
        }
        let body = payload
            .body
            .as_ref()
            .map(|(b, _)| b.clone())
            .unwrap_or_default();
        if let Some(signature) = &spec.signature {
            sign(&mut headers, signature, credentials, &body)?;
        }
        check_headers(&headers)?;
        Ok(OutboundRequest {
            method: payload.method.clone(),
            url: url.to_string(),
            headers,
            body,
            timeout: call.timeout,
            transport,
        })
    }
}

fn transport(spec: &HttpSpec, credentials: &Credentials) -> Result<Transport, String> {
    let mut transport = Transport {
        follow_redirects: spec.follow_redirects,
        proxy: match spec.proxy.as_deref() {
            None => Proxy::Environment,
            Some("none") => Proxy::None,
            Some(url) => Proxy::Url(url.to_string()),
        },
        ..Transport::default()
    };
    if let Some(tls) = &spec.tls {
        transport.insecure_skip_verify = tls.insecure_skip_verify;
        if let Some(path) = &tls.ca_file {
            transport.ca_pem =
                Some(std::fs::read(path).map_err(|e| format!("tls.ca_file '{path}': {e}"))?);
        }
        if let Some(id) = &tls.client_credential {
            let fields = credentials.of(Some(id))?;
            let identity = format!(
                "{}\n{}\n",
                field_of(fields, "cert")?.trim_end(),
                field_of(fields, "key")?.trim_end()
            );
            transport.identity_pem = Some(identity.into_bytes());
        }
    }
    Ok(transport)
}

async fn oauth2_token(
    http: &dyn HttpSenderPort,
    call: &Call<'_>,
    oauth: &OAuth2,
    transport: &Transport,
) -> Result<(String, Duration), String> {
    let id = call.credentials.required("client_id")?;
    let secret = call.credentials.required("client_secret")?;
    let mut form: Vec<(&str, &str)> = vec![("grant_type", "client_credentials")];
    let scope_list = oauth.scopes.join(" ");
    if !oauth.scopes.is_empty() {
        form.push(("scope", &scope_list));
    }
    if let Some(audience) = &oauth.audience {
        form.push(("audience", audience));
    }
    let mut headers = vec![(
        "Content-Type".to_string(),
        "application/x-www-form-urlencoded".to_string(),
    )];
    match oauth.client_auth {
        ClientAuth::Basic => headers.push((
            "Authorization".to_string(),
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{id}:{secret}"))
            ),
        )),
        ClientAuth::Body => {
            form.push(("client_id", id));
            form.push(("client_secret", secret));
        }
    }
    // Encoded before the await: the serializer is not Send.
    let form = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let answer = token_request(
        http,
        OutboundRequest {
            method: "POST".into(),
            url: oauth.token_url.clone(),
            headers,
            body: form.into_bytes(),
            timeout: call.timeout,
            transport: transport.clone(),
        },
        "auth.token_url",
    )
    .await?;
    let parsed = parse_body(&answer);
    let token = parsed["access_token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .ok_or("auth.token_url answered no access_token")?
        .to_string();
    // Renewed a little before it runs out, so a send never carries a token
    // that expires on the way.
    let lifetime = parsed["expires_in"]
        .as_u64()
        .unwrap_or(300)
        .saturating_sub(30);
    Ok((token, Duration::from_secs(lifetime.max(1))))
}

async fn login_token(
    http: &dyn HttpSenderPort,
    call: &Call<'_>,
    login: &Login,
    transport: &Transport,
) -> Result<(String, Duration), String> {
    let scope = Scope::new().with("credential", call.credentials.as_value());
    let (body, headers) = match &login.body {
        Some(body) => (
            serde_json::to_vec(&scope.render(body)).map_err(|e| e.to_string())?,
            vec![("Content-Type".to_string(), "application/json".to_string())],
        ),
        None => (Vec::new(), Vec::new()),
    };
    let answer = token_request(
        http,
        OutboundRequest {
            method: login.method.clone(),
            url: login.url.clone(),
            headers,
            body,
            timeout: call.timeout,
            transport: transport.clone(),
        },
        "auth.url",
    )
    .await?;
    let headers: Map<String, Value> = answer
        .headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), json!(v)))
        .collect();
    let token = Scope::new()
        .with("response", parse_body(&answer))
        .with("headers", Value::Object(headers))
        .render_text(&login.token);
    if token.is_empty() {
        return Err(format!(
            "auth.url answered, but {} is empty in its answer",
            login.token
        ));
    }
    Ok((token, Duration::from_secs(login.ttl_seconds)))
}

async fn token_request(
    http: &dyn HttpSenderPort,
    request: OutboundRequest,
    what: &str,
) -> Result<OutboundResponse, String> {
    let answer = http
        .send(request)
        .await
        .map_err(|e| format!("{what}: {}", e.message))?;
    if !(200..300).contains(&answer.status) {
        return Err(format!("{what}: {}", describe(&answer)));
    }
    Ok(answer)
}

fn sign(
    headers: &mut Vec<(String, String)>,
    signature: &Signature,
    credentials: &Credentials,
    body: &[u8],
) -> Result<(), String> {
    let secret = field_of(
        credentials.of(signature.credential.as_ref())?,
        &signature.field,
    )?;
    let mut signed = Vec::new();
    if let Some(header) = &signature.timestamp_header {
        let now = crate::domain::unix_now().to_string();
        signed.extend_from_slice(now.as_bytes());
        signed.push(b'.');
        set_header(headers, header, now);
    }
    signed.extend_from_slice(body);
    let digest = hmac(signature.algorithm, secret.as_bytes(), &signed);
    let encoded = match signature.encoding {
        Encoding::Hex => hex(&digest),
        Encoding::Base64 => base64::engine::general_purpose::STANDARD.encode(&digest),
    };
    set_header(
        headers,
        &signature.header,
        format!("{}{encoded}", signature.prefix()),
    );
    Ok(())
}

// HMAC (RFC 2104) over sha2, already in the tree for config ETags.
pub fn hmac(algorithm: Algorithm, key: &[u8], message: &[u8]) -> Vec<u8> {
    use sha2::{Digest, Sha256, Sha512};
    fn mac<D: Digest>(block: usize, key: &[u8], message: &[u8]) -> Vec<u8> {
        let mut padded = vec![0u8; block];
        if key.len() > block {
            let digest = D::digest(key);
            padded[..digest.len()].copy_from_slice(&digest);
        } else {
            padded[..key.len()].copy_from_slice(key);
        }
        let inner_key: Vec<u8> = padded.iter().map(|b| b ^ 0x36).collect();
        let outer_key: Vec<u8> = padded.iter().map(|b| b ^ 0x5c).collect();
        let inner = D::new()
            .chain_update(&inner_key)
            .chain_update(message)
            .finalize();
        D::new()
            .chain_update(&outer_key)
            .chain_update(inner)
            .finalize()
            .to_vec()
    }
    match algorithm {
        Algorithm::Sha256 => mac::<Sha256>(64, key, message),
        Algorithm::Sha512 => mac::<Sha512>(128, key, message),
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Sets a header, replacing any of the same name (headers are not case
// sensitive): one header, one value.
fn set_header(headers: &mut Vec<(String, String)>, name: &str, value: String) {
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    headers.push((name.to_string(), value));
}

// A header value HTTP can carry: no line breaks or other control characters,
// which a secret read from a file often ends with. Refused here, before the
// request: it is the configuration's to fix, and retrying would not.
fn check_headers(headers: &[(String, String)]) -> Result<(), String> {
    for (name, value) in headers {
        if value.chars().any(|c| c.is_control() && c != '\t') {
            return Err(format!(
                "the value for header '{name}' has a line break or a control character \
                 (a credential read from a file with a trailing newline?)"
            ));
        }
    }
    Ok(())
}

// An answer that did not count, with the start of what the receiver said.
pub fn describe(response: &OutboundResponse) -> String {
    let excerpt: String = response.body.chars().take(300).collect();
    format!("HTTP {}: {}", response.status, excerpt.trim())
}

// Retry-After in seconds (the HTTP-date form is rare from APIs, and read as
// absent).
fn retry_after(response: &OutboundResponse) -> Option<Duration> {
    response
        .header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

// An answer's body as JSON; text that is not JSON is a string, nothing is
// null.
pub fn parse_body(response: &OutboundResponse) -> Value {
    if response.body.trim().is_empty() {
        return Value::Null;
    }
    serde_json::from_str(&response.body).unwrap_or_else(|_| Value::String(response.body.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::http_sender::SendFuture;
    use std::sync::Mutex;

    // Answers in order, the last one forever; records what it was sent.
    struct Script {
        answers: Mutex<VecDeque<Result<OutboundResponse, SendError>>>,
        sent: Mutex<Vec<OutboundRequest>>,
    }

    impl Script {
        fn new(answers: Vec<Result<OutboundResponse, SendError>>) -> Self {
            Self {
                answers: Mutex::new(answers.into()),
                sent: Mutex::new(Vec::new()),
            }
        }
        fn sent(&self) -> Vec<OutboundRequest> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl HttpSenderPort for Script {
        fn send(&self, request: OutboundRequest) -> SendFuture<'_> {
            self.sent.lock().unwrap().push(request);
            let mut answers = self.answers.lock().unwrap();
            let answer = if answers.len() > 1 {
                answers.pop_front().unwrap()
            } else {
                answers.front().cloned().unwrap()
            };
            Box::pin(async move { answer })
        }
    }

    fn ok(status: u16, body: &str) -> Result<OutboundResponse, SendError> {
        Ok(OutboundResponse {
            status,
            body: body.into(),
            headers: vec![],
        })
    }

    fn spec(yaml: &str) -> HttpSpec {
        serde_saphyr::from_str(yaml).unwrap()
    }

    fn creds(fields: &[(&str, &str)]) -> Credentials {
        Credentials {
            item: Some((
                "cred".into(),
                fields
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            )),
            others: HashMap::new(),
        }
    }

    fn call<'a>(spec: &'a HttpSpec, credentials: &'a Credentials, attempts: u32) -> Call<'a> {
        Call {
            item: "item",
            spec,
            credentials,
            timeout: Duration::from_secs(5),
            retry: Retry {
                attempts,
                initial_delay_seconds: 0,
                ..Retry::default()
            },
        }
    }

    fn post(body: &str) -> Payload {
        Payload {
            method: "POST".into(),
            body: Some((body.as_bytes().to_vec(), "application/json".into())),
            headers: vec![("X-Event".into(), "e".into())],
        }
    }

    fn header<'a>(request: &'a OutboundRequest, name: &str) -> Option<&'a str> {
        request
            .headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    #[tokio::test]
    async fn every_auth_puts_the_credential_where_it_says() {
        let engine = Engine::new();
        let http = Script::new(vec![ok(200, "")]);
        let scope = Scope::new().with("event", json!({"project": "infra"}));
        let c = creds(&[("token", "t0k"), ("username", "u"), ("password", "p")]);
        for (yaml, name, value) in [
            (
                "url: https://x/$event.project\n",
                "Authorization",
                "Bearer t0k",
            ),
            (
                "url: https://x\nauth: bearer\n",
                "Authorization",
                "Bearer t0k",
            ),
            (
                "url: https://x\nauth: basic\n",
                "Authorization",
                "Basic dTpw",
            ),
            (
                "url: https://x\nauth: {type: header, name: X-API-Key}\n",
                "X-API-Key",
                "t0k",
            ),
        ] {
            let s = spec(yaml);
            let r = engine
                .build(&http, &call(&s, &c, 1), &scope, &post("{}"))
                .await
                .unwrap();
            assert_eq!(header(&r, name), Some(value), "{yaml}");
            assert_eq!(header(&r, "X-Event"), Some("e"));
            assert_eq!(header(&r, "Content-Type"), Some("application/json"));
        }
        let s = spec(
            "url: https://x/$event.project\nquery: {a: 'b c', p: $event.project}\nauth: {type: query, name: key}\nheaders: {X-P: $event.project}\n",
        );
        let r = engine
            .build(&http, &call(&s, &c, 1), &scope, &post("{}"))
            .await
            .unwrap();
        assert_eq!(r.url, "https://x/infra?a=b+c&p=infra&key=t0k");
        assert_eq!(header(&r, "X-P"), Some("infra"));
        let s = spec("url: https://x\nauth: none\n");
        let r = engine
            .build(&http, &call(&s, &c, 1), &scope, &post("{}"))
            .await
            .unwrap();
        assert_eq!(header(&r, "Authorization"), None);
        let s = spec("url: https://x\nauth: bearer\n");
        let none = creds(&[("token", "a\n")]);
        let e = engine
            .build(&http, &call(&s, &none, 1), &scope, &post("{}"))
            .await
            .unwrap_err();
        assert!(e.contains("line break"), "{e}");
        let e = engine
            .build(&http, &call(&s, &creds(&[]), 1), &scope, &post("{}"))
            .await
            .unwrap_err();
        assert!(e.contains("credential 'cred' has no 'token' field"), "{e}");
    }

    #[tokio::test]
    async fn a_signature_is_the_hmac_of_what_is_sent() {
        let engine = Engine::new();
        let http = Script::new(vec![ok(200, "")]);
        let c = creds(&[("secret", "key")]);
        let s = spec("url: https://x\nauth: none\nsignature: {}\n");
        let r = engine
            .build(&http, &call(&s, &c, 1), &Scope::new(), &post("{\"a\":1}"))
            .await
            .unwrap();
        let expected = hex(&hmac(Algorithm::Sha256, b"key", b"{\"a\":1}"));
        assert_eq!(
            header(&r, "X-Hub-Signature-256"),
            Some(format!("sha256={expected}").as_str())
        );
        let s = spec(
            "url: https://x\nauth: none\nsignature: {algorithm: sha512, encoding: base64, header: X-Sig, timestamp_header: X-Ts}\n",
        );
        let r = engine
            .build(&http, &call(&s, &c, 1), &Scope::new(), &post("b"))
            .await
            .unwrap();
        let ts = header(&r, "X-Ts").unwrap();
        let expected = base64::engine::general_purpose::STANDARD.encode(hmac(
            Algorithm::Sha512,
            b"key",
            format!("{ts}.b").as_bytes(),
        ));
        assert_eq!(
            header(&r, "X-Sig"),
            Some(format!("sha512={expected}").as_str())
        );
        // RFC 4231 test case 2.
        assert_eq!(
            hex(&hmac(
                Algorithm::Sha256,
                b"Jefe",
                b"what do ya want for nothing?"
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[tokio::test]
    async fn retries_follow_the_statuses_and_stop_at_attempts() {
        let engine = Engine::new();
        let s = spec("url: https://x\n");
        let c = Credentials::default();
        let http = Script::new(vec![ok(503, "busy"), ok(429, ""), ok(201, "{}")]);
        let o = engine
            .send(&http, &call(&s, &c, 4), &Scope::new(), &post(""))
            .await;
        assert!(o.ok && o.attempts == 3 && o.status == Some(201), "{o:?}");

        let http = Script::new(vec![ok(404, "no such hook")]);
        let o = engine
            .send(&http, &call(&s, &c, 4), &Scope::new(), &post(""))
            .await;
        assert_eq!((o.ok, o.attempts), (false, 1));
        assert_eq!(o.error.as_deref(), Some("HTTP 404: no such hook"));

        let http = Script::new(vec![Err(SendError {
            message: "could not connect".into(),
            retryable: true,
        })]);
        let o = engine
            .send(&http, &call(&s, &c, 2), &Scope::new(), &post(""))
            .await;
        assert_eq!((o.ok, o.attempts, o.status), (false, 2, None));

        let http = Script::new(vec![Err(SendError {
            message: "bad url".into(),
            retryable: false,
        })]);
        let o = engine
            .send(&http, &call(&s, &c, 4), &Scope::new(), &post(""))
            .await;
        assert_eq!(o.attempts, 1);

        let picky = spec("url: https://x\nsuccess_status: [202]\nretry_on_status: [200]\n");
        let http = Script::new(vec![ok(200, ""), ok(202, "")]);
        let o = engine
            .send(&http, &call(&picky, &c, 3), &Scope::new(), &post(""))
            .await;
        assert!(o.ok && o.attempts == 2);
    }

    #[tokio::test]
    async fn oauth2_buys_a_token_keeps_it_and_buys_again_after_a_401() {
        let engine = Engine::new();
        let s = spec(
            "url: https://api/x\nauth: {type: oauth2, token_url: https://idp/token, scopes: [read, write], audience: api}\n",
        );
        let c = creds(&[("client_id", "id"), ("client_secret", "sec")]);
        let http = Script::new(vec![
            ok(200, r#"{"access_token": "A", "expires_in": 3600}"#),
            ok(200, ""),
            ok(200, ""),
            ok(401, "expired"),
            ok(200, r#"{"access_token": "B"}"#),
            ok(200, ""),
        ]);
        let cl = call(&s, &c, 1);
        assert!(engine.send(&http, &cl, &Scope::new(), &post("")).await.ok);
        assert!(engine.send(&http, &cl, &Scope::new(), &post("")).await.ok);
        let o = engine.send(&http, &cl, &Scope::new(), &post("")).await;
        assert!(o.ok, "{o:?}");
        let sent = http.sent();
        assert_eq!(sent.len(), 6);
        assert_eq!(sent[0].url, "https://idp/token");
        let form = String::from_utf8(sent[0].body.clone()).unwrap();
        assert_eq!(
            form,
            "grant_type=client_credentials&scope=read+write&audience=api"
        );
        assert_eq!(header(&sent[0], "Authorization"), Some("Basic aWQ6c2Vj"));
        assert_eq!(header(&sent[1], "Authorization"), Some("Bearer A"));
        assert_eq!(header(&sent[2], "Authorization"), Some("Bearer A"));
        assert_eq!(header(&sent[5], "Authorization"), Some("Bearer B"));

        let body_auth = spec(
            "url: https://api/x\nauth: {type: oauth2, token_url: https://idp/token, client_auth: body}\n",
        );
        let http = Script::new(vec![ok(500, "down")]);
        let e = Engine::new()
            .build(&http, &call(&body_auth, &c, 1), &Scope::new(), &post(""))
            .await
            .unwrap_err();
        assert!(e.contains("auth.token_url: HTTP 500"), "{e}");
        assert!(
            String::from_utf8(http.sent()[0].body.clone())
                .unwrap()
                .contains("client_secret=sec")
        );
    }

    #[tokio::test]
    async fn a_login_reads_its_token_from_the_answer() {
        let engine = Engine::new();
        let s = spec(
            "url: https://p/api/users\nauth:\n  type: login\n  url: https://p/api/auth\n  body: {Username: $credential.username, Password: $credential.password}\n  token: $response.jwt\n",
        );
        let c = creds(&[("username", "admin"), ("password", "pw")]);
        let http = Script::new(vec![ok(200, r#"{"jwt": "J"}"#), ok(200, "")]);
        assert!(
            engine
                .send(&http, &call(&s, &c, 1), &Scope::new(), &post(""))
                .await
                .ok
        );
        let sent = http.sent();
        let login: Value = serde_json::from_slice(&sent[0].body).unwrap();
        assert_eq!(login, json!({"Username": "admin", "Password": "pw"}));
        assert_eq!(header(&sent[1], "Authorization"), Some("Bearer J"));

        let by_header = spec(
            "url: https://p/x\nauth:\n  type: login\n  url: https://p/login\n  token: $headers.x-auth-token\n  header: X-Auth-Token\n  prefix: ''\n",
        );
        let http = Script::new(vec![
            Ok(OutboundResponse {
                status: 200,
                body: String::new(),
                headers: vec![("X-Auth-Token".into(), "H".into())],
            }),
            ok(200, ""),
        ]);
        let other = Engine::new();
        let r = other
            .build(&http, &call(&by_header, &c, 1), &Scope::new(), &post(""))
            .await
            .unwrap();
        assert_eq!(header(&r, "X-Auth-Token"), Some("H"));

        let http = Script::new(vec![ok(200, "{}")]);
        let e = Engine::new()
            .build(&http, &call(&s, &c, 1), &Scope::new(), &post(""))
            .await
            .unwrap_err();
        assert!(e.contains("$response.jwt is empty"), "{e}");
    }

    #[tokio::test]
    async fn a_wait_polls_until_its_condition() {
        let engine = Engine::new();
        let s = spec("url: https://jobs/launch\n");
        let c = Credentials::default();
        let w: Wait = serde_saphyr::from_str(
            "url: https://jobs$response.url\nuntil: $poll.status == successful\nfail_if: $poll.status in [failed]\ninterval_seconds: 1\ntimeout_seconds: 5\n",
        )
        .unwrap();
        let launched = OutboundResponse {
            status: 201,
            body: r#"{"url": "/jobs/7/"}"#.into(),
            headers: vec![],
        };
        let http = Script::new(vec![
            ok(200, r#"{"status": "running"}"#),
            ok(200, r#"{"status": "successful"}"#),
        ]);
        let cl = call(&s, &c, 1);
        engine
            .wait(&http, &cl, &Scope::new(), &w, &launched)
            .await
            .unwrap();
        assert_eq!(http.sent()[0].url, "https://jobs/jobs/7/");
        assert_eq!(http.sent()[0].method, "GET");

        let http = Script::new(vec![ok(200, r#"{"status": "failed"}"#)]);
        let e = engine
            .wait(&http, &cl, &Scope::new(), &w, &launched)
            .await
            .unwrap_err();
        assert_eq!(e, "wait: $poll.status is 'failed'");

        let quick: Wait = serde_saphyr::from_str(
            "url: https://jobs/x\nuntil: $poll.status == successful\ninterval_seconds: 1\ntimeout_seconds: 1\n",
        )
        .unwrap();
        let http = Script::new(vec![ok(200, r#"{"status": "running"}"#)]);
        let e = engine
            .wait(&http, &cl, &Scope::new(), &quick, &launched)
            .await
            .unwrap_err();
        assert!(
            e.contains("gave up after 1s ($poll.status is 'running')"),
            "{e}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rate_limit_spaces_the_sends() {
        let engine = Engine::new();
        let s = spec("url: https://x\nrate_limit: {requests: 2, per_seconds: 10}\n");
        let c = Credentials::default();
        let http = Script::new(vec![ok(200, "")]);
        let started = tokio::time::Instant::now();
        for _ in 0..3 {
            engine
                .send(&http, &call(&s, &c, 1), &Scope::new(), &post(""))
                .await;
        }
        assert!(started.elapsed() >= Duration::from_secs(10));
    }

    #[tokio::test]
    async fn the_transport_carries_tls_and_proxy() {
        let engine = Engine::new();
        let http = Script::new(vec![ok(200, "")]);
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, "PEM").unwrap();
        let s = spec(&format!(
            "url: https://x\nproxy: none\nfollow_redirects: true\ntls: {{ca_file: {}, client_credential: cred-m}}\n",
            ca.display()
        ));
        let mut c = Credentials::default();
        c.others.insert(
            "cred-m".into(),
            HashMap::from([("cert".into(), "C\n".into()), ("key".into(), "K".into())]),
        );
        let r = engine
            .build(&http, &call(&s, &c, 1), &Scope::new(), &post(""))
            .await
            .unwrap();
        assert_eq!(r.transport.proxy, Proxy::None);
        assert!(r.transport.follow_redirects);
        assert_eq!(r.transport.ca_pem.as_deref(), Some(&b"PEM"[..]));
        assert_eq!(r.transport.identity_pem.as_deref(), Some(&b"C\nK\n"[..]));
        let s = spec("url: https://x\nproxy: http://p:3128\ntls: {ca_file: /nowhere.pem}\n");
        let e = engine
            .build(&http, &call(&s, &c, 1), &Scope::new(), &post(""))
            .await
            .unwrap_err();
        assert!(e.contains("tls.ca_file '/nowhere.pem'"), "{e}");
    }
}
