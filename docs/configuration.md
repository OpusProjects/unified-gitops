# Configuration

Everything is YAML in one directory (`CONFIG_DIR`, default `./config`), one item per file:
`config.yaml` at the root holds the server settings, and every other `*.yaml` / `*.yml`
file, at any depth, holds items under a typed top-level key. Parsing is strict: an unknown key fails
startup naming the file and the key. `unified-gitops --check-config` validates the directory exactly
as startup does, and exits.

```
/etc/unified-gitops/
├── config.yaml                        # required, at the root, under this name
├── applications/                      # folders are for people: any names, any depth, or none
│   ├── storage/
│   │   ├── isilon-prod.yaml
│   │   └── isilon-dr.yaml
│   └── ascender-green.yaml
├── connectors/
│   └── powerscale.yaml
├── projects/
│   └── desired-state.yaml
├── credentials/
│   └── cred-isilon.yaml
├── api_keys/
│   └── key-pipeline.yaml
└── webhooks/
    └── wh-failures.yaml
```

| Top-level key  | Holds                                                         |
| -------------- | ------------------------------------------------------------- |
| `applications` | Each live system, its folder of desired state, how to enforce it |
| `connectors`      | Scripts, and how to run them                                  |
| `projects`     | Git repositories: desired state, and connector scripts           |
| `credentials`  | Where each credential's secrets come from (never the values)  |
| `api_keys`     | API keys by name and role (the values come from env vars)     |
| `webhooks` | What runs report, and where: webhooks, Ascender and AWX job templates |

- **The key says what an item is, never the folder.** A file in `connectors/` holding `applications:` is
  read as an application.
- **One item per file is the convention**, not a rule: a file may hold several items, of several
  types. A `name` defined twice is an error naming both files.
- **`name` is the identity**: what other items reference, what the API and the state
  directory use. Letters, digits, `-`, `_` and `.`. `description` is optional, for people.
- **References drop the `_id`:** `connector`, `project`, `credential`, `credentials`.
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

state:
  dir: "/var/lib/unified-gitops/state"     # the local copy of each system's settings

projects:
  dir: "/var/lib/unified-gitops/projects"  # one checkout per project

# config_api:
#   enabled: true                  # read/write this directory over HTTP (admin only)
```

Restart-only (a reload reports them in `restart_required`): `server.host`, `server.port`,
`server.ui.enabled`, `server.ui.session_ttl_seconds`, `server.ui.assets_dir`, `state.dir`,
`projects.dir`, `config_api.enabled`. Everything else reloads live.

## applications

One item per live system: how to reach it, and where its desired state lives.

```yaml
# applications/storage/isilon-prod.yaml
applications:
  - name: isilon-prod                  # identity: URLs, the state folder, API keys' scopes
    description: "Isilon production"
    # The system
    connector: powerscale
    credentials: [cred-isilon]         # handed to the connector as CREDENTIAL_*
    config:                            # handed to the connector as APPLICATION_CONFIG (not secret)
      url: "https://isilon-prod.example.com:8080"
    # The desired state
    project: desired-state
    path: apps/isilon-prod             # default "."; every *.yaml / *.yml under it, recursively
    # How to enforce it
    sync_interval_seconds: 300         # default 300: how often to compare with the system
    auto_sync: false                   # default false: true = apply automatically when out of sync
    prune: false                       # default false: true = delete objects git does not declare
```

The connection stays here, in the configuration admins own, and never in the desired-state
repository: whoever can merge there must not be able to point the writes, or the credentials, at
another system. Two systems with the same settings (production and DR) are two items with the same
`path`.

## connectors

```yaml
# connectors/powerscale.yaml
connectors:
  - name: powerscale
    description: "PowerScale"
    type: script                       # optional, the default (the only type so far)
    project: connectors                   # optional: resolve path in this checkout
    path: connectors/powerscale.py
    timeout_seconds: 120               # each describe/list/apply is killed past this
