//! The network proxy of one [`BwrapSandbox`](crate::bwrap::BwrapSandbox), on the host.
//!
//! The sandbox's network namespace has nothing but its own loopback, so a command can only
//! reach the network through this proxy. It listens on a Unix socket in a private directory
//! (mode 0700) that is bind-mounted read-only into the sandbox at `/run/nixxa`; inside, the
//! daemon forwards `127.0.0.1:3128` to it, and commands get `HTTP_PROXY` and friends pointing
//! there. The daemon starts each connection with a line naming the job that made it.
//!
//! * HTTPS is `CONNECT host:port` only. The host is checked against the policy, resolved here,
//!   and the tunnel is opened only if the TLS ClientHello's server name is that host. Nothing
//!   is decrypted and no certificate is injected.
//! * Plain HTTP is absolute-form (`GET http://host/path`), sent on with `Connection: close`.
//! * Names are resolved on the host, and only public addresses are dialled: an allowed name that
//!   resolves to loopback, a private network, link-local, CGNAT or multicast is refused. IP
//!   literals need a rule naming that exact address.
//! * Every request, allowed or blocked, is logged as a [`NetRequest`]: in the foreground
//!   command's observation, and as a [`SandboxEvent::NetRequest`]. Under `Off` nothing is
//!   allowed and nothing is logged.
//!
//! The job a request is attributed to is what the daemon says. A command that connects to the
//! socket directly could claim another job: attribution is for the log, never for the policy.

mod http;
mod rules;
mod tls;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::{Notify, Semaphore, broadcast};

use nixxa_contracts::{JobId, NetRequest, NetResult, NetworkPolicy, SandboxEvent, Timestamp};

use self::rules::Host;
use self::tls::Hello;

/// The socket's file name in the proxy's directory.
const SOCKET: &str = "proxy.sock";
/// Connections handled at once; more wait.
const MAX_CONNECTIONS: usize = 256;
/// How long the daemon's preamble and the request head may take.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the TLS ClientHello may take once the tunnel is open.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long connecting upstream may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a client may keep sending after upstream has finished.
const AFTER_UPSTREAM: Duration = Duration::from_secs(5);
/// How long a finished foreground command waits for its last requests to be logged.
const SETTLE: Duration = Duration::from_secs(1);

/// The proxy: its directory, its policy, and its listener once serving.
pub(crate) struct Proxy {
    /// The private directory holding the socket, or why there is none.
    dir: Result<PathBuf, String>,
    shared: Arc<Shared>,
    listener: Mutex<Option<std::os::unix::net::UnixListener>>,
    task: Mutex<Option<tokio::task::AbortHandle>>,
}

/// Who made a request, as the daemon says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    /// The agent's foreground command.
    Foreground,
    /// A background job (later, the terminal too).
    Job(JobId),
    /// The daemon couldn't tell.
    Unknown,
}

struct Shared {
    policy: RwLock<NetworkPolicy>,
    /// Names resolved without asking the system resolver (tests).
    hosts: HashMap<String, Vec<IpAddr>>,
    events: broadcast::Sender<SandboxEvent>,
    /// The foreground command's requests, while one runs.
    foreground: Mutex<Option<Vec<NetRequest>>>,
    /// Foreground connections not yet logged.
    open_foreground: AtomicUsize,
    settled: Notify,
    slots: Arc<Semaphore>,
    /// Where to dial instead (tests).
    #[cfg(test)]
    redirect: Mutex<HashMap<SocketAddr, SocketAddr>>,
}

