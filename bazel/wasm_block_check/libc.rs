//! The C libraries the native check knows: for each, the functions std and a
//! dependency reach that wait. A module's `target triple` picks one
//! ([`for_triple`]); a target whose C library is not here is refused.
//!
//! Each list is drawn from that library's own implementation.
//!
//! # `syscall`
//!
//! On Linux, glibc and bionic both, std makes some kernel calls through
//! `syscall(2)`: futexes for its locks, `getrandom`, `statx` and others. A
//! direct call to `syscall` whose number is a constant in the IR is read as
//! a call to an external named after the kernel call, as a path:
//! `syscall::SYS_futex::FUTEX_WAIT_BITSET`, `syscall::SYS_gettid`
//! ([`syscall_name`]). The lists and the IO classes use that name. A number
//! the table does not know is `syscall::nr_<n>`; a number or a futex
//! operation that is not a constant there is `unknown`. Any other reference
//! to `syscall` stays the external `syscall`.

/// One C library: the functions whose purpose is to block the calling thread
/// until something else happens. std reaches its locks', condition
/// variables' and thread parking's; the rest are there for dependencies that
/// call the C library themselves. The readiness waits (`poll`, `select`,
/// `kevent`, `epoll_wait`) are here rather than in an IO category because
/// they are what wasip1's `poll_oneoff` is there, which that check counts as
/// a wait. A trailing `*` matches a prefix.
pub(crate) struct Libc {
    pub(crate) name: &'static str,
    pub(crate) os: Os,
    pub(crate) waits: &'static [&'static str],
}

/// Which OS's IO calls a library's names map to ([`super::io`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Os {
    Apple,
    Linux,
}

impl Libc {
    pub(crate) fn waits(&self, name: &str) -> bool {
        listed(self.waits, name)
    }
}

pub(crate) fn listed(patterns: &[&str], name: &str) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == *p,
    })
}

/// The library a module's target triple links against, if the check knows it.
pub(crate) fn for_triple(triple: &str) -> Option<&'static Libc> {
    if triple.contains("-apple-") {
        Some(&APPLE)
    } else if triple.contains("-linux-android") {
        Some(&BIONIC)
    } else if triple.contains("-linux-gnu") {
        Some(&GLIBC)
    } else {
        None
    }
}

/// The libraries the check knows, for a refusal to name.
pub(crate) const KNOWN: &[&Libc] = &[&APPLE, &BIONIC, &GLIBC];

pub(crate) const APPLE: Libc = Libc {
    name: "Apple's libSystem",
    os: Os::Apple,
    waits: &[
        "pthread_mutex_lock",
        "pthread_cond_wait",
        "pthread_cond_timedwait",
        "dispatch_semaphore_wait",
        "pthread_rwlock_rdlock",
        "pthread_rwlock_wrlock",
        "pthread_join",
        "os_unfair_lock_lock",
        "os_sync_wait_on_address",
        "os_sync_wait_on_address_with_timeout",
        "os_sync_wait_on_address_with_deadline",
        "__ulock_wait",
        "__ulock_wait2",
        "dispatch_sync",
        "dispatch_group_wait",
        "semaphore_wait",
        "sem_wait",
        "nanosleep",
        "usleep",
        "sleep",
        "pause",
        "sigsuspend",
        "waitpid",
        "wait4",
        "poll",
        "select",
        "select$DARWIN_EXTSN",
        "pselect",
        "pselect$DARWIN_EXTSN",
        "kevent",
        "kevent64",
        "kevent_qos",
    ],
};

/// Android's bionic. std uses futexes for its locks here, as on glibc.
pub(crate) const BIONIC: Libc = Libc {
    name: "Android's bionic",
    os: Os::Linux,
    waits: &[
        "pthread_mutex_lock",
        "pthread_cond_wait",
        "pthread_cond_timedwait",
        "pthread_rwlock_rdlock",
        "pthread_rwlock_wrlock",
        "pthread_join",
        "sem_wait",
        "sem_timedwait",
        "nanosleep",
        "clock_nanosleep",
        "usleep",
        "sleep",
        "pause",
        "sigsuspend",
        "waitpid",
        "waitid",
        "wait4",
        "poll",
        "ppoll",
        "select",
        "epoll_wait",
        "epoll_pwait",
        "syscall::SYS_futex::FUTEX_WAIT",
        "syscall::SYS_futex::FUTEX_WAIT_BITSET",
        "syscall::SYS_futex::FUTEX_LOCK_PI",
        "syscall::SYS_futex::FUTEX_LOCK_PI2",
        "syscall::SYS_futex::FUTEX_WAIT_REQUEUE_PI",
    ],
};

