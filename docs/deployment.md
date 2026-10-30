# Deployment

The image is `ghcr.io/opusprojects/unified-gitops:<version>`, published by each release only. It
holds the binary, the web UI, `git`, and Python 3 with `requests`, `pyyaml` and `jinja2` for scripts.
It holds no configuration: mount yours at `/app/config` (the default `CONFIG_DIR`) or set `CONFIG_DIR`.

## Docker

```bash
docker run -p 8183:8183 \
  -e UNIFIED_GITOPS_KEY="$(openssl rand -hex 24)" \
  -v gitops-state:/var/lib/unified-gitops \
  -v "$PWD/config:/app/config:ro" \
  ghcr.io/opusprojects/unified-gitops:latest
```

- `/var/lib/unified-gitops` holds the project checkouts (and their virtualenvs); a volume keeps them
  across restarts (without one, they are cloned again at boot).
- The process runs as the unprivileged `unified` user.
- `deploy/compose/compose.yaml` is the same as a Compose file.

## Kubernetes

`deploy/k8s/` is a kustomization: Deployment (one replica: the locks are in-process), Service, PVC
for `/var/lib/unified-gitops`, and the config directory from a ConfigMap. Credentials come in as env
vars from a Secret (External Secrets from Vault works unchanged), or straight from Vault through a
`type: vault` credential with Kubernetes auth.

Probes: `/healthz` (alive) and `/readyz` (serving; it does not depend on a git server or a system
being up, so their outage never restarts this pod).

Run **one replica**. Two would each pull and each run every item on every commit; the locks that keep
two pulls of a checkout, or two runs of one item, from overlapping are in-process.

## Production checklist

- API keys as `api_keys` items, values from a Secret; LDAP for people (ldap-login.md).
- A credential per item with only the permissions it needs.
- Repositories pulled with a read-only token; scripts in a repository with stricter reviewers than
  the data they read (`script.project`).
- Dry-run a new item (`POST …/run?dry_run=true`) and read what it would send before its first
  commit; add `on_delete` only once its creates and updates look right.
- Alerts from observability.md.