impl Drop for Proxy {
    fn drop(&mut self) {
        if let Some(task) = self.task.lock().ok().and_then(|mut task| task.take()) {
            task.abort();
        }
        if let Ok(dir) = &self.dir {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

impl Proxy {
    /// A proxy with its socket in a new private directory under `base`. A proxy that can't bind
    /// keeps the reason; [`Proxy::usable`] reports it and the sandbox stays offline.
    pub(crate) fn new(
        base: &Path,
        hosts: HashMap<String, Vec<IpAddr>>,
        events: broadcast::Sender<SandboxEvent>,
    ) -> Self {
        let bound = bind(base);
        let (dir, listener) = match bound {
            Ok((dir, listener)) => (Ok(dir), Some(listener)),
            Err(why) => (Err(why), None),
        };
        let shared = Arc::new(Shared {
            policy: RwLock::new(NetworkPolicy::Off),
            hosts,
            events,
            foreground: Mutex::new(None),
            open_foreground: AtomicUsize::new(0),
            settled: Notify::new(),
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            #[cfg(test)]
            redirect: Mutex::new(HashMap::new()),
        });
        Self { dir, shared, listener: Mutex::new(listener), task: Mutex::new(None) }
    }

    /// The directory to mount at `/run/nixxa`, if the proxy could bind.
    pub(crate) fn dir(&self) -> Option<&Path> {
        self.dir.as_deref().ok()
    }

    /// Why the network can't be turned on, if it can't.
    pub(crate) fn usable(&self) -> Result<(), String> {
        self.dir.as_ref().map(|_| ()).map_err(|why| format!("the network proxy can't start: {why}"))
    }

    /// Starts accepting connections (once; needs a tokio runtime).
    pub(crate) fn serve(&self) -> Result<(), String> {
        let Some(listener) = self.listener.lock().ok().and_then(|mut l| l.take()) else { return Ok(()) };
        let listener =
            UnixListener::from_std(listener).map_err(|err| format!("the network proxy can't start: {err}"))?;
        let shared = Arc::clone(&self.shared);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let Ok(slot) = Arc::clone(&shared.slots).acquire_owned().await else { return };
                let shared = Arc::clone(&shared);
                tokio::spawn(async move {
                    shared.handle(stream).await;
                    drop(slot);
                });
            }
        });
        if let Ok(mut slot) = self.task.lock() {
            *slot = Some(task.abort_handle());
        }
        Ok(())
    }

    pub(crate) fn policy(&self) -> NetworkPolicy {
        self.shared.policy.read().map(|p| p.clone()).unwrap_or_default()
    }

    /// Takes effect for the next connection.
    pub(crate) fn set_policy(&self, policy: NetworkPolicy) {
        if let Ok(mut current) = self.shared.policy.write() {
            *current = policy;
        }
    }

    /// Starts collecting the foreground command's requests.
    pub(crate) fn begin_foreground(&self) {
        if let Ok(mut slot) = self.shared.foreground.lock() {
            *slot = Some(Vec::new());
        }
    }

    /// The foreground command's requests, once its connections are logged (at most
    /// [`SETTLE`] after it ends).
    pub(crate) async fn end_foreground(&self) -> Vec<NetRequest> {
        let wait = async {
            while self.shared.open_foreground.load(Ordering::SeqCst) > 0 {
                let settled = self.shared.settled.notified();
                if self.shared.open_foreground.load(Ordering::SeqCst) == 0 {
                    break;
                }
                settled.await;
            }
        };
        let _ = tokio::time::timeout(SETTLE, wait).await;
        self.shared.foreground.lock().ok().and_then(|mut slot| slot.take()).unwrap_or_default()
    }

    #[cfg(test)]
    fn redirect(&self, from: SocketAddr, to: SocketAddr) {
        self.shared.redirect.lock().unwrap().insert(from, to);
    }
}

