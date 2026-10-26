# API

Every route is under `/api/v1` except the probes, `/metrics` and the docs. The full OpenAPI document
is at `/api-docs/openapi.json` and browsable at `/swagger-ui/`; in a browser with the UI enabled,
every `/api/v1` route also renders as a page.

Authentication: an `X-API-Key` header, or a browser session from the login page (writes from a
session also need its `X-CSRF-Token`). Every failure is `{"error": "..."}`.

| Route                                 | Method | Who                  | What                                               |
| ------------------------------------- | ------ | -------------------- | -------------------------------------------------- |
| `/api/v1/`                            | GET    | any key              | Every collection this key may read                 |
| `/api/v1/applications`                | GET    | any key (its apps)   | Applications with status, commit, pending counts   |
| `/api/v1/applications/{id}`           | GET    | key granted the app  | The same, plus the pending plan and run history    |
| `/api/v1/applications/{id}/refresh`   | POST   | key granted the app  | Compare now; change nothing                        |
| `/api/v1/applications/{id}/sync`      | POST   | key granted the app  | Compare, then apply every operation found          |
| `/api/v1/applications/{id}/rollback`  | POST   | key granted the app  | Apply an older commit's desired state; pause auto-sync |
| `/api/v1/applications/{id}/state/{kind}` | GET | key granted the app  | The system's current settings, as of its last check |
| `/api/v1/search?q=`                   | GET    | any key (its apps)   | Find an object by key or value: its application, kind, whether git declares it |
| `/api/v1/connectors`                     | GET    | admin                | Connectors: script, project, timeout, who uses them |
| `/api/v1/projects`                    | GET    | admin                | Git checkouts, their health, what uses them        |
| `/api/v1/projects/{id}/sync`          | POST   | admin                | Pull a project now                                 |
| `/api/v1/credentials`                 | GET    | admin                | Credentials: where from, whether they resolve      |
| `/api/v1/settings`                    | GET    | admin                | The configuration in force, defaults included      |
| `/api/v1/auth`                        | GET    | any key              | Who the caller is; for admin, every way in         |
| `/api/v1/config…`                     | …      | admin                | The configuration directory (see config-api.md)    |
| `/healthz`, `/readyz`                 | GET    | public               | Probes                                             |
| `/metrics`                            | GET    | public (or key)      | Prometheus exposition                              |

## Examples

```bash
H="X-API-Key: $GITOPS_KEY"

# What is out of sync, everywhere
curl -s -H "$H" localhost:8183/api/v1/applications | jq '.[] | {name, status, pending_operations}'

# What a sync of one application would do, field by field
curl -s -X POST -H "$H" localhost:8183/api/v1/applications/isilon-prod/refresh | jq .result
curl -s -H "$H" localhost:8183/api/v1/applications/isilon-prod | jq '.plan.operations'

# Apply it
curl -s -X POST -H "$H" localhost:8183/api/v1/applications/isilon-prod/sync | jq '{result, applied}'

# What the Isilon has now, in desired-state format
curl -s -H "$H" localhost:8183/api/v1/applications/isilon-prod/state/nfs_export | jq .items
```

## The run record

`refresh` and `sync` answer with the run they made, which is also the newest entry in the
application's `history`:

```json
{
  "id": 7, "trigger": "manual", "mode": "apply", "result": "partial",
  "started_at": 1793001234, "finished_at": 1793001236,
  "revision": "6e488a7…", "planned": 3,
  "applied": [
    {"kind": "nfs_export", "key": "/ifs/a", "op": "create", "ok": true},
    {"kind": "nfs_export", "key": "/ifs/b", "op": "update", "ok": false,
     "error": "… exit code Some(1): 403 Forbidden"}
  ],
  "errors": ["update nfs_export '/ifs/b': … 403 Forbidden"]
}
```

A plan operation (`plan.operations[]`) has `kind`, `key`, `op`, the `object` the connector
will receive, and for an update the `changes`: `[{"field": "read_only", "from": false, "to": true}]`.
