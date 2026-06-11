# runic

Local SOCKS5 proxy that relays via HTTP CONNECT upstream, written in Rust.
Sized as a drop-in replacement of [gost](https://github.com/go-gost/gost) for
the narrow case "SOCKS5 in, single upstream HTTP CONNECT out, static creds".

## What it does

```
+--------+   SOCKS5    +--------+   HTTP CONNECT    +-------------+   HTTPS    +--------+
| client | ----------> | runic  | ----------------> | DataImpulse | ---------> | target |
+--------+ 127.0.0.1   +--------+ gw.dataimpulse    +-------------+            +--------+
            :7777                  .com:823
            (no auth)              (basic auth)
```

- **Listen**: SOCKS5 on `127.0.0.1:7777` (no auth; loopback exposure enforced by
  the deployment, see install docs).
- **Upstream**: HTTP CONNECT to a single fixed DataImpulse gateway, with
  `Proxy-Authorization: Basic` from env-injected creds.
- **Pump**: pure `tokio::io::copy_bidirectional` after handshake — no inspection,
  no rewriting, no buffering beyond what TCP already does.
- **Hot reload**: the YAML config is watched and applied without restart. New
  sessions pick up the latest values; in-flight sessions keep their connect-time
  config. See the per-OS install docs for details.

The end-to-end TLS is between the client and the target — runic only sees opaque
encrypted bytes once the CONNECT succeeds.

## Scope (this release)

In:

- SOCKS5 CMD `CONNECT` (TCP only), IPv4 / IPv6 / domain ATYP.
- Upstream pool with static YAML config; env-injected or inline creds.
- Multi-provider routing via the SOCKS5 username (v0.7).
- Admin API for runtime / permanent config changes (v0.6).
- Live status surface on the admin port: a self-contained HTML page +
  `GET /v1/status` JSON — active route, leak (`direct`) warning, session/request
  counters, and per-silo-variation counts (no client key needed to view).
- YAML config with file-watch hot reload (debounced ~100 ms).
- Empty / `default`-less pool tolerated: runic can boot bare and be configured
  live via the admin API (unmatched sessions are declined cleanly).
- `direct` upstream kind for credential-free local/CI runs — **not proxied**
  (local IP exposed). Allowed by default but never implicit (must be an explicit
  `kind: direct` upstream); set `RUNIC_ALLOW_DIRECT=0` to forbid it for hardening.
- Switch the active default route by name, live, via the admin API
  (`PUT /v1/route/default`) — clients keep the same local port.
- **Encrypted, per-client config** (opt-in *silo* mode): each client holds its
  own off-box key; runic keeps only ciphertext + a one-way fingerprint on disk,
  so a seized box reveals nothing. Bind by token-in-password or a dedicated
  loopback port. See [`docs/install/silo.md`](docs/install/silo.md).

Out (future):

- Per-task session-ID stickiness (parsed, not yet enforced).
- Smart fail-rate routing, GB tracking.
- Auth on the SOCKS5 / admin surfaces (this release = loopback only).
- UDP ASSOCIATE / BIND.
- Desktop tray app (Windows + Linux).

## Install

Per-OS install guides live in [`docs/`](docs/README.md):

- [`docs/install/docker.md`](docs/install/docker.md) — Docker sidecar
  (OS-agnostic, fastest path to a running service).
- [`docs/install/linux-systemd.md`](docs/install/linux-systemd.md) — Linux,
  per-user systemd unit, with hot-reload on config change.
- [`docs/install/socks5-routing.md`](docs/install/socks5-routing.md) —
  Multi-provider routing via the SOCKS5 username (clients pick which
  upstream of the pool to use per session).
- [`docs/install/admin-api.md`](docs/install/admin-api.md) — Admin API:
  change the upstream pool at runtime and persist with `?permanent=true`
  (firewalld-style runtime/permanent).
- [`docs/install/silo.md`](docs/install/silo.md) — Config silo: encrypted,
  per-client config whose keys never touch the box's disk (opt-in).

## Config reference

`docker/runic/runic.yaml` is the canonical example (mounted into the container
at `/etc/runic/runic.yaml`):

```yaml
listen:
  addr: "0.0.0.0:7777"        # bound inside container; loopback exposure via host port mapping
  auth: none

admin:                        # optional; runtime/permanent admin API (v0.6), loopback only
  addr: "127.0.0.1:48484"

upstreams:                    # pool keyed by name; the routing layer picks per session (v0.7)
  default:                    # this release routes all traffic through `default`
    kind: http_connect
    host: gw.dataimpulse.com
    port: 823
    auth:
      username_env: DATAIMPULSE_LOGIN
      password_env: DATAIMPULSE_PASSWORD
```

Required env vars:

| Name                   | Purpose                                      |
| ---------------------- | -------------------------------------------- |
| `DATAIMPULSE_LOGIN`    | DataImpulse static-gateway username          |
| `DATAIMPULSE_PASSWORD` | DataImpulse static-gateway password          |
| `RUNIC_LOG` (optional) | `tracing` filter, default `runic=info`       |

CLI flags:

```
runic [--config /etc/runic/runic.yaml] [--log <env_filter>]
```

## Operational notes

- **Loopback only.** The `127.0.0.1:` prefix in any port mapping is
  load-bearing — don't drop it, the SOCKS5 surface has no auth.
- **Admin API is loopback + unauthenticated** by default (`127.0.0.1:48484`):
  the bind address is the trust boundary. `runic.snapshot.json` stores upstream
  credentials in clear (written `0600`) — treat it as a secret-bearing file.
- **Status page.** Point a browser at the admin port — `http://127.0.0.1:48484/`
  — for a live, self-refreshing status page (active route, session counts,
  upstream pool); or `curl http://127.0.0.1:48484/v1/status` for the same data
  as JSON. Both are loopback-only — under Docker, publish the admin port
  (`127.0.0.1:48484:48484`) to reach it from the host. See
  [`docs/install/admin-api.md`](docs/install/admin-api.md).
- **Logs are plain stdout/stderr**, structured via `tracing`. `RUNIC_LOG` is an
  `EnvFilter` string (e.g. `runic=debug` for more detail).
- **No state.** Restart is free, no warm-up, no persisted session.
- **No TLS deps in the binary**: end-to-end TLS is the client's job
  (CONNECT-tunneled), and the DataImpulse gateway speaks plain HTTP CONNECT on
  port 823. → `gcr.io/distroless/static-debian12` is enough at runtime.

## Prior art

runic's design borrows from prior work, gratefully acknowledged:

- [gost](https://github.com/go-gost/gost) — the SOCKS5-in / forward-out baseline
  runic is a narrow drop-in for, and the resource-per-name admin API shape with
  a persist toggle.
- [HAProxy](https://www.haproxy.org/) — the runtime-vs-saved-config model behind
  the runtime/permanent split, and the versioned round-trippable state dump.
- [shadowsocks-rust](https://github.com/shadowsocks/shadowsocks-rust) — idiomatic
  tokio service structure and lock-free config-sharing patterns.
- [firewalld](https://firewalld.org/) — the runtime/permanent mental model the
  admin API exposes (`?permanent=true`, runtime-to-permanent).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md). Notable changes are tracked in
[CHANGELOG.md](CHANGELOG.md).

## License

Dual-licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE)
at your option.
