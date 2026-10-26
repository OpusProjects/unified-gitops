# CLAUDE.md — Unified GitOps

## What is this

GitOps for anything with an API, written in Rust. Pulls desired state (YAML) from git projects,
compares it with what each live system has, and applies the difference through connector scripts — the
way Argo CD does for Kubernetes, for Isilons, Ascender, AnsibleForms, InfiniBox and the like.
Sister project of [unified-api](https://github.com/OpusProjects/unified-api), from whose 0.34.0 code
it started; the web UI is unified-ui's `apps/unified-gitops`.

## Organization

- **GitHub Org:** [OpusProjects](https://github.com/OpusProjects)
- **License:** Apache 2.0
- **Owners:** Fernando Roca and Blai Peidro

## Tech stack

Rust (edition 2024), axum, tokio, serde + serde_json + serde-saphyr (reads config and desired
state, writes the state copy), utoipa (OpenAPI), metrics + metrics-exporter-prometheus, ldap3, subtle.

## Build & run

```bash
cargo build
cargo run                                   # reads ./config, serves on :8183
cargo run -- --check-config                 # validate the configuration and exit
cargo test
CONFIG_DIR=/etc/unified-gitops cargo run
```

## Project structure

```
src/
├── main.rs                   # load config, build app, boot clones, start the scheduler
├── lib.rs                    # module tree + AppBuilder (composition root)
├── state.rs                  # AppState (ports + registries) and RuntimeConfig (reloadable)
├── config.rs                 # config.yaml + item files at any depth (AWX-style CasC: typed
│                             #   top-level key, `name` = identity); strict, every error at once
├── domain/
│   ├── app.rs                # Application: the system (connector, credentials, config) + its
│   │                         #   desired state (project, path, auto_sync, prune)
│   ├── connector.rs          # Connector (how to run it); what it manages comes from `describe`
│   ├── desired.rs            # desired-state files: parse, load a folder, validate, order
│   ├── plan.rs               # THE DIFF: only declared fields compared → Operation list
│   ├── run.rs                # SyncStatus, RunRecord, PlanSnapshot, AppStateRegistry
│   ├── yaml.rs               # YAML → JSON with exact scalars ("0644" stays a string)
│   ├── credential.rs, api_key.rs, ldap.rs, project.rs, sync_health.rs
├── application/
│   ├── reconcile.rs          # THE LOOP: validate → list → copy → diff → apply → re-list
│   ├── projects.rs           # pull a checkout (bounded by timeout_seconds)
│   ├── scripts.rs            # per-run script path resolution into checkouts
│   ├── credentials.rs, login.rs, config.rs (reload)
├── ports/                    # connector, state_store, git, secrets, directory, config_store, venv
└── adapters/
    ├── in/http/              # applications (+ state copies), connectors, projects,
    │                         #   credentials, settings, users (auth), config, login, session,
    │                         #   auth, metrics, health, html (browsable API), ui_assets
    ├── in/scheduler/         # one task per application; project pulls; reload generations
    └── out/
        ├── connectors/process.rs  # ConnectorPort → script (scrubbed env, APPLICATION_CONFIG, timeout kills)
        ├── state/fs.rs         # StateStorePort → <state.dir>/<application>/<kind>.yaml
        ├── git/, secrets/, directory/, config/, python.rs, process_env.rs
connectors/                      # unified_connector.py (the Python SDK) + kv.py (example)
examples/desired-state/       # what the bundled config reconciles
tests/                        # integration tests; tests/connectors/ holds test connector scripts
```

## Architecture

Hexagonal monolith, `adapters → application → ports → domain`. The reconcile loop lives in
`application/reconcile.rs` ONLY: HTTP (refresh/sync) and the scheduler both call it. A run takes one
`RuntimeConfig` snapshot (`state.config()`), so a reload mid-run cannot mix generations. Config is
parsed strictly (`deny_unknown_fields`); restart-only keys are named in `RestartOnlySettings`.

## Rules worth knowing

- **Validate before touching anything.** A run loads and validates every desired-state file first;
  any error fails the run before a connector is listed or applied.
- **Connectors describe themselves.** Config says how to run a script; the script's `describe`
  says what it manages (kinds, keys, depends_on), validated at the start of every run.
- **The core diffs, connectors don't.** Connectors `list` and `apply` one operation; ordering, prune,
  drift, locks and history are the core's. Only fields git declares are compared (plan.rs).
- **Always diff against a fresh list.** The local copy is written from what the system just said,
  never used instead of asking it: applying against a stale copy would undo changes made since.
- **Safe defaults.** `auto_sync: false`, `prune: false`. An application is ONE system: its
  connection (connector, config, credentials) is in its `applications` item, never in the git folder, and
  it only owns the kinds its files mention.
- **Locks.** One per application (a scheduled check and a manual Sync never interleave).
  In-process: run ONE replica.
- **Writes are opt-in for scripts.** `UNIFIED_ALLOW_WRITE=1` is set only on `apply`; the SDK refuses
  to write without it. The environment is scrubbed; timeouts kill.
- **Errors carry a body** (`ApiError` → `{"error": ...}`). Restricted keys and LDAP rules scope by
  `applications` (their state copies included); connectors, projects, credentials and config are
  admin-only.
- **Metrics** are `unified_gitops_*`, computed at scrape time. Session cookie `ugitops_session`.
- **OpenAPI version** comes from `CARGO_PKG_VERSION` — bump only `Cargo.toml`.

## Shared code with unified-api

Secrets, git/projects, process env, scheduler basics, auth/sessions/LDAP, the config API, metrics
plumbing and UI serving are the same code as unified-api's. Fix a bug in one, check the other, until
they move into a shared crate (unified-core).

## Releasing

Same process as unified-api (see its CLAUDE.md): `release/x.y.z` branch, bump `Cargo.toml` +
`Cargo.lock` + CHANGELOG, PR, squash-merge, tag the squashed commit on `main`; CI publishes
`ghcr.io/opusprojects/unified-gitops` and the GitHub Release from the tag.

## Conventions

- Only `## [Unreleased]` is editable in CHANGELOG.md.
- Private by default, `pub` only what needs exposing. Comments only when the WHY is non-obvious.
- All code comments in English. Teaching comments that explain Rust concepts are intentional —
  keep them with the code they explain.
