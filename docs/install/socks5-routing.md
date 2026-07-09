# Multi-provider routing via SOCKS5 username

From v0.7 onwards, runic lets clients pick an upstream from the YAML pool by
passing routing intent inside the SOCKS5 username during the handshake. The
configuration stays the same — declare a pool of named `upstreams`, and the
client tells runic which entry to use per session.

Think of it as the inverse of HAProxy: HAProxy receives external traffic and
load-balances to internal backends; runic receives local traffic and routes
to external providers based on what the client asks for.

## Username format

The SOCKS5 username is a list of `key=value` pairs separated by `;`. The keys
supported in v0.7:

- `provider=<name>` — name of the upstream entry to use. Must match a key in
  the `upstreams` map of the YAML config. Falls back to `default` if missing
  from the pool, the field is absent, or the username is empty.
- `sessid=<token>` — reserved for the v0.7.1 sticky-session feature, captured
  by the parser but not yet acted on.

Unknown keys are ignored silently to keep the format forward-compatible
(future versions can add keys without breaking older runic deployments).

## Client compatibility

Username routing needs a SOCKS5 client that actually **sends a username** — i.e.
one that does RFC 1929 username/password auth. That covers curl, most SDK HTTP
clients, and CLI tools.

It does **not** cover clients that only speak no-auth SOCKS5 — notably
Chromium-based browsers, which never offer RFC 1929 auth, so there is no
username field to carry routing intent. For those, route **out of band**
instead of in the handshake:

- set the active default route via the admin API
  (`PUT /v1/route/default {"upstream":"<name>"}`, see
  [`admin-api.md`](admin-api.md)), or
- give the client a dedicated loopback port bound to a specific config (see the
  `none` binding mode in [`silo.md`](silo.md)).

The SOCKS5 password field is **ignored** in v0.7 — it's reserved for v0.8+
provider-level secret overrides. Send an empty string.

## Examples

### curl

```bash
# Route via the upstream named `us-residential` (must exist in YAML pool)
curl --socks5 "provider=us-residential:@127.0.0.1:7878" https://api.ipify.org

# No routing intent — runic uses the `default` upstream
curl --socks5 127.0.0.1:7878 https://api.ipify.org

# Reserve a sticky-session id (no-op today, ready for v0.7.1)
curl --socks5 "provider=fr;sessid=abc123:@127.0.0.1:7878" https://api.ipify.org
```

### Chrome extension (Manifest v3)

```js
chrome.proxy.settings.set({
  scope: 'regular',
  value: {
    mode: 'fixed_servers',
    rules: {
      singleProxy: {
        scheme: 'socks5',
        host: '127.0.0.1',
        port: 7878,
        username: `provider=${providerName};sessid=${sessionId}`,
        password: '',
      },
    },
  },
});
```

The extension provides the routing intent on a per-tab or global basis;
runic does not need to know about the extension itself. Credentials for
the upstream (e.g. DataImpulse) stay in runic's YAML and never reach the
extension.

## YAML pool example

```yaml
listen:
  addr: "0.0.0.0:7878"
  auth: none

upstreams:
  default:                          # required — used when no provider is specified
    kind: http_connect
    host: gw.dataimpulse.com
    port: 823
    auth:
      username_env: DATAIMPULSE_LOGIN
      password_env: DATAIMPULSE_PASSWORD

  us-residential:                   # optional — addressed via `provider=us-residential`
    kind: http_connect
    host: gw-us.example.com
    port: 823
    auth:
      username_env: USRES_LOGIN
      password_env: USRES_PASSWORD
```

## Backwards compatibility

Clients that connect without offering METHOD 0x02 (user/pass) — i.e. classic
no-auth SOCKS5 clients — still work. They get routed through the `default`
upstream as in v0.5. Adding v0.7 routing on the runic side does not break
existing consumers.

## What is not in v0.7

These are tracked in follow-up tickets:

- **Sticky-session routing** (`sessid=token` → cached upstream pick with a TTL)
  is parsed but not enforced. A first session pinned to a given upstream may be
  re-routed to a different one on the next call.
- **Target-host based routing** (route `*.us-target.com` through one provider
  by URL alone, without the client opting in) is a separate roadmap item.
- **Fail-rate aware routing** (mark a provider degraded after N consecutive
  errors) is roadmap as well.
- **Password as a credential override** for the chosen upstream is reserved
  for v0.8+.
