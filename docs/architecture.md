# Architecture

A hexagonal monolith, the same shape as unified-api: one binary, ports and adapters inside.
Dependency direction: `adapters → application → ports → domain`, never the reverse.

```
src/
├── domain/        pure types and rules: the items (Api, Script, Webhook), the request they
│                  share (http.rs), templates, triggers and globs, run records (run.rs)
├── application/   use cases: projects (pull and tell), actions (which items a commit runs,
│                  and running them), http (sending one configured request), webhooks
│                  (messages and their queue), login, config reload
├── ports/         traits: GitPort, HttpSenderPort, ProcessPort, SecretsPort, DirectoryPort…
└── adapters/
    ├── in/        HTTP (axum handlers, auth, sessions, metrics, UI assets) and the scheduler
    └── out/       git, the HTTP client, script processes, secrets (env, Vault), LDAP, config store
```

A pull lives in `application/projects.rs` and nowhere else: the HTTP handler (sync), the scheduler
and the boot clones all call it, and it hands the commits it brought to `application/actions.rs`,
which decides which items run. Every outbound request, a webhook's or an API's, is built and sent by
`application/http.rs`. A pull and a run each take one configuration snapshot, so a reload in the
middle cannot mix an old item with a new credential.

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
the access rule type (`projects`) with what it grants, `Permissions`, how api_keys.yaml becomes keys
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

What is gitops' own: the items and their runs, the request engine, the script runner, and the HTTP
routes over them.
