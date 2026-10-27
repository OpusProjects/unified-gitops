// Integration tests for the git CLI adapter. They create real repositories
// in temp directories and talk to them over the file:// protocol — no
// network, but the same code paths as an https remote (minus auth).
use std::collections::HashMap;
use std::path::Path;

use unified_gitops::adapters::out::git::cli::CliGit;
use unified_gitops::domain::project::GitProject;
use unified_gitops::ports::git::GitPort;

async fn run(dir: &Path, args: &[&str]) {
    let status = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // Identity for the test commits — independent of the host's gitconfig
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .await
        .expect("failed to run git");
    assert!(status.success(), "git {:?} failed", args);
}

// Creates an origin repository with one committed script and returns
// (tempdir guard, file:// url)
async fn make_origin() -> (tempfile::TempDir, String) {
    let origin = tempfile::tempdir().unwrap();
    run(origin.path(), &["init", "--initial-branch", "main"]).await;
    tokio::fs::write(origin.path().join("fetch.py"), "print('v1')\n")
        .await
        .unwrap();
    run(origin.path(), &["add", "."]).await;
    run(origin.path(), &["commit", "-m", "v1"]).await;
    let url = format!("file://{}", origin.path().display());
    (origin, url)
}

fn project(url: &str) -> GitProject {
    // Building the domain type from YAML keeps the test honest about what
    // the config file actually looks like.
    serde_saphyr::from_str(&format!("git_url: \"{}\"\n", url)).unwrap()
}

