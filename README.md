# Unified GitOps

[![unified-gitops CI](https://github.com/OpusProjects/unified-gitops/actions/workflows/build.yaml/badge.svg)](https://github.com/OpusProjects/unified-gitops/actions/workflows/build.yaml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg?logo=rust)](https://www.rust-lang.org/)

Git-triggered ops automation inside your network: when a repository changes, call APIs, run
scripts and tell webhooks.

Unified GitOps watches git repositories from inside your network. When a commit lands, it calls
the APIs that should hear about it, with the files that changed as their bodies; it runs scripts
from a repository, with what changed in their environment; and it tells webhooks what happened.
Nothing has to reach in, no CI runner has to be registered, and every item is a YAML file in a
configuration directory.

```yaml
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

scripts:
  - name: reload-dns
    project: infra
    trigger: {paths: [dns/**]}
    script:
      project: ops-scripts              # the file can live in another repository
      file: reload-dns.sh
      args: [--zone, example.com]

webhooks:
  - name: notify
    project: infra
    events: [commit_pulled, api_failed, script_failed]
    webhook:
      url: https://hooks.example.com/services/x
      body: {text: "$event.project: $event.event at $event.revision_short"}
```

---

## ✨ Features

- **Pull, not push**: repositories are pulled on an interval, a cron schedule or on demand
- **Acts on what changed**: each item names the files it watches; a commit runs only the items it
  touched, with only those files
- **APIs**: one request per changed file (its contents as the body) or per commit; upsert, compare
  before writing, delete when a file is removed, order, dry run
- **Scripts**: from the watched repository or another; arguments, environment, stdin, exit codes,
  timeouts that stop the whole process group, output kept
- **Webhooks**: any HTTP receiver, the body shaped by a template; pulls, API runs and script runs
  as events
- **Every common request option**: bearer, basic, header, query, OAuth 2.0 client credentials,
  login flows; HMAC signatures; retries with Retry-After; TLS with your own CA or a client
  certificate; proxies; rate limits; following a job until it finishes
- **Triggers**: on a change, at startup, on a cron schedule or by hand, with a debounce
- **Run history**: every run, its commit and each step's outcome, in the API and the UI
- **Config as code**: one item per file, any folders; every template and setting checked at load
- **Web UI and LDAP sign-in**, API keys scoped by project, secrets from Vault or the environment,
  Prometheus metrics, live config reload, single binary

---

## 📚 Documentation

| Document                                  | What it covers                                                         |
| ----------------------------------------- | ---------------------------------------------------------------------- |
| [How it works](docs/how-it-works.md)      | Pulls, changed files, triggers, runs and events                         |
| [APIs](docs/apis.md)                      | Calling an API with what changed: per file or commit, upsert, compare   |
| [Scripts](docs/scripts.md)                | Running a script on a change: its file, arguments and environment       |
| [Webhooks](docs/webhooks.md)              | Telling a receiver what happened: events, the message, body templates   |
| [HTTP requests](docs/http-requests.md)    | The request settings APIs and webhooks share: auth, signing, TLS, retry |
| [Configuration](docs/configuration.md)    | Every YAML file field by field                                          |
| [API](docs/api.md)                        | All routes with authentication and examples                            |
| [Architecture](docs/architecture.md)      | The layers, the dependency rule, and what is shared with unified-api   |
| [Configuration API](docs/config-api.md)   | Pushing the config directory over HTTP and reloading it live            |
| [Deployment](docs/deployment.md)          | Container image, Kubernetes and Compose                                |
| [LDAP sign-in](docs/ldap-login.md)        | Directory accounts, access rules and sessions                          |
| [Observability](docs/observability.md)    | Logs, metrics and the alerts worth having                              |
| [Projects](docs/projects.md)              | Git checkouts: when they are pulled, credentials, virtualenvs          |
| [Releasing](docs/releasing.md)            | How release-please cuts a version, the CHANGELOG, a stuck release PR   |
| [Secrets](docs/secrets.md)                | Credential backends, the resolution cache, native Vault                |
| [Testing](docs/testing.md)                | Running the suite and what it covers                                   |
| [Web UI](docs/ui.md)                      | The UI the image serves and how signing in works                       |

---

## 🤝 Contributing

Contributions are welcome: [CONTRIBUTING.md](CONTRIBUTING.md) covers the PR workflow, commit style, CI gates and architecture rules.

Security issues: see [SECURITY.md](SECURITY.md) for private reporting.

---

## 👥 Authors

- [Fernando Roca](https://github.com/fernandorocagonzalez)
- [Blai Peidro](https://github.com/blaipr)

---

## ⚖️ License

[Apache 2.0](LICENSE)
