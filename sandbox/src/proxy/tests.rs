//! The proxy against a real socket, with names resolved from a table and one public-looking
//! address dialled on a local server instead.

use std::net::Ipv4Addr;

use tokio::net::TcpListener;
use tokio::sync::broadcast::Receiver;

use super::*;
use nixxa_contracts::DomainRule;

/// A documentation address: public as far as the proxy can tell, redirected to a local server.
const PUBLIC: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 10));

struct Rig {
    proxy: Proxy,
    events: Receiver<SandboxEvent>,
    /// What the upstream server received on each connection.
    received: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    _base: TempDir,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A proxy where `allowed.test` and `other.test` resolve to [`PUBLIC`] and `local.test` to
/// 127.0.0.1. Upstream answers each connection with `pong` after reading what it was sent.
async fn rig(policy: NetworkPolicy) -> Rig {
    static N: AtomicUsize = AtomicUsize::new(0);
    let base = std::env::temp_dir().join(format!(
        "nixxa-proxy-test-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&base).unwrap();
    let hosts = HashMap::from([
        ("allowed.test".to_string(), vec![PUBLIC]),
        ("other.test".to_string(), vec![PUBLIC]),
        ("local.test".to_string(), vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]),
    ]);
    let (events_tx, events) = broadcast::channel(64);
    let proxy = Proxy::new(&base, hosts, events_tx);
    proxy.serve().unwrap();
    proxy.set_policy(policy);
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let local = upstream.local_addr().unwrap();
    proxy.redirect(SocketAddr::new(PUBLIC, 443), local);
    proxy.redirect(SocketAddr::new(PUBLIC, 80), local);
    let (tx, received) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = upstream.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut got = vec![0u8; 64 * 1024];
                let n = stream.read(&mut got).await.unwrap_or(0);
                let _ = tx.send(got[..n].to_vec());
                let _ = stream.write_all(b"pong").await;
            });
        }
    });
    Rig { proxy, events, received, _base: TempDir(base) }
}

fn allow(rules: &[&str]) -> NetworkPolicy {
    NetworkPolicy::Allow(rules.iter().map(|rule| DomainRule::parse(rule).unwrap()).collect())
}

impl Rig {
    /// Connects as the daemon would, for `job`, and sends `request`.
    async fn connect(&self, job: &str, request: &str) -> UnixStream {
        let mut stream = UnixStream::connect(self.proxy.dir().unwrap().join(SOCKET)).await.unwrap();
        stream.write_all(format!("job {job}\n{request}").as_bytes()).await.unwrap();
        stream
    }

    async fn logged(&mut self) -> NetRequest {
        match tokio::time::timeout(Duration::from_secs(5), self.events.recv()).await {
            Ok(Ok(SandboxEvent::NetRequest(entry))) => entry,
            other => panic!("expected a logged request, got {other:?}"),
        }
    }
}

/// Everything the proxy says until it closes; then the client closes its side too, as a real
/// one does once it has its response.
async fn answer(stream: &mut UnixStream) -> String {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut out)).await;
    let _ = stream.shutdown().await;
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads the proxy's `200` to a CONNECT, and nothing more.
async fn established(stream: &mut UnixStream) {
    let mut head = vec![0u8; 39];
    stream.read_exact(&mut head).await.unwrap();
    assert_eq!(head, b"HTTP/1.1 200 Connection Established\r\n\r\n");
}

fn blocked(entry: &NetRequest) -> &str {
    match &entry.result {
        NetResult::Blocked { reason } => reason,
        NetResult::Allowed => panic!("expected blocked: {entry:?}"),
    }
}

#[tokio::test]
async fn an_allowed_host_with_a_matching_server_name_is_tunnelled() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let mut client = rig.connect("j3", "CONNECT allowed.test:443 HTTP/1.1\r\n\r\n").await;
    established(&mut client).await;
    let hello = tls::sample_hello(Some("allowed.test"), 16384);
    client.write_all(&hello).await.unwrap();
    let got = rig.received.recv().await.unwrap();
    assert_eq!(got, hello, "upstream gets the ClientHello untouched");
    assert_eq!(answer(&mut client).await, "pong");
    let entry = rig.logged().await;
    assert_eq!((entry.host.as_str(), entry.port, &entry.result), ("allowed.test", 443, &NetResult::Allowed));
    assert_eq!(entry.job, Some(JobId::new("j3")));
    assert_eq!((entry.bytes_up, entry.bytes_down), (hello.len() as u64, 4));
}

