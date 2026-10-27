# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- A project's last pulls: `history` on each project in `GET /api/v1/projects`, newest first, up to 50, in memory since the process started, as an application's runs are. Each says when it started, how long it took, why (`scheduled`, `manual`, `check` before an application's check or sync, `startup`), who asked a manual one (the API key's name or the signed-in user), how it ended, and the commit it moved the checkout from and to (see `docs/projects.md`).

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
