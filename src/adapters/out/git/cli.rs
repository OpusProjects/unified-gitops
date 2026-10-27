use std::collections::HashMap;
use std::path::Path;

use base64::Engine;
use tokio::process::Command;
use tracing::{debug, info};

use crate::domain::project::GitProject;
use crate::ports::git::{GitError, GitFuture, GitPort};

// GitPort implementation that shells out to the git binary — the same
// philosophy as the process connector: the tool everyone debugs with is the
// tool the app uses, so `git -C <dir> log` always tells the truth.
pub struct CliGit;

impl Default for CliGit {
    fn default() -> Self {
        Self::new()
    }
}

impl CliGit {
    pub fn new() -> Self {
        Self
    }
}

impl GitPort for CliGit {
    fn ensure(
        &self,
        dir: &Path,
        project: &GitProject,
        credentials: &HashMap<String, String>,
    ) -> GitFuture<'_> {
        let dir = dir.to_path_buf();
        let project = project.clone();
        let credentials = credentials.clone();

        Box::pin(async move {
            let envs = auth_env(&credentials);

            if dir.join(".git").exists() {
                // Existing checkout: fetch the branch tip and hard-reset to it.
                // FETCH_HEAD instead of origin/<branch> because shallow clones
                // don't always keep remote-tracking refs up to date.
                debug!(dir = %dir.display(), "Updating git checkout");
                // The remote is set from the configuration on every pull:
                // `origin` was fixed at clone time, so a project whose git_url
                // changed (the repository moved, or now points elsewhere)
                // would otherwise go on reading the old one without a word.
                run_git(
                    &[
                        "-C",
                        path_str(&dir)?,
                        "remote",
                        "set-url",
                        "origin",
                        &project.git_url,
                    ],
                    &envs,
                )
                .await?;
                run_git(
                    &[
                        "-C",
                        path_str(&dir)?,
                        "fetch",
                        "--depth",
                        "1",
                        "origin",
                        &project.branch,
                    ],
                    &envs,
                )
                .await?;
                run_git(
                    &["-C", path_str(&dir)?, "reset", "--hard", "FETCH_HEAD"],
                    &envs,
                )
                .await?;
            } else {
                if let Some(parent) = dir.parent() {
                    tokio::fs::create_dir_all(parent)
                        .await
                        .map_err(|e| GitError {
                            message: format!("create '{}': {}", parent.display(), e),
                        })?;
                }
                info!(url = %crate::domain::project::without_userinfo(&project.git_url), dir = %dir.display(), branch = %project.branch, "Cloning project");
                // --depth 1: connector scripts only need the tip, not history
                run_git(
                    &[
                        "clone",
                        "--depth",
                        "1",
                        "--branch",
                        &project.branch,
                        &project.git_url,
                        path_str(&dir)?,
                    ],
                    &envs,
                )
                .await?;
            }

            Ok(())
        })
    }

    fn revision(&self, dir: &Path) -> crate::ports::git::RevisionFuture<'_> {
        let dir = dir.to_path_buf();
        Box::pin(async move {
            let mut cmd = Command::new("git");
            cmd.args(["-C", dir.to_str()?, "rev-parse", "HEAD"]);
            cmd.env("GIT_TERMINAL_PROMPT", "0");
            cmd.kill_on_drop(true);
            let output = cmd.output().await.ok()?;
            if !output.status.success() {
                return None;
            }
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!sha.is_empty()).then_some(sha)
        })
    }

    fn tree(&self, dir: &Path, folder: &str) -> crate::ports::git::RevisionFuture<'_> {
        let dir = dir.to_path_buf();
        // `HEAD:` alone names the root tree.
        let spec = format!("HEAD:{}", if folder == "." { "" } else { folder });
        Box::pin(async move {
            let mut cmd = Command::new("git");
            cmd.args(["-C", dir.to_str()?, "rev-parse", &spec]);
            cmd.env("GIT_TERMINAL_PROMPT", "0");
            cmd.kill_on_drop(true);
            let output = cmd.output().await.ok()?;
            if !output.status.success() {
                return None;
            }
            let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (!id.is_empty()).then_some(id)
        })
    }

    // `from` was HEAD before the pull and is still in the object store, so
    // the paths between the two are a local diff. The commits are not: the
    // pull fetched only the tip, so when `from` is not yet an ancestor of
    // `to` the history is deepened once by `max` commits, never unshallowed
    // (a repository's whole history is not worth one message).
    #[allow(clippy::too_many_arguments)]
    fn commits<'a>(
        &'a self,
        dir: &'a Path,
        project: &'a GitProject,
        credentials: &'a HashMap<String, String>,
        from: &'a str,
        to: &'a str,
        max: usize,
    ) -> crate::ports::git::CommitsFuture<'a> {
        Box::pin(async move {
            use crate::ports::git::{Commit, CommitRange};
            for revision in [from, to] {
                if !(4..=64).contains(&revision.len())
                    || !revision.chars().all(|c| c.is_ascii_hexdigit())
                {
                    return Err(GitError {
                        message: format!("'{revision}' is not a commit id"),
                    });
                }
            }
            let dir_s = path_str(dir)?;
            let local = |args: Vec<String>| async move {
                Command::new("git")
                    .arg("-C")
                    .arg(dir_s)
                    .args(args)
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|e| GitError {
                        message: format!("git: {e}"),
                    })
            };
            let is_ancestor = || async {
                local(vec![
                    "merge-base".into(),
                    "--is-ancestor".into(),
                    from.into(),
                    to.into(),
                ])
                .await
                .map(|o| o.status.success())
            };
            let shallow = || async {
                local(vec!["rev-parse".into(), "--is-shallow-repository".into()])
                    .await
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
            };

            let mut range = CommitRange::default();
            let mut ancestor = is_ancestor().await?;
            if !ancestor && shallow().await? {
                let depth = format!("--deepen={max}");
                run_git(
                    &[
                        "-C",
                        dir_s,
                        "fetch",
                        "--quiet",
                        &depth,
                        "origin",
                        &project.branch,
                    ],
                    &auth_env(credentials),
                )
                .await?;
                range.fetched = true;
                ancestor = is_ancestor().await?;
            }
            // Fields split by the unit separator, which no name or subject
            // carries; one commit per line.
            let mut log_args: Vec<String> =
                vec!["log".into(), "--format=%H%x1f%an%x1f%ct%x1f%s".into()];
            if ancestor {
                log_args.extend([format!("-n{}", max + 1), to.into(), format!("^{from}")]);
            } else {
                // A rewrite, or a history deeper than `max`. `from` was a
                // shallow tip, its parents unknown, so `^from` would not
                // exclude the history both share: only the new tip is
                // certain. It is a rewrite for sure only when the new history
                // was there whole (no shallow boundary behind `to`) and
                // `from` is not in it.
                log_args.extend(["-n1".into(), to.into()]);
                range.more = true;
                let git_path = local(vec![
                    "rev-parse".into(),
                    "--git-path".into(),
                    "shallow".into(),
                ])
                .await?;
                let shallow_file = dir.join(String::from_utf8_lossy(&git_path.stdout).trim());
                let mut whole = true;
                if let Ok(boundaries) = tokio::fs::read_to_string(&shallow_file).await {
                    for boundary in boundaries.lines().filter(|l| !l.is_empty()) {
                        let behind = local(vec![
                            "merge-base".into(),
                            "--is-ancestor".into(),
                            boundary.into(),
                            to.into(),
                        ])
                        .await?;
                        if behind.status.success() {
                            whole = false;
                            break;
                        }
                    }
                }
                range.forced = whole;
            }
            let log = local(log_args).await?;
            if log.status.success() {
                for line in String::from_utf8_lossy(&log.stdout).lines() {
                    let mut fields = line.splitn(4, '\u{1f}');
                    let (Some(revision), Some(author), Some(at), Some(message)) =
                        (fields.next(), fields.next(), fields.next(), fields.next())
                    else {
                        continue;
                    };
                    range.commits.push(Commit {
                        revision: revision.to_string(),
                        author: author.to_string(),
                        message: message.to_string(),
                        at: at.parse().unwrap_or(0),
                    });
                }
            }
            if range.commits.len() > max {
                range.commits.truncate(max);
                range.more = true;
            }

            let diff = local(vec![
                "diff".into(),
                "--name-only".into(),
                "-z".into(),
                from.into(),
                to.into(),
            ])
            .await?;
            if diff.status.success() {
                range.paths = Some(
                    String::from_utf8_lossy(&diff.stdout)
                        .split('\0')
                        .filter(|p| !p.is_empty())
                        .map(str::to_string)
                        .collect(),
                );
            }
            Ok(range)
        })
    }

    // `git archive` streams the tree at that commit straight from the object
    // store; `tar` unpacks it. Nothing in the checkout moves, so a pull
    // running at the same time cannot be confused by it, nor it by the pull.
    fn export<'a>(
        &'a self,
        dir: &'a Path,
        project: &'a GitProject,
        credentials: &'a HashMap<String, String>,
        revision: &'a str,
        path: &'a str,
        dest: &'a Path,
    ) -> crate::ports::git::ExportFuture<'a> {
        Box::pin(async move {
            let fail = |message: String| GitError { message };
            // A commit id, never an option or a ref expression: it is user
            // input, and it ends up on a command line.
            if !(4..=64).contains(&revision.len())
                || !revision.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Err(fail(format!("'{revision}' is not a commit id")));
            }
            let dir_s = path_str(dir)?;
            let resolve = || async {
                Command::new("git")
                    .args(["-C", dir_s, "rev-parse", "--verify", "--quiet"])
                    .arg(format!("{revision}^{{commit}}"))
                    .env("GIT_TERMINAL_PROMPT", "0")
                    .kill_on_drop(true)
                    .output()
                    .await
                    .map_err(|e| fail(format!("git rev-parse: {e}")))
            };
            let mut resolved = resolve().await?;
            if !resolved.status.success() {
                // The checkout is shallow (pulls fetch only the tip): fetch
                // the branch's history once, then look again.
                let shallow = Command::new("git")
                    .args(["-C", dir_s, "rev-parse", "--is-shallow-repository"])
                    .output()
                    .await
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
                    .unwrap_or(false);
                let mut args = vec!["-C", dir_s, "fetch", "--quiet"];
                if shallow {
                    args.push("--unshallow");
                }
                args.extend(["origin", project.branch.as_str()]);
                run_git(&args, &auth_env(credentials)).await?;
                resolved = resolve().await?;
            }
            if !resolved.status.success() {
                return Err(fail(format!(
                    "commit {revision} is not in the project's history"
                )));
            }
            let sha = String::from_utf8_lossy(&resolved.stdout).trim().to_string();

            let archive = Command::new("git")
                .args(["-C", dir_s, "archive", "--format=tar", &sha, "--", path])
                .env("GIT_TERMINAL_PROMPT", "0")
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| fail(format!("git archive: {e}")))?;
            if !archive.status.success() {
                return Err(fail(format!(
                    "git archive {sha} -- {path}: {}",
                    String::from_utf8_lossy(&archive.stderr).trim()
                )));
            }

            std::fs::create_dir_all(dest).map_err(|e| fail(format!("{}: {e}", dest.display())))?;
            let mut tar = Command::new("tar")
                .args(["-x", "-C", path_str(dest)?])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| fail(format!("tar: {e}")))?;
            {
                use tokio::io::AsyncWriteExt;
                let mut stdin = tar
                    .stdin
                    .take()
                    .ok_or_else(|| fail("tar: no stdin".into()))?;
                stdin
                    .write_all(&archive.stdout)
                    .await
                    .map_err(|e| fail(format!("tar: {e}")))?;
            }
            let out = tar
                .wait_with_output()
                .await
                .map_err(|e| fail(format!("tar: {e}")))?;
            if !out.status.success() {
                return Err(fail(format!(
                    "tar: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(sha)
        })
    }
}

