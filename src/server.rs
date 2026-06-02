use std::io;
use std::net::Ipv6Addr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Mutex};
use tracing::{debug, error, info, warn};

use crate::config::{Config, ListenAuth, Upstream, UpstreamKind};
use crate::silo::VariationCache;
use crate::upstream;

const SOCKS5_VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_USERPASS: u8 = 0x02;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const USERPASS_VERSION: u8 = 0x01;
const USERPASS_STATUS_SUCCESS: u8 = 0x00;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REPLY_SUCCEEDED: u8 = 0x00;
const REPLY_GENERAL_FAILURE: u8 = 0x01;
const REPLY_CMD_NOT_SUPPORTED: u8 = 0x07;
const REPLY_ATYP_NOT_SUPPORTED: u8 = 0x08;

pub async fn run(
    mut cfg_rx: watch::Receiver<Arc<Config>>,
    silo: Option<Arc<Mutex<VariationCache>>>,
) -> Result<()> {
    let initial = cfg_rx.borrow().clone();
    let mut current_addr = initial.listen.addr;
    let mut listener = TcpListener::bind(current_addr)
        .await
        .with_context(|| format!("bind SOCKS5 listener on {current_addr}"))?;
    info!(
        addr = %current_addr,
        upstream_default = %default_label(&initial),
        pool_size = initial.upstreams.len(),
        "runic listening"
    );
    warn_on_direct(&initial);

    loop {
        tokio::select! {
            res = listener.accept() => {
                let (client, peer) = match res {
                    Ok(v) => v,
                    Err(e) => {
                        error!(?e, "accept failed");
                        continue;
                    }
                };
                let session_cfg = cfg_rx.borrow().clone();
                let silo = silo.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve(client, &session_cfg, &silo).await {
                        warn!(%peer, error = %e, "session ended with error");
                    }
                });
            }
            changed = cfg_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let new_cfg = cfg_rx.borrow().clone();
                warn_on_direct(&new_cfg);
                let new_addr = new_cfg.listen.addr;
                if new_addr != current_addr {
                    info!(old = %current_addr, new = %new_addr, "listen addr changed; attempting rebind");
                    match TcpListener::bind(new_addr).await {
                        Ok(new_l) => {
                            current_addr = new_addr;
                            listener = new_l;
                            info!(addr = %current_addr, "rebound");
                        }
                        Err(e) => {
                            warn!(target_addr = %new_addr, error = %e, "rebind failed; staying on previous addr");
                        }
                    }
                } else {
                    debug!("config changed (upstream / auth); future sessions will use new values");
                }
            }
        }
    }
}

/// Human label for the default route in the boot log: `host:port`, `direct`,
/// or `<none>` when the pool has no `default` entry (API-driven / empty config).
fn default_label(cfg: &Config) -> String {
    match cfg.default_upstream() {
        None => "<none>".to_string(),
        Some(u) if u.kind == UpstreamKind::Direct => "direct".to_string(),
        Some(u) => format!("{}:{}", u.host, u.port),
    }
}

/// Loudly flag any active `direct` upstream — it is NOT proxied (the target is
/// reached over a plain connection with the local IP exposed), only meant for
/// dev/CI. Emitted at boot and on every config change.
fn warn_on_direct(cfg: &Config) {
    for (name, up) in &cfg.upstreams {
        if up.kind == UpstreamKind::Direct {
            warn!(
                upstream = %name,
                "direct upstream active — traffic NOT proxied (local IP exposed), dev/CI only"
            );
        }
    }
}

/// Credentials negotiated during the SOCKS5 handshake. The username carries the
/// routing intent (`provider=name;sessid=xyz`); the password carries the **silo
/// token** in silo `rfc1929` mode (otherwise unused).
#[derive(Debug, Clone)]
pub struct UserPass {
    pub username: String,
    pub password: String,
}

