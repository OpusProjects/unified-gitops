# Changelog

All notable changes to this project are documented in this file.
The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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

[Unreleased]: https://github.com/OpusProjects/unified-gitops/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/OpusProjects/unified-gitops/releases/tag/v0.1.0
