//! Brokered read-only HTTP(S) fetch proxy.
//!
//! Sandboxed workers get no direct network egress (Seatbelt profile); any
//! network need goes through this local proxy, which:
//! - permits GET/HEAD (plain-HTTP forwarding) and CONNECT tunnels to
//!   allowlisted package registries,
//! - rejects mutations (POST/PUT/DELETE/PATCH) and non-allowlisted CONNECT,
//! - records every decision to the ledger via the audit callback.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// Domains allowed for CONNECT tunnels and plain-HTTP fetches.
pub const DEFAULT_ALLOWLIST: &[&str] = &[
    "registry.npmjs.org",
    "crates.io",
    "index.crates.io",
    "static.crates.io",
    "pypi.org",
    "files.pythonhosted.org",
    "proxy.golang.org",
    "rubygems.org",
];

#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub allowlist: Vec<String>,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            allowlist: DEFAULT_ALLOWLIST.iter().map(|s| s.to_string()).collect(),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum ProxyDecision {
    Allow,
    Deny(String),
}

/// Split `host[:port]`, defaulting to `default_port`.
fn split_host(authority: &str, default_port: u16) -> (String, u16) {
    match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => {
            (host.to_string(), port.parse().unwrap())
        }
        _ => (authority.to_string(), default_port),
    }
}

/// Plain-HTTP request decision. Only read methods, and only to allowlisted
/// hosts (arbitrary GETs still hit the audit log). Returns the decision plus
/// the parsed (host, port).
pub fn decide_http(method: &str, uri: &str, config: &ProxyConfig) -> (ProxyDecision, String, u16) {
    let (host, port) = uri
        .strip_prefix("http://")
        .and_then(|rest| rest.split('/').next())
        .map(|h| split_host(h, 80))
        .unwrap_or_else(|| (String::new(), 80));
    if !matches!(method, "GET" | "HEAD") {
        return (
            ProxyDecision::Deny(format!("method {method} is a mutation")),
            host,
            port,
        );
    }
    if !config.allowlist.contains(&host) {
        return (
            ProxyDecision::Deny(format!("host {host} is not allowlisted")),
            host,
            port,
        );
    }
    (ProxyDecision::Allow, host, port)
}

/// CONNECT (HTTPS tunnel) decision: allowlisted domains only.
pub fn decide_connect(authority: &str, config: &ProxyConfig) -> (ProxyDecision, String) {
    let (host, _) = split_host(authority, 443);
    if config.allowlist.contains(&host) {
        (ProxyDecision::Allow, host)
    } else {
        (
            ProxyDecision::Deny(format!("host {host} is not allowlisted")),
            host,
        )
    }
}

/// Audit sink: (domain, action, allowed).
pub type AuditFn = Arc<dyn Fn(String, String, bool) + Send + Sync>;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub struct FetchProxy {
    pub addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
    handle: tokio::task::JoinHandle<()>,
}

impl FetchProxy {
    pub async fn start(config: ProxyConfig, audit: AuditFn) -> std::io::Result<Self> {
        Self::start_on("127.0.0.1:0", config, audit).await
    }

    pub async fn start_on(
        addr: &str,
        config: ProxyConfig,
        audit: AuditFn,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let shutdown_task = Arc::clone(&shutdown);
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_task.notified() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _)) => {
                                let config = config.clone();
                                let audit = Arc::clone(&audit);
                                tokio::spawn(async move {
                                    if let Err(e) = handle_connection(stream, &config, &audit).await {
                                        tracing::debug!(error = %e, "proxy connection ended");
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "proxy accept failed");
                                tokio::time::sleep(Duration::from_millis(50)).await;
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            addr: local,
            shutdown,
            handle,
        })
    }

    pub async fn shutdown(self) {
        self.shutdown.notify_waiters();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.handle).await;
    }
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await
}

