# Troubleshooting

Start at the item: `GET /api/v1/apis/{name}` or `GET /api/v1/scripts/{name}` (or its page in the
UI) has its last runs, why each ran, at which commit, and every step's status, exit code, output
and error. A webhook's sends are at `GET /api/v1/webhooks/{name}`; a project's pulls at
`GET /api/v1/projects`.

| Symptom                                  | Look at                                     | Usual cause                                           |
| ---------------------------------------- | ------------------------------------------- | ----------------------------------------------------- |
| A commit runs nothing                    | the project's `history` and `sync_health`   | The pull has not happened yet (interval) or fails; or the item's `trigger.paths` select none of the files (globs are per segment: `users/*.json` is not `users/team/a.json`) |
| A run fails before any step              | the run's `error`                           | A credential that does not resolve, a script file not in its project yet |
| An API step `HTTP 4xx`                   | the step's `error` (the start of the answer) | A wrong URL, body or permission; a 4xx is not retried |
| An API keeps updating the same object    | the step's `changed_fields`                 | The API changes a field after a write: add it to `compare.ignore_fields` |
| A script `exit code N`                   | the step's `output`                         | The script's own error; its environment is scrubbed, so a variable it expects from the host may be missing |
| A script `timed out after Ns`            | `timeout_seconds`                           | Raise it, or make the script faster; its whole process group was stopped |
| `does not run by hand` (409)             | `trigger.on`                                | `manual` is not listed; a dry run is always allowed |
| A webhook never arrives                  | its history, `unified_gitops_webhooks_total` | The receiver refuses it, or the event is not in `events`, or `trigger.paths` filtered the commit |
| What would an item do?                   | `POST …/run?dry_run=true`                   | Builds every request (with bodies) or command, writes nothing |

`unified-gitops --check-config [dir]` validates the configuration without starting; a typo in a
config file, an unknown template field or a path that escapes its checkout is named with its key.
It checks `dir` when given, else `CONFIG_DIR`, else `./config`.
