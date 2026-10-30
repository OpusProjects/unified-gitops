# Scripts

A script item runs a file from a repository when a watched project changes, with what changed in
its environment: reload a DNS server after its zones change, run a playbook for the hosts whose
files moved, regenerate a report.

- [An item](#an-item)
- [Where the file lives](#where-the-file-lives)
- [What a script receives](#what-a-script-receives)
- [How it runs](#how-it-runs)
- [Running, dry runs and history](#running-dry-runs-and-history)

---

## An item

```yaml
# scripts/reload-dns.yaml
scripts:
  - name: reload-dns
    description: Reload DNS when a zone changes
    enabled: true                      # default true
    project: infra                     # the watched project: its changes run it
    labels: {team: net}
    trigger:                           # see how-it-works.md
      paths: [dns/**]
      on: [change, manual]             # the default
    credential: cred-dns               # its fields as CREDENTIAL_<FIELD>
    timeout_seconds: 300               # one execution (default 300, at most a day)
    retry: {attempts: 1}               # the default: a script runs once
    on_failure: continue               # continue (default) | stop
    script:
      project: ops-scripts             # where the file lives; default the watched project
      file: bin/reload-dns.sh          # relative to that project's checkout
      args: [--zone, $event.project]   # templated; no shell, nothing to quote
      interpreter: bash                # optional: for a file without its executable bit
      working_dir: dns                 # relative to the WATCHED checkout; default its root
      env: {MODE: apply}               # templated; not secret
      per: commit                      # commit (default) | file
      stdin: none                      # none (default) | event: the event as JSON
      success_exit_codes: [0]
      kill_grace_seconds: 10           # SIGTERM, then SIGKILL after this
      max_output_bytes: 65536          # the end of its output is kept
```

Only `project` and `script.file` are required.

## Where the file lives

The file can be in the watched repository, or in another (`script.project`, any configured
project). A separate repository is usually better: the people who can change data need not be the
people who can change code that runs with a credential, and one script can serve several
repositories. The other project is pulled on its own schedule; a commit there runs nothing, and the
next run uses it. Each run records both commits.

`file` and `working_dir` must stay inside their checkouts (no absolute paths, no `..`). A file
that runs itself needs its executable bit and a shebang in git; otherwise name an `interpreter`
(`bash`, `python3`, `/usr/bin/pwsh`), which receives the file as its first argument. When the
script's project has a virtualenv (`python_venv: true`), its `bin` goes first on `PATH`.

## What a script receives

| Variable | Is |
| --- | --- |
| `GITOPS_ITEM`, `GITOPS_PROJECT`, `GITOPS_TRIGGER` | The item, the watched project, and why it runs (`change`, `startup`, `schedule`, `manual`) |
| `GITOPS_CHECKOUT` | The watched checkout's path |
| `GITOPS_COMMIT`, `GITOPS_PREVIOUS_COMMIT` | The watched project's commit, and before the change (empty when not a change) |
| `GITOPS_SCRIPT_COMMIT` | The commit of the project the file is in |
| `GITOPS_CHANGED_FILES` | The selected files, one per line |
| `GITOPS_FILE`, `GITOPS_FILE_STATUS` | With `per: file`: this file, and `changed`, `present` or `removed` |
| `CREDENTIAL_<FIELD>` | Each field of the item's credential, upper case (`token` → `CREDENTIAL_TOKEN`) |
| The item's `env` | As written, templated |

`args` and `env` are templates over `$event` (and with `per: file`, `$file`: `path`, `name`,
`stem`, `ext`, `dir`, `status`). The environment starts **empty**: of the service's own variables a
script sees only `PATH`, `HOME`, `TMPDIR`, `LANG`, `LC_ALL`, `TZ`, `PYTHONPATH`, the proxy
variables and the CA-bundle variables. API keys and other items' credentials never reach it.
`env` may not set `GITOPS_*` or `CREDENTIAL_*`.

## How it runs

- **Once per commit**, or with `per: file` **once per selected file**, removed ones included (with
  `GITOPS_FILE_STATUS=removed`, for a script that cleans up).
- **Success** is an exit code in `success_exit_codes`. Anything else, a signal or a timeout fails
  the step; `retry.attempts` runs it again that many times in all.
- **A timeout** sends SIGTERM to the script's whole process group (what it started stops too),
  then SIGKILL after `kill_grace_seconds`.
- **Output**: stdout and stderr together, the last `max_output_bytes` kept (`[…]` marks a cut).
- **Runs of one item never overlap.** With `on_failure: stop`, a failed file ends the run.

## Running, dry runs and history

- A run starts on its trigger, or with `POST /api/v1/scripts/{name}/run` when `on` has `manual`.
- `POST /api/v1/scripts/{name}/run?dry_run=true` runs nothing and says what would run: the
  command, its arguments, where, and the **names** of the variables it would get (never their
  values).
- `GET /api/v1/scripts/{name}` shows its settings (the names of its `env`, never values) and its
  last 50 runs, each with the commits, and every execution's exit code, attempts, output and error.
- A run is told to the project's webhooks as `script_succeeded`, `script_failed` and
  `script_recovered` (with exit codes, never output).
- A restricted API key sees and runs the scripts of its projects.
