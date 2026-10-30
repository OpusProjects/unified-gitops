# CLAUDE.md — Unified GitOps

## What is this

Git-triggered ops automation inside your network, written in Rust. Pulls git projects and,
when they change, calls APIs with the files that changed (`apis`), runs scripts from a repository
(`scripts`) and tells webhooks (`webhooks`). Sister project of [unified-api](https://github.com/OpusProjects/unified-api),
from whose 0.34.0 code it started; the web UI is the ui repository's `apps/unified-gitops`.

## Organization

- **GitHub Org:** [OpusProjects](https://github.com/OpusProjects)
- **License:** Apache 2.0
- **Owners:** Fernando Roca and Blai Peidro

## Tech stack

Rust (edition 2024), axum, tokio, serde + serde_json + serde-saphyr (reads config), utoipa
(OpenAPI), metrics + metrics-exporter-prometheus, and
[auth](https://github.com/OpusProjects/auth)'s `secrets-rs` and `auth-rs` crates.

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
│   ├── webhook.rs            # Webhook: project, events, path trigger, its request and body
│   ├── api.rs                # Api: per file/commit, body, upsert, compare, on_delete, order
│   ├── script.rs             # Script: the file (in the watched project or another), args,
│   │                         #   interpreter, working_dir, env, per, stdin, exit codes, output
│   ├── run.rs                # Run, Step, RunLog (history + failing → recovered), Kind
│   ├── http.rs               # HttpSpec, shared by every outbound block: url, method, headers,
│   │                         #   query, auth (bearer/basic/header/query/oauth2/login), signature,
│   │                         #   statuses, TLS, proxy, rate limit; Retry; Wait (poll a job)
│   ├── template.rs           # `$event.project` templates and `$poll.status == x` conditions
│   ├── trigger.rs            # PathFilter (globs over changed files); Trigger: on change,
│   │                         #   startup, schedule (cron), manual; debounce
│   ├── yaml.rs               # YAML → JSON with exact scalars ("0644" stays a string)
│   ├── credential.rs, api_key.rs, ldap.rs, project.rs, sync_health.rs
├── application/
│   ├── projects.rs           # pull a checkout (bounded by timeout_seconds), tell what it did
│   ├── http.rs               # Engine: HttpSpec → request; auth tokens, signing, retries,
│   │                         #   Retry-After, rate limits, wait polling
│   ├── webhooks.rs           # event per pull (detect), message, body, queue + worker
│   ├── actions.rs            # Runner: which items a commit starts, debounce, one run per item
│   │                         #   at a time, an API run file by file, a script's executions,
│   │                         #   their record and events
│   ├── credentials.rs, login.rs, config.rs (reload)
├── ports/                    # git, secrets, directory, config_store, venv, process (runs a
│                             #   script),
│                             #   http_sender (sends one built HTTP request)
└── adapters/
    ├── in/http/              # projects, apis, scripts, webhooks, credentials, settings, users (auth), config,
    │                         #   login, session, auth, metrics, health, html (browsable API),
    │                         #   ui_assets
    ├── in/scheduler/         # project pulls; item cron schedules; reload generations
    └── out/
        ├── http_client/http.rs # HttpSenderPort → reqwest, one client per Transport
        ├── process.rs          # ProcessPort → a child in its own process group; SIGTERM, then
        │                       #   SIGKILL on timeout; output tail kept
        ├── process_env.rs      # the scrubbed environment every script starts from
        ├── git/, secrets/, directory/, config/, python.rs
tests/                        # integration tests
```

## Architecture

Hexagonal monolith, `adapters → application → ports → domain`. A pull and what it tells live in
`application/projects.rs` ONLY: HTTP (sync) and the scheduler both call it. A pull takes one
`RuntimeConfig` snapshot (`state.config()`), so a reload mid-pull cannot mix generations. Config is
parsed strictly (`deny_unknown_fields`); restart-only keys are named in `RestartOnlySettings`.

## Rules worth knowing

- **Pull, not push.** Projects are pulled on their own schedule, at boot, or on demand; nothing
  has to reach in.
- **One message per pull.** A pull produces at most one of `commit_pulled`, `pull_failed`,
  `pull_recovered`; a failure is told when it starts and the recovery when it ends.
- **Webhooks are generic.** One HTTP request shaped by the item (`webhook.body` over `$event`);
  no receiver is special-cased. Every outbound request goes through `application/http.rs`.
- **Templates are checked at load.** `$event.<field>` must be an event field (`EVENT_FIELDS`), a
  root must be available where it is written; what validates is what renders.
- **Items act on what changed.** A pull lists the files its commits touched; each item whose
  trigger selects one runs (application/actions.rs), with those files only. A scheduled,
  startup or manual run takes every selected file in the checkout. A dry run reads, never writes.
- **Scripts see a scrubbed environment.** A short passthrough list (PATH, HOME, proxies, CA
  bundles), then `GITOPS_*` (what changed), `CREDENTIAL_<FIELD>` (the item's credential) and the
  item's `env`. Never the service's own variables. A script runs once unless `retry` asks.
- **Locks.** One per checkout (a scheduled pull and a manual one never interleave). In-process:
  run ONE replica.
- **Errors carry a body** (`ApiError` → `{"error": ...}`). Restricted keys and LDAP rules scope by
  `projects` (a restricted caller sees and pulls its own); credentials, webhooks and config are
  admin-only.
- **Metrics** are `unified_gitops_*`, computed at scrape time. Session cookie `ugitops_session`.
- **OpenAPI version** comes from `CARGO_PKG_VERSION` — bump only `Cargo.toml`.

## Shared code with unified-api

Credential resolution, LDAP sign-in, API keys and browser sessions come from
[auth](https://github.com/OpusProjects/auth), pinned by tag in `Cargo.toml`:
`secrets-rs` (Credential, SecretsPort, env and Vault adapters, cache, `build_chain`,
`SecretsConfig`) and `auth-rs` (DirectoryPort, LdapDirectory, `LdapConfig<R>`, the login use case
and throttle, and its `http` module: API key registry and middleware, session store, CSRF, the form
layer). The modules that held them re-export them under their old paths; what stays here is
`CredentialRules` (load-time checks), the access rule type (`projects`) and its `grant_for`. Fix
that code in the auth repository, release it, then move both services to the new tag in a `build:` PR
(`cargo update -p secrets-rs -p auth-rs`); never patch a copy here. To try a change first,
use an uncommitted `[patch."https://github.com/OpusProjects/auth"]` pointing at a local
checkout. `deny.toml` allows exactly these git sources (auth, git and web).

Git checkouts come the same way from [git](https://github.com/OpusProjects/git)'s `git-rs` crate:
`GitPort`, `Remote`, `Commit`, `CommitRange` and `CliGit` (clone, pull, revision, tree, commits,
export), and `without_userinfo` / `redact_userinfo`, re-exported from `ports::git`,
`adapters::out::git::cli` and `domain::project`. `GitProject::remote()` is what a project hands to git;
when to pull, the per-checkout lock, webhooks and sync health stay in `application/projects.rs`.

The web layer comes the same way from [web](https://github.com/OpusProjects/web)'s `web-rs` crate:
`ApiError`/`ErrorBody` (`http::error`), `audit::record`, the web UI serving (`http::ui_assets`, fixed to
`Permissions`), the route help the browsable pages read from the OpenAPI document (`http::spec`), and the
config API: the store (`FsConfigStore`, built with this service's loader by `fs_store`), the file rules
and every config route's logic. `http/config.rs` keeps the route functions, `ConfigSummary`,
`ReloadInfo` and a `ConfigHost` (what a reload does). The browsable pages and the sign-in form come
from it too: `http::html::site()` is this service's `Site`, and `http/login.rs` keeps the route
functions and a `SignIn` that runs the LDAP login and maps its `Grant`. The Users page (`http/users.rs`)
stays here, its fields being this service's scopes. Move to a new release with
`cargo update -p web-rs` in a `build:` PR.

Project pulls, process env, scheduler basics, metrics plumbing and the Users page are still the
same code as unified-api's, copied: fix a bug in one, check the other.

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
