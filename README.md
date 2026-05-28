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
  docker port mapping).
- **Upstream**: HTTP CONNECT to a single fixed DataImpulse gateway, with
  `Proxy-Authorization: Basic` from env-injected creds.
- **Pump**: pure `tokio::io::copy_bidirectional` after handshake — no inspection,
  no rewriting, no buffering beyond what TCP already does.

The end-to-end TLS is between the client and the target — runic only sees opaque
encrypted bytes once the CONNECT succeeds.

## Scope (this release)

In:

- SOCKS5 CMD `CONNECT` (TCP only), IPv4 / IPv6 / domain ATYP.
- Single upstream, static config, env-injected creds.
- Static YAML config, no admin API, no hot reload.

Out (future):

- Multi-provider routing.
- Per-task session-ID rotation.
- Smart fail-rate routing, GB tracking.
- Admin API, SIGHUP reload.
- Auth on the SOCKS5 surface (this release = loopback only).
- UDP ASSOCIATE / BIND.

## Config

`docker/runic/runic.yaml` (mounted into the container at `/etc/runic/runic.yaml`):

```yaml
listen:
  addr: "0.0.0.0:7777"        # bound inside container; loopback exposure via compose port mapping
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

## Build & run

### Docker (recommended)

```bash
docker build -t runic:0.1 .
docker run --rm -it \
  -p 127.0.0.1:7777:7777 \
  -v "$PWD/docker/runic/runic.yaml:/etc/runic/runic.yaml:ro" \
  -e DATAIMPULSE_LOGIN=... \
  -e DATAIMPULSE_PASSWORD=... \
  runic:0.1
```

### docker-compose sidecar

See `docker/runic/compose.snippet.yaml` for the block to paste into your compose
`services:` map.

### Local cargo

```bash
DATAIMPULSE_LOGIN=... DATAIMPULSE_PASSWORD=... \
  cargo run --release -- --config docker/runic/runic.yaml
```

## Smoke test

```bash
./scripts/smoke.sh
```

What it checks:

- **With real creds** → `200 OK` + a residential FR IP from `api.ipify.org`.
- **With mock creds** → the upstream returns `407 Proxy Authentication Required`;
  runic surfaces it as a SOCKS5 `general failure` reply, so curl reports a
  proxy-side failure. **That's still success of the chain wiring** — it proves
  the CONNECT round-trip works; only the auth is wrong. Check `docker logs runic`
  to see the upstream 407.

## Local-dev note: port 7777 collision

If your dev machine already binds `127.0.0.1:7777` to another process, remap
the host side of the compose mapping — the container still listens on `7777`
internally:

```
ports:
  - "127.0.0.1:7780:7777"     # dev-only host remap
```

Then test with `PROXY=127.0.0.1:7780 ./scripts/smoke.sh`. Inside a real
sidecar compose this doesn't apply, every container has its own stack.

## Operational notes

- **Loopback only.** The `127.0.0.1:` prefix in the compose port mapping is
  load-bearing — don't drop it, the SOCKS5 surface has no auth.
- **Logs are plain stdout/stderr**, structured via `tracing`. `RUNIC_LOG` is an
  `EnvFilter` string (e.g. `runic=debug` for more detail).
- **No state.** Restart is free, no warm-up, no persisted session.
- **No TLS deps in the binary**: end-to-end TLS is the client's job
  (CONNECT-tunneled), and the DataImpulse gateway speaks plain HTTP CONNECT on
  port 823. → `gcr.io/distroless/static-debian12` is enough at runtime.

## License

Dual-licensed under MIT OR Apache-2.0.