/// glibc. std uses futexes for its locks here.
pub(crate) const GLIBC: Libc = Libc {
    name: "glibc",
    os: Os::Linux,
    waits: &[
        "pthread_mutex_lock",
        "pthread_cond_wait",
        "pthread_cond_timedwait",
        "pthread_cond_clockwait",
        "pthread_rwlock_rdlock",
        "pthread_rwlock_wrlock",
        "pthread_join",
        "sem_wait",
        "sem_timedwait",
        "nanosleep",
        "clock_nanosleep",
        "usleep",
        "sleep",
        "pause",
        "sigsuspend",
        "waitpid",
        "waitid",
        "wait4",
        "poll",
        "ppoll",
        "select",
        "pselect",
        "epoll_wait",
        "epoll_pwait",
        "syscall::SYS_futex::FUTEX_WAIT",
        "syscall::SYS_futex::FUTEX_WAIT_BITSET",
        "syscall::SYS_futex::FUTEX_LOCK_PI",
        "syscall::SYS_futex::FUTEX_LOCK_PI2",
        "syscall::SYS_futex::FUTEX_WAIT_REQUEUE_PI",
    ],
};

/// The kernel calls std makes through `syscall`, by number, on the
/// architectures the check knows. x86_64 has its own table; aarch64 uses
/// the kernel's generic one.
const SYSCALLS: &[(&str, u64, u64)] = &[
    // (name, x86_64, aarch64)
    ("SYS_futex", 202, 98),
    ("SYS_gettid", 186, 178),
    ("SYS_getrandom", 318, 278),
    ("SYS_statx", 332, 291),
    ("SYS_copy_file_range", 326, 285),
    ("SYS_splice", 275, 76),
    ("SYS_preadv", 295, 69),
    ("SYS_pwritev", 296, 70),
    ("SYS_accept4", 288, 242),
    ("SYS_sched_getaffinity", 204, 123),
    ("SYS_pidfd_open", 434, 434),
    ("SYS_pidfd_send_signal", 424, 424),
    ("SYS_pidfd_getfd", 438, 438),
    ("SYS_clone3", 435, 435),
    ("SYS_openat2", 437, 437),
];

/// `futex(2)`'s operations by number, after the private and clock flags.
const FUTEX_OPS: &[(u64, &str)] = &[
    (0, "FUTEX_WAIT"),
    (1, "FUTEX_WAKE"),
    (3, "FUTEX_REQUEUE"),
    (4, "FUTEX_CMP_REQUEUE"),
    (5, "FUTEX_WAKE_OP"),
    (6, "FUTEX_LOCK_PI"),
    (7, "FUTEX_UNLOCK_PI"),
    (8, "FUTEX_TRYLOCK_PI"),
    (9, "FUTEX_WAIT_BITSET"),
    (10, "FUTEX_WAKE_BITSET"),
    (11, "FUTEX_WAIT_REQUEUE_PI"),
    (12, "FUTEX_CMP_REQUEUE_PI"),
    (13, "FUTEX_LOCK_PI2"),
];

const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;

/// The external a direct call to `syscall` on a Linux target is read as, from
/// the call's argument list (what follows `@syscall(` on the line). `arch` is
/// the triple's first component.
pub(crate) fn syscall_name(arch: &str, args: &str) -> String {
    let args = call_args(args);
    let Some(number) = args.first().and_then(|a| constant(a)) else {
        return "syscall::unknown".into();
    };
    let column = match arch {
        "x86_64" => |s: &(&str, u64, u64)| s.1,
        "aarch64" => |s: &(&str, u64, u64)| s.2,
        _ => return format!("syscall::nr_{number}"),
    };
    let Some((name, _, _)) = SYSCALLS.iter().find(|s| column(s) == number) else {
        return format!("syscall::nr_{number}");
    };
    if *name != "SYS_futex" {
        return format!("syscall::{name}");
    }
    let op = args
        .get(2)
        .and_then(|a| constant(a))
        .map(|op| op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME))
        .and_then(|op| FUTEX_OPS.iter().find(|(n, _)| *n == op))
        .map_or("unknown", |(_, name)| name);
    format!("syscall::SYS_futex::{op}")
}

