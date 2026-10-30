use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

use unified_gitops::adapters::out::secrets::mock::MockSecrets;
use unified_gitops::ports::http_sender::{
    HttpSenderPort, OutboundRequest, OutboundResponse, SendFuture,
};

// Script items end to end: a watched repository and a scripts repository,
// both real over file://, and scripts that write what they were given to a
// file the test reads.

#[derive(Default)]
struct Recorder {
    sent: Mutex<Vec<Value>>,
}

impl HttpSenderPort for Recorder {
    fn send(&self, request: OutboundRequest) -> SendFuture<'_> {
        self.sent
            .lock()
            .unwrap()
            .push(serde_json::from_slice(&request.body).unwrap_or(Value::Null));
        Box::pin(async {
            Ok(OutboundResponse {
                status: 200,
                ..Default::default()
            })
        })
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

async fn repo(dir: &Path, files: &[(&str, &str, bool)]) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "--initial-branch", "main"]).await;
    commit(dir, files).await;
}

async fn commit(dir: &Path, files: &[(&str, &str, bool)]) {
    for (file, body, executable) in files {
        let path = dir.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        if *executable {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    git(dir, &["add", "-A"]).await;
    git(dir, &["commit", "-m", "change"]).await;
}

async fn call(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-api-key", "adm")
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

async fn until(what: &str, check: impl Fn() -> bool) {
    for _ in 0..100 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripts_run_with_what_changed() {
    let root = tempfile::tempdir().unwrap();
    let out = root.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let infra = root.path().join("infra");
    let ops = root.path().join("ops");
    repo(&infra, &[("dns/a.zone", "a\n", false)]).await;
    // One script runs itself; the other has no executable bit and needs an
    // interpreter.
    repo(
        &ops,
        &[
            (
                "reload.sh",
                &format!(
                    "#!/bin/sh\n{{ echo \"args=$*\"; echo \"item=$GITOPS_ITEM project=$GITOPS_PROJECT trigger=$GITOPS_TRIGGER\"; echo \"files=$(echo $GITOPS_CHANGED_FILES)\"; echo \"token=$CREDENTIAL_TOKEN zone=$ZONE\"; echo \"pwd=$(pwd)\"; echo \"stdin=$(cat)\"; echo \"secret_leak=$UNIFIED_GITOPS_PARENT_ONLY\"; }} > {}/reload.txt\necho reloaded\n",
                    out.display()
                ),
                true,
            ),
            (
                "each.sh",
                &format!(
                    "echo \"$GITOPS_FILE $GITOPS_FILE_STATUS $1\" >> {}/each.txt\n[ \"$GITOPS_FILE_STATUS\" = removed ] || [ \"$GITOPS_FILE\" != dns/bad.zone ]\n",
                    out.display()
                ),
                false,
            ),
            ("slow.sh", "#!/bin/sh\nsleep 30\n", true),
        ],
    )
    .await;
    // Something only this process has: it must not reach a script.
    unsafe { std::env::set_var("UNIFIED_GITOPS_PARENT_ONLY", "leaked") };

    let checkouts = tempfile::tempdir().unwrap();
    let project = |dir: &Path| -> unified_gitops::domain::project::GitProject {
        serde_saphyr::from_str(&format!("git_url: file://{}\n", dir.display())).unwrap()
    };
    let script = |yaml: &str| -> unified_gitops::domain::script::Script {
        serde_saphyr::from_str(yaml).unwrap()
    };
    let hook: unified_gitops::domain::webhook::Webhook = serde_saphyr::from_str(
        "project: infra\nevents: [script_succeeded, script_failed, script_recovered]\nwebhook:\n  url: https://hooks.example.com/x\n",
    )
    .unwrap();
    let recorder = Arc::new(Recorder::default());
    let (app, state) = unified_gitops::AppBuilder::new()
        .projects(
            HashMap::from([
                ("infra".to_string(), project(&infra)),
                ("ops".to_string(), project(&ops)),
            ]),
            checkouts.path().to_path_buf(),
        )
        .scripts(HashMap::from([
            (
                "reload".to_string(),
                script(
                    "project: infra\ncredential: cred-dns\ntrigger: {paths: [dns/**]}\nscript:\n  project: ops\n  file: reload.sh\n  args: [--zone, $event.project, $event.revision_short]\n  working_dir: dns\n  env: {ZONE: $event.project}\n  stdin: event\n",
                ),
            ),
            (
                "each".to_string(),
                script(
                    "project: infra\ntrigger: {paths: [dns/*.zone]}\non_failure: continue\nscript:\n  project: ops\n  file: each.sh\n  interpreter: sh\n  per: file\n  args: [$file.stem]\n",
                ),
            ),
            (
                "slow".to_string(),
                script(
                    "project: infra\ntimeout_seconds: 1\ntrigger: {paths: [never/**]}\nscript:\n  project: ops\n  file: slow.sh\n  kill_grace_seconds: 1\n",
                ),
            ),
            (
                "missing".to_string(),
                script("project: infra\ntrigger: {paths: [never/**]}\nscript:\n  file: nowhere.sh\n"),
            ),
        ]))
        .webhooks(HashMap::from([("hook".to_string(), hook)]))
        .http(recorder.clone())
        .secrets(Arc::new(MockSecrets::new().with("cred-dns", [("token", "t0k")])))
        .api_key(Some("adm".into()))
        .build_with_state();
    state.start_webhooks();
    for id in ["ops", "infra"] {
        let (status, body) = call(&app, "POST", &format!("/api/v1/projects/{id}/sync")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }

    // A commit to two zones: reload runs once with both, each runs per file.
    commit(
        &infra,
        &[("dns/a.zone", "a2\n", false), ("dns/b.zone", "b\n", false)],
    )
    .await;
    let (status, _) = call(&app, "POST", "/api/v1/projects/infra/sync").await;
    assert_eq!(status, StatusCode::OK);
    let reload = out.join("reload.txt");
    until("1", || reload.exists() && out.join("each.txt").exists()).await;
    until("2", || {
        state
            .runner
            .log
            .last(unified_gitops::domain::run::Kind::Script, "each")
            .is_some()
    })
    .await;
    let text = std::fs::read_to_string(&reload).unwrap();
    let revision = state
        .runner
        .log
        .last(unified_gitops::domain::run::Kind::Script, "reload")
        .unwrap()
        .revision
        .unwrap();
    for expected in [
        format!("args=--zone infra {}", &revision[..7]),
        "item=reload project=infra trigger=change".to_string(),
        "files=dns/a.zone dns/b.zone".to_string(),
        "token=t0k zone=infra".to_string(),
        format!("pwd={}", checkouts.path().join("infra/dns").display()),
        "\"project\":\"infra\"".to_string(),
        "secret_leak=\n".to_string(),
    ] {
        assert!(text.contains(&expected), "missing {expected:?} in {text}");
    }
    let each = std::fs::read_to_string(out.join("each.txt")).unwrap();
    assert_eq!(each, "dns/a.zone changed a\ndns/b.zone changed b\n");

    let (_, detail) = call(&app, "GET", "/api/v1/scripts/reload").await;
    let run = &detail["history"][0];
    assert_eq!(run["ok"], true, "{run}");
    assert_eq!(run["steps"][0]["exit_code"], 0);
    assert_eq!(run["steps"][0]["output"], "reloaded\n");
    assert!(run["script_revision"].is_string());
    assert_eq!(detail["script_project"], "ops");
    assert_eq!(detail["env"], json!(["ZONE"]));

    // A file that fails: the run fails, the others still run, the failure
    // is told; fixed, the recovery is.
    commit(
        &infra,
        &[("dns/bad.zone", "x\n", false), ("dns/c.zone", "c\n", false)],
    )
    .await;
    call(&app, "POST", "/api/v1/projects/infra/sync").await;
    until("3", || {
        std::fs::read_to_string(out.join("each.txt"))
            .unwrap()
            .contains("dns/c.zone")
    })
    .await;
    until("4", || {
        recorder
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|m| m["event"] == "script_failed" && m["item"] == "each")
    })
    .await;
    let failed = state
        .runner
        .log
        .last(unified_gitops::domain::run::Kind::Script, "each")
        .unwrap();
    assert!(!failed.ok);
    assert_eq!(failed.steps[0].error.as_deref(), Some("exit code 1"));
    assert!(failed.steps[1].ok, "on_failure: continue runs the rest");
    std::fs::remove_file(infra.join("dns/bad.zone")).unwrap();
    git(&infra, &["add", "-A"]).await;
    git(&infra, &["commit", "-m", "fix"]).await;
    call(&app, "POST", "/api/v1/projects/infra/sync").await;
    until("5", || {
        recorder
            .sent
            .lock()
            .unwrap()
            .iter()
            .any(|m| m["event"] == "script_recovered")
    })
    .await;

    // A dry run says what would run and runs nothing, without values.
    std::fs::remove_file(&reload).unwrap();
    let (status, run) = call(&app, "POST", "/api/v1/scripts/reload/run?dry_run=true").await;
    assert_eq!(status, StatusCode::OK, "{run}");
    let output = run["steps"][0]["output"].as_str().unwrap();
    assert!(output.starts_with("would run: "), "{output}");
    assert!(
        output.contains("CREDENTIAL_TOKEN") && !output.contains("t0k"),
        "{output}"
    );
    assert!(!reload.exists());

    // A run past its timeout is stopped; a file that is not there says so.
    let (_, run) = call(&app, "POST", "/api/v1/scripts/slow/run").await;
    assert_eq!(run["ok"], false);
    assert_eq!(run["steps"][0]["error"], "timed out after 1s");
    let (_, run) = call(&app, "POST", "/api/v1/scripts/missing/run").await;
    assert!(
        run["error"]
            .as_str()
            .unwrap()
            .contains("is not in project 'infra'"),
        "{run}"
    );

    let (_, list) = call(&app, "GET", "/api/v1/scripts").await;
    assert_eq!(list.as_array().unwrap().len(), 4);
    let (status, _) = call(&app, "GET", "/api/v1/scripts/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, projects) = call(&app, "GET", "/api/v1/projects").await;
    let infra_info = projects
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "infra")
        .unwrap();
    assert_eq!(
        infra_info["used_by"]["scripts"],
        json!(["each", "missing", "reload", "slow"])
    );
}
