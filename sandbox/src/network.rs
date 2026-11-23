//! Network presets: the hosts a package manager really uses, so the user can allow one in a
//! keystroke. Each list was checked by running the tool through the proxy with the network
//! open and logging every host it contacted (2026-11-23).

use nixxa_contracts::DomainRule;

/// A named set of hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    /// What the user sees, e.g. `crates.io`.
    pub name: &'static str,
    /// The hosts it allows (exact names; ports 443 and 80).
    pub hosts: &'static [&'static str],
}

impl Preset {
    /// The rules it adds.
    pub fn rules(&self) -> Vec<DomainRule> {
        self.hosts.iter().map(|host| DomainRule { host: (*host).to_string(), ports: Vec::new() }).collect()
    }
}

/// The presets offered in the network settings.
pub const PRESETS: [Preset; 4] = [
    // `cargo fetch` / `build`: the sparse index and the downloads; `crates.io` for search and
    // the API.
    Preset { name: "crates.io", hosts: &["crates.io", "index.crates.io", "static.crates.io"] },
    // The registry serves both the metadata and the tarballs.
    Preset { name: "npm", hosts: &["registry.npmjs.org"] },
    // The simple index, then the files.
    Preset { name: "PyPI", hosts: &["pypi.org", "files.pythonhosted.org"] },
    // Clones and API (github.com), archives (codeload), raw files, and release assets (which now
    // redirect to release-assets.githubusercontent.com; older links use objects.).
    Preset {
        name: "GitHub",
        hosts: &[
            "github.com",
            "codeload.github.com",
            "raw.githubusercontent.com",
            "objects.githubusercontent.com",
            "release-assets.githubusercontent.com",
        ],
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_host_is_a_valid_rule() {
        for preset in PRESETS {
            for rule in preset.rules() {
                assert_eq!(DomainRule::parse(&rule.host), Ok(rule.clone()), "{}", preset.name);
            }
        }
    }
}
