use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::ports::state_store::{Snapshot, SnapshotInfo, StateStorePort};

// `<dir>/<application>/<kind>.yaml`. Application and kind ids are checked by config
// validation (letters, digits, `-`, `_`, `.`, not starting with a dot), so
// they can be path segments without escaping and cannot climb out of `dir`.
pub struct FsStateStore {
    dir: PathBuf,
}

impl FsStateStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn file(&self, application: &str, kind: &str) -> PathBuf {
        self.dir.join(application).join(format!("{kind}.yaml"))
    }

    // Hidden, and not YAML: never mistaken for a kind's copy.
    fn rollback_file(&self, application: &str) -> PathBuf {
        self.dir.join(application).join(".rollback.json")
    }
}

// The file a person reads: a header saying what it is, then the same three
// keys a desired-state file has.
#[derive(serde::Serialize)]
struct Doc<'a> {
    kind: &'a str,
    items: &'a [Value],
}

const HEADER: &str = "# Written by unified-gitops: the settings this system had at its last check.\n\
                      # Same format as desired state in git; copy it there to manage these objects.\n";

fn written_at(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl StateStorePort for FsStateStore {
    fn write(&self, application: &str, kind: &str, items: &[Value]) -> Result<(), String> {
        let path = self.file(application, kind);
        let parent = path.parent().ok_or("state path has no parent")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let body = serde_saphyr::to_string(&Doc { kind, items })
            .map_err(|e| format!("could not write {application}/{kind} as YAML: {e}"))?;
        // Temp file + rename: a reader (or a crash) never sees half a file.
        let tmp = path.with_extension("yaml.tmp");
        std::fs::write(&tmp, format!("{HEADER}{body}"))
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn read(&self, application: &str, kind: &str) -> Result<Option<Snapshot>, String> {
        let path = self.file(application, kind);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let doc = crate::domain::desired::parse_doc(&format!("{application}/{kind}.yaml"), &text)?;
        Ok(Some(Snapshot {
            application: application.to_string(),
            kind: doc.kind,
            items: doc.items.into_iter().map(Value::Object).collect(),
            written_at: written_at(&path),
        }))
    }

    fn rollback(&self, application: &str) -> Option<crate::domain::run::Rollback> {
        let text = std::fs::read_to_string(self.rollback_file(application)).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn set_rollback(
        &self,
        application: &str,
        rollback: Option<&crate::domain::run::Rollback>,
    ) -> Result<(), String> {
        let path = self.rollback_file(application);
        match rollback {
            None => match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(format!("{}: {e}", path.display())),
            },
            Some(rollback) => {
                let parent = path.parent().ok_or("state path has no parent")?;
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("{}: {e}", parent.display()))?;
                let body = serde_json::to_string(rollback).map_err(|e| e.to_string())?;
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
                std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
            }
        }
    }

    fn list(&self) -> Result<Vec<SnapshotInfo>, String> {
        let mut out = Vec::new();
        let Ok(applications) = std::fs::read_dir(&self.dir) else {
            return Ok(out);
        };
        for application in applications.flatten() {
            if !application.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let app_id = application.file_name().to_string_lossy().to_string();
            let Ok(files) = std::fs::read_dir(application.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name().to_string_lossy().to_string();
                let Some(kind) = name.strip_suffix(".yaml") else {
                    continue;
                };
                if let Ok(Some(snapshot)) = self.read(&app_id, kind) {
                    out.push(SnapshotInfo {
                        application: snapshot.application,
                        kind: snapshot.kind,
                        items: snapshot.items.len(),
                        written_at: snapshot.written_at,
                    });
                }
            }
        }
        out.sort_by(|a, b| (&a.application, &a.kind).cmp(&(&b.application, &b.kind)));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // What is written must read back as exactly the same values: a string
    // that looks like a number ("0644", "08001", "true") must stay a string,
    // or the next diff would report a change nobody made.
    #[test]
    fn a_snapshot_reads_back_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsStateStore::new(dir.path());
        let items = vec![
            json!({"name": "a", "mode": "0644", "zip": "08001", "flag": "true", "n": 3, "f": 1.5,
                   "list": ["x", "y"], "nested": {"k": null}, "text": "line one\nline two"}),
            json!({"name": "b", "empty": "", "colon": "a: b", "hash": "# not a comment"}),
        ];
        store.write("app", "kind", &items).unwrap();
        let back = store.read("app", "kind").unwrap().unwrap();
        assert_eq!(back.items, items);
        assert_eq!(store.list().unwrap()[0].items, 2);
        assert!(store.read("app", "other").unwrap().is_none());
    }
}
