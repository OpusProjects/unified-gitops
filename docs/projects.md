# Projects

Git repositories the app watches and reads from: the files APIs send and scripts
read, and the scripts themselves. A project is a checkout the app clones and
keeps up to date — so changing what a system should look like, or rolling a
new script version, is a `git push`, not a deploy. Field reference lives in
[configuration → projects](configuration.md#projects).

A pull that brings commits is what runs the project's APIs and scripts and
tells its webhooks ([how it works](how-it-works.md)), so a project's interval
or schedule is how soon a commit is acted on; a pipeline that wants it at once
calls `POST /api/v1/projects/{id}/sync` after pushing. A failed pull is
recorded on the project, and runs go on against the checkout already on disk.

- [The lifecycle](#the-lifecycle)
- [Boot never blocks on git](#boot-never-blocks-on-git)
- [Python virtualenvs](#python-virtualenvs)
- [Keeping checkouts up to date](#keeping-checkouts-up-to-date)
- [Private repositories](#private-repositories)
- [Health and routes](#health-and-routes)

---

## The lifecycle

One directory per project id under `projects.dir` (default `./projects`),
created by a shallow clone and updated by fetch + hard reset — local drift in
a checkout is deliberately discarded, because the repository is the truth.

Three sync styles compose from two knobs:

| Style | Config | Behavior |
|---|---|---|
| Automatic | `sync_interval_seconds: N` | update at boot + every N seconds |
| Boot only | interval 0/absent | clone/update at boot, then frozen |
| Manual / pipeline-driven | `sync_on_boot: false`, interval 0/absent | an existing checkout is used **as-is**; updates only via `POST /api/v1/projects/{id}/sync` |

With `sync_on_boot: false` a *missing* checkout is still cloned — without the
scripts there is nothing to execute — so first bring-up needs no special
casing. For the manual style, keep `projects.dir` on a persistent volume: the
checkout IS the durable state.

Every git operation is bounded by the project's `timeout_seconds` (default
300); a timed-out clone or pull kills the git child rather than abandoning
it, fails the sync with a clear error, and lands in the project's health.

---

## Boot never blocks on git

The listener binds and serves **before** the project clones, so one
unreachable remote can delay that project's scripts — never `/healthz` or a
Kubernetes startup probe.

The clones run concurrently in a background task (boot waits for the slowest,
not the sum, each bounded by its `timeout_seconds`), and the scheduled pulls,
item schedules and `on: startup` runs start only after they have had their
bounded chance, so nothing races its own checkout's clone.

---

## Python virtualenvs

With `python_venv: true`, a `requirements.txt` in the checkout gets a real
virtualenv the app builds and maintains — the escape from "one PyPI package
means a derived image".

- Built after the clone and refreshed after any pull whose `requirements.txt`
  changed; an unchanged pull costs two file reads, not a pip run.
- Stored **outside** the checkout (`<projects.dir>/.venvs/<project>`), where
  the hard reset cannot wipe it.
- When a script item whose file is in this project runs, the venv's `bin/` leads its PATH — a
  `#!/usr/bin/env python3` shebang resolves to the venv's interpreter, and
  non-Python scripts in the same project are untouched.
- A failing install (typo'd package, unreachable index) **fails the project
  sync**, visible in its `sync_health` and bounded by `timeout_seconds` —
  instead of surfacing later as one import error per script run.

---

## Keeping checkouts up to date

Scripts are read from disk on every execution, so
an updated checkout takes effect on the next run — no restart, ever.

The periodic pull (`sync_interval_seconds`, or a cron `schedule` in UTC —
mutually exclusive; the boot clone happens either way, cron only paces the
re-pulls) runs with the scheduler's full failure machinery: startup jitter
for intervals, exponential backoff on failure, panic supervision, shutdown
draining. The on-demand route is how a pipeline in the
scripts repository rolls a new version deliberately:

```bash
curl -s -H "x-api-key: $ADMIN_KEY" -X POST $BASE/api/v1/projects/ops-scripts/sync
```

---

## Private repositories

The credential in the project's `credentials` (a list, as on every item, of at
most one; the singular `credential:` still loads, deprecated) authenticates
the clone and every pull —
without ever putting the secret on a command line (argv is world-readable in
`/proc` while git runs).

A `token` (or username/password) credential becomes a Basic-auth header passed
through `GIT_CONFIG_*` environment variables; an `ssh_key` credential sets
`GIT_SSH_COMMAND` pointing at the key file. See [secrets](configuration.md#credentialsyaml)
for how credentials resolve.

---

## Health and routes

A checkout stuck on a stale commit looks healthy from the outside —
`checkout_present` stays true while every pull fails — which is exactly what
the health block exists to expose.

| Route | Meaning |
|---|---|
| `GET /api/v1/projects` | Configured projects with `checkout_present`, sync settings, `sync_health` and `history`, their last pulls (admin-only) |
| `POST /api/v1/projects/{id}/sync` | Clone/update to the branch tip now; `502` with the git error on failure (admin-only) |

Every sync — boot, interval, on demand — records last attempt / last success
/ last error / consecutive failures per project, and `/metrics` exposes
`unified_gitops_project_sync_consecutive_failures{project}` and
`unified_gitops_project_sync_last_success_age_seconds{project}` at scrape time —
see [observability](observability.md).

Each pull is also kept in the project's `history`: its last 50, newest first,
in memory since the process started (a restart starts them over, as it does
the health). A row says when the pull started (`started_at`, and `age_seconds`
measured on the server, so a client's clock does not move it) and how long it took, why
(`scheduled`, `manual` or `startup`), who asked a manual one, how it ended
(`error`), and the commit it found the checkout at and left it at
(`revision_before`, `revision`): when the files or a script actually changed.

Both commits are read while the pull holds the checkout, so two pulls at once
never take credit for each other's move.
