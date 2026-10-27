# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Credentials name their environment variables in full with `env_vars: {<field>: <VAR>}` (`env_vars: {password: LDAP_PASSWORD}` reads `LDAP_PASSWORD` as `password`), and API keys with `env_vars: {key: <VAR>}`; `env_vars` and `env_prefix` on one credential, `secret_keys` next to `env_vars` without a `vault_path` or `secret_file`, an API key `env_vars` field other than `key`, and an API key with both or neither of `env_vars.key` and `env` are refused at load (see `docs/secrets.md`).
- Every credential field also reaches the connector under its credential's id, `CREDENTIAL_<ID>_<FIELD>` (the name without a leading `cred-`/`cred_`, anything outside `a-z0-9` made `_`: `cred-portainer-ldap`'s `password` is `CREDENTIAL_PORTAINER_LDAP_PASSWORD`), so an application can use two credentials with the same fields; a field already named after its credential (`portainer_url` on `cred-portainer`) is not prefixed again.
- A project's last pulls: `history` on each project in `GET /api/v1/projects`, newest first, up to 50, in memory since the process started, as an application's runs are. Each says when it started, how long it took, why (`scheduled`, `manual`, `check` before an application's check or sync, kept only when it moved the commit or failed, `startup`), who asked a manual one (the API key's name or the signed-in user), how it ended, and the commit it found the checkout at and left it at (see `docs/projects.md`).
- A Vault is a credential of `type: vault` (`address`, `mount`, `env_vars: {token: <VAR>}` or `kubernetes_role`, `timeout_seconds`, `cache_ttl_seconds`), as AWX models a Vault lookup; a credential reads from it with `vault: <its name>` and `vault_path`, so several Vaults can coexist, each with its own login and cache. Naming a Vault credential as an application's, project's, webhook's or LDAP bind's credential, a Vault field on another type, and a `vault:` that is not a Vault are refused at load; `GET /api/v1/credentials` adds `vault`, `used_by.credentials` on a Vault lists the credentials reading from it, and `vault_connection` (address, mount, auth, kubernetes_role, token_env, timeout_seconds, cache_ttl_seconds) describes each Vault, null for any other type (see `docs/secrets.md`).

### Changed

- **Breaking (scripts reading a field two credentials share):** `CREDENTIAL_<FIELD>` is delivered only when exactly one of the application's credentials has that field, instead of one credential silently overwriting the other; read `CREDENTIAL_<ID>_<FIELD>`.
- Env and file credentials are read on every resolution instead of being reused for 60 seconds: a rotated mounted file is seen on the next check. Only a Vault caches, for its own `cache_ttl_seconds`; `GET /api/v1/settings` answers `secrets.cache_ttl_seconds` as null unless `config.yaml` still sets it.

### Deprecated

- A credential's `env_prefix` + `secret_keys` and an API key's `env: VAR` still load, with a notice at startup and from `--check-config` naming the `env_vars` to write in their place; a later release refuses them.
- The `secrets:` block in `config.yaml` (`cache_ttl_seconds`, and `vault:` with `token_env`) still loads, with a notice saying to declare the Vault as a `type: vault` credential; while set, its cache still covers every credential and a `vault_path` with no `vault:` reads from it. A later release refuses it.

## [0.3.0] - 2026-10-27

### Added

- Project webhooks: a webhook's `projects:` (`all`, or names) hears the projects' own events and those of every application reading from them, including applications added later, combined with `applications:`; three project events, `commit_pulled` (the commits a pull brought, newest first with author and subject, whether history was rewritten, and the applications whose folders changed), `pull_failed` and `pull_recovered`, each sent once per change of state; application messages and the send history now name the project, and `GET /api/v1/webhooks` adds `all_projects`, `projects` and `project_applications` (see `docs/webhooks.md`).
- The web UI from unified-ui 0.4.0: project webhooks on the Webhooks pages, and the Webhooks card always on the Overview.

## [0.2.0] - 2026-10-26

### Added

- Webhooks: a `webhooks` item sends, for `all` applications or the ones it names, `drift_detected`, `out_of_sync`, `sync_succeeded`, `sync_failed` and `back_in_sync` to a webhook (any receiver, with bearer, basic or header auth, a body template and optional HMAC signing) or to an Ascender or AWX job template (through the template's own webhook receiver, so no user is needed), one message per run, drift told apart from a commit by the application's own folder in git, queued and retried without one receiver holding up the others, its changes listed by name and never with a field's value; `GET /api/v1/webhooks` shows each one's last send, `GET /api/v1/webhooks/{id}` its last 50, and `POST /api/v1/webhooks/{id}/test` sends a test (see `docs/webhooks.md`).
- Metrics `unified_gitops_git_requests_total{project,operation,outcome}` and `unified_gitops_git_request_duration_seconds`: every clone, pull and rollback fetch this instance makes to a git server, as `unified_gitops_connector_calls_total` counts those to the systems.
- The web UI from unified-ui 0.3.0: Webhooks pages, live ages, and requests to repositories on the Overview.

### Changed

- **Breaking (who runs the image):** the image no longer bundles a demo configuration or connectors (`/app/config`, `/app/connectors`); mount your configuration at `/app/config` or set `CONFIG_DIR`, and take connectors from a project such as [unified-connectors](https://github.com/OpusProjects/unified-connectors), which now holds the SDK and the `kv` example.

## [0.1.0] - 2026-10-26

First version: GitOps for anything with an API.

### Added

- Applications: one live system each (a connector, its config and credentials) and a folder of desired state as YAML in a git project, checked on an interval and applied on demand or with `auto_sync`.
- A diff that belongs to the core: objects matched on each kind's key, only the fields git declares compared, kinds applied in `depends_on` order, and deletes only with `prune`.
- Connectors as plain scripts with a small contract (`describe`, `list`, `apply`) and a Python SDK; writes allowed only on `apply`.
- Drift detection, a local copy of each system's settings, a plan before every apply, run history with diffs, and rollback to an older commit.
- Configuration as code, one item per file, validated strictly, reloadable live and pushable over the configuration API.
- API keys and LDAP sign-in, secrets from env or Vault, Prometheus metrics, and the web UI from unified-ui 0.2.0.

[Unreleased]: https://github.com/OpusProjects/unified-gitops/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/OpusProjects/unified-gitops/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/OpusProjects/unified-gitops/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/OpusProjects/unified-gitops/releases/tag/v0.1.0
