use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;
use tower::ServiceExt;

use unified_gitops::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};

// Rollback through the real router, the real git binary and the example
// connector: a remote with two commits, a sync to the newer one, a rollback to
// the older one, the pause it puts on checks, and the sync that ends it.

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-C", dir.to_str().unwrap()])
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn commit(remote: &Path, retries: u32) -> String {
    let file = remote.join("apps/demo/entries.yaml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(
        &file,
        format!(
            "kind: entry\nitems:\n  - name: retries\n    namespace: team-a\n    value: {retries}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        remote.join("apps/demo/namespaces.yaml"),
        "kind: namespace\nitems:\n  - name: team-a\n",
    )
    .unwrap();
    git(remote, &["add", "-A"]);
    git(remote, &["commit", "-qm", &format!("retries {retries}")]);
    git(remote, &["rev-parse", "HEAD"])
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    key: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-api-key", key)
        .header("accept", "application/json");
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
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
async fn rollback_applies_an_older_commit_and_pauses_until_the_next_sync() {
    let remote = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "-q", "-b", "main"]);
    let old = commit(remote.path(), 3);
    let new = commit(remote.path(), 5);

    let connector: unified_gitops::domain::connector::Connector = serde_saphyr::from_str(&format!(
        "path: {}/connectors/kv.py\n",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let app_def: unified_gitops::domain::app::Application = serde_saphyr::from_str(&format!(
        "connector: kv\nconfig:\n  dir: {}\nproject: prj\npath: apps/demo\nauto_sync: true\n",
        store.path().display()
    ))
    .unwrap();
    let project: unified_gitops::domain::project::GitProject =
        serde_saphyr::from_str(&format!("git_url: file://{}\n", remote.path().display())).unwrap();
    let app = unified_gitops::AppBuilder::new()
        .applications(HashMap::from([("demo".to_string(), app_def)]))
        .connectors(HashMap::from([("kv".to_string(), connector)]))
        .projects(
            HashMap::from([("prj".to_string(), project)]),
            projects.path().to_path_buf(),
        )
        .state_dir(state.path().to_path_buf())
        .api_key(Some("admin".into()))
        .api_keys(vec![
            ResolvedApiKey {
                name: "admin".into(),
                secret: "admin".into(),
                permissions: Permissions::Admin,
            },
            ResolvedApiKey {
                name: "other".into(),
                secret: "other".into(),
                permissions: Permissions::Scoped {
                    applications: HashSet::from(["elsewhere".to_string()]),
                },
            },
        ])
        .build();
    let retries = || -> Value {
        let text = std::fs::read_to_string(store.path().join("entry.json")).unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["retries"]["value"].clone()
    };

    // Sync to HEAD: retries 5.
    let (_, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/sync",
        "admin",
        None,
    )
    .await;
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(run["revision"], new.as_str());
    assert_eq!(retries(), json!(5));

    // Preview first: what the rollback would change, with nothing changed.
    let (status, plan) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/rollback/preview",
        "admin",
        Some(json!({"revision": &old[..8]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{plan}");
    assert_eq!(plan["revision"], old.as_str());
    let op = &plan["operations"][0];
    assert_eq!(op["op"], "update");
    assert_eq!(op["key"], "retries");
    assert_eq!(
        op["changes"][0],
        json!({"field": "value", "from": 5, "to": 3})
    );
    assert_eq!(retries(), json!(5));
    let (_, detail) = call(&app, "GET", "/api/v1/applications/demo", "admin", None).await;
    assert_eq!(detail["revision"], new.as_str(), "nothing recorded");
    assert!(detail.get("rollback").is_none());

    // Roll back to the older commit, by its short id: retries 3 again.
    let (status, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/rollback",
        "admin",
        Some(json!({"revision": &old[..8]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(run["trigger"], "rollback");
    assert_eq!(run["revision"], old.as_str());
    assert_eq!(retries(), json!(3));
    // The checkout itself stays at HEAD.
    assert_eq!(
        git(&projects.path().join("prj"), &["rev-parse", "HEAD"]),
        new
    );

    let (_, detail) = call(&app, "GET", "/api/v1/applications/demo", "admin", None).await;
    assert_eq!(detail["rollback"]["revision"], old.as_str());
    assert_eq!(detail["status"], "rolled_back");

    // A check compares with HEAD again: out of sync, and nothing applied.
    let (_, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/refresh",
        "admin",
        None,
    )
    .await;
    assert_eq!(run["result"], "out_of_sync", "{run}");
    assert_eq!(retries(), json!(3));
    // Still rolled back, which is why nothing was applied.
    let (_, detail) = call(&app, "GET", "/api/v1/applications/demo", "admin", None).await;
    assert_eq!(detail["status"], "rolled_back");
    // The pause is on disk, so a restart keeps it.
    assert!(state.path().join("demo/.rollback.json").exists());

    // A manual sync applies HEAD and ends the rollback.
    let (_, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/sync",
        "admin",
        None,
    )
    .await;
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(retries(), json!(5));
    let (_, detail) = call(&app, "GET", "/api/v1/applications/demo", "admin", None).await;
    assert!(detail.get("rollback").is_none(), "{detail}");
    assert_eq!(detail["status"], "synced");

    // Not a commit id, or not in the history: refused, nothing applied.
    for bad in ["--help", "zzzz", "deadbeefdeadbeef"] {
        let (status, body) = call(
            &app,
            "POST",
            "/api/v1/applications/demo/rollback",
            "admin",
            Some(json!({"revision": bad})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}: {body}");
    }
    assert_eq!(retries(), json!(5));

    // A key without the application may not roll it back.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/rollback",
        "other",
        Some(json!({"revision": &old[..8]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

// Desired state at the repository's root (`path: ""`), committed as is.
fn commit_root(remote: &Path, namespace: &str, retries: u32) -> String {
    std::fs::write(
        remote.join("entries.yaml"),
        format!(
            "kind: entry\nitems:\n  - name: retries\n    namespace: {namespace}\n    value: {retries}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        remote.join("namespaces.yaml"),
        "kind: namespace\nitems:\n  - name: team-a\n",
    )
    .unwrap();
    git(remote, &["add", "-A"]);
    git(
        remote,
        &["commit", "-qm", &format!("{namespace} {retries}")],
    );
    git(remote, &["rev-parse", "HEAD"])
}

#[tokio::test]
async fn failed_rollbacks_do_not_pause_and_checks_never_apply_over_one() {
    use unified_gitops::application::reconcile::pull_and_reconcile;
    use unified_gitops::domain::run::{RunMode, Trigger};

    let remote = tempfile::tempdir().unwrap();
    let projects = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let state_dir = tempfile::tempdir().unwrap();
    git(remote.path(), &["init", "-q", "-b", "main"]);
    let old = commit_root(remote.path(), "team-a", 3);
    // Every operation of this one fails: its namespace does not exist.
    let broken = commit_root(remote.path(), "team-z", 4);
    let new = commit_root(remote.path(), "team-a", 5);

    let connector: unified_gitops::domain::connector::Connector = serde_saphyr::from_str(&format!(
        "path: {}/connectors/kv.py\n",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    // `path: ""`: the repository's root, which a rollback must export too.
    let app_def: unified_gitops::domain::app::Application = serde_saphyr::from_str(&format!(
        "connector: kv\nconfig:\n  dir: {}\nproject: prj\npath: \"\"\nauto_sync: true\n",
        store.path().display()
    ))
    .unwrap();
    let project: unified_gitops::domain::project::GitProject =
        serde_saphyr::from_str(&format!("git_url: file://{}\n", remote.path().display())).unwrap();
    let (app, state) = unified_gitops::AppBuilder::new()
        .applications(HashMap::from([("demo".to_string(), app_def)]))
        .connectors(HashMap::from([("kv".to_string(), connector)]))
        .projects(
            HashMap::from([("prj".to_string(), project)]),
            projects.path().to_path_buf(),
        )
        .state_dir(state_dir.path().to_path_buf())
        .api_key(Some("admin".into()))
        .api_keys(vec![ResolvedApiKey {
            name: "admin".into(),
            secret: "admin".into(),
            permissions: Permissions::Admin,
        }])
        .build_with_state();
    let retries = || -> Value {
        let text = std::fs::read_to_string(store.path().join("entry.json")).unwrap();
        serde_json::from_str::<Value>(&text).unwrap()["retries"]["value"].clone()
    };

    let (_, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/sync",
        "admin",
        None,
    )
    .await;
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(run["revision"], new.as_str());

    // A rollback that changed nothing (every operation failed) is no reason
    // to pause the checks.
    let (status, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/rollback",
        "admin",
        Some(json!({"revision": broken})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["result"], "failed", "{run}");
    assert_eq!(retries(), json!(5));
    let (_, detail) = call(&app, "GET", "/api/v1/applications/demo", "admin", None).await;
    assert!(detail.get("rollback").is_none(), "{detail}");

    // An application at the repository's root rolls back like any other.
    let (status, run) = call(
        &app,
        "POST",
        "/api/v1/applications/demo/rollback",
        "admin",
        Some(json!({"revision": old})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(retries(), json!(3));

    // A scheduled check with auto_sync asks to apply; the run itself, under
    // the application's lock, sees the rollback and only compares.
    let config = state.config();
    let record = pull_and_reconcile(
        &state.reconciler(),
        &config.catalog(),
        "demo",
        RunMode::Apply,
        Trigger::Schedule,
    )
    .await
    .unwrap();
    assert_eq!(record.mode, RunMode::Plan);
    assert!(record.applied.is_empty());
    assert_eq!(
        retries(),
        json!(3),
        "HEAD must not be applied over the rollback"
    );
}
