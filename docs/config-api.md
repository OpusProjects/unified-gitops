# Configuration API

The configuration of a unified-gitops instance is a directory of YAML files read
at startup. The configuration API makes that directory readable and writable
over HTTP, so a configuration-as-code pipeline can **push** a change to each
instance instead of publishing an artifact each instance has to **pull**.

It is **off by default**. Turning it on means an admin key can rewrite every
file the loader reads — the API keys included, which is the same authority
as editing the directory the container mounts.

```yaml
# config.yaml
config_api:
  enabled: true
```

- [What it is for](#what-it-is-for)
- [Routes](#routes)
- [Pushing a whole directory](#pushing-a-whole-directory)
- [What a reload can and cannot apply](#what-a-reload-can-and-cannot-apply)
- [Safety properties](#safety-properties)
- [Concurrency: ETags and If-Match](#concurrency-etags-and-if-match)
- [Permissions and audit](#permissions-and-audit)
- [Making the directory writable](#making-the-directory-writable)
- [Metrics](#metrics)

---

## What it is for

A pipeline that owns the configuration of a fleet has two ways to get a change
onto an instance:

| | Pull | Push (this API) |
|---|---|---|
| Delivery | publish an image/artifact, instance adopts it on its own schedule | `PUT` the files, instance validates and answers |
| Feedback | none — a bad file is discovered by the instance, later | immediate: `400` with every error, and nothing written |
| Latency | whatever the sync timer or image updater is set to | the request |
| Applying it | restart or replace the process | `?reload=true`, no restart |
| Needs | a registry, pull credentials, an updater per instance | an API key and a route to the instance |

Neither is wrong. Pull survives a pipeline that cannot reach the instance;
push tells the pipeline whether the change was accepted while it still has the
context to fix it.

---

## Routes

All admin-only, all under the API key, all `403` when `config_api.enabled` is
not set — with a body that says so rather than a bare `404`.

| Route | Meaning |
|---|---|
| `GET /api/v1/config` | The directory: every file with size, sha256 and mtime, what is missing, whether it still loads, whether the process is running it |
| `GET /api/v1/config/{path}` | One file, verbatim. `ETag` is the sha256; `If-None-Match` answers `304` |
| `PUT /api/v1/config/{path}` | Create or replace one file (folders are created). Body is raw YAML |
| `DELETE /api/v1/config/{path}` | Remove one file (folders it leaves empty go too) |
| `PUT /api/v1/config` | Replace the whole directory in one transaction |
| `POST /api/v1/config/validate` | Dry run: same checks, nothing written, ever |
| `POST /api/v1/config/reload` | Apply what is on disk to the running process |

`PUT` and `DELETE` take `?reload=true` to apply the change as part of the
write, so a push is one request rather than two.

`{path}` is a file's path inside the directory, slashes included, e.g.
`apis/portainer/users.yaml` (see configuration.md for the layout).
It is a path the loader would read — segments of letters, digits, `-`, `_` and
`.`, none hidden, ending in `.yaml` or `.yml` — or a `404` naming that rule.
`config.yaml`, at the root, cannot be deleted.

---

## Pushing a whole directory

The verb a pipeline wants. `prune: true` makes the directory become *exactly*
the payload — the same semantics as the configuration image it replaces, where
what is not in the image is not in `/config`.

```bash
curl -sS -X PUT "$BASE/api/v1/config?reload=true" \
  -H "X-API-Key: $KEY" \
  -H 'Content-Type: application/json' \
  -H "If-Match: \"$ETAG\"" \
  -d @- <<'JSON'
{
  "prune": true,
  "files": {
    "config.yaml": "server:\n  host: \"0.0.0.0\"\n  port: 8183\n",
    "apis/users.yaml": "apis:\n  - name: users\n    project: infra\n    api:\n      url: ...\n",
    "projects/infra.yaml": "projects:\n  - name: infra\n    git_url: \"...\"\n"
  }
}
JSON
```

```json
{
  "written": ["apis/users.yaml", "config.yaml", "projects/infra.yaml"],
  "etag": "9f2c…",
  "summary": {"credentials": 0, "projects": 1, "api_keys": 0,
              "webhooks": 0, "apis": 1, "scripts": 0},
  "reloaded": {
    "generation": 3,
    "applied": ["apis"],
    "apis": {"added": ["users"], "changed": ["groups"]},
    "api_keys": 2
  },
  "reload_pending": false
}
```

Validate first from CI, against the instance that will run it, without writing
anything:

```bash
curl -sS -X POST "$BASE/api/v1/config/validate" \
  -H "X-API-Key: $KEY" -H 'Content-Type: application/json' \
  -d '{"files": {"config.yaml": "...", "apis/users.yaml": "..."}, "prune": true}'
```

```json
{"valid": false, "errors": [
  "API 'users' references unknown project 'infra-typo'",
  "Script 'reload-dns': script.file '../reload.sh' must stay inside the checkout ('..' is not allowed)"
]}
```

Every problem at once, the same list `--check-config` prints — because it is
the same code, run against a staged copy of the directory the push would
produce.

A whole-directory push is one request body, so it lives under
`server.max_body_bytes` (default 2 MiB) like every other route; a directory
that outgrows it answers `413` naming the key to raise.

---

## What a reload can and cannot apply

What a reload can do is decided by **where each setting is read**. Anything
read per request or per tick can be swapped, because the next reader reads the
new value. Anything consumed once, at construction, cannot — the thing it built
is already running.

**Applied by a reload, no restart:**

| | Effect |
|---|---|
| `apis`, `scripts` | Used from their next run; a changed schedule restarts its task |
| `webhooks` | Used from the next event |
| `projects` | Pull tasks restart; a project that arrived with the reload is cloned in the background |
| `credentials`, `secrets:` | The resolver chain is rebuilt (which also drops each Vault's resolution cache) |
| `api_keys` | In force on the next request |
| `server.shutdown_grace_seconds` | The next shutdown — the drain reads whatever a reload set last |
| `server.metrics_require_auth` | The next scrape — the `/metrics` handler checks the flag per request |
| `server.cors_allowed_origins` | The next request — the CORS middleware reads the list per request, so adding a dashboard's origin is a push, not a fleet restart |
| `server.max_body_bytes` | The next request — raising the limit for a growing configuration push needs no restart |

**Needs a restart** — reported, never silently ignored:

`server.host`, `server.port`, `projects.dir`, `config_api.enabled`,
`server.ui.enabled`, `server.ui.session_ttl_seconds`, `server.ui.assets_dir`.

A write that touches one of them still lands on disk; the response names it:

```json
{"reloaded": {"generation": 4, "applied": ["apis"],
              "restart_required": ["server.port"]}}
```

`GET /api/v1/config` keeps reporting `restart_required` until a restart
actually adopts it, so the state is visible to anything that looks, not only
to whoever made the write — and `/metrics` exports the count as the
`unified_gitops_config_restart_required` gauge, so a fleet where some pods took
a push they could only partially adopt is one alert away, not one `GET` per
pod (see [observability](observability.md)).

### What happens to the running work

The scheduler does not try to reconfigure its tasks — it replaces them. The
outgoing generation is told to stop and finishes whatever it was in the middle
of (a pull is never cut in the middle, the same rule shutdown follows); a new
generation is spawned from the new configuration immediately. The two can
overlap for the length of one pull, which is safe because each checkout has
its own lock, and runs of one item never overlap whichever generation started
them. A run in progress finishes on the configuration it started with.

---

## Safety properties

- **Validated as a directory, before anything moves.** The proposed set is
  staged in a temporary directory and loaded there. A rejected change never
  touched the real one.
- **Rejected whole.** Cross-file references (an item naming a project and a
  credential, a restricted key naming a project) only make sense
  against the complete set, so the complete set is what is accepted or refused.
- **Atomic per file.** Every file is written to a temporary name and renamed
  into place, so no reader ever sees a half-written file.
- **A reload cannot turn authentication off.** A configuration with no API
  keys is legitimate — it is how a fresh instance starts — but *arriving* at
  it under a running process is refused with `409`. Write the file and restart
  if that is really the intent.
- **A key that cannot be resolved fails the whole operation.** If
  an API key names an env var that is not set, the write is refused
  **before** it is committed, rather than landing and then locking a consumer
  out.
- **A write without `?reload=true` says so.** `reload_pending: true` means the
  files are on disk and the process is still serving the previous
  configuration.

---

## Concurrency: ETags and If-Match

Every file's `ETag` is the sha256 of its bytes; the directory has one too, over
every file's name and hash. Send it back as `If-Match` and a write that would
clobber someone else's change is refused with `412` instead of winning
silently:

```bash
ETAG=$(curl -sS "$BASE/api/v1/config" -H "X-API-Key: $KEY" | jq -r .etag)
# … render the files …
curl -sS -X PUT "$BASE/api/v1/config" -H "If-Match: \"$ETAG\"" …
```

Content-addressed rather than mtime-based, on purpose: re-pushing identical
bytes changes nothing and does not change the ETag, so an idempotent pipeline
stays idempotent.

Omitting `If-Match` means "I am the only writer", which is the common case for
a single pipeline and stays unceremonious.

---

## Permissions and audit

Admin keys only, **reads included** — `config.yaml` and the credentials
describe the estate (which systems exist, which variable holds which
credential), which is exactly what a restricted consumer key has no business
enumerating.

Every write and reload emits an audit line under the `audit` target, with the
key that did it and the request id:

```
actor=pipeline action=config_write_reload resource=apis/users.yaml request_id=req-42 outcome=success
```

`RUST_LOG=audit=info` to isolate them. See [observability](observability.md).

---

## Making the directory writable

The API writes to `CONFIG_DIR`. Whatever is there must be writable **and
should survive a restart** — otherwise a push is applied to the running
process and then lost the next time the container starts from its image.

| Deployment | What to do |
|---|---|
| Kubernetes | Mount `CONFIG_DIR` from a PersistentVolumeClaim, or from an `emptyDir` seeded by an initContainer if losing pushes on restart is acceptable. A ConfigMap mount is read-only and cannot be used |
| Container on a host | A bind-mounted host directory, which is what an edge already uses |
| Systemd | The directory as-is, owned by the service user |

An instance whose directory is read-only will refuse writes with a `500`
naming the filesystem error; leave `config_api.enabled` off there.

---

## Metrics

| Metric | Meaning |
|---|---|
| `unified_gitops_config_writes_total{outcome}` | `success`, `rejected` (did not validate) or `error` (could not be written) |
| `unified_gitops_config_reloads_total{outcome}` | `success` or `invalid` |

The reload generation is in `GET /api/v1/config` rather than in a metric: it is
a property of a configuration, not a rate.
