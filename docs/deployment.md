# Deployment

The image is `ghcr.io/opusprojects/unified-gitops:<version>`, published from version tags only. It
holds the binary, the web UI, Python 3 with `requests`, `pyyaml` and `jinja2` for connectors, and `git`.
It holds no configuration: mount yours at `/app/config` (the default `CONFIG_DIR`) or set `CONFIG_DIR`.

## Docker

```bash
docker run -p 8183:8183 \
  -e UNIFIED_GITOPS_KEY="$(openssl rand -hex 24)" \
  -v gitops-state:/var/lib/unified-gitops \
  -v "$PWD/config:/app/config:ro" \
  ghcr.io/opusprojects/unified-gitops:latest
```

- `/var/lib/unified-gitops` holds the project checkouts and the local copy of each system's settings;
  a volume keeps them across restarts (without one, they are rebuilt at the first check).
- The process runs as the unprivileged `unified` user.
- `deploy/compose/compose.yaml` is the same as a Compose file.

## Kubernetes

`deploy/k8s/` is a kustomization: Deployment (one replica: the locks are in-process), Service, PVC
for `/var/lib/unified-gitops`, and the config directory from a ConfigMap. Credentials come in as env
vars from a Secret (External Secrets from Vault works unchanged), or straight from Vault through a
`type: vault` credential with Kubernetes auth.

Probes: `/healthz` (alive) and `/readyz` (serving; it does not depend on the systems being up, so an
outage of a managed system never restarts this pod).

Run **one replica**. Two would each check and apply on their own schedule; the per-application lock
that keeps a check and a sync from overlapping is in-process.

## Production checklist

- API keys as `api_keys` items, values from a Secret; LDAP for people (ldap-login.md).
- A credential per application with only the permissions its connector needs.
- Desired-state repositories pulled with a read-only token.
- Start every application with `auto_sync: false` and `prune: false`; turn them on once its plans
  have been read for a while and look right.
- Alerts from observability.md.
