//! The C libraries the native check knows: for each, which of its functions
//! return without waiting, which wait, and what the bounded lock's acquire
//! waits in. A module's `target triple` picks one ([`for_triple`]); a target
//! whose C library is not here is refused.
//!
//! "Wait" means: the call can keep the caller until another thread or the
//! outside world does something. A call that only takes a lock its own
//! implementation releases before returning, and that calls nothing back
//! (the allocator), is not one.
//!
//! Each list is drawn from that library's own implementation, so a group
//! shared between libraries is shared only where its reason holds for each.
//!
//! # `syscall`
//!
//! On Linux, glibc and bionic both, std makes some kernel calls through
//! `syscall(2)`: futexes for its locks, `getrandom`, `statx` and others. A
//! direct call to `syscall` whose number is a constant in the IR is read as
//! a call to an external named after the kernel call, as a path so that
//! `trusted` can name it: `syscall::SYS_futex::FUTEX_WAKE`,
//! `syscall::SYS_gettid` ([`syscall_name`]). The lists, the IO classes and
//! `trusted` use that name. A number the table does not know is
//! `syscall::nr_<n>`; a number or a futex operation that is not a constant
//! there is `unknown`; no list knows either, so the call is "cannot verify".
//! Any other reference to `syscall` stays the external `syscall`, which no
//! list knows either.

/// One C library: its no-wait list by group, each with why, the waits std
/// reaches through it (only how a finding is worded: anything off the no-wait
/// list is already refused), and what the bounded lock's acquire, a
/// `std::sync::Mutex` lock, may wait in. A trailing `*` matches a prefix.
pub(crate) struct Libc {
    pub(crate) name: &'static str,
    pub(crate) os: Os,
    pub(crate) no_wait: &'static [(&'static [&'static str], &'static str)],
    pub(crate) waits: &'static [&'static str],
    pub(crate) acquire_waits: &'static [&'static str],
}

/// Which OS's IO calls a library's names map to ([`super::io`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Os {
    Apple,
    Linux,
}

