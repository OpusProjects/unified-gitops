use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tower::ServiceExt;

use unified_gitops::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};

// The whole loop through the real router and the real example connector
// (tests/connectors/kv.py, a key-value store kept in a folder): desired state in a
// checkout, a check that reports what is out of sync, a sync that applies it,
// the local copy of the system's settings, and drift made by hand.

struct Fixture {
    app: axum::Router,
    store: PathBuf,
    desired: PathBuf,
    state: PathBuf,
    _dirs: Vec<tempfile::TempDir>,
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

fn fixture(prune: bool) -> Fixture {
    fixture_with(prune, "tests/connectors/kv.py")
}

fn fixture_with(prune: bool, script: &str) -> Fixture {
    let projects = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let desired = projects.path().join("prj-state/apps/demo");
    write(
        &desired.join("namespaces.yaml"),
        "kind: namespace\nitems:\n  - name: team-a\n    description: Team A\n",
    );
    write(
        &desired.join("entries.yaml"),
        "kind: entry\nitems:\n  - name: timeout\n    namespace: team-a\n    value: \"0030\"\n  - name: retries\n    namespace: team-a\n    value: 3\n",
    );

    let connector: unified_gitops::domain::connector::Connector =
        serde_saphyr::from_str(&format!("path: {}/{script}\n", env!("CARGO_MANIFEST_DIR")))
            .unwrap();
    // Both applications reach the same store: one system, two owners.
    let app_def = |prune: bool| -> unified_gitops::domain::app::Application {
        serde_saphyr::from_str(&format!(
            "description: Demo\nconnector: kv\nconfig:\n  dir: {}\nproject: prj-state\npath: apps/demo\nprune: {prune}\n",
            store.path().display()
        ))
        .unwrap()
    };
    let project: unified_gitops::domain::project::GitProject =
        serde_saphyr::from_str("git_url: https://example.com/state.git\n").unwrap();

    let restricted = ResolvedApiKey {
        id: "other".into(),
        name: "other".into(),
        secret: "other-secret".into(),
        permissions: Permissions::Scoped {
            applications: HashSet::from(["app-other".to_string()]),
        },
    };
    let app = unified_gitops::AppBuilder::new()
        .applications(HashMap::from([
            ("app-demo".to_string(), app_def(prune)),
            ("app-other".to_string(), app_def(false)),
        ]))
        .connectors(HashMap::from([("kv".to_string(), connector)]))
        .projects(
            HashMap::from([("prj-state".to_string(), project)]),
            projects.path().to_path_buf(),
        )
        .state_dir(state.path().to_path_buf())
        .api_key(Some("admin-secret".into()))
        .api_keys(vec![
            ResolvedApiKey {
                id: "admin".into(),
                name: "admin".into(),
                secret: "admin-secret".into(),
                permissions: Permissions::Admin,
            },
            restricted,
        ])
        .build();

    Fixture {
        app,
        store: store.path().to_path_buf(),
        desired,
        state: state.path().to_path_buf(),
        _dirs: vec![projects, store, state],
    }
}

async fn call(app: &axum::Router, method: &str, uri: &str, key: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", key)
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

fn store_file(f: &Fixture, kind: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(f.store.join(format!("{kind}.json"))).unwrap())
        .unwrap()
}

#[tokio::test]
async fn refresh_reports_sync_applies_and_drift_is_noticed() {
    let f = fixture(false);

    // A check compares and changes nothing.
    let (status, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/refresh",
        "admin-secret",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{run}");
    assert_eq!(run["result"], "out_of_sync");
    assert_eq!(run["planned"], 3);
    assert!(
        !f.store.join("entry.json").exists(),
        "a refresh must not write"
    );

    let (_, detail) = call(
        &f.app,
        "GET",
        "/api/v1/applications/app-demo",
        "admin-secret",
    )
    .await;
    assert_eq!(detail["status"], "out_of_sync");
    let ops = detail["plan"]["operations"].as_array().unwrap();
    assert!(ops.iter().all(|o| o["op"] == "create"));

    // Sync applies — the namespace before the entries that need it (the
    // connector refuses an entry whose namespace does not exist yet).
    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(run["applied"].as_array().unwrap().len(), 3);
    assert_eq!(run["applied"][0]["kind"], "namespace");
    assert_eq!(store_file(&f, "entry")["timeout"]["value"], "0030");

    let (_, detail) = call(
        &f.app,
        "GET",
        "/api/v1/applications/app-demo",
        "admin-secret",
    )
    .await;
    assert_eq!(detail["status"], "synced");
    assert_eq!(detail["pending_operations"], 0);
    assert_eq!(detail["history"].as_array().unwrap().len(), 2);
    // Each run keeps what it found, so its changes can be read later: the
    // sync (newest) set out to apply the same three operations it applied.
    let sync = &detail["history"][0];
    assert_eq!(sync["operations"].as_array().unwrap().len(), 3, "{sync}");
    assert_eq!(sync["operations"][0]["kind"], "namespace");
    // Each change names the file in git that declares it.
    let file = sync["operations"][0]["file"].as_str().unwrap_or_default();
    assert!(file.ends_with("/namespaces.yaml"), "{file}");

    // The local copy of the system's settings, in the desired-state format.
    let copy = std::fs::read_to_string(f.state.join("app-demo/entry.yaml")).unwrap();
    assert!(
        copy.contains("kind: entry") && copy.contains("timeout") && !copy.contains("target"),
        "{copy}"
    );
    let (status, snapshot) = call(
        &f.app,
        "GET",
        "/api/v1/applications/app-demo/state/entry",
        "admin-secret",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 2);

    // Someone changes the system by hand, and adds an object git knows nothing of.
    let mut entries = store_file(&f, "entry");
    entries["retries"]["value"] = json!(9);
    entries["manual"] = json!({"name": "manual", "namespace": "team-a", "value": 1});
    std::fs::write(f.store.join("entry.json"), entries.to_string()).unwrap();

    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/refresh",
        "admin-secret",
    )
    .await;
    assert_eq!(run["result"], "out_of_sync");
    let (_, detail) = call(
        &f.app,
        "GET",
        "/api/v1/applications/app-demo",
        "admin-secret",
    )
    .await;
    let op = &detail["plan"]["operations"][0];
    assert_eq!(op["op"], "update");
    assert_eq!(
        op["changes"][0],
        json!({"field": "value", "from": 9, "to": 3})
    );
    // Prune is off: the hand-made object is reported, never deleted.
    assert_eq!(detail["plan"]["unmanaged"][0]["key"], "manual");

    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    assert_eq!(run["result"], "succeeded");
    assert_eq!(store_file(&f, "entry")["retries"]["value"], 3);
    assert!(store_file(&f, "entry").get("manual").is_some());
}

#[tokio::test]
async fn prune_deletes_what_git_no_longer_declares() {
    let f = fixture(true);
    call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;

    // Remove one entry from git.
    write(
        &f.desired.join("entries.yaml"),
        "kind: entry\nitems:\n  - name: timeout\n    namespace: team-a\n    value: \"0030\"\n",
    );
    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    assert_eq!(run["result"], "succeeded", "{run}");
    assert_eq!(run["applied"][0]["op"], "delete");
    assert!(store_file(&f, "entry").get("retries").is_none());
}

// A kind that cannot be listed this run may still have objects: the kinds it
// depends on must not be deleted from under them.
#[tokio::test]
async fn a_kind_that_cannot_be_listed_keeps_what_it_depends_on() {
    let f = fixture(true);
    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    assert_eq!(run["result"], "succeeded", "{run}");

    // git removes the namespace and its entries, while listing entries fails.
    write(
        &f.desired.join("namespaces.yaml"),
        "kind: namespace\nitems: []\n",
    );
    write(&f.desired.join("entries.yaml"), "kind: entry\nitems: []\n");
    std::fs::write(f.store.join("entry.json"), "{not json").unwrap();

    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    assert_ne!(run["result"], "succeeded", "{run}");
    let applied = run["applied"].as_array().unwrap();
    assert_eq!(applied.len(), 1, "{run}");
    assert_eq!(applied[0]["kind"], "namespace");
    assert_eq!(applied[0]["ok"], false);
    assert!(
        applied[0]["error"]
            .as_str()
            .unwrap()
            .contains("'entry' could not be listed"),
        "{run}"
    );
    assert!(
        store_file(&f, "namespace").get("team-a").is_some(),
        "the namespace must stay while its entries are unknown"
    );
}

// A client that goes away mid-sync (a proxy timeout, Ctrl-C) must not cut the
// run short: it finishes, and its history entry is recorded.
#[tokio::test]
async fn a_sync_finishes_when_the_client_goes_away() {
    let f = fixture_with(false, "tests/connectors/slow_kv.py");
    let request = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    );
    // Each apply takes a second: the request is dropped while it writes.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), request)
            .await
            .is_err(),
        "the sync should still be running"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let detail = loop {
        let (_, detail) = call(
            &f.app,
            "GET",
            "/api/v1/applications/app-demo",
            "admin-secret",
        )
        .await;
        if !detail["history"].as_array().unwrap().is_empty() {
            break detail;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sync never finished"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(detail["history"][0]["result"], "succeeded", "{detail}");
    assert_eq!(detail["history"][0]["applied"].as_array().unwrap().len(), 3);
    assert_eq!(store_file(&f, "entry")["timeout"]["value"], "0030");
}

#[tokio::test]
async fn invalid_desired_state_touches_nothing() {
    let f = fixture(false);
    write(
        &f.desired.join("broken.yaml"),
        "kind: entry\nitems:\n  - value: no key\n",
    );
    let (_, run) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    assert_eq!(run["result"], "failed");
    assert!(
        run["errors"][0].as_str().unwrap().contains("broken.yaml"),
        "{run}"
    );
    assert!(
        !f.store.join("namespace.json").exists(),
        "nothing may be applied"
    );

    let (_, detail) = call(
        &f.app,
        "GET",
        "/api/v1/applications/app-demo",
        "admin-secret",
    )
    .await;
    assert_eq!(detail["status"], "failed");
    assert_eq!(detail["sync_health"]["consecutive_failures"], 1);
}

#[tokio::test]
async fn a_restricted_key_sees_and_syncs_only_its_applications() {
    let f = fixture(false);
    let (_, list) = call(&f.app, "GET", "/api/v1/applications", "other-secret").await;
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["app-other"]);

    let (status, _) = call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "other-secret",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // Nor read the other application's copy of its system.
    let (status, _) = call(
        &f.app,
        "GET",
        "/api/v1/applications/app-demo/state/entry",
        "other-secret",
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&f.app, "GET", "/api/v1/connectors", "other-secret").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(!f.store.join("namespace.json").exists());
}

#[tokio::test]
async fn calls_to_the_system_are_counted_on_metrics() {
    let f = fixture(false);
    // Touch /metrics first: the recorder is installed on first use, and a
    // counter incremented before it exists is not kept.
    let _ = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    let resp = f
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    for command in ["describe", "list", "apply"] {
        let line = text
            .lines()
            .find(|l| {
                l.starts_with("unified_gitops_connector_calls_total")
                    && l.contains("application=\"app-demo\"")
                    && l.contains(&format!("command=\"{command}\""))
                    && l.contains("outcome=\"success\"")
            })
            .unwrap_or_else(|| panic!("no {command} line in:\n{text}"));
        let count: f64 = line.rsplit(' ').next().unwrap().parse().unwrap();
        assert!(count >= 1.0, "{line}");
    }
    assert!(text.contains("unified_gitops_connector_call_duration_seconds_bucket"));
}

#[tokio::test]
async fn objects_are_found_by_key_or_value_with_who_owns_them() {
    let f = fixture(false);
    call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/sync",
        "admin-secret",
    )
    .await;
    // Someone adds an object by hand; the next check lists it as unmanaged.
    let mut entries = store_file(&f, "entry");
    entries["manual"] = json!({"name": "manual", "namespace": "team-a", "value": "0030-x"});
    std::fs::write(f.store.join("entry.json"), entries.to_string()).unwrap();
    call(
        &f.app,
        "POST",
        "/api/v1/applications/app-demo/refresh",
        "admin-secret",
    )
    .await;

    // By key.
    let (status, found) = call(&f.app, "GET", "/api/v1/search?q=TIME", "admin-secret").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(found[0]["application"], "app-demo");
    assert_eq!(found[0]["kind"], "entry");
    assert_eq!(found[0]["key"], "timeout");
    assert_eq!(found[0]["managed"], true);
    assert!(found[0].get("field").is_none());

    // By value: "0030" is the value of timeout, and inside manual's.
    let (_, found) = call(&f.app, "GET", "/api/v1/search?q=0030", "admin-secret").await;
    let keys: Vec<(&str, &str, bool)> = found
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["key"].as_str().unwrap(),
                o["field"].as_str().unwrap(),
                o["managed"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        vec![("manual", "value", false), ("timeout", "value", true)]
    );

    // A restricted key finds nothing on applications it may not see.
    let (_, found) = call(&f.app, "GET", "/api/v1/search?q=timeout", "other-secret").await;
    assert_eq!(found, json!([]));
    let (status, _) = call(&f.app, "GET", "/api/v1/search?q=%20", "admin-secret").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
