// GitPort — keep a local checkout of a project's repository up to date, and
// read from it. The port, its types and the adapter that runs the git binary
// come from the unified-git crate (the `git` repository), shared with
// unified-api; re-exported here so the ports layer still lists every
// interface the application depends on.
pub use unified_git::{
    Commit, CommitRange, CommitsFuture, ExportFuture, GitError, GitFuture, GitPort, Remote,
    RevisionFuture,
};
