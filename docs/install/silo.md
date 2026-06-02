# Config silo — encrypted, per-client config

The **config silo** lets runic hold its routing config as **encrypted-at-rest**
snapshots whose decryption keys **never touch the machine's disk**. It is built
for an untrusted host: if the box is seized while powered off, the persisted
state is useless to whoever has it.

Silo mode is **opt-in**. Without it, runic behaves exactly as documented
elsewhere (plain YAML + cleartext snapshot). This guide covers what a silo is,
how to enable it, and how a client uses it.

## Why

In some deployments runic runs on a host you don't fully control (e.g. a remote
residential box). You still want it configured live over the
[admin API](admin-api.md), but you don't want its on-disk state to reveal your
upstream providers or credentials if the disk is imaged.

The silo gives that: each client's config lives in its own **encrypted blob**;
the key is a **token** the client holds off-box; runic only ever keeps a one-way
fingerprint of the token plus the ciphertext on disk.

## Enabling it (cold YAML)

```yaml
listen:
  addr: "0.0.0.0:7777"
  auth: none

silo:
  enabled: true
  ttl_days: 7          # idle variations are purged after this many days
  auth: rfc1929        # client→config binding mode: rfc1929 (default) | none
```

When `silo.enabled` is true, runic keeps a `runic.silo/` directory next to its
snapshot:

- `index.json` — **cleartext**, but holds **no secrets**: per-entry it has only
  the token's one-way hash, timestamps, and the (public) encryption nonce. This
  is what lets the idle-purge run without any token.
- one **encrypted blob** per client config.

## Model

- A silo has a shared, durable **cold base** (the `upstreams` in your YAML, if
  any) plus **N independent encrypted configs**, one per token.
- A **token** is a 256-bit secret runic mints and returns **once**, then
  forgets. It is the only thing that decrypts that config. Hold it off-box.
- On disk runic stores **`SHA256(token)`** (an index/verifier — it cannot be
  reversed to the token and cannot decrypt anything) and the **ciphertext**
  (sealed with a *separate* key derived from the token via HKDF). It never
  stores a raw token.
- Decrypted config is held in RAM only while in use, and dropped after an idle
  timeout (the keep-alive window).
- Configs that go untouched longer than `ttl_days` are garbage-collected.

> The notion of separate "configs/variations" is an internal detail. From the
> client's side there is just: *open the silo → get a token*, then *present the
> token*. A client may hold several tokens if it wants several independent
> configs.

## Opening a silo (the API)

A single verb, on the loopback admin API:

| Request | Result |
| ------- | ------ |
| `POST /v1/silo/open` (no auth) | mint a fresh token → `{ "token": "<token>" }` (and, in `none` mode, a `"port"`) |
| `POST /v1/silo/open` + `Authorization: Bearer <token>` | "show your token": confirm it opens a live config → `{ "ok": true }` (or `{ "port": <n> }` in `none` mode) |
| same, but token unknown / purged | **`404 { "code": "silo_token_unknown" }`** — never a silent re-mint |

The `silo_token_unknown` response is the **deterministic lost/expired signal**:
on it, the client re-opens **without** auth to get a fresh token, then re-pushes
its config (see [Lifecycle](#lifecycle)).

```bash
# First run: get a token, keep it safe (runic won't show it again).
curl -X POST http://127.0.0.1:7778/v1/silo/open
# → {"token":"q1w2...e3r4"}
```

## Configuring your silo

Use the existing upstream routes, scoped to your silo with the token:

```bash
curl -X POST http://127.0.0.1:7778/v1/upstreams/dataimpulse \
  -H 'authorization: Bearer q1w2...e3r4' \
  -d '{"kind":"http_connect","host":"gw.example","port":823,
       "auth":{"username":"u","password":"p"}}'
```

In silo mode a config **is** its encrypted on-disk blob, so a push is
**write-through persistent** — there is no separate "permanent" step, and
`?permanent=true` is ignored. On a later run, opening with your token returns the
config already populated, so you can **skip re-pushing** (check `GET /v1/config`
with your `Bearer` token first).

## Binding modes

How a *data* connection (the SOCKS5 traffic, not the admin API) is matched to a
config depends on `silo.auth`:

### `rfc1929` (default)

The client authenticates to the SOCKS5 port with its **token as the password**
(RFC 1929 username/password). runic resolves the config per connection and routes
through it. Use this for SOCKS5 clients that support auth (curl `--proxy-user`,
most SDK HTTP clients).

```
socks5h://<anything>:<token>@127.0.0.1:7777
```

### `none` (loopback port binding)

For clients that **don't** implement SOCKS5 auth (notably Chromium-based
browsers — they only offer no-auth SOCKS5). The client opens its silo over the
admin API and gets back a **dedicated loopback port**:

```bash
curl -X POST http://127.0.0.1:7778/v1/silo/open \
  -H 'authorization: Bearer q1w2...e3r4'
# → {"port":41987}
```

then points the browser at `socks5://127.0.0.1:41987` (no auth). runic serves
that port from the client's config. Idle ports are torn down (the same
keep-alive that evicts the decrypted config from RAM).

## Lifecycle

- **Warm start** — the client still has its token: `POST /v1/silo/open` with
  `Bearer` returns its config (and, in `none` mode, a port). It can skip
  re-pushing.
- **Lost or expired token** — `open` with that `Bearer` returns
  `404 silo_token_unknown`. The client re-opens **without** auth (new token) and
  re-pushes its config. A silo's cold base is never thrown away; only the
  idle/lost per-client configs are.

## Security model

- **A seized, powered-off box** yields `index.json` (hashes + timestamps +
  nonces) and the ciphertext blobs — **no key, no readable config**. The keys
  live off-box, in RAM only while a client is active.
- The token is the whole secret. Treat it like a credential; if it leaks, that
  one config is exposed (rotate by opening a fresh silo and re-pushing).
- The SOCKS5 and admin surfaces remain **loopback-only**; the silo adds
  encryption-at-rest and per-client isolation, not network exposure controls.

## Not in scope

- Key escrow / recovery — a lost token means a fresh config, by design.
- Sharing one config across tokens — each token is its own config.
