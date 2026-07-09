# Changelog

All notable changes to this project will be documented in this file.

This is a curated, human-readable record — **not a commit log**. Each entry
says *what changed and why it matters to a user*, in plain language, not how it
was implemented. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) ("changelogs are for
humans, not machines"), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] - 2026-07-09

### Changed

- **The default SOCKS5 port is now `7878` (was `7777`), and it is a real
  built-in default.** `7777` sits in a range other local tools commonly grab;
  `7878` is quieter — same reasoning as the admin port's earlier move to
  `48484`. Until now the port was only a convention repeated in the shipped
  configs and docs; `listen:` now has a code-level default
  (`127.0.0.1:7878`, no auth) and — like `admin:` — can be omitted entirely.
  Existing installs are unaffected: every shipped config sets `listen.addr`
  explicitly and package upgrades keep the edited `/etc/runic/runic.yaml`; only
  setups recreating a config from the new docs/examples pick up the new port.

- **The shipped default config is fully commented out — on every OS.** Each
  key ships commented, showing its built-in default; uncomment only what you
  change. An empty or comment-only config file is now valid and boots runic
  entirely on defaults (loopback listeners, empty pool, no filter). This covers
  the packaged `/etc/runic/runic.yaml`, the Linux **and macOS** tarballs'
  `runic.yaml.example` (macOS previously shipped no config at all), the Windows
  ZIP/MSI example, and the config the tray writes on first "Open config". The
  Windows example also drops its divergent `1080` listen port in favour of the
  product-wide `7878` default.

## [0.7.1] - 2026-07-08

### Documentation

- **The silo filter floor is confirmed hot-reloadable.** Editing the `filter:`
  block of the YAML config applies to every silo's *next* CONNECT — already-warm
  silos included, no restart — like the rest of the cold layer. This was always
  the design; it is now stated in `docs/install/filtering.md` and pinned by an
  end-to-end test (real config file, real watcher, warm silo), including the
  layering guarantees: a runtime admin-API filter never floors a silo and
  survives a file reload untouched. In-flight tunnels keep the verdict they
  connected with. No behaviour change.

## [0.7.0] - 2026-07-08

### Added

- **The Linux tarball is now self-contained for a no-root install.** Each
  Linux release tarball ships the per-user systemd unit (`runic.service`) and
  a commented example config (`runic.yaml.example`) next to the binary — the
  per-user install path no longer needs anything from a repo clone. The
  packaging dry-run verifies the tarball layout on every relevant change.

### Fixed

- **The per-user systemd unit starts without a `creds.env`.** The credentials
  file is now optional in the unit: a config that references no `*_env` at all
  (for example one driven entirely over the admin API) previously failed to
  start the service. The unit also gains `NoNewPrivileges` as a hardening
  baseline.

## [0.6.0] - 2026-07-07

### Added

- **`GET /v1/status` now reports the machine's `hostname`.** Detected once from
  the OS, `"unknown"` as a fallback — so a consumer (or a human) juggling
  several runic instances can tell the boxes apart from the status API alone.
  The status page shows it in the header next to the version.

## [0.5.2] - 2026-07-03

### Fixed

- **MSI installer builds again in the workspace layout.** `cargo wix` refuses
  to run without an explicit package name inside a Cargo workspace; the MSI
  script now passes `-p runic-tray`. Second and last of the packaging fallouts
  from the workspace unification (the portable-ZIP path was fixed in 0.5.1,
  which itself never published because of this one).

## [0.5.1] - 2026-07-03

### Fixed

- **Windows packages build again.** The portable-ZIP and MSI packaging scripts
  still looked for the tray binary in `runic-tray/target/`, but since the Cargo
  workspace unification everything builds into the workspace-root `target/` —
  so the release pipeline's Windows job failed before publishing anything. The
  scripts now ask `cargo metadata` for the real target directory. (The `v0.5.0`
  tag hit this and never produced a release; 0.5.1 is the first published build
  of the 0.5 line.)

## [0.5.0] - 2026-07-02

### Changed

- **Silo domain filters now compose on top of a static file floor, and the
  filter API surface is strictly per-silo.** The `filter:` block in the config
  file acts as a baseline that every silo composes over: a silo's own rules are
  evaluated first, then fall through to the file floor (a silo can add or
  override, instead of the previous "silo replaces the global entirely"). Only
  the **static file** filter floors a silo — the admin-API runtime/permanent
  filter (`PUT /v1/filter` *without* a `Bearer` token) now governs **non-silo
  sessions only** and can no longer affect any silo. This closes a
  compartmentalization gap: a filter change pushed without a client's token must
  not pierce silo isolation, and a declarative file baseline (immutable at
  runtime) doesn't have that risk. A silo still sets its **own** filter with
  `PUT /v1/filter` + `Bearer`, stored in its encrypted blob. The retired
  `enforce_in_silo` flag is superseded by this (its hard-floor job is now the
  safe, file-sourced floor); old configs carrying it load fine (the key is
  ignored). `GET /v1/status` swaps the `filter.enforce_in_silo` field for
  `filter.silo_floor_rules`. See [`docs/install/filtering.md`](docs/install/filtering.md).

## [0.4.0] - 2026-07-01

### Added

- **Domain filtering at the CONNECT layer.** runic can now allow or deny a
  session by its target host, before opening the tunnel — an ordered
  allow/deny rule list (first-match-wins, like a firewall chain) with a default
  action. The same engine expresses a blocklist (`default: allow` + `deny`
  rules) or a strict allowlist (`default: deny` + `allow` rules); patterns match
  an exact host, a `*.` subdomain wildcard, and an optional `:port`. This cuts
  bandwidth for scraping (block image CDNs, ad/tracker hosts) and locks a proxy
  to known destinations — all **without any TLS interception**: runic still only
  ever sees the target host, never the encrypted payload. A blocked target is
  refused with the SOCKS5 "connection not allowed by ruleset" reply, and no
  upstream quota is spent on it. Configure it in the YAML (`filter:` section),
  live via the admin API (`GET/PUT/DELETE /v1/filter`, firewalld-style
  runtime/permanent), and — in silo mode — per client inside each encrypted
  variation config. The status page shows the filter posture and a cumulative
  count of blocked CONNECTs. See [`docs/install/filtering.md`](docs/install/filtering.md).

## [0.3.0] - 2026-06-28

### Added

- **A live status page.** The admin API now serves a small self-contained web
  page (open the admin port in a browser) showing what runic is doing right now:
  the active route and whether any traffic is leaving un-proxied, the live and
  cumulative session counts, the runtime upstream pool, and — in silo mode — each
  client config with its current connections and request count. The same data is
  available as JSON at `GET /v1/status`. It is built from public metadata and
  in-memory counters only: no client key is ever needed, and nothing encrypted is
  read to render it.

### Fixed

- **`runic.exe` now works out of the box on Windows.** The CLI's default config
  path and the persisted snapshot location are now platform-aware: they resolve
  under `%APPDATA%\runic\` on Windows instead of Unix-only paths (which left a
  bare `runic.exe` unable to find its config, and could drop the snapshot — with
  cleartext credentials — into the current directory). Unix behaviour is
  unchanged (`/etc/runic/runic.yaml` for the config, `$XDG_CONFIG_HOME` for the
  snapshot). The resolver is shared with the Windows tray so the two never
  diverge.

### Changed

- **The admin API now defaults to port `48484` (was `7778`).** The old default
  sat right next to the SOCKS5 port `7777`, a commonly-used range; `48484` is a
  quieter, well-known default so a client can find runic's control plane without
  being told the port. The port is still fully configurable via `admin.addr`;
  only setups that relied on the default need to update (the project has no
  published release yet, so nothing in the wild is affected).

- **Direct egress is now allowed by default — but never implicit.** Declaring a
  `kind: "direct"` upstream no longer requires the `RUNIC_ALLOW_DIRECT=1`
  opt-in. The guard against accidentally leaving the proxy is the rule that an
  upstream is always explicit: traffic only goes out direct if you deliberately
  declared a `direct` upstream *and* routed to it — an empty pool or empty silo
  is a dead end (CONNECT refused), never a silent passthrough. For prod
  hardening, `RUNIC_ALLOW_DIRECT=0` forbids direct outright. (Previously direct
  was fail-closed and required `=1`.)

### Documentation

- **Spelled out the "an upstream is mandatory" rule.** The admin-API and silo
  docs now state plainly that runic never egresses direct by default: a session
  that resolves to no upstream is refused, not silently sent out un-proxied (an
  empty pool is a valid *boot* state for an API-driven runic, but not a
  passthrough). The `kind: "direct"` contract — allowed by default, always
  explicit, `RUNIC_ALLOW_DIRECT=0` to forbid — and its canonical request body
  (`kind` alone) are now documented.

- **Linux install guide now covers the easy paths.** The Linux doc previously
  only walked through a from-source build; it now leads with installing the
  `.deb`/`.rpm` package (system service, hardened `DynamicUser`) and the
  prebuilt static binary, keeping the from-source per-user systemd unit as the
  no-root alternative — and points at the live status page once running.

## [0.2.0] - 2026-06-02

Pre-1.0; the first tagged cut. So far runic can:

### Added

- **Tunnel local traffic through a credentialed upstream.** Point any
  SOCKS5-aware tool at runic and it forwards out through an upstream HTTP
  `CONNECT` provider — the provider's credentials stay on the runic side
  instead of being copied into every client.
- **Pick up config changes without a restart.** Edit the YAML and runic applies
  it on the fly: in-flight sessions keep the settings they started with, new
  ones use the latest.
- **Route different sessions through different providers.** Declare a pool of
  named upstreams; a client chooses one per session via the SOCKS5 username
  (`provider=…`), with a sensible default when it asks for nothing. One proxy,
  many providers.
- **Reconfigure the running proxy without touching files.** A local admin API
  lets an operator add, change or remove providers while runic is running, and
  optionally make the change stick across restarts — with a clear view of
  what's live versus saved. The model mirrors a firewall's "runtime vs
  permanent" split, so it feels familiar.
- **Start with nothing configured and fill it in later.** runic now boots even
  with an empty pool (or none named `default`) instead of refusing to start, so
  it can be brought up bare and configured entirely over the admin API. A
  session that arrives before any matching route exists is declined cleanly
  rather than bringing the proxy down.
- **Exercise the proxy locally without credentials.** A new `direct` upstream
  kind makes runic connect straight to the requested target instead of relaying
  through a provider — useful for dev/CI smoke tests that shouldn't spend real
  proxy quota. See the Security note: it is off unless explicitly enabled.
- **Switch the active provider live, by name.** The admin API can point the
  default (no-provider) route at any upstream in the pool
  (`PUT /v1/route/default`) without re-sending its credentials, and clear it to
  fall back to the `default` entry. An operator flips which provider new
  sessions use on the fly — clients keep talking to the same local port and
  never see the change.
- **Keep config encrypted on an untrusted host (opt-in "silo" mode).** A client
  opens the silo and gets a token — its key, which it holds off the box. runic
  keeps only an encrypted blob plus a one-way fingerprint on disk, so a seized,
  powered-off machine reveals neither the providers nor their credentials. Each
  token has its own isolated config; configs are dropped from memory when idle
  and expire on a TTL. A client binds either by sending its token as the SOCKS5
  password, or — for clients that can't authenticate to SOCKS5, like browsers —
  by asking for a dedicated loopback port. See `docs/install/silo.md`.
- **Reuse the proxy as a library.** The core is published as a library with the
  command-line tool layered on top, so another front-end (for example a desktop
  tray app) can drive the same engine.
- **Deploy it easily.** Runs as a small container image or as a per-user Linux
  background service.

### Security

- The proxy and the admin interface listen on loopback only and assume that
  whoever can reach them is trusted — keep them off public interfaces. Upstream
  credentials come from the environment; if you persist runtime changes, the
  saved file holds those credentials in clear and is written owner-only.
- In silo mode the per-client encryption key never touches the box's disk — it
  lives off-box and is held in memory only while a client is active. On disk
  runic keeps only the ciphertext and a one-way `SHA256(token)` index (the AEAD
  key is a *separate* HKDF derivation, so the on-disk index can't decrypt). A
  lost token means a fresh config, by design — there is no key recovery.
- The `direct` upstream kind is **not** proxied — the target sees the machine's
  own IP, with no provider in between. It is fail-closed: runic refuses to start
  (and the admin API rejects the change) unless `RUNIC_ALLOW_DIRECT=1` is set,
  and it logs a warning for as long as a direct upstream is active. This keeps
  it firmly a dev/CI tool that can't reach production by an accidental config
  copy-paste.
