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
- Single upstream, static config, env-injected creds.
- YAML config with file-watch hot reload (debounced ~100 ms).

Out (future):

- Multi-provider routing.
- Per-task session-ID rotation.
- Smart fail-rate routing, GB tracking.
- Admin API on a separate port.
- Auth on the SOCKS5 surface (this release = loopback only).
- UDP ASSOCIATE / BIND.
- Desktop tray app (Windows + Linux).

## Install

Per-OS install guides live in [`docs/`](docs/README.md):

- [`docs/install/docker.md`](docs/install/docker.md) — Docker sidecar
  (OS-agnostic, fastest path to a running service).
- [`docs/install/linux-systemd.md`](docs/install/linux-systemd.md) — Linux,
  per-user systemd unit, with hot-reload on config change.

## Config reference

`docker/runic/runic.yaml` is the canonical example (mounted into the container
at `/etc/runic/runic.yaml`):

```yaml
listen:
  addr: "0.0.0.0:7777"        # bound inside container; loopback exposure via host port mapping
  auth: none

upstream:
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
- **Logs are plain stdout/stderr**, structured via `tracing`. `RUNIC_LOG` is an
  `EnvFilter` string (e.g. `runic=debug` for more detail).
- **No state.** Restart is free, no warm-up, no persisted session.
- **No TLS deps in the binary**: end-to-end TLS is the client's job
  (CONNECT-tunneled), and the DataImpulse gateway speaks plain HTTP CONNECT on
  port 823. → `gcr.io/distroless/static-debian12` is enough at runtime.

## License

Dual-licensed under MIT OR Apache-2.0.
