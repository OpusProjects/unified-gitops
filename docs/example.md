# Example

A complete configuration directory: one repository watched, Portainer users kept in step with
its JSON files, a DNS reload script run from another repository, and a webhook told about
every pull and every failure. `unified-gitops --check-config` accepts it as it is.

```
config/
├── config.yaml
├── projects/
│   ├── infra.yaml
│   └── ops-scripts.yaml
├── credentials/
│   ├── cred-github.yaml
│   └── cred-portainer.yaml
├── apis/
│   └── portainer-users.yaml
├── scripts/
│   └── reload-dns.yaml
└── webhooks/
    └── notify.yaml
```

## The server

```yaml
# config.yaml
server:
  host: "0.0.0.0"
  port: 8183
```

## The repositories

`infra` is the one watched; `ops-scripts` only holds the script.

```yaml
# projects/infra.yaml
projects:
  - name: infra
    git_url: https://github.example.com/ops/infra.git
    credentials: [cred-github]

# projects/ops-scripts.yaml
projects:
  - name: ops-scripts
    git_url: https://github.example.com/ops/ops-scripts.git
    credentials: [cred-github]
```

## The credentials

Each names the environment variable holding the secret, never the secret itself.

```yaml
# credentials/cred-github.yaml
credentials:
  - name: cred-github
    type: token
    env_vars: {token: GITHUB_TOKEN}

# credentials/cred-portainer.yaml
credentials:
  - name: cred-portainer
    type: token
    env_vars: {token: PORTAINER_TOKEN}
```

## An API

Every changed `portainer/users/*.json` file is sent as the body of a `PUT` to the user named after
it, unless Portainer already has the same contents; removing the file deletes the user.

```yaml
# apis/portainer-users.yaml
apis:
  - name: portainer-users
    project: infra                      # the watched repository
    trigger: {paths: [portainer/users/*.json]}
    credential: cred-portainer
    api:
      url: https://portainer.example.com/api/users/$file.stem
      method: PUT
      compare: {ignore_fields: [Id]}    # skip what is already there
      on_delete: {method: DELETE}
```

## A script

A commit touching `dns/` runs `reload-dns.sh` from `ops-scripts`, with the changed files in its
environment.

```yaml
# scripts/reload-dns.yaml
scripts:
  - name: reload-dns
    project: infra
    trigger: {paths: [dns/**]}
    script:
      project: ops-scripts              # the file can live in another repository
      file: reload-dns.sh
      args: [--zone, example.com]
```

## A webhook

Each pull that brings commits, and each failed API or script run, posts a one-line message.

```yaml
# webhooks/notify.yaml
webhooks:
  - name: notify
    project: infra
    events: [commit_pulled, api_failed, script_failed]
    webhook:
      url: https://hooks.example.com/services/x
      body: {text: "$event.project: $event.event at $event.revision_short"}
```

Every option of each is in [apis.md](apis.md), [scripts.md](scripts.md),
[webhooks.md](webhooks.md) and [configuration.md](configuration.md).
