# Install — Docker sidecar

Run `runic` as a container next to whatever consumes its SOCKS5 endpoint.
OS-agnostic (Linux / macOS / Windows hosts all work, since everything happens
inside the container).

## One-shot

```bash
docker build -t runic:0.1 .
docker run --rm -it \
  -p 127.0.0.1:7777:7777 \
  -v "$PWD/docker/runic/runic.yaml:/etc/runic/runic.yaml:ro" \
  -e DATAIMPULSE_LOGIN=... \
  -e DATAIMPULSE_PASSWORD=... \
  runic:0.1
```

## docker-compose sidecar

Paste this into your compose `services:` map (see
[`docker/runic/compose.snippet.yaml`](../../docker/runic/compose.snippet.yaml)
for the canonical version with comments):

```yaml
services:
  runic:
    image: ghcr.io/quazardous/runic:0.1
    container_name: runic
    restart: unless-stopped
    ports:
      - "127.0.0.1:7777:7777"     # loopback only — not exposed on the LAN
    volumes:
      - ./docker/runic/runic.yaml:/etc/runic/runic.yaml:ro
    environment:
      DATAIMPULSE_LOGIN: ${DATAIMPULSE_LOGIN}
      DATAIMPULSE_PASSWORD: ${DATAIMPULSE_PASSWORD}
      RUNIC_LOG: "runic=info"
```

The `127.0.0.1:` prefix in the port mapping is load-bearing — the SOCKS5
surface has no auth and is meant for loopback only.

## Smoke test

```bash
./scripts/smoke.sh
```

With real upstream creds → `200 OK` and a residential IP from the upstream
gateway. With mock creds → curl reports a SOCKS5 connection failure and the
container logs show the upstream's `HTTP 407 Proxy Authentication Required`,
which already proves the CONNECT chain works.

## Local-dev note: port 7777 already in use

If `127.0.0.1:7777` is taken on your dev box, remap the host side of the port
mapping — the container still listens on `7777` internally:

```
ports:
  - "127.0.0.1:7780:7777"     # dev-only host remap
```

Then run the smoke test with `PROXY=127.0.0.1:7780 ./scripts/smoke.sh`.
