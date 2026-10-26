use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

// The local copy of each system's current settings: one file per
// (application, kind), in the same shape as a desired-state file in git
// (kind, items). Refreshed from the system at every check and after every apply, so
// it is what the system had at that moment — never just what this tool last
// wrote, which would hide changes made by hand.
//
// Kept on disk (a volume in Kubernetes) on purpose: an operator can diff it
// against the git repository with any tool, and copying a file into git is how
// an existing system is brought under management without retyping it.
pub trait StateStorePort: Send + Sync {
    fn write(&self, application: &str, kind: &str, items: &[Value]) -> Result<(), String>;
    fn read(&self, application: &str, kind: &str) -> Result<Option<Snapshot>, String>;
    // Every snapshot there is, for the API.
    fn list(&self) -> Result<Vec<SnapshotInfo>, String>;
    // The application's rollback, kept beside its copies so a restart does
    // not quietly resume auto-sync (which would undo it). None clears it.
    fn rollback(&self, application: &str) -> Option<crate::domain::run::Rollback>;
    fn set_rollback(
        &self,
        application: &str,
        rollback: Option<&crate::domain::run::Rollback>,
    ) -> Result<(), String>;
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Snapshot {
    pub application: String,
    pub kind: String,
    #[schema(value_type = Vec<Object>)]
    pub items: Vec<Value>,
    // Unix seconds the file was last written.
    pub written_at: u64,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SnapshotInfo {
    pub application: String,
    pub kind: String,
    pub items: usize,
    pub written_at: u64,
}