/// A call's arguments up to its closing parenthesis, split at the commas
/// outside any nested parentheses or brackets.
fn call_args(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' if depth == 0 => {
                out.push(s[start..i].trim());
                return out;
            }
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// An argument's value when it is an integer constant (`i64 noundef 202`),
/// as the unsigned bits of its type's width.
fn constant(arg: &str) -> Option<u64> {
    let mut words = arg.split_whitespace();
    let ty = words.next()?;
    let bits: u32 = ty.strip_prefix('i')?.parse().ok()?;
    let value: i128 = words.last()?.parse().ok()?;
    let mask = if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 };
    Some((value as u64) & mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_triple_names_its_library() {
        assert_eq!(for_triple("arm64-apple-macosx11.0.0").unwrap().name, APPLE.name);
        assert_eq!(for_triple("arm64-apple-ios17.0.0").unwrap().name, APPLE.name);
        assert_eq!(for_triple("aarch64-unknown-linux-android").unwrap().name, BIONIC.name);
        assert_eq!(for_triple("x86_64-unknown-linux-android").unwrap().name, BIONIC.name);
        assert_eq!(for_triple("x86_64-unknown-linux-gnu").unwrap().name, GLIBC.name);
        assert_eq!(for_triple("aarch64-unknown-linux-gnu").unwrap().name, GLIBC.name);
        assert!(for_triple("x86_64-unknown-linux-musl").is_none());
        assert!(for_triple("x86_64-pc-windows-msvc").is_none());
    }

    #[test]
    fn a_futex_call_is_named_by_its_operation() {
        let wake = "i64 noundef 202, ptr noundef nonnull align 4 %0, i32 noundef 129, i32 noundef 1)";
        assert_eq!(syscall_name("x86_64", wake), "syscall::SYS_futex::FUTEX_WAKE");
        let wait = "i64 noundef 98, ptr noundef nonnull align 4 %6, i32 noundef 137, i32 noundef -1, \
                    ptr noundef %43, ptr noundef null, i32 noundef -1)";
        assert_eq!(syscall_name("aarch64", wait), "syscall::SYS_futex::FUTEX_WAIT_BITSET");
        // A futex word that is a constant expression, with its own commas
        // and parentheses, does not move the operation.
        let gep = "i64 noundef 202, ptr noundef nonnull align 4 getelementptr inbounds nuw (i8, ptr @S, i64 8), \
                   i32 noundef 129, i32 noundef 2147483647)";
        assert_eq!(syscall_name("x86_64", gep), "syscall::SYS_futex::FUTEX_WAKE");
    }

    #[test]
    fn a_number_is_read_in_its_architectures_table() {
        assert_eq!(syscall_name("x86_64", "i64 noundef 318, ptr %1, i64 %2, i32 4)"), "syscall::SYS_getrandom");
        assert_eq!(syscall_name("aarch64", "i64 noundef 278, ptr %1, i64 %2, i32 4)"), "syscall::SYS_getrandom");
        // 98 is futex on aarch64 and nothing the table knows on x86_64.
        assert_eq!(syscall_name("x86_64", "i64 noundef 98)"), "syscall::nr_98");
    }

    #[test]
    fn what_is_not_a_constant_is_unknown() {
        assert_eq!(syscall_name("x86_64", "i64 noundef %n, ptr %1)"), "syscall::unknown");
        let op = "i64 noundef 202, ptr noundef %f, i32 noundef %op, i32 noundef 1)";
        assert_eq!(syscall_name("x86_64", op), "syscall::SYS_futex::unknown");
        assert_eq!(syscall_name("riscv64", "i64 noundef 98)"), "syscall::nr_98");
    }

    /// A futex wait is a wait on both Linux libraries, and a wake is not.
    #[test]
    fn futex_operations_are_classified_on_both_linux_libraries() {
        for libc in [&BIONIC, &GLIBC] {
            assert!(!libc.waits("syscall::SYS_futex::FUTEX_WAKE"), "{}", libc.name);
            for w in [
                "syscall::SYS_futex::FUTEX_WAIT",
                "syscall::SYS_futex::FUTEX_WAIT_BITSET",
                "syscall::SYS_futex::FUTEX_LOCK_PI",
                "syscall::SYS_futex::FUTEX_LOCK_PI2",
                "syscall::SYS_futex::FUTEX_WAIT_REQUEUE_PI",
            ] {
                assert!(libc.waits(w), "{} {w}", libc.name);
            }
        }
    }
}
