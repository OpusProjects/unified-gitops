use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

use crate::domain::desired::key_of;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum OpKind {
    Create,
    Update,
    Delete,
}

impl OpKind {
    pub fn as_str(self) -> &'static str {
        match self {
            OpKind::Create => "create",
            OpKind::Update => "update",
            OpKind::Delete => "delete",
        }
    }
}

// One field that differs, by its path inside the object (`clients`,
// `quota.hard`). `from` is what the system has now (null when it has nothing
// there), `to` what git says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Change {
    pub field: String,
    #[schema(value_type = Object)]
    pub from: Value,
    #[schema(value_type = Object)]
    pub to: Value,
}

// One step of a plan: what to do to one object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Operation {
    pub kind: String,
    pub key: String,
    pub op: OpKind,
    // What the connector receives: the desired object for create and update (the
    // fields git declares, nothing more), the object as listed for delete.
    #[schema(value_type = Object)]
    pub object: Value,
    // For an update, the fields that differ; empty otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<Change>,
    // The desired-state file that declares the object, from the repository's
    // root. None for a delete: nothing declares what prune removes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
}

// The diff for one kind.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct KindDiff {
    pub operations: Vec<Operation>,
    // Objects on the system that git does not declare, while prune is off.
    // Not operations — nothing will happen to them — but worth showing: they
    // are either hand-made drift or something git forgot.
    pub unmanaged: Vec<Operation>,
    pub in_sync: usize,
}

// Compares what git declares with what the system has, for one kind.
//
// Only the fields git declares are compared. A live system returns dozens of
// fields with defaults (an Isilon export has 40); comparing all of them would
// make every object "changed" forever. A field git leaves out is, by
// definition, not managed. Nested objects follow the same rule recursively;
// lists and scalars are compared whole — a list's ordering and spelling is
// the connector's to normalise in `list`, since only it knows whether
// ["b","a"] and ["a","b"] are the same thing on its system.
pub fn diff(
    kind: &str,
    key: &str,
    desired: &[serde_json::Map<String, Value>],
    actual: &[Value],
    prune: bool,
) -> KindDiff {
    let mut current: BTreeMap<String, &serde_json::Map<String, Value>> = BTreeMap::new();
    for object in actual {
        // An object the connector listed without a usable key cannot be matched
        // or managed; it is left out rather than guessed at.
        if let Value::Object(o) = object
            && let Some(k) = key_of(o, key)
        {
            current.insert(k, o);
        }
    }

    let mut out = KindDiff::default();
    let mut declared = std::collections::HashSet::new();
    for want in desired {
        let Some(k) = key_of(want, key) else { continue };
        declared.insert(k.clone());
        match current.get(&k) {
            None => out.operations.push(Operation {
                kind: kind.into(),
                key: k,
                op: OpKind::Create,
                object: Value::Object(want.clone()),
                changes: Vec::new(),
                file: None,
            }),
            Some(have) => {
                let mut changes = Vec::new();
                compare("", want, have, key, &mut changes);
                if changes.is_empty() {
                    out.in_sync += 1;
                } else {
                    out.operations.push(Operation {
                        kind: kind.into(),
                        key: k,
                        op: OpKind::Update,
                        object: Value::Object(want.clone()),
                        changes,
                        file: None,
                    });
                }
            }
        }
    }

    for (k, have) in current {
        if declared.contains(&k) {
            continue;
        }
        let op = Operation {
            kind: kind.into(),
            key: k,
            op: OpKind::Delete,
            object: Value::Object(have.clone()),
            changes: Vec::new(),
            file: None,
        };
        if prune {
            out.operations.push(op);
        } else {
            out.unmanaged.push(op);
        }
    }
    out
}

fn compare(
    prefix: &str,
    want: &serde_json::Map<String, Value>,
    have: &serde_json::Map<String, Value>,
    key: &str,
    changes: &mut Vec<Change>,
) {
    for (field, wanted) in want {
        // The key matched by construction; a "change" to it is meaningless.
        if prefix.is_empty() && field == key {
            continue;
        }
        let path = if prefix.is_empty() {
            field.clone()
        } else {
            format!("{prefix}.{field}")
        };
        match (wanted, have.get(field)) {
            (Value::Object(w), Some(Value::Object(h))) => compare(&path, w, h, key, changes),
            (w, Some(h)) if same(w, h) => {}
            // `field:` or `field: null` in git, and a system that leaves empty
            // fields out of its listing: both say "nothing there". Reported,
            // it would be an update from null to null, applied forever.
            (Value::Null, None) => {}
            (w, h) => changes.push(Change {
                field: path,
                from: h.cloned().unwrap_or(Value::Null),
                to: w.clone(),
            }),
        }
    }
}

