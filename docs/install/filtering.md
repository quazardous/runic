# Domain filtering

runic can **allow or deny the target host at CONNECT time**, before it opens the
tunnel. This trims bandwidth for scraping (skip image CDNs, ad/tracker domains)
and lets you lock a proxy to a fixed set of destinations — without any TLS
interception.

## What it can and can't do

runic is a SOCKS5 → HTTP `CONNECT` tunnel: on HTTPS it sees the target
`host:port` and then an **opaque encrypted stream**. So the filter works on the
**hostname**, not on URLs or content:

- ✅ Refuse a tunnel to a whole host — `img.example.com`, `*.doubleclick.net`,
  `google-analytics.com`. Sites usually serve heavy assets from separate
  subdomains/CDNs, so a host deny-list cuts real bandwidth.
- ❌ Distinguish an image request from an HTML one on the **same** host, or
  rewrite a response to a stub image. That requires decrypting TLS (a MITM
  proxy), which breaks runic's "never reads plaintext" model — do it in the
  client instead (a headless browser can `abort()` requests by resource type),
  or with a dedicated MITM tool.

Prior art: the ordered, first-match-wins rule model follows
[firewalld](https://firewalld.org/) rich rules and iptables chains; blocking by
hostname follows the spirit of adblock hostlists (runic ships **no** bundled
list — you declare your own rules).

## The rule model

An **ordered list of rules**, each an `allow` or a `deny` bound to a host
pattern. The **first** rule whose pattern matches the target decides; if none
matches, the `default` action applies. One engine gives you both a blocklist and
a strict allowlist:

```yaml
filter:
  default: allow          # allow | deny
  rules:
    - deny:  "*.doubleclick.net"
    - deny:  "google-analytics.com"
    - deny:  "*.cloudfront.net"
    - allow: "cdn.mysite.com"   # exception: listed first, so it wins over a broader deny
```

- **Blocklist** — `default: allow` + `deny` rules (the example above): everything
  is allowed except what you block.
- **Strict allowlist** — `default: deny` + `allow` rules: nothing is allowed
  except the hosts you list. Locks a proxy to a known set of destinations.

A denied CONNECT is refused with SOCKS5 reply **`0x02`** ("connection not allowed
by ruleset") — a clean rejection, and no proxy quota is spent on the blocked host.

### Pattern matching

| Pattern              | Matches                                              |
|----------------------|------------------------------------------------------|
| `example.com`        | the apex `example.com` only                          |
| `*.example.com`      | any subdomain (`a.example.com`, `x.y.example.com`) — **not** the apex |
| `example.com:443`    | `example.com`, but only on port 443                  |
| `*.cdn.net:443`      | any `*.cdn.net` subdomain on port 443                |
| `203.0.113.4`        | that IPv4 literal, exactly                           |

Matching is ASCII-case-insensitive. A `:port` suffix restricts the rule to that
port; without it the rule matches on any port. Bracketless IPv6 literals are not
supported as patterns (the `:` reads as a port separator) — filter IPv6 targets
by an enclosing hostname.

## Managing it at runtime — the admin API

The filter is firewalld-style: a **runtime** override lives in RAM, a
**permanent** one is persisted in the snapshot. Same loopback admin port as the
rest of the control plane.

```sh
# Read the effective filter
curl -s http://127.0.0.1:48484/v1/filter

# Replace the whole filter at runtime (RAM only)
curl -s -X PUT http://127.0.0.1:48484/v1/filter \
  -d '{"default":"allow","rules":[{"deny":"*.doubleclick.net"}]}'

# Same, but persist it across restarts
curl -s -X PUT 'http://127.0.0.1:48484/v1/filter?permanent=true' \
  -d '{"default":"deny","rules":[{"allow":"api.target.com"}]}'

# Clear the runtime override (falls back to permanent, then the YAML)
curl -s -X DELETE http://127.0.0.1:48484/v1/filter
# ...or clear the permanent one too
curl -s -X DELETE 'http://127.0.0.1:48484/v1/filter?permanent=true'
```

`PUT` replaces the whole ruleset (atomic, like editing a firewalld zone). The
effective filter is `runtime ▷ permanent ▷ cold YAML`, mirroring the upstream
layers. Changes apply live: in-flight sessions keep their decision, new sessions
use the latest rules.

The status page (open the admin port in a browser) shows the filter posture —
`blocklist · N rules` / `allowlist · N rules` / `off` — and a cumulative
**Filtered** counter of refused CONNECTs. The same data is in `GET /v1/status`
(`filter` object + `filtered_total`).

## In silo mode

Each silo variation carries **its own filter**, stored inside its encrypted blob
(isolated per client, invisible off-box, decrypted in memory only while the silo
is warm). A silo client sets it with a `Bearer` token, exactly like it sets its
upstreams:

```sh
curl -s -X PUT http://127.0.0.1:48484/v1/filter \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"default":"allow","rules":[{"deny":"*.ads.example"}]}'
```

**How the filters compose** (see `filter::decide_session`):

- A **non-silo** session obeys the instance filter (the file `filter:` plus any
  admin-API runtime/permanent overrides).
- A **silo** session composes its own rules **on top of a static file floor**:
  1. the silo's own rules, first-match — they add to / override the floor;
  2. a silo that declares `default: deny` is a **closed allowlist** (its own
     allows only; the floor cannot re-open it);
  3. otherwise it falls through to the file floor — the `filter:` block of the
     cold YAML — then that floor's `default`. A silo that sets no filter of its
     own is governed entirely by the file floor.

Only the **static file** `filter:` floors a silo. This is deliberate: the
admin-API filter surface that floors silos must be **declarative and immutable at
runtime**, so a filter change pushed *without* a client's `Bearer` token can
never pierce silo isolation. Concretely:

- **`PUT /v1/filter` with a `Bearer`** → writes that silo's own filter (encrypted
  blob). Per-client, sovereign.
- **`PUT /v1/filter` without a `Bearer`** → the instance filter for **non-silo**
  sessions only. It does **not** reach any silo (no token-less cross-silo lever).
- The **file floor** is set in the deployed `filter:` YAML — visible, auditable,
  changed only by redeploy/hot-reload, never by a live API call. Like the rest
  of the cold layer it **follows the file at runtime**: edit the `filter:`
  block and the new floor applies to the next CONNECT of every silo — already
  warm ones included — without a restart. In-flight tunnels keep their
  connect-time verdict, and a live runtime/permanent API filter is neither
  clobbered by the reload nor promoted into the floor.

```yaml
# The file floor: a baseline every silo composes on top of (each silo adds/
# overrides its own rules; a silo can re-allow a floor-denied host unless it
# runs its own strict allowlist).
filter:
  default: allow
  rules:
    - deny: "*.doubleclick.net"
    - deny: "*.ads.example"
```

The status page shows the floor as `silo floor · N` alongside the instance
filter posture; `GET /v1/status` carries `filter.silo_floor_rules`.
