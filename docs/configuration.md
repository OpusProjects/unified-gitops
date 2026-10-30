# Configuration

Everything is YAML in one directory (`CONFIG_DIR`, default `./config`), one item per file:
`config.yaml` at the root holds the server settings, and every other `*.yaml` / `*.yml`
file, at any depth, holds items under a typed top-level key. Parsing is strict: an unknown key fails
startup naming the file and the key. `unified-gitops --check-config [dir]` validates the directory
(`dir`, else `CONFIG_DIR`, else `./config`) exactly as startup does, and exits.

```
/etc/unified-gitops/
├── config.yaml                        # required, at the root, under this name
├── projects/                          # folders are for people: any names, any depth, or none
│   ├── infra.yaml
│   └── ops-scripts.yaml
├── apis/
│   └── portainer/
│       └── users.yaml
├── scripts/
│   └── reload-dns.yaml
├── webhooks/
│   └── notify-failures.yaml
├── credentials/
│   └── cred-portainer.yaml
└── api_keys/
    └── key-pipeline.yaml
```

| Top-level key  | Holds                                                         |
| -------------- | ------------------------------------------------------------- |
| `projects`     | The git repositories this instance watches (and the ones scripts live in) |
| `apis`         | APIs to call when a project changes ([apis.md](apis.md))       |
| `scripts`      | Scripts to run when a project changes ([scripts.md](scripts.md)) |
| `webhooks`     | What to tell, and where ([webhooks.md](webhooks.md))           |
| `credentials`  | Where each credential's secrets come from (never the values)  |
| `api_keys`     | API keys by name and role (the values come from env vars)     |

- **The key says what an item is, never the folder.** A file in `scripts/` holding `apis:` is read
  as an API.
- **One item per file is the convention**, not a rule: a file may hold several items, of several
  types. A `name` defined twice is an error naming both files.
- **`name` is the identity**: what other items reference, what the API and the UI use. Letters,
  digits, `-`, `_` and `.`. `description` is optional, for people.
- **References drop the `_id`:** `project`, `credential` (one per item; a project's is
  `credentials`, a list of at most one, and its singular `credential:` still loads, deprecated).
- **Every item that acts or tells** (`apis`, `scripts`, `webhooks`) names the one `project` it
  watches, and takes `description`, `enabled` (default true) and `labels` (free-form, for finding
  it).
- Hidden files and folders are skipped; symlinks are followed (a ConfigMap mount is links).
- A path may only use letters, digits, `-`, `_` and `.` in each segment.

## config.yaml

```yaml
server:
  host: "0.0.0.0"
  port: 8183
  cors_allowed_origins: []         # [] = no CORS headers; ["*"] = any origin
  metrics_require_auth: false      # true = /metrics needs a key or a session
  shutdown_grace_seconds: 20       # how long in-flight runs may finish at shutdown
  max_body_bytes: 2097152
  ui:
    enabled: true                  # login page, sessions, the browsable API
    assets_dir: "/usr/share/unified-gitops/ui"   # the web UI at "/"
    session_ttl_seconds: 3600
    # home: "/"                    # where a sign-in with no ?next= lands
    # ldap: {...}                  # see ldap-login.md

projects:
  dir: "/var/lib/unified-gitops/projects"  # one checkout per project

# config_api:
#   enabled: true                  # read/write this directory over HTTP (admin only)
```

Restart-only (a reload reports them in `restart_required`): `server.host`, `server.port`,
`server.ui.enabled`, `server.ui.session_ttl_seconds`, `server.ui.assets_dir`, `projects.dir`, `config_api.enabled`. Everything else reloads live.

## projects

```yaml
# projects/infra.yaml
projects:
  - name: infra
    description: "Infrastructure as files"
    git_url: "https://github.example.com/ops/infra.git"
    branch: main                       # default main
    credentials: [cred-github]         # private repos: one token or ssh_key credential
    sync_interval_seconds: 60          # periodic pull: how soon a commit is acted on
    # schedule: "0 * * * *"            # cron (UTC) instead of the interval
    timeout_seconds: 300
    sync_on_boot: true
    python_venv: false                 # true = build a virtualenv from its requirements.txt
```

A pull that brings commits is what runs the project's APIs and scripts and tells its webhooks, so
the interval (or the schedule) is how soon a commit is acted on. A pipeline that wants it at once
calls `POST /api/v1/projects/{id}/sync` after pushing. See [projects.md](projects.md).

## apis, scripts and webhooks

Each has its own page with every option: [apis.md](apis.md), [scripts.md](scripts.md),
[webhooks.md](webhooks.md), and [http-requests.md](http-requests.md) for the request settings APIs
and webhooks share.

