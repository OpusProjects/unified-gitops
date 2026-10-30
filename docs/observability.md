# Observability

## Logs

Structured logs on stdout (`RUST_LOG` sets the level, default `info`). Every pull, webhook send
and run that could not start logs a line. Every pull, run, dry run and test send made over the API
logs an `audit` event with the actor (key name or `ldap:<username>`), the action and the outcome.

## Metrics

`GET /metrics`, Prometheus text. Public by default, like the probes; set
`server.metrics_require_auth: true` to require a key or a signed-in session.

Gauges are computed on each scrape, not pushed after a run: ages grow with the clock, and a task that
stops running pushes nothing, so only a scrape-time gauge shows the silence.

| Metric                                                    | Labels                   | Meaning                                       |
| --------------------------------------------------------- | ------------------------ | --------------------------------------------- |
| `unified_gitops_project_sync_consecutive_failures`        | project                  | Failed pulls in a row                         |
| `unified_gitops_project_sync_last_success_age_seconds`    | project                  | Seconds since the last good pull              |
| `unified_gitops_runs_total`                               | kind, item, outcome      | Runs of APIs and scripts (`kind` api or script); dry runs are not counted |
| `unified_gitops_run_duration_seconds`                     | kind, item               | How long a run took (histogram)               |
| `unified_gitops_git_requests_total`                       | project, operation, outcome | Requests to git servers: a `clone` or a `pull` per project |
| `unified_gitops_git_request_duration_seconds`             | project, operation       | How long each request to a git server took (histogram) |
| `unified_gitops_webhooks_total`                      | webhook, event, outcome | Messages sent (`success`), failed after retries (`error`), or dropped by a full queue |
| `unified_gitops_webhook_duration_seconds`            | webhook             | How long a send took, retries included (histogram) |
| `unified_gitops_http_requests_total`                      | method, path, status     | Requests, by matched route                    |
| `unified_gitops_http_request_duration_seconds`            | method, path             | Latency histogram                             |
| `unified_gitops_config_generation`                        |                          | Bumped by every applied reload                |
| `unified_gitops_config_restart_required`                  |                          | Restart-only keys a reload could not apply    |
| `unified_gitops_build_info`                               | version                  | Constant 1                                    |
| `process_*`                                               |                          | Memory, CPU, open files (Linux)               |

## Alerts worth having

```yaml
- alert: GitopsRunsFailing
  # An API or a script failing whatever the commit.
  expr: sum by (kind, item) (increase(unified_gitops_runs_total{outcome="error"}[1h])) >= 3
- alert: GitopsWebhooksFailing
  expr: sum by (webhook) (increase(unified_gitops_webhooks_total{outcome=~"error|dropped"}[1h])) > 0
- alert: GitopsProjectNotPulling
  expr: unified_gitops_project_sync_consecutive_failures >= 3
- alert: GitopsProjectStale
  # A project not pulled successfully for hours: its changes are not being acted on.
  expr: unified_gitops_project_sync_last_success_age_seconds > 3 * 3600
```