impl Libc {
    /// Whether an external is known not to wait.
    pub(crate) fn no_wait(&self, name: &str) -> bool {
        self.no_wait.iter().any(|(names, _)| listed(names, name))
    }

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

// Groups whose reason holds for every library here.

const PURE: &[&str] = &[
    "memcmp", "memcpy", "memmove", "memset", "memchr", "bcmp", "bzero", "strlen", "strnlen",
    "strcmp", "strncmp", "strchr", "strrchr",
];
const PURE_WHY: &str = "pure functions over memory the caller passes";

const MAPPINGS: (&[&str], &str) = (
    &["mmap", "mmap64", "munmap", "mprotect", "madvise"],
    "kernel calls that change this process's mappings and return",
);

const UNWIND_FRAME: (&[&str], &str) = (
    &[
        "_Unwind_GetIP", "_Unwind_GetIPInfo", "_Unwind_GetCFA", "_Unwind_GetLanguageSpecificData",
        "_Unwind_GetRegionStart", "_Unwind_GetDataRelBase", "_Unwind_GetTextRelBase",
        "_Unwind_SetGR", "_Unwind_SetIP", "_Unwind_DeleteException",
    ],
    "read or write the frame the unwinder hands a personality routine",
);

const SIGNALS: (&[&str], &str) = (
    &["sigaction", "sigaltstack", "sigemptyset", "sigaddset", "signal", "pthread_sigmask"],
    "set signal disposition or masks, and return",
);

const EXIT: (&[&str], &str) = (&["abort", "_exit"], "end the process without running anything else");

const PTHREAD_RELEASE: &[&str] = &[
    "pthread_mutex_unlock", "pthread_mutex_trylock", "pthread_mutex_init", "pthread_mutex_destroy",
    "pthread_mutexattr_*", "pthread_cond_signal", "pthread_cond_broadcast", "pthread_cond_init",
    "pthread_cond_destroy", "pthread_condattr_*", "pthread_rwlock_unlock",
    "pthread_rwlock_tryrdlock", "pthread_rwlock_trywrlock", "pthread_rwlock_init",
    "pthread_rwlock_destroy",
];

pub(crate) const APPLE: Libc = Libc {
    name: "Apple's libSystem",
    os: Os::Apple,
    no_wait: &[
        (
            &[
                "memcmp", "memcpy", "memmove", "memset", "memchr", "bcmp", "bzero", "strlen",
                "strnlen", "strcmp", "strncmp", "strchr", "strrchr", "strerror_r",
            ],
            PURE_WHY,
        ),
        (
            &[
                "malloc", "calloc", "realloc", "free", "posix_memalign", "aligned_alloc",
                "malloc_size", "malloc_usable_size", "valloc",
            ],
            "the system allocator: its locks guard only its own structures, are \
             released before it returns, and it calls nothing back",
        ),
        MAPPINGS,
        (
            &[
                "pthread_mutex_unlock", "pthread_mutex_trylock", "pthread_mutex_init",
                "pthread_mutex_destroy", "pthread_mutexattr_*", "pthread_cond_signal",
                "pthread_cond_broadcast", "pthread_cond_init", "pthread_cond_destroy",
                "pthread_condattr_*", "pthread_rwlock_unlock", "pthread_rwlock_tryrdlock",
                "pthread_rwlock_trywrlock", "pthread_rwlock_init", "pthread_rwlock_destroy",
                "os_unfair_lock_unlock", "os_unfair_lock_trylock", "dispatch_semaphore_signal",
                "dispatch_semaphore_create", "dispatch_release", "dispatch_retain",
                "dispatch_time", "__ulock_wake", "os_sync_wake_by_address_*",
            ],
            "release, wake, try, create or destroy a lock; none of them waits",
        ),
        (
            &[
                "pthread_self", "pthread_threadid_np", "pthread_get_stackaddr_np",
                "pthread_get_stacksize_np", "pthread_getspecific", "pthread_setspecific",
                "pthread_key_create", "pthread_attr_*", "pthread_detach", "pthread_setname_np",
                "_tlv_atexit", "__error",
            ],
            "read or set the calling thread's own state, or a thread's attributes",
        ),
        (
            &[
                "clock_gettime", "clock_gettime_nsec_np", "mach_absolute_time",
                "mach_continuous_time", "mach_timebase_info", "gettimeofday", "time",
                "getpid", "getppid", "getuid", "geteuid", "getgid", "getegid", "sysconf",
                "_NSGetArgc", "_NSGetArgv", "_NSGetEnviron", "_NSGetExecutablePath",
            ],
            "read a clock, an id or a process constant; none waits for anything to \
             happen",
        ),
        (
            &["CCRandomGenerateBytes", "arc4random", "arc4random_buf", "arc4random_uniform", "getentropy"],
            "kernel entropy, which on Apple platforms never blocks once the system \
             has booted",
        ),
        UNWIND_FRAME,
        SIGNALS,
        EXIT,
    ],
    waits: &[
        "pthread_mutex_lock",
        "pthread_cond_wait",
        "pthread_cond_timedwait",
        "pthread_cond_timedwait_relative_np",
        "pthread_rwlock_rdlock",
        "pthread_rwlock_wrlock",
        "pthread_join",
        "dispatch_semaphore_wait",
        "dispatch_sync",
        "dispatch_sync_f",
        "dispatch_group_wait",
        "os_unfair_lock_lock",
        "os_sync_wait_on_address",
        "__ulock_wait",
        "__ulock_wait2",
        "nanosleep",
        "usleep",
        "sleep",
        "mach_wait_until",
        "pause",
        "sigsuspend",
        "waitpid",
        "wait4",
        "poll",
        "select",
        "kevent",
        "flock",
        // Not a wait itself, but only a spin loop waiting on another thread calls it.
        "sched_yield",
    ],
    // The native `Mutex` is a `pthread_mutex_t`.
    acquire_waits: &["pthread_mutex_lock"],
};

/// The futex operations that only wake waiters or read, on either Linux
/// library: the kernel returns without waiting for anyone.
const FUTEX_WAKES: &[&str] = &["syscall::SYS_futex::FUTEX_WAKE", "syscall::SYS_futex::FUTEX_WAKE_BITSET"];

/// The kernel's own entropy call, made through the C library's wrapper or
/// `syscall`. It blocks only until the kernel's pool is first initialised,
/// which happens during boot, before an app can run.
const LINUX_ENTROPY: (&[&str], &str) = (
    &["getrandom", "syscall::SYS_getrandom"],
    "kernel entropy, which blocks only until the kernel first seeds its pool, \
     during boot and before any app runs",
);

/// The kernel's thread and CPU queries, the same call on either library.
const LINUX_THREAD_IDS: &[&str] = &["gettid", "syscall::SYS_gettid", "sched_getaffinity"];

/// Android's bionic. std uses futexes for its locks here, as on glibc, and
/// keeps thread-local destructors in a pthread key rather than registering
/// them with the C library.
///
/// Not on the list: `__system_property_get`, whose first read of a property
/// area opens and maps it under bionic's own lock, and `dl_iterate_phdr`,
/// which holds the linker's lock that `dlopen` holds while it runs a
/// library's constructors.
pub(crate) const BIONIC: Libc = Libc {
    name: "Android's bionic",
    os: Os::Linux,
    no_wait: &[
        (PURE, PURE_WHY),
        (
            // bionic's strerror_r copies from a fixed table; it has no locale.
            &["strerror_r", "__strerror_r", "gai_strerror"],
            "copy a fixed message for an error code; bionic has no message \
             catalogs to load",
        ),
        (
            &[
                "malloc", "calloc", "realloc", "free", "posix_memalign", "aligned_alloc",
                "memalign", "malloc_usable_size",
            ],
            "bionic's allocator (Scudo): its locks guard only its own structures, \
             are released before it returns, and it calls nothing back",
        ),
        MAPPINGS,
        (
            PTHREAD_RELEASE,
            "release, try, create or destroy a pthread lock; none of them waits",
        ),
        (FUTEX_WAKES, "wake a futex's waiters, which the kernel does without waiting"),
        (
            &[
                "pthread_self", "pthread_getspecific", "pthread_setspecific", "pthread_key_create",
                "pthread_key_delete", "pthread_attr_init", "pthread_attr_destroy",
                "pthread_attr_setstacksize", "pthread_attr_getstack", "pthread_attr_getguardsize",
                "pthread_detach", "__errno",
            ],
            "read or set the calling thread's own state, or a thread's attributes; \
             bionic's pthread keys are a fixed array claimed with atomics",
        ),
        (LINUX_THREAD_IDS, "ask the kernel for a thread's id or CPU set"),
        (
            &[
                "clock_gettime", "gettimeofday", "time", "getpid", "getppid", "getuid", "geteuid",
                "getgid", "getegid", "getauxval",
            ],
            "read a clock, an id or a process constant; none waits for anything to \
             happen",
        ),
        LINUX_ENTROPY,
        UNWIND_FRAME,
        SIGNALS,
        EXIT,
    ],
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
        "flock",
        "sched_yield",
        "syscall::SYS_futex::FUTEX_WAIT",
        "syscall::SYS_futex::FUTEX_WAIT_BITSET",
        "syscall::SYS_futex::FUTEX_LOCK_PI",
        "syscall::SYS_futex::FUTEX_LOCK_PI2",
        "syscall::SYS_futex::FUTEX_WAIT_REQUEUE_PI",
        "dl_iterate_phdr",
    ],
    // std's futex `Mutex`: a contended lock waits on the futex.
    acquire_waits: &["syscall::SYS_futex::FUTEX_WAIT_BITSET"],
};

