# Connectors

A connector is a script that knows one kind of system. It does two things: list the objects of a kind
that exist now, and apply one operation. Everything else (the diff, drift, prune, ordering, retries,
locks, history) is the core's, so every connector gets it without writing it.

## The contract

```
<script> describe              → {"protocol": 1, "kinds": [{"kind": "nfs_export", "key": "path"}, ...]}
<script> list  --kind <kind>   → JSON array of the objects that exist now
<script> apply --kind <kind>   ← stdin {"op": "create" | "update" | "delete", "object": {...}}
```

| What the script gets         | Where                                                                |
| ---------------------------- | -------------------------------------------------------------------- |
| The application's `config`   | `APPLICATION_CONFIG`, JSON                                           |
| Each credential field        | `CREDENTIAL_<FIELD>` (`CREDENTIAL_USERNAME`, `CREDENTIAL_PASSWORD`…) |
| Permission to write          | `UNIFIED_ALLOW_WRITE=1`, on `apply` only                             |
| The project's virtualenv     | On `PATH`, when the connector's project has one                         |

Rules:

- **Output:** JSON on stdout, messages on stderr. Exit 0 only on success, never with half a document.
- **Environment:** scrubbed. The script sees `PATH`, `HOME`, locale, proxy and CA variables plus what
  is listed above; never the service's API keys or another application's credentials.
- **Time:** bounded by the connector's `timeout_seconds`; past it the script is killed, not abandoned.
- **`describe`** is called once per run; a different `protocol` fails the run with a clear message.

### What `describe` returns

What the connector manages is the script's to say, not configuration: the knowledge lives next to the
code that handles the objects, once, the way a Kubernetes API server describes its own resources.

```json
{"protocol": 1,
 "kinds": [
   {"kind": "nfs_export", "key": "path"},
   {"kind": "quota", "key": "path", "depends_on": ["nfs_export"]}
 ]}
```

- **`kind`**: a type of object; desired-state files name it (`kind: nfs_export`). Letters, digits,
  `-`, `_` and `.`: it also names a file in the state directory.
- **`key`**: the field that identifies one object, matched between git and the system (`path` for
  an export, `name` for a job template). It is never itself "changed".
- **`depends_on`**: types that must exist first. Creates and updates follow it; deletes run in
  reverse. An unknown type or a cycle fails the run, before anything is listed.

### What `list` should return

- Every object of the kind, each carrying the kind's `key` field.
- **Normalised**: the same thing written the same way every time. Sort lists whose order the system
  does not care about, lowercase what is case-insensitive, expand short forms. The core compares
  values literally (numbers by value), so `["b","a"]` and `["a","b"]` are a change unless the connector
  sorts them.
- Extra fields are fine: only the fields git declares are compared.

### What `apply` receives

- **create / update**: the object exactly as git declares it, only its declared fields. An update
  should change those fields and leave the rest alone, as a `PATCH` would.
- **delete**: the object as `list` returned it, so the connector has whatever id it needs.

## The Python SDK

The SDK, the `unified_connectors` package in
[unified-connectors](https://github.com/OpusProjects/unified-connectors), implements the contract.
A connector writes two functions:

```python
from unified_connectors import ConnectorError, run

def list_objects(kind, config, credentials):
    return [...]                                  # what exists now

def apply(kind, op, obj, config, credentials):
    ...                                           # one create, update or delete

run(
    kinds={
        "nfs_export": {"key": "path"},
        "quota": {"key": "path", "depends_on": ["nfs_export"]},
    },
    list_objects=list_objects,
    apply=apply,
)
```

`run` answers `describe` from `kinds`, parses the subcommand, reads the config and credentials,
enforces the write guard, prints the
JSON and sets the exit code. Raise `ConnectorError("…")` for a one-line message; anything else prints a
traceback. Standard library only.

`kv/kv.py` there is a complete example: a key-value store kept as JSON files in a folder, with two
kinds (`namespace`, and `entry`, which depends on it). This repository's tests run a copy of both.

## Declaring a connector

```yaml
# connectors/powerscale.yaml
connectors:
  - name: powerscale
    description: "Dell PowerScale"
    type: script                        # optional: script is the only type, and the default
    project: prj-connectors             # resolve path inside this checkout (optional)
    path: connectors/powerscale.py
    timeout_seconds: 120
```

That is all a connector's configuration holds: how to run the script. What it manages comes from
its `describe`.

With a `project`, the script is looked up in that project's checkout on every run, so a new
version of the connector is used as soon as its project is pulled. Without one, `path` is used as
written (an absolute path in the image, for example).

## Sharing scripts with unified-api

unified-api's connectors use the same environment conventions (`CREDENTIAL_*`, a scrubbed
environment, timeouts that kill). One script can serve both tools with subcommands: an `inventory`
command for unified-api, `list` and `apply` for unified-gitops. Give unified-api a read-only API user:
the write guard stops accidents in the script, and the system's own permissions stop everything else.

## Testing a connector by hand

```bash
# in a unified-connectors checkout
export APPLICATION_CONFIG='{"dir": "/tmp/store"}'
./kv/kv.py describe
./kv/kv.py list --kind namespace
echo '{"op":"create","object":{"name":"team-a"}}' | UNIFIED_ALLOW_WRITE=1 ./kv/kv.py apply --kind namespace
```
