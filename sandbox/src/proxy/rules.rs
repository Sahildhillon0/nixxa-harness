//! Whether a destination is allowed, before anything is resolved or connected.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use nixxa_contracts::{DomainRule, NetworkPolicy};

/// Ports allowed for a rule that names none, and under `Open`.
const DEFAULT_PORTS: [u16; 2] = [443, 80];

/// A host as asked for: a name, or an IP literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Host {
    Name(String),
    Ip(IpAddr),
}

impl Host {
    /// Lower-cased, without a trailing dot or IPv6 brackets.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.strip_prefix('[').and_then(|t| t.strip_suffix(']')).unwrap_or(text);
        if let Ok(ip) = text.parse::<IpAddr>() {
            return Some(Self::Ip(ip));
        }
        let name = text.trim_end_matches('.').to_ascii_lowercase();
        let valid = !name.is_empty()
            && name.len() <= 253
            && name.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
        valid.then_some(Self::Name(name))
    }

    /// How it is written in a log line.
    pub(crate) fn shown(&self) -> String {
        match self {
            Self::Name(name) => name.clone(),
            Self::Ip(ip) => ip.to_string(),
        }
    }
}

/// Decides whether `host:port` is allowed under `policy`. The error is the reason, in words the
/// agent and the user can act on.
pub(crate) fn decide(policy: &NetworkPolicy, host: &Host, port: u16) -> Result<(), String> {
    let shown = host.shown();
    match policy {
        NetworkPolicy::Off => Err("the network is off in this session; ask the user to allow it".into()),
        NetworkPolicy::Open => match host {
            Host::Ip(_) => Err(ip_refused(&shown)),
            Host::Name(_) if DEFAULT_PORTS.contains(&port) => Ok(()),
            Host::Name(_) => Err(port_refused(&shown, port)),
        },
        NetworkPolicy::Allow(rules) => {
            let matching: Vec<&DomainRule> = rules.iter().filter(|rule| matches(rule, host)).collect();
            if matching.is_empty() {
                return Err(match host {
                    Host::Ip(_) => ip_refused(&shown),
                    Host::Name(_) => {
                        format!("host {shown} isn't allowed in this session; ask the user to allow it")
                    }
                });
            }
            let allowed = |rule: &&DomainRule| {
                if rule.ports.is_empty() { DEFAULT_PORTS.contains(&port) } else { rule.ports.contains(&port) }
            };
            if matching.iter().any(allowed) { Ok(()) } else { Err(port_refused(&shown, port)) }
        }
    }
}

fn ip_refused(ip: &str) -> String {
    format!("{ip} is an IP address, which is only reachable when the user allows that exact address")
}

fn port_refused(host: &str, port: u16) -> String {
    format!("port {port} isn't allowed for {host} (only 443 and 80, unless the user allows another)")
}

fn matches(rule: &DomainRule, host: &Host) -> bool {
    match host {
        Host::Ip(ip) => rule.host.parse::<IpAddr>().is_ok_and(|allowed| allowed == *ip),
        Host::Name(name) => match rule.host.strip_prefix("*.") {
            Some(domain) => name.strip_suffix(domain).is_some_and(|sub| sub.ends_with('.') && sub.len() > 1),
            None => *name == rule.host,
        },
    }
}

/// Whether `ip` is an address on the public internet: not loopback, private (RFC 1918, ULA),
/// link-local, CGNAT, multicast, broadcast, reserved or unspecified. An IPv4 address inside
/// IPv6 (mapped, compatible, NAT64) is judged as the IPv4 address.
pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => match embedded_v4(v6) {
            Some(v4) => is_public_v4(v4),
            None => is_public_v6(v6),
        },
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(a == 0 // "this network", including the unspecified address
        || a == 10
        || a == 127
        || (a == 100 && (64..128).contains(&b)) // CGNAT
        || (a == 169 && b == 254) // link-local
        || (a == 172 && (16..32).contains(&b))
        || (a == 192 && b == 168)
        || (a == 192 && b == 0 && ip.octets()[2] == 0) // IETF protocol assignments
        || (a == 198 && (b == 18 || b == 19)) // benchmarking
        || a >= 224) // multicast, reserved, broadcast
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    !(ip.is_unspecified()
        || ip.is_loopback()
        || (first & 0xfe00) == 0xfc00 // unique local
        || (first & 0xffc0) == 0xfe80 // link-local
        || (first & 0xffc0) == 0xfec0 // site-local (deprecated)
        || (first & 0xff00) == 0xff00) // multicast
}

