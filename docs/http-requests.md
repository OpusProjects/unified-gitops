# HTTP requests

What an API's `api:` block and a webhook's `webhook:` block share: where the request goes, how it
authenticates and signs, which answers count, and how it travels. Each block adds its own (a
webhook's body, an API's per-file flow); these settings work the same in both.

```yaml
url: https://api.example.com/users/$file.stem   # templated
method: PUT                                      # an API's default PUT, a webhook's POST
headers: {X-Team: ops}                           # templated; never a secret
query: {dryRun: "false"}                         # templated, encoded, added to the URL
auth: bearer                                     # see below
signature: {}                                    # see below
success_status: [200-299]                        # default: any 2xx
retry_on_status: [429, 500-599]                  # the default
follow_redirects: false                          # the default
tls:
  ca_file: /etc/ssl/internal-ca.pem
  client_credential: cred-mtls
  insecure_skip_verify: false
proxy: http://proxy.example.com:3128             # or none; left out: HTTPS_PROXY / NO_PROXY
rate_limit: {requests: 10, per_seconds: 1}
```

The item around the block adds `credential` (whose fields `auth` and `signature` read),
`timeout_seconds` (one attempt, at most 300) and `retry`.

## auth

One word, or a mapping with a `type`. Left out: a bearer token when the item's credential has a
`token`, nothing otherwise.

| `auth` | Sends | Reads from the credential |
| --- | --- | --- |
| `none` | Nothing | — |
| `bearer` | `Authorization: Bearer <token>` | `token` |
| `basic` | `Authorization: Basic …` | `username`, `password` (may be empty) |
| `{type: header, name: X-API-Key}` | The token in that header | `token` |
| `{type: query, name: api_key}` | The token as that query parameter | `token` |
| `{type: oauth2, …}` | `Authorization: Bearer <access token>` | `client_id`, `client_secret` |
| `{type: login, …}` | The token a login call answered | whatever its body names |

**OAuth 2.0 client credentials**, kept until shortly before it expires:

```yaml
auth:
  type: oauth2
  token_url: https://idp.example.com/oauth2/token
  scopes: [read, write]
  audience: https://api.example.com              # optional
  client_auth: basic                             # basic (default) | body
```

**A login call first**, for APIs that hand out a session token:

```yaml
auth:
  type: login
  url: https://portainer.example.com/api/auth
  method: POST                                   # default POST
  body: {Username: $credential.username, Password: $credential.password}
  token: $response.jwt                           # or $headers.x-auth-token
  header: Authorization                          # default Authorization
  prefix: "Bearer "                              # default "Bearer "
  ttl_seconds: 300                               # reuse it this long (default 300)
```

A bought token (oauth2, login) that the API answers with a 401 is bought again, and the request
sent again once.

## signature

An HMAC of what is sent, for receivers that check signatures:

```yaml
signature:
  credential: cred-signing         # default: the item's credential
  field: secret                    # default secret
  algorithm: sha256                # sha256 (default) | sha512
  encoding: hex                    # hex (default) | base64
  header: X-Hub-Signature-256      # the default
  prefix: "sha256="                # default by algorithm
  timestamp_header: X-Timestamp    # optional: sign "<unix seconds>.<body>", send the seconds
```

## Statuses and retries

- `success_status` and `retry_on_status` take numbers, ranges (`"200-299"`) and classes (`2xx`).
  An answer in neither is final.
- `retry` is the item's: `attempts` in all, the first included (default 4; a script's default is
  1), waiting `initial_delay_seconds` (1), doubling (`backoff: exponential`) or not (`fixed`), at
  most `max_delay_seconds` (60). A `Retry-After` in seconds is honoured, within that bound.
- No answer at all (refused, timed out, TLS) is retried too; a request that cannot be built (a
  credential without the field auth reads, a value with a line break) is not.

## Transport

- `tls.ca_file` trusts that PEM bundle instead of the system's CAs; `tls.client_credential` names a
  credential whose `cert` and `key` fields hold a client certificate and its key, in PEM, for mutual
  TLS; `insecure_skip_verify` accepts any certificate (a lab; refused beside `ca_file`).
- `proxy` is an `http://`, `https://` or `socks5://` URL, or `none` to bypass the environment's.
- `follow_redirects` is off: a redirect would resend the body, credentials included, wherever the
  receiver points.
- `rate_limit` spaces every request the item sends (retries and polls included) across all its
  runs.

## Templates

`url`, `headers`, `query` and a body are templates: `$event.<field>`, and for an API `$file` and
`$doc`. `${event.project}` ends a name where text follows, `$$` writes a `$`, and a `$` that starts
no name (`$5`) is text. Every template is checked when the configuration loads.
