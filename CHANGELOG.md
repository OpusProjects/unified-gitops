# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- Each run in an application's history keeps its operations (`operations`, at most 200; `planned`
  keeps the full count), so the UI can show a past run's changes as a diff.

- First version: GitOps for anything with an API. Each application is one live
  system (a connector, its config and credentials) and a folder of desired state
  as YAML in a git project, reconciled through the connector's script
  (`describe`, `list --kind`, `apply --kind`). See `docs/how-it-works.md`.
- The diff is the core's: objects are matched on each kind's key and only the
  fields git declares are compared, so the defaults a system returns are never
  drift. Kinds declare `depends_on`; creates follow it, deletes run in reverse.
- A local copy of each system's current settings, one YAML file per application
  and kind in `state.dir`, refreshed at every check and readable at
  `GET /api/v1/applications/{id}/state/{kind}`.
- Applications are checked every `sync_interval_seconds` after pulling their
  project; `auto_sync` (off by default) applies what a check finds, `prune`
  (off by default) deletes what git does not declare.
- Configuration laid out like AWX config as code: `config.yaml` at the root, and
  one item per file under a typed top-level key (`applications:`, `connectors:`,
  `projects:`, `credentials:`, `api_keys:`), in any folders at any depth. `name`
  is each item's identity, `description` is optional, and references are
  `connector`, `project`, `credential` and `credentials`. API responses use the
  same keys, and the configuration API addresses files by path.
- Routes: applications (list, detail with the pending plan and run history,
  `refresh`, `sync`, state copies), connectors, projects, credentials, settings, auth,
  and the configuration API.
- Rollback: `POST /api/v1/applications/{id}/rollback` applies an older
  commit's desired state once and pauses automatic sync (kept across
  restarts) until the next manual Sync applies HEAD again.
- `GET /api/v1/search?q=`: find an object on the systems by its key or any
  value, across the local copies of the applications the caller may see, with
  the application that owns it and whether git declares it.
- Connectors describe what they manage: `describe` returns each object type
  with its `key` and `depends_on`, and the configuration only says how to run
  the script (no `kinds:`). The Python SDK takes them in `run(kinds={...})`.
  A connector names its script with `project` + `path`, and may say
  `type: script` (the default, and so far the only type).
- Every call to a system is counted: `unified_gitops_connector_calls_total`
  (application, connector, command, outcome) and a duration histogram, so the
  traffic this instance originates is visible, not only the requests it serves.
- A browser that is not signed in is sent to the login form by the server
  when it asks for a page of the UI, so the app never loads only to redirect
  itself (no flash of the overview before the login page).
- A Python SDK for connectors (`connectors/unified_connector.py`) with the write guard
  (`UNIFIED_ALLOW_WRITE`), and an example connector (`connectors/kv.py`).
- Shared with unified-api 0.34.0, from which this started: secrets (env, files,
  Vault), git checkouts and virtualenvs, the scheduler (jitter, backoff, panic
  supervisor, live reload), API keys, LDAP sign-in, sessions and CSRF, metrics,
  the config API, and serving the web UI (`server.ui.assets_dir`).