// Equality with one allowance: numbers compare by value, so 10 and 10.0 (one
// side YAML, the other an API's float) are the same number.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => same_number(x, y),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        _ => a == b,
    }
}

// By value, without going through f64 when it would round: two integers
// above 2^53 that differ (an id, a byte count) are both the same f64, and a
// real change would be missed. An integer and a float are equal only when the
// float is that exact whole number.
fn same_number(x: &serde_json::Number, y: &serde_json::Number) -> bool {
    let int = |n: &serde_json::Number| {
        n.as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
    };
    let whole = |f: f64| (f.fract() == 0.0 && f.abs() < 1e38).then_some(f as i128);
    match (int(x), int(y), x.as_f64(), y.as_f64()) {
        (Some(a), Some(b), _, _) => a == b,
        (Some(a), None, _, Some(f)) | (None, Some(a), Some(f), _) => whole(f) == Some(a),
        (_, _, a, b) => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> serde_json::Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn only_declared_fields_are_compared() {
        let desired = [obj(json!({"path": "/a", "read_only": false}))];
        let actual = [json!({"path": "/a", "read_only": false, "id": 7, "zone": "System"})];
        let d = diff("export", "path", &desired, &actual, false);
        assert!(d.operations.is_empty());
        assert_eq!(d.in_sync, 1);
    }

    #[test]
    fn creates_updates_and_unmanaged_objects() {
        let desired = [
            obj(json!({"path": "/new", "clients": ["10.0.0.0/24"]})),
            obj(json!({"path": "/old", "read_only": true, "quota": {"hard": 10}})),
        ];
        let actual = [
            json!({"path": "/old", "read_only": false, "quota": {"hard": 10.0, "soft": 5}}),
            json!({"path": "/stray", "read_only": true}),
        ];
        let d = diff("export", "path", &desired, &actual, false);
        assert_eq!(d.operations.len(), 2);
        assert_eq!(d.operations[0].op, OpKind::Create);
        assert_eq!(d.operations[1].op, OpKind::Update);
        // 10 and 10.0 are equal; only read_only changed.
        assert_eq!(
            d.operations[1].changes,
            vec![Change {
                field: "read_only".into(),
                from: json!(false),
                to: json!(true)
            }]
        );
        assert_eq!(d.unmanaged.len(), 1);
        assert_eq!(d.unmanaged[0].key, "/stray");
    }

    #[test]
    fn prune_turns_unmanaged_objects_into_deletes() {
        let d = diff("k", "name", &[], &[json!({"name": "x"})], true);
        assert_eq!(d.operations[0].op, OpKind::Delete);
        assert!(d.unmanaged.is_empty());
    }

    #[test]
    fn a_declared_null_matches_a_field_the_system_leaves_out() {
        let desired = [obj(json!({"name": "a", "description": null}))];
        let d = diff("k", "name", &desired, &[json!({"name": "a"})], false);
        assert!(d.operations.is_empty(), "{:?}", d.operations);
        assert_eq!(d.in_sync, 1);
        // A value the system does have is still a change to null.
        let d = diff(
            "k",
            "name",
            &desired,
            &[json!({"name": "a", "description": "x"})],
            false,
        );
        assert_eq!(d.operations.len(), 1);
    }

    #[test]
    fn numbers_compare_exactly() {
        let big = 9_007_199_254_740_993u64; // 2^53 + 1: rounds to 2^53 as f64
        let desired = [obj(json!({"name": "a", "size": big}))];
        let d = diff(
            "k",
            "name",
            &desired,
            &[json!({"name": "a", "size": big - 1})],
            false,
        );
        assert_eq!(d.operations.len(), 1, "a change past 2^53 must be seen");
        let d = diff(
            "k",
            "name",
            &desired,
            &[json!({"name": "a", "size": big})],
            false,
        );
        assert!(d.operations.is_empty());
        // An integer and the float of the same whole number are equal; a
        // fraction is not.
        assert!(same(&json!(10), &json!(10.0)));
        assert!(same(&json!(-3.0), &json!(-3)));
        assert!(!same(&json!(10), &json!(10.5)));
        assert!(same(&json!(0.5), &json!(0.5)));
    }

    #[test]
    fn nested_fields_report_their_path() {
        let desired = [obj(json!({"name": "a", "quota": {"hard": 20}}))];
        let actual = [json!({"name": "a", "quota": {"hard": 10, "soft": 5}})];
        let d = diff("k", "name", &desired, &actual, false);
        assert_eq!(d.operations[0].changes[0].field, "quota.hard");
    }
}
