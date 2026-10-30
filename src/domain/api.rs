use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use utoipa::ToSchema;

use crate::domain::http::{self, HttpSpec, Retry};
use crate::domain::template::{self, Allowed};
use crate::domain::trigger::{Trigger, glob};
use crate::domain::webhook::EVENT_FIELDS;

// An API item: when the watched project changes, call an API with what
// changed. One request per changed file (its contents as the body), or one
// per commit.
//
//   apis:
//     - name: portainer-users
//       project: infra
//       trigger: {paths: [portainer/users/*.json]}
//       credential: cred-portainer
//       api:
//         url: https://portainer.example/api/users/$file.stem
//         method: PUT
//         compare: {ignore_fields: [Id]}
//         on_delete: {method: DELETE}
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Api {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub project: String,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub trigger: Trigger,
    #[serde(default)]
    pub credential: Option<String>,
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub retry: Retry,
    // What a failed request does to the rest of the run: go on with the
    // other files, or stop there.
    #[serde(default)]
    pub on_failure: OnFailure,
    pub api: ApiRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    #[default]
    Continue,
    Stop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApiRequest {
    pub http: HttpSpec,
    pub per: Per,
    // A template over $file, $doc and $event. Left out: per file, the file's
    // contents ($doc); per commit, no body.
    pub body: Option<Value>,
    // How a file is read into $doc. Left out: by its extension (.json,
    // .yaml/.yml; anything else is raw text).
    pub format: Option<Format>,
    // Left out: application/json for a JSON body, text/plain for text.
    pub content_type: Option<String>,
    pub upsert: Option<Upsert>,
    pub compare: Option<Compare>,
    pub on_delete: Option<Step>,
    // Which files go first in one run: those matching the first pattern,
    // then the second; the rest after, each group in path order.
    pub order: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiOwn {
    #[serde(default)]
    per: Per,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    format: Option<Format>,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    upsert: Option<Upsert>,
    #[serde(default)]
    compare: Option<Compare>,
    #[serde(default)]
    on_delete: Option<Step>,
    #[serde(default)]
    order: Vec<String>,
}

impl<'de> Deserialize<'de> for ApiRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let (http, own): (HttpSpec, ApiOwn) = http::split(deserializer)?;
        Ok(Self {
            http,
            per: own.per,
            body: own.body,
            format: own.format,
            content_type: own.content_type,
            upsert: own.upsert,
            compare: own.compare,
            on_delete: own.on_delete,
            order: own.order,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Per {
    #[default]
    File,
    Commit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    Json,
    Yaml,
    Raw,
}

// One request of a flow: a method and a URL, each falling back to the
// api's own when left out.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

// Create or update, as the API says the object exists: `exists` is asked
// first (a 2xx: it does, a 404: it does not), then `update` or `create` is
// sent. Their defaults: GET, PUT and POST, at the api's URL.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Upsert {
    #[serde(default)]
    pub exists: Option<Step>,
    #[serde(default)]
    pub create: Option<Step>,
    #[serde(default)]
    pub update: Option<Step>,
}

// Ask first, and send nothing when the live object already has what the
// body says: every field the body names is compared (fields the API adds
// are not), except `ignore_fields` (dotted for nested ones).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compare {
    // Where the live object is read; left out, the api's URL (or upsert's
    // exists).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub ignore_fields: Vec<String>,
}

fn yes() -> bool {
    true
}

fn default_timeout_seconds() -> u64 {
    30
}

// What $file holds: the changed file, named every way a URL wants it.
pub const FILE_FIELDS: [&str; 6] = ["path", "name", "stem", "ext", "dir", "status"];

impl Upsert {
    pub fn exists(&self, api: &HttpSpec) -> (String, String) {
        resolve(self.exists.as_ref(), "GET", api)
    }
    pub fn create(&self, api: &HttpSpec) -> (String, String) {
        resolve(self.create.as_ref(), "POST", api)
    }
    pub fn update(&self, api: &HttpSpec) -> (String, String) {
        resolve(self.update.as_ref(), "PUT", api)
    }
}

impl Step {
    pub fn resolve(&self, default_method: &str, api: &HttpSpec) -> (String, String) {
        resolve(Some(self), default_method, api)
    }
}

fn resolve(step: Option<&Step>, default_method: &str, api: &HttpSpec) -> (String, String) {
    (
        step.and_then(|s| s.method.clone())
            .unwrap_or_else(|| default_method.to_string()),
        step.and_then(|s| s.url.clone())
            .unwrap_or_else(|| api.url.clone()),
    )
}

impl Api {
    // Everything wrong with this item on its own. `parse_cron` checks a
    // trigger schedule.
    pub fn errors(&self, id: &str, parse_cron: impl Fn(&str) -> Result<(), String>) -> Vec<String> {
        let mut errors = self.trigger.errors(parse_cron);
        let request = &self.api;
        let per_file = request.per == Per::File;
        let file_fields: &[(&str, &[&str])] = &[("event", &EVENT_FIELDS), ("file", &FILE_FIELDS)];
        let event_only: &[(&str, &[&str])] = &[("event", &EVENT_FIELDS)];
        let (url_context, body_context) = if per_file {
            (
                Allowed {
                    roots: &["event", "file", "doc"],
                    fields: file_fields,
                },
                Allowed {
                    roots: &["event", "file", "doc"],
                    fields: file_fields,
                },
            )
        } else {
            (
                Allowed {
                    roots: &["event"],
                    fields: event_only,
                },
                Allowed {
                    roots: &["event"],
                    fields: event_only,
                },
            )
        };
        errors.extend(request.http.errors(
            "api",
            self.credential.is_some(),
            &http::Contexts {
                request: Allowed {
                    roots: url_context.roots,
                    fields: url_context.fields,
                },
            },
        ));
        if let Some(body) = &request.body {
            errors.extend(template::errors("api.body", body, &body_context));
        }
        if let Some(content_type) = &request.content_type
            && (content_type.is_empty() || content_type.chars().any(|c| c.is_control()))
        {
            errors.push(format!(
                "api.content_type '{content_type}' is not a media type"
            ));
        }
        let mut steps: Vec<(String, (String, String))> = Vec::new();
        if let Some(upsert) = &request.upsert {
            steps.push(("api.upsert.exists".into(), upsert.exists(&request.http)));
            steps.push(("api.upsert.create".into(), upsert.create(&request.http)));
            steps.push(("api.upsert.update".into(), upsert.update(&request.http)));
        }
        if let Some(compare) = &request.compare
            && let Some(url) = &compare.url
        {
            steps.push(("api.compare".into(), ("GET".into(), url.clone())));
        }
        if let Some(delete) = &request.on_delete {
            steps.push((
                "api.on_delete".into(),
                delete.resolve("DELETE", &request.http),
            ));
            if !per_file {
                errors.push(
                    "api.on_delete deletes what a removed file described: it needs per: file"
                        .to_string(),
                );
            }
        }
        for (at, (method, url)) in &steps {
            if !http::METHODS.contains(&method.as_str()) {
                errors.push(format!(
                    "{at}.method '{method}' must be one of {}",
                    http::METHODS.join(", ")
                ));
            }
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                errors.push(format!(
                    "{at}.url '{}' must be an http:// or https:// address",
                    http::origin(url)
                ));
            }
            // A deleted file has no contents: its URL can name the file only.
            let roots: &[&str] = if at == "api.on_delete" {
                &["event", "file"]
            } else {
                url_context.roots
            };
            errors.extend(template::string_errors(
                &format!("{at}.url"),
                url,
                &Allowed {
                    roots,
                    fields: url_context.fields,
                },
            ));
        }
        if (request.upsert.is_some() || request.compare.is_some()) && !per_file {
            errors.push(
                "api.upsert and api.compare read one object per file: they need per: file"
                    .to_string(),
            );
        }
        if !request.order.is_empty() && !per_file {
            errors.push("api.order orders files: it needs per: file".to_string());
        }
        for pattern in &request.order {
            if pattern.trim().is_empty() || pattern.starts_with('/') {
                errors.push(format!(
                    "api.order '{pattern}' must be a path pattern relative to the repository"
                ));
            }
        }
        errors.extend(http::timeout_errors(self.timeout_seconds));
        errors.extend(http::retry_errors(&self.retry));
        errors
            .into_iter()
            .map(|e| format!("API '{id}': {e}"))
            .collect()
    }

    pub fn credentials(&self) -> Vec<&String> {
        self.credential
            .iter()
            .chain(self.api.http.extra_credentials())
            .collect()
    }

    pub fn destination(&self) -> String {
        http::origin(&self.api.http.url)
    }
}

impl ApiRequest {
    // `files` in the order `order` asks for: by the first pattern each
    // matches, then the rest; ties in path order.
    pub fn ordered<'a>(&self, files: &[&'a str]) -> Vec<&'a str> {
        let rank = |f: &str| {
            self.order
                .iter()
                .position(|p| glob(p, f))
                .unwrap_or(self.order.len())
        };
        let mut sorted: Vec<&str> = files.to_vec();
        sorted.sort_by(|a, b| (rank(a), *a).cmp(&(rank(b), *b)));
        sorted
    }

    // How a file's bytes become $doc.
    pub fn read(&self, path: &str, bytes: &[u8]) -> Result<Value, String> {
        let format = self.format.unwrap_or_else(|| format_of(path));
        let text = String::from_utf8_lossy(bytes);
        match format {
            Format::Json => serde_json::from_str(&text).map_err(|e| format!("{path}: {e}")),
            Format::Yaml => crate::domain::yaml::to_json(&text).map_err(|e| format!("{path}: {e}")),
            Format::Raw => Ok(Value::String(text.into_owned())),
        }
    }

    pub fn content_type_for(&self, body: &Value) -> String {
        match (&self.content_type, body) {
            (Some(content_type), _) => content_type.clone(),
            (None, Value::String(_)) => "text/plain; charset=utf-8".to_string(),
            (None, _) => "application/json".to_string(),
        }
    }
}