```yaml
# apis/portainer/users.yaml
apis:
  - name: portainer-users
    project: infra
    trigger: {paths: [portainer/users/*.json]}
    credential: cred-portainer
    api:
      url: https://portainer.example.com/api/users/$file.stem
      method: PUT

# scripts/reload-dns.yaml
scripts:
  - name: reload-dns
    project: infra
    trigger: {paths: [dns/**]}
    script:
      project: ops-scripts
      file: reload-dns.sh

# webhooks/notify-failures.yaml
webhooks:
  - name: notify-failures
    project: infra
    events: [pull_failed, api_failed, script_failed]
    webhook:
      url: https://hooks.example.com/services/x
```

## credentials and api_keys

As in unified-api. A credential names env vars, a file or a Vault path, never a value:

```yaml
# credentials/cred-portainer.yaml
credentials:
  - name: cred-portainer
    description: "Portainer API user"
    type: username_password            # username_password | token | ssh_key | vault
    env_vars:                          # field → the env var holding it
      username: PORTAINER_USERNAME
      password: PORTAINER_PASSWORD
    # vault: cred-vault                # or Vault: the type: vault credential to read from
    # vault_path: "ops/portainer"      #   and the KV v2 path under its mount
    # secret_file: "/run/secrets/portainer.json"
```

A Vault is a credential too, of `type: vault`: the
credentials that read from it name it with `vault:` and say where with `vault_path`. Several Vaults
can coexist, each with its own login and cache.

```yaml
# credentials/cred-vault.yaml — the connection to a Vault
credentials:
  - name: cred-vault
    description: Vault
    type: vault
    address: https://vault.example.com:8200
    mount: secret                      # KV v2 mount (default "secret")
    env_vars:
      token: VAULT_TOKEN               # token auth: re-read per resolution
    # kubernetes_role: unified-gitops  # OR Kubernetes auth (with an optional jwt_path)
    timeout_seconds: 10                # bound on every request (default 10)
    cache_ttl_seconds: 60              # how long a value read here is reused
                                       # (default 60; 0 = every resolution)
```

| `type: vault` field | Notes |
|---|---|
| `address` | Required. `http://` or `https://` |
| `mount` | The KV v2 mount `vault_path` values live under (default `secret`) |
| `env_vars.token` / `kubernetes_role` | How to log in: exactly one. `jwt_path` goes with `kubernetes_role` |
| `timeout_seconds` | Bound on every request (default 10) |
| `cache_ttl_seconds` | How long a value read from this Vault is reused (default 60; `0` = every resolution): what keeps busy schedules from becoming a request storm, and also how long a rotation takes to show |

Credentials *without* a `vault_path` keep resolving from env/files, uncached, so adoption is per
credential. A Vault credential is never handed to a project, API, script, webhook or LDAP bind:
naming one there is refused, since its token is not a login.

The `secrets:` block in `config.yaml` (`cache_ttl_seconds`, and a single `vault:` with `token_env`)
still loads but is **deprecated**: startup and `--check-config` say what to write instead, and a
later release refuses it. While it is set, its `cache_ttl_seconds` still caches every credential, as
it did (in place of each Vault's own cache, never on top of it), and a `vault_path` with no `vault:`
reads from its `vault:`, cached for 60 seconds when `cache_ttl_seconds` is absent, as 0.3.0 did by
default.

An API key names the env var holding its secret, and is `admin` or restricted to projects:

```yaml
# api_keys/key-pipeline.yaml
api_keys:
  - name: key-pipeline                 # shown in the audit log and on the Users page
    description: "Deploy pipeline"
    env_vars:
      key: GITOPS_PIPELINE_KEY         # the env var holding the secret; `key` is the only field
    role: restricted                   # default restricted; admin = everything
    projects: [infra]                  # may see and pull these, and see and run their APIs and scripts
```

The older `env_prefix` + `secret_keys` (credentials) and `env: VAR` (API keys) still load, with a
notice at startup and from `--check-config` naming the `env_vars` to write; a later release refuses them.
Setting both forms on one credential is refused, and so is `env_vars` beside a `vault_path`, a
`secret_file` or `secret_keys`: a credential reads from one place.

How an item uses its credential's fields: an API or a webhook reads them where `auth` and
`signature` say ([http-requests.md](http-requests.md)); a script receives each as
`CREDENTIAL_<FIELD>` ([scripts.md](scripts.md)).

`UNIFIED_GITOPS_KEY`, if set, is one more admin key. With no keys at all the API is open, which is
logged loudly at startup.