#[tokio::test]
async fn another_host_is_blocked_and_logged() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let mut client = rig.connect("fg", "CONNECT other.test:443 HTTP/1.1\r\n\r\n").await;
    let reply = answer(&mut client).await;
    assert!(reply.starts_with("HTTP/1.1 403 Forbidden"), "{reply}");
    assert!(reply.contains("host other.test isn't allowed in this session; ask the user to allow it"), "{reply}");
    let entry = rig.logged().await;
    assert_eq!(blocked(&entry), "host other.test isn't allowed in this session; ask the user to allow it");
    assert!(rig.received.try_recv().is_err(), "nothing was sent upstream");
}

#[tokio::test]
async fn a_server_name_that_does_not_match_the_connect_host_is_refused() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let mut client = rig.connect("fg", "CONNECT allowed.test:443 HTTP/1.1\r\n\r\n").await;
    established(&mut client).await;
    client.write_all(&tls::sample_hello(Some("other.test"), 16384)).await.unwrap();
    assert_eq!(answer(&mut client).await, "", "the tunnel is closed");
    let entry = rig.logged().await;
    assert_eq!(blocked(&entry), "the TLS server name `other.test` doesn't match allowed.test");
    // Upstream was dialled but got nothing: no bytes of the hello reached it.
    assert_eq!(rig.received.recv().await.unwrap_or_default(), Vec::<u8>::new());
}

#[tokio::test]
async fn a_tunnel_must_carry_tls_with_a_server_name() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let mut client = rig.connect("fg", "CONNECT allowed.test:443 HTTP/1.1\r\n\r\n").await;
    established(&mut client).await;
    client.write_all(b"SSH-2.0-OpenSSH_9.8\r\n").await.unwrap();
    answer(&mut client).await;
    assert_eq!(blocked(&rig.logged().await), "a CONNECT tunnel must carry TLS");
    let mut client = rig.connect("fg", "CONNECT allowed.test:443 HTTP/1.1\r\n\r\n").await;
    established(&mut client).await;
    client.write_all(&tls::sample_hello(None, 16384)).await.unwrap();
    answer(&mut client).await;
    assert_eq!(blocked(&rig.logged().await), "the TLS hello names no server");
}

#[tokio::test]
async fn an_allowed_name_that_resolves_to_a_local_address_is_refused() {
    for policy in [allow(&["local.test"]), NetworkPolicy::Open] {
        let mut rig = rig(policy).await;
        let mut client = rig.connect("fg", "CONNECT local.test:443 HTTP/1.1\r\n\r\n").await;
        assert!(answer(&mut client).await.starts_with("HTTP/1.1 403"));
        assert_eq!(blocked(&rig.logged().await), "local.test resolves to 127.0.0.1, a local or private address");
    }
}

#[tokio::test]
async fn ip_literals_and_other_ports_are_refused() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let mut client = rig.connect("fg", "CONNECT 203.0.113.10:443 HTTP/1.1\r\n\r\n").await;
    assert!(answer(&mut client).await.starts_with("HTTP/1.1 403"));
    assert!(blocked(&rig.logged().await).contains("IP address"));
    let mut client = rig.connect("fg", "CONNECT allowed.test:22 HTTP/1.1\r\n\r\n").await;
    assert!(answer(&mut client).await.starts_with("HTTP/1.1 403"));
    assert!(blocked(&rig.logged().await).starts_with("port 22 isn't allowed for allowed.test"));
    // Even an allowed private IP stays unreachable.
    let mut rig = self::rig(allow(&["127.0.0.1"])).await;
    let mut client = rig.connect("fg", "CONNECT 127.0.0.1:443 HTTP/1.1\r\n\r\n").await;
    assert!(answer(&mut client).await.starts_with("HTTP/1.1 403"));
    assert!(blocked(&rig.logged().await).contains("local or private"));
}

#[tokio::test]
async fn plain_http_is_sent_on_with_connection_close_and_its_url_logged() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let request = "GET http://allowed.test/crates?q=1 HTTP/1.1\r\nProxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n";
    let mut client = rig.connect("fg", request).await;
    assert_eq!(answer(&mut client).await, "pong");
    let got = String::from_utf8(rig.received.recv().await.unwrap()).unwrap();
    assert_eq!(got, "GET /crates?q=1 HTTP/1.1\r\nHost: allowed.test\r\nAccept: */*\r\nConnection: close\r\n\r\n");
    let entry = rig.logged().await;
    assert_eq!(entry.method.as_deref(), Some("GET"));
    assert_eq!(entry.url.as_deref(), Some("http://allowed.test/crates?q=1"));
    assert_eq!(entry.result, NetResult::Allowed);
}

