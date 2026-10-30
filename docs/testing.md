# Testing

```bash
cargo test                                         # everything: unit and integration
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo deny check && cargo audit                    # licenses and advisories, as CI runs them
```

No network, no Docker and no git remote are needed: repositories are local `file://` ones, the
systems an item calls are small servers on a local port, scripts are shell, and the router is called
as a service rather than over TCP.

## What covers what

| Where                                   | What it proves                                                   |
| --------------------------------------- | ---------------------------------------------------------------- |
| `domain/*.rs` tests                     | Every item's options and load-time checks; templates, conditions, globs, statuses |
| `application/http.rs` tests             | Every auth flow, signing (an RFC 4231 vector), retries, OAuth2 renewal after a 401, login tokens, polling, rate limits, transport |
| `adapters/out/process.rs` tests         | Output tail, exit codes, stdin, a timeout stopping a script and what it started, venv on PATH |
| `adapters/out/http_client` tests        | Redirects only when asked, a client per transport, transports that cannot be built |
| `tests/apis_test.rs`                    | APIs over a real repository and a local system: create, compare skip, update, failure and recovery told, dry run, delete, debounce, scoping, schedule and startup runs |
| `tests/scripts_test.rs`                 | Scripts over two real repositories: arguments, variables, credential, working directory, stdin, a scrubbed environment, per-file runs, failures told, dry run, timeout |
| `tests/project_webhooks_test.rs`        | Pull events: commits and their files, a failing repository told once, its recovery |
| `tests/config_api_test.rs`              | The config directory over HTTP, staged validation, live reload    |
| `tests/ldap_login_test.rs`              | Sign-in, access rules, sessions, CSRF, throttling                 |
| the rest of `tests/`                    | git checkouts, the whole service end to end, Vault, metrics auth, UI assets, `--check-config` |

## Where a new test belongs

- A rule about an item's settings, a template or a glob: a unit test next to it in `domain/`.
- How a request is built or retried: `application/http.rs`, against a scripted sender.
- Something a person, a pipeline or a commit does: `tests/apis_test.rs`, `tests/scripts_test.rs`
  or a new file in `tests/`, through the router and a real repository.
