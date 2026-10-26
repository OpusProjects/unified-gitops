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

unified-gitops started as a copy of unified-api 0.34.0 with the inventory removed. These parts are the
same code and should stay the same until they move into a shared crate:

| Part               | Where                                                       |
| ------------------ | ----------------------------------------------------------- |
| Secrets            | `adapters/out/secrets` (env, files, Vault, cache, reload)   |
| Running scripts    | `adapters/out/process_env.rs`, `python.rs` (scrubbed env, venvs) |
| Git checkouts      | `adapters/out/git`, `application/projects.rs`               |
| Scheduler basics   | jitter, backoff, panic supervisor, reload generations       |
| HTTP conventions   | `ApiError`, health shapes, metrics recorder, CORS, body limit |
| Auth               | API keys, sessions, CSRF, LDAP sign-in, the login page      |
| Configuration      | strict loading, staged validation, atomic commit, live reload |
| UI serving         | `ui_assets.rs`                                              |

What is gitops' own: the domain (applications, connectors, desired state, plan, runs), the
connector adapter, the state store, the reconcile use case, and the HTTP routes over them.
