# APIs

An API item calls an API when a watched project changes, with what changed: one request per
changed file, its contents as the body, or one request per commit. It is how a file in git becomes
an object in a system: a user in Portainer, a record in an IPAM, a job template, a firewall rule.

- [An item](#an-item)
- [Per file](#per-file)
- [Upsert, compare and delete](#upsert-compare-and-delete)
- [Per commit](#per-commit)
- [Running, dry runs and history](#running-dry-runs-and-history)

---

## An item

```yaml
# apis/portainer/users.yaml
apis:
  - name: portainer-users
    description: Portainer users from git
    enabled: true                      # default true
    project: infra                     # the watched project
    labels: {team: ops}
    trigger:                           # see how-it-works.md
      paths: [portainer/users/*.json]
      ignore_paths: []
      on: [change, manual]             # the default
      debounce_seconds: 0
    credential: cred-portainer
    timeout_seconds: 30                # one request (default 30, at most 300)
    retry: {attempts: 4}               # see http-requests.md
    on_failure: continue               # continue (default) | stop: a failed file ends the run
    api:
      url: https://portainer.example.com/api/users/$file.stem
      method: PUT                      # default PUT
      # every request setting: headers, query, auth, signature, statuses, tls, proxy,
      # rate_limit, follow_redirects (http-requests.md)
      per: file                        # file (default) | commit
      body: $doc                       # the default per file; a template
      format: json                     # json | yaml | raw; default by extension
      content_type: application/json   # default by body
      upsert: {...}
      compare: {...}
      on_delete: {...}
      order: [portainer/users/admin*]
```

Only `project` and `api.url` are required.

## Per file

Each file the trigger selects (those a commit touched, or every one in the checkout for a
scheduled, startup or manual run) is one request, in path order (`order` puts the files matching
its patterns first, in its order).

| Template | Is |
| --- | --- |
| `$file.path` | `users/team/jane.json` |
| `$file.name`, `$file.stem`, `$file.ext`, `$file.dir` | `jane.json`, `jane`, `json`, `users/team` |
| `$file.status` | `changed` (a commit touched it), `present` (a full run), `removed` |
| `$doc` | The file read: `.json` as JSON, `.yaml`/`.yml` as YAML (`0644` stays a string), anything else as text; `format` overrides |
| `$event.<field>` | The commit: `project`, `revision`, `revision_short`, `previous_revision`, `files`, `trigger` |

The body is `$doc` unless `body` says otherwise:

```yaml
body:
  user: $doc                           # the whole file, as an object
  source: "$event.project@$event.revision_short"
```

A body that is text (a raw file) goes as `text/plain`; anything else as JSON. `content_type`
overrides the header. `GET`, `HEAD` and `DELETE` send no body.

## Upsert, compare and delete

```yaml
api:
  url: https://portainer.example.com/api/users/$file.stem
  upsert:                              # create or update, as the API says it exists
    exists: {url: https://portainer.example.com/api/users/$file.stem}   # GET; 2xx: it does, 404: not
    create: {method: POST, url: https://portainer.example.com/api/users}
    update: {method: PUT}              # each defaults: GET / POST / PUT at api.url
  compare:                             # send nothing when it already matches
    url: https://portainer.example.com/api/users/$file.stem             # default: exists, or api.url
    ignore_fields: [Id, meta.created]
  on_delete:                           # a removed file
    method: DELETE                     # default DELETE
    url: https://portainer.example.com/api/users/$file.stem             # default api.url
```

- **compare** reads the live object and skips the file when every field the body names already has
  that value (numbers by value; lists element by element; fields the API adds are not compared).
  The step records the names of the fields that differed, never their values.
- **upsert** reads first: a 2xx means update, a 404 create; any other answer fails the step. One
  read serves both when compare and exists read the same URL.
- **on_delete** runs for a file a commit removed; without it, a removed file is skipped (and says
  so). A removed file has no `$doc`: its URL may name `$file` only.

## Per commit

```yaml
api:
  url: https://ci.example.com/api/rebuild
  method: POST
  per: commit
  body: {files: $event.files, commit: $event.revision}
```

One request per run, with no `$file` or `$doc`; no body unless `body` gives one. Upsert, compare,
on_delete and order are per file only.

## Running, dry runs and history

- A run starts on its trigger (see [how-it-works.md](how-it-works.md)), or with
  `POST /api/v1/apis/{name}/run` when `on` has `manual`.
- `POST /api/v1/apis/{name}/run?dry_run=true` builds every request and makes the reads compare and
  upsert make, but writes nothing: each step carries the body it would send. Always allowed.
- `GET /api/v1/apis/{name}` shows its settings (never a credential or a header value) and its last
  50 runs: why, by whom, at which commit, and each step's file, action (`send`, `create`, `update`,
  `delete`, `skip`), method, URL (without its query), status, attempts and error.
- A run is told to the project's webhooks as `api_succeeded`, `api_failed` and `api_recovered`.
- A restricted API key sees and runs the APIs of its projects; another's read as absent.
