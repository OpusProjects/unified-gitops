use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Serialize;
use serde_json::Value;

use crate::domain::connector::Kinds;

// One desired-state file: the objects of ONE kind on the application's system.
//
//   kind: nfs_export
//   items:
//     - path: /ifs/data/projects
//       clients: [10.0.0.0/24]
//
// One kind per file keeps a file readable on its own and makes "where is
// this object declared" a grep. Several files may declare the same kind; an
// object key may appear only once across all of them. The file does not name
// its system: the application's folder belongs to one system, and where the
// writes go is the configuration's to say, not the repository's.
#[derive(Debug, Clone, Serialize)]
pub struct DesiredDoc {
    // Relative to the application folder, for messages and the API.
    pub file: String,
    pub kind: String,
    pub items: Vec<serde_json::Map<String, Value>>,
}

// Desired state per kind, merged across files — what the diff consumes.
pub type DesiredSet = BTreeMap<String, Vec<serde_json::Map<String, Value>>>;

// The key of an object, as a string: a key field may be a string or a number
// in YAML and in the API's JSON, and "8080" from one side must match 8080
// from the other.
pub fn key_of(object: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    match object.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        Value::Number(n) => Some(number_key(n)),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

// A whole number is spelled the same however it arrived: an API that returns
// 1.0 where git says 1 names the same object, and must not plan a create (and,
// with prune, a delete) for it. Floats with a fraction keep their spelling.
fn number_key(n: &serde_json::Number) -> String {
    if n.is_f64()
        && let Some(f) = n.as_f64()
        && f.fract() == 0.0
        && f.abs() < 9.2e18
    {
        return (f as i64).to_string();
    }
    n.to_string()
}

pub fn parse_doc(file: &str, text: &str) -> Result<DesiredDoc, String> {
    let value = crate::domain::yaml::to_json(text).map_err(|e| format!("{file}: {e}"))?;
    let Value::Object(mut top) = value else {
        return Err(format!("{file}: expected a mapping with kind and items"));
    };
    // Strict, as the configuration is: an unknown top-level key is a typo
    // (`item:`, `kinds:`) whose objects would otherwise be silently ignored.
    for k in top.keys() {
        if !["kind", "items"].contains(&k.as_str()) {
            return Err(format!(
                "{file}: unknown key '{k}' (expected kind and items)"
            ));
        }
    }
    let text_field = |top: &serde_json::Map<String, Value>, name: &str| match top.get(name) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        _ => Err(format!("{file}: '{name}' must be a non-empty string")),
    };
    let kind = text_field(&top, "kind")?;
    let items = match top.remove("items") {
        Some(Value::Array(items)) => items,
        // An empty file section is a deliberate "nothing of this kind".
        None | Some(Value::Null) => Vec::new(),
        Some(_) => return Err(format!("{file}: 'items' must be a list")),
    };
    let mut objects = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        match item {
            Value::Object(o) => objects.push(o),
            _ => return Err(format!("{file}: item {} is not a mapping", i + 1)),
        }
    }
    Ok(DesiredDoc {
        file: file.to_string(),
        kind,
        items: objects,
    })
}

// The application's folder `path` inside `root` (a checkout, or a commit
// exported for a rollback), loaded as load_dir does. The folder and every
// folder on the way to it must be real directories: `is_dir` and `read_dir`
// follow symlinks, so a committed link (`apps/prod -> /etc`, or into another
// project's checkout) would otherwise make files outside the repository this
// application's desired state.
pub fn load_folder(root: &Path, path: &str) -> Result<Vec<DesiredDoc>, Vec<String>> {
    let mut dir = root.to_path_buf();
    for part in Path::new(path).components() {
        if let std::path::Component::Normal(name) = part {
            dir.push(name);
            if std::fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(vec![format!(
                    "{} is a symbolic link: desired state must be a folder in the repository",
                    dir.display()
                )]);
            }
        }
    }
    load_dir(&dir)
}

// Every *.yaml / *.yml under `dir`, recursively, sorted by path so the same
// commit always produces the same plan. Hidden entries are skipped (.git,
// editor swap files).
pub fn load_dir(dir: &Path) -> Result<Vec<DesiredDoc>, Vec<String>> {
    if !dir.is_dir() {
        return Err(vec![format!(
            "{} is not a directory in the checkout",
            dir.display()
        )]);
    }
    let mut files = Vec::new();
    collect(dir, dir, &mut files).map_err(|e| vec![e])?;
    files.sort();

    let mut docs = Vec::new();
    let mut errors = Vec::new();
    for rel in files {
        let text = match std::fs::read_to_string(dir.join(&rel)) {
            Ok(t) => t,
            Err(e) => {
                errors.push(format!("{rel}: {e}"));
                continue;
            }
        };
        match parse_doc(&rel, &text) {
            Ok(doc) => docs.push(doc),
            Err(e) => errors.push(e),
        }
    }
    if errors.is_empty() {
        Ok(docs)
    } else {
        Err(errors)
    }
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        // file_type, not path.is_dir(): a symlink is not followed, so a link
        // in the repository cannot pull files from elsewhere on the host in.
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            collect(root, &path, out)?;
        } else if kind.is_file()
            && (name.ends_with(".yaml") || name.ends_with(".yml"))
            && let Ok(rel) = path.strip_prefix(root)
        {
            out.push(rel.to_string_lossy().to_string());
        }
    }
    Ok(())
}

