//! The loopback redirect (RFC 8252): a listener on `127.0.0.1` with a random
//! port that reads one request line per connection and answers with a short page.
use anyhow::{Result, anyhow};
use reqwest::Url;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const HEAD_LIMIT: usize = 16 * 1024;

pub(crate) struct Loopback {
    listener: TcpListener,
    pub redirect_uri: String,
}

impl Loopback {
    pub(crate) async fn bind() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|_| anyhow!("cannot listen for the MCP sign-in redirect"))?;
        let port = listener
            .local_addr()
            .map_err(|_| anyhow!("cannot listen for the MCP sign-in redirect"))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// The next GET request's full URL with its connection, kept open for the answer.
    /// Malformed or slow connections are dropped and the wait continues.
    pub(crate) async fn next(&self) -> (Url, TcpStream) {
        loop {
            let Ok((mut stream, _)) = self.listener.accept().await else {
                continue;
            };
            let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut stream)).await;
            let Ok(Some(target)) = head else { continue };
            match Url::parse(&self.redirect_uri).and_then(|base| base.join(&target)) {
                Ok(url) => return (url, stream),
                Err(_) => answer(stream, false).await,
            }
        }
    }
}

async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 2048];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 || bytes.len() + read > HEAD_LIMIT {
            return None;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let line = std::str::from_utf8(&bytes).ok()?.lines().next()?.to_owned();
    let mut parts = line.split(' ');
    let (method, target) = (parts.next()?, parts.next()?);
    (method == "GET" && target.starts_with('/')).then(|| target.to_owned())
}

/// A short static page; nothing from the request is echoed.
pub(crate) async fn answer(mut stream: TcpStream, signed_in: bool) {
    let (status, text) = if signed_in {
        (
            "200 OK",
            "Signed in. You can close this window and return to BubblePaw.",
        )
    } else {
        (
            "400 Bad Request",
            "This sign-in link is not valid. Return to BubblePaw and try again.",
        )
    };
    let body =
        format!("<!doctype html><meta charset=\"utf-8\"><title>BubblePaw</title><p>{text}</p>");
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = stream.write_all(reply.as_bytes()).await;
        let _ = stream.shutdown().await;
    })
    .await;
}
