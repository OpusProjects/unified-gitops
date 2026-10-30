# Security Policy

## Supported versions

Only the latest release line receives security fixes.

| Version | Supported |
|---|---|
| 0.x (latest release) | Yes |
| older tags | No |

## Reporting a vulnerability

Please **do not open a public issue** for security problems.

Use GitHub's private vulnerability reporting instead:
[Report a vulnerability](https://github.com/OpusProjects/unified-gitops/security/advisories/new)
— it opens a private thread with the maintainers.

Include what you can: affected endpoint or component, reproduction steps, and
impact. You should hear back within a week. Once a fix ships, the advisory is
published and credited unless you prefer otherwise.

## Scope notes

- Unified GitOps is designed to run on a **trusted internal network**; treat
  network exposure and key handling as part of your deployment's threat model.
- Authentication is **API keys** declared as `api_keys` items in the configuration: each key is
  `role: admin` (everything) or restricted to explicit project ids, and
  its secret is read from the environment variable the definition names —
  never from the YAML. `UNIFIED_GITOPS_KEY`, if set, is one extra admin key. Keys are compared in constant time. With **no
  keys configured at all, authentication is off** and every caller is treated
  as admin — the startup log says so loudly; do not run that way outside
  local development.
- `/healthz`, `/readyz` and the Swagger UI are always public. `/metrics` is
  public by default, and its exposition names every project, API, script and
  webhook — a description of what this instance reaches. On a shared network, set
  `server.metrics_require_auth: true`.
- The **configuration API** (`config_api.enabled`, off by default) makes the
  configuration directory writable over HTTP for admin keys — the API keys
  included, which is the same authority as editing the directory the container
  mounts. With it on, treat every admin key as root on the instance. Two
  guardrails are enforced before a reload commits: a change that would leave
  the API with no keys at all is refused, as is one naming a key env var that
  is not set. All config writes and reloads land in the audit log.
- **This service writes to production systems.** Scripts run with the
  daemon's privileges and their item's credential; APIs send what the watched
  repository holds. Config files, the scripts' repositories and the watched
  repositories are trusted input: whoever can push to a watched repository
  decides what is sent at the next pull, though never where it goes or as whom
  (the URL and the credential live in the configuration, not in the
  repository), and whoever can push to a scripts repository decides what runs.
  Protect those repositories like production access, keep scripts in a
  repository with stricter reviewers than the data (`script.project`), give
  each item a credential with only the permissions it needs, and dry-run a new
  item before its first commit.
- Credentials are never stored by the service; they are resolved at the start of each run
  from environment variables, files, or Vault (KV v2, token or Kubernetes
  auth), passed to scripts via their environment (which starts empty: a
  script never sees the service's own variables). Environment and file
  credentials are read on every use; a value read from Vault is held in a
  short in-memory cache (the Vault credential's `cache_ttl_seconds`, default
  60).
- There is **no rate limiting** on authentication attempts — a deliberate
  trade for the trusted-network posture. Front the service with a proxy if
  your deployment needs it.