async fn serve(
    mut client: TcpStream,
    cfg: &Config,
    silo: &Option<Arc<Mutex<VariationCache>>>,
) -> Result<()> {
    let creds = negotiate_method(&mut client, &cfg.listen.auth).await?;
    let (host, port) = parse_request(&mut client).await?;
    debug!(
        %host,
        port,
        socks5_user = creds.as_ref().map(|c| c.username.as_str()).unwrap_or(""),
        "client requested CONNECT"
    );

    // Resolve the upstream for this session (owned). In silo mode the SOCKS5
    // password carries the silo token; otherwise routing is by the username.
    let chosen_upstream = match resolve_route(cfg, silo, creds.as_ref()).await {
        Some(u) => u,
        None => {
            // No matching route: empty pool, unknown provider/silo-token, or no
            // `default`. Fail the session cleanly rather than panicking — the
            // normal state for an API-driven runic before a route is pushed.
            warn!(
                %host,
                port,
                socks5_user = creds.as_ref().map(|c| c.username.as_str()).unwrap_or(""),
                "no route for session — refusing CONNECT"
            );
            reply(&mut client, REPLY_GENERAL_FAILURE).await?;
            return Ok(());
        }
    };

    let mut upstream_stream = match upstream::connect(&chosen_upstream, &host, port).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%host, port, error = %e, "upstream connect failed");
            reply(&mut client, REPLY_GENERAL_FAILURE).await?;
            return Err(e);
        }
    };

    reply(&mut client, REPLY_SUCCEEDED).await?;

    match copy_bidirectional(&mut client, &mut upstream_stream).await {
        Ok((c_to_u, u_to_c)) => debug!(%host, port, c_to_u, u_to_c, "session closed"),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
            ) => {}
        Err(e) => warn!(%host, port, ?e, "pump error"),
    }
    Ok(())
}

/// Resolve this session's upstream to an owned value, or `None` for no route.
///
/// - **non-silo**: pick from the live pool by the SOCKS5 username (`provider=…`).
/// - **silo (rfc1929)**: the SOCKS5 **password** is the silo token. Open its
///   variation and route over the **cold pool ∪ that variation's pool** (the
///   variation wins on a name clash). An absent or unknown token ⇒ `None`
///   (`silo_token_unknown`, surfaced to the client as a clean SOCKS5 failure).
async fn resolve_route(
    cfg: &Config,
    silo: &Option<Arc<Mutex<VariationCache>>>,
    creds: Option<&UserPass>,
) -> Option<Upstream> {
    let user = creds.map(|c| c.username.as_str());
    match silo {
        None => crate::routing::pick_upstream(cfg, user).cloned(),
        Some(cache) => {
            let token = creds
                .map(|c| c.password.as_str())
                .filter(|p| !p.is_empty())?;
            let data = cache.lock().await.access(token, unix_now()).ok()?;
            // Session pool = cold base overlaid with this variation's config.
            let mut pool = cfg.upstreams.clone();
            pool.extend(data.upstreams);
            let session = Config {
                listen: cfg.listen.clone(),
                upstreams: pool,
                active_route: cfg.active_route.clone(),
            };
            crate::routing::pick_upstream(&session, user).cloned()
        }
    }
}

/// Current unix time in seconds (the silo clock).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn negotiate_method<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut S,
    _auth: &ListenAuth,
) -> Result<Option<UserPass>> {
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr).await.context("read greeting")?;
    if hdr[0] != SOCKS5_VERSION {
        bail!("unsupported SOCKS version 0x{:02x}", hdr[0]);
    }
    let nmethods = hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    client
        .read_exact(&mut methods)
        .await
        .context("read methods")?;

    // V0.7 preference: METHOD_USERPASS (0x02) > METHOD_NO_AUTH (0x00). When the
    // client offers user/pass we always take it — the username carries the
    // routing intent (`provider=...;sessid=...`) the routing layer needs.
    // Listen-side auth is still loopback-only; the configured `ListenAuth` field
    // is informational for V0.7 and reserved for future modes.
    let chosen = if methods.contains(&METHOD_USERPASS) {
        METHOD_USERPASS
    } else if methods.contains(&METHOD_NO_AUTH) {
        METHOD_NO_AUTH
    } else {
        METHOD_NO_ACCEPTABLE
    };

    client
        .write_all(&[SOCKS5_VERSION, chosen])
        .await
        .context("write method choice")?;

    match chosen {
        METHOD_NO_AUTH => Ok(None),
        METHOD_USERPASS => Ok(Some(parse_userpass_auth(client).await?)),
        METHOD_NO_ACCEPTABLE => {
            bail!(
                "client offered no acceptable auth method (offered: {:?})",
                methods
            )
        }
        _ => unreachable!("chosen byte set above"),
    }
}

