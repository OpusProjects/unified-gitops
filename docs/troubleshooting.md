# Troubleshooting

Start at the application: `GET /api/v1/applications/{id}` (or its page in the UI) has the status,
the last plan with its `errors`, and the run history with each operation's error.

| Symptom                                  | Look at                                     | Usual cause                                           |
| ---------------------------------------- | ------------------------------------------- | ----------------------------------------------------- |
| `failed`, error names a desired-state file | `plan.errors`                             | A typo: unknown kind or top-level key, a missing key, a duplicate |
| `failed`, "has no checkout yet"          | `GET /api/v1/projects`                      | The first pull has not finished, or fails (credentials, URL) |
| `failed`, "describe" or "protocol"       | the connector's stderr in the error            | The script crashed, or speaks another contract version |
| `partial`                                | `history[0].applied[].error`                | The system refused some operations (permissions, validation) |
| Always `out_of_sync` after a sync        | `plan.operations[].changes`                 | The connector's `list` does not normalise (list order, case, defaults) |
| A hand-made object is not deleted        | `plan.unmanaged`                            | `prune` is off, by design                              |
| A new commit is not picked up            | `revision`, project `sync_health`           | The pull fails; checks go on against the old checkout  |
| Nothing happens on its own               | `auto_sync`                                 | Off by default: checks only report                     |
| A check takes long or is killed          | the connector's `timeout_seconds`              | A slow system API; raise the timeout or page the list  |

The local copy of what the system has is at `GET /api/v1/applications/{id}/state/{kind}` and in
`state.dir`: when a diff looks wrong, compare that file with the desired-state file directly.

`unified-gitops --check-config` validates the configuration without starting; a typo in a config
file is named with its key.
