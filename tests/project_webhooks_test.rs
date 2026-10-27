use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

use unified_gitops::ports::http_sender::{
    HttpSenderPort, OutboundRequest, OutboundResponse, SendFuture,
};

// Project webhooks through the real router and a real repository over
// file://: a pull that brings a commit sends commit_pulled with the
// applications it touched; a pull that fails sends pull_failed once, however
// many times it fails; the first pull that works again sends pull_recovered.

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<OutboundRequest>>,
}

impl HttpSenderPort for Recorder {
    fn send(&self, request: OutboundRequest) -> SendFuture<'_> {
        self.sent.lock().unwrap().push(request);
        Box::pin(async {
            Ok(OutboundResponse {
                status: 200,
                body: String::new(),
            })
        })
    }
}

impl Recorder {
    async fn wait_for(&self, count: usize) -> Vec<Value> {
        for _ in 0..100 {
            let sent = self.sent.lock().unwrap().clone();
            if sent.len() >= count {
                return sent
                    .iter()
                    .map(|r| serde_json::from_slice(&r.body).unwrap())
                    .collect();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {count} sends, got {}",
            self.sent.lock().unwrap().len()
        );
    }
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

async fn commit(origin: &Path, file: &str, body: &str, message: &str) {
    let path = origin.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, body).unwrap();
    git(origin, &["add", "."]).await;
    git(origin, &["commit", "-m", message]).await;
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", "admin-secret")
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
async fn a_project_tells_its_commits_its_failures_and_its_recovery() {
    let root = tempfile::tempdir().unwrap();
    let origin = root.path().join("origin");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--initial-branch", "main"]).await;
    commit(
        &origin,
        "apps/demo/ns.yaml",
        "kind: namespace\nitems: []\n",
        "start",
    )
    .await;
    let projects = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();

    let app_at = |path: &str| -> unified_gitops::domain::app::Application {
        serde_saphyr::from_str(&format!(
            "connector: kv\nproject: prj-state\npath: {path}\n"
        ))
        .unwrap()
    };
    let project: unified_gitops::domain::project::GitProject = serde_saphyr::from_str(&format!(
        "description: Desired state\ngit_url: file://{}\n",
        origin.display()
    ))
    .unwrap();
    let hook: unified_gitops::domain::webhook::Webhook = serde_saphyr::from_str(
        "type: webhook\nevents: [commit_pulled, pull_failed, pull_recovered]\nprojects: [prj-state]\nwebhook:\n  url: https://hooks.example.com/x\n  retries: 0\n",
    )
    .unwrap();
    let connector: unified_gitops::domain::connector::Connector = serde_saphyr::from_str(&format!(
        "path: {}/tests/connectors/kv.py\n",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();

    let recorder = Arc::new(Recorder::default());
    let (app, app_state) = unified_gitops::AppBuilder::new()
        .applications(HashMap::from([
            ("app-demo".to_string(), app_at("apps/demo")),
            ("app-other".to_string(), app_at("apps/other")),
        ]))
        .connectors(HashMap::from([("kv".to_string(), connector)]))
        .projects(
            HashMap::from([("prj-state".to_string(), project)]),
            projects.path().to_path_buf(),
        )
        .webhooks(HashMap::from([("hook".to_string(), hook)]))
        .http(recorder.clone())
        .state_dir(state.path().to_path_buf())
        .api_key(Some("admin-secret".into()))
        .build_with_state();
    app_state.start_webhooks();

    // The first pull is a clone: nothing to compare with, nothing sent.
    let sync = "/api/v1/projects/prj-state/sync";
    let (status, body) = call(&app, "POST", sync).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Two commits, one of them to app-demo's folder.
    commit(&origin, "README.md", "hello\n", "docs").await;
    commit(
        &origin,
        "apps/demo/ns.yaml",
        "kind: namespace\nitems:\n  - name: a\n",
        "add a",
    )
    .await;
    let (status, body) = call(&app, "POST", sync).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sent = recorder.wait_for(1).await;
    let m = &sent[0];
    assert_eq!(m["event"], "commit_pulled");
    assert_eq!(m["project"], "prj-state");
    assert_eq!(m["application"], Value::Null);
    assert_eq!(m["description"], "Desired state");
    assert_eq!(m["webhook"], "hook");
    assert_eq!(m["commits"][0]["message"], "add a");
    assert_eq!(m["commits"][0]["author"], "Jane Doe");
    assert_eq!(m["commits"][1]["message"], "docs");
    assert_eq!(m["commits"][0]["revision"], m["revision"]);
    assert_ne!(m["previous_revision"], m["revision"]);
    assert_eq!(m["more_commits"], false);
    assert_eq!(m["forced"], false);
    assert_eq!(m["applications"], serde_json::json!(["app-demo"]));

    // Nothing new: nothing sent.
    let (status, _) = call(&app, "POST", sync).await;
    assert_eq!(status, StatusCode::OK);

    // The repository goes away: one pull_failed, however often it fails.
    let moved = root.path().join("gone");
    std::fs::rename(&origin, &moved).unwrap();
    let (status, _) = call(&app, "POST", sync).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let (status, _) = call(&app, "POST", sync).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let sent = recorder.wait_for(2).await;
    let m = &sent[1];
    assert_eq!(m["event"], "pull_failed");
    assert_eq!(m["consecutive_failures"], 1);
    assert!(m["errors"][0].as_str().unwrap().contains("git"), "{m}");
    assert!(m["revision"].is_string(), "the checkout keeps its commit");

    // Back: pull_recovered, once.
    std::fs::rename(&moved, &origin).unwrap();
    let (status, _) = call(&app, "POST", sync).await;
    assert_eq!(status, StatusCode::OK);
    let sent = recorder.wait_for(3).await;
    assert_eq!(sent[2]["event"], "pull_recovered");
    assert!(sent[2]["failing_seconds"].is_u64());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(recorder.sent.lock().unwrap().len(), 3);

    // The history records the project, and the API lists the selection.
    let (_, detail) = call(&app, "GET", "/api/v1/webhooks/hook").await;
    assert_eq!(detail["projects"], serde_json::json!(["prj-state"]));
    assert_eq!(
        detail["project_applications"],
        serde_json::json!(["app-demo", "app-other"])
    );
    assert_eq!(detail["history"][0]["project"], "prj-state");
    assert_eq!(detail["history"][0]["application"], Value::Null);
}