/// glibc. std uses futexes for its locks here, and registers thread-local
/// destructors with `__cxa_thread_atexit_impl`.
///
/// Not on the list: `__xpg_strerror_r`, which translates its message through
/// gettext and can load a message catalog under gettext's lock; `sysconf`,
/// which reads `/sys` and `/proc` for some names; and `_Unwind_Backtrace`
/// and `_Unwind_FindEnclosingFunction`, which walk the loaded objects with
/// `dl_iterate_phdr`.
pub(crate) const GLIBC: Libc = Libc {
    name: "glibc",
    os: Os::Linux,
    no_wait: &[
        (PURE, PURE_WHY),
        (&["gai_strerror"], "return a fixed message for an error code"),
        (
            &[
                "malloc", "calloc", "realloc", "free", "posix_memalign", "aligned_alloc",
                "memalign", "malloc_usable_size", "valloc",
            ],
            "glibc's malloc: its arena locks guard only its own structures, are \
             released before it returns, and it calls nothing back",
        ),
        MAPPINGS,
        (
            PTHREAD_RELEASE,
            "release, try, create or destroy a pthread lock; none of them waits",
        ),
        (FUTEX_WAKES, "wake a futex's waiters, which the kernel does without waiting"),
        (
            &[
                "pthread_self", "pthread_getspecific", "pthread_setspecific", "pthread_key_create",
                "pthread_key_delete", "pthread_attr_init", "pthread_attr_destroy",
                "pthread_attr_setstacksize", "pthread_attr_getstack", "pthread_attr_getguardsize",
                "pthread_detach", "__errno_location",
            ],
            "read or set the calling thread's own state, or a thread's attributes; \
             glibc's pthread keys are a fixed array claimed with atomics",
        ),
        (LINUX_THREAD_IDS, "ask the kernel for a thread's id or CPU set"),
        (
            &[
                "clock_gettime", "gettimeofday", "time", "getpid", "getppid", "getuid", "geteuid",
                "getgid", "getegid", "getauxval",
            ],
            "read a clock (through the vDSO), an id or a process constant; none \
             waits for anything to happen",
        ),
        LINUX_ENTROPY,
        UNWIND_FRAME,
        SIGNALS,
        EXIT,
    ],
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
        "flock",
        "sched_yield",
        "syscall::SYS_futex::FUTEX_WAIT",
        "syscall::SYS_futex::FUTEX_WAIT_BITSET",
        "syscall::SYS_futex::FUTEX_LOCK_PI",
        "syscall::SYS_futex::FUTEX_LOCK_PI2",
        "syscall::SYS_futex::FUTEX_WAIT_REQUEUE_PI",
        // Both take the dynamic loader's lock, which `dlopen` holds while it
        // runs a library's constructors.
        "__cxa_thread_atexit_impl",
        "dl_iterate_phdr",
    ],
    // std's futex `Mutex`: a contended lock waits on the futex.
    acquire_waits: &["syscall::SYS_futex::FUTEX_WAIT_BITSET"],
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
        for libc in KNOWN {
            for n in ["syscall::unknown", "syscall::SYS_futex::unknown", "syscall::nr_98", "syscall"] {
                assert!(!libc.no_wait(n), "{} {n}", libc.name);
            }
        }
    }

    /// A futex wake does not wait and a futex wait does, on both Linux
    /// libraries, and the acquire may wait only in the wait.
    #[test]
    fn futex_operations_are_classified_on_both_linux_libraries() {
        for libc in [&BIONIC, &GLIBC] {
            assert!(libc.no_wait("syscall::SYS_futex::FUTEX_WAKE"), "{}", libc.name);
            for w in [
                "syscall::SYS_futex::FUTEX_WAIT",
                "syscall::SYS_futex::FUTEX_WAIT_BITSET",
                "syscall::SYS_futex::FUTEX_LOCK_PI",
                "syscall::SYS_futex::FUTEX_LOCK_PI2",
                "syscall::SYS_futex::FUTEX_WAIT_REQUEUE_PI",
            ] {
                assert!(libc.waits(w), "{} {w}", libc.name);
                assert!(!libc.no_wait(w), "{} {w}", libc.name);
            }
            assert_eq!(libc.acquire_waits, ["syscall::SYS_futex::FUTEX_WAIT_BITSET"]);
        }
    }

    #[test]
    fn each_library_names_its_own_errno() {
        assert!(APPLE.no_wait("__error"));
        assert!(!APPLE.no_wait("__errno"));
        assert!(BIONIC.no_wait("__errno"));
        assert!(!BIONIC.no_wait("__errno_location"));
        assert!(GLIBC.no_wait("__errno_location"));
        assert!(!GLIBC.no_wait("__errno"));
    }

    /// glibc's `strerror_r` translates through gettext; bionic's copies from
    /// a table.
    #[test]
    fn strerror_r_is_known_only_where_it_loads_nothing() {
        assert!(BIONIC.no_wait("strerror_r"));
        assert!(!GLIBC.no_wait("__xpg_strerror_r"));
        assert!(!GLIBC.no_wait("strerror_r"));
    }

    #[test]
    fn the_loader_lock_is_a_wait_on_glibc() {
        assert!(GLIBC.waits("__cxa_thread_atexit_impl"));
        assert!(!GLIBC.no_wait("__cxa_thread_atexit_impl"));
        assert!(GLIBC.waits("dl_iterate_phdr"));
        assert!(BIONIC.waits("dl_iterate_phdr"));
    }

    /// No name is both known not to wait and a wait in one library.
    #[test]
    fn no_list_contradicts_another() {
        for libc in KNOWN {
            for w in libc.waits {
                assert!(!libc.no_wait(w), "{}: `{w}`", libc.name);
            }
            for a in libc.acquire_waits {
                assert!(libc.waits(a), "{}: `{a}`", libc.name);
            }
        }
    }
}