/// The IPv4 address carried by an IPv4-mapped, IPv4-compatible or NAT64 (64:ff9b::/96) address.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    let v4 = || Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    match s {
        [0, 0, 0, 0, 0, 0xffff, ..] | [0x64, 0xff9b, 0, 0, 0, 0, ..] => Some(v4()),
        [0, 0, 0, 0, 0, 0, hi, _] if hi != 0 => Some(v4()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(rules: &[&str]) -> NetworkPolicy {
        NetworkPolicy::Allow(rules.iter().map(|r| DomainRule::parse(r).unwrap()).collect())
    }

    fn name(n: &str) -> Host {
        Host::parse(n).unwrap()
    }

    #[test]
    fn exact_hosts_and_wildcards() {
        let policy = allow(&["crates.io", "*.githubusercontent.com"]);
        assert_eq!(decide(&policy, &name("crates.io"), 443), Ok(()));
        assert_eq!(decide(&policy, &name("CRATES.IO."), 80), Ok(()));
        assert!(decide(&policy, &name("index.crates.io"), 443).is_err(), "exact means exact");
        assert_eq!(decide(&policy, &name("objects.githubusercontent.com"), 443), Ok(()));
        assert!(decide(&policy, &name("githubusercontent.com"), 443).is_err(), "*. needs a subdomain");
        assert!(decide(&policy, &name("evilgithubusercontent.com"), 443).is_err());
        let reason = decide(&policy, &name("example.com"), 443).unwrap_err();
        assert_eq!(reason, "host example.com isn't allowed in this session; ask the user to allow it");
    }

    #[test]
    fn ports_are_443_and_80_unless_a_rule_says_otherwise() {
        let policy = allow(&["github.com", "ssh.example.com:22"]);
        assert!(decide(&policy, &name("github.com"), 22).unwrap_err().starts_with("port 22 isn't allowed"));
        assert_eq!(decide(&policy, &name("ssh.example.com"), 22), Ok(()));
        assert!(decide(&policy, &name("ssh.example.com"), 443).is_err());
        assert!(decide(&NetworkPolicy::Open, &name("example.com"), 8080).is_err());
        assert_eq!(decide(&NetworkPolicy::Open, &name("example.com"), 443), Ok(()));
    }

    #[test]
    fn ip_literals_need_that_exact_address() {
        let ip = Host::parse("1.1.1.1").unwrap();
        assert!(decide(&NetworkPolicy::Open, &ip, 443).unwrap_err().contains("IP address"));
        assert!(decide(&allow(&["crates.io"]), &ip, 443).is_err());
        assert_eq!(decide(&allow(&["1.1.1.1"]), &ip, 443), Ok(()));
        assert_eq!(decide(&allow(&["2606:4700::1111"]), &Host::parse("[2606:4700::1111]").unwrap(), 443), Ok(()));
    }

    #[test]
    fn off_refuses_everything() {
        assert!(decide(&NetworkPolicy::Off, &name("crates.io"), 443).is_err());
    }

    #[test]
    fn local_and_private_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "127.1.2.3",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "240.0.0.1",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.1.1.1",
            "64:ff9b::a00:1",
            "::127.0.0.1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} must not count as public");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "172.32.0.1", "100.128.0.1", "2606:4700::1111", "::ffff:1.1.1.1"] {
            assert!(is_public(ip.parse().unwrap()), "{ip} is public");
        }
    }
}