/// Read HTTP request head (through the blank line), bounded.
async fn read_head(reader: &mut BufReader<TcpStream>) -> std::io::Result<String> {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        let n = tokio::time::timeout(IO_TIMEOUT, reader.read_line(&mut line)).await??;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "eof",
            ));
        }
        head.push_str(&line);
        if line == "\r\n" || head.len() > MAX_HEAD_BYTES {
            return Ok(head);
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    config: &ProxyConfig,
    audit: &AuditFn,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream);
    let head = read_head(&mut reader).await?;
    let request_line = head.lines().next().unwrap_or("").to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");

    if method == "CONNECT" {
        let (decision, host) = decide_connect(target, config);
        let allowed = decision == ProxyDecision::Allow;
        audit(host.clone(), "tunnel".into(), allowed);
        match decision {
            ProxyDecision::Deny(reason) => {
                respond(reader.get_mut(), "403 Forbidden", &reason).await?;
            }
            ProxyDecision::Allow => {
                let upstream = TcpStream::connect(target).await?;
                reader
                    .get_mut()
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await?;
                // Forward any already-buffered TLS bytes before tunneling.
                let buffered = reader.buffer().to_vec();
                let mut upstream = upstream;
                if !buffered.is_empty() {
                    upstream.write_all(&buffered).await?;
                }
                let mut client = reader.into_inner();
                tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
            }
        }
        return Ok(());
    }

    let (decision, host, port) = decide_http(method, target, config);
    let allowed = decision == ProxyDecision::Allow;
    audit(host.clone(), "fetch".into(), allowed);
    match decision {
        ProxyDecision::Deny(reason) => {
            respond(reader.get_mut(), "405 Method Not Allowed", &reason).await?;
        }
        ProxyDecision::Allow => {
            // Rewrite absolute-form to origin-form and force close.
            let path = target
                .strip_prefix("http://")
                .and_then(|rest| {
                    rest[rest.find('/').unwrap_or(rest.len())..]
                        .split_whitespace()
                        .next()
                })
                .unwrap_or("/");
            let mut upstream = TcpStream::connect(format!("{host}:{port}")).await?;
            let mut rewritten = String::new();
            for (i, line) in head.lines().enumerate() {
                if i == 0 {
                    rewritten.push_str(&format!("{method} {path} HTTP/1.1\r\n"));
                } else if line.to_ascii_lowercase().starts_with("proxy-connection")
                    || line.to_ascii_lowercase().starts_with("connection")
                {
                    rewritten.push_str("Connection: close\r\n");
                } else if !line.is_empty() {
                    rewritten.push_str(line);
                    rewritten.push_str("\r\n");
                }
            }
            rewritten.push_str("\r\n");
            upstream.write_all(rewritten.as_bytes()).await?;
            tokio::time::timeout(IO_TIMEOUT, async {
                tokio::io::copy(&mut upstream, reader.get_mut()).await
            })
            .await??;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn cfg() -> ProxyConfig {
        ProxyConfig {
            allowlist: vec!["crates.io".into(), "127.0.0.1".into(), "localhost".into()],
        }
    }

    #[test]
    fn decisions_enforce_read_only_and_allowlist() {
        let c = cfg();
        assert_eq!(
            decide_http("GET", "http://crates.io/x", &c).0,
            ProxyDecision::Allow
        );
        assert_eq!(
            decide_http("HEAD", "http://crates.io/x", &c).0,
            ProxyDecision::Allow
        );
        assert!(matches!(
            decide_http("POST", "http://crates.io/x", &c).0,
            ProxyDecision::Deny(_)
        ));
        assert!(matches!(
            decide_http("GET", "http://evil.example.com/x", &c).0,
            ProxyDecision::Deny(_)
        ));
        assert_eq!(decide_connect("crates.io:443", &c).0, ProxyDecision::Allow);
        assert!(matches!(
            decide_connect("github.com:443", &c).0,
            ProxyDecision::Deny(_)
        ));
    }

    async fn spawn_origin(body: &'static str) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn proxy_forwards_get_and_denies_mutations() {
        let origin = spawn_origin("hello-body").await;
        let audits: Arc<std::sync::Mutex<Vec<(String, String, bool)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let audits_clone = Arc::clone(&audits);
        let proxy = FetchProxy::start(
            ProxyConfig {
                allowlist: vec!["127.0.0.1".into()],
            },
            Arc::new(move |domain, action, allowed| {
                audits_clone.lock().unwrap().push((domain, action, allowed));
            }),
        )
        .await
        .unwrap();

        // GET through the proxy is forwarded and audited.
        let mut client = TcpStream::connect(proxy.addr).await.unwrap();
        client
            .write_all(
                format!(
                    "GET http://127.0.0.1:{}/path HTTP/1.1\r\nHost: x\r\n\r\n",
                    origin.port()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.ends_with("hello-body"));

        // POST is rejected without touching the origin.
        let mut client = TcpStream::connect(proxy.addr).await.unwrap();
        client
            .write_all(b"POST http://127.0.0.1/x HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 405"));

        {
            let records = audits.lock().unwrap();
            assert!(
                records
                    .iter()
                    .any(|(d, a, ok)| d == "127.0.0.1" && a == "fetch" && *ok)
            );
            assert!(records.iter().any(|(_, a, ok)| a == "fetch" && !*ok));
        }

        proxy.shutdown().await;
    }

    #[tokio::test]
    async fn connect_tunnel_only_for_allowlisted() {
        let origin = spawn_origin("tunneled").await;
        let proxy = FetchProxy::start(
            ProxyConfig {
                allowlist: vec!["127.0.0.1".into()],
            },
            Arc::new(|_, _, _| {}),
        )
        .await
        .unwrap();

        // Allowlisted CONNECT tunnels bytes.
        let mut client = TcpStream::connect(proxy.addr).await.unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{} HTTP/1.1\r\n\r\n", origin.port()).as_bytes())
            .await
            .unwrap();
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert!(line.contains("200"));
        reader.read_line(&mut line).await.unwrap(); // blank line
        reader
            .get_mut()
            .write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        reader.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("tunneled"));

        // Non-allowlisted CONNECT is refused.
        let mut client = TcpStream::connect(proxy.addr).await.unwrap();
        client
            .write_all(b"CONNECT github.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 403"));

        proxy.shutdown().await;
    }
}
