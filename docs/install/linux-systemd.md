# Install — Linux

Three ways to get `runic` onto a Linux box, easiest first:

| # | Binary comes from | Default service model | Needs |
| - | ----------------- | --------------------- | ----- |
| **A** | [`.deb` / `.rpm` package](#a-from-a-package-recommended) | **system** unit, runs at boot under a transient `DynamicUser` | root, a release |
| **B** | [Prebuilt binary tarball](#b-prebuilt-binary--per-user-service) | **per-user** unit (`systemctl --user`), no root | a release |
| **C** | [Build from source](#c-build-from-source) | **per-user** unit | a Rust toolchain (or Docker) |

A is the standard distro path — one command installs the binary, a default
config, and a hardened service. B and C give you a no-root, per-user daemon and
are the way to go on a box where you can't (or don't want to) install a package.

The table pairs each install with its *natural* service model, but the two axes
are independent: the per-user unit runs **any** runic binary. In particular you
can install the package for the binary and clean upgrades, yet run the service
in your own session — see
[Packaged binary, per-user service](#packaged-binary-per-user-service).

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

### Packaged binary, per-user service

The system unit is the *default* way to run the binary the package installs —
not the only one. To keep the package (binary + upgrades via `apt`/`dnf`) but
run runic in **your own session**, skip steps 1–4 above entirely — config and
creds live in your home instead, editable without `sudo` — and point a per-user
unit at `/usr/bin/runic`:

```bash
# config lives in your home; the packaged default is a fine starting point
mkdir -p ~/.config/runic
cp /etc/runic/runic.yaml ~/.config/runic/runic.yaml
"${EDITOR:-vi}" ~/.config/runic/runic.yaml   # add your upstream(s)

# same unit as the tarball's (section B) — only ExecStart differs
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/runic.service <<'EOF'
[Unit]
Description=runic — local SOCKS5 to upstream HTTP CONNECT proxy
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/runic --config %h/.config/runic/runic.yaml
EnvironmentFile=-%h/.config/runic/creds.env
NoNewPrivileges=yes
Restart=on-failure
RestartSec=2

[Install]
WantedBy=default.target
EOF
systemctl --user daemon-reload
systemctl --user enable --now runic
```

Everything else from [section B](#b-prebuilt-binary--per-user-service) applies
unchanged: the optional `~/.config/runic/creds.env`, lingering, the smoke test.

Two caveats:

- **Don't skip the config copy.** The unit passes an **explicit** `--config`
  path, and an explicit path must exist — runic exits with
  `file not found — create it (an empty file is a valid config) or check the
  path`, and `Restart=on-failure` turns that into a crash-loop
  (`journalctl --user -u runic` shows the message; `systemctl status` only
  shows the exit code). Only the *platform default* path
  (`/etc/runic/runic.yaml`, i.e. running `runic` with no `--config` at all)
  is allowed to be absent: there runic boots on built-in defaults with a
  warning, and hot-loads the file if you create it later.
- **Don't run both.** If the system service is enabled, disable it first
  (`sudo systemctl disable --now runic`) — otherwise the two instances fight
  over the same loopback ports.
- **Don't `systemctl edit` a `User=you` into the system unit instead.** Its
  hardening assumes `DynamicUser`: `ProtectHome=yes` would lock a `User=you`
  service out of your own home, and `/var/lib/runic` keeps its previous owner.
  The per-user unit above is the supported shape.

---

## B. Prebuilt binary + per-user service

No root, no package — grab the tarball for your arch from the
[Releases page](https://github.com/quazardous/runic/releases). It is
self-contained: the static musl binary, the per-user systemd unit and a
commented example config all ship inside, so nothing else is needed. The unit
uses systemd's `%h` specifier — all paths are relative to your home, no edits
before copying it in.

(The same per-user service also runs a package-installed or self-built binary —
only `ExecStart` changes. See the
[packaged variant](#packaged-binary-per-user-service) and
[section C](#c-build-from-source).)

### Files

```
runic.tar.gz                                # ships: runic, runic.service, runic.yaml.example
~/.local/bin/runic                          # the binary
~/.config/systemd/user/runic.service        # the unit
~/.config/runic/runic.yaml                  # listen + upstream config
~/.config/runic/creds.env                   # upstream creds (optional, chmod 600)
```

### Install

```bash
# 1. download + extract the musl tarball for your arch
v=vX.Y.Z   # pick the latest from the Releases page
curl -fsSL -o runic.tar.gz \
  "https://github.com/quazardous/runic/releases/download/${v}/runic-${v}-x86_64-unknown-linux-musl.tar.gz"
tar -xzf runic.tar.gz && cd "runic-${v}-x86_64-unknown-linux-musl"

# 2. binary
mkdir -p ~/.local/bin
install -m755 runic ~/.local/bin/

# 3. config — start from the shipped example, then add your upstream(s)
mkdir -p ~/.config/runic
cp runic.yaml.example ~/.config/runic/runic.yaml
"${EDITOR:-vi}" ~/.config/runic/runic.yaml

# 3b. creds — only if your YAML references *_env credentials
cat > ~/.config/runic/creds.env <<'EOF'
RUNIC_UPSTREAM_USER=your-username
RUNIC_UPSTREAM_PASS=your-password
EOF
chmod 600 ~/.config/runic/creds.env

# 4. unit — shipped in the tarball too
mkdir -p ~/.config/systemd/user
cp runic.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now runic

# 5. verify
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

Then follow [section B](#b-prebuilt-binary--per-user-service) from step 2,
using `target/release/runic` (or the `./runic` you copied out) as the binary.
You already have the tarball's two support files in the repo clone:
`packaging/linux/runic.yaml` is the example config and
`packaging/systemd/runic.service` is the user unit.

---

## Smoke test

Whichever path you took — with the default config the SOCKS5 port is
auto-picked by the OS, so first read the real one from the status endpoint
(the admin port is fixed):

```bash
PROXY=$(curl -s http://127.0.0.1:48484/v1/status | grep -o '"listen":"[^"]*"' | cut -d'"' -f4)
curl --socks5 "$PROXY" https://api.ipify.org
```

(If you pinned an explicit port in `listen.addr`, use it directly:
`curl --socks5 127.0.0.1:7878 …`.)

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
| `listen.addr` / `listen.port_range`              | The listener rebinds (in auto-port mode this mints a new port — re-read it from `/v1/status`). If the new bind fails (port in use), the daemon keeps listening on the previous address and logs a warning. A reload that doesn't touch these keys never rebinds. |
| Invalid YAML / parse error                       | Warning logged, previous config stays live. The daemon does not crash.        |

What is **not** hot-reloadable: credentials read from an `EnvironmentFile`
(`creds.env` / `runic.env`) are read once at boot. After editing them,
`systemctl [--user] restart runic` is required.

## Reload check

```bash
# Edit the default upstream host to something that resolves but can't accept the chain
sed -i 's/host: gw.dataimpulse.com/host: example.invalid/' ~/.config/runic/runic.yaml

# Wait < 1s for the watcher to pick it up, then (reusing $PROXY from the smoke test):
curl --socks5 "$PROXY" https://api.ipify.org
# → curl reports a SOCKS5 failure (proves the new upstream is in effect)

# Restore
sed -i 's/host: example.invalid/host: gw.dataimpulse.com/' ~/.config/runic/runic.yaml
```

## Tear down

```bash
# package install (A)
sudo apt remove runic        # or: sudo dnf remove runic

# per-user service (B / C, or the per-user variant of A)
systemctl --user disable --now runic
rm ~/.config/systemd/user/runic.service
systemctl --user daemon-reload
# (config + creds left in ~/.config/runic — remove manually if you really mean it)
```
