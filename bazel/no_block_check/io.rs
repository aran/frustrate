//! IO categories: one vocabulary for the IO a claimed body may not do, mapped
//! per target to the calls that do it.
//!
//! A project names a category (`--io files`); each check expands it to its own
//! target's calls. On wasip1 that is `wasi_snapshot_preview1` imports, a closed
//! set the WASI ABI fixes, so the mapping there is the whole category. On a
//! native target it is C library functions, an open set, so the mapping is the
//! functions std reaches for that category and the ones a dependency calling
//! the C library directly is likely to: a C library that does its own IO
//! behind a function of its own is not seen through. One map serves Apple
//! targets and one Linux, glibc and bionic both: which IO a call does is fixed
//! by the POSIX or Linux interface it implements, whose names both Linux
//! libraries share (glibc adds `64` to some, which the prefixes cover). On
//! Linux the kernel calls std makes through `syscall` are in the map by the
//! name [`super::libc::syscall_name`] gives them. Either way the boundary
//! is a call the optimizer cannot inline away, which is why IO is checked
//! here and not through `--slow` paths: std's IO functions inline away.
//!
//! Natively a category changes how a finding is worded, not what is found:
//! no call in one is known not to wait, so without the category the native
//! check already reports each as "cannot verify". Naming it makes the
//! finding red and names the category. On wasip1 a category is the only way
//! its calls are found.
//!
//! What no category covers: writes to an already-open descriptor (`write`,
//! `fd_write`). Standard output and error are such writes, and so is the panic
//! hook's report, which the wasm scan cannot tell apart from a claimed body's
//! own call. Opening a file is in `files`, so a body that writes a file it
//! opened is caught at the open.
//!
//! Sleeping is no category: it is a wait on every target ([`WASI_WAITS`] here,
//! `nanosleep` natively, `memory.atomic.wait32` on threaded wasm).
//!
//! A web build (`wasm32-unknown-unknown`) has no category's calls: std's files
//! and sockets are stubs that fail at once, and IO through JavaScript is
//! asynchronous. The rule does not run a category on it.

use super::libc::Os;

/// One category: its name as a project writes it, and its calls per target.
/// A trailing `*` matches a prefix.
pub(crate) struct Category {
    pub(crate) name: &'static str,
    /// Fields of [`WASI_MODULE`].
    wasi: &'static [&'static str],
    /// C library functions, as the IR of an Apple build names them.
    apple: &'static [&'static str],
    /// C library functions and `syscall` calls, as the IR of a Linux build,
    /// glibc or Android, names them.
    linux: &'static [&'static str],
}

/// The only import module a wasip1 std calls.
pub(crate) const WASI_MODULE: &str = "wasi_snapshot_preview1";

/// The wasip1 imports that wait. On wasip1 without `+atomics`, sleeping and
/// polling are `poll_oneoff`, and nothing else can block the caller.
pub(crate) const WASI_WAITS: &[&str] = &["poll_oneoff"];

