//! seccomp deny-list applied to the daemon at start-up. Every command inherits it.

use std::collections::BTreeMap;

use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch, apply_filter, sock_filter,
};

/// Applies the seccomp deny-list to the current process. Returns `true` when it is installed.
pub(crate) fn apply() -> bool {
    let arch = match TargetArch::try_from(std::env::consts::ARCH) {
        Ok(arch) => arch,
        Err(_) => return false,
    };

    // Block x32 syscalls (nr & 0x40000000) on x86_64, which would otherwise
    // bypass the deny-list because they arrive with AUDIT_ARCH_X86_64.
    #[cfg(target_arch = "x86_64")]
    {
        let x32_filter: BpfProgram = vec![
            sock_filter { code: 0x20, jt: 0, jf: 0, k: 0 },
            sock_filter { code: 0x45, jt: 0, jf: 1, k: 0x40000000 },
            sock_filter { code: 0x06, jt: 0, jf: 0, k: 0x00050001 },
            sock_filter { code: 0x06, jt: 0, jf: 0, k: 0x7fff0000 },
        ];
        if apply_filter(&x32_filter).is_err() {
            return false;
        }
    }

    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // Process introspection / memory access.
    deny(&mut rules, libc::SYS_ptrace);
    deny(&mut rules, libc::SYS_process_vm_readv);
    deny(&mut rules, libc::SYS_process_vm_writev);

    // Key management.
    deny(&mut rules, libc::SYS_keyctl);
    deny(&mut rules, libc::SYS_add_key);
    deny(&mut rules, libc::SYS_request_key);

    // BPF, perf, userfaultfd.
    deny(&mut rules, libc::SYS_bpf);
    deny(&mut rules, libc::SYS_perf_event_open);
    deny(&mut rules, libc::SYS_userfaultfd);

    // Mount / filesystem reconfiguration.
    deny(&mut rules, libc::SYS_mount);
    deny(&mut rules, libc::SYS_umount2);
    deny(&mut rules, libc::SYS_pivot_root);
    deny(&mut rules, libc::SYS_move_mount);
    deny(&mut rules, libc::SYS_open_tree);
    deny(&mut rules, libc::SYS_fsopen);
    deny(&mut rules, libc::SYS_fsmount);
    deny(&mut rules, libc::SYS_fsconfig);

    // Namespace manipulation.
    deny(&mut rules, libc::SYS_unshare);
    deny(&mut rules, libc::SYS_setns);

    // clone with any namespace flag is denied.
    let ns_bits = [
        libc::CLONE_NEWNS,
        libc::CLONE_NEWUTS,
        libc::CLONE_NEWIPC,
        libc::CLONE_NEWUSER,
        libc::CLONE_NEWPID,
        libc::CLONE_NEWNET,
        libc::CLONE_NEWCGROUP,
        libc::CLONE_NEWTIME,
    ];
    let mut clone_rules = Vec::new();
    for bit in ns_bits {
        if let Ok(cond) =
            SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::MaskedEq(bit as u64), bit as u64)
        {
            if let Ok(rule) = SeccompRule::new(vec![cond]) {
                clone_rules.push(rule);
            }
        }
    }
    if !clone_rules.is_empty() {
        rules.insert(libc::SYS_clone, clone_rules);
    }

    // kexec / modules.
    deny(&mut rules, libc::SYS_kexec_load);
    #[cfg(target_arch = "x86_64")]
    deny(&mut rules, 320);
    #[cfg(target_arch = "aarch64")]
    deny(&mut rules, 294);
    deny(&mut rules, libc::SYS_init_module);
    deny(&mut rules, libc::SYS_finit_module);
    deny(&mut rules, libc::SYS_delete_module);

    // ioctl TIOCSTI / TIOCLINUX.
    let mut ioctl_rules = Vec::new();
    for cmd in [libc::TIOCSTI, libc::TIOCLINUX] {
        if let Ok(cond) = SeccompCondition::new(1, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, cmd) {
            if let Ok(rule) = SeccompRule::new(vec![cond]) {
                ioctl_rules.push(rule);
            }
        }
    }
    if !ioctl_rules.is_empty() {
        rules.insert(libc::SYS_ioctl, ioctl_rules);
    }

    // io_uring.
    deny(&mut rules, libc::SYS_io_uring_setup);
    deny(&mut rules, libc::SYS_io_uring_enter);
    deny(&mut rules, libc::SYS_io_uring_register);

    // personality: deny anything but the default (0) and the query (0xffffffff).
    if let (Ok(c1), Ok(c2)) = (
        SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, 0),
        SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, 0xffffffff),
    ) {
        if let Ok(rule) = SeccompRule::new(vec![c1, c2]) {
            rules.entry(libc::SYS_personality).or_default().push(rule);
        }
    }

    // socket for families other than AF_UNIX.
    if let Ok(cond) = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, libc::AF_UNIX as u64) {
        if let Ok(rule) = SeccompRule::new(vec![cond]) {
            rules.entry(libc::SYS_socket).or_default().push(rule);
        }
    }

    // Build and install the main filter (EPERM on match).
    let filter = match SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32), arch) {
        Ok(f) => f,
        Err(_) => return false,
    };
    let bpf: BpfProgram = match filter.try_into() {
        Ok(b) => b,
        Err(_) => return false,
    };
    if apply_filter(&bpf).is_err() {
        return false;
    }

    // clone3 -> ENOSYS so libc falls back to clone.
    let mut rules3: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules3.insert(libc::SYS_clone3, Vec::new());
    let filter3 =
        match SeccompFilter::new(rules3, SeccompAction::Allow, SeccompAction::Errno(libc::ENOSYS as u32), arch) {
            Ok(f) => f,
            Err(_) => return false,
        };
    let bpf3: BpfProgram = match filter3.try_into() {
        Ok(b) => b,
        Err(_) => return false,
    };
    if apply_filter(&bpf3).is_err() {
        return false;
    }

    true
}

fn deny(rules: &mut BTreeMap<i64, Vec<SeccompRule>>, sys: i64) {
    // An empty rule vector matches every call to this syscall.
    rules.insert(sys, Vec::new());
}
