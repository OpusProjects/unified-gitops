# Web UI

The image serves a web UI on the same address as the API: open the instance in a browser, sign in,
and see every application's status and run history, the connectors, and the projects, credentials,
metrics and configuration behind them. The pending plan and each system's current settings are in
the API (`GET /api/v1/applications/{id}` and `…/state/{kind}`).

The UI is [unified-ui](https://github.com/OpusProjects/unified-ui)'s `apps/unified-gitops`, built
from the same components as unified-api's UI. Each unified-gitops release builds the unified-ui
version it was tested with into its image (`UI_REF` in `docker/Dockerfile`).

## Turning it on

```yaml
# config.yaml
server:
  ui:
    enabled: true
    assets_dir: "/usr/share/unified-gitops/ui"   # where the image puts the build
```

`assets_dir` needs `enabled: true` (the UI signs in through the login page) and is restart-only.

| Path                                      | With a UI                                         | Without              |
| ----------------------------------------- | ------------------------------------------------- | -------------------- |
| `/` and any page of the UI                | the UI                                            | `/` redirects to Swagger |
| `/assets/*`                               | the UI's files, cached for good                   | 404                  |
| `/metrics`                                | the Metrics page for a browser, text for Prometheus | text               |
| `/api/...`, `/swagger-ui/`, `/api-docs/`  | unchanged; an unknown API path is a JSON 404       | unchanged            |

## Signing in

The login page is `/api/v1/login`: an API key, or LDAP when configured (see ldap-login.md). A call
without a session sends the browser there with `?next=` set to the page it was on. A restricted key
or LDAP rule sees only its applications; connectors, projects, credentials and the configuration are
admin-only and leave the menu.

The session cookie is `ugitops_session`, so signing in to unified-gitops and unified-api on the same
host (different ports) keeps two separate sessions.
