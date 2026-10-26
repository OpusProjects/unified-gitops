use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

use unified_gitops::adapters::out::secrets::mock::MockSecrets;
use unified_gitops::ports::notifier::{
    HttpSenderPort, OutboundRequest, OutboundResponse, SendFuture,
};

// Notifications through the real router and the example connector: a sync
// that applies a commit sends sync_succeeded to the receiver, with the
// credential where the notification says; a failing check sends sync_failed;
// the API lists the last send and sends a test.

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<OutboundRequest>>,
}

impl HttpSenderPort for Recorder {
    fn send(&self, request: OutboundRequest) -> SendFuture<'_> {
        self.sent.lock().unwrap().push(request);
        Box::pin(async {
            Ok(OutboundResponse {
                status: 202,
                body: r#"{"message": "Job queued."}"#.into(),
            })
        })
    }
}

impl Recorder {
    async fn wait_for(&self, count: usize) -> Vec<OutboundRequest> {
        for _ in 0..100 {
            let sent = self.sent.lock().unwrap().clone();
            if sent.len() >= count {
                return sent;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {count} sends, got {}",
            self.sent.lock().unwrap().len()
        );
    }
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn header<'a>(request: &'a OutboundRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

struct Fixture {
    app: axum::Router,
    recorder: Arc<Recorder>,
    _dirs: Vec<tempfile::TempDir>,
}

fn fixture() -> Fixture {
    let projects = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write(
        &projects.path().join("prj-state/apps/demo/namespaces.yaml"),
        "kind: namespace\nitems:\n  - name: team-a\n    description: Team A\n",
    );

    let connector = |script: &str| -> unified_gitops::domain::connector::Connector {
        serde_saphyr::from_str(&format!("path: {}/{script}\n", env!("CARGO_MANIFEST_DIR"))).unwrap()
    };
    let app_def = |connector: &str| -> unified_gitops::domain::app::Application {
        serde_saphyr::from_str(&format!(
            "description: Demo\nconnector: {connector}\nconfig:\n  dir: {}\nproject: prj-state\npath: apps/demo\n",
            store.path().display()
        ))
        .unwrap()
    };
    let project: unified_gitops::domain::project::GitProject =
        serde_saphyr::from_str("git_url: https://example.com/state.git\n").unwrap();
    let hook: unified_gitops::domain::notification::Notification = serde_saphyr::from_str(
        "type: webhook\nevents: [sync_succeeded, sync_failed]\nwebhook:\n  url: https://hooks.example.com/services/T0/secret-path\n  credentials: [cred-hook]\n  retries: 0\n",
    )
    .unwrap();

    let recorder = Arc::new(Recorder::default());
    let (app, app_state) = unified_gitops::AppBuilder::new()
        .applications(HashMap::from([
            ("app-demo".to_string(), app_def("kv")),
            ("app-broken".to_string(), app_def("missing")),
        ]))
        .connectors(HashMap::from([
            ("kv".to_string(), connector("tests/connectors/kv.py")),
            (
                "missing".to_string(),
                connector("tests/connectors/does-not-exist.py"),
            ),
        ]))
        .projects(
            HashMap::from([("prj-state".to_string(), project)]),
            projects.path().to_path_buf(),
        )
        .notifications(HashMap::from([("hook".to_string(), hook)]))
        .secrets(Arc::new(
            MockSecrets::new().with("cred-hook", [("token", "hook-token")]),
        ))
        .http(recorder.clone())
        .state_dir(state.path().to_path_buf())
        .api_key(Some("admin-secret".into()))
        .build_with_state();
    app_state.start_notifier();

    Fixture {
        app,
        recorder,
        _dirs: vec![projects, store, state],
    }
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", "admin-secret")
                .header("accept", "application/json")
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

#[tokio::test]
async fn a_sync_that_applies_a_change_sends_sync_succeeded() {
    let f = fixture();
    let (status, run) = call(&f.app, "POST", "/api/v1/applications/app-demo/sync").await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["result"], "succeeded");

    let sent = f.recorder.wait_for(1).await;
    let request = &sent[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.url,
        "https://hooks.example.com/services/T0/secret-path"
    );
    assert_eq!(header(request, "Authorization"), Some("Bearer hook-token"));
    assert_eq!(
        header(request, "X-Unified-Gitops-Event"),
        Some("sync_succeeded")
    );
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    assert_eq!(body["event"], "sync_succeeded");
    assert_eq!(body["application"], "app-demo");
    assert_eq!(body["cause"], "commit");
    assert_eq!(body["applied"], 1);
    assert_eq!(body["changes"][0]["kind"], "namespace");
    assert_eq!(body["notification"], "hook");

    // A second sync finds nothing to do: nothing more is sent.
    let (_, again) = call(&f.app, "POST", "/api/v1/applications/app-demo/sync").await;
    assert_eq!(again["result"], "succeeded");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.recorder.sent.lock().unwrap().len(), 1);

    // The list shows the last send, and where it went without its path.
    let (status, list) = call(&f.app, "GET", "/api/v1/notifications").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list[0]["name"], "hook");
    assert_eq!(list[0]["destination"], "https://hooks.example.com");
    assert_eq!(list[0]["last"]["ok"], true);
    assert_eq!(list[0]["last"]["event"], "sync_succeeded");
    assert_eq!(list[0]["last"]["status"], 202);
}

