//! The network policy, in the words the agent's instructions use.

use nixxa_contracts::{NetworkAccess, NetworkPolicy};

/// One paragraph for the agent's instructions, rebuilt every step so it is always the current
/// policy. Only the user changes it: there is no tool for it.
pub(crate) fn instructions(access: &NetworkAccess) -> String {
    let rest = "Blocked requests show in the command's result with the reason. Only the user can change \
                the network policy; if a task needs a host that isn't allowed, ask them to allow it.";
    match access {
        NetworkAccess::Unrestricted => {
            "Network: not restricted. This place isn't isolated, so commands reach whatever this machine can.".into()
        }
        NetworkAccess::Controlled(NetworkPolicy::Off) => format!(
            "Network: off. Commands can't reach the internet (installing packages or fetching anything fails). {rest}"
        ),
        NetworkAccess::Controlled(NetworkPolicy::Allow(rules)) => {
            let hosts: Vec<String> = rules.iter().map(ToString::to_string).collect();
            format!(
                "Network: only these hosts are reachable, through a proxy that commands use automatically \
                 (HTTPS and plain HTTP; ports 443 and 80 unless shown): {}. Local and private addresses \
                 never are. {rest}",
                hosts.join(", ")
            )
        }
        NetworkAccess::Controlled(NetworkPolicy::Open) => format!(
            "Network: open to public hosts through a proxy that logs every request (HTTPS and plain HTTP on \
             ports 443 and 80). Local and private addresses and raw IP addresses are blocked. {rest}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use nixxa_contracts::DomainRule;

    use super::*;

    #[test]
    fn each_policy_is_described() {
        let rules = vec![DomainRule::parse("crates.io").unwrap(), DomainRule::parse("ssh.example.com:22").unwrap()];
        let allow = instructions(&NetworkAccess::Controlled(NetworkPolicy::Allow(rules)));
        assert!(allow.contains("crates.io, ssh.example.com:22"), "{allow}");
        assert!(allow.contains("ask them to allow it"), "{allow}");
        assert!(instructions(&NetworkAccess::Controlled(NetworkPolicy::Off)).starts_with("Network: off."));
        assert!(instructions(&NetworkAccess::Controlled(NetworkPolicy::Open)).starts_with("Network: open"));
        assert!(instructions(&NetworkAccess::Unrestricted).contains("not restricted"));
    }
}