// Checks the documents against the kinds the application's connector
// describes, and merges them per
// kind. Every problem is reported at once: a commit with three typos should
// say three things, not one per sync.
pub fn validate(
    connector_id: &str,
    kinds: &Kinds,
    docs: &[DesiredDoc],
) -> Result<DesiredSet, Vec<String>> {
    let mut errors = Vec::new();
    let mut set: DesiredSet = BTreeMap::new();
    let mut seen: HashMap<(String, String), String> = HashMap::new();

    for doc in docs {
        let Some(kind) = kinds.get(&doc.kind) else {
            let known: Vec<&str> = kinds.names().map(String::as_str).collect();
            errors.push(format!(
                "{}: connector '{}' has no kind '{}' (it manages: {})",
                doc.file,
                connector_id,
                doc.kind,
                known.join(", ")
            ));
            continue;
        };
        for (i, item) in doc.items.iter().enumerate() {
            let Some(key) = key_of(item, &kind.key) else {
                errors.push(format!(
                    "{}: item {} has no '{}', the key of kind '{}'",
                    doc.file,
                    i + 1,
                    kind.key,
                    doc.kind
                ));
                continue;
            };
            let id = (doc.kind.clone(), key.clone());
            if let Some(first) = seen.get(&id) {
                errors.push(format!(
                    "{}: {} '{}' is already declared in {}",
                    doc.file, doc.kind, key, first
                ));
                continue;
            }
            seen.insert(id, doc.file.clone());
            set.entry(doc.kind.clone()).or_default().push(item.clone());
        }
        // A file with no items still declares the kind: with prune on, an
        // emptied file is how "remove all of these" is written.
        set.entry(doc.kind.clone()).or_default();
    }

    if errors.is_empty() {
        Ok(set)
    } else {
        Err(errors)
    }
}

// Which file declares each object, by (kind, key): what a change is shown
// with, so a reader knows where in git to look. Built from documents that
// already passed `validate`, so every key resolves and is declared once.
pub fn declared_in(kinds: &Kinds, docs: &[DesiredDoc]) -> HashMap<(String, String), String> {
    let mut files = HashMap::new();
    for doc in docs {
        let Some(kind) = kinds.get(&doc.kind) else {
            continue;
        };
        for item in &doc.items {
            if let Some(key) = key_of(item, &kind.key) {
                files.insert((doc.kind.clone(), key), doc.file.clone());
            }
        }
    }
    files
}

// The kinds an application reconciles, in apply order: the connector's kinds
// with dependencies first. Only kinds that appear in desired state are
// touched: an application that never mentions a kind does not own the
// objects of that kind on its system.
pub fn ordered_kinds(set: &DesiredSet, kinds: &Kinds) -> Vec<String> {
    kinds
        .in_order()
        .into_iter()
        .filter(|kind| set.contains_key(kind))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_parses_with_exact_scalars() {
        let doc = parse_doc("a.yaml", "kind: k\nitems:\n  - name: x\n    mode: 0644\n").unwrap();
        assert_eq!(doc.items[0]["mode"], Value::String("0644".into()));
    }

    #[test]
    fn unknown_top_level_keys_and_bad_items_are_refused() {
        assert!(
            parse_doc("a.yaml", "kind: k\nitem: []\n")
                .unwrap_err()
                .contains("unknown key 'item'")
        );
        assert!(
            parse_doc("a.yaml", "kind: k\nitems: [3]\n")
                .unwrap_err()
                .contains("item 1 is not a mapping")
        );
        // A system is the configuration's to name, never the repository's.
        assert!(
            parse_doc("a.yaml", "target: t\nkind: k\nitems: []\n")
                .unwrap_err()
                .contains("unknown key 'target'")
        );
        assert!(
            parse_doc("a.yaml", "items: []\n")
                .unwrap_err()
                .contains("'kind'")
        );
    }

    #[test]
    fn number_and_string_keys_match() {
        let mut o = serde_json::Map::new();
        o.insert("port".into(), Value::from(8080));
        assert_eq!(key_of(&o, "port").as_deref(), Some("8080"));
        o.insert("port".into(), Value::from(""));
        assert_eq!(key_of(&o, "port"), None);
    }

    // git says 1, the API answers 1.0: the same object, not a create plus
    // (with prune) a delete.
    #[test]
    fn whole_floats_and_integers_are_the_same_key() {
        let mut o = serde_json::Map::new();
        o.insert("id".into(), serde_json::json!(1.0));
        assert_eq!(key_of(&o, "id").as_deref(), Some("1"));
        o.insert("id".into(), serde_json::json!(-7.0));
        assert_eq!(key_of(&o, "id").as_deref(), Some("-7"));
        o.insert("id".into(), serde_json::json!(1.5));
        assert_eq!(key_of(&o, "id").as_deref(), Some("1.5"));
        o.insert("id".into(), serde_json::json!(u64::MAX));
        assert_eq!(key_of(&o, "id"), Some(u64::MAX.to_string()));
    }

    #[test]
    fn a_symlinked_application_folder_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("secret.yaml"),
            "kind: k\nitems:\n  - name: x\n",
        )
        .unwrap();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("apps")).unwrap();
        std::os::unix::fs::symlink(outside.path(), repo.path().join("apps/prod")).unwrap();
        // The folder itself, and a link higher up the path.
        std::os::unix::fs::symlink(outside.path(), repo.path().join("linked")).unwrap();

        for path in ["apps/prod", "./apps/prod/", "linked"] {
            let err = load_folder(repo.path(), path).unwrap_err();
            assert!(err[0].contains("symbolic link"), "{path}: {err:?}");
        }

        std::fs::create_dir_all(repo.path().join("apps/dev")).unwrap();
        std::fs::write(
            repo.path().join("apps/dev/a.yaml"),
            "kind: k\nitems:\n  - name: x\n",
        )
        .unwrap();
        assert_eq!(load_folder(repo.path(), "apps/dev").unwrap().len(), 1);
        // From the root, the links inside are skipped, as they always were.
        assert_eq!(load_folder(repo.path(), ".").unwrap().len(), 1);
    }
}
