# Changelog

All notable changes to this project will be documented in this file.

This is a curated, human-readable record — **not a commit log**. Each entry
says *what changed and why it matters to a user*, in plain language, not how it
was implemented. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) ("changelogs are for
humans, not machines"), and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Pre-1.0 development; nothing tagged yet. So far runic can:

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
- The `direct` upstream kind is **not** proxied — the target sees the machine's
  own IP, with no provider in between. It is fail-closed: runic refuses to start
  (and the admin API rejects the change) unless `RUNIC_ALLOW_DIRECT=1` is set,
  and it logs a warning for as long as a direct upstream is active. This keeps
  it firmly a dev/CI tool that can't reach production by an accidental config
  copy-paste.
