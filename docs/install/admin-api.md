# Admin API — runtime / permanent config (firewalld-style)

From v0.6, runic exposes a small **loopback HTTP/JSON admin API** to change the
upstream pool at runtime, without editing the YAML or restarting. The model is
borrowed from [firewalld](https://firewalld.org/): changes are **runtime**
(volatile) by default, and you opt into making them **permanent** with
`?permanent=true`.

## Mental model — three layers

runic merges three layers into the effective upstream pool:

| Layer        | Owner             | Lives in                         | Survives restart? |
| ------------ | ----------------- | -------------------------------- | ----------------- |
| **cold**     | sysadmin          | `runic.yaml` on disk             | yes (it *is* the file) |
| **snapshot** | operator / agent  | `runic.snapshot.json`            | yes (persisted cache) |
| **hot**      | operator / agent  | RAM                              | no                |

Precedence (effective value on a name conflict): **hot > snapshot > cold**.

The snapshot is a **dumb persistent cache**, not a source of truth: last write
wins, no history, no versioning, no rollback. It exists so a runtime change can
survive a restart without re-querying whatever authority pushed it.

This is the same split HAProxy draws between its runtime socket and the saved
config / server state-file — runtime changes are volatile, persistence is an
explicit, separate act.

## Listener

The admin API binds to `admin.addr` from the cold YAML (default
`127.0.0.1:7778`). It is **loopback, no auth** — the bind address is the trust
boundary, exactly like the SOCKS5 surface. Do not expose it off-host.

```yaml
listen:
  addr: "0.0.0.0:7777"
  auth: none

admin:
  addr: "127.0.0.1:7778"   # optional; this is the default

upstreams:
  default:
    kind: http_connect
    host: gw.dataimpulse.com
    port: 823
    auth:
      username_env: DATAIMPULSE_LOGIN
      password_env: DATAIMPULSE_PASSWORD
```

## Endpoints

| Method + path | Effect |
| ------------- | ------ |
| `GET /v1/status` | health, version, uptime, effective pool size |
| `GET /v1/config` | merged effective config (cold ∪ snapshot ∪ hot) |
| `GET /v1/diagnose` | per-upstream effective **source** (`cold` / `snapshot` / `hot`) |
| `GET /v1/diff` | upstreams defined in more than one layer (what shadows what) |
| `POST /v1/upstreams/<name>` | set an upstream in the **hot** layer (runtime only) |
| `POST /v1/upstreams/<name>?permanent=true` | set it in the **snapshot** (persisted) |
| `DELETE /v1/upstreams/<name>` | drop the hot entry; falls back to snapshot or cold |
| `DELETE /v1/upstreams/<name>?permanent=true` | drop hot **and** snapshot; falls back to cold |
| `POST /v1/snapshot/promote` | firewalld `--runtime-to-permanent`: fold the whole hot layer into the snapshot |
| `DELETE /v1/snapshot` | wipe the snapshot file; next boot = cold YAML only |

### Upstream body

The POST body is one upstream entry. Credentials may be **inline** (the usual
admin-API form — lets you rotate without a restart) or **env-var** indirection
(the cold-YAML form):

```json
{ "kind": "http_connect", "host": "gw-us.example", "port": 823,
  "auth": { "username": "user", "password": "secret" } }
```

> The snapshot stores credentials in clear (the file is written `0600`). Treat
> `runic.snapshot.json` as a secret-bearing file.

## Examples

```bash
# Add an upstream at runtime (hot only — gone on restart)
curl -X POST http://127.0.0.1:7778/v1/upstreams/us-residential \
  -d '{"kind":"http_connect","host":"gw-us.example","port":823,
       "auth":{"username":"u","password":"p"}}'

# Where does each upstream come from?
curl http://127.0.0.1:7778/v1/diagnose
# → {"default":"cold","us-residential":"hot"}

# Make the current runtime state permanent
curl -X POST http://127.0.0.1:7778/v1/snapshot/promote
curl http://127.0.0.1:7778/v1/diagnose
# → {"default":"cold","us-residential":"snapshot"}   ← survives restart now

# See what shadows what
curl http://127.0.0.1:7778/v1/diff

# Forget all persisted runtime state
curl -X DELETE http://127.0.0.1:7778/v1/snapshot
```

## firewalld mapping

For operators who know firewalld:

| firewalld | runic |
| --------- | ----- |
| runtime change (`firewall-cmd ...`) | `POST /v1/upstreams/<name>` (hot) |
| `--permanent` | `?permanent=true` (snapshot) |
| `--runtime-to-permanent` | `POST /v1/snapshot/promote` |
| reload from saved config | restart (cold YAML + snapshot replay) |

One thing runic adds that HAProxy lacks: `GET /v1/diff` answers "what differs
between the runtime/persisted state and the config on disk" directly, instead of
diffing dumps by hand.

## Not in v0.6

- Auth / mTLS on the admin API (loopback is the boundary).
- Streaming config changes (WebSocket / SSE).
- Snapshot history / versioning / rollback — by design: it is a dumb cache.
- A `runicctl` CLI — the HTTP API + `curl` is enough for now.
