# Secrets

How credentials resolve, rotate, and reach the scripts. The one principle
everything follows: **configuration never holds a secret** — a `credentials` item
describes *where* to read each value, and the infrastructure (env vars,
mounted files, Vault) delivers it. Field reference lives in
[configuration → credentials](configuration.md#credentials-and-api_keys).

- [The three backends](#the-three-backends)
- [Resolution order](#resolution-order)
- [What scripts receive](#what-scripts-receive)
- [The resolution cache and rotation](#the-resolution-cache-and-rotation)
- [Native Vault](#native-vault)
- [Failure semantics](#failure-semantics)
- [API keys are separate](#api-keys-are-separate)

---

## The three backends

A credential names one delivery mechanism for its values; the three coexist
in one deployment, so migrating between them is per credential, never
all-or-nothing.

| Backend | Declared by | Delivery |
|---|---|---|
| Environment variables | `env_vars` (field → variable) | `SECTION9_USERNAME` etc., injected by the platform (k8s Secret via `envFrom`, `.env`, ESO) |
| JSON file | `secret_file` + `secret_keys` | A mounted file of values, re-read per resolution |
| Vault (KV v2) | `vault` (a `type: vault` credential) + `vault_path` | Read over HTTPS at resolution time |

`file_keys` is orthogonal to all three: files a script consumes **by path**
(SSH keys, certificates) — the path is delivered, never the content.

---

## Resolution order

Per credential, exactly one value source wins — there is no fallback chain
between backends, because a secret silently coming from somewhere unexpected
is worse than an error.

1. `vault_path` set → the value comes from Vault (env/file settings on the
   same credential are not consulted).
2. Otherwise `env_vars` → environment variables, then the deprecated
   `env_prefix`, then `secret_file` → the JSON file.
3. `file_keys` entries are appended as `<key>_path` values in every case.

`env_vars` maps our names to full variable names: `{password: LDAP_PASSWORD}`
reads `LDAP_PASSWORD` and delivers `password`. `secret_keys` maps our names to
fields of a Vault secret or of the JSON file.

Setting both `env_vars` and `env_prefix` is refused at load, and so is
`secret_keys` next to `env_vars` without a `vault_path` or `secret_file`.

The older form, `env_prefix` + `secret_keys` (prefix joined to each value with
`_`), still loads but is deprecated: startup and `--check-config` name the
`env_vars` to write in its place, and a later release refuses it.

---

## What scripts receive

A connector sees only the credentials its application declares, and nothing
else: the child environment is scrubbed, so no script can read the API keys or
another application's values. Each field arrives twice:

- `CREDENTIAL_<ID>_<FIELD>`, always. `<ID>` is the credential's name without a
  leading `cred-`/`cred_`, anything outside `a-z0-9` made `_`: the `password`
  of `cred-portainer-ldap` is `CREDENTIAL_PORTAINER_LDAP_PASSWORD`.
- `CREDENTIAL_<FIELD>`, only when no other declared credential has that field.
  Two credentials with a `password` deliver neither as `CREDENTIAL_PASSWORD`,
  rather than one silently overwriting the other.

`env_vars: {username: D42_USERNAME}` on `cred-d42` reads `D42_USERNAME` and
delivers `CREDENTIAL_D42_USERNAME` (and `CREDENTIAL_USERNAME` when it is the
only one); a `file_keys` entry `ssh_key: /run/secrets/id_rsa` delivers
`CREDENTIAL_SSH_KEY_PATH`. Git checkouts consume the same shapes
(`username`, `ssh_key_path`, `token`) and never put a secret on a command
line. See [connectors](connectors.md) for the full script environment.

---

## The resolution cache and rotation

Resolution runs on every check of every application. Against env vars that is
free, and a file is a small local read, so those are read fresh every time: a
rotated mounted file is seen on the next check. Against Vault it would be a
request storm, so each Vault credential reuses what it read for its own
`cache_ttl_seconds` (default 60; `0` disables).

That TTL **is** the Vault's rotation latency, stated plainly: a value rotated
in Vault is picked up within `cache_ttl_seconds`, not on the very next sync.
Errors are never cached — a transient backend blip is retried on the next
resolution instead of being remembered for the TTL.

The deprecated `secrets.cache_ttl_seconds` in `config.yaml`, while set, still
caches every credential for that long, as it did.

---

## Native Vault

Declare the Vault as a credential of `type: vault`, and give each credential
that reads from it `vault:` and a `vault_path`; everything without a
`vault_path` keeps resolving exactly as before.

```yaml
# credentials/cred-vault.yaml — the connection to a Vault
credentials:
  - name: cred-vault
    description: Vault
    type: vault
    address: https://vault.example.com:8200
    mount: secret                  # KV v2 mount (default "secret")
    env_vars:
      token: VAULT_TOKEN           # token auth: re-read per resolution,
                                   # so rotating the token needs no restart
    # kubernetes_role: unified-gitops # OR Kubernetes auth: the service-account
    # jwt_path: /var/run/secrets/kubernetes.io/serviceaccount/token
    #                              # JWT is exchanged for a client token,
    #                              # cached and renewed at 80% of its lease
    timeout_seconds: 10            # bound on every request (default 10)
    cache_ttl_seconds: 60          # how long a value read here is reused
                                   # (default 60; 0 = every resolution)

# credentials/cred-isilon-vault.yaml — a login read from it
credentials:
  - name: cred-isilon-vault
    type: username_password
    vault: cred-vault
    vault_path: ops/isilon-prod
    secret_keys:                   # our key -> field in the Vault secret
      username: user               # (omit secret_keys to take every field)
      password: pass
```

The adapter speaks **KV v2 only** (`/v1/<mount>/data/<path>`) and says so if
a v1-shaped answer comes back. A `vault_path` naming no Vault, or naming a
credential that is not one, fails validation at startup. The old
`secrets.vault:` block in `config.yaml` still works, deprecated (see
[configuration](configuration.md#credentials-and-api_keys)).

---

## Failure semantics

A credential that fails to resolve **fails that application's run,
naming the credential** — never a silent skip that would let a connector run
half-authenticated and fail later with a confusing error.

The failure lands in the application's run errors and `sync_health` (and its
Prometheus gauges), the failing task backs off, and — because errors are not
cached — the next attempt re-asks the backend.

---

## API keys are separate

The keys consumers present to *this* API live in `api_keys` items, follow the
same never-in-YAML principle (each definition names the env var holding its
secret as `env_vars: {key: VAR}`; the older `env: VAR` is deprecated), and are
documented with the [API](api.md#authentication).

Their rotation is deliberately external: swap the env var's value and restart.
How the env vars and files themselves get into the deployment — plain Secret,
Sealed Secrets, External Secrets Operator — is the
[deployment page's](deployment.md#secrets-three-variants) decision.
