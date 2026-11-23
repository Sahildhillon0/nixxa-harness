//! The sandbox's side of the network proxy: `127.0.0.1:3128`, forwarded to the host's proxy
//! through the Unix socket mounted at `/run/nixxa/proxy.sock`.
//!
//! The port is bound at start-up, so no command can take it first; while the network is off,
//! connections to it are closed at once and commands get no proxy variables, as before Stage 6.
//! Each forwarded connection starts with `job <who>`: the job of the process that connected,
//! found through `/proc` (its socket's inode, the process holding it, its nearest supervisor).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream, UnixStream};

use crate::jobs::Supervisors;

/// Where commands find the proxy.
pub(crate) const PROXY: &str = "127.0.0.1:3128";
/// The host proxy's socket, as mounted inside.
const SOCKET: &str = "/run/nixxa/proxy.sock";

/// The variables that send commands' traffic to the proxy: the usual ones in both cases, and
/// npm's own (it reads its config, not always these).
pub(crate) fn env() -> Vec<(String, String)> {
    let url = format!("http://{PROXY}");
    let mut env = Vec::new();
    for name in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "npm_config_proxy", "npm_config_https_proxy"] {
        env.push((name.to_string(), url.clone()));
        env.push((name.to_ascii_lowercase(), url.clone()));
    }
    for name in ["NO_PROXY", "no_proxy"] {
        env.push((name.to_string(), "localhost,127.0.0.1,::1".to_string()));
    }
    env.sort();
    env.dedup();
    env
}

/// Forwards `127.0.0.1:3128` to the host proxy while `on`.
pub(crate) struct Forwarder {
    on: Arc<AtomicBool>,
}

impl Forwarder {
    /// Binds the port now (off until [`Forwarder::set`]).
    pub(crate) async fn start(supervisors: Supervisors, on: bool) -> std::io::Result<Self> {
        let listener = TcpListener::bind(PROXY).await?;
        let on = Arc::new(AtomicBool::new(on));
        let accepting = Arc::clone(&on);
        tokio::spawn(async move {
            loop {
                let Ok((stream, peer)) = listener.accept().await else { continue };
                if accepting.load(Ordering::Relaxed) {
                    tokio::spawn(forward(stream, peer, supervisors.clone()));
                }
                // Off: dropped, so the connection closes at once.
            }
        });
        Ok(Self { on })
    }

    pub(crate) fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }

    pub(crate) fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }
}

async fn forward(mut stream: TcpStream, peer: SocketAddr, supervisors: Supervisors) {
    let owner = attribute(peer.port(), &supervisors).unwrap_or_else(|| "-".to_string());
    let Ok(mut upstream) = UnixStream::connect(SOCKET).await else { return };
    if upstream.write_all(format!("job {owner}\n").as_bytes()).await.is_err() {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
}

/// The job of the process whose socket has local port `port` and talks to the proxy.
fn attribute(port: u16, supervisors: &Supervisors) -> Option<String> {
    let inode = socket_inode(port)?;
    let target = format!("socket:[{inode}]");
    let me = rustix::process::getpid().as_raw_nonzero().get();
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
        if pid == me {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        let holds =
            fds.flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|link| link.as_os_str() == target.as_str()));
        if holds {
            return supervisors.owner_of(pid);
        }
    }
    None
}

/// The inode of the client socket `127.0.0.1:port` → the proxy port, from `/proc/self/net/tcp`.
fn socket_inode(port: u16) -> Option<u64> {
    let proxy_port = PROXY.rsplit_once(':').and_then(|(_, p)| p.parse::<u16>().ok())?;
    let table = std::fs::read_to_string("/proc/self/net/tcp").ok()?;
    table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let port_of = |addr: &str| addr.rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok());
        let (local, remote, inode) = (fields.get(1)?, fields.get(2)?, fields.get(9)?);
        (port_of(local)? == port && port_of(remote)? == proxy_port).then(|| inode.parse().ok()).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_get_both_spellings_and_no_proxy_for_loopback() {
        let env = env();
        for name in ["HTTP_PROXY", "https_proxy", "ALL_PROXY", "npm_config_https_proxy"] {
            assert!(env.contains(&(name.to_string(), "http://127.0.0.1:3128".to_string())), "{name}");
        }
        assert!(env.contains(&("no_proxy".to_string(), "localhost,127.0.0.1,::1".to_string())));
    }
}
