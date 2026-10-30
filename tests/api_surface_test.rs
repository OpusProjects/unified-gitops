use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

use unified_gitops::adapters::out::secrets::mock::MockSecrets;

// The service end to end, as an operator meets it: the index, settings,
// credentials and readiness routes, a webhook's test send through the real
// HTTP client to a real listener, and the scheduler pulling a project on its
// interval and on its cron schedule, surviving a reload, and telling the
// commit it pulled.

const KEY: &str = "admin-secret";

// Every request the receiver got: its Authorization header and its body.
type Received = Arc<Mutex<Vec<(Option<String>, Value)>>>;

// A receiver on a free local port; every body it is sent, as JSON.
async fn receiver() -> (String, Received) {
    let got: Received = Arc::default();
    let seen = Arc::clone(&got);
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(
            move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let seen = Arc::clone(&seen);
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string);
                    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    seen.lock().unwrap().push((auth, body));
                    "ok"
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/hook"), got)
}

async fn wait_for(got: &Mutex<Vec<(Option<String>, Value)>>, event: &str) -> Value {
    for _ in 0..200 {
        if let Some((_, body)) = got
            .lock()
            .unwrap()
            .iter()
            .find(|(_, b)| b["event"] == event)
        {
            return body.clone();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no {event} received: {:?}", got.lock().unwrap());
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

async fn commit(origin: &Path, file: &str, body: &str) {
    std::fs::write(origin.join(file), body).unwrap();
    git(origin, &["add", "."]).await;
    git(origin, &["commit", "-m", file]).await;
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", KEY)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_service_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--initial-branch", "main"]).await;
    commit(&origin, "a.txt", "a\n").await;
    let checkouts = tempfile::tempdir().unwrap();

    let (url, got) = receiver().await;
    let project = |extra: &str| -> unified_gitops::domain::project::GitProject {
        serde_saphyr::from_str(&format!(
            "description: Infra\ngit_url: file://{}\n{extra}",
            origin.display()
        ))
        .unwrap()
    };
    let hook = |project: &str| -> unified_gitops::domain::webhook::Webhook {
        serde_saphyr::from_str(&format!(
            "project: {project}\nevents: [commit_pulled]\ncredential: cred-hook\nretry: {{attempts: 1}}\nwebhook:\n  url: {url}\n"
        ))
        .unwrap()
    };
    let credential: unified_gitops::domain::credential::Credential =
        serde_saphyr::from_str("type: token\nenv_vars:\n  token: HOOK_TOKEN\n").unwrap();

    let (app, state) = unified_gitops::AppBuilder::new()
        .projects(
            HashMap::from([
                (
                    "prj-interval".to_string(),
                    project("sync_interval_seconds: 1\n"),
                ),
                (
                    "prj-cron".to_string(),
                    project("schedule: \"* * * * * *\"\n"),
                ),
            ]),
            checkouts.path().to_path_buf(),
        )
        .webhooks(HashMap::from([
            ("hook".to_string(), hook("prj-interval")),
            ("hook-cron".to_string(), hook("prj-cron")),
        ]))
        .credentials(
            HashMap::from([("cred-hook".to_string(), credential)]),
            Default::default(),
        )
        .secrets(Arc::new(
            MockSecrets::new().with("cred-hook", [("token", "t0k")]),
        ))
        .api_key(Some(KEY.into()))
        .build_with_state();
    state.start_webhooks();

    // The index lists the routes; settings and credentials describe the
    // instance without a secret value.
    let (status, index) = call(&app, "GET", "/api/v1/").await;
    assert_eq!(status, StatusCode::OK, "{index}");
    let (status, settings) = call(&app, "GET", "/api/v1/settings").await;
    assert_eq!(status, StatusCode::OK, "{settings}");
    assert!(settings["version"].is_string());
    let (status, creds) = call(&app, "GET", "/api/v1/credentials").await;
    assert_eq!(status, StatusCode::OK, "{creds}");
    assert_eq!(creds[0]["name"], "cred-hook");
    assert_eq!(
        creds[0]["used_by"]["webhooks"],
        serde_json::json!(["hook", "hook-cron"])
    );
    assert!(!creds.to_string().contains("t0k"));

    // Before any pull, both projects are pending.
    let (_, ready) = call(&app, "GET", "/readyz").await;
    assert_eq!(ready["ready"], true);
    assert_eq!(ready["projects_total"], 2);
    assert_eq!(ready["projects_pending"].as_array().unwrap().len(), 2);

    // The webhook, its test send through the real client, and its history.
    let (status, list) = call(&app, "GET", "/api/v1/webhooks").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["project"], "prj-interval");
    assert_eq!(list[0]["request"]["auth"], "bearer");
    let (status, delivery) = call(&app, "POST", "/api/v1/webhooks/hook/test").await;
    assert_eq!(status, StatusCode::OK, "{delivery}");
    assert_eq!(delivery["ok"], true, "{delivery}");
    let (auth, body) = got.lock().unwrap()[0].clone();
    assert_eq!(auth.as_deref(), Some("Bearer t0k"));
    assert_eq!(body["event"], "test");
    let (status, _) = call(&app, "POST", "/api/v1/webhooks/nope/test").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, detail) = call(&app, "GET", "/api/v1/webhooks/hook").await;
    assert_eq!(detail["history"][0]["event"], "test");

    // The scheduler: clone both, then pick up a commit on each cadence.
    for id in ["prj-interval", "prj-cron"] {
        let (status, body) = call(&app, "POST", &format!("/api/v1/projects/{id}/sync")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (_, ready) = call(&app, "GET", "/readyz").await;
    assert_eq!(ready["projects_pulled"], 2);

    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    let supervisor = unified_gitops::adapters::r#in::scheduler::start_supervisor(
        Arc::clone(&state),
        shutdown_rx,
    );
    // A reload restarts the tasks; they keep pulling.
    state.reload.bump();
    commit(&origin, "b.txt", "b\n").await;
    let pulled = wait_for(&got, "commit_pulled").await;
    assert_eq!(pulled["files"], serde_json::json!(["b.txt"]));
    let mut seen: Vec<String> = Vec::new();
    for _ in 0..200 {
        seen = got
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, b)| b["event"] == "commit_pulled")
            .map(|(_, b)| b["project"].as_str().unwrap_or("").to_string())
            .collect();
        seen.sort();
        seen.dedup();
        if seen.len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(seen, ["prj-cron", "prj-interval"]);

    let _ = shutdown.send(true);
    tokio::time::timeout(Duration::from_secs(10), supervisor)
        .await
        .expect("the scheduler stops on shutdown")
        .unwrap();

    let (status, _) = call(&app, "GET", "/metrics").await;
    assert_eq!(status, StatusCode::OK);
}
