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
pub type CommitsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommitRange, GitError>> + Send + 'a>>;

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

    // What a pull brought: the commits after `from` up to `to` (newest
    // first, at most `max`), and the files that differ between the two.
    // Checkouts are shallow, so the history in between may first have to be
    // fetched: hence the project and its resolved credentials.
    #[allow(clippy::too_many_arguments)]
    fn commits<'a>(
        &'a self,
        _dir: &'a Path,
        _project: &'a GitProject,
        _credentials: &'a HashMap<String, String>,
        _from: &'a str,
        _to: &'a str,
        _max: usize,
    ) -> CommitsFuture<'a> {
        Box::pin(async {
            Err(GitError {
                message: "this git adapter cannot list commits".to_string(),
            })
        })
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

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub revision: String,
    pub author: String,
    // The first line of its message.
    pub message: String,
    // Unix seconds, as committed.
    pub at: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommitRange {
    pub commits: Vec<Commit>,
    // Commits left out: more than `max`, or a history too deep to reach.
    pub more: bool,
    // `from` is not in the new history: it was rewritten (a force push).
    // Only claimed when the whole history was there to look at.
    pub forced: bool,
    // The paths that differ between `from` and `to`; None when git could
    // not compare them.
    pub paths: Option<Vec<String>>,
    // Whether finding them took a fetch (counted as a git request).
    pub fetched: bool,
}

#[derive(Debug)]
pub struct GitError {
    pub message: String,
}