fn path_str(path: &Path) -> Result<&str, GitError> {
    path.to_str().ok_or_else(|| GitError {
        message: format!("path '{}' is not valid UTF-8", path.display()),
    })
}

// Translate resolved credentials into git authentication, WITHOUT putting the
// secret on the command line (argv is world-readable in /proc while git runs):
//
// - `ssh_key_path` (an SshKey credential's file_keys) → GIT_SSH_COMMAND
// - `token` or `username`+`password` → an http.extraHeader Basic auth header,
//   passed through GIT_CONFIG_* environment variables (git ≥ 2.31)
//
// Empty credentials = no env = anonymous access (public repos).
fn auth_env(credentials: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut envs: Vec<(String, String)> = Vec::new();

    if let Some(key_path) = credentials.get("ssh_key_path") {
        envs.push((
            "GIT_SSH_COMMAND".to_string(),
            format!(
                "ssh -i {} -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new",
                key_path
            ),
        ));
        return envs;
    }

    // Token providers accept Basic auth with the token as password; the
    // username is mostly decorative (GitHub docs use x-access-token).
    let basic = if let Some(token) = credentials.get("token") {
        let user = credentials
            .get("username")
            .map(String::as_str)
            .unwrap_or("x-access-token");
        Some(format!("{}:{}", user, token))
    } else if let (Some(user), Some(password)) =
        (credentials.get("username"), credentials.get("password"))
    {
        Some(format!("{}:{}", user, password))
    } else {
        None
    };

    if let Some(userpass) = basic {
        let header = format!(
            "Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode(userpass)
        );
        envs.push(("GIT_CONFIG_COUNT".to_string(), "1".to_string()));
        envs.push((
            "GIT_CONFIG_KEY_0".to_string(),
            "http.extraHeader".to_string(),
        ));
        envs.push(("GIT_CONFIG_VALUE_0".to_string(), header));
    }

    envs
}

