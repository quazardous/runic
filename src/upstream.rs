use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::config::Upstream;

const RESPONSE_HEAD_LIMIT: usize = 8 * 1024;

pub async fn connect_via(upstream: &Upstream, target_host: &str, target_port: u16) -> Result<TcpStream> {
    let endpoint = format!("{}:{}", upstream.host, upstream.port);
    let mut stream = TcpStream::connect(&endpoint)
        .await
        .with_context(|| format!("dial upstream {endpoint}"))?;

    let target = format!("{target_host}:{target_port}");
    let credential = B64.encode(format!("{}:{}", upstream.auth.username, upstream.auth.password));

    let request = format!(
        "CONNECT {target} HTTP/1.1\r\n\
         Host: {target}\r\n\
         Proxy-Authorization: Basic {credential}\r\n\
         Proxy-Connection: Keep-Alive\r\n\
         \r\n"
    );

    stream
        .write_all(request.as_bytes())
        .await
        .context("write CONNECT request to upstream")?;

    let head = read_response_head(&mut stream).await?;
    let status = parse_status_line(&head)?;

    if status.code != 200 {
        return Err(anyhow!(
            "upstream rejected CONNECT for {target}: HTTP {} {}",
            status.code,
            status.reason
        ));
    }

    Ok(stream)
}

async fn read_response_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        let n = stream
            .read(&mut byte)
            .await
            .context("read upstream CONNECT response")?;
        if n == 0 {
            return Err(anyhow!("upstream closed before CONNECT response complete"));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            return Ok(buf);
        }
        if buf.len() > RESPONSE_HEAD_LIMIT {
            return Err(anyhow!(
                "upstream CONNECT response exceeds {RESPONSE_HEAD_LIMIT} bytes without terminator"
            ));
        }
    }
}

#[derive(Debug)]
struct StatusLine {
    code: u16,
    reason: String,
}

fn parse_status_line(head: &[u8]) -> Result<StatusLine> {
    let first_line_end = head
        .windows(2)
        .position(|w| w == b"\r\n")
        .ok_or_else(|| anyhow!("upstream response has no CRLF"))?;
    let line = std::str::from_utf8(&head[..first_line_end])
        .map_err(|_| anyhow!("upstream status line not UTF-8"))?;

    let mut parts = line.splitn(3, ' ');
    let _version = parts
        .next()
        .ok_or_else(|| anyhow!("upstream status line missing version"))?;
    let code_str = parts
        .next()
        .ok_or_else(|| anyhow!("upstream status line missing status code"))?;
    let reason = parts.next().unwrap_or("").to_string();

    let code = code_str
        .parse::<u16>()
        .with_context(|| format!("upstream status code unparseable: '{code_str}'"))?;

    Ok(StatusLine { code, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_200_connection_established() {
        let head = b"HTTP/1.1 200 Connection Established\r\nProxy-Agent: foo\r\n\r\n";
        let s = parse_status_line(head).unwrap();
        assert_eq!(s.code, 200);
        assert_eq!(s.reason, "Connection Established");
    }

    #[test]
    fn parses_407_no_user() {
        let head = b"HTTP/1.1 407 NO_USER\r\nProxy-Authenticate: Basic\r\n\r\n";
        let s = parse_status_line(head).unwrap();
        assert_eq!(s.code, 407);
        assert_eq!(s.reason, "NO_USER");
    }

    #[test]
    fn parses_200_minimal_no_reason() {
        // Some upstreams send just "HTTP/1.1 200\r\n\r\n" with no reason phrase.
        let head = b"HTTP/1.1 200 \r\n\r\n";
        let s = parse_status_line(head).unwrap();
        assert_eq!(s.code, 200);
        assert_eq!(s.reason, "");
    }

    #[test]
    fn rejects_no_crlf() {
        let head = b"HTTP/1.1 200 OK";
        let err = parse_status_line(head).unwrap_err();
        assert!(
            err.to_string().contains("CRLF") || err.to_string().contains("no CRLF"),
            "got: {err}"
        );
    }

    #[test]
    fn rejects_unparseable_code() {
        let head = b"HTTP/1.1 XYZ Some Reason\r\n\r\n";
        let err = parse_status_line(head).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("XYZ") || msg.contains("unparseable"), "got: {msg}");
    }

    #[test]
    fn rejects_missing_code() {
        let head = b"HTTP/1.1 \r\n\r\n";
        // splitn(3, ' ') on "HTTP/1.1 " → ["HTTP/1.1", "", ""], code = "" which fails to parse.
        let err = parse_status_line(head).unwrap_err();
        assert!(err.to_string().to_lowercase().contains("unparseable"), "got: {err}");
    }
}