pub(crate) const CATEGORIES: &[Category] = &[
    Category {
        name: "files",
        // `path_*` is every operation that names a file; the `fd_*` here are
        // reads, positional writes, syncs and metadata on an open descriptor.
        wasi: &[
            "path_*",
            "fd_read",
            "fd_pread",
            "fd_pwrite",
            "fd_readdir",
            "fd_sync",
            "fd_datasync",
            "fd_allocate",
            "fd_filestat_*",
        ],
        apple: &[
            "open*",
            "fopen*",
            "freopen*",
            "fdopendir*",
            "readdir*",
            "stat*",
            "lstat*",
            "fstat*",
            "access",
            "faccessat",
            "mkdir*",
            "rmdir",
            "unlink*",
            "rename*",
            "link",
            "linkat",
            "symlink*",
            "readlink*",
            "realpath*",
            "chmod",
            "fchmod*",
            "chown",
            "lchown",
            "fchown*",
            "truncate",
            "ftruncate",
            "utimes",
            "futimes",
            "utimensat",
            "futimens",
            "read",
            "read$NOCANCEL",
            "readv",
            "pread*",
            "preadv*",
            "pwrite*",
            "pwritev*",
            "fsync*",
            "fcntl",
            "getattrlist*",
            "setattrlist*",
            "fgetattrlist*",
            "getattrlistbulk",
            "getdirentries*",
            "copyfile",
            "fcopyfile",
            "clonefile*",
            "fclonefileat",
            "getxattr",
            "fgetxattr",
            "setxattr",
            "fsetxattr",
            "listxattr",
            "flistxattr",
            "removexattr",
            "fremovexattr",
            "getcwd",
            "chdir",
            "fchdir",
        ],
        linux: &[
            "open*",
            "fopen*",
            "freopen*",
            "opendir",
            "fdopendir",
            "readdir*",
            "stat*",
            "lstat*",
            "fstat*",
            "access",
            "faccessat",
            "mkdir*",
            "mkfifo*",
            "rmdir",
            "unlink*",
            "rename*",
            "link",
            "linkat",
            "symlink*",
            "readlink*",
            "realpath",
            "chmod",
            "fchmod*",
            "chown",
            "lchown",
            "fchown*",
            "truncate*",
            "ftruncate*",
            "utimes",
            "futimes",
            "utimensat",
            "futimens",
            "read",
            "readv",
            "pread*",
            "preadv*",
            "pwrite*",
            "pwritev*",
            "fsync",
            "fdatasync",
            "fcntl",
            "copy_file_range",
            "sendfile*",
            "splice",
            "getxattr",
            "fgetxattr",
            "setxattr",
            "fsetxattr",
            "listxattr",
            "flistxattr",
            "removexattr",
            "fremovexattr",
            "getcwd",
            "chdir",
            "fchdir",
            "chroot",
            "syscall::SYS_statx",
            "syscall::SYS_copy_file_range",
            "syscall::SYS_splice",
            "syscall::SYS_preadv",
            "syscall::SYS_pwritev",
            "syscall::SYS_openat2",
        ],
    },
    Category {
        name: "network",
        // wasip1 has no `connect`: std's `TcpStream::connect` is a stub there
        // that fails at once.
        wasi: &["sock_*"],
        apple: &[
            "socket",
            "socketpair",
            "connect*",
            "accept*",
            "bind",
            "listen",
            "recv*",
            "send*",
            "shutdown",
            "getaddrinfo",
            "getnameinfo",
            "gethostbyname*",
            "gethostbyaddr*",
        ],
        linux: &[
            "socket",
            "socketpair",
            "connect",
            "accept*",
            "bind",
            "listen",
            "recv*",
            "send",
            "sendto",
            "sendmsg",
            "sendmmsg",
            "shutdown",
            "getaddrinfo",
            "getnameinfo",
            "gethostbyname*",
            "gethostbyaddr*",
            "res_*",
            "__res_*",
            "syscall::SYS_accept4",
        ],
    },
    Category {
        name: "process",
        // wasip1 cannot start a program.
        wasi: &[],
        apple: &[
            "posix_spawn",
            "posix_spawnp",
            "fork",
            "vfork",
            "execv*",
            "execl*",
            "execve",
            "system",
            "popen",
        ],
        linux: &[
            "posix_spawn",
            "posix_spawnp",
            "pidfd_spawn",
            "pidfd_spawnp",
            "fork",
            "vfork",
            "clone",
            "execv*",
            "execl*",
            "execve",
            "system",
            "popen",
            "syscall::SYS_clone3",
        ],
    },
];

pub(crate) fn parse(name: &str) -> Result<&'static Category, String> {
    CATEGORIES.iter().find(|c| c.name == name).ok_or_else(|| {
        format!(
            "no_block_check: `--io {name}` is not an IO category. The categories \
             are {}.",
            names()
        )
    })
}

