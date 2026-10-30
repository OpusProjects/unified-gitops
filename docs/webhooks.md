# Webhooks

A webhook tells a receiver what happened to a project: commits pulled, a pull that fails, an API or
a script that ran. It is one HTTP request to any receiver (a chat, an incident tool, a job runner,
your own service), shaped by the item. Every send goes out from this instance, so nothing has to
reach in.

- [An item](#an-item)
- [Events](#events)
- [The message](#the-message)
- [Shaping the body](#shaping-the-body)
- [Following what it started](#following-what-it-started)
- [Sending, retries and what is kept](#sending-retries-and-what-is-kept)

---

## An item

```yaml
# webhooks/notify-failures.yaml
webhooks:
  - name: notify-failures
    description: Failures to the ops channel
    enabled: true                      # default true
    project: infra                     # the watched project whose events it tells
    labels: {team: ops}
    events: [pull_failed, api_failed, script_failed]
    trigger:                           # commit_pulled only: commits touching these files
      paths: [vcenter/**]
      ignore_paths: ["**/*.md"]
    credential: cred-hook              # read where webhook.auth / signature say
    timeout_seconds: 30                # one attempt (default 30, at most 300)
    retry:                             # see http-requests.md
      attempts: 4
    webhook:
      url: https://hooks.example.com/services/x
      method: POST                     # default POST
      content_type: json               # json (default) | form | text
      body: {text: "$event.project: $event.event"}   # left out: the message as it is
      # every request setting: headers, query, auth, signature, statuses, tls, proxy,
      # rate_limit, follow_redirects (http-requests.md)
      # wait: {...}                    # follow what it started
```

Only `project`, `events` and `webhook.url` are required.

## Events

| Event | When | Fires |
| --- | --- | --- |
| `commit_pulled` | A pull brought commits | Every such pull; with `trigger.paths`, only when they touched a selected file |
| `pull_failed` | The repository could not be pulled: unreachable, refused, timed out | When the project starts failing |
| `pull_recovered` | Pulls work again after failing | When it stops failing |
| `api_succeeded` | A run of one of the project's APIs worked | Every such run |
| `api_failed` | A run of one of its APIs failed (a step failed, or it could not start) | Every such run |
| `api_recovered` | The first run of an API that works after failures | Once per recovery |
| `script_succeeded`, `script_failed`, `script_recovered` | The same for its scripts | As for APIs |

- **A first clone is not a commit:** there is nothing to compare it with.
- **One commit, several facts:** a commit sends `commit_pulled` from the pull, then an
  `api_*` / `script_*` event from each item it ran. `events` picks which to hear.
- **Dry runs are never told.**
- **Pull state is in memory:** after a restart, a project that is still failing is told again.

## The message

Every event has the envelope; each adds its own fields (null where it has none):

```json
{
  "event": "api_failed",
  "project": "infra",
  "description": "Portainer users",
  "revision": "4f1c2a9e…",
  "revision_short": "4f1c2a9",
  "at": "2026-10-30T17:03:27Z",
  "url": "https://gitops.example.com/apis/portainer-users",
  "webhook": "notify-failures",

  "item": "portainer-users",
  "kind": "api",
  "trigger": "change",
  "ok": false,
  "duration_ms": 812,
  "steps": [
    {"file": "users/jane.json", "action": "update", "status": 400, "exit_code": null, "ok": false, "error": "HTTP 400: …"}
  ],
  "errors": ["HTTP 400: …"]
}
```

| Field | In | What |
| --- | --- | --- |
| `event`, `project`, `revision`, `revision_short`, `at`, `webhook` | every event | What, of which project, at which commit, when, to which webhook |
| `description` | every event | The project's (pull events) or the item's (run events) |
| `url` | every event | Its page in the UI, when `server.url` is set |
| `previous_revision`, `commits`, `more_commits`, `forced`, `files` | `commit_pulled` | The commits (author, subject, time: up to 20), whether there were more, a force-push, and the files they touched (up to 200) |
| `errors`, `consecutive_failures` | `pull_failed` | Why, and how many pulls in a row |
| `failing_seconds` | `pull_recovered` | How long it was failing |
| `item`, `kind`, `trigger`, `ok`, `duration_ms`, `steps`, `errors` | `api_*`, `script_*` | Which item, why it ran, how it ended, and each step (up to 50) |

File contents, request bodies and script output never leave in a message: a step says what was
done, not with what.

## Shaping the body

Left out, the body is the message as JSON. With `body`, it is whatever the receiver expects,
written in YAML with `$event` templates:

```yaml
webhook:
  url: https://jobs.example.com/api/v2/job_templates/42/launch/
  auth: bearer                         # the item's credential's token
  body:
    limit: vcenter_hosts               # a fixed key the receiver understands
    extra_vars:
      gitops: $event                   # the whole message, as an object
      files: $event.files              # one field, as JSON (a list stays a list)
      site: madrid                     # your own keys
```

```yaml
webhook:
  url: https://chat.example.com/hooks/x
  body:
    text: "$event.project: $event.event at $event.revision_short ($event.url)"
```

- `$event` is the message; `$event.<field>` one of its fields; `$event.commits.0.author` goes
  deeper. `${event.project}x` ends a name where text follows; `$$` is a `$`.
- **A value that is only a variable keeps its JSON type** (`$event.files` is a list, `$event` an
  object). Inside text, a value is written as text: strings as they are, null as nothing, anything
  else as JSON.
- Templates are checked when the configuration loads: a field the message does not have
  (`$event.prjoect`) is an error then, not an empty value at 3 a.m.
- `content_type: form` sends a mapping URL-encoded (nested values as JSON); `content_type: text`
  sends text as it is. `method: GET` sends no body.
- `url`, `headers` and `query` are templated the same way.
- Every request goes with `User-Agent: unified-gitops/<version>` and `X-Unified-Gitops-Event`.

## Following what it started

A receiver that starts something (a job, a pipeline) usually answers with where to watch it.
`wait` asks there until it is done, and the send succeeds or fails with it:

```yaml
webhook:
  url: https://jobs.example.com/api/v2/job_templates/42/launch/
  wait:
    url: https://jobs.example.com$response.url      # $response: what the request answered
    until: $poll.status == successful                # $poll: what the URL answers now
    fail_if: $poll.status in [failed, error, canceled]
    interval_seconds: 10                             # default 10
    timeout_seconds: 3600                            # default 3600, at most a day
```

Conditions are `$poll.<path> == value`, `!=`, `in [a, b]` and `not in [a, b]`, compared as text.
The polls use the webhook's auth, TLS and proxy. A "Send test" never waits.

## Sending, retries and what is kept

- **Queued, never in the way.** A pull queues its messages and goes on; a worker sends them, at
  most 8 at once. The queue holds 1000; past that a message is dropped, logged and counted
  (`unified_gitops_webhooks_total{outcome="dropped"}`).
- **Retried as `retry` says**: on no answer, and on the statuses in `retry_on_status` (429 and
  5xx by default), honouring `Retry-After`. A 4xx is final: it will not fix itself.
- **History.** Each webhook keeps its last 50 sends in memory (`GET /api/v1/webhooks/{name}`):
  the event, when, how many attempts, the status, and why it failed.
- **"Send test"** (`POST /api/v1/webhooks/{name}/test`) sends a message with `event: test` once,
  now, and answers how it went.
- **Shutdown** waits for queued messages up to `server.shutdown_grace_seconds`.
- **Where it sends** is shown as scheme and host only: a webhook URL's path often is its secret.
