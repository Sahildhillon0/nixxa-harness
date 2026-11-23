//! What a sandbox's commands may reach on the network.

use std::fmt;
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

/// The network policy of a sandbox. Every session starts with [`NetworkPolicy::Off`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", content = "rules", rename_all = "snake_case")]
pub enum NetworkPolicy {
    /// No network at all.
    #[default]
    Off,
    /// Only these hosts.
    Allow(Vec<DomainRule>),
    /// Every public address. Requests are still logged, and local and private addresses are
    /// still refused.
    Open,
}

/// What a sandbox reports about its network.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "access", content = "policy", rename_all = "snake_case")]
pub enum NetworkAccess {
    /// The sandbox enforces this policy;
    /// [`Sandbox::set_network`](crate::Sandbox::set_network) changes it.
    Controlled(NetworkPolicy),
    /// The sandbox can't restrict the network (it isn't isolated): commands reach whatever the
    /// host can.
    Unrestricted,
}

impl Default for NetworkAccess {
    fn default() -> Self {
        Self::Controlled(NetworkPolicy::Off)
    }
}

/// One allowed destination: an exact host (`crates.io`), every subdomain of one
/// (`*.githubusercontent.com`, which doesn't match the bare domain), or an IP address. Without
/// ports, ports 443 and 80 are allowed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DomainRule {
    /// The host pattern, lower-case, without a trailing dot.
    pub host: String,
    /// The allowed ports; empty means 443 and 80.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<u16>,
}

/// Why a [`DomainRule`] can't be made from some text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadRule(pub String);

impl fmt::Display for BadRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BadRule {}

impl DomainRule {
    /// A rule from what the user typed: `host`, `*.domain`, an IP address, each optionally with
    /// `:port`. Lower-cased; a trailing dot is dropped.
    pub fn parse(text: &str) -> Result<Self, BadRule> {
        let text = text.trim();
        // An IPv6 address has colons of its own: only `[addr]:port` carries a port.
        let (host, port) = match text.strip_prefix('[') {
            Some(rest) => {
                let (addr, after) = rest.split_once(']').ok_or_else(|| BadRule(format!("`{text}`: missing `]`")))?;
                let port = match after {
                    "" => None,
                    after => Some(after.strip_prefix(':').ok_or_else(|| BadRule(format!("`{text}`: bad port")))?),
                };
                (addr, port)
            }
            None if text.matches(':').count() > 1 => (text, None),
            None => match text.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (text, None),
            },
        };
        let ports = match port {
            None => Vec::new(),
            Some(port) => match port.parse::<u16>() {
                Ok(port) if port > 0 => vec![port],
                _ => return Err(BadRule(format!("`{text}`: `{port}` isn't a port"))),
            },
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if let Ok(ip) = host.parse::<IpAddr>() {
            return Ok(Self { host: ip.to_string(), ports });
        }
        let name = host.strip_prefix("*.").unwrap_or(&host);
        let valid_label = |label: &str| {
            !label.is_empty()
                && label.len() <= 63
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        };
        if name.is_empty() || name.len() > 253 || !name.split('.').all(valid_label) {
            return Err(BadRule(format!("`{text}` isn't a host name, `*.domain` or an IP address")));
        }
        if host.starts_with("*.") && !name.contains('.') {
            return Err(BadRule(format!("`{text}` would allow a whole top-level domain")));
        }
        Ok(Self { host, ports })
    }
}

impl fmt::Display for DomainRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ipv6 = self.host.contains(':');
        match self.ports.as_slice() {
            [] => f.write_str(&self.host),
            ports => {
                let host = if ipv6 { format!("[{}]", self.host) } else { self.host.clone() };
                let ports: Vec<String> = ports.iter().map(u16::to_string).collect();
                write!(f, "{host}:{}", ports.join(","))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(host: &str, ports: &[u16]) -> DomainRule {
        DomainRule { host: host.into(), ports: ports.to_vec() }
    }

    #[test]
    fn rules_parse_hosts_wildcards_ips_and_ports() {
        assert_eq!(DomainRule::parse(" Crates.IO. ").unwrap(), rule("crates.io", &[]));
        assert_eq!(DomainRule::parse("*.githubusercontent.com").unwrap(), rule("*.githubusercontent.com", &[]));
        assert_eq!(DomainRule::parse("example.com:8443").unwrap(), rule("example.com", &[8443]));
        assert_eq!(DomainRule::parse("1.2.3.4").unwrap(), rule("1.2.3.4", &[]));
        assert_eq!(DomainRule::parse("2001:db8::1").unwrap(), rule("2001:db8::1", &[]));
        assert_eq!(DomainRule::parse("[2001:db8::1]:22").unwrap(), rule("2001:db8::1", &[22]));
        assert_eq!(rule("2001:db8::1", &[22]).to_string(), "[2001:db8::1]:22");
        assert_eq!(rule("crates.io", &[]).to_string(), "crates.io");
    }

    #[test]
    fn bad_rules_say_why() {
        for bad in ["", "*", "*.com", "exa mple.com", "-bad.com", "a..b", "host:0", "host:99999", "host:x", "[::1"] {
            assert!(DomainRule::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_policy_round_trips() {
        for policy in [NetworkPolicy::Off, NetworkPolicy::Open, NetworkPolicy::Allow(vec![rule("crates.io", &[])])] {
            let json = serde_json::to_string(&policy).unwrap();
            assert_eq!(serde_json::from_str::<NetworkPolicy>(&json).unwrap(), policy, "{json}");
        }
        let access = NetworkAccess::Controlled(NetworkPolicy::Allow(vec![rule("a.b", &[1])]));
        let json = serde_json::to_string(&access).unwrap();
        assert_eq!(serde_json::from_str::<NetworkAccess>(&json).unwrap(), access);
    }
}
