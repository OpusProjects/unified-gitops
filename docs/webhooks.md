# Webhooks

A webhook tells another system what happened to an application (drift found, a commit applied,
a sync that failed) or to a project's repository (new commits pulled, a pull that fails). It is a
configuration item like the others, under `webhooks:`. Every send goes out from this instance, so
nothing has to reach in.

- [Events](#events)
- [Applications and projects](#applications-and-projects)
- [Types](#types)
- [The message](#the-message)
- [Ascender and AWX](#ascender-and-awx)
- [Sending, retries and what is kept](#sending-retries-and-what-is-kept)

---

## Events

Application events come from a run (a check or a sync) of one application. Each run produces at
most one, so one change never arrives twice under two names.

| Event | When | Fires |
| --- | --- | --- |
| `drift_detected` | Differences at a commit already applied: someone changed the system outside git | On entering drift; with `auto_sync`, every revert |
| `out_of_sync` | A new commit waits to be applied (`auto_sync` off) | On entering the state |
| `sync_succeeded` | A sync applied at least one change from git: a commit or a rollback | Every such sync |
| `sync_failed` | A check or sync could not finish, or some operations failed | On entering the state |
| `back_in_sync` | In sync again after a failure or a difference | On entering the state |

- **Drift and commits are told apart by the application's own folder:** differences found while its
  folder in git is unchanged since it was last in sync were made on the system; after its folder
  changed, they come from git. Commits to other folders change nothing here. Without git to ask
  (a checkout that is not a repository), the `cause` is left out rather than guessed.
- **With `auto_sync`, a reverted drift is one `drift_detected`** with `reverted: true`, not a
  `drift_detected` plus a `sync_succeeded`.
- **History is in memory:** after a restart, the first run fires only failures and applied
  changes, so a restart does not report every pending difference again.
- **While a rollback stands,** scheduled checks report only failures: they compare the system
  with git's HEAD on purpose, and what they find is the rollback. The next manual sync ends it.

Project events come from a pull of a project's repository: by its own schedule, before an
application's run, by `POST /api/v1/projects/{id}/sync`, or at startup. Each pull produces at most
one of each.

| Event | When | Fires |
| --- | --- | --- |
| `commit_pulled` | A pull brought new commits | Every pull that moves the checkout |
| `pull_failed` | The repository could not be pulled: unreachable, refused, or timed out | When the project starts failing |
| `pull_recovered` | Pulls work again after failing | When the project stops failing |

- **A first clone is not a commit:** there is nothing to compare it with.
- **One commit, two facts:** with `auto_sync`, a commit sends `commit_pulled` from the pull, then
  `sync_succeeded` from each application it changed. The repository moved, then a system did;
  `events` picks which to hear.
- **While pulls fail,** applications keep running against the commit they have. `pull_failed`
  is what says their repository stopped moving.

## Applications and projects

A webhook selects what it hears with `applications`, `projects`, or both; the two are combined.

```yaml
webhooks:
  - name: wh-portainer
    type: webhook
    events: [commit_pulled, pull_failed, drift_detected, sync_failed]
    projects: [prj-portainer-casc]       # the project's own events, and every application on it
    applications: [vault-config]         # one more application, wherever it reads from
    webhook:
      url: "https://alerts.example.com/hooks/unified-gitops"
```

- **`projects`** is `all`, or projects by name. It hears the projects' own events, and the
  application events of every application reading from them, including applications added later.
- **`applications`** is `all`, or applications by name: their events only. It narrows a webhook to
  some applications of a repository, when they belong to different teams or receivers.
- **Project events need `projects`:** a webhook listing `commit_pulled`, `pull_failed` or
  `pull_recovered` without naming a project fails the configuration, since it could never send.
- **Neither, or both empty:** the webhook hears nothing; Send test still works.

## Types

The `type` names the receiver, and its settings live in the block of the same name.

```yaml
# webhooks/wh-failures.yaml
webhooks:
  - name: wh-failures
    description: "Any application that fails"
    type: webhook
    events: [sync_failed, back_in_sync]
    applications: all                    # all, or a list; left out or [] = none
    webhook:
      url: "https://alerts.example.com/hooks/unified-gitops"
      method: POST                       # POST (default), PUT or PATCH
      credentials: [cred-alerts]
      auth: bearer                       # bearer (default) | basic | none | {header: X-Name}
      headers:
        X-Source: unified-gitops
      # body: '{"text": "{{application}}: {{event}}"}'   # optional; default is the message
      # sign: {}                         # optional HMAC-SHA256 of the body
      timeout_seconds: 30
      retries: 3
```

- **`applications`** and **`projects`** are `all` (present and future), or names; see
  [Applications and projects](#applications-and-projects). Left out or `[]`, they select nothing.

| `auth` | What is sent | Credential field |
| --- | --- | --- |
| `bearer`, or left out with a credential | `Authorization: Bearer <token>` | `token` |
| `basic` | `Authorization: Basic …` | `username`, `password` |
| `{header: X-Name}` | `X-Name: <token>` | `token` |
| `none` | nothing | — |

- **`body`** is a template: `{{field}}` takes any field of [the message](#the-message), plus
  `{{revision_short}}`. Text is written escaped for a JSON string (write the quotes yourself); lists,
  numbers and booleans as JSON. A field with no value is `""` inside quotes and `null` outside
  them, so the body stays valid JSON. A name the message does not have fails the configuration.
- **`sign`** adds `X-Hub-Signature-256: sha256=<hex>` over the body, from the credential field
  `secret`. `header`, `prefix` and `secret_field` change those.
- **Never in the YAML:** a token, a password, or a URL that carries one. They belong in
  credentials, which come from env vars or Vault.
- **One header, one source:** a fixed header with the name the auth or the signature writes (say,
  `Authorization` with a bearer credential) fails the configuration.

The `ascender` and `awx` types start a job template through its webhook; see
[Ascender and AWX](#ascender-and-awx).

## The message

The default body, and the fields a `body` template can use. An application event:

```json
{
  "event": "drift_detected",
  "webhook": "wh-ascender-drift",
  "application": "portainer",
  "project": "prj-portainer-casc",
  "description": "Portainer settings, environments and users",
  "status": "synced",
  "previous_status": "synced",
  "cause": "drift",
  "reverted": true,
  "revision": "39d6efb30883db193fcda905e0aa3def7b5d1f30",
  "applied": 1,
  "changes": [
    {"kind": "user", "key": "someone.else", "op": "delete", "fields": [], "ok": true}
  ],
  "errors": [],
  "at": "2026-10-26T15:42:10Z",
  "url": "https://gitops.example.com/applications/portainer"
}
```

- **`changes`** names each object, its operation and the names of the fields that changed, never
  their values: a value can be a secret, and the message leaves this instance.
- **`cause`** is `commit`, `drift` or `rollback` (left out when it cannot be told); **`reverted`** is
  set on `drift_detected` only.
- **`errors`**: the first five lines of a failure, for `sync_failed`, as the connector or the
  system reported them. Unlike `changes`, that is free text: a connector that prints a value in its
  error sends it on.
- **`url`** links the application in the UI when `server.url` is set.

A project event has the same envelope, with `application` null and `description` the project's:

```json
{
  "event": "commit_pulled",
  "webhook": "wh-portainer",
  "application": null,
  "project": "prj-portainer-casc",
  "description": "Portainer configuration as code",
  "revision": "8b1f0c2e4d6a8b0c1e3f5a7b9c1d3e5f7a9b1c3d",
  "previous_revision": "39d6efb30883db193fcda905e0aa3def7b5d1f30",
  "commits": [
    {
      "revision": "8b1f0c2e4d6a8b0c1e3f5a7b9c1d3e5f7a9b1c3d",
      "author": "Jane Doe",
      "message": "feat: add the ops team to Portainer",
      "at": "2026-10-27T09:12:44Z"
    }
  ],
  "more_commits": false,
  "forced": false,
  "applications": ["portainer"],
  "at": "2026-10-27T09:13:02Z",
  "url": "https://gitops.example.com/projects/prj-portainer-casc"
}
```

- **`commits`**, newest first, at most 20, each with its author and the first line of its message.
  `more_commits` is true when some were left out. Checkouts are shallow, so listing them may fetch
  up to 20 commits of history; that happens only when a webhook wants `commit_pulled`.
- **`forced`** is true when the history was rewritten (a force push) and the new one could be seen
  whole. After a rewrite, only the new tip is listed, with `more_commits` true.
- **`applications`**: those whose folder the commits changed. `null` when git could not compare.
- **`pull_failed`** carries `errors` (the git error, one line) and `consecutive_failures`;
  **`pull_recovered`** carries `failing_seconds`, how long pulls had been failing.

## Ascender and AWX

Both start a job template (or a workflow job template) through the template's own webhook receiver.
Their routes are the same today; they are separate types so each follows its own product.

```yaml
# webhooks/wh-ascender-drift.yaml
webhooks:
  - name: wh-ascender-drift
    description: "Drift report in Ascender"
    type: ascender                       # or awx, with an awx: block
    events: [drift_detected]
    applications: [portainer]
    ascender:
      url: "https://ascender.example.com"   # the controller, without /api
      job_template: 123                  # or workflow_job_template: 45
      credentials: [cred-ascender-jt123] # its token is the template's webhook key
```

- **On the template:** turn its **Webhooks** option on, with service **GitLab**. The controller then
  gives it a key; put that in a credential, from Vault. No user and no token are needed, and the key
  can start that one template only.
- **The send:** `POST /api/v2/job_templates/{id}/gitlab/` with the key in `X-Gitlab-Token`, compared
  as is, and the message as the body. Any event is accepted.
- **In the playbook:** the message is `ascender_webhook_payload` (Ascender), `awx_webhook_payload`
  (AWX), or `tower_webhook_payload` (both).

## Sending, retries and what is kept

- **A queue, not the run:** a run queues its event and moves on; a worker sends, eight requests at a
  time. A request holds its place only while it is being sent, never through the wait before a
  retry, so a receiver that is down slows its own messages, not the others'. Past 1000 messages
  waiting or being sent, the newest is dropped, counted in the metrics.
- **Retries:** an unanswered send, a `429` or a `5xx` is retried `retries` times, waiting 2, 4, 8 …
  up to 60 seconds. A `4xx` is not, nor a request that cannot be built (a missing credential field,
  a credential with a line break in it): those are the configuration's to fix.
- **Shutdown:** after the last runs finish, what they queued is still sent, within
  `server.shutdown_grace_seconds`; whatever is left then is dropped and logged.
- **Redirects are not followed:** the body and its credential go only where the configuration says.
- **What is kept:** each webhook's last 50 sends (event, application or project, result,
  status, error), in memory: the last on `GET /api/v1/webhooks`, all of them on
  `GET /api/v1/webhooks/{id}`. `POST /api/v1/webhooks/{id}/test` sends a test message
  now, once, and answers the outcome.
- **Metrics:** `unified_gitops_webhooks_total{webhook,event,outcome}` (`success`,
  `error`, `dropped`) and `unified_gitops_webhook_duration_seconds`.
- **Reload:** webhooks reload like applications. A message keeps the definition it was queued
  with.