pub(crate) fn names() -> String {
    CATEGORIES
        .iter()
        .map(|c| format!("`{}`", c.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

impl Category {
    /// Whether a wasm import is one of this category's calls.
    pub(crate) fn wasi_call(&self, module: &str, field: &str) -> bool {
        module == WASI_MODULE && self.wasi.iter().any(|p| matches(p, field))
    }

    /// Whether a native external function is one of this category's calls
    /// on `os`.
    pub(crate) fn native_call(&self, os: Os, sym: &str) -> bool {
        let calls = match os {
            Os::Apple => self.apple,
            Os::Linux => self.linux,
        };
        calls.iter().any(|p| matches(p, sym))
    }
}

/// Whether a wasm import waits on wasip1.
pub(crate) fn wasi_wait(module: &str, field: &str) -> bool {
    module == WASI_MODULE && WASI_WAITS.contains(&field)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_category_maps_to_each_targets_calls() {
        let files = parse("files").unwrap();
        assert!(files.wasi_call(WASI_MODULE, "path_open"));
        assert!(files.wasi_call(WASI_MODULE, "fd_read"));
        assert!(
            !files.wasi_call(WASI_MODULE, "fd_write"),
            "stderr is a write"
        );
        assert!(!files.wasi_call("env", "path_open"));
        assert!(files.native_call(Os::Apple, "open"));
        assert!(files.native_call(Os::Apple, "realpath$DARWIN_EXTSN"));
        assert!(!files.native_call(Os::Apple, "write"));
        assert!(files.native_call(Os::Linux, "open64"));
        assert!(files.native_call(Os::Linux, "fstatat64"));
        assert!(files.native_call(Os::Linux, "syscall::SYS_statx"));
        assert!(!files.native_call(Os::Linux, "write"));
        assert!(!files.native_call(Os::Linux, "syscall::SYS_futex::FUTEX_WAKE"));
        let net = parse("network").unwrap();
        assert!(net.wasi_call(WASI_MODULE, "sock_recv"));
        assert!(net.native_call(Os::Apple, "connect"));
        assert!(net.native_call(Os::Apple, "getaddrinfo"));
        assert!(!net.native_call(Os::Apple, "open"));
        assert!(net.native_call(Os::Linux, "accept4"));
        assert!(net.native_call(Os::Linux, "syscall::SYS_accept4"));
        let process = parse("process").unwrap();
        assert!(process.native_call(Os::Linux, "pidfd_spawnp"));
        assert!(!process.native_call(Os::Apple, "pidfd_spawnp"));
    }

    #[test]
    fn an_unknown_category_is_refused_naming_the_known_ones() {
        let e = parse("disk").err().unwrap();
        assert!(e.contains("`files`, `network`, `process`"), "{e}");
    }

    /// What makes a category on a native target only wording: every call in
    /// it is "cannot verify" without it, in every C library of its OS.
    #[test]
    fn no_native_call_in_a_category_is_known_not_to_wait() {
        for libc in crate::libc::KNOWN {
            for c in CATEGORIES {
                let calls = match libc.os {
                    Os::Apple => c.apple,
                    Os::Linux => c.linux,
                };
                for p in calls {
                    let stem = p.trim_end_matches('*');
                    assert!(!libc.no_wait(stem), "{}: `{p}` in `{}`", libc.name, c.name);
                }
            }
            for (names, _) in libc.no_wait {
                for n in names.iter().map(|n| n.trim_end_matches('*')) {
                    assert!(
                        !CATEGORIES.iter().any(|c| c.native_call(libc.os, n)),
                        "{}: `{n}`",
                        libc.name
                    );
                }
            }
        }
    }

    #[test]
    fn poll_oneoff_is_wasip1s_wait() {
        assert!(wasi_wait(WASI_MODULE, "poll_oneoff"));
        assert!(!wasi_wait(WASI_MODULE, "path_open"));
    }
}
