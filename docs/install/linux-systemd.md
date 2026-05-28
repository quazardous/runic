# Install — Linux, systemd user unit

Run `runic` as a per-user systemd service. No root, no system unit, no port
below 1024. The daemon hot-reloads its YAML config when you edit it.

## Files

```
~/.local/bin/runic                          # the binary
~/.config/systemd/user/runic.service        # the unit
~/.config/runic/runic.yaml                  # listen + upstream config
~/.config/runic/creds.env                   # upstream creds (chmod 600)
```

The unit template uses systemd's `%h` specifier so all paths are relative to
your home — no edits required before copying it in.

## Build the binary

```bash
cargo build --release
```

If you don't have a Rust toolchain locally, build the Docker image and copy
the binary out:

```bash
docker build -t runic:0.1 .
id=$(docker create runic:0.1) && docker cp "$id:/usr/local/bin/runic" ./runic && docker rm "$id"
```

## Install

```bash
# 1. binary
mkdir -p ~/.local/bin
cp target/release/runic ~/.local/bin/        # or ./runic if you copied from Docker
chmod +x ~/.local/bin/runic

# 2. config + creds
mkdir -p ~/.config/runic
cp docker/runic/runic.yaml ~/.config/runic/runic.yaml
cat > ~/.config/runic/creds.env <<'EOF'
DATAIMPULSE_LOGIN=your-username
DATAIMPULSE_PASSWORD=your-password
EOF
chmod 600 ~/.config/runic/creds.env

# 3. unit
mkdir -p ~/.config/systemd/user
cp packaging/systemd/runic.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now runic

# 4. verify
systemctl --user status runic
journalctl --user -u runic -f
```

## Smoke test

```bash
curl --socks5 127.0.0.1:7777 https://api.ipify.org
```

With real creds → an HTTP 200 and a residential IP from the upstream. With
mock creds → curl reports a SOCKS5 failure and `journalctl` shows the upstream
`HTTP 407`, which already proves the CONNECT chain works.

## Hot reload

`runic` watches its YAML config (the path passed to `--config`) and reloads on
change, debounced ~100 ms.

| Field changed                                    | What happens                                                                  |
| ------------------------------------------------ | ----------------------------------------------------------------------------- |
| `upstream.host` / `port` / `auth.*_env`          | Next session uses the new values; in-flight sessions keep their connect-time settings. |
| `listen.addr`                                    | The listener rebinds. If the new bind fails (port in use), the daemon keeps listening on the previous address and logs a warning. |
| Invalid YAML / parse error                       | Warning logged, previous config stays live. The daemon does not crash.        |

What is **not** hot-reloadable in this release:

- `DATAIMPULSE_LOGIN` / `DATAIMPULSE_PASSWORD` — read once at boot from
  `EnvironmentFile`. After editing `creds.env`,
  `systemctl --user restart runic` is required.

## Reload check

```bash
# Edit the upstream host to something that resolves but can't accept the chain
sed -i 's/host: gw.dataimpulse.com/host: example.invalid/' ~/.config/runic/runic.yaml

# Wait < 1s for the watcher to pick it up, then:
curl --socks5 127.0.0.1:7777 https://api.ipify.org
# → curl reports a SOCKS5 failure (proves the new upstream is in effect)

# Restore
sed -i 's/host: example.invalid/host: gw.dataimpulse.com/' ~/.config/runic/runic.yaml
```

## Tear down

```bash
systemctl --user disable --now runic
rm ~/.config/systemd/user/runic.service
systemctl --user daemon-reload
# (config + creds left in ~/.config/runic — remove manually if you really mean it)
```
