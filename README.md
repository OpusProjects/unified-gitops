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

See [a complete example](docs/example.md): an API, a script and a webhook.

---

## ✨ Features

- **Pull, not push**: repositories pulled on an interval, a cron schedule or on demand
- **Acts on what changed**: a commit runs only the items whose files it touched
- **APIs**: a request per changed file or per commit; upsert, compare, delete, dry run
- **Scripts**: from any repository; args, env, stdin, exit codes, timeouts, output kept
- **Webhooks**: any HTTP receiver, the body shaped by a template over each event
- **Every request option**: OAuth 2.0, login flows, HMAC signing, retries, TLS, proxies
- **Triggers**: on a change, at startup, on a cron schedule or by hand, with a debounce
- **Run history**: every run, its commit and each step's outcome, in the API and the UI
- **Config as code**: one YAML item per file, every template and setting checked at load
- **Web UI**, LDAP sign-in, scoped API keys, Vault secrets, Prometheus metrics, one binary

---

## 📚 Documentation

| Document                                  | What it covers                                                         |
| ----------------------------------------- | ---------------------------------------------------------------------- |
| [How it works](docs/how-it-works.md)      | Pulls, changed files, triggers, runs and events                         |
| [Example](docs/example.md)                | A complete configuration directory: an API, a script and a webhook      |
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
