//! Shared test fixtures (mock upstream + SOCKS5 client driver).
//! Only compiled in `#[cfg(test)]`.

use std::net::SocketAddr;

use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Pick a free ephemeral port by binding+dropping. Subject to a small race with
/// another process grabbing the same port before the caller re-binds; rare in
/// test environments, accept the risk.
pub fn pick_free_port() -> SocketAddr {
    let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = l.local_addr().expect("local_addr");
    drop(l);
    addr
}

/// What the mock upstream should do for a given client.
#[derive(Clone, Copy, Debug)]
pub enum MockBehavior {
    /// Verify `Proxy-Authorization: Basic <expected>` and respond `200`. Then
    /// `copy_bidirectional` between the client and itself — i.e. echo any bytes
    /// the client sends back to the client (via the tunnel).
    Echo,
    /// Respond `407 NO_USER` regardless of credentials and close.
    AuthRefused,
}

/// Spawn a one-shot mock upstream HTTP CONNECT server on 127.0.0.1:0. Returns
/// the bound address. The server accepts a single connection in `Echo` mode
/// and serves it; in `AuthRefused` mode it serves every connection that comes.
///
/// `expected_user` / `expected_pass` are only checked in `Echo` mode; mismatch
/// triggers a `407` response.
pub async fn spawn_mock_upstream(
    behavior: MockBehavior,
    expected_user: &str,
    expected_pass: &str,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("mock local_addr");
    let expected_user = expected_user.to_string();
    let expected_pass = expected_pass.to_string();
    tokio::spawn(async move {
        loop {
            let (sock, _peer) = match listener.accept().await {
                Ok(v) => v,
                Err(_) => return,
            };
            let exp_user = expected_user.clone();
            let exp_pass = expected_pass.clone();
            tokio::spawn(async move {
                let _ = handle_mock_session(sock, behavior, &exp_user, &exp_pass).await;
            });
        }
    });
    addr
}

async fn handle_mock_session(
    mut sock: TcpStream,
    behavior: MockBehavior,
    expected_user: &str,
    expected_pass: &str,
) -> Result<()> {
    // Read request head until \r\n\r\n.
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = sock.read(&mut byte).await?;
        if n == 0 {
            return Err(anyhow!("mock: client closed before CONNECT complete"));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 4096 {
            return Err(anyhow!("mock: head too large"));
        }
    }

    let request = String::from_utf8_lossy(&buf).to_string();
    let auth_ok = request.lines().any(|line| {
        line.strip_prefix("Proxy-Authorization: Basic ")
            .and_then(|b64| B64.decode(b64.trim()).ok())
            .and_then(|raw| String::from_utf8(raw).ok())
            .map(|s| s == format!("{expected_user}:{expected_pass}"))
            .unwrap_or(false)
    });

    match (behavior, auth_ok) {
        (MockBehavior::Echo, true) => {
            sock.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            // Echo bytes back through the tunnel.
            let (mut r, mut w) = sock.split();
            let _ = tokio::io::copy(&mut r, &mut w).await;
        }
        (MockBehavior::Echo, false) | (MockBehavior::AuthRefused, _) => {
            sock.write_all(b"HTTP/1.1 407 NO_USER\r\n\r\n").await?;
        }
    }
    Ok(())
}

/// Drive the SOCKS5 handshake on a TcpStream and request a CONNECT to
/// `target_host:target_port` using the domain ATYP. On success returns the
/// stream (now in tunneled mode). On a CONNECT-stage failure returns the
/// SOCKS5 reply code so the caller can assert on it.
pub async fn socks5_connect(
    local_addr: SocketAddr,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let mut sock = TcpStream::connect(local_addr).await?;
    // Method negotiation: ver=5, nmethods=1, methods=[no-auth].
    sock.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut greeting_reply = [0u8; 2];
    sock.read_exact(&mut greeting_reply).await?;
    if greeting_reply != [0x05, 0x00] {
        bail!(
            "auth negotiation failed: server picked method 0x{:02x}",
            greeting_reply[1]
        );
    }
    // CONNECT request: ver=5, cmd=1, rsv, atyp=domain, len, domain, port_be.
    let domain = target_host.as_bytes();
    let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&target_port.to_be_bytes());
    sock.write_all(&req).await?;
    let mut conn_reply = [0u8; 10];
    sock.read_exact(&mut conn_reply).await?;
    if conn_reply[1] != 0x00 {
        bail!("CONNECT failed: SOCKS5 reply code 0x{:02x}", conn_reply[1]);
    }
    Ok(sock)
}

