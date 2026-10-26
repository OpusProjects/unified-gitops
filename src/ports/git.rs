use std::collections::HashMap;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use crate::domain::project::GitProject;

// Boxed future alias, same pattern as the other ports (see connector.rs
// for why Pin<Box<dyn Future>>).
pub type GitFuture<'a> = Pin<Box<dyn Future<Output = Result<(), GitError>> + Send + 'a>>;
pub type RevisionFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;
pub type ExportFuture<'a> = Pin<Box<dyn Future<Output = Result<String, GitError>> + Send + 'a>>;

// GitPort — keep a local checkout of a project's repository up to date.
// The concrete implementation shells out to the git binary; the trait exists
// so tests (and a future libgit2/gix adapter) can swap it.
pub trait GitPort: Send + Sync {
    // Make `dir` an up-to-date checkout of the project's branch:
    // clone when the directory has no checkout yet, fetch + reset when it does.
    // `credentials` come already resolved from the SecretsPort (empty for
    // public repos).
    fn ensure(
        &self,
        dir: &Path,
        project: &GitProject,
        credentials: &HashMap<String, String>,
    ) -> GitFuture<'_>;

    // The commit the checkout in `dir` is at, for the run history: "which
    // commit did this apply" is the first question after a bad change, and
    // the answer is what gets reverted. None when it cannot tell (no checkout
    // yet); the default lets test doubles that do not care leave it out.
    fn revision(&self, _dir: &Path) -> RevisionFuture<'_> {
        Box::pin(async { None })
    }

    // The git tree id of `folder` at the checkout's HEAD: it changes only
    // when something in that folder does, unlike the commit, which any change
    // anywhere in the repository moves. What tells drift (the system changed)
    // from a commit (git changed) for one application. Works on a shallow
    // clone. None when it cannot tell.
    fn tree(&self, _dir: &Path, _folder: &str) -> RevisionFuture<'_> {
        Box::pin(async { None })
    }

    // Write `path` of the checkout in `dir` as it was at `revision` into
    // `dest`, without touching the checkout itself (its files, index and
    // HEAD stay where the pulls left them): what a rollback applies. Answers
    // the full commit id the revision resolved to. Checkouts are shallow, so
    // an older commit may first have to be fetched: hence the project and
    // its resolved credentials.
    #[allow(clippy::too_many_arguments)]
    fn export<'a>(
        &'a self,
        _dir: &'a Path,
        _project: &'a GitProject,
        _credentials: &'a HashMap<String, String>,
        _revision: &'a str,
        _path: &'a str,
        _dest: &'a Path,
    ) -> ExportFuture<'a> {
        Box::pin(async {
            Err(GitError {
                message: "this git adapter cannot read an older commit".to_string(),
            })
        })
    }
}

#[derive(Debug)]
pub struct GitError {
    pub message: String,
}