#[tokio::test]
async fn a_failing_check_sends_sync_failed_once() {
    let f = fixture();
    let (_, run) = call(&f.app, "POST", "/api/v1/applications/app-broken/refresh").await;
    assert_eq!(run["result"], "failed", "{run}");
    let sent = f.recorder.wait_for(1).await;
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["event"], "sync_failed");
    assert_eq!(body["application"], "app-broken");
    assert!(!body["errors"].as_array().unwrap().is_empty());

    // Still failing: not a new event.
    call(&f.app, "POST", "/api/v1/applications/app-broken/refresh").await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(f.recorder.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn send_test_sends_now_and_answers_the_outcome() {
    let f = fixture();
    let (status, delivery) = call(&f.app, "POST", "/api/v1/notifications/hook/test").await;
    assert_eq!(status, StatusCode::OK, "{delivery}");
    assert_eq!(delivery["ok"], true);
    assert_eq!(delivery["event"], "test");
    let sent = f.recorder.sent.lock().unwrap().clone();
    let body: Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(body["event"], "test");

    let (status, _) = call(&f.app, "POST", "/api/v1/notifications/nope/test").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// The real client against a local receiver: headers and body arrive as
// built, and a redirect is reported, never followed with the credential.
#[tokio::test]
async fn the_http_sender_delivers_and_does_not_follow_redirects() {
    use axum::routing::post;
    let seen: Arc<Mutex<Vec<(String, String)>>> = Arc::default();
    let seen_in = Arc::clone(&seen);
    let receiver = axum::Router::new()
        .route(
            "/hook",
            post(move |headers: axum::http::HeaderMap, body: String| {
                let seen = Arc::clone(&seen_in);
                async move {
                    let token = headers
                        .get("x-gitlab-token")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen.lock().unwrap().push((token, body));
                    (StatusCode::ACCEPTED, "{\"message\": \"Job queued.\"}")
                }
            }),
        )
        .route(
            "/moved",
            post(|| async { axum::response::Redirect::temporary("/hook") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });

    let sender = unified_gitops::adapters::out::notify::http::ReqwestSender::new();
    let request = |path: &str| OutboundRequest {
        method: "POST".into(),
        url: format!("http://{addr}{path}"),
        headers: vec![
            ("X-Gitlab-Token".into(), "jt-key".into()),
            ("Content-Type".into(), "application/json".into()),
        ],
        body: br#"{"event":"drift_detected"}"#.to_vec(),
        timeout: Duration::from_secs(5),
    };
    let answer = sender.send(request("/hook")).await.unwrap();
    assert_eq!(answer.status, 202);
    assert!(answer.body.contains("Job queued."));
    assert_eq!(
        seen.lock().unwrap()[0],
        (
            "jt-key".to_string(),
            r#"{"event":"drift_detected"}"#.to_string()
        )
    );

    let moved = sender.send(request("/moved")).await.unwrap();
    assert_eq!(moved.status, 307);
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "the redirect was not followed"
    );

    let unreachable = sender
        .send(OutboundRequest {
            url: "http://127.0.0.1:1/hook?token=secret".into(),
            ..request("/hook")
        })
        .await
        .unwrap_err();
    assert!(!unreachable.contains("secret"), "{unreachable}");
}