/// Drive the SOCKS5 username/password sub-negotiation per RFC 1929. Always
/// replies success (status 0x00) — we accept any creds and let the routing
/// layer decide what to do with the username; password is ignored in V0.7.
async fn parse_userpass_auth<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut S,
) -> Result<UserPass> {
    let mut hdr = [0u8; 2];
    client
        .read_exact(&mut hdr)
        .await
        .context("read userpass greeting")?;
    if hdr[0] != USERPASS_VERSION {
        bail!("userpass auth: bad version 0x{:02x}", hdr[0]);
    }
    let ulen = hdr[1] as usize;
    let mut username = vec![0u8; ulen];
    client
        .read_exact(&mut username)
        .await
        .context("read username")?;
    let mut plen_buf = [0u8; 1];
    client
        .read_exact(&mut plen_buf)
        .await
        .context("read plen")?;
    let plen = plen_buf[0] as usize;
    let mut password = vec![0u8; plen];
    client
        .read_exact(&mut password)
        .await
        .context("read password")?;

    // Reply success — we never reject on creds shape; that's the routing layer's
    // call (and even there, unrecognised provider just falls back to default).
    client
        .write_all(&[USERPASS_VERSION, USERPASS_STATUS_SUCCESS])
        .await
        .context("write userpass status")?;

    Ok(UserPass {
        username: String::from_utf8_lossy(&username).into_owned(),
        password: String::from_utf8_lossy(&password).into_owned(),
    })
}

async fn parse_request<S: AsyncRead + AsyncWrite + Unpin>(client: &mut S) -> Result<(String, u16)> {
    let mut hdr = [0u8; 4];
    client
        .read_exact(&mut hdr)
        .await
        .context("read request header")?;
    if hdr[0] != SOCKS5_VERSION {
        bail!("request: bad version 0x{:02x}", hdr[0]);
    }
    let cmd = hdr[1];
    let atyp = hdr[3];

    if cmd != CMD_CONNECT {
        reply(client, REPLY_CMD_NOT_SUPPORTED).await?;
        bail!("command 0x{:02x} not supported (CONNECT only)", cmd);
    }

    let (host, port) = match atyp {
        ATYP_IPV4 => {
            let mut a = [0u8; 4];
            client.read_exact(&mut a).await.context("read IPv4")?;
            let mut p = [0u8; 2];
            client.read_exact(&mut p).await.context("read port")?;
            (
                format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3]),
                u16::from_be_bytes(p),
            )
        }
        ATYP_DOMAIN => {
            let mut l = [0u8; 1];
            client.read_exact(&mut l).await.context("read domain len")?;
            let mut d = vec![0u8; l[0] as usize];
            client.read_exact(&mut d).await.context("read domain")?;
            let mut p = [0u8; 2];
            client.read_exact(&mut p).await.context("read port")?;
            let host = String::from_utf8(d).context("domain not UTF-8")?;
            (host, u16::from_be_bytes(p))
        }
        ATYP_IPV6 => {
            let mut a = [0u8; 16];
            client.read_exact(&mut a).await.context("read IPv6")?;
            let mut p = [0u8; 2];
            client.read_exact(&mut p).await.context("read port")?;
            (Ipv6Addr::from(a).to_string(), u16::from_be_bytes(p))
        }
        _ => {
            reply(client, REPLY_ATYP_NOT_SUPPORTED).await?;
            bail!("address type 0x{:02x} not supported", atyp);
        }
    };
    Ok((host, port))
}

