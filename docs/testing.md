# Testing

```bash
cargo test                                         # everything: unit and integration
cargo fmt --check && cargo clippy --all-targets -- -D warnings
cargo deny check && cargo audit                    # licenses and advisories, as CI runs them
```

No network, no Docker and no git remote are needed: tests run connectors as local scripts, use temp
directories for checkouts and state, and call the router as a service rather than over TCP.

## What covers what

| Where                                   | What it proves                                                   |
| --------------------------------------- | ---------------------------------------------------------------- |
| `domain/plan.rs` tests                  | Only declared fields compared; creates, updates, unmanaged, prune |
| `domain/desired.rs` tests               | Desired-state parsing: exact scalars, strict keys, bad items      |
| `domain/connector.rs` tests                | described kinds: `depends_on` ordering, unknown kinds and cycles refused |
| `adapters/out/connectors/process.rs` tests | Scrubbed env, write flag only on apply, a hung connector is killed   |
| `adapters/out/state/fs.rs` tests        | The local copy reads back exactly (strings stay strings)          |
| `tests/reconcile_test.rs`               | The whole loop over HTTP with `tests/connectors/kv.py`: refresh, sync in order, the local copy, drift, prune, invalid state touching nothing, restricted keys |
| `tests/config_api_test.rs`              | The config directory over HTTP, staged validation, live reload    |
| `tests/ldap_login_test.rs`              | Sign-in, access rules, sessions, CSRF, throttling                 |
| the rest of `tests/`                    | git checkouts, Vault, metrics auth, UI assets, `--check-config`   |

## Where a new test belongs

- A rule about desired state or the diff: a unit test next to it in `domain/`.
- Something a person or pipeline does over the API: `tests/reconcile_test.rs`, through the router.
- A connector's behaviour: test the connector script on its own (see docs/connectors.md); the core's tests
  use the scripts in `tests/connectors/` (`kv.py` and the SDK are copies from unified-connectors).