async fn run_git(args: &[&str], envs: &[(String, String)]) -> Result<(), GitError> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    for (key, value) in envs {
        cmd.env(key, value);
    }
    // Never let git fall back to an interactive credential prompt — in a
    // container there is no terminal and the process would just hang.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // The caller bounds `ensure` with the project's timeout_seconds; dropping
    // this future must KILL the git child rather than abandon it, or a hung
    // remote would accumulate a live git process per attempt.
    cmd.kill_on_drop(true);

    let output = cmd.output().await.map_err(|e| GitError {
        message: format!("failed to run git {:?}: {}", args.first().unwrap_or(&""), e),
    })?;

    if !output.status.success() {
        // A git_url may carry a token (https://bot:<token>@host/...): it is on
        // the command line and git quotes it back in stderr, and this message
        // ends up in the logs and in the project's sync_health on the API.
        return Err(GitError {
            message: crate::domain::project::redact_userinfo(&format!(
                "git {} failed ({}): {}",
                args.join(" "),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_credentials_means_no_env() {
        assert!(auth_env(&HashMap::new()).is_empty());
    }

    #[test]
    fn ssh_key_sets_git_ssh_command() {
        let creds: HashMap<String, String> =
            [("ssh_key_path".to_string(), "/run/secrets/key".to_string())]
                .into_iter()
                .collect();
        let envs = auth_env(&creds);
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "GIT_SSH_COMMAND");
        assert!(envs[0].1.contains("-i /run/secrets/key"));
    }

    #[test]
    fn token_sets_basic_auth_header_via_env() {
        let creds: HashMap<String, String> = [("token".to_string(), "s3cr3t".to_string())]
            .into_iter()
            .collect();
        let envs = auth_env(&creds);
        let header = envs
            .iter()
            .find(|(k, _)| k == "GIT_CONFIG_VALUE_0")
            .map(|(_, v)| v.clone())
            .unwrap();
        // "x-access-token:s3cr3t" in base64
        assert_eq!(header, "Authorization: Basic eC1hY2Nlc3MtdG9rZW46czNjcjN0");
        // And the secret never appears in a command-line argument
        assert!(!envs.iter().any(|(_, v)| v.contains("s3cr3t")));
    }

    #[test]
    fn username_password_sets_basic_auth() {
        let creds: HashMap<String, String> = [
            ("username".to_string(), "batou".to_string()),
            ("password".to_string(), "rangiku".to_string()),
        ]
        .into_iter()
        .collect();
        let envs = auth_env(&creds);
        assert!(envs.iter().any(|(k, _)| k == "GIT_CONFIG_KEY_0"));
    }
}
