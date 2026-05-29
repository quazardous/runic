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