/// Makes the private directory and binds the socket in it.
fn bind(base: &Path) -> Result<(PathBuf, std::os::unix::net::UnixListener), String> {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = base.join(format!("nixxa-net-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::DirBuilder::new().mode(0o700).create(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let listener = std::os::unix::net::UnixListener::bind(dir.join(SOCKET))
        .and_then(|listener| listener.set_nonblocking(true).map(|()| listener))
        .map_err(|err| format!("{}: {err}", dir.display()));
    match listener {
        Ok(listener) => Ok((dir, listener)),
        Err(why) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(why)
        }
    }
}

/// One request being handled, for its log line.
struct Request {
    owner: Owner,
    started: Instant,
    time: Timestamp,
    host: String,
    port: u16,
    method: Option<String>,
    url: Option<String>,
}

impl Shared {
    async fn handle(self: Arc<Self>, mut client: UnixStream) {
        let Ok(Ok(Some((owner, buf)))) = tokio::time::timeout(HEAD_TIMEOUT, read_preamble(&mut client)).await else {
            return;
        };
        // Counted from here so a finished foreground command waits for this line.
        let foreground = owner == Owner::Foreground;
        if foreground {
            self.open_foreground.fetch_add(1, Ordering::SeqCst);
        }
        self.serve_request(owner, client, buf).await;
        if foreground && self.open_foreground.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.settled.notify_waiters();
        }
    }

    async fn serve_request(&self, owner: Owner, mut client: UnixStream, mut buf: Vec<u8>) {
        let head = match tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client, &mut buf)).await {
            Ok(Ok(head)) => head,
            Ok(Err(why)) => return refuse(&mut client, 400, &why).await,
            Err(_) => return,
        };
        let policy = self.policy.read().map(|p| p.clone()).unwrap_or_default();
        if policy == NetworkPolicy::Off {
            // Nothing is logged under Off.
            return refuse(&mut client, 403, "the network is off in this session").await;
        }
        let body = buf.split_off(head.len);
        let time = Timestamp(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64));
        if head.method == "CONNECT" {
            let Some((host, port)) = http::authority(&head.target, None) else {
                return refuse(&mut client, 400, "CONNECT needs host:port").await;
            };
            let req = Request { owner, started: Instant::now(), time, host, port, method: None, url: None };
            self.tunnel(&policy, req, client, body).await;
        } else if let Some((host, port, path)) = http::absolute(&head.target) {
            let req = Request {
                owner,
                started: Instant::now(),
                time,
                host,
                port,
                method: Some(head.method.clone()),
                url: Some(head.target.clone()),
            };
            self.forward(&policy, req, client, &head, &path, body).await;
        } else {
            refuse(&mut client, 400, "this proxy takes CONNECT host:port, or an absolute http:// URL").await;
        }
    }

    /// CONNECT: check, resolve, connect, then the ClientHello's server name must be the host.
    async fn tunnel(&self, policy: &NetworkPolicy, mut req: Request, mut client: UnixStream, early: Vec<u8>) {
        let (host, mut upstream) = match self.open(policy, &mut req).await {
            Ok(opened) => opened,
            Err((status, why)) => return self.block(&mut client, req, status, why).await,
        };
        if client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await.is_err() {
            return;
        }
        let mut hello = early;
        let check = tokio::time::timeout(HELLO_TIMEOUT, async {
            loop {
                match tls::client_hello(&hello) {
                    Hello::Incomplete if hello.len() < tls::MAX_HELLO => {}
                    Hello::Incomplete => return Hello::NotTls,
                    decided => return decided,
                }
                let mut chunk = [0u8; 4096];
                match client.read(&mut chunk).await {
                    Ok(0) | Err(_) => return Hello::Incomplete,
                    Ok(n) => hello.extend_from_slice(&chunk[..n]),
                }
            }
        })
        .await;
        let refused = match (check, &host) {
            (Ok(Hello::Sni(name)), Host::Name(asked)) if name == *asked => None,
            (Ok(Hello::Sni(name)), _) => Some(format!("the TLS server name `{name}` doesn't match {}", req.host)),
            (Ok(Hello::NoSni), Host::Ip(_)) => None,
            (Ok(Hello::NoSni), Host::Name(_)) => Some("the TLS hello names no server".into()),
            (Ok(Hello::NotTls), _) => Some("a CONNECT tunnel must carry TLS".into()),
            // The client gave up before saying anything: nothing was sent anywhere.
            (Ok(Hello::Incomplete), _) | (Err(_), _) => return,
        };
        if let Some(why) = refused {
            // The 200 is already out: just close.
            return self.log(req, 0, 0, NetResult::Blocked { reason: why });
        }
        if upstream.write_all(&hello).await.is_err() {
            return self.log(req, 0, 0, NetResult::Allowed);
        }
        let (up, down) = splice(&mut client, &mut upstream).await;
        self.log(req, hello.len() as u64 + up, down, NetResult::Allowed);
    }

    /// Plain HTTP: check, resolve, connect, then exactly one request: the head with `Host`
    /// written from the checked URL, and its one body. Nothing the client sends after that body
    /// is read, so a second request (for another host on the same address) never goes out.
    async fn forward(
        &self,
        policy: &NetworkPolicy,
        mut req: Request,
        mut client: UnixStream,
        head: &http::Head,
        path: &str,
        early: Vec<u8>,
    ) {
        let framing = match http::framing(head) {
            Ok(framing) => framing,
            Err(why) => {
                refuse(&mut client, 400, &why).await;
                return self.log(req, 0, 0, NetResult::Blocked { reason: why });
            }
        };
        let (host, upstream) = match self.open(policy, &mut req).await {
            Ok(opened) => opened,
            Err((status, why)) => return self.block(&mut client, req, status, why).await,
        };
        let upstream_head = http::upstream_head(head, &host.shown(), req.port, path);
        let (mut client_read, mut client_write) = client.into_split();
        let (mut upstream_read, mut upstream_write) = upstream.into_split();
        if upstream_write.write_all(&upstream_head).await.is_err() {
            return self.log(req, 0, 0, NetResult::Allowed);
        }
        // The response is copied while the body is sent, so a client waiting for
        // `100 Continue` gets it.
        let send = http::forward_body(&mut client_read, &mut upstream_write, early, framing);
        let receive = async {
            let n = tokio::io::copy(&mut upstream_read, &mut client_write).await.unwrap_or(0);
            let _ = client_write.shutdown().await;
            n
        };
        tokio::pin!(send, receive);
        let (body, down) = tokio::select! {
            sent = &mut send => match sent {
                Ok(body) => (body, receive.await),
                // A broken body: nothing more is sent, and the connection ends.
                Err(_) => (0, 0),
            },
            // The server answered and closed before the body was all sent.
            down = &mut receive => (0, down),
        };
        self.log(req, upstream_head.len() as u64 + body, down, NetResult::Allowed);
    }

    /// Checks the policy, resolves, and connects to the first public address that answers.
    /// The error is an HTTP status and a reason; 403 means blocked.
    async fn open(&self, policy: &NetworkPolicy, req: &mut Request) -> Result<(Host, TcpStream), (u16, String)> {
        let host = Host::parse(&req.host).ok_or((400, format!("`{}` isn't a host", req.host)))?;
        req.host = host.shown();
        rules::decide(policy, &host, req.port).map_err(|why| (403, why))?;
        let addrs = self.resolve(&host, req.port).await.map_err(|why| (502, why))?;
        let public: Vec<SocketAddr> = addrs.iter().copied().filter(|addr| rules::is_public(addr.ip())).collect();
        if public.is_empty() {
            let shown: Vec<String> = addrs.iter().map(|addr| addr.ip().to_string()).collect();
            return Err((403, format!("{} resolves to {}, a local or private address", req.host, shown.join(", "))));
        }
        let mut last = String::new();
        for addr in public {
            let dial = self.dial_address(addr);
            match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dial)).await {
                Ok(Ok(stream)) => return Ok((host, stream)),
                Ok(Err(err)) => last = err.to_string(),
                Err(_) => last = "timed out".into(),
            }
        }
        Err((502, format!("couldn't connect to {}: {last}", req.host)))
    }

    async fn resolve(&self, host: &Host, port: u16) -> Result<Vec<SocketAddr>, String> {
        match host {
            Host::Ip(ip) => Ok(vec![SocketAddr::new(*ip, port)]),
            Host::Name(name) => {
                if let Some(ips) = self.hosts.get(name) {
                    return Ok(ips.iter().map(|ip| SocketAddr::new(*ip, port)).collect());
                }
                let found = tokio::net::lookup_host((name.as_str(), port))
                    .await
                    .map_err(|err| format!("couldn't resolve {name}: {err}"))?;
                Ok(found.collect())
            }
        }
    }

    #[cfg(not(test))]
    fn dial_address(&self, addr: SocketAddr) -> SocketAddr {
        addr
    }

    #[cfg(test)]
    fn dial_address(&self, addr: SocketAddr) -> SocketAddr {
        self.redirect.lock().unwrap().get(&addr).copied().unwrap_or(addr)
    }

    /// Answers with `status` and logs the request: as blocked for a 403, as allowed (it failed
    /// upstream) otherwise.
    async fn block(&self, client: &mut UnixStream, req: Request, status: u16, why: String) {
        refuse(client, status, &why).await;
        let result = if status == 403 { NetResult::Blocked { reason: why } } else { NetResult::Allowed };
        self.log(req, 0, 0, result);
    }

    fn log(&self, req: Request, bytes_up: u64, bytes_down: u64, result: NetResult) {
        let job = match &req.owner {
            Owner::Job(job) => Some(job.clone()),
            Owner::Foreground | Owner::Unknown => None,
        };
        let entry = NetRequest {
            time: req.time,
            job,
            host: req.host,
            port: req.port,
            method: req.method,
            url: req.url,
            bytes_up,
            bytes_down,
            duration_ms: req.started.elapsed().as_millis() as u64,
            result,
        };
        if req.owner == Owner::Foreground {
            if let Ok(mut slot) = self.foreground.lock() {
                if let Some(list) = slot.as_mut() {
                    list.push(entry.clone());
                }
            }
        }
        let _ = self.events.send(SandboxEvent::NetRequest(entry));
    }
}

