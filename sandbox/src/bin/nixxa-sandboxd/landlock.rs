//! Landlock rules. The daemon applies [`apply`] to itself at start-up, so every process in the
//! sandbox inherits it; the supervisor of each of the agent's commands then adds
//! [`confine_job`], a second layer without the terminal devices (D-066).

use landlock::{
    ABI, Access, AccessFs, BitFlags, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetError, RulesetStatus,
    path_beneath_rules,
};

/// Where the network proxy's socket is mounted.
const PROXY_DIR: &str = "/run/nixxa";
/// The Landlock version the rules are written for.
const ABI: ABI = ABI::V9;
/// System paths: read and execute.
const SYSTEM: [&str; 8] = ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/.nixxa"];
/// Device nodes commands read and write.
const DEVICES: [&str; 6] = ["/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty"];
/// Full access.
const WORK: [&str; 3] = ["/workspace", "/tmp", "/dev/shm"];
/// The user's terminal's home (start-up files, history): theirs only.
pub(crate) const TERMINAL_HOME: &str = "/nixxa-home";
/// The PTY multiplexer and the terminals it makes (bubblewrap's `--dev` gives the sandbox its
/// own `devpts` instance; `/dev/ptmx` links to `/dev/pts/ptmx`).
const PTY: [&str; 2] = ["/dev/ptmx", "/dev/pts"];

/// Applies the daemon's Landlock policy to the current process. Returns `true` when it is
/// enforced. It allows opening terminals, for the user's terminal.
pub(crate) fn apply() -> bool {
    restrict(true).is_ok_and(|status| status != RulesetStatus::NotEnforced)
}

/// Adds the layer for one of the agent's commands: the daemon's rules without the terminal
/// devices or the terminal's home, and `/dev` itself listable but not readable as a tree. So no command of the
/// agent's can open the user's terminal (`/dev/pts/N`) to read what is typed or write into it,
/// or make a terminal of its own. Being a nested domain also keeps it from `ptrace`-level
/// access (`/proc/<pid>/fd`, `environ`) to processes outside it, the user's shell included.
pub(crate) fn confine_job() -> Result<(), String> {
    match restrict(false) {
        Ok(RulesetStatus::NotEnforced) => Err("Landlock is not enforced".into()),
        Ok(_) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

fn restrict(pty: bool) -> Result<RulesetStatus, RulesetError> {
    // Filesystem only. TCP is left alone on purpose (D-045): the sandbox's network namespace
    // has nothing but its own loopback, and commands need it for local servers and tests.
    let mut created = Ruleset::default().handle_access(AccessFs::from_all(ABI))?.create()?;
    created = created.add_rules(path_beneath_rules(SYSTEM, AccessFs::from_read(ABI)))?;
    created = created.add_rules(path_beneath_rules(["/proc"], AccessFs::from_read(ABI)))?;
    if pty {
        created = created.add_rules(path_beneath_rules(["/dev"], AccessFs::from_read(ABI)))?;
        let terminal: BitFlags<AccessFs> = AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::IoctlDev;
        created = created.add_rules(path_beneath_rules(PTY, terminal))?;
        created = created.add_rules(path_beneath_rules([TERMINAL_HOME], AccessFs::from_all(ABI)))?;
    } else {
        created = created.add_rules(path_beneath_rules(["/dev"], AccessFs::ReadDir))?;
    }
    created = created.add_rules(path_beneath_rules(DEVICES, AccessFs::from_all(ABI)))?;
    created = created.add_rules(path_beneath_rules(WORK, AccessFs::from_all(ABI)))?;
    // Connecting to the network proxy's socket, and nothing else there: no reading or listing
    // `/run/nixxa`, no creating sockets in it. Only kernels with Landlock ABI 9 restrict
    // connecting to a Unix socket at all; on older ones this right isn't handled (D-064).
    if std::path::Path::new(PROXY_DIR).is_dir() {
        created = created.add_rules(path_beneath_rules([PROXY_DIR], AccessFs::ResolveUnix))?;
    }
    Ok(created.restrict_self()?.ruleset)
}
