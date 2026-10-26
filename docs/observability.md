# Observability

## Logs

Structured logs on stdout (`RUST_LOG` sets the level, default `info`). Every run logs one line when
it finishes, with the application, mode, result, planned and applied counts and the commit. Every
refresh and sync made over the API logs an `audit` event with the actor (key name or
`ldap:<username>`), the action and the outcome.

## Metrics

`GET /metrics`, Prometheus text. Public by default, like the probes; set
`server.metrics_require_auth: true` to require a key or a signed-in session.

Gauges are computed on each scrape, not pushed after a run: ages grow with the clock, and a task that
stops running pushes nothing, so only a scrape-time gauge shows the silence.

| Metric                                                    | Labels                   | Meaning                                       |
| --------------------------------------------------------- | ------------------------ | --------------------------------------------- |
| `unified_gitops_application_status`                       | application, status      | 1 on the current status, 0 on the others      |
| `unified_gitops_application_pending_operations`           | application              | Operations the last check found               |
| `unified_gitops_application_unmanaged_objects`            | application              | Objects on the system that git does not declare |
| `unified_gitops_application_last_check_age_seconds`       | application              | Seconds since the last comparison             |
| `unified_gitops_application_consecutive_failures`         | application              | Failed or partial runs in a row               |
| `unified_gitops_application_last_success_age_seconds`     | application              | Seconds since the last run that finished      |
| `unified_gitops_project_sync_consecutive_failures`        | project                  | Failed pulls in a row                         |
| `unified_gitops_project_sync_last_success_age_seconds`    | project                  | Seconds since the last good pull              |
| `unified_gitops_connector_calls_total`                       | application, connector, command, outcome | Calls to the systems: `list` reads, `apply` writes, `describe` starts a run |
| `unified_gitops_connector_call_duration_seconds`             | application, connector, command | How long each call to a system took (histogram) |
| `unified_gitops_http_requests_total`                      | method, path, status     | Requests, by matched route                    |
| `unified_gitops_http_request_duration_seconds`            | method, path             | Latency histogram                             |
| `unified_gitops_config_generation`                        |                          | Bumped by every applied reload                |
| `unified_gitops_config_restart_required`                  |                          | Restart-only keys a reload could not apply    |
| `unified_gitops_build_info`                               | version                  | Constant 1                                    |
| `process_*`                                               |                          | Memory, CPU, open files (Linux)               |

## Alerts worth having

```yaml
- alert: GitopsApplicationFailing
  expr: unified_gitops_application_consecutive_failures >= 3
- alert: GitopsApplicationOutOfSync
  # Only for auto_sync applications; manual ones are out of sync by design until someone syncs.
  expr: unified_gitops_application_status{status="out_of_sync"} == 1
  for: 30m
- alert: GitopsApplicationNotChecked
  expr: unified_gitops_application_last_check_age_seconds > 3 * 3600
- alert: GitopsConnectorCallsFailing
  # A system refusing reads or writes, whatever the application.
  expr: sum by (connector) (rate(unified_gitops_connector_calls_total{outcome="error"}[15m])) > 0
  for: 30m
- alert: GitopsProjectNotPulling
  expr: unified_gitops_project_sync_consecutive_failures >= 3
```