/// The daemon's first line, `job <who>`: `fg` for the foreground command, `-` when it couldn't
/// tell, else a job id. Returns it and whatever followed it.
async fn read_preamble(stream: &mut UnixStream) -> std::io::Result<Option<(Owner, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 512];
    while !buf.contains(&b'\n') {
        if buf.len() > 256 {
            return Ok(None);
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let end = buf.iter().position(|b| *b == b'\n').unwrap_or(0);
    let line = String::from_utf8_lossy(&buf[..end]).into_owned();
    let rest = buf.split_off(end + 1);
    let owner = match line.strip_prefix("job ") {
        Some("fg") => Owner::Foreground,
        Some("-") | None => Owner::Unknown,
        Some(job) => Owner::Job(JobId::new(job)),
    };
    Ok(Some((owner, rest)))
}

async fn read_head(stream: &mut UnixStream, buf: &mut Vec<u8>) -> Result<http::Head, String> {
    loop {
        if let Some(head) = http::head(buf)? {
            return Ok(head);
        }
        let mut chunk = [0u8; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return Err("the request ended early".into()),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn refuse(stream: &mut UnixStream, status: u16, why: &str) {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        _ => "Bad Gateway",
    };
    let body = format!("blocked by Nixxa: {why}\n");
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Copies both ways; returns (client → upstream, upstream → client). Once upstream is done,
/// the client gets [`AFTER_UPSTREAM`] to finish too, so a client that never closes its side
/// can't keep the connection (and its log line) open forever.
async fn splice<A, B>(client: &mut A, upstream: &mut B) -> (u64, u64)
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    let up = async {
        let n = tokio::io::copy(&mut client_read, &mut upstream_write).await.unwrap_or(0);
        let _ = upstream_write.shutdown().await;
        n
    };
    let down = async {
        let n = tokio::io::copy(&mut upstream_read, &mut client_write).await.unwrap_or(0);
        let _ = client_write.shutdown().await;
        n
    };
    tokio::pin!(up, down);
    tokio::select! {
        sent = &mut up => (sent, down.await),
        received = &mut down => (tokio::time::timeout(AFTER_UPSTREAM, up).await.unwrap_or(0), received),
    }
}

#[cfg(test)]
mod tests;