fn format_of(path: &str) -> Format {
    let lower = path.to_ascii_lowercase();
    if lower.ends_with(".json") {
        Format::Json
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        Format::Yaml
    } else {
        Format::Raw
    }
}

// $file for a path: users/team/jane.json → name jane.json, stem jane, ext
// json, dir users/team. `status` is added, removed or changed.
pub fn file_value(path: &str, status: &str) -> Value {
    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, ext),
        _ => (name, ""),
    };
    json!({
        "path": path,
        "name": name,
        "stem": stem,
        "ext": ext,
        "dir": dir,
        "status": status,
    })
}

// Whether `live` already has everything `desired` says: every key desired
// names, with an equal value, at every depth (numbers by value); a list
// must have the same length, element by element. `ignore` names fields not
// compared, dotted for nested ones (`spec.replicas`).
pub fn covers(live: &Value, desired: &Value, ignore: &[String]) -> bool {
    covers_at(live, desired, ignore, "")
}

fn covers_at(live: &Value, desired: &Value, ignore: &[String], at: &str) -> bool {
    match (live, desired) {
        (Value::Object(live), Value::Object(desired)) => desired.iter().all(|(key, want)| {
            let path = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            ignore.contains(&path)
                || live
                    .get(key)
                    .is_some_and(|have| covers_at(have, want, ignore, &path))
        }),
        (Value::Array(live), Value::Array(desired)) => {
            live.len() == desired.len()
                && live
                    .iter()
                    .zip(desired)
                    .all(|(have, want)| covers_at(have, want, ignore, at))
        }
        (Value::Number(a), Value::Number(b)) => a.as_f64() == b.as_f64(),
        (a, b) => a == b,
    }
}

