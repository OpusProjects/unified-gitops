use axum::body::Body;
use axum::extract::{Path as UrlPath, State};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

use unified_gitops::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};
use unified_gitops::adapters::out::secrets::mock::MockSecrets;

// API items end to end: a real repository over file://, a real HTTP server
// standing in for the system, and the service between them. A commit to a
// watched file calls the API with it; compare skips what is already there;
// upsert creates or updates; a removed file is deleted; runs are recorded
// and told to the project's webhooks.

#[derive(Default)]
struct System {
    users: BTreeMap<String, Value>,
    // Every request, as "METHOD /path".
    calls: Vec<String>,
    hooks: Vec<Value>,
    failing: bool,
}

type Shared = Arc<Mutex<System>>;

async fn system() -> (String, Shared) {
    use axum::routing::{get, post};
    let shared: Shared = Arc::default();
    async fn read(State(s): State<Shared>, UrlPath(id): UrlPath<String>) -> (StatusCode, String) {
        let mut s = s.lock().unwrap();
        s.calls.push(format!("GET /users/{id}"));
        match s.users.get(&id) {
            Some(u) => (StatusCode::OK, u.to_string()),
            None => (StatusCode::NOT_FOUND, String::new()),
        }
    }
    async fn write(
        State(s): State<Shared>,
        UrlPath(id): UrlPath<String>,
        body: axum::body::Bytes,
    ) -> StatusCode {
        let mut s = s.lock().unwrap();
        s.calls.push(format!("PUT /users/{id}"));
        if s.failing {
            return StatusCode::BAD_REQUEST;
        }
        let mut user: Value = serde_json::from_slice(&body).unwrap();
        user["Id"] = json!(7);
        s.users.insert(id, user);
        StatusCode::OK
    }
    async fn create(State(s): State<Shared>, body: axum::body::Bytes) -> StatusCode {
        let mut s = s.lock().unwrap();
        let user: Value = serde_json::from_slice(&body).unwrap();
        let id = user["Username"].as_str().unwrap().to_string();
        s.calls.push(format!("POST /users ({id})"));
        s.users.insert(id, user);
        StatusCode::CREATED
    }
    async fn remove(State(s): State<Shared>, UrlPath(id): UrlPath<String>) -> StatusCode {
        let mut s = s.lock().unwrap();
        s.calls.push(format!("DELETE /users/{id}"));
        s.users.remove(&id);
        StatusCode::NO_CONTENT
    }
    async fn bulk(State(s): State<Shared>, body: axum::body::Bytes) -> StatusCode {
        let mut s = s.lock().unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        s.calls.push(format!("POST /sync {}", body["files"]));
        StatusCode::ACCEPTED
    }
    async fn hook(State(s): State<Shared>, body: axum::body::Bytes) -> StatusCode {
        s.lock()
            .unwrap()
            .hooks
            .push(serde_json::from_slice(&body).unwrap());
        StatusCode::OK
    }
    let app = axum::Router::new()
        .route("/users/{id}", get(read).put(write).delete(remove))
        .route("/users", post(create))
        .route("/sync", post(bulk))
        .route("/hook", post(hook))
        .with_state(Arc::clone(&shared));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), shared)
}

async fn git(dir: &Path, args: &[&str]) {
    let status = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Jane Doe")
        .env("GIT_AUTHOR_EMAIL", "jane@example.com")
        .env("GIT_COMMITTER_NAME", "Jane Doe")
        .env("GIT_COMMITTER_EMAIL", "jane@example.com")
        .status()
        .await
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

async fn commit(origin: &Path, files: &[(&str, Option<&str>)]) {
    for (file, body) in files {
        let path = origin.join(file);
        match body {
            Some(body) => {
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, body).unwrap();
            }
            None => std::fs::remove_file(&path).unwrap(),
        }
    }
    git(origin, &["add", "-A"]).await;
    git(origin, &["commit", "-m", "change"]).await;
}

