# Notifications

A notification tells another system what happened to an application: drift found, a commit applied,
a sync that failed. It is a configuration item like the others, under `notifications:`. Every send
goes out from this instance, so nothing has to reach in.

- [Events](#events)
- [Types](#types)
- [The message](#the-message)
- [Ascender and AWX](#ascender-and-awx)
- [Sending, retries and what is kept](#sending-retries-and-what-is-kept)

---

## Events

Each run produces at most one event, so one change never arrives twice under two names.

| Event | When | Fires |
| --- | --- | --- |
| `drift_detected` | Differences at a commit already applied: someone changed the system outside git | On entering drift; with `auto_sync`, every revert |
| `out_of_sync` | A new commit waits to be applied (`auto_sync` off) | On entering the state |
| `sync_succeeded` | A sync applied at least one change from git: a commit or a rollback | Every such sync |
| `sync_failed` | A check or sync could not finish, or some operations failed | On entering the state |
| `back_in_sync` | In sync again after a failure or a difference | On entering the state |

- **Drift and commits are told apart by commit:** differences found at the commit the application
  was last in sync at were made on the system; at a newer commit, they come from git.
- **With `auto_sync`, a reverted drift is one `drift_detected`** with `reverted: true`, not a
  `drift_detected` plus a `sync_succeeded`.
- **History is in memory:** after a restart, the first run fires only failures and applied
  changes, so a restart does not report every pending difference again.

## Types

The `type` names the receiver, and its settings live in the block of the same name.

```yaml
# notifications/ntf-failures.yaml
notifications:
  - name: ntf-failures
    description: "Any application that fails"
    type: webhook
    events: [sync_failed, back_in_sync]
    applications: []                     # empty = every application
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

| `auth` | What is sent | Credential field |
| --- | --- | --- |
| `bearer`, or left out with a credential | `Authorization: Bearer <token>` | `token` |
| `basic` | `Authorization: Basic …` | `username`, `password` |
| `{header: X-Name}` | `X-Name: <token>` | `token` |
| `none` | nothing | — |

- **`body`** is a template: `{{field}}` takes any field of [the message](#the-message), plus
  `{{revision_short}}`. Text is written escaped for a JSON string (write the quotes yourself); lists,
  numbers and booleans as JSON. A name the message does not have fails the configuration.
- **`sign`** adds `X-Hub-Signature-256: sha256=<hex>` over the body, from the credential field
  `secret`. `header`, `prefix` and `secret_field` change those.
- **Never in the YAML:** a token, a password, or a URL that carries one. They belong in
  credentials, which come from env vars or Vault.

The `ascender` and `awx` types start a job template; see [Ascender and AWX](#ascender-and-awx).

## The message

The default body, and the fields a `body` template can use:

```json
{
  "event": "drift_detected",
  "notification": "ntf-ascender-drift",
  "application": "portainer",
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
- **`cause`** is `commit`, `drift` or `rollback`; **`reverted`** is set on `drift_detected` only.
- **`errors`**: the first five lines of a failure, for `sync_failed`.
- **`url`** links the application in the UI when `server.url` is set.

## Ascender and AWX

Both start a job template (or a workflow job template) in two ways. Their routes are the same today;
they are separate types so each follows its own product.

```yaml
# notifications/ntf-ascender-drift.yaml
notifications:
  - name: ntf-ascender-drift
    description: "Drift report in Ascender"
    type: ascender                       # or awx, with an awx: block
    events: [drift_detected]
    applications: [portainer]
    ascender:
      url: "https://ascender.example.com"   # the controller, without /api
      job_template: 123                  # or workflow_job_template: 45
      trigger: webhook                   # webhook (default) | launch
      credentials: [cred-ascender-jt123]
      # extra_vars: {report: drift}      # launch only
```

| `trigger` | Route | The credential's `token` | The playbook reads the message as |
| --- | --- | --- | --- |
| `webhook` | `/api/v2/job_templates/{id}/gitlab/` | The template's webhook key | `ascender_webhook_payload` (Ascender), `tower_webhook_payload` (both) |
| `launch` | `/api/v2/job_templates/{id}/launch/` | A user's OAuth token | `unified_gitops`, beside `extra_vars` |

- **`webhook`** needs the template's **Webhooks** option on, with service **GitLab**: the
  controller then gives it a key, compared as is, and no user is needed. Any event is accepted.
- **`launch`** needs a user allowed to run the template, and **Prompt on launch** for its variables.
  Without it the controller drops `extra_vars`; the send reports that as a failure instead of
  starting a job without the event.
- The last send records the job a `launch` started.

## Sending, retries and what is kept

- **A queue, not the run:** a run queues its event and moves on; a worker sends, eight at a time,
  so a slow receiver never holds up a check or a sync. A full queue (1000) drops the newest message,
  counted in the metrics.
- **Retries:** an unanswered send, a `429` or a `5xx` is retried `retries` times, waiting 2, 4, 8 …
  up to 60 seconds. A `4xx` is not: a wrong key or template does not fix itself.
- **Redirects are not followed:** the body and its credential go only where the configuration says.
- **What is kept:** each notification's last send (event, application, result, status, job,
  error), in memory, on `GET /api/v1/notifications`. `POST /api/v1/notifications/{id}/test`
  sends a test message now, once, and answers the outcome.
- **Metrics:** `unified_gitops_notifications_total{notification,event,outcome}` (`success`,
  `error`, `dropped`) and `unified_gitops_notification_duration_seconds`.
- **Reload:** notifications reload like applications. A message keeps the definition it was queued
  with.