async fn reply<S: AsyncWrite + Unpin>(client: &mut S, rep: u8) -> io::Result<()> {
    let buf = [SOCKS5_VERSION, rep, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0];
    client.write_all(&buf).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};

    // negotiate_method ---------------------------------------------------------

    #[tokio::test]
    async fn negotiate_picks_no_auth_when_only_no_auth_offered() {
        let (mut client, mut server) = duplex(64);
        // Client greeting: ver=5, nmethods=1, methods=[0x00 no-auth]
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();

        let creds = negotiate_method(&mut server, &ListenAuth::None)
            .await
            .unwrap();
        assert!(creds.is_none(), "no-auth path must not yield UserPass");

        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, METHOD_NO_AUTH]);
    }

    #[tokio::test]
    async fn negotiate_prefers_userpass_when_both_offered() {
        let (mut client, mut server) = duplex(128);
        // Client greeting: ver=5, nmethods=2, methods=[0x00, 0x02]
        client.write_all(&[0x05, 0x02, 0x00, 0x02]).await.unwrap();
        // After server picks userpass, client must send the userpass payload.
        client
            .write_all(&[
                0x01, // userpass version
                0x10, // ulen = 16
                b'p', b'r', b'o', b'v', b'i', b'd', b'e', b'r', b'=', b'u', b's', b'-', b'f', b'o',
                b'o', b';', 0x00, // plen = 0 (no password — V0.7 ignores it anyway)
            ])
            .await
            .unwrap();

        let creds = negotiate_method(&mut server, &ListenAuth::None)
            .await
            .unwrap();
        let creds = creds.expect("server should yield UserPass when 0x02 was negotiated");
        assert_eq!(creds.username, "provider=us-foo;");
        assert_eq!(creds.password, "");

        // Method choice: 0x02. Then userpass success: ver=1 status=0.
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, METHOD_USERPASS]);

        let mut auth_reply = [0u8; 2];
        client.read_exact(&mut auth_reply).await.unwrap();
        assert_eq!(auth_reply, [USERPASS_VERSION, USERPASS_STATUS_SUCCESS]);
    }

    #[tokio::test]
    async fn negotiate_picks_userpass_when_only_userpass_offered() {
        let (mut client, mut server) = duplex(128);
        // Client greeting offers ONLY userpass (0x02).
        client.write_all(&[0x05, 0x01, 0x02]).await.unwrap();
        // Send minimal userpass: u="x" p="y".
        client
            .write_all(&[0x01, 0x01, b'x', 0x01, b'y'])
            .await
            .unwrap();

        let creds = negotiate_method(&mut server, &ListenAuth::None)
            .await
            .unwrap();
        let creds = creds.expect("UserPass expected");
        assert_eq!(creds.username, "x");
        assert_eq!(creds.password, "y");

        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).await.unwrap();
        assert_eq!(method_reply, [0x05, METHOD_USERPASS]);
    }

    #[tokio::test]
    async fn negotiate_rejects_when_no_acceptable_method() {
        let (mut client, mut server) = duplex(64);
        // Client offers only GSSAPI (0x01) and CHAP (0x03) — neither supported.
        client.write_all(&[0x05, 0x02, 0x01, 0x03]).await.unwrap();

        let err = negotiate_method(&mut server, &ListenAuth::None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no acceptable"), "got: {err}");

        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, METHOD_NO_ACCEPTABLE]);
    }

    #[tokio::test]
    async fn negotiate_rejects_wrong_version() {
        let (mut client, mut server) = duplex(64);
        // SOCKS4-ish version byte; we want only SOCKS5.
        client.write_all(&[0x04, 0x01, 0x00]).await.unwrap();

        let err = negotiate_method(&mut server, &ListenAuth::None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("0x04"), "got: {err}");
    }

    // parse_request ------------------------------------------------------------

    #[tokio::test]
    async fn parse_request_ipv4_443() {
        let (mut client, mut server) = duplex(64);
        // ver, cmd=CONNECT, rsv, atyp=IPv4, 192.168.1.10, port=443
        client
            .write_all(&[0x05, 0x01, 0x00, 0x01, 192, 168, 1, 10, 0x01, 0xbb])
            .await
            .unwrap();

        let (host, port) = parse_request(&mut server).await.unwrap();
        assert_eq!(host, "192.168.1.10");
        assert_eq!(port, 443);
    }

    #[tokio::test]
    async fn parse_request_ipv6_80() {
        let (mut client, mut server) = duplex(64);
        // ::1 in IPv6
        let mut buf = vec![0x05, 0x01, 0x00, 0x04];
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);
        buf.extend_from_slice(&[0x00, 0x50]);
        client.write_all(&buf).await.unwrap();

        let (host, port) = parse_request(&mut server).await.unwrap();
        assert_eq!(host, "::1");
        assert_eq!(port, 80);
    }

    #[tokio::test]
    async fn parse_request_domain_example_com_443() {
        let (mut client, mut server) = duplex(64);
        let domain = b"example.com";
        let mut buf = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
        buf.extend_from_slice(domain);
        buf.extend_from_slice(&[0x01, 0xbb]);
        client.write_all(&buf).await.unwrap();

        let (host, port) = parse_request(&mut server).await.unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 443);
    }

    #[tokio::test]
    async fn parse_request_rejects_cmd_bind_with_reply() {
        let (mut client, mut server) = duplex(64);
        // CMD=BIND (0x02), unsupported.
        client
            .write_all(&[0x05, 0x02, 0x00, 0x01, 0, 0, 0, 0, 0x00, 0x00])
            .await
            .unwrap();

        let err = parse_request(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("0x02"), "got: {err}");

        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], REPLY_CMD_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn parse_request_rejects_atyp_unknown_with_reply() {
        let (mut client, mut server) = duplex(64);
        // CMD=CONNECT, ATYP=0x99 (invalid).
        client.write_all(&[0x05, 0x01, 0x00, 0x99]).await.unwrap();

        let err = parse_request(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("0x99"), "got: {err}");

        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply[0], 0x05);
        assert_eq!(reply[1], REPLY_ATYP_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn parse_request_rejects_wrong_version() {
        let (mut client, mut server) = duplex(64);
        client
            .write_all(&[0x04, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0x00, 0x00])
            .await
            .unwrap();

        let err = parse_request(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("0x04"), "got: {err}");
    }

    // End-to-end integration: real SOCKS5 client → server::run → mock upstream.
    // ------------------------------------------------------------------------

    use crate::config::{
        Listen, ListenAuth, Upstream, UpstreamCreds, UpstreamKind, DEFAULT_UPSTREAM_NAME,
    };
    use crate::test_helpers::{
        echo_roundtrip, pick_free_port, socks5_connect, socks5_connect_capture_code,
        spawn_mock_upstream, MockBehavior,
    };
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn cfg_for(
        listen_addr: std::net::SocketAddr,
        upstream_addr: std::net::SocketAddr,
        user: &str,
        pass: &str,
    ) -> Config {
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            DEFAULT_UPSTREAM_NAME.to_string(),
            Upstream {
                kind: UpstreamKind::HttpConnect,
                host: upstream_addr.ip().to_string(),
                port: upstream_addr.port(),
                auth: UpstreamCreds {
                    username: user.to_string(),
                    password: pass.to_string(),
                },
            },
        );
        Config {
            listen: Listen {
                addr: listen_addr,
                auth: ListenAuth::None,
            },
            upstreams,
            active_route: None,
        }
    }

    async fn wait_until_listening(addr: std::net::SocketAddr) {
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("server didn't start listening on {addr} within 1s");
    }

    #[tokio::test]
    async fn e2e_socks5_through_runic_to_upstream_echoes() {
        let upstream_addr = spawn_mock_upstream(MockBehavior::Echo, "alice", "s3cret").await;
        let listen_addr = pick_free_port();
        let cfg = cfg_for(listen_addr, upstream_addr, "alice", "s3cret");
        let (_tx, rx) = watch::channel(Arc::new(cfg));

        tokio::spawn(async move {
            let _ = run(rx, None).await;
        });
        wait_until_listening(listen_addr).await;

        let mut tunnel = socks5_connect(listen_addr, "any.target.example", 443)
            .await
            .expect("SOCKS5 CONNECT should succeed");

        let payload = b"ping-through-tunnel";
        let echoed = echo_roundtrip(&mut tunnel, payload).await.unwrap();
        assert_eq!(echoed, payload);
    }

    #[tokio::test]
    async fn e2e_upstream_407_surfaces_as_socks5_general_failure() {
        let upstream_addr = spawn_mock_upstream(MockBehavior::AuthRefused, "", "").await;
        let listen_addr = pick_free_port();
        let cfg = cfg_for(listen_addr, upstream_addr, "anyone", "ignored");
        let (_tx, rx) = watch::channel(Arc::new(cfg));

        tokio::spawn(async move {
            let _ = run(rx, None).await;
        });
        wait_until_listening(listen_addr).await;

        let code = socks5_connect_capture_code(listen_addr, "any.target.example", 443)
            .await
            .expect("SOCKS5 reply readable");
        assert_eq!(
            code, REPLY_GENERAL_FAILURE,
            "expected SOCKS5 general failure (0x01), got 0x{code:02x}"
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn cfg_for_two_upstreams(
        listen_addr: std::net::SocketAddr,
        default_up: std::net::SocketAddr,
        default_user: &str,
        default_pass: &str,
        named: &str,
        named_up: std::net::SocketAddr,
        named_user: &str,
        named_pass: &str,
    ) -> Config {
        let mut upstreams = BTreeMap::new();
        upstreams.insert(
            DEFAULT_UPSTREAM_NAME.to_string(),
            Upstream {
                kind: UpstreamKind::HttpConnect,
                host: default_up.ip().to_string(),
                port: default_up.port(),
                auth: UpstreamCreds {
                    username: default_user.to_string(),
                    password: default_pass.to_string(),
                },
            },
        );
        upstreams.insert(
            named.to_string(),
            Upstream {
                kind: UpstreamKind::HttpConnect,
                host: named_up.ip().to_string(),
                port: named_up.port(),
                auth: UpstreamCreds {
                    username: named_user.to_string(),
                    password: named_pass.to_string(),
                },
            },
        );
        Config {
            listen: Listen {
                addr: listen_addr,
                auth: ListenAuth::None,
            },
            upstreams,
            active_route: None,
        }
    }

    #[tokio::test]
    async fn e2e_userpass_provider_routes_to_named_upstream() {
        // Two mock upstreams with DIFFERENT expected credentials. Routing must
        // pick the upstream whose creds match — if routing routed wrongly, the
        // wrong mock would respond 407 NO_USER and CONNECT would fail.
        let upstream_default = spawn_mock_upstream(MockBehavior::Echo, "alice", "a-secret").await;
        let upstream_us = spawn_mock_upstream(MockBehavior::Echo, "bob", "b-secret").await;

        let listen_addr = pick_free_port();
        let cfg = cfg_for_two_upstreams(
            listen_addr,
            upstream_default,
            "alice",
            "a-secret",
            "us-residential",
            upstream_us,
            "bob",
            "b-secret",
        );
        let (_tx, rx) = watch::channel(Arc::new(cfg));
        tokio::spawn(async move {
            let _ = run(rx, None).await;
        });
        wait_until_listening(listen_addr).await;

        // Client signals routing intent `provider=us-residential` via SOCKS5 user.
        let mut tunnel = crate::test_helpers::socks5_connect_with_userpass(
            listen_addr,
            "provider=us-residential",
            "",
            "any.target.example",
            443,
        )
        .await
        .expect("CONNECT should succeed via the us-residential upstream");

        // Payload must echo back unchanged, proving the tunnel landed on the
        // 'bob/b-secret' upstream (not the default).
        let echoed = echo_roundtrip(&mut tunnel, b"routed-via-us").await.unwrap();
        assert_eq!(echoed, b"routed-via-us");
    }

    #[tokio::test]
    async fn e2e_userpass_empty_provider_falls_back_to_default() {
        let upstream_default = spawn_mock_upstream(MockBehavior::Echo, "alice", "a-secret").await;
        let upstream_us = spawn_mock_upstream(MockBehavior::Echo, "bob", "b-secret").await;

        let listen_addr = pick_free_port();
        let cfg = cfg_for_two_upstreams(
            listen_addr,
            upstream_default,
            "alice",
            "a-secret",
            "us-residential",
            upstream_us,
            "bob",
            "b-secret",
        );
        let (_tx, rx) = watch::channel(Arc::new(cfg));
        tokio::spawn(async move {
            let _ = run(rx, None).await;
        });
        wait_until_listening(listen_addr).await;

        // SOCKS5 user does not specify provider — routing should land on default.
        let mut tunnel = crate::test_helpers::socks5_connect_with_userpass(
            listen_addr,
            "sessid=xyz",
            "",
            "any.target.example",
            443,
        )
        .await
        .expect("CONNECT should succeed via default when provider is missing");

        let echoed = echo_roundtrip(&mut tunnel, b"falls-back").await.unwrap();
        assert_eq!(echoed, b"falls-back");
    }

    #[tokio::test]
    async fn e2e_no_auth_client_still_routes_to_default() {
        // V0.7 must remain backwards-compat with no-auth (METHOD 0x00) clients.
        let upstream_default = spawn_mock_upstream(MockBehavior::Echo, "alice", "a-secret").await;
        let upstream_us = spawn_mock_upstream(MockBehavior::AuthRefused, "", "").await;

        let listen_addr = pick_free_port();
        let cfg = cfg_for_two_upstreams(
            listen_addr,
            upstream_default,
            "alice",
            "a-secret",
            "us-residential",
            upstream_us,
            "bob",
            "b-secret",
        );
        let (_tx, rx) = watch::channel(Arc::new(cfg));
        tokio::spawn(async move {
            let _ = run(rx, None).await;
        });
        wait_until_listening(listen_addr).await;

        let mut tunnel = socks5_connect(listen_addr, "any.target.example", 443)
            .await
            .expect("legacy no-auth client should still route to default");

        let echoed = echo_roundtrip(&mut tunnel, b"legacy-no-auth")
            .await
            .unwrap();
        assert_eq!(echoed, b"legacy-no-auth");
    }

    // --- silo (rfc1929) data plane ------------------------------------------

    #[tokio::test]
    async fn e2e_silo_token_in_password_routes_via_variation() {
        use crate::silo::{SiloStore, VariationCache, VariationData};

        // The variation's `default` upstream points at this mock.
        let upstream_addr = spawn_mock_upstream(MockBehavior::Echo, "alice", "s3cret").await;

        let dir = tempfile::tempdir().unwrap();
        let mut cache = VariationCache::new(
            SiloStore::open(dir.path().join("runic.silo"), 3600).unwrap(),
            3600,
        );
        let token = cache.create(0).unwrap();
        let mut ups = BTreeMap::new();
        ups.insert(
            "default".to_string(),
            Upstream {
                kind: UpstreamKind::HttpConnect,
                host: upstream_addr.ip().to_string(),
                port: upstream_addr.port(),
                auth: UpstreamCreds {
                    username: "alice".into(),
                    password: "s3cret".into(),
                },
            },
        );
        cache
            .write(&token, &VariationData { upstreams: ups }, 0)
            .unwrap();
        let silo = Some(Arc::new(Mutex::new(cache)));

        // Cold pool is EMPTY — the only route comes from the variation.
        let listen_addr = pick_free_port();
        let cfg = Config {
            listen: Listen {
                addr: listen_addr,
                auth: ListenAuth::None,
            },
            upstreams: BTreeMap::new(),
            active_route: None,
        };
        let (_tx, rx) = watch::channel(Arc::new(cfg));
        tokio::spawn(async move {
            let _ = run(rx, silo).await;
        });
        wait_until_listening(listen_addr).await;

        // The token rides in the SOCKS5 password (RFC 1929).
        let mut tunnel = crate::test_helpers::socks5_connect_with_userpass(
            listen_addr,
            "",
            &token,
            "any.target.example",
            443,
        )
        .await
        .expect("CONNECT should succeed via the variation's upstream");
        let echoed = echo_roundtrip(&mut tunnel, b"silo-routed").await.unwrap();
        assert_eq!(echoed, b"silo-routed");
    }

    #[tokio::test]
    async fn e2e_silo_unknown_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = crate::silo::VariationCache::new(
            crate::silo::SiloStore::open(dir.path().join("runic.silo"), 3600).unwrap(),
            3600,
        );
        let silo = Some(Arc::new(Mutex::new(cache)));
        let listen_addr = pick_free_port();
        let cfg = Config {
            listen: Listen {
                addr: listen_addr,
                auth: ListenAuth::None,
            },
            upstreams: BTreeMap::new(),
            active_route: None,
        };
        let (_tx, rx) = watch::channel(Arc::new(cfg));
        tokio::spawn(async move {
            let _ = run(rx, silo).await;
        });
        wait_until_listening(listen_addr).await;

        // A bogus token as the password → no route → CONNECT refused.
        let res = crate::test_helpers::socks5_connect_with_userpass(
            listen_addr,
            "",
            "not-a-real-token",
            "any.target.example",
            443,
        )
        .await;
        assert!(res.is_err(), "unknown silo token must be refused");
    }

    #[tokio::test]
    async fn e2e_rebinds_on_listen_addr_change() {
        let upstream_addr = spawn_mock_upstream(MockBehavior::Echo, "u", "p").await;
        let addr1 = pick_free_port();
        let addr2 = pick_free_port();
        let cfg1 = cfg_for(addr1, upstream_addr, "u", "p");
        let (tx, rx) = watch::channel(Arc::new(cfg1));

        tokio::spawn(async move {
            let _ = run(rx, None).await;
        });
        wait_until_listening(addr1).await;

        // Sanity: addr1 works.
        let mut t1 = socks5_connect(addr1, "any.example", 443).await.unwrap();
        let echoed = echo_roundtrip(&mut t1, b"on-addr1").await.unwrap();
        assert_eq!(echoed, b"on-addr1");
        drop(t1);

        // Push a new config with addr2 — server should drop its addr1 listener
        // and rebind on addr2.
        let upstream2 = spawn_mock_upstream(MockBehavior::Echo, "u", "p").await;
        let cfg2 = cfg_for(addr2, upstream2, "u", "p");
        tx.send(Arc::new(cfg2)).unwrap();
        wait_until_listening(addr2).await;

        let mut t2 = socks5_connect(addr2, "any.example", 443).await.unwrap();
        let echoed = echo_roundtrip(&mut t2, b"on-addr2").await.unwrap();
        assert_eq!(echoed, b"on-addr2");

        // And addr1 should now refuse — listener was dropped.
        let dial_old = tokio::net::TcpStream::connect(addr1).await;
        assert!(
            dial_old.is_err(),
            "addr1 should no longer accept after rebind"
        );
    }
}
