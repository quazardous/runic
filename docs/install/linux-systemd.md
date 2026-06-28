# Install — Linux

Three ways to run `runic` on Linux, easiest first:

| # | Path | Service model | Needs |
| - | ---- | ------------- | ----- |
| **A** | [`.deb` / `.rpm` package](#a-from-a-package-recommended) | **system** unit, runs at boot under a transient `DynamicUser` | root, a release |
| **B** | [Prebuilt binary tarball](#b-prebuilt-binary--per-user-service) | **per-user** unit (`systemctl --user`), no root | a release |
| **C** | [Build from source](#c-build-from-source) | **per-user** unit | a Rust toolchain (or Docker) |

A is the standard distro path — one command installs the binary, a default
config, and a hardened service. B and C give you a no-root, per-user daemon and
are the way to go on a box where you can't (or don't want to) install a package.

The daemon hot-reloads its YAML config when you edit it (all paths) — see
[Hot reload](#hot-reload).

---

## A. From a package (recommended)

Each release publishes a Debian and an RPM package on the
[Releases page](https://github.com/quazardous/runic/releases). Download the one
for your distro and install it:

```bash
# Debian / Ubuntu
sudo apt install ./runic_0.3.0-1_amd64.deb

# Fedora / RHEL / openSUSE
sudo dnf install ./runic-0.3.0-1.x86_64.rpm
```

The package installs:

```
/usr/bin/runic                          # the binary (static musl, no deps)
/etc/runic/runic.yaml                   # default config — empty pool, edit it (kept on upgrade)
/usr/lib/systemd/system/runic.service   # system unit, DynamicUser + hardened
```

It does **not** auto-start (so it never carries traffic with a placeholder
config). Edit the config, optionally drop credentials, then enable it:

```bash
# 1. configure the upstream pool (boots empty = refuses traffic until set)
sudoedit /etc/runic/runic.yaml

# 2. upstream credentials, if your YAML references them via *_env
sudo tee /etc/runic/runic.env >/dev/null <<'EOF'
RUNIC_UPSTREAM_USER=your-username
RUNIC_UPSTREAM_PASS=your-password
EOF
sudo chmod 600 /etc/runic/runic.env

# 3. start at boot
sudo systemctl enable --now runic

# 4. verify
systemctl status runic
journalctl -u runic -f
```

The service runs unprivileged under a transient `DynamicUser` (no `useradd`,
nothing to clean up on removal). Its persisted config snapshot — which may hold
cleartext upstream credentials — lands in `/var/lib/runic` (owner-only, `0700`).

Remove with `sudo apt remove runic` / `sudo dnf remove runic`; the uninstall
disables and stops the service first. `/etc/runic/runic.yaml` is a config file,
so your edits survive upgrades and are left behind on removal.

---

## B. Prebuilt binary + per-user service

No root, no package — grab the static musl binary from the
[Releases page](https://github.com/quazardous/runic/releases) and run it as a
per-user systemd service. The unit template uses systemd's `%h` specifier, so
all paths are relative to your home — no edits before copying it in.

### Files

```
~/.local/bin/runic                          # the binary
~/.config/systemd/user/runic.service        # the unit
~/.config/runic/runic.yaml                  # listen + upstream config
~/.config/runic/creds.env                   # upstream creds (chmod 600)
```

### Install

```bash
# 1. binary — download + extract the musl tarball for your arch
curl -fsSL -o runic.tar.gz \
  https://github.com/quazardous/runic/releases/download/v0.3.0/runic-v0.3.0-x86_64-unknown-linux-musl.tar.gz
tar -xzf runic.tar.gz
mkdir -p ~/.local/bin
install -m755 runic-v0.3.0-x86_64-unknown-linux-musl/runic ~/.local/bin/

# 2. config + creds
mkdir -p ~/.config/runic
cp docker/runic/runic.yaml ~/.config/runic/runic.yaml      # from a repo clone, or write your own
cat > ~/.config/runic/creds.env <<'EOF'
DATAIMPULSE_LOGIN=your-username
DATAIMPULSE_PASSWORD=your-password
EOF
chmod 600 ~/.config/runic/creds.env

# 3. unit (from a repo clone)
mkdir -p ~/.config/systemd/user
cp packaging/systemd/runic.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now runic

# 4. verify
systemctl --user status runic
journalctl --user -u runic -f
```

No root, no system unit, no port below 1024. (For the unit to keep running after
you log out, enable lingering once: `loginctl enable-linger "$USER"`.)

---

## C. Build from source

Same per-user service as B, but you compile the binary yourself.

```bash
cargo build --release        # produces ./target/release/runic
```

No Rust toolchain? Build the Docker image and copy the binary out:

```bash
docker build -t runic:local .
id=$(docker create runic:local) && docker cp "$id:/usr/local/bin/runic" ./runic && docker rm "$id"
```

Then follow [section B](#b-prebuilt-binary--per-user-service) from step 1,
using `target/release/runic` (or the `./runic` you copied out) as the binary.

---

## Smoke test

Whichever path you took:

```bash
curl --socks5 127.0.0.1:7777 https://api.ipify.org
```

With real creds → an HTTP 200 and an IP from the upstream. With mock creds →
curl reports a SOCKS5 failure and the journal shows the upstream `HTTP 407`,
which already proves the CONNECT chain works.

The admin API also serves a live status page — open the admin port in a browser:

```
http://127.0.0.1:48484/
```

It shows the active route, whether any traffic is leaving un-proxied, the live
and cumulative session counts, and the runtime upstream pool. The same data is
JSON at `GET /v1/status`. See [`admin-api.md`](admin-api.md).

## Hot reload

`runic` watches its YAML config (the path passed to `--config`) and reloads on
change, debounced ~100 ms.

| Field changed                                    | What happens                                                                  |
| ------------------------------------------------ | ----------------------------------------------------------------------------- |
| `upstream.host` / `port` / `auth.*_env`          | Next session uses the new values; in-flight sessions keep their connect-time settings. |
| `listen.addr`                                    | The listener rebinds. If the new bind fails (port in use), the daemon keeps listening on the previous address and logs a warning. |
| Invalid YAML / parse error                       | Warning logged, previous config stays live. The daemon does not crash.        |

What is **not** hot-reloadable: credentials read from an `EnvironmentFile`
(`creds.env` / `runic.env`) are read once at boot. After editing them,
`systemctl [--user] restart runic` is required.

## Reload check

```bash
# Edit the default upstream host to something that resolves but can't accept the chain
sed -i 's/host: gw.dataimpulse.com/host: example.invalid/' ~/.config/runic/runic.yaml

# Wait < 1s for the watcher to pick it up, then:
curl --socks5 127.0.0.1:7777 https://api.ipify.org
# → curl reports a SOCKS5 failure (proves the new upstream is in effect)

# Restore
sed -i 's/host: example.invalid/host: gw.dataimpulse.com/' ~/.config/runic/runic.yaml
```

## Tear down

```bash
# package install (A)
sudo apt remove runic        # or: sudo dnf remove runic

# per-user install (B / C)
systemctl --user disable --now runic
rm ~/.config/systemd/user/runic.service
systemctl --user daemon-reload
# (config + creds left in ~/.config/runic — remove manually if you really mean it)
```