```

What a connector manages (its object types, each one's key, what goes first) is not configured: the
script says it in `describe` (see connectors.md). Application names are path segments in the state
directory, so they may only contain letters, digits, `-`, `_` and `.`, and may not start with a dot.

## projects

```yaml
# projects/desired-state.yaml
projects:
  - name: desired-state
    description: "Desired state"
    git_url: "https://github.example.com/ops/desired-state.git"
    branch: main                       # default main
    credential: cred-github            # private repos: token or ssh_key credential
    sync_interval_seconds: 1800        # periodic pull; an application also pulls before each check
    # schedule: "0 * * * *"            # cron (UTC) instead of the interval
    timeout_seconds: 300
    sync_on_boot: true
    python_venv: false                 # true = build a virtualenv from its requirements.txt
```

## credentials and api_keys

As in unified-api. A credential names env vars, a file or a Vault path, never a value:

```yaml
# credentials/cred-isilon.yaml
credentials:
  - name: cred-isilon
    description: "Isilon API user"
    type: username_password            # username_password | token | ssh_key | vault
    env_vars:                          # field → the env var holding it
      username: ISILON_USERNAME
      password: ISILON_PASSWORD
    # vault: cred-vault                # or Vault: the type: vault credential to read from
    # vault_path: "ops/isilon-prod"    #   and the KV v2 path under its mount
    # secret_file: "/run/secrets/isilon.json"
```

A Vault is a credential too, of `type: vault`, as AWX models a HashiCorp Vault lookup: the
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
| `cache_ttl_seconds` | How long a value read from this Vault is reused (default 60; `0` = every resolution): what keeps the check schedule from becoming a request storm, and also how long a rotation takes to show |

Credentials *without* a `vault_path` keep resolving from env/files, uncached, so adoption is per
credential. A Vault credential is never handed to an application, project, webhook or LDAP bind:
naming one there is refused, since its token is not a login.

The `secrets:` block in `config.yaml` (`cache_ttl_seconds`, and a single `vault:` with `token_env`)
still loads but is **deprecated**: startup and `--check-config` say what to write instead, and a
later release refuses it. While it is set, its `cache_ttl_seconds` still caches every credential, as
it did, and a `vault_path` with no `vault:` reads from its `vault:`.

An API key names the env var holding its secret, and is `admin` or restricted to applications:

```yaml
# api_keys/key-pipeline.yaml
api_keys:
  - name: key-pipeline                 # shown in the audit log and on the Users page
    description: "Deploy pipeline"
    env_vars:
      key: GITOPS_PIPELINE_KEY         # the env var holding the secret; `key` is the only field
    role: restricted                   # default restricted; admin = everything
    applications: [isilon-prod]        # may see, refresh and sync these only
```

The older `env_prefix` + `secret_keys` (credentials) and `env: VAR` (API keys) still load, with a
notice at startup and from `--check-config` naming the `env_vars` to write; a later release refuses them.

`UNIFIED_GITOPS_KEY`, if set, is one more admin key. With no keys at all the API is open, which is
logged loudly at startup.

## webhooks

Which events, from which applications, go where. The full reference is [webhooks.md](webhooks.md):

```yaml
# webhooks/wh-ascender-drift.yaml
webhooks:
  - name: wh-ascender-drift
    type: ascender                     # webhook | ascender | awx
    events: [drift_detected]
    applications: [isilon-prod]        # or all; left out or [] = none
    ascender:
      url: "https://ascender.example.com"
      job_template: 123
      credentials: [cred-ascender-jt123]   # its token is the template's webhook key
```

## Desired-state files

In the application's folder, one kind per file:

```yaml
kind: nfs_export
items:
  - path: /ifs/data/projects
    clients: [10.0.0.0/24]
    read_only: false
```

- `kind` and `items` only; any other top-level key is an error (a file never names its system).
- Every item has the kind's `key`; a key may appear once across all files of the application.
- Scalars keep their YAML 1.2 meaning: `0644` and `08001` stay strings, `yes` is a string, `true` a
  bool. Quote anything meant as a string to be sure.
- An empty `items:` declares the kind with no objects: with `prune: true`, it removes them all.