async fn call(app: &axum::Router, key: &str, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// Until `check` holds, or five seconds.
async fn until(shared: &Shared, check: impl Fn(&System) -> bool) {
    for _ in 0..100 {
        if check(&shared.lock().unwrap()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let s = shared.lock().unwrap();
    panic!("timed out: calls {:?}, hooks {:?}", s.calls, s.hooks);
}

fn take_calls(shared: &Shared) -> Vec<String> {
    std::mem::take(&mut shared.lock().unwrap().calls)
}

fn keys() -> Vec<ResolvedApiKey> {
    vec![
        ResolvedApiKey {
            id: "admin".into(),
            name: "admin".into(),
            secret: "adm".into(),
            permissions: Permissions::Admin,
        },
        ResolvedApiKey {
            id: "other".into(),
            name: "other".into(),
            secret: "oth".into(),
            permissions: Permissions::Scoped {
                projects: ["prj-other".to_string()].into_iter().collect(),
            },
        },
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apis_act_on_what_changed() {
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--initial-branch", "main"]).await;
    commit(&origin, &[("README.md", Some("users\n"))]).await;
    let checkouts = tempfile::tempdir().unwrap();
    let (base, shared) = system().await;

    let project: unified_gitops::domain::project::GitProject =
        serde_saphyr::from_str(&format!("git_url: file://{}\n", origin.display())).unwrap();
    let item = |yaml: String| -> unified_gitops::domain::api::Api {
        serde_saphyr::from_str(&yaml).unwrap()
    };
    let users = item(format!(
        "description: Users\nproject: prj\ntrigger: {{paths: [users/*.json, users/*.yaml]}}\ncredential: cred\nretry: {{attempts: 1}}\napi:\n  url: {base}/users/$file.stem\n  auth: bearer\n  upsert:\n    create: {{url: {base}/users}}\n  compare: {{ignore_fields: [Id]}}\n  on_delete: {{method: DELETE}}\n  order: [users/admin*]\n"
    ));
    let bulk = item(format!(
        "project: prj\ntrigger: {{paths: [bulk/**], debounce_seconds: 1}}\napi:\n  url: {base}/sync\n  method: POST\n  per: commit\n  body: {{files: $event.files, by: $event.trigger}}\n"
    ));
    let nightly = item(format!(
        "project: prj\ntrigger: {{paths: [users/*.json], on: [schedule, startup], schedule: '* * * * * *'}}\nenabled: false\napi:\n  url: {base}/users/$file.stem\n"
    ));
    let hook: unified_gitops::domain::webhook::Webhook = serde_saphyr::from_str(&format!(
        "project: prj\nevents: [api_succeeded, api_failed, api_recovered]\nwebhook:\n  url: {base}/hook\n  body: {{event: $event.event, item: $event.item, steps: $event.steps}}\n"
    ))
    .unwrap();

    let (app, state) = unified_gitops::AppBuilder::new()
        .projects(
            HashMap::from([("prj".to_string(), project)]),
            checkouts.path().to_path_buf(),
        )
        .apis(HashMap::from([
            ("users".to_string(), users),
            ("bulk".to_string(), bulk),
            ("nightly".to_string(), nightly),
        ]))
        .webhooks(HashMap::from([("hook".to_string(), hook)]))
        .secrets(Arc::new(
            MockSecrets::new().with("cred", [("token", "t0k")]),
        ))
        .api_keys(keys())
        .build_with_state();
    state.start_webhooks();
    let pull = |app: axum::Router| async move {
        let (status, body) = call(&app, "adm", "POST", "/api/v1/projects/prj/sync").await;
        assert_eq!(status, StatusCode::OK, "{body}");
    };

    // The clone runs nothing.
    pull(app.clone()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(take_calls(&shared).is_empty());

    // Two new users: each is read (404) and created, the admin first.
    commit(
        &origin,
        &[
            ("users/zed.json", Some(r#"{"Username": "zed", "Role": 2}"#)),
            ("users/admin.yaml", Some("Username: admin\nRole: 1\n")),
            ("docs/x.md", Some("not watched\n")),
        ],
    )
    .await;
    pull(app.clone()).await;
    until(&shared, |s| s.users.len() == 2 && !s.hooks.is_empty()).await;
    assert_eq!(
        take_calls(&shared),
        [
            "GET /users/admin",
            "POST /users (admin)",
            "GET /users/zed",
            "POST /users (zed)"
        ]
    );
    {
        let s = shared.lock().unwrap();
        assert_eq!(s.hooks[0]["event"], "api_succeeded");
        assert_eq!(s.hooks[0]["item"], "users");
        assert_eq!(s.hooks[0]["steps"][0]["action"], "create");
    }

    // zed changes, admin is touched without changing: zed is updated, admin
    // skipped by compare.
    commit(
        &origin,
        &[
            ("users/zed.json", Some(r#"{"Username": "zed", "Role": 3}"#)),
            ("users/admin.yaml", Some("Username: admin\nRole: 1.0\n")),
        ],
    )
    .await;
    pull(app.clone()).await;
    until(&shared, |s| s.users["zed"]["Role"] == 3).await;
    let (_, detail) = call(&app, "adm", "GET", "/api/v1/apis/users").await;
    let last = &detail["history"][0];
    assert_eq!(last["ok"], true, "{last}");
    assert_eq!(last["trigger"], "change");
    let actions: Vec<(&str, &str)> = last["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["file"].as_str().unwrap(), s["action"].as_str().unwrap()))
        .collect();
    assert_eq!(
        actions,
        [("users/admin.yaml", "skip"), ("users/zed.json", "update")]
    );
    assert_eq!(last["steps"][1]["changed_fields"], json!(["Role"]));
    assert_eq!(last["steps"][1]["url"], format!("{base}/users/zed"));
    take_calls(&shared);

    // The system refuses: the run fails and is told; fixed, it recovers.
    shared.lock().unwrap().failing = true;
    commit(
        &origin,
        &[("users/zed.json", Some(r#"{"Username": "zed", "Role": 4}"#))],
    )
    .await;
    pull(app.clone()).await;
    until(&shared, |s| {
        s.hooks.iter().any(|h| h["event"] == "api_failed")
    })
    .await;
    shared.lock().unwrap().failing = false;
    let (status, run) = call(&app, "adm", "POST", "/api/v1/apis/users/run").await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(
        (run["ok"].clone(), run["trigger"].clone(), run["by"].clone()),
        (json!(true), json!("manual"), json!("admin"))
    );
    until(&shared, |s| {
        s.hooks.iter().any(|h| h["event"] == "api_recovered")
    })
    .await;
    take_calls(&shared);

    // A dry run builds every request and writes nothing: asked before the
    // commit is pulled, so only its reads reach the system.
    let (status, run) = call(&app, "adm", "POST", "/api/v1/apis/users/run?dry_run=true").await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["dry_run"], true);
    let calls = take_calls(&shared);
    assert!(calls.iter().all(|c| c.starts_with("GET")), "{calls:?}");
    commit(
        &origin,
        &[("users/zed.json", Some(r#"{"Username": "zed", "Role": 5}"#))],
    )
    .await;
    pull(app.clone()).await;
    until(&shared, |s| s.users["zed"]["Role"] == 5).await;

    // Removing a file deletes what it described.
    commit(&origin, &[("users/zed.json", None)]).await;
    pull(app.clone()).await;
    until(&shared, |s| !s.users.contains_key("zed")).await;
    assert!(take_calls(&shared).contains(&"DELETE /users/zed".to_string()));

    // Per commit with a debounce: two quick commits, one request with both.
    commit(&origin, &[("bulk/a", Some("a"))]).await;
    pull(app.clone()).await;
    commit(&origin, &[("bulk/b", Some("b"))]).await;
    pull(app.clone()).await;
    until(&shared, |s| {
        s.calls.iter().any(|c| c.starts_with("POST /sync"))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let bulk: Vec<String> = take_calls(&shared)
        .into_iter()
        .filter(|c| c.starts_with("POST /sync"))
        .collect();
    assert_eq!(bulk, [r#"POST /sync ["bulk/a","bulk/b"]"#]);

    // An item whose trigger has no manual does not run by hand, and says
    // so.
    let (status, body) = call(&app, "adm", "POST", "/api/v1/apis/nightly/run").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Scoped keys: another project's items read as absent.
    let (_, list) = call(&app, "oth", "GET", "/api/v1/apis").await;
    assert_eq!(list, json!([]));
    let (status, _) = call(&app, "oth", "POST", "/api/v1/apis/users/run").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, list) = call(&app, "adm", "GET", "/api/v1/apis").await;
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bulk", "nightly", "users"]);
    assert_eq!(list[2]["auth"], "bearer");
    let (_, projects) = call(&app, "adm", "GET", "/api/v1/projects").await;
    assert_eq!(
        projects[0]["used_by"]["apis"],
        json!(["bulk", "nightly", "users"])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_and_startup_runs_cover_every_selected_file() {
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--initial-branch", "main"]).await;
    commit(
        &origin,
        &[
            ("users/a.json", Some(r#"{"Username": "a"}"#)),
            ("other.txt", Some("x")),
        ],
    )
    .await;
    let checkouts = tempfile::tempdir().unwrap();
    let (base, shared) = system().await;
    let project: unified_gitops::domain::project::GitProject =
        serde_saphyr::from_str(&format!("git_url: file://{}\n", origin.display())).unwrap();
    let api: unified_gitops::domain::api::Api = serde_saphyr::from_str(&format!(
        "project: prj\ntrigger: {{paths: [users/**], on: [schedule, startup], schedule: '* * * * * *'}}\napi:\n  url: {base}/users/$file.stem\n"
    ))
    .unwrap();
    let (_app, state) = unified_gitops::AppBuilder::new()
        .projects(
            HashMap::from([("prj".to_string(), project)]),
            checkouts.path().to_path_buf(),
        )
        .apis(HashMap::from([("each".to_string(), api)]))
        .build_with_state();
    state.start_webhooks();
    let config = state.config();
    unified_gitops::application::projects::sync_project(
        &*state.git,
        &*state.secrets,
        &*state.venv,
        &state.project_health,
        "prj",
        &config.projects["prj"],
        &state.projects_dir,
        Some(state.announce(&config)),
        unified_gitops::application::projects::RunOrigin::of(
            unified_gitops::domain::sync_health::RunTrigger::Startup,
        ),
    )
    .await
    .unwrap();

    state.runner.at_startup(&config);
    until(&shared, |s| s.calls == ["PUT /users/a"]).await;
    take_calls(&shared);

    let (shutdown, rx) = tokio::sync::watch::channel(false);
    let tasks =
        unified_gitops::adapters::r#in::scheduler::start_item_schedules(Arc::clone(&state), rx);
    assert_eq!(tasks.len(), 1);
    until(&shared, |s| s.calls.iter().any(|c| c == "PUT /users/a")).await;
    let _ = shutdown.send(true);
    for task in tasks {
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    let history = state
        .runner
        .log
        .history(unified_gitops::domain::run::Kind::Api, "each");
    assert!(
        history
            .iter()
            .any(|r| r.trigger == unified_gitops::domain::trigger::On::Schedule)
    );
    assert!(
        history
            .iter()
            .any(|r| r.trigger == unified_gitops::domain::trigger::On::Startup)
    );
}
