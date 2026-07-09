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

## Routing & the "upstream is mandatory" rule

An **upstream is mandatory to carry traffic** — runic never goes out direct by
default. A session resolves to an upstream (by `provider=`, the active-route
pointer, or the `default` entry; in silo mode by the variation's token/port).
**If none resolves, the CONNECT is refused** (`REPLY_GENERAL_FAILURE`) — there
is **no implicit direct passthrough**.

Two consequences worth stating plainly:

- **Booting with an empty pool is fine** — an API-driven runic can start with no
  upstreams and have routes pushed later. That is a *boot* state, not an error.
  The "error" is per-session and at CONNECT time: a session that finds no route
  is refused, not silently sent out direct.
- **Direct egress is never implicit.** To make a session go out direct you must
  push an explicit `kind: "direct"` upstream and route to it — see
  [`kind: "direct"`](#kind-direct). It is allowed by default but always explicit;
  `RUNIC_ALLOW_DIRECT=0` forbids it for hardening. An empty silo therefore does
  **not** mean "passthrough"; it means "no route → refused".

## Listener

The admin API binds to `admin.addr` from the cold YAML (default
`127.0.0.1:48484`). It is **loopback, no auth** — the bind address is the trust
boundary, exactly like the SOCKS5 surface. Do not expose it off-host.

```yaml
listen:
  addr: "0.0.0.0:7878"
  auth: none

admin:
  addr: "127.0.0.1:48484"   # optional; this is the default

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
| `GET /` (also `/status`, `/index.html`) | the human status page — a self-contained HTML+JS view that polls `/v1/status` |
| `GET /v1/status` | the live runtime view as JSON (see [Status surface](#status-surface)) |
| `GET /v1/config` | merged effective config (cold ∪ snapshot ∪ hot) |
| `GET /v1/diagnose` | per-upstream effective **source** (`cold` / `snapshot` / `hot`) |
| `GET /v1/diff` | upstreams defined in more than one layer (what shadows what) |
| `POST /v1/upstreams/<name>` | set an upstream in the **hot** layer (runtime only) |
| `POST /v1/upstreams/<name>?permanent=true` | set it in the **snapshot** (persisted) |
| `DELETE /v1/upstreams/<name>` | drop the hot entry; falls back to snapshot or cold |
| `DELETE /v1/upstreams/<name>?permanent=true` | drop hot **and** snapshot; falls back to cold |
| `POST /v1/snapshot/promote` | firewalld `--runtime-to-permanent`: fold the whole hot layer into the snapshot |
| `DELETE /v1/snapshot` | wipe the snapshot file; next boot = cold YAML only |
| `PUT /v1/route/default` | point the default (no-provider) route at a named upstream — body `{"upstream":"<name>"}` (runtime only) |
| `DELETE /v1/route/default` | clear the pointer; the default route falls back to the `default` entry |
| `GET /v1/filter` | the effective domain filter (hot ▷ snapshot ▷ cold). With a silo `Bearer`, that variation's own filter |
| `PUT /v1/filter` | replace the whole domain filter (runtime; `?permanent=true` persists). With a silo `Bearer`, write-through to that variation. See [`filtering.md`](filtering.md) |
| `DELETE /v1/filter` | clear the runtime filter override (`?permanent=true` clears the snapshot too). With a silo `Bearer`, clear that variation's filter |

## Status surface

`GET /v1/status` returns the **live runtime view** — the current effective route,
the hot upstream layer, live session counters, and (when silo mode is on) each
variation with its own counters. `hostname` is the machine's own name (detected
once at first use), so a consumer juggling several runic boxes can tell the
instances apart from this endpoint alone. A small self-contained HTML page at `GET /`
consumes this same endpoint and paints it (auto-refreshing); it ships no external
assets and talks only to the loopback admin port.

```json
{
  "status": "ok",
  "version": "0.2.0",
  "hostname": "scraper-03",
  "uptime_secs": 1234,
  "pool_size": 1,
  "listen": "127.0.0.1:7878",
  "active_route": { "name": "dataimpulse-fr", "kind": "http_connect" },
  "any_active_direct": false,
  "active_sessions": 2,
  "requests_total": 57,
  "upstreams_hot": [ { "name": "lab", "kind": "direct" } ],
  "silo": {
    "enabled": true,
    "auth": "none",
    "variations": [
      { "id": "71c080ed2515", "warm": true,
        "route": { "name": "default", "kind": "http_connect" },
        "connections": 2, "requests": 57,
        "last_access_secs": 0, "ttl_secs_remaining": 604800 }
    ]
  }
}
```

- `active_route` — the upstream a no-provider session resolves to right now
  (`null` for an empty / default-less pool), with its **kind**.
- `any_active_direct` — `true` iff at least one **active** session is routing
  through a `kind: direct` upstream (local IP exposed). The conservative "is any
  traffic leaking right now?" signal — a declared-but-unused `direct` upstream
  does **not** trip it.
- `upstreams_hot` — only the **hot** layer (runtime-pushed), not the cold YAML.
- `silo.variations[]` — one row per variation, read entirely from the cleartext
  index + RAM counters: **no token, no decryption**. `connections` is the live
  active-session gauge; `requests` is cumulative (survives idle eviction).
  `route` is resolved only while the variation is warm (`null` when cold —
  a cold variation has no live sessions, so it cannot leak). Viewing the status
  never extends a variation's lifetime.

### Upstream body

The POST body is one upstream entry. Credentials may be **inline** (the usual
admin-API form — lets you rotate without a restart) or **env-var** indirection
(the cold-YAML form):

```json
{ "kind": "http_connect", "host": "gw-us.example", "port": 823,
  "auth": { "username": "user", "password": "secret" } }
```

`kind` defaults to `http_connect`. For `http_connect`, `host`, `port` and
`auth` are **required**. There is no implicit fallback: a session that resolves
to no upstream is **refused** (CONNECT fails) rather than going out direct —
see [Routing & the "upstream is mandatory" rule](#routing--the-upstream-is-mandatory-rule).

> The snapshot stores credentials in clear (the file is written `0600`). Treat
> `runic.snapshot.json` as a secret-bearing file.

#### `kind: "direct"`

A `direct` upstream makes a plain TCP connect straight to the target — **not
proxied, the local IP is exposed**. It exists so a `runic` route can be
exercised without a real gateway (dev/CI, or a silo smoke-test).

**Direct is allowed by default — but it is never implicit.** The guard against
accidental direct egress is not an env flag; it's that **an upstream is always
explicit**: a session only goes out direct if you deliberately declared a
`kind: "direct"` upstream *and* routed to it. An empty pool or empty silo is a
dead end (CONNECT refused), never a silent passthrough — see
[Routing & the "upstream is mandatory" rule](#routing--the-upstream-is-mandatory-rule).

For **prod hardening**, set `RUNIC_ALLOW_DIRECT=0` on the process to forbid
direct outright — pushing or loading a `kind: "direct"` upstream then fails:

```
upstreams.<name> kind=direct is forbidden by RUNIC_ALLOW_DIRECT=0 —
direct mode is NOT proxied (local IP exposed)
```

`RUNIC_ALLOW_DIRECT=0` is the only knob (an out-of-band opt-out, set where the
process is launched — systemd `Environment=`, Docker `-e`, or an `export`); any
other value, or unset, leaves direct allowed.

The canonical body is **`kind` alone** — `host`, `port` and `auth` are
meaningless for direct and are **ignored if present** (the stored entry zeroes
them):

```json
{ "kind": "direct" }
```

`kind: "direct"` is a **stable contract**. Any active session routing through a
direct upstream flips `any_active_direct` to `true` in `GET /v1/status` (the
"is any traffic leaking right now?" signal).

## Examples

```bash
# Add an upstream at runtime (hot only — gone on restart)
curl -X POST http://127.0.0.1:48484/v1/upstreams/us-residential \
  -d '{"kind":"http_connect","host":"gw-us.example","port":823,
       "auth":{"username":"u","password":"p"}}'

# Where does each upstream come from?
curl http://127.0.0.1:48484/v1/diagnose
# → {"default":"cold","us-residential":"hot"}

# Make the current runtime state permanent
curl -X POST http://127.0.0.1:48484/v1/snapshot/promote
curl http://127.0.0.1:48484/v1/diagnose
# → {"default":"cold","us-residential":"snapshot"}   ← survives restart now

# See what shadows what
curl http://127.0.0.1:48484/v1/diff

# Flip the active provider for new sessions, by name (clients don't notice)
curl -X PUT http://127.0.0.1:48484/v1/route/default \
  -H 'content-type: application/json' -d '{"upstream":"us-residential"}'
# ... later, fall back to the `default` entry
curl -X DELETE http://127.0.0.1:48484/v1/route/default

# Forget all persisted runtime state
curl -X DELETE http://127.0.0.1:48484/v1/snapshot
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
