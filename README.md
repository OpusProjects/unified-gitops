# Unified GitOps

[![unified-gitops CI](https://github.com/OpusProjects/unified-gitops/actions/workflows/build.yaml/badge.svg)](https://github.com/OpusProjects/unified-gitops/actions/workflows/build.yaml)
[![License: Apache 2.0](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg?logo=rust)](https://www.rust-lang.org/)

GitOps for anything with an API: declare state in YAML, reconcile it through pluggable scripts.

Unified GitOps pulls desired state from git, compares it with what each live system has, and applies
the difference, the way Argo CD does for Kubernetes. The systems are anything a script can talk to:
an Isilon's NFS exports, Ascender job templates, AnsibleForms forms, InfiniBox volumes.

Changes no longer wait in a queue of CI runners: a commit is picked up at the next check, inside your
network, and a change made by hand on a system shows up as drift.

---

## ✨ Features

- **Pull, not push**: projects are pulled from git before every check; no CI runner on the path
- **Dumb connectors, smart core**: a connector lists objects and applies one operation; the core diffs
- **Compares only what git declares**: the 40 default fields an API returns never look like drift
- **Drift detection**: every check lists the live system, so a hand edit shows up as out of sync
- **A local copy of each system's settings**: one YAML file per application and kind, git's format
- **Plan before apply**: refresh shows what a sync would do, field by field, and changes nothing
- **Safe by default**: no automatic sync, no deletes (prune) unless an application asks for them
- **Self-describing connectors**: a script says what it manages, each type's key and what goes first
- **One application, one system**: its address and credentials live in config, never in git
- **Run history**: which commit was applied, when, and the result of every operation
- **Config as code, AWX style**: one item per file, any folders; `name` is the identity
- **Strict validation**: a typo in desired state or config fails before any system is touched
- **Web UI and LDAP sign-in**: the same UI family and sign-in as unified-api, on its own address
- **Secrets from Vault or env**, metrics for Prometheus, live config reload, single binary

---

## 📚 Documentation

| Document                                  | What it covers                                                         |
| ----------------------------------------- | ---------------------------------------------------------------------- |
| [How it works](docs/how-it-works.md)      | The reconcile loop: pull, list, diff, apply, and what each status means |
| [Connectors](docs/connectors.md)                | The script contract, the Python SDK, and writing a connector               |
| [Configuration](docs/configuration.md)    | Every YAML file field by field, and desired-state files                 |
| [API](docs/api.md)                        | All routes with authentication and examples                            |
| [Architecture](docs/architecture.md)      | The layers, the dependency rule, and what is shared with unified-api   |
| [Configuration API](docs/config-api.md)   | Pushing the config directory over HTTP and reloading it live            |
| [Deployment](docs/deployment.md)          | Container image, Kubernetes and Compose                                |
| [LDAP sign-in](docs/ldap-login.md)        | Directory accounts, access rules and sessions                          |
| [Observability](docs/observability.md)    | Logs, metrics and the alerts worth having                              |
| [Projects](docs/projects.md)              | Git checkouts: when they are pulled, credentials, virtualenvs          |
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
