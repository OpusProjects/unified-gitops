# Architecture

A hexagonal monolith, the same shape as unified-api: one binary, ports and adapters inside.
Dependency direction: `adapters → application → ports → domain`, never the reverse.

```
src/
├── domain/        pure types and rules: Application, Connector, desired-state parsing and
│                  validation, the diff (plan.rs), run records and statuses (run.rs)
├── application/   use cases: reconcile (the loop), projects (pull), login, config reload
├── ports/         traits: ConnectorPort, StateStorePort, GitPort, SecretsPort, DirectoryPort…
└── adapters/
    ├── in/        HTTP (axum handlers, auth, sessions, metrics, UI assets) and the scheduler
    └── out/       connector processes, the state folder, git, secrets (env, Vault), LDAP, config store
```

The loop lives in `application/reconcile.rs` and nowhere else: the HTTP handlers (refresh, sync) and
the scheduler (periodic checks) both call it. It takes one configuration snapshot for the whole run,
so a reload in the middle of a run cannot mix an old application with a new connector.

## Shared with unified-api

unified-gitops started as a copy of unified-api 0.34.0 with the inventory removed. Credential resolution,
LDAP sign-in, API keys and browser sessions now come from [auth](https://github.com/OpusProjects/auth), and git
checkouts from [git](https://github.com/OpusProjects/git), pinned by tag in `Cargo.toml` and re-exported under
their old module paths:

| Crate             | What it provides                                             | Re-exported here as |
| ----------------- | ------------------------------------------------------------ | ------------------- |
| `secrets-rs` | `Credential`, `SecretsPort`, env and Vault adapters, cache, `build_chain` | `domain::credential`, `ports::secrets`, `adapters::out::secrets` |
| `auth-rs`    | `DirectoryPort`, `LdapDirectory`, `LdapConfig<R>`, the login use case | `ports::directory`, `adapters::out::directory`, `application::login` |
| `auth-rs` (`http`) | API key registry and middleware, session store, CSRF, form layer | `adapters::in::http::auth`, `adapters::in::http::session` |
| `git-rs`     | `GitPort`, `CliGit`: clone, pull, commits, export, address redaction | `ports::git`, `adapters::out::git::cli`, `domain::project` |
| `web-rs`     | `ApiError`, `ErrorBody`, audit events, the web UI serving, route help from the OpenAPI document, the config store and API, the browsable pages, the sign-in form | `http::error`, `http::audit`, `http::ui_assets`, `http::spec`, `http::config`, `ports::config_store`, `adapters::out::config`, `http::html`, `http::login` |

What stays here is this service's own: the load-time checks on a credential (`CredentialRules`) and
the access rule type (`applications`) with what it grants, `Permissions`, how api_keys.yaml becomes keys
and the session cookie's name. These other parts are still the same code
as unified-api's, copied, and should stay the same:

| Part               | Where                                                       |
| ------------------ | ----------------------------------------------------------- |
| Running scripts    | `adapters/out/process_env.rs`, `python.rs` (scrubbed env, venvs) |
| Project pulls      | `application/projects.rs` (when to pull, the lock, sync health) |
| Scheduler basics   | jitter, backoff, panic supervisor, reload generations       |
| HTTP conventions   | health shapes, metrics recorder, CORS, body limit            |
| Users page         | `http/users.rs` (fields are each service's own scopes)      |
| Configuration      | strict loading, live reload                                   |

What is gitops' own: the domain (applications, connectors, desired state, plan, runs), the
connector adapter, the state store, the reconcile use case, and the HTTP routes over them.
