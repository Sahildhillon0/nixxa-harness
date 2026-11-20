//! Landlock rules applied to the daemon at start-up. Every command inherits them.

use landlock::{
    ABI, Access, AccessFs, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetError, RulesetStatus, path_beneath_rules,
};

/// Applies the Landlock policy to the current process. Returns `true` when it is enforced.
pub(crate) fn apply() -> bool {
    apply_inner().unwrap_or(false)
}

fn apply_inner() -> Result<bool, RulesetError> {
    let abi = ABI::V9;

    // Filesystem only. TCP is left alone on purpose (D-045): the sandbox's network namespace
    // has nothing but its own loopback, and commands need it for local servers and tests.
    let mut created = Ruleset::default().handle_access(AccessFs::from_all(abi))?.create()?;

    // Read + execute on the system paths.
    let read_exec = ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/etc", "/proc", "/dev", "/.nixxa"];
    created = created.add_rules(path_beneath_rules(&read_exec, AccessFs::from_read(abi)))?;

    // Read + write on the device nodes commands need.
    let rw_files = ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/tty"];
    created = created.add_rules(path_beneath_rules(&rw_files, AccessFs::from_all(abi)))?;

    // Full access to the workspace, /tmp and /dev/shm.
    let full = ["/workspace", "/tmp", "/dev/shm"];
    created = created.add_rules(path_beneath_rules(&full, AccessFs::from_all(abi)))?;

    let status = created.restrict_self()?;
    Ok(status.ruleset != RulesetStatus::NotEnforced)
}