// The differences, for a dry run or a run's record: the fields whose value
// would change, by dotted path, never their values (a value can be a
// secret).
pub fn differing_fields(live: &Value, desired: &Value, ignore: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    diff_at(live, desired, ignore, "", &mut out);
    out
}

fn diff_at(live: &Value, desired: &Value, ignore: &[String], at: &str, out: &mut Vec<String>) {
    if let (Value::Object(live), Value::Object(desired)) = (live, desired) {
        for (key, want) in desired {
            let path = if at.is_empty() {
                key.clone()
            } else {
                format!("{at}.{key}")
            };
            if ignore.contains(&path) {
                continue;
            }
            match live.get(key) {
                Some(have) => diff_at(have, want, ignore, &path, out),
                None => out.push(path),
            }
        }
    } else if !covers_at(live, desired, ignore, at) {
        out.push(if at.is_empty() {
            "(body)".to_string()
        } else {
            at.to_string()
        });
    }
}

// An empty map, for the message fields a step has none of.
pub fn empty() -> Map<String, Value> {
    Map::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Api {
        serde_saphyr::from_str(yaml).unwrap()
    }

    fn errors(yaml: &str) -> String {
        parse(yaml).errors("a", |_| Ok(())).join("\n")
    }

    #[test]
    fn an_api_needs_a_project_and_a_url() {
        let a = parse("project: p\napi:\n  url: https://x/users/$file.stem\n");
        assert!(
            a.errors("a", |_| Ok(())).is_empty(),
            "{:?}",
            a.errors("a", |_| Ok(()))
        );
        assert_eq!(a.api.per, Per::File);
        assert_eq!(a.api.http.method_or("PUT"), "PUT");
        assert_eq!(a.on_failure, OnFailure::Continue);
    }

    #[test]
    fn every_option_parses() {
        let yaml = "description: Users\nenabled: false\nproject: p\nlabels: {team: ops}\ntrigger:\n  paths: [users/*.json]\n  ignore_paths: ['**/*.md']\n  on: [change, schedule, manual, startup]\n  schedule: '0 3 * * *'\n  debounce_seconds: 30\ncredential: cred\ntimeout_seconds: 20\nretry: {attempts: 2}\non_failure: stop\napi:\n  url: https://x/api/users/$file.stem\n  method: PUT\n  auth: {type: login, url: https://x/api/auth, body: {Username: $credential.username}, token: $response.jwt}\n  body: {user: $doc, by: $event.revision_short}\n  format: json\n  content_type: application/json\n  upsert:\n    exists: {url: https://x/api/users/$file.stem}\n    create: {method: POST, url: https://x/api/users}\n    update: {method: PUT}\n  compare: {ignore_fields: [Id, meta.created]}\n  on_delete: {method: DELETE, url: https://x/api/users/$file.stem}\n  order: [roles/**, users/**]\n";
        assert_eq!(errors(yaml), "");
        let a = parse(yaml);
        assert_eq!(
            a.api.upsert.as_ref().unwrap().update(&a.api.http),
            (
                "PUT".to_string(),
                "https://x/api/users/$file.stem".to_string()
            )
        );
    }

    #[test]
    fn what_cannot_work_is_named() {
        let e = errors(
            "project: p\napi:\n  url: https://x/$doc.id\n  per: commit\n  body: {a: $file.path}\n  upsert: {}\n  on_delete: {url: ftp://x/$doc.id}\n  order: [a]\n",
        );
        for expected in [
            "api.url names $doc.id",
            "api.body names $file.path",
            "it needs per: file",
            "api.on_delete.url 'ftp://x' must be",
            "api.order orders files",
        ] {
            assert!(e.contains(expected), "missing {expected:?} in {e}");
        }
        let e = errors(
            "project: p\napi:\n  url: https://x/$file.nmae\n  on_delete: {method: REMOVE}\n",
        );
        assert!(e.contains("$file.nmae") && e.contains("'REMOVE'"), "{e}");
        assert!(
            serde_saphyr::from_str::<Api>("project: p\napi:\n  url: https://x\n  prune: true\n")
                .is_err()
        );
    }

    #[test]
    fn files_are_named_read_and_ordered() {
        assert_eq!(
            file_value("users/team/jane.json", "changed"),
            json!({"path": "users/team/jane.json", "name": "jane.json", "stem": "jane", "ext": "json", "dir": "users/team", "status": "changed"})
        );
        assert_eq!(file_value("Makefile", "added")["stem"], "Makefile");
        assert_eq!(file_value(".env", "added")["stem"], ".env");
        let a = parse("project: p\napi:\n  url: https://x\n  order: [roles/**, users/admin*]\n");
        assert_eq!(
            a.api
                .ordered(&["users/b.json", "users/admin.json", "roles/r.json", "a.json"]),
            ["roles/r.json", "users/admin.json", "a.json", "users/b.json"]
        );
        assert_eq!(
            a.api.read("x.json", b"{\"a\": 1}").unwrap(),
            json!({"a": 1})
        );
        assert_eq!(
            a.api.read("x.yml", b"a: '0644'\n").unwrap(),
            json!({"a": "0644"})
        );
        assert_eq!(a.api.read("x.txt", b"hi").unwrap(), json!("hi"));
        assert!(a.api.read("x.json", b"{").is_err());
        assert_eq!(
            a.api.content_type_for(&json!("hi")),
            "text/plain; charset=utf-8"
        );
        assert_eq!(a.api.content_type_for(&json!({})), "application/json");
    }

    #[test]
    fn covers_compares_only_what_is_desired() {
        let live = json!({"Id": 7, "Username": "jane", "Role": 1, "meta": {"created": 1, "by": "x"}, "tags": ["a"]});
        assert!(covers(
            &live,
            &json!({"Username": "jane", "Role": 1.0}),
            &[]
        ));
        assert!(!covers(&live, &json!({"Username": "john"}), &[]));
        assert!(covers(
            &live,
            &json!({"Id": 8, "meta": {"created": 2}}),
            &["Id".into(), "meta.created".into()]
        ));
        assert!(!covers(&live, &json!({"tags": ["a", "b"]}), &[]));
        assert!(!covers(&live, &json!({"Missing": null}), &[]));
        assert_eq!(
            differing_fields(
                &live,
                &json!({"Username": "john", "Role": 1, "New": 1, "meta": {"by": "y"}}),
                &[]
            ),
            ["New", "Username", "meta.by"]
        );
        assert_eq!(differing_fields(&json!("a"), &json!("b"), &[]), ["(body)"]);
        assert!(empty().is_empty());
    }
}
