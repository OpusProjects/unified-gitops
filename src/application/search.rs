use std::collections::HashSet;

use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::app::Application;
use crate::domain::connector::DescribedKinds;
use crate::domain::desired::key_of;
use crate::domain::plan::OpKind;
use crate::domain::run::AppStateRegistry;
use crate::ports::state_store::StateStorePort;

// "Where is this object?" — unified-api's host search, for gitops: an NFS
// export, a job template or a form, found by its key or any value in it,
// across the current settings of every system the caller may see. Read from
// the local copies, so it costs no call to any system.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct FoundObject {
    pub application: String,
    pub kind: String,
    pub key: String,
    /// The field whose value matched, as a dotted path; absent when the key did
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Whether git declares it, as of the last check: false for an object on
    /// the system that git does not list (unmanaged, or a pending delete);
    /// null before the application's first check
    pub managed: Option<bool>,
    /// Unix seconds the copy it was found in was read from the system
    pub written_at: u64,
}

pub fn search<'a>(
    query: &str,
    limit: usize,
    applications: impl Iterator<Item = (&'a String, &'a Application)>,
    // What each connector last described: the key of every kind.
    described: &DescribedKinds,
    store: &dyn StateStorePort,
    states: &AppStateRegistry,
) -> Vec<FoundObject> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let snapshots = store.list().unwrap_or_default();
    // (rank, found): key matches first, then matches inside a value.
    let mut found: Vec<(u8, FoundObject)> = Vec::new();
    for (app_id, app) in applications {
        // A connector no run has described since the start: its copies cannot
        // be matched by key yet.
        let Some(kinds) = described.get(&app.connector) else {
            continue;
        };
        // What git does not declare, per the last check: the unmanaged list,
        // or the deletes a prune would make.
        let plan = states.get(app_id).plan;
        let not_declared: Option<HashSet<(String, String)>> = plan.map(|p| {
            p.unmanaged
                .iter()
                .chain(p.operations.iter().filter(|o| o.op == OpKind::Delete))
                .map(|o| (o.kind.clone(), o.key.clone()))
                .collect()
        });
        for info in snapshots.iter().filter(|s| &s.application == app_id) {
            let Some(kind) = kinds.get(&info.kind) else {
                continue;
            };
            let Ok(Some(snapshot)) = store.read(app_id, &info.kind) else {
                continue;
            };
            for item in &snapshot.items {
                let Value::Object(object) = item else {
                    continue;
                };
                let Some(key) = key_of(object, &kind.key) else {
                    continue;
                };
                let (rank, field) = if key.to_lowercase().contains(&needle) {
                    (0, None)
                } else if let Some(path) = matching_field(item, &needle, "") {
                    (1, Some(path))
                } else {
                    continue;
                };
                let managed = not_declared
                    .as_ref()
                    .map(|set| !set.contains(&(info.kind.clone(), key.clone())));
                found.push((
                    rank,
                    FoundObject {
                        application: app_id.clone(),
                        kind: info.kind.clone(),
                        key,
                        field,
                        managed,
                        written_at: snapshot.written_at,
                    },
                ));
            }
        }
    }
    found.sort_by(|(ra, a), (rb, b)| {
        (ra, &a.application, &a.kind, &a.key).cmp(&(rb, &b.application, &b.kind, &b.key))
    });
    found.into_iter().take(limit).map(|(_, f)| f).collect()
}

// The first scalar under `value` whose text contains `needle`, as a dotted
// path ("clients.0", "quota.hard").
fn matching_field(value: &Value, needle: &str, path: &str) -> Option<String> {
    let join = |part: &str| {
        if path.is_empty() {
            part.to_string()
        } else {
            format!("{path}.{part}")
        }
    };
    match value {
        Value::Object(map) => map
            .iter()
            .find_map(|(k, v)| matching_field(v, needle, &join(k))),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| matching_field(v, needle, &join(&i.to_string()))),
        Value::String(s) if s.to_lowercase().contains(needle) => Some(path.to_string()),
        Value::Number(n) if n.to_string().contains(needle) => Some(path.to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_value_match_names_its_field() {
        let item = json!({"path": "/ifs/a", "clients": ["10.0.0.0/24"], "quota": {"hard": 20}});
        assert_eq!(
            matching_field(&item, "10.0.0", "").as_deref(),
            Some("clients.0")
        );
        assert_eq!(
            matching_field(&item, "20", "").as_deref(),
            Some("quota.hard")
        );
        assert_eq!(matching_field(&item, "nothing", ""), None);
    }
}
