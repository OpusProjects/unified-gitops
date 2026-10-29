# CLAUDE.md — Unified GitOps

## What is this

GitOps for anything with an API, written in Rust. Pulls desired state (YAML) from git projects,
compares it with what each live system has, and applies the difference through connector scripts — the
way Argo CD does for Kubernetes, for Isilons, Ascender, AnsibleForms, InfiniBox and the like.
Sister project of [unified-api](https://github.com/OpusProjects/unified-api), from whose 0.34.0 code
it started; the web UI is the ui repository's `apps/unified-gitops`.

## Organization

- **GitHub Org:** [OpusProjects](https://github.com/OpusProjects)
- **License:** Apache 2.0
- **Owners:** Fernando Roca and Blai Peidro

## Tech stack

Rust (edition 2024), axum, tokio, serde + serde_json + serde-saphyr (reads config and desired
state, writes the state copy), utoipa (OpenAPI), metrics + metrics-exporter-prometheus, and
[auth](https://github.com/OpusProjects/auth)'s `unified-secrets` and `unified-auth` crates.

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
│   ├── webhook.rs            # Webhook: type (webhook/ascender/awx), events, its block
│   ├── yaml.rs               # YAML → JSON with exact scalars ("0644" stays a string)
│   ├── credential.rs, api_key.rs, ldap.rs, project.rs, sync_health.rs
├── application/
│   ├── reconcile.rs          # THE LOOP: validate → list → copy → diff → apply → re-list
│   ├── projects.rs           # pull a checkout (bounded by timeout_seconds)
│   ├── webhooks.rs           # event per run (detect), message, request per type, queue + worker
│   ├── scripts.rs            # per-run script path resolution into checkouts
│   ├── credentials.rs, login.rs, config.rs (reload)
├── ports/                    # connector, state_store, git, secrets, directory, config_store, venv,
│                             #   http_sender (sends one built HTTP request)
└── adapters/
    ├── in/http/              # applications (+ state copies), connectors, projects,
    │                         #   credentials, settings, users (auth), config, login, session,
    │                         #   auth, metrics, health, html (browsable API), ui_assets
    ├── in/scheduler/         # one task per application; project pulls; reload generations
    └── out/
        ├── connectors/process.rs  # ConnectorPort → script (scrubbed env, APPLICATION_CONFIG, timeout kills)
        ├── state/fs.rs         # StateStorePort → <state.dir>/<application>/<kind>.yaml
        ├── webhooks/http.rs    # HttpSenderPort → reqwest (no redirects followed)
        ├── git/, secrets/, directory/, config/, python.rs, process_env.rs
tests/                        # integration tests; tests/connectors/ holds test connector scripts
                              #   (kv.py + the SDK, copied from the connectors repository)
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

Credential resolution, LDAP sign-in, API keys and browser sessions come from
[auth](https://github.com/OpusProjects/auth), pinned by tag in `Cargo.toml`:
`unified-secrets` (Credential, SecretsPort, env and Vault adapters, cache, `build_chain`,
`SecretsConfig`) and `unified-auth` (DirectoryPort, LdapDirectory, `LdapConfig<R>`, the login use case
and throttle, and its `http` module: API key registry and middleware, session store, CSRF, the form
layer). The modules that held them re-export them under their old paths; what stays here is
`CredentialRules` (load-time checks), the access rule type (`applications`) and its `grant_for`. Fix
that code in the auth repository, release it, then move both services to the new tag in a `build:` PR
(`cargo update -p unified-secrets -p unified-auth`); never patch a copy here. To try a change first,
use an uncommitted `[patch."https://github.com/OpusProjects/auth"]` pointing at a local
checkout. `deny.toml` allows exactly these git sources (auth and git).

Git checkouts come the same way from [git](https://github.com/OpusProjects/git)'s `unified-git` crate:
`GitPort`, `Remote`, `Commit`, `CommitRange` and `CliGit` (clone, pull, revision, tree, commits,
export), and `without_userinfo` / `redact_userinfo`, re-exported from `ports::git`,
`adapters::out::git::cli` and `domain::project`. `GitProject::remote()` is what a project hands to git;
when to pull, the per-checkout lock, webhooks and sync health stay in `application/projects.rs`.

Project pulls, process env, scheduler basics, the config API, metrics plumbing
and UI serving are still the same code as unified-api's, copied: fix a bug in one, check the other.

## Releasing

Same process as unified-api (see its CLAUDE.md, and `docs/releasing.md` here): release-please
(`.github/workflows/release.yml`, as `github-actions[bot]`) keeps one `chore: release vX.Y.Z` PR open
that bumps `Cargo.toml`, `Cargo.lock` and the manifest and writes the CHANGELOG from the squashed PR
titles. A PR's title is its CHANGELOG line and decides the version (`feat:` MINOR, `fix:`/`perf:`
PATCH, hidden types release nothing), so write it for a release-notes reader. CI on the release PR is
dispatched by `release.yml` (`gh workflow run build.yaml --ref release-please--branches--main` if it
is missing). Merging it is the release: tag `vX.Y.Z`, GitHub Release, and the image
`ghcr.io/opusprojects/unified-gitops` as `X.Y.Z`, `<sha>` and `latest`. Never bump, tag or release by
hand; `UI_REF` in `docker/Dockerfile` moves in its own `build:` PR.

## Conventions

- CHANGELOG.md is written by release-please: no entries in feature PRs, the PR title is the entry.
- Private by default, `pub` only what needs exposing. Comments only when the WHY is non-obvious.
- All code comments in English. Teaching comments that explain Rust concepts are intentional —
  keep them with the code they explain.