#[tokio::test]
async fn off_refuses_everything_and_logs_nothing() {
    let mut rig = rig(NetworkPolicy::Off).await;
    let mut client = rig.connect("fg", "CONNECT allowed.test:443 HTTP/1.1\r\n\r\n").await;
    assert!(answer(&mut client).await.starts_with("HTTP/1.1 403"));
    assert!(tokio::time::timeout(Duration::from_millis(200), rig.events.recv()).await.is_err(), "nothing logged");
}

#[tokio::test]
async fn open_allows_any_public_host_on_443_and_80() {
    let mut rig = rig(NetworkPolicy::Open).await;
    let mut client = rig.connect("fg", "GET http://other.test/ HTTP/1.1\r\n\r\n").await;
    assert_eq!(answer(&mut client).await, "pong");
    assert_eq!(rig.logged().await.result, NetResult::Allowed);
}

#[tokio::test]
async fn the_foreground_commands_requests_are_collected_for_its_observation() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    rig.proxy.begin_foreground();
    let mut background = rig.connect("j1", "CONNECT other.test:443 HTTP/1.1\r\n\r\n").await;
    let mut foreground = rig.connect("fg", "CONNECT other.test:443 HTTP/1.1\r\n\r\n").await;
    answer(&mut background).await;
    answer(&mut foreground).await;
    let collected = rig.proxy.end_foreground().await;
    assert_eq!(collected.len(), 1, "only the foreground command's request: {collected:?}");
    assert_eq!(collected[0].job, None);
    rig.logged().await;
    rig.logged().await;
}

#[tokio::test]
async fn a_policy_change_applies_to_the_next_connection() {
    let mut rig = rig(NetworkPolicy::Off).await;
    rig.proxy.set_policy(allow(&["allowed.test"]));
    let mut client = rig.connect("fg", "GET http://allowed.test/ HTTP/1.1\r\n\r\n").await;
    assert_eq!(answer(&mut client).await, "pong");
    assert_eq!(rig.logged().await.result, NetResult::Allowed);
}

/// Regression (PR #16 review): the client's own `Host`, and any request after the first, must
/// never reach the allowed host's address: on a shared address they name other sites.
#[tokio::test]
async fn plain_http_sends_only_the_checked_host_and_one_request() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let request = "GET http://allowed.test/ HTTP/1.1\r\nHost: evil.example\r\n\r\n\
                   GET /second HTTP/1.1\r\nHost: evil2.example\r\n\r\n";
    let mut client = rig.connect("fg", request).await;
    assert_eq!(answer(&mut client).await, "pong");
    let got = String::from_utf8(rig.received.recv().await.unwrap()).unwrap();
    assert_eq!(got, "GET / HTTP/1.1\r\nHost: allowed.test\r\nConnection: close\r\n\r\n");
    assert_eq!(rig.logged().await.host, "allowed.test");
}

#[tokio::test]
async fn a_request_body_is_sent_once_and_ambiguous_framing_is_refused() {
    let mut rig = rig(allow(&["allowed.test"])).await;
    let request = "POST http://allowed.test/up HTTP/1.1\r\nContent-Length: 5\r\n\r\nhelloGET /x HTTP/1.1\r\n\r\n";
    let mut client = rig.connect("fg", request).await;
    assert_eq!(answer(&mut client).await, "pong");
    let got = String::from_utf8(rig.received.recv().await.unwrap()).unwrap();
    assert!(got.ends_with("Connection: close\r\n\r\nhello"), "{got:?}");
    rig.logged().await;
    let smuggled =
        "POST http://allowed.test/ HTTP/1.1\r\nContent-Length: 4\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n";
    let mut client = rig.connect("fg", smuggled).await;
    assert!(answer(&mut client).await.starts_with("HTTP/1.1 400"));
    assert_eq!(blocked(&rig.logged().await), "a request with both Content-Length and Transfer-Encoding");
    assert!(rig.received.try_recv().is_err(), "nothing went upstream");
}
