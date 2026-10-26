#!/usr/bin/env python3
"""Example connector: a key-value store kept as JSON files in a folder.

A copy of unified-connectors kv/kv.py, kept here so the tests run offline.

It stands in for a real system's API so the whole gitops loop can run on a
laptop: an application's config names the folder (`dir`), and each kind is one file
of objects keyed by `name`. Editing a file by hand is "someone changed the
system outside git", which the next check reports as drift.

Kinds (described to unified-gitops by `describe`, see KINDS below):
  namespace  {"name", "description"?}
  entry      {"name", "namespace", "value", ...}   depends on namespace
"""

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from unified_connector import ConnectorError, run  # noqa: E402

# What this connector manages: each type of object, the field that identifies
# one, and what must exist first. Entries live in a namespace, so namespaces
# are created before entries and deleted after them.
KINDS = {
    "namespace": {"key": "name"},
    "entry": {"key": "name", "depends_on": ["namespace"]},
}


def _file(config, kind):
    folder = config.get("dir")
    if not folder:
        raise ConnectorError("application config needs 'dir': the folder the store lives in")
    os.makedirs(folder, exist_ok=True)
    return os.path.join(folder, f"{kind}.json")


def _load(config, kind):
    path = _file(config, kind)
    if not os.path.exists(path):
        return {}
    with open(path) as f:
        return json.load(f)


def _save(config, kind, data):
    path = _file(config, kind)
    tmp = path + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2, sort_keys=True)
    os.replace(tmp, path)


def list_objects(kind, config, credentials):
    # Normalised as a real connector should: sorted by key, so the order of
    # the file never looks like a change.
    data = _load(config, kind)
    return [data[name] for name in sorted(data)]


def apply(kind, op, obj, config, credentials):
    data = _load(config, kind)
    name = obj["name"]
    if kind == "entry" and op != "delete":
        namespaces = _load(config, "namespace")
        if obj.get("namespace") not in namespaces:
            raise ConnectorError(f"entry '{name}': namespace '{obj.get('namespace')}' does not exist")
    if op == "delete":
        data.pop(name, None)
    elif op == "create":
        data[name] = obj
    else:
        # update: only the declared fields change; the rest of what the
        # system holds is left alone, as a PATCH to a real API would.
        data[name] = {**data.get(name, {}), **obj}
    _save(config, kind, data)
    print(f"{op} {kind} '{name}'", file=sys.stderr)


run(kinds=KINDS, list_objects=list_objects, apply=apply)
