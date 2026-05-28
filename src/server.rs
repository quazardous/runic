use std::io;
use std::net::Ipv6Addr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::io::{copy_bidirectional, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::config::{Config, ListenAuth};
use crate::upstream;

const SOCKS5_VERSION: u8 = 0x05;
const METHOD_NO_AUTH: u8 = 0x00;
const METHOD_NO_ACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REPLY_SUCCEEDED: u8 = 0x00;
const REPLY_GENERAL_FAILURE: u8 = 0x01;
const REPLY_CMD_NOT_SUPPORTED: u8 = 0x07;
const REPLY_ATYP_NOT_SUPPORTED: u8 = 0x08;

pub async fn run(mut cfg_rx: watch::Receiver<Arc<Config>>) -> Result<()> {
    let initial = cfg_rx.borrow().clone();
    let mut current_addr = initial.listen.addr;
    let mut listener = TcpListener::bind(current_addr)
        .await
        .with_context(|| format!("bind SOCKS5 listener on {current_addr}"))?;
    info!(addr = %current_addr, upstream = %format!("{}:{}", initial.upstream.host, initial.upstream.port), "runic listening");

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
                tokio::spawn(async move {
                    if let Err(e) = serve(client, &session_cfg).await {
                        warn!(%peer, error = %e, "session ended with error");
                    }
                });
            }
            changed = cfg_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let new_addr = cfg_rx.borrow().listen.addr;
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

async fn serve(mut client: TcpStream, cfg: &Config) -> Result<()> {
    negotiate_method(&mut client, &cfg.listen.auth).await?;
    let (host, port) = parse_request(&mut client).await?;
    debug!(%host, port, "client requested CONNECT");

    let mut upstream_stream = match upstream::connect_via(&cfg.upstream, &host, port).await {
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
        Err(e) if matches!(e.kind(), io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe) => {}
        Err(e) => warn!(%host, port, ?e, "pump error"),
    }
    Ok(())
}

async fn negotiate_method<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut S,
    auth: &ListenAuth,
) -> Result<()> {
    let mut hdr = [0u8; 2];
    client.read_exact(&mut hdr).await.context("read greeting")?;
    if hdr[0] != SOCKS5_VERSION {
        bail!("unsupported SOCKS version 0x{:02x}", hdr[0]);
    }
    let nmethods = hdr[1] as usize;
    let mut methods = vec![0u8; nmethods];
    client.read_exact(&mut methods).await.context("read methods")?;

    let chosen = match auth {
        ListenAuth::None => {
            if methods.contains(&METHOD_NO_AUTH) {
                METHOD_NO_AUTH
            } else {
                METHOD_NO_ACCEPTABLE
            }
        }
    };

    client
        .write_all(&[SOCKS5_VERSION, chosen])
        .await
        .context("write method choice")?;

    if chosen == METHOD_NO_ACCEPTABLE {
        bail!("client offered no acceptable auth method (offered: {:?})", methods);
    }
    Ok(())
}

async fn parse_request<S: AsyncRead + AsyncWrite + Unpin>(
    client: &mut S,
) -> Result<(String, u16)> {
    let mut hdr = [0u8; 4];
    client.read_exact(&mut hdr).await.context("read request header")?;
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
    async fn negotiate_picks_no_auth_when_offered() {
        let (mut client, mut server) = duplex(64);
        // Client greeting: ver=5, nmethods=1, methods=[0x00 no-auth]
        client.write_all(&[0x05, 0x01, 0x00]).await.unwrap();

        negotiate_method(&mut server, &ListenAuth::None).await.unwrap();

        let mut reply = [0u8; 2];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(reply, [0x05, METHOD_NO_AUTH]);
    }

    #[tokio::test]
    async fn negotiate_rejects_when_no_acceptable_method() {
        let (mut client, mut server) = duplex(64);
        // Client offers only username/password (0x02), we want no-auth only.
        client.write_all(&[0x05, 0x01, 0x02]).await.unwrap();

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
        client
            .write_all(&[0x05, 0x01, 0x00, 0x99])
            .await
            .unwrap();

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
}
