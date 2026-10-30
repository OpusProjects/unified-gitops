# API

Every route is under `/api/v1` except the probes, `/metrics` and the docs. The full OpenAPI document
is at `/api-docs/openapi.json` and browsable at `/swagger-ui/`; in a browser with the UI enabled,
every `/api/v1` route also renders as a page.

Authentication: an `X-API-Key` header, or a browser session from the login page (writes from a
session also need its `X-CSRF-Token`). Every failure is `{"error": "..."}`.

| Route                                 | Method | Who                  | What                                               |
| ------------------------------------- | ------ | -------------------- | -------------------------------------------------- |
| `/api/v1/`                            | GET    | any key              | Every collection this key may read                 |
| `/api/v1/apis`                        | GET    | any key (its projects) | API items: what they watch, where they send, last run |
| `/api/v1/apis/{id}`                   | GET    | key granted its project | One API item and its last 50 runs               |
| `/api/v1/apis/{id}/run`               | POST   | key granted its project | Run it now over every selected file; `?dry_run=true` writes nothing |
| `/api/v1/scripts`                     | GET    | any key (its projects) | Script items: what they watch, what they run, last run |
| `/api/v1/scripts/{id}`                | GET    | key granted its project | One script item and its last 50 runs, with output |
| `/api/v1/scripts/{id}/run`            | POST   | key granted its project | Run it now; `?dry_run=true` runs nothing and says what would run |
| `/api/v1/webhooks`                    | GET    | admin                | Webhooks: project, events, destination, last send  |
| `/api/v1/webhooks/{id}`               | GET    | admin                | One webhook and its last 50 sends                  |
| `/api/v1/webhooks/{id}/test`          | POST   | admin                | Send a test message now, once; answers the outcome |
| `/api/v1/projects`                    | GET    | any key (its projects) | Git checkouts, their health, what uses them      |
| `/api/v1/projects/{id}/sync`          | POST   | key granted it       | Pull a project now: what a pipeline calls after pushing |
| `/api/v1/credentials`                 | GET    | admin                | Credentials: where from, whether they resolve      |
| `/api/v1/settings`                    | GET    | admin                | The configuration in force, defaults included      |
| `/api/v1/auth`                        | GET    | any key              | Who the caller is; for admin, every way in         |
| `/api/v1/config…`                     | …      | admin                | The configuration directory (see config-api.md)    |
| `/healthz`, `/readyz`                 | GET    | public               | Probes                                             |
| `/metrics`                            | GET    | public (or key)      | Prometheus exposition                              |

A restricted key (or LDAP rule) names `projects`: it sees and pulls those, and sees and runs
their APIs and scripts; another project's read as absent (404).

## Examples

```bash
H="X-API-Key: $GITOPS_KEY"

# A pipeline, after pushing: pull now, so the commit is acted on at once
curl -s -X POST -H "$H" localhost:8183/api/v1/projects/infra/sync | jq '{success, error}'

# What a run of an API would send, without sending it
curl -s -X POST -H "$H" "localhost:8183/api/v1/apis/portainer-users/run?dry_run=true" \
  | jq '.steps[] | {file, action, method, url, body}'

# How the last runs of a script went
curl -s -H "$H" localhost:8183/api/v1/scripts/reload-dns \
  | jq '.history[] | {trigger, revision, ok, steps: [.steps[] | {file, exit_code, error}]}'

# Run it now
curl -s -X POST -H "$H" localhost:8183/api/v1/scripts/reload-dns/run | jq '{ok, error}'
```

## The run record

A run endpoint answers with the run it made, which is also the newest entry in the item's
`history`:

```json
{
  "item": "portainer-users", "kind": "api", "project": "infra",
  "trigger": "manual", "by": "key-pipeline", "dry_run": false,
  "revision": "6e488a7…", "script_revision": null,
  "started_at": 1793001234, "duration_ms": 812, "ok": false, "error": null,
  "steps": [
    {"file": "users/admin.json", "action": "skip", "ok": true, "attempts": 0},
    {"file": "users/jane.json", "action": "update", "method": "PUT",
     "url": "https://portainer.example.com/api/users/jane", "status": 400, "ok": false,
     "attempts": 1, "changed_fields": ["Role"], "error": "HTTP 400: …"}
  ]
}
```

A script's steps have `action: run`, its `exit_code` and `output` (the end of it) instead of a
method and a URL. A dry run's steps carry the `body` an API would send, or the command a script
would run.