#[tokio::test]
async fn ensure_clones_a_fresh_checkout() {
    let (_origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .expect("clone failed");

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('v1')\n");
}

#[tokio::test]
async fn ensure_updates_an_existing_checkout() {
    let (origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .expect("clone failed");

    // The origin moves forward...
    tokio::fs::write(origin.path().join("fetch.py"), "print('v2')\n")
        .await
        .unwrap();
    run(origin.path(), &["commit", "-am", "v2"]).await;

    // ...and a second ensure() brings the checkout to the new tip
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .expect("update failed");

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('v2')\n");
}

#[tokio::test]
async fn ensure_discards_local_modifications() {
    let (_origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .unwrap();

    // Someone (or something) edits the checkout in place — the next ensure
    // must restore the repository state, not carry the drift forward
    tokio::fs::write(checkout.join("fetch.py"), "tampered\n")
        .await
        .unwrap();

    git.ensure(&checkout, &project(&url), &HashMap::new())
        .await
        .unwrap();

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('v1')\n");
}

#[tokio::test]
async fn ensure_fails_cleanly_on_bad_url() {
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");

    let git = CliGit::new();
    let err = git
        .ensure(
            &checkout,
            &project("file:///nonexistent/repo.git"),
            &HashMap::new(),
        )
        .await
        .expect_err("expected clone to fail");
    assert!(err.message.contains("git clone"));
}

// A project pointed at another repository: the existing checkout follows the
// new git_url instead of pulling the one it was cloned from.
#[tokio::test]
async fn ensure_follows_a_changed_git_url() {
    let (_first, first_url) = make_origin().await;
    let second = tempfile::tempdir().unwrap();
    run(second.path(), &["init", "--initial-branch", "main"]).await;
    tokio::fs::write(second.path().join("fetch.py"), "print('moved')\n")
        .await
        .unwrap();
    run(second.path(), &["add", "."]).await;
    run(second.path(), &["commit", "-m", "moved"]).await;
    let second_url = format!("file://{}", second.path().display());

    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");
    let git = CliGit::new();
    git.ensure(&checkout, &project(&first_url), &HashMap::new())
        .await
        .unwrap();
    git.ensure(&checkout, &project(&second_url), &HashMap::new())
        .await
        .expect("pull from the new address");

    let content = tokio::fs::read_to_string(checkout.join("fetch.py"))
        .await
        .unwrap();
    assert_eq!(content, "print('moved')\n");
}

// A token written into git_url is on git's command line and in its stderr;
// neither may reach the error that is logged and shown on /api/v1/projects.
#[tokio::test]
async fn a_token_in_git_url_never_reaches_the_error() {
    let workdir = tempfile::tempdir().unwrap();
    let git = CliGit::new();
    // No repository there: the clone fails at once, with the address (token
    // included) on its command line.
    let url = "file://bot:s3cr3t-token@localhost/nonexistent/repo.git";
    let err = git
        .ensure(
            &workdir.path().join("prj-a"),
            &project(url),
            &HashMap::new(),
        )
        .await
        .expect_err("nothing to clone from");
    assert!(err.message.contains("git clone"), "{}", err.message);
    assert!(!err.message.contains("s3cr3t-token"), "{}", err.message);

    // And from a pull of an existing checkout.
    let (_origin, origin_url) = make_origin().await;
    let checkout = workdir.path().join("prj-b");
    git.ensure(&checkout, &project(&origin_url), &HashMap::new())
        .await
        .unwrap();
    let err = git
        .ensure(&checkout, &project(url), &HashMap::new())
        .await
        .expect_err("nothing to fetch from");
    assert!(!err.message.contains("s3cr3t-token"), "{}", err.message);
}

// =========================================================================
// HTTP routes: on-demand project sync (admin-only)
// =========================================================================

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use unified_gitops::adapters::r#in::http::auth::{Permissions, ResolvedApiKey};

fn keys() -> Vec<ResolvedApiKey> {
    vec![
        ResolvedApiKey {
            name: "admin".to_string(),
            secret: "adm".to_string(),
            permissions: Permissions::Admin,
        },
        ResolvedApiKey {
            name: "limited".to_string(),
            secret: "lim".to_string(),
            permissions: Permissions::Scoped {
                applications: std::collections::HashSet::new(),
            },
        },
    ]
}

async fn request(app: axum::Router, method: &str, path: &str, key: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("x-api-key", key)
        .body(axum::body::Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

#[tokio::test]
async fn post_project_sync_clones_on_demand() {
    let (_origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();

    let projects = [("prj-test".to_string(), project(&url))]
        .into_iter()
        .collect();
    let app = unified_gitops::AppBuilder::new()
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    // No checkout yet
    let (status, body) = request(app.clone(), "GET", "/api/v1/projects", "adm").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"checkout_present\":false"));
    assert!(
        !body.contains("\"revision\""),
        "no checkout, no commit: {body}"
    );

    // On-demand sync clones it
    let (status, body) =
        request(app.clone(), "POST", "/api/v1/projects/prj-test/sync", "adm").await;
    assert_eq!(status, StatusCode::OK, "body: {}", body);
    assert!(body.contains("\"success\":true"));
    assert!(workdir.path().join("prj-test/fetch.py").exists());

    let (_, body) = request(app, "GET", "/api/v1/projects", "adm").await;
    assert!(body.contains("\"checkout_present\":true"));
    // The commit the checkout is at: what this instance runs.
    let list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let revision = list[0]["revision"].as_str().unwrap_or_default();
    assert_eq!(revision.len(), 40, "{body}");
}

#[tokio::test]
async fn project_routes_are_admin_only() {
    let workdir = tempfile::tempdir().unwrap();
    let projects = [("prj-test".to_string(), project("file:///nowhere/repo.git"))]
        .into_iter()
        .collect();
    let app = unified_gitops::AppBuilder::new()
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    let (status, _) = request(app.clone(), "GET", "/api/v1/projects", "lim").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = request(app, "POST", "/api/v1/projects/prj-test/sync", "lim").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn project_sync_unknown_id_is_404_and_bad_repo_is_502() {
    let workdir = tempfile::tempdir().unwrap();
    let projects = [("prj-test".to_string(), project("file:///nowhere/repo.git"))]
        .into_iter()
        .collect();
    let app = unified_gitops::AppBuilder::new()
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    let (status, _) = request(
        app.clone(),
        "POST",
        "/api/v1/projects/prj-ghost/sync",
        "adm",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Configured but uncloneable: the failure is the upstream's, hence 502
    let (status, body) = request(app, "POST", "/api/v1/projects/prj-test/sync", "adm").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("\"success\":false"));
}

// What reads its scripts from a checkout: the list names them, so a project
// can be seen to be unused (safe to remove) or relied on.
#[tokio::test]
async fn a_project_lists_what_uses_it() {
    let workdir = tempfile::tempdir().unwrap();
    let projects = [
        ("prj-test".to_string(), project("file:///nowhere/repo.git")),
        ("prj-idle".to_string(), project("file:///nowhere/idle.git")),
    ]
    .into_iter()
    .collect();
    let app_def: unified_gitops::domain::app::Application =
        serde_saphyr::from_str("connector: kv\nproject: prj-test\npath: apps\n").unwrap();
    let app = unified_gitops::AppBuilder::new()
        .applications([("app-a".to_string(), app_def)].into_iter().collect())
        .projects(projects, workdir.path().to_path_buf())
        .api_keys(keys())
        .build();

    let (status, body) = request(app, "GET", "/api/v1/projects", "adm").await;
    assert_eq!(status, StatusCode::OK);
    let list: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
    let by_id = |id: &str| list.iter().find(|p| p["name"] == id).unwrap().clone();
    assert_eq!(
        by_id("prj-test")["used_by"]["applications"],
        serde_json::json!(["app-a"])
    );
    assert_eq!(
        by_id("prj-idle")["used_by"]["applications"],
        serde_json::json!([])
    );
    assert_eq!(by_id("prj-test")["timeout_seconds"], 300);
}

async fn head(dir: &Path) -> String {
    CliGit::new().revision(dir).await.expect("a revision")
}

async fn commit_file(origin: &Path, file: &str, body: &str, message: &str) {
    let path = origin.join(file);
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&path, body).await.unwrap();
    run(origin, &["add", "."]).await;
    run(origin, &["commit", "-m", message]).await;
}

#[tokio::test]
async fn commits_lists_what_a_shallow_pull_brought() {
    let (origin, url) = make_origin().await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");
    let git = CliGit::new();
    let project = project(&url);
    git.ensure(&checkout, &project, &HashMap::new())
        .await
        .unwrap();
    let from = head(&checkout).await;

    // Three commits land; the pull fetches only the tip.
    commit_file(origin.path(), "apps/a/x.yaml", "a: 1\n", "add a").await;
    commit_file(origin.path(), "apps/b/x.yaml", "b: 1\n", "add b").await;
    commit_file(origin.path(), "apps/a/x.yaml", "a: 2\n", "change a").await;
    git.ensure(&checkout, &project, &HashMap::new())
        .await
        .unwrap();
    let to = head(&checkout).await;

    let range = git
        .commits(&checkout, &project, &HashMap::new(), &from, &to, 20)
        .await
        .expect("commits");
    let messages: Vec<&str> = range.commits.iter().map(|c| c.message.as_str()).collect();
    assert_eq!(messages, ["change a", "add b", "add a"]);
    assert_eq!(range.commits[0].revision, to);
    assert_eq!(range.commits[0].author, "test");
    assert!(range.fetched, "the shallow checkout needed deepening");
    assert!(!range.more && !range.forced);
    let mut paths = range.paths.expect("paths");
    paths.sort();
    assert_eq!(paths, ["apps/a/x.yaml", "apps/b/x.yaml"]);

    // A cap shorter than what came in says there is more.
    let capped = git
        .commits(&checkout, &project, &HashMap::new(), &from, &to, 2)
        .await
        .unwrap();
    assert_eq!(capped.commits.len(), 2);
    assert!(capped.more);
}

#[tokio::test]
async fn commits_tells_a_rewritten_history() {
    let (origin, url) = make_origin().await;
    commit_file(origin.path(), "apps/a/x.yaml", "a: 1\n", "add a").await;
    let workdir = tempfile::tempdir().unwrap();
    let checkout = workdir.path().join("prj-test");
    let git = CliGit::new();
    let project = project(&url);
    git.ensure(&checkout, &project, &HashMap::new())
        .await
        .unwrap();
    let from = head(&checkout).await;

    // The last commit is replaced (amend + force push, as far as a clone
    // can tell).
    tokio::fs::write(origin.path().join("apps/a/x.yaml"), "a: 9\n")
        .await
        .unwrap();
    run(
        origin.path(),
        &["commit", "-a", "--amend", "-m", "add a, again"],
    )
    .await;
    git.ensure(&checkout, &project, &HashMap::new())
        .await
        .unwrap();
    let to = head(&checkout).await;
    assert_ne!(from, to);

    let range = git
        .commits(&checkout, &project, &HashMap::new(), &from, &to, 20)
        .await
        .unwrap();
    assert!(range.forced, "{range:?}");
    assert_eq!(range.commits[0].message, "add a, again");
    assert_eq!(range.paths.unwrap(), ["apps/a/x.yaml"]);
}

#[tokio::test]
async fn commits_refuses_what_is_not_a_commit_id() {
    let git = CliGit::new();
    let project = project("file:///nowhere");
    let dir = tempfile::tempdir().unwrap();
    let err = git
        .commits(dir.path(), &project, &HashMap::new(), "--all", "abcd", 20)
        .await
        .unwrap_err();
    assert!(err.message.contains("not a commit id"), "{}", err.message);
}