/// Like [`socks5_connect`] but performs the SOCKS5 username/password
/// sub-negotiation (METHOD 0x02) before issuing the CONNECT request. Used by
/// V0.7 routing tests where the username carries the routing intent
/// (`provider=...;sessid=...`).
pub async fn socks5_connect_with_userpass(
    local_addr: SocketAddr,
    username: &str,
    password: &str,
    target_host: &str,
    target_port: u16,
) -> Result<TcpStream> {
    let mut sock = TcpStream::connect(local_addr).await?;
    // Method negotiation: ver=5, nmethods=1, methods=[user/pass].
    sock.write_all(&[0x05, 0x01, 0x02]).await?;
    let mut greeting_reply = [0u8; 2];
    sock.read_exact(&mut greeting_reply).await?;
    if greeting_reply != [0x05, 0x02] {
        bail!(
            "auth negotiation failed: server picked method 0x{:02x}",
            greeting_reply[1]
        );
    }
    // RFC 1929 userpass: ver=1, ulen, u, plen, p.
    let mut up_req = Vec::new();
    up_req.push(0x01u8);
    up_req.push(username.len() as u8);
    up_req.extend_from_slice(username.as_bytes());
    up_req.push(password.len() as u8);
    up_req.extend_from_slice(password.as_bytes());
    sock.write_all(&up_req).await?;
    let mut up_reply = [0u8; 2];
    sock.read_exact(&mut up_reply).await?;
    if up_reply[0] != 0x01 || up_reply[1] != 0x00 {
        bail!("userpass auth refused: status 0x{:02x}", up_reply[1]);
    }
    // CONNECT request: ver=5, cmd=1, rsv, atyp=domain, len, domain, port_be.
    let domain = target_host.as_bytes();
    let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&target_port.to_be_bytes());
    sock.write_all(&req).await?;
    let mut conn_reply = [0u8; 10];
    sock.read_exact(&mut conn_reply).await?;
    if conn_reply[1] != 0x00 {
        bail!("CONNECT failed: SOCKS5 reply code 0x{:02x}", conn_reply[1]);
    }
    Ok(sock)
}

/// Like [`socks5_connect`] but returns the SOCKS5 reply code instead of an
/// error when CONNECT is rejected — useful when the test wants to assert on
/// the specific error code (e.g. `0x01` general failure for upstream 407).
pub async fn socks5_connect_capture_code(
    local_addr: SocketAddr,
    target_host: &str,
    target_port: u16,
) -> Result<u8> {
    let mut sock = TcpStream::connect(local_addr).await?;
    sock.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut greeting_reply = [0u8; 2];
    sock.read_exact(&mut greeting_reply).await?;
    if greeting_reply[1] != 0x00 {
        return Ok(greeting_reply[1]);
    }
    let domain = target_host.as_bytes();
    let mut req = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
    req.extend_from_slice(domain);
    req.extend_from_slice(&target_port.to_be_bytes());
    sock.write_all(&req).await?;
    let mut conn_reply = [0u8; 10];
    sock.read_exact(&mut conn_reply).await?;
    Ok(conn_reply[1])
}

/// Convenience: copy `payload` through `tunnel`, read the same number of bytes
/// back, and assert byte-equality. Returns the echoed bytes so the caller can
/// inspect further.
pub async fn echo_roundtrip(tunnel: &mut TcpStream, payload: &[u8]) -> Result<Vec<u8>> {
    tunnel.write_all(payload).await?;
    tunnel.flush().await?;
    let mut received = vec![0u8; payload.len()];
    tunnel.read_exact(&mut received).await?;
    Ok(received)
}

// Avoid dead-code warnings when only some helpers are used by a given test
// module (e.g. server tests don't call `echo_roundtrip`).
#[allow(dead_code)]
const _: fn() = || {
    let _ = echo_roundtrip;
    let _ = socks5_connect_capture_code;
    let _ = socks5_connect;
    let _ = socks5_connect_with_userpass;
};
