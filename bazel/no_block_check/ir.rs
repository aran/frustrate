//! The native check: the same `#[bridge(no_block)]` claims, settled over the
//! LLVM IR of a native build rather than over a wasm module.
//!
//! The input is one module: the bridge crate built for one platform as a
//! `staticlib` with fat LTO and `--emit=llvm-ir`, so std and every Rust
//! dependency are merged in from their bitcode, under
//! `--cfg frustrate_no_block_check`, which adds one root per
//! claimed member and natively keeps every production export, so the whole
//! program is present. On the native target a wait is a call into the OS, and
//! the IR keeps every call out of the Rust program as a call to an external
//! declaration.
//!
//! The module is read as text. The crates that read LLVM IR (`llvm-ir`,
//! `inkwell`) bind libLLVM through `llvm-sys`, which needs `llvm-config` and
//! the headers of rustc's own LLVM version; the Rust toolchain ships neither.
//! `opt`'s call graph has the direct calls only, not the address-takes in
//! initialisers or where an indirect call's value came from, which
//! [`super::provenance`] needs. The reader's direct calls are checked
//! against that graph by `:callgraph_test` (`callgraph_check.rs`).
//!
//! # How it decides
//!
//! The graph's edges are every symbol a function body or a global initialiser
//! mentions: direct calls, and the address-takes that put a function in a
//! vtable or a function pointer. A root's closure is everything reachable
//! along them.
//!
//! **Externals are allowed by name, never denied by name.** An external a
//! closure reaches is fine only if the target's C library ([`super::libc`],
//! chosen by the module's `target triple`) lists it as known not to wait,
//! each group with why, or the project lists it with `--allow`. Any other
//! external is "cannot verify", naming it: a C dependency, an OS call nobody
//! has classified, a wait. A few std functions are vouched for by path
//! ([`KNOWN_PATHS`]): the externals each lists are allowed where only it
//! leads to them. The library's waits list only words the finding
//! for the waits std reaches. A module whose triple names no library the
//! check knows is refused. On Linux a call to `syscall` is an external named
//! after the kernel call it makes ([`libc::syscall_name`]).
//!
//! **The bounded lock is the one sanctioned wait.** frustrate's registries
//! share one lock type (`runtime/rust/src/bounded.rs`) whose acquire the check
//! build exports as [`ACQUIRE`]. Its wait is bounded by the longest critical
//! section any thread runs under it, so the edge into the acquire is not
//! followed only when every section in the program is proven: every function
//! that references the acquire (each `Lock::with` instance) must reach no
//! wait, no slow or IO sink, no unvouched external and no second acquire, and
//! the acquire itself must reach nothing but its library's acquire waits,
//! the native `Mutex`'s lock. A root that
//! reaches the lock while a section fails is "cannot verify", naming the
//! section.
//!
//! **Each full root must reach its member's own function**, by the path the
//! census carries. The roots take the request bytes as real parameters, so no
//! optimizer can delete a body behind a decode it proved fails; this control
//! is what catches a build where the member was still not reached.
//!
//! **Inline asm is an external.** A body's inline asm with any instruction
//! in its template (a template of assembler comments only, like zeroize's
//! barrier, has none) is a call to an external named `asm "<template>"`, so a root that
//! reaches it is "cannot verify" unless silenced by that name, or unless the
//! template's instructions are, exactly, a sequence [`KNOWN_ASM`] lists for
//! the module's architecture. Its `sym` operands are references like any
//! other.
//!
//! Not followed: edges into the fatal entries ([`super::FATAL_ENTRIES`]),
//! the panic handler among them.
//!
//! **Silences.** Any finding is silenced by naming what it names: `--allow <name>` (the check target's `allow`)
//! for every claim, or a census `allow` row (`no_block(allow(...))` in
//! source) for one member. A function can also be silenced whole: one marked
//! `#[frustrate::no_block(allow)]` calls [`MARKER`] first, and a function
//! that calls it is a stop point: no claim's traversal enters it, so it and
//! everything it reaches are excluded, the bounded lock's section proof
//! included. Each applied silence is listed in the verdict and written for
//! [`super::silences`], which fails on one that no analysis of the check
//! target applied (for a marked function: one a claimed member reaches, whose
//! subtree holds no finding). A marked function no claim reaches is ignored.
//!
//! # What green does not cover
//!
//! - **A call through a value from a heap object the member was handed**
//!   (a handle's object, say), unless the value came from a static or an
//!   external function. A call through a value loaded from a mutable static,
//!   or returned by an external function, is "cannot verify"
//!   ([`super::provenance`]), unless the static is one frustrate knows
//!   ([`KNOWN_SLOTS`]); one through a value the member produced is followed,
//!   and so is one through a static that holds only the program's own
//!   functions, into each of them.
//! - **The panic path**, cut at the fatal entries: a panicking member runs
//!   std's hook, any installed hook, and the unwinder on its own thread.
//! - **Termination**: a section's or a body's own loops.
//!
//! Green covers the one platform the module was built for. A body split by
//! `#[cfg]` is checked in that arm only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::libc::{self, Libc};
use super::silences::{Applied, Silence};
use super::symbol::trait_path;
use super::{match_census, own_path, readable, Claims, Green, Sinks, FATAL_ENTRIES, ROOT_PREFIX};

/// The export name `runtime/rust/src/bounded.rs` gives the bounded lock's
/// acquire in the check build.
pub(crate) const ACQUIRE: &str = "frustrate_bounded_lock_acquire";

/// The export name `runtime/rust/src/lib.rs` gives the marker that
/// `#[frustrate::no_block(allow)]` makes a function call first, in the check
/// build.
pub(crate) const MARKER: &str = "frustrate_no_block_allow";

/// Statics a call may be made through without a finding: what is in them
/// is installed by code frustrate knows, and why it cannot stall the caller.
pub(crate) const KNOWN_SLOTS: &[(&str, &str)] = &[
    (
        "core::io::error::os_functions::OS_FUNCTIONS",
        "std's own table of OS error functions, installed by std at startup; \
         they decode and describe an error code",
    ),
    (
        "std::sys::thread_local::destructors::list::DTORS",
        "std's list of this thread's thread-local destructors, which std runs \
         when the thread exits, never within a call",
    ),
    (
        "frustrate::post::native::dl::API",
        "the Dart VM's API table, filled at the isolate handshake; posting a \
         message takes only the VM's port and queue locks, for a map lookup \
         and an enqueue",
    ),
];

/// std functions whose externals frustrate vouches for: each with the
/// externals that are not a finding when the function is the only way a
/// claim reaches them, and why. Anything else beneath it is a finding as
/// anywhere, and so is a listed external the claim also reaches another way.
/// A vouch outranks an IO category: what it waives is not IO whose time
/// depends on a device or a filesystem.
pub(crate) const KNOWN_PATHS: &[(&str, &[&str], &str)] = &[(
    "std::sys::random::linux::hashmap_random_keys",
    &[
        "open",
        "open64",
        "read",
        "poll",
        "close",
        "__xpg_strerror_r",
        "syscall::SYS_futex::FUTEX_WAIT_BITSET",
    ],
    "std's `HashMap` keys on Linux and Android. It reads `/dev/urandom` only \
     where the getrandom call is missing or refuses (ENOSYS, a seccomp \
     filter, an unseeded pool on a kernel without GRND_INSECURE), and a read \
     of `/dev/urandom` never waits for entropy. Its lock is the `OnceLock` \
     that holds the open device, so a wait on it lasts one `open`. It asks \
     for keys without waiting for the pool, which skips the `poll` of \
     `/dev/random` and the `close` that follows it; `__xpg_strerror_r` \
     describes an error on its way to a panic",
)];

/// One LLVM IR module, as a graph over its symbols.
struct Ir {
    names: Vec<String>,
    index: BTreeMap<String, u32>,
    defined: Vec<bool>,
    /// Direct calls (`call`/`invoke` of `@f(`) per function.
    calls: Vec<BTreeSet<u32>>,
    /// Every symbol a body or initialiser mentions; a superset of `calls`.
    refs: Vec<BTreeSet<u32>>,
    triple: String,
}

impl Ir {
    /// The triple's architecture, its first component.
    /// The target's architecture, by LLVM's name for it: Apple's triples
    /// spell aarch64 `arm64`.
    fn arch(&self) -> &str {
        match self.triple.split('-').next().unwrap_or("") {
            "arm64" => "aarch64",
            a => a,
        }
    }

    fn id(&mut self, name: &str) -> u32 {
        if let Some(i) = self.index.get(name) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), i);
        self.defined.push(false);
        self.calls.push(BTreeSet::new());
        self.refs.push(BTreeSet::new());
        i
    }

    fn label(&self, f: u32) -> String {
        readable(&self.names[f as usize])
    }
}

/// The `@symbol`s on one line, each with whether `(` follows it — which in a
/// function body is a direct call, the only instruction that spells one.
/// Quoted text that is not a symbol's name (string constants, inline asm
/// templates and constraints, attribute values) is skipped, and so is a
/// comment, so neither an `@` inside data nor a `;` inside an asm template
/// changes what the line references. IR spells a quote inside quoted text as
/// `\22`, so the next `"` always closes it.
fn symbols(line: &str) -> Vec<(String, bool)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b';' => break,
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += 1;
                }
                i += 1;
            }
            b'@' => {
                i += 1;
                let name = if b.get(i) == Some(&b'"') {
                    let start = i + 1;
                    let mut j = start;
                    while j < b.len() && b[j] != b'"' {
                        j += 1;
                    }
                    i = j + 1;
                    unescape(&line[start..j.min(b.len())])
                } else {
                    let start = i;
                    while i < b.len()
                        && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'-' | b'$' | b'.' | b'_'))
                    {
                        i += 1;
                    }
                    line[start..i].to_string()
                };
                if !name.is_empty() && !name.starts_with("llvm.") {
                    out.push((name, b.get(i) == Some(&b'(')));
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Inline asm known not to wait, per architecture: each group the exact
/// instruction sequences of a template, as rustc's IR spells them, and why.
/// A template matches only when its instructions are one sequence, in order
/// and nothing else.
pub(crate) const KNOWN_ASM: &[(&str, &[&[&str]], &str)] = &[
    (
        "x86_64",
        &[&["mov ${0:q}, rbx", "cpuid", "xchg ${0:q}, rbx"]],
        "`cpuid`, as std_detect issues it, with `rbx` (which LLVM reserves) kept \
         around it: the CPU reports its features into registers. Under a \
         hypervisor it exits to the hypervisor, which answers and returns; \
         nothing waits on another thread",
    ),
    (
        "aarch64",
        &[&["fcvt ${0:h}, ${1:d}"], &["fcvt ${0:d}, ${1:h}"]],
        "`fcvt` between half and double precision, as the `half` crate issues \
         it: a conversion from one floating-point register to another. It \
         touches no memory, and a floating-point exception only sets a flag \
         in FPSR, since the OS leaves exception traps off",
    ),
];

/// Whether an `asm "<template>"` external is one [`KNOWN_ASM`] lists for
/// `arch`: its template's instructions, split at newlines and `;`, are
/// exactly one of the sequences.
fn known_asm(arch: &str, name: &str) -> bool {
    let Some(template) = name.strip_prefix("asm \"").and_then(|t| t.strip_suffix('"')) else {
        return false;
    };
    let insts: Vec<&str> = template
        .split("\\0A")
        .flat_map(|l| l.split(['\n', ';']))
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .collect();
    KNOWN_ASM
        .iter()
        .filter(|(a, _, _)| *a == arch)
        .any(|(_, seqs, _)| seqs.contains(&insts.as_slice()))
}

/// The template of the inline asm a body line calls, as the IR spells it,
/// when it holds any instruction. An empty template (`black_box`'s) does
/// nothing, and neither does one of assembler comments only (zeroize's
/// `# ${0}` barrier): a statement that begins with `#` or `//` is a comment
/// on x86_64 and aarch64 alike. Any other can branch or trap into the OS
/// without naming a symbol, so the caller records it as a call to an
/// external named after it.
fn inline_asm(line: &str) -> Option<&str> {
    let head = &line[..line.find('"')?];
    let words: Vec<&str> = head.split_whitespace().collect();
    let is_call = words.iter().any(|w| matches!(*w, "call" | "invoke" | "callbr"));
    if !is_call || !words.contains(&"asm") {
        return None;
    }
    let rest = &line[head.len() + 1..];
    let template = &rest[..rest.find('"')?];
    let instruction = template
        .split("\\0A")
        .flat_map(|l| l.split(['\n', ';']))
        .map(str::trim)
        .any(|i| !i.is_empty() && !i.starts_with('#') && !i.starts_with("//"));
    instruction.then_some(template)
}

/// A quoted IR name's `\XX` hex escapes.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 2 < b.len() + 1 {
            if let Ok(v) = u8::from_str_radix(s.get(i + 1..i + 3).unwrap_or(""), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse(text: &str) -> Result<Ir, String> {
    let mut ir = Ir {
        names: Vec::new(),
        index: BTreeMap::new(),
        defined: Vec::new(),
        calls: Vec::new(),
        refs: Vec::new(),
        triple: String::new(),
    };
    let mut current: Option<u32> = None;
    for line in text.lines() {
        if let Some(f) = current {
            if line.starts_with('}') {
                current = None;
                continue;
            }
            for (name, call) in symbols(line) {
                let name = match (call, name.as_str()) {
                    (true, "syscall") if ir.triple.contains("-linux-") => {
                        let args = &line[line.find("@syscall(").map_or(line.len(), |i| i + 9)..];
                        libc::syscall_name(ir.arch(), args)
                    }
                    _ => name,
                };
                let c = ir.id(&name);
                ir.refs[f as usize].insert(c);
                if call {
                    ir.calls[f as usize].insert(c);
                }
            }
            if let Some(template) = inline_asm(line) {
                let c = ir.id(&format!("asm \"{template}\""));
                ir.refs[f as usize].insert(c);
                ir.calls[f as usize].insert(c);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("target triple = \"") {
            ir.triple = rest.trim_end_matches('"').to_string();
        } else if line.starts_with("define ") {
            let Some((name, _)) = symbols(line).into_iter().next() else {
                return Err(format!("a `define` with no name: `{line}`"));
            };
            let f = ir.id(&name);
            ir.defined[f as usize] = true;
            // A body's attributes can name its personality function.
            for (n, _) in symbols(line).into_iter().skip(1) {
                let c = ir.id(&n);
                ir.refs[f as usize].insert(c);
            }
            if line.trim_end().ends_with('{') {
                current = Some(f);
            }
        } else if line.starts_with("declare ") {
            if let Some((name, _)) = symbols(line).into_iter().next() {
                ir.id(&name);
            }
        } else if line.starts_with('@') {
            // A global or alias: its initialiser's references belong to it.
            let mut syms = symbols(line).into_iter();
            if let Some((name, _)) = syms.next() {
                let g = ir.id(&name);
                ir.defined[g as usize] = true;
                for (n, _) in syms {
                    let c = ir.id(&n);
                    ir.refs[g as usize].insert(c);
                }
            }
        }
    }
    if current.is_some() {
        return Err("the module ends inside a function body".into());
    }
    if ir.triple.is_empty() {
        return Err("no `target triple` line: this is not an LLVM IR module".into());
    }
    Ok(ir)
}

/// Whether a sink makes a claim red (it waits, or is named) or unprovable (it
/// is outside what this check can see into).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    Red,
    CannotVerify,
}

/// What each function is as a sink, if anything.
fn sink_kinds(ir: &Ir, libc: &Libc, paths: &[Vec<String>], sinks: &Sinks) -> BTreeMap<u32, (Verdict, String)> {
    let mut kinds = BTreeMap::new();
    // `f` indexes the IR's parallel tables, not just `paths`.
    #[allow(clippy::needless_range_loop)]
    for f in 0..ir.names.len() {
        let n = &ir.names[f];
        let mut red: Vec<String> = Vec::new();
        let mut unknown = false;
        if !ir.defined[f] {
            if libc.waits(n) {
                red.push("waits".into());
            }
            red.extend(
                sinks
                    .io
                    .iter()
                    .filter(|c| c.native_call(libc.os, n))
                    .map(|c| format!("IO `{}`", c.name)),
            );
            unknown = red.is_empty() && !libc.no_wait(n) && !known_asm(ir.arch(), n);
        } else {
            red.extend(
                sinks
                    .paths
                    .iter()
                    .filter(|p| p.matches(&paths[f]))
                    .map(|p| format!("named slow by `{}`", p.text())),
            );
        }
        if !red.is_empty() {
            kinds.insert(f as u32, (Verdict::Red, red.join(", ")));
        } else if unknown {
            kinds.insert(
                f as u32,
                (
                    Verdict::CannotVerify,
                    "outside the Rust program, and not known not to wait".into(),
                ),
            );
        }
    }
    kinds
}

/// The functions of [`KNOWN_PATHS`] in the module, each with what it vouches
/// for.
fn known_paths(ir: &Ir, paths: &[Vec<String>]) -> Vec<(u32, &'static [&'static str])> {
    (0..ir.names.len())
        .filter(|f| ir.defined[*f])
        .filter_map(|f| {
            let p = paths[f].join("::");
            KNOWN_PATHS.iter().find(|(k, _, _)| *k == p).map(|(_, ext, _)| (f as u32, *ext))
        })
        .collect()
}

/// What `start` reaches with `cut` cut, less the externals that only a
/// function of [`KNOWN_PATHS`] leads to and that each such function it
/// reaches them through vouches for.
fn reach_vouched(
    ir: &Ir,
    start: u32,
    edges: &[BTreeSet<u32>],
    cut: &BTreeSet<u32>,
    known: &[(u32, &[&str])],
) -> BTreeSet<u32> {
    let all = reach(start, edges, cut);
    let inside: Vec<(BTreeSet<u32>, &[&str])> = known
        .iter()
        .filter(|(k, _)| all.contains(k))
        .map(|(k, ext)| (reach(*k, edges, cut), *ext))
        .collect();
    if inside.is_empty() {
        return all;
    }
    let mut around = cut.clone();
    around.extend(known.iter().map(|(k, _)| *k).filter(|k| *k != start));
    let outside = reach(start, edges, &around);
    all.into_iter()
        .filter(|f| {
            outside.contains(f)
                || ir.defined[*f as usize]
                || !inside
                    .iter()
                    .filter(|(r, _)| r.contains(f))
                    .all(|(_, ext)| ext.contains(&ir.names[*f as usize].as_str()))
        })
        .collect()
}

fn reach(start: u32, edges: &[BTreeSet<u32>], cut: &BTreeSet<u32>) -> BTreeSet<u32> {
    let mut seen = BTreeSet::from([start]);
    let mut stack = vec![start];
    while let Some(f) = stack.pop() {
        for c in &edges[f as usize] {
            if !cut.contains(c) && seen.insert(*c) {
                stack.push(*c);
            }
        }
    }
    seen
}

/// Whether `path` is a member's own function. `want` is the census's path
/// from the crate root (`crate::api::Doc::render`) and `display` the member's
/// name (`Doc::render`), whose segments end `want`; `krate` is the name rustc
/// compiled the bridge crate under. A free function's path is `krate`, the
/// module, then the name. A method's is its type's, which may be defined
/// outside the module holding the bridged impl, so it is `krate`, any
/// module, then the type and name. A `dyn` parent's trait method matches by
/// its [`trait_path`].
fn is_member(path: &[String], krate: &str, want: &[String], display: &[&str]) -> bool {
    let module = &want[1..want.len().saturating_sub(display.len())];
    let Some((first, rest)) = path.split_first() else {
        return false;
    };
    let ends = rest.len() >= display.len() && rest[rest.len() - display.len()..].iter().zip(display).all(|(a, b)| a == b);
    first == krate && ends && (display.len() > 1 || rest.starts_with(module) && rest.len() == module.len() + 1)
}

/// A marked function's path, one for all its instances: [`own_path`] leaves
/// out generic arguments, so every instance of a generic function or method
/// has the same one.
pub(crate) fn marked_key(sym: &str) -> String {
    own_path(sym).join("::")
}

/// One bounded-lock section that fails its proof, with the path to why.
struct DirtySection {
    section: u32,
    path: Vec<(u32, bool)>,
    why: String,
}

/// Prove the bounded lock's wait bounded: the acquire reaches only its own
/// wait, and every section reaches no sink and no second acquire. Answers the
/// sections that fail.
fn prove_bounded(
    ir: &Ir,
    libc: &Libc,
    acquire: u32,
    kinds: &BTreeMap<u32, (Verdict, String)>,
    fatal: &BTreeSet<u32>,
    marked: &BTreeSet<u32>,
    known: &[(u32, &[&str])],
) -> Vec<DirtySection> {
    let mut dirty = Vec::new();
    let own: BTreeSet<u32> = reach(acquire, &ir.refs, fatal)
        .into_iter()
        .filter(|f| kinds.contains_key(f) && !libc.acquire_waits.contains(&ir.names[*f as usize].as_str()))
        .collect();
    if !own.is_empty() {
        let path = shortest(&ir.refs, &ir.calls, acquire, &own, fatal).expect("reached");
        let last = path.last().unwrap().0;
        dirty.push(DirtySection {
            section: acquire,
            why: format!("the acquire itself reaches {} — {}", ir.label(last), kinds[&last].1),
            path,
        });
    }
    let mut cut = fatal.clone();
    cut.insert(acquire);
    let sections: Vec<u32> = (0..ir.names.len() as u32)
        .filter(|f| *f != acquire && !marked.contains(f) && ir.refs[*f as usize].contains(&acquire))
        .collect();
    for s in &sections {
        let reached = reach_vouched(ir, *s, &ir.refs, &cut, known);
        let bad: BTreeSet<u32> = reached
            .iter()
            .copied()
            .filter(|f| kinds.contains_key(f) || (*f != *s && ir.refs[*f as usize].contains(&acquire)))
            .collect();
        if bad.is_empty() {
            continue;
        }
        let path = shortest(&ir.calls, &ir.calls, *s, &bad, &cut)
            .or_else(|| shortest(&ir.refs, &ir.calls, *s, &bad, &cut))
            .expect("reached");
        let last = path.last().unwrap().0;
        let why = match kinds.get(&last) {
            Some((_, k)) => format!("reaches {} — {k}", ir.label(last)),
            None => format!(
                "takes a bounded lock again in {}, which can deadlock",
                ir.label(last)
            ),
        };
        dirty.push(DirtySection { section: *s, path, why });
    }
    dirty
}

/// Settle the claims of one native IR module.
pub(crate) fn run_ir(
    path: &str,
    label: &str,
    claims: Option<&Claims>,
    sinks: &Sinks,
) -> Result<Green, String> {
    super::budget::set(path, label);
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut ir = parse(&text).map_err(|e| format!("no_block_check: {label}: {e}"))?;

    let Some(libc) = libc::for_triple(&ir.triple) else {
        return Err(format!(
            "no_block_check: {label} targets `{}`, and this check knows which \
             C library calls do not wait only for {}. A list for another C \
             library has to be drawn from its implementation before it can be \
             trusted.",
            ir.triple,
            libc::KNOWN.iter().map(|l| l.name).collect::<Vec<_>>().join(", "),
        ));
    };

    // Control: the module is the whole program. After fat LTO every Rust
    // function has a body here; a Rust symbol that is only declared is code
    // this scan cannot see into, so a green over it would mean nothing.
    let missing: Vec<String> = (0..ir.names.len())
        .filter(|f| !ir.defined[*f] && (ir.names[*f].starts_with("_R") || ir.names[*f].starts_with("_ZN")))
        .take(super::MEMBER_CAP)
        .map(|f| format!("  {}", ir.label(f as u32)))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "no_block_check: cannot verify {label}: Rust functions are declared \
             here without a body, so this is not the whole program:\n\n{}\n\n\
             The IR must come from a fat-LTO build of a final crate type \
             (`-Clto=fat`, `link` among the emits), which merges std and every \
             dependency into the crate's module. Without one, rustc writes the \
             crate's own module before LTO.",
            missing.join("\n")
        ));
    }

    let mut roots: Vec<u32> = Vec::new();
    let mut root_names: Vec<String> = Vec::new();
    for (i, n) in ir.names.iter().enumerate() {
        if ir.defined[i] && n.starts_with(ROOT_PREFIX) {
            roots.push(i as u32);
            root_names.push(n.clone());
        }
    }
    match_census(&root_names, claims, label)?;

    let paths: Vec<Vec<String>> = ir.names.iter().map(|n| own_path(n)).collect();
    let trait_paths: Vec<Option<Vec<String>>> = ir.names.iter().map(|n| trait_path(n)).collect();

    // Control: every `--slow` path names a function in the program. This
    // build keeps every production export, so a path that matches nothing
    // here is misspelt, or names a function this target's build never calls;
    // either way it checks nothing.
    let unmatched: Vec<String> = sinks
        .paths
        .iter()
        .filter(|p| !(0..ir.names.len()).any(|f| ir.defined[f] && p.matches(&paths[f])))
        .map(|p| format!("  `{}`", p.text()))
        .collect();
    if !unmatched.is_empty() {
        return Err(format!(
            "no_block_check: {label}: {} `slow` path(s) name no function in \
             this program:\n\n{}\n\n\
             This build keeps every production export, so the path is misspelt \
             (it starts with the crate's name as rustc sees it, hyphens as \
             underscores, and matches whole segments), or nothing in this \
             target's build calls the function. Either way it would check \
             nothing. Correct it, or remove it.",
            unmatched.len(),
            unmatched.join("\n")
        ));
    }

    let fatal: BTreeSet<u32> = (0..ir.names.len() as u32)
        .filter(|f| FATAL_ENTRIES.iter().any(|e| e.matches(&ir.names[*f as usize])))
        .collect();
    let acquire = ir
        .index
        .get(ACQUIRE)
        .copied()
        .filter(|a| ir.defined[*a as usize]);
    let mut cut = fatal.clone();
    cut.extend(acquire);
    // Functions marked `#[frustrate::no_block(allow)]`: stop points for every
    // traversal below except the control that a root reaches its member.
    let marked: BTreeSet<u32> = match ir.index.get(MARKER) {
        Some(m) => (0..ir.names.len() as u32)
            .filter(|f| *f != *m && ir.defined[*f as usize] && ir.calls[*f as usize].contains(m))
            .collect(),
        None => BTreeSet::new(),
    };
    let mut stops = cut.clone();
    stops.extend(&marked);

    // Control: every full root reaches its member's own function. The
    // crate's name is the one its glue (`<crate>::frustrate_generated::call_*`,
    // which the root calls) was compiled under.
    //
    // A member of a `dyn` parent is called through the vtable of whatever
    // object the handle holds, built wherever that object was made, so its
    // own function is never in the root's closure. Its possible callees are
    // every function in a vtable that implements that trait method, named
    // `<Impl as Trait>::method` ([`trait_path`]): the root gets an edge to each.
    let address_taken: BTreeSet<u32> = (0..ir.names.len())
        .flat_map(|g| ir.refs[g].difference(&ir.calls[g]).copied().collect::<Vec<_>>())
        .collect();
    // Roots that do not reach their member's own function, with its path: a
    // finding of their own unless the root is red regardless.
    let mut unreached: BTreeMap<String, String> = BTreeMap::new();
    if let Some(c) = claims {
        let mut unreached_found: Vec<(String, String)> = Vec::new();
        for (sym, display) in &c.artifact {
            let (Some(&r), Some(want)) = (ir.index.get(sym), c.member_paths.get(sym)) else {
                continue;
            };
            let segs: Vec<&str> = display.split("::").collect();
            let krate = ir.calls[r as usize].iter().find_map(|g| {
                let p = &paths[*g as usize];
                (p.len() == 3 && p[1] == super::GLUE_MODULE && p[2].starts_with("call_"))
                    .then(|| p[0].clone())
            });
            let reached = krate.is_some_and(|k| {
                reach(r, &ir.refs, &cut).iter().any(|f| {
                    let f = *f as usize;
                    ir.defined[f]
                        && (is_member(&paths[f], &k, want, &segs)
                            || trait_paths[f].as_ref().is_some_and(|t| is_member(t, &k, want, &segs)))
                })
            });
            if reached {
                continue;
            }
            let implementers: Vec<u32> = if segs.len() == 2 {
                address_taken
                    .iter()
                    .copied()
                    .filter(|f| {
                        ir.defined[*f as usize]
                            && trait_paths[*f as usize]
                                .as_ref()
                                .is_some_and(|t| t.ends_with(&[segs[0].to_string(), segs[1].to_string()]))
                    })
                    .collect()
            } else {
                Vec::new()
            };
            if implementers.is_empty() {
                unreached_found.push((sym.clone(), want.join("::")));
            } else {
                ir.refs[r as usize].extend(implementers);
            }
        }
        unreached = unreached_found.into_iter().collect();
    }

    // Calls through a value the member did not produce: each is a sink of
    // its own, named by the function that makes it.
    let mut unresolved: BTreeMap<u32, String> = BTreeMap::new();
    let mut site_of: BTreeMap<u32, String> = BTreeMap::new();
    {
        let mut analysed: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &roots {
            for f in reach(*r, &ir.refs, &BTreeSet::new()) {
                if ir.defined[f as usize] {
                    analysed.insert(ir.names[f as usize].clone());
                }
            }
        }
        let known = |p: &str| KNOWN_SLOTS.iter().any(|(k, _)| *k == p);
        let calls = super::provenance::foreign_calls(&text, &analysed, &known);
        // A call through what is loaded from a dispatch table calls one of
        // the functions stored in it.
        for (table, stored) in &calls.tables {
            let t = ir.id(table);
            for f in stored {
                let f = ir.id(f);
                ir.refs[t as usize].insert(f);
            }
        }
        for site in calls.sites {
            let f = ir.id(&site.at);
            let name = format!("an indirect call in {}", readable(&site.func));
            let s = ir.id(&name);
            ir.defined[s as usize] = true;
            ir.calls[f as usize].insert(s);
            ir.refs[f as usize].insert(s);
            site_of.insert(s, site.func.clone());
            let entry = unresolved.entry(s).or_default();
            for src in site.sources {
                if !entry.split("; ").any(|e| e == src) {
                    if !entry.is_empty() {
                        entry.push_str("; ");
                    }
                    entry.push_str(&src);
                }
            }
        }
    }
    let paths: Vec<Vec<String>> = ir.names.iter().map(|n| own_path(n)).collect();
    let mut kinds = sink_kinds(&ir, libc, &paths, sinks);
    let known = known_paths(&ir, &paths);
    for (s, why) in &unresolved {
        kinds.insert(*s, (Verdict::CannotVerify, format!("its callee may be {why}")));
    }
    // What a silence names: an external function by its symbol, an indirect
    // call by the path of the function that makes it.
    let key_of: BTreeMap<u32, String> = kinds
        .keys()
        .map(|s| {
            let key = match site_of.get(s) {
                Some(func) => own_path(func).join("::"),
                None if ir.defined[*s as usize] => own_path(&ir.names[*s as usize]).join("::"),
                None => ir.names[*s as usize].clone(),
            };
            (*s, key)
        })
        .collect();
    for (s, key) in &key_of {
        if let Some(k) = kinds.get_mut(s) {
            k.1.push_str(&format!(" (silence it as `{key}`)"));
        }
    }
    // The check target's silences apply everywhere, sections included.
    let mut used_target: BTreeSet<String> = BTreeSet::new();
    let target_silenced: BTreeSet<u32> = key_of
        .iter()
        .filter(|(_, k)| sinks.allowed.iter().any(|w| w == *k))
        .map(|(s, _)| *s)
        .collect();
    for s in &target_silenced {
        kinds.remove(s);
    }

    let display = |sym: &str| -> String {
        claims
            .and_then(|c| c.shown(sym))
            .unwrap_or_else(|| sym.to_string())
    };

    let dirty = match acquire {
        Some(a) => {
            let mut stops = fatal.clone();
            stops.extend(&marked);
            prove_bounded(&ir, libc, a, &kinds, &stops, &marked, &known)
        }
        None => Vec::new(),
    };
    // A target silence is used if any section of the bounded lock reaches
    // what it names.
    if let Some(a) = acquire {
        let mut sect_cut = fatal.clone();
        sect_cut.insert(a);
        sect_cut.extend(&marked);
        for f in 0..ir.names.len() as u32 {
            if f != a && !marked.contains(&f) && ir.refs[f as usize].contains(&a) {
                for g in reach_vouched(&ir, f, &ir.refs, &sect_cut, &known) {
                    if target_silenced.contains(&g) {
                        used_target.insert(key_of[&g].clone());
                    }
                }
            }
        }
    }
    let empty: Vec<String> = Vec::new();
    let member_allow = |sym: &str| -> (&str, &Vec<String>) {
        claims
            .and_then(|c| c.ids.get(sym).and_then(|id| c.allow.get(id).map(|a| (id.as_str(), a))))
            .unwrap_or(("", &empty))
    };
    let mut applied = Applied::default();
    let mut silences: Vec<String> = Vec::new();
    // Marked functions a claim reaches, by path: whether any of the
    // instances holds a finding the marker keeps out of the verdict.
    let mut marks: BTreeMap<String, bool> = BTreeMap::new();

    struct Finding {
        who: String,
        verdict: Verdict,
        path: Vec<(u32, bool)>,
        why: String,
        sites: usize,
    }
    let mut findings: Vec<Finding> = Vec::new();
    for (r, sym) in roots.iter().zip(&root_names) {
        let closure = reach_vouched(&ir, *r, &ir.refs, &stops, &known);
        for m in marked.iter().filter(|m| closure.iter().any(|f| ir.refs[*f as usize].contains(m))) {
            let key = marked_key(&ir.names[*m as usize]);
            let live = reach(*m, &ir.refs, &stops).iter().any(|f| kinds.contains_key(f));
            let held = marks.entry(key).or_default();
            *held |= live;
        }
        for g in closure.iter().filter(|g| target_silenced.contains(g)) {
            used_target.insert(key_of[g].clone());
        }
        let (id, mine) = member_allow(sym);
        let mut used_mine: BTreeSet<&str> = BTreeSet::new();
        let reached: BTreeSet<u32> = closure
            .iter()
            .copied()
            .filter(|f| kinds.contains_key(f))
            .filter(|f| match key_of.get(f) {
                Some(k) => match mine.iter().find(|w| *w == k) {
                    Some(w) => {
                        used_mine.insert(w);
                        false
                    }
                    None => true,
                },
                None => true,
            })
            .collect();
        for w in mine.iter().filter(|w| used_mine.contains(w.as_str())) {
            silences.push(format!("  `{w}` in {}", display(sym)));
            applied.silences.insert(Silence::Member(id.to_string(), w.clone()));
        }
        // Red ahead of unprovable: a wait is the finding when there is one.
        let worst = reached.iter().map(|f| kinds[f].0).min();
        if let Some(v) = worst {
            let targets: BTreeSet<u32> = reached.iter().copied().filter(|f| kinds[f].0 == v).collect();
            let path = shortest(&ir.calls, &ir.calls, *r, &targets, &stops)
                .or_else(|| shortest(&ir.refs, &ir.calls, *r, &targets, &stops))
                .expect("a reached sink has a path");
            let last = path.last().unwrap().0;
            findings.push(Finding {
                who: display(sym),
                verdict: v,
                why: format!("reaches {} — {}", ir.label(last), kinds[&last].1),
                path,
                sites: reached.len(),
            });
            continue;
        }
        if let Some(want) = unreached.get(sym) {
            findings.push(Finding {
                who: display(sym),
                verdict: Verdict::CannotVerify,
                why: format!(
                    "does not reach its own function `{want}`: it was inlined into \
                     its glue (`#[inline(always)]`, or the crate's own opt-level), \
                     or rustc names it otherwise, so the scan did not read its body"
                ),
                path: vec![(*r, true)],
                sites: 1,
            });
            continue;
        }
        if let (Some(a), Some(d)) = (acquire, dirty.first()) {
            let sections: BTreeSet<u32> = closure
                .iter()
                .copied()
                .filter(|f| ir.refs[*f as usize].contains(&a))
                .collect();
            if let Some(path) = shortest(&ir.refs, &ir.calls, *r, &sections, &stops) {
                findings.push(Finding {
                    who: display(sym),
                    verdict: Verdict::CannotVerify,
                    why: format!(
                        "takes the bounded lock, whose wait is unproven: the section \
                         in {} {}",
                        ir.label(d.section),
                        d.why
                    ),
                    path,
                    sites: 1,
                });
            }
        }
    }

    for (path, live) in marks {
        if live {
            silences.push(format!("  `{path}` in every claim (`#[frustrate::no_block(allow)]`)"));
            applied.silences.insert(Silence::Mark(path));
        } else {
            applied.idle_marks.insert(path);
        }
    }
    for w in sinks.allowed.iter().filter(|w| used_target.contains(*w)) {
        silences.push(format!("  `{w}` in every claim (the check target's `allow`)"));
        applied.silences.insert(Silence::Target(w.clone()));
    }
    let silence_note = if silences.is_empty() {
        String::new()
    } else {
        format!("\nSilenced:\n{}", silences.join("\n"))
    };

    let sink_note = if sinks.is_empty() {
        String::new()
    } else {
        format!(", and {}", sinks.text())
    };

    if findings.is_empty() {
        let text = format!(
            "no_block_check: {label} is clean — {} `no_block` root(s) on `{}` reach \
             only externals known not to wait{sink_note}; {}{}.{silence_note}",
            roots.len(),
            ir.triple,
            if acquire.is_some() {
                "the bounded lock's sections are all proven"
            } else {
                "no bounded lock is present"
            },
            claims.map(Claims::accounting).unwrap_or_default()
        );
        return Ok(Green { text, applied });
    }

    findings.sort_by_key(|f| f.verdict);
    let red = findings.iter().filter(|f| f.verdict == Verdict::Red).count();
    let mut msg = format!(
        "no_block_check: {label}: on `{}`, {red} `#[bridge(no_block)]` member(s) \
         reach a wait{} and {} cannot be verified.\n\n",
        ir.triple,
        if sinks.is_empty() { "" } else { " or a function named slow or IO" },
        findings.len() - red,
    );
    for (n, f) in findings.iter().enumerate() {
        let last = f.path.last().unwrap().0;
        if n >= super::WITNESSED {
            if n == super::WITNESSED {
                msg.push_str("  Without paths:\n");
            }
            msg.push_str(&format!("  {} — {}\n", f.who, f.why));
            continue;
        }
        let tag = match f.verdict {
            Verdict::Red => "",
            Verdict::CannotVerify => "cannot verify: ",
        };
        msg.push_str(&format!("  {}\n      {tag}{}\n", f.who, f.why));
        if f.sites > 1 {
            msg.push_str(&format!("      and {} other site(s)\n", f.sites - 1));
        }
        for (d, (g, call)) in f.path.iter().enumerate() {
            msg.push_str(&format!(
                "        {}{}{}\n",
                "  ".repeat(d.min(8)),
                ir.label(*g),
                if *call || d == 0 { "" } else { "   (address taken: a `dyn` or fn-pointer call)" }
            ));
        }
        let _ = last;
        msg.push('\n');
    }
    if let Some(d) = dirty.first() {
        msg.push_str(&format!("The bounded lock's unproven section, {}:\n", ir.label(d.section)));
        for (i, (g, _)) in d.path.iter().enumerate() {
            msg.push_str(&format!("        {}{}\n", "  ".repeat(i.min(8)), ir.label(*g)));
        }
        if dirty.len() > 1 {
            msg.push_str("  and the others:\n");
            for d in dirty.iter().skip(1).take(super::WITNESSED) {
                msg.push_str(&format!("    {}\n      {}\n", ir.label(d.section), d.why));
            }
            if dirty.len() > super::WITNESSED + 1 {
                msg.push_str(&format!("    and {} more\n", dirty.len() - super::WITNESSED - 1));
            }
        }
        msg.push('\n');
    }
    msg.push_str(
        "A claimed member's caller may be the main isolate, where a wait stalls \
         the UI for as\nlong as another thread holds what it waits for. Ways out: \
         take the lock or the slow\ncall out of the body, make the member `async` \
         so it is dispatched off the caller, or\ndrop the claim. \"Cannot verify\" \
         names what the check cannot see into: a function outside the\nRust \
         program not known not to wait, or an indirect call whose target may \
         come from outside it.\nIf a finding cannot stall the caller in practice, silence it: `no_block(allow(<name>))` on the\nmember, its impl or its file \
         (`bridge_file!`), or `allow` in the check target for every claim, or put \
         `#[frustrate::no_block(allow)]` on a function of your own that the member reaches.\n\
         \nNot followed, and not counted above:\n",
    );
    for f in FATAL_ENTRIES {
        msg.push_str(&format!("  {} — {}.\n", f.what, f.why));
    }
    Err(msg)
}

/// A shortest path from `start` to the nearest of `targets` over `edges`,
/// each step marked with whether it is a direct call.
fn shortest(
    edges: &[BTreeSet<u32>],
    calls: &[BTreeSet<u32>],
    start: u32,
    targets: &BTreeSet<u32>,
    cut: &BTreeSet<u32>,
) -> Option<Vec<(u32, bool)>> {
    let mut prev: BTreeMap<u32, u32> = BTreeMap::new();
    let mut seen = BTreeSet::from([start]);
    let mut queue = VecDeque::from([start]);
    while let Some(f) = queue.pop_front() {
        if targets.contains(&f) {
            let mut path = vec![f];
            let mut at = f;
            while let Some(p) = prev.get(&at) {
                path.push(*p);
                at = *p;
            }
            path.reverse();
            let marked = path
                .iter()
                .enumerate()
                .map(|(i, n)| (*n, i == 0 || calls[path[i - 1] as usize].contains(n)))
                .collect();
            return Some(marked);
        }
        for c in &edges[f as usize] {
            if !cut.contains(c) && seen.insert(*c) {
                prev.insert(*c, f);
                queue.push_back(*c);
            }
        }
    }
    None
}


// Built only by :callgraph_test, which supplies the file and the cfg.
#[cfg(callgraph_check)]
#[path = "callgraph_check.rs"]
mod callgraph_check;

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "target triple = \"arm64-apple-macosx11.0.0\"\n";

    fn run_text(text: &str, claims: Option<&Claims>, sinks: &Sinks) -> Result<String, String> {
        run_green(text, claims, sinks).map(|g| g.text)
    }

    fn run_green(text: &str, claims: Option<&Claims>, sinks: &Sinks) -> Result<Green, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("ir_no_block_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("m{}.ll", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, text).unwrap();
        run_ir(p.to_str().unwrap(), "//pkg:target", claims, sinks)
    }

    fn sinks(slow: &[&str], io: &[&str], allowed: &[&str]) -> Sinks {
        Sinks {
            paths: slow.iter().map(|t| super::super::SlowPath::parse(t).unwrap()).collect(),
            io: io.iter().map(|t| super::super::io::parse(t).unwrap()).collect(),
            wasi: false,
            allowed: allowed.iter().map(|t| t.to_string()).collect(),
        }
    }

    fn check(body: &str, slow: &[&str], io: &[&str]) -> Result<String, String> {
        run_text(&format!("{HEAD}{body}"), None, &sinks(slow, io, &[]))
    }

    const ROOT: &str = "define void @frustrate_check_block_example_1_render() {\n  call void @_RNvCs1_7example6render()\n  ret void\n}\n";

    #[test]
    fn a_member_that_locks_a_mutex_is_named_with_its_path() {
        let e = check(
            &format!(
                "{ROOT}define void @_RNvCs1_7example6render() {{\n  %1 = call i32 @pthread_mutex_lock(ptr %0)\n  ret void\n}}\n\
                 declare i32 @pthread_mutex_lock(ptr)\n"
            ),
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("1 `#[bridge(no_block)]` member(s) reach a wait"), "{e}");
        assert!(e.contains("reaches pthread_mutex_lock — waits"), "{e}");
        assert!(e.contains("example::render"), "{e}");
    }

    /// The inversion: an external nobody classified is not passed because it
    /// is missing from a list of waits.
    #[test]
    fn an_external_not_known_not_to_wait_cannot_be_verified() {
        let body = format!(
            "{ROOT}define void @_RNvCs1_7example6render() {{\n  call i32 @sqlite3_step(ptr %0)\n  call ptr @malloc(i64 8)\n  ret void\n}}\n\
             declare i32 @sqlite3_step(ptr)\ndeclare ptr @malloc(i64)\n"
        );
        let e = check(&body, &[], &[]).unwrap_err();
        assert!(e.contains("cannot verify: reaches sqlite3_step"), "{e}");
        assert!(!e.contains("reaches malloc"), "malloc is known not to wait: {e}");
        let ok = run_text(&format!("{HEAD}{body}"), None, &sinks(&[], &[], &["sqlite3_step"])).unwrap();
        assert!(ok.contains("`sqlite3_step` in every claim (the check target's `allow`)"), "{ok}");
    }

    /// A spin loop's yield is a wait, which the target's `allow` silences like any finding.
    #[test]
    fn sched_yield_is_a_wait() {
        let body = format!(
            "{ROOT}define void @_RNvCs1_7example6render() {{\n  call i32 @sched_yield()\n  ret void\n}}\n\
             declare i32 @sched_yield()\n"
        );
        let e = check(&body, &[], &[]).unwrap_err();
        assert!(e.contains("reaches sched_yield — waits"), "{e}");
        let ok = run_text(&format!("{HEAD}{body}"), None, &sinks(&[], &[], &["sched_yield"])).unwrap();
        assert!(ok.contains("`sched_yield` in every claim (the check target's `allow`)"), "{ok}");
    }

    #[test]
    fn a_wait_reached_through_a_vtable_is_attributed_to_the_member() {
        let e = check(
            &format!(
                "{ROOT}define void @_RNvCs1_7example6render() {{\n  store ptr @vtable.0, ptr %x\n  ret void\n}}\n\
                 @vtable.0 = private constant <{{ ptr }}> <{{ ptr @_RNvCs1_7example4lock }}>\n\
                 define void @_RNvCs1_7example4lock() {{\n  call i32 @pthread_mutex_lock(ptr %0)\n  ret void\n}}\n\
                 declare i32 @pthread_mutex_lock(ptr)\n"
            ),
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("example::lock   (address taken"), "{e}");
    }

    #[test]
    fn a_wait_behind_the_panic_handler_is_not_followed() {
        let ok = check(
            &format!(
                "{ROOT}define void @_RNvCs1_7example6render() {{\n  call void @rust_begin_unwind(ptr %p)\n  ret void\n}}\n\
                 define void @rust_begin_unwind(ptr %p) {{\n  call i32 @pthread_mutex_lock(ptr %0)\n  ret void\n}}\n\
                 declare i32 @pthread_mutex_lock(ptr)\n"
            ),
            &[],
            &[],
        )
        .unwrap();
        assert!(ok.contains("clean — 1 `no_block` root(s)"), "{ok}");
    }

    /// The bounded lock: `with` calls the acquire, then the section.
    fn bounded(section_body: &str, elsewhere: &str) -> String {
        format!(
            "{HEAD}define void @frustrate_check_block_example_1_render() {{\n  call void @_RNvCs1_9frustrate4with()\n  ret void\n}}\n\
             define void @frustrate_bounded_lock_acquire() {{\n  call i32 @pthread_mutex_lock(ptr %0)\n  ret void\n}}\n\
             define void @_RNvCs1_9frustrate4with() {{\n  call void @frustrate_bounded_lock_acquire()\n  call void @_RNvCs1_9frustrate7section()\n  call i32 @pthread_mutex_unlock(ptr %0)\n  ret void\n}}\n\
             define void @_RNvCs1_9frustrate7section() {{\n{section_body}  ret void\n}}\n\
             {elsewhere}\
             declare i32 @pthread_mutex_lock(ptr)\ndeclare i32 @pthread_mutex_unlock(ptr)\ndeclare i32 @nanosleep(ptr)\n"
        )
    }

    #[test]
    fn the_bounded_locks_wait_is_exempt_when_every_section_is_proven() {
        let ok = run_text(
            &bounded("  call ptr @malloc(i64 8)\n", "declare ptr @malloc(i64)\n"),
            None,
            &Sinks::default(),
        )
        .unwrap();
        assert!(ok.contains("the bounded lock's sections are all proven"), "{ok}");
    }

    #[test]
    fn a_section_that_waits_is_red_through_the_member() {
        let e = run_text(&bounded("  call i32 @nanosleep(ptr %0)\n", ""), None, &Sinks::default()).unwrap_err();
        assert!(e.contains("reaches nanosleep — waits"), "{e}");
    }

    /// A section the member never runs still holds the lock the member takes,
    /// which an exemption by module path would miss.
    #[test]
    fn a_section_elsewhere_that_waits_makes_every_member_taking_the_lock_unprovable() {
        let elsewhere = "define void @_RNvCs1_9frustrate5other() {\n  call void @frustrate_bounded_lock_acquire()\n  call i32 @nanosleep(ptr %0)\n  ret void\n}\n";
        let e = run_text(&bounded("", elsewhere), None, &Sinks::default()).unwrap_err();
        assert!(e.contains("cannot verify: takes the bounded lock, whose wait is unproven"), "{e}");
        assert!(e.contains("frustrate::other"), "{e}");
    }

    #[test]
    fn a_section_that_takes_the_lock_again_is_refused() {
        let nested = "  call void @_RNvCs1_9frustrate5inner()\n";
        let elsewhere = "define void @_RNvCs1_9frustrate5inner() {\n  call void @frustrate_bounded_lock_acquire()\n  ret void\n}\n";
        let e = run_text(&bounded(nested, elsewhere), None, &Sinks::default()).unwrap_err();
        assert!(e.contains("takes a bounded lock again"), "{e}");
    }

    /// frustrate's own code calling into `std::sync` gets no exemption by
    /// module path. Without the bounded lock's identity it is an ordinary wait.
    #[test]
    fn a_runtime_function_taking_a_std_lock_is_an_ordinary_wait() {
        let e = check(
            "define void @frustrate_check_block_example_1_render() {\n  call void @_RNvNtCs1_9frustrate8resident4live()\n  ret void\n}\n\
             define void @_RNvNtCs1_9frustrate8resident4live() {\n  call void @_RNvNtNtCs2_3std4sync4once9call_once()\n  ret void\n}\n\
             define void @_RNvNtNtCs2_3std4sync4once9call_once() {\n  call i32 @dispatch_semaphore_wait(ptr %0)\n  ret void\n}\n\
             declare i32 @dispatch_semaphore_wait(ptr)\n",
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("reaches dispatch_semaphore_wait — waits"), "{e}");
    }

    #[test]
    fn io_categories_and_paths_are_sinks_too() {
        let body = format!(
            "{ROOT}define void @_RNvCs1_7example6render() {{\n  call i32 @open(ptr %0)\n  ret void\n}}\ndeclare i32 @open(ptr)\n"
        );
        let e = check(&body, &[], &[]).unwrap_err();
        assert!(e.contains("cannot verify: reaches open"), "{e}");
        let e = check(&body, &[], &["files"]).unwrap_err();
        assert!(e.contains("reaches open — IO `files`"), "{e}");
        let e = check(&body, &["example::render"], &[]).unwrap_err();
        assert!(e.contains("named slow by `example::render`"), "{e}");
    }

    #[test]
    fn a_slow_path_that_names_no_function_is_refused() {
        let body = format!(
            "{ROOT}define void @_RNvCs1_7example6render() {{\n  ret void\n}}\n\
             define void @_RNvCs1_7example4slow() {{\n  ret void\n}}\n"
        );
        let ok = check(&body, &["example::slow"], &[]).unwrap();
        assert!(ok.contains("is clean"), "{ok}");
        let e = check(&body, &["example::slow", "example::slwo"], &[]).unwrap_err();
        assert!(e.contains("1 `slow` path(s) name no function"), "{e}");
        assert!(e.contains("`example::slwo`"), "{e}");
        assert!(!e.contains("`example::slow`"), "{e}");
    }

    #[test]
    fn an_at_sign_inside_a_string_constant_is_not_a_reference() {
        let syms = symbols(r#"@s = private constant [5 x i8] c"a@b\00x" ; @comment"#);
        assert_eq!(syms, vec![("s".to_string(), false)]);
        let syms = symbols(r#"  %3 = invoke i32 @"realpath$DARWIN_EXTSN"(ptr @x) to label %a"#);
        assert_eq!(
            syms,
            vec![("realpath$DARWIN_EXTSN".to_string(), true), ("x".to_string(), false)]
        );
    }

    #[test]
    fn a_semicolon_inside_an_asm_template_does_not_end_the_line() {
        // `asm!("nop; bl {f}", f = sym waiter)`, as rustc lowers it.
        let syms = symbols(
            r#"  %0 = call { i32, <2 x i64> } asm sideeffect alignstack "nop; bl ${1:c}", "={w0},={q0},s,~{memory}"(ptr @waiter), !srcloc !3"#,
        );
        assert_eq!(syms, vec![("waiter".to_string(), false)]);
        let syms = symbols(r#"  call void asm "; @not_a_symbol", ""() #1 ; @comment"#);
        assert_eq!(syms, vec![]);
    }

    #[test]
    fn an_asm_template_is_read_from_rustcs_lowering() {
        let line = r#"  %0 = call { i32, <2 x i64> } asm sideeffect alignstack "nop; bl ${1:c}", "={w0},={q0},s,~{memory}"(ptr @waiter), !srcloc !3"#;
        assert_eq!(inline_asm(line), Some("nop; bl ${1:c}"));
        assert_eq!(inline_asm(r#"  call void asm sideeffect "", "~{memory}"(), !srcloc !59339"#), None);
        assert_eq!(inline_asm(r#"  store ptr @"asm \22x\22", ptr %p"#), None);
    }

    #[test]
    fn a_wait_reached_from_an_asm_operand_is_found() {
        let e = check(
            &format!(
                "{ROOT}define void @_RNvCs1_7example6render() {{\n  \
                 call void asm sideeffect \"nop; bl ${{0:c}}\", \"s\"(ptr @_RNvCs1_7example6waiter)\n  ret void\n}}\n\
                 define void @_RNvCs1_7example6waiter() {{\n  call i32 @sched_yield()\n  ret void\n}}\n\
                 declare i32 @sched_yield()\n"
            ),
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("reaches sched_yield — waits"), "{e}");
    }

    /// An asm template can wait without naming a symbol: `svc` here, or a
    /// `bl` written into the template. Only an empty template is passed.
    #[test]
    fn inline_asm_with_instructions_cannot_be_verified() {
        let body = |asm: &str| {
            format!(
                "{ROOT}define void @_RNvCs1_7example6render() {{\n  \
                 {asm}\n  ret void\n}}\n"
            )
        };
        let e = check(&body(r#"call void asm sideeffect "svc #0x80", "~{memory}"()"#), &[], &[]).unwrap_err();
        assert!(e.contains(r#"cannot verify: reaches asm "svc #0x80""#), "{e}");
        let e = check(&body(r#"%0 = call i32 asm sideeffect "bl _usleep", "={w0}"()"#), &[], &[]).unwrap_err();
        assert!(e.contains(r#"asm "bl _usleep""#), "{e}");
        check(&body(r#"call void asm sideeffect "", "r,~{memory}"(ptr %p)"#), &[], &[]).unwrap();
        let ok = run_text(
            &format!("{HEAD}{}", body(r#"call void asm sideeffect "svc #0x80", "~{memory}"()"#)),
            None,
            &sinks(&[], &[], &[r#"asm "svc #0x80""#]),
        )
        .unwrap();
        assert!(ok.contains("is clean"), "{ok}");
    }

    /// std_detect's `cpuid` passes on x86_64; the same template with another
    /// instruction in it, or on another architecture, does not.
    #[test]
    fn a_known_asm_template_passes_on_its_architecture_only() {
        let cpuid = r#"%0 = call i64 asm sideeffect inteldialect "mov ${0:q}, rbx\0Acpuid\0Axchg ${0:q}, rbx", "=r,={ax},={cx},={dx},0,1,2"(i32 0, i32 0)"#;
        let ok = run_text(&linux("x86_64-unknown-linux-gnu", &format!("  {cpuid}\n")), None, &Sinks::default()).unwrap();
        assert!(ok.contains("is clean"), "{ok}");
        let more = cpuid.replace("\\0Axchg", "\\0Asyscall\\0Axchg");
        let e = run_text(&linux("x86_64-unknown-linux-gnu", &format!("  {more}\n")), None, &Sinks::default()).unwrap_err();
        assert!(e.contains(r#"cannot verify: reaches asm "mov ${0:q}, rbx\0Acpuid\0Asyscall"#), "{e}");
        let e = run_text(&linux("aarch64-unknown-linux-gnu", &format!("  {cpuid}\n")), None, &Sinks::default()).unwrap_err();
        assert!(e.contains(r#"cannot verify: reaches asm "mov ${0:q}, rbx\0Acpuid"#), "{e}");
    }

    /// zeroize's barrier is a comment on both architectures; a comment
    /// beside an instruction still leaves the instruction.
    #[test]
    fn a_template_of_comments_only_is_no_instruction() {
        let line = |t: &str| format!(r#"  call void asm sideeffect "{t}", "r,~{{memory}}"(ptr %p), !srcloc !7"#);
        assert_eq!(inline_asm(&line("# ${0}")), None);
        assert_eq!(inline_asm(&line("# ${0:q}")), None);
        assert_eq!(inline_asm(&line("// ${0}\\0A# ${0}")), None);
        assert_eq!(inline_asm(&line("# ${0}\\0Asvc #0x80")), Some("# ${0}\\0Asvc #0x80"));
        assert_eq!(inline_asm(&line("# ${0}; svc #0x80")), Some("# ${0}; svc #0x80"));
    }

    /// half's `fcvt` passes on aarch64 only, and only alone.
    #[test]
    fn halfs_fcvt_passes_on_aarch64() {
        for t in ["fcvt ${0:h}, ${1:d}", "fcvt ${0:d}, ${1:h}"] {
            let call = format!(r#"%0 = call double asm sideeffect "{t}", "=w,w"(half %h)"#);
            let ok = run_text(&linux("aarch64-unknown-linux-gnu", &format!("  {call}\n")), None, &Sinks::default()).unwrap();
            assert!(ok.contains("is clean"), "{ok}");
            let e = run_text(&linux("x86_64-unknown-linux-gnu", &format!("  {call}\n")), None, &Sinks::default()).unwrap_err();
            assert!(e.contains("cannot verify"), "{e}");
        }
        // Apple's triple names the architecture `arm64`.
        let call = r#"%0 = call double asm sideeffect "fcvt ${0:h}, ${1:d}", "=w,w"(half %h)"#;
        check(
            &format!("{ROOT}define void @_RNvCs1_7example6render() {{\n  {call}\n  ret void\n}}\n"),
            &[],
            &[],
        )
        .unwrap();
        let more = r#"%0 = call double asm sideeffect "fcvt ${0:h}, ${1:d}\0Asvc #0", "=w,w"(half %h)"#;
        let e = run_text(&linux("aarch64-unknown-linux-gnu", &format!("  {more}\n")), None, &Sinks::default()).unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
    }

    #[test]
    fn a_module_that_only_declares_a_rust_function_is_refused() {
        let e = check(
            &format!(
                "{ROOT}define void @_RNvCs1_7example6render() {{\n  call void @_RNvNtCs2_3std4sync4lock()\n  ret void\n}}\n\
                 declare void @_RNvNtCs2_3std4sync4lock()\n"
            ),
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("not the whole program"), "{e}");
        assert!(e.contains("std::sync::lock"), "{e}");
    }

    fn census(rows: &str) -> Claims {
        Claims::parse(&format!("{}\ncrate example\n{rows}\n", super::super::CENSUS_HEADER), "c").unwrap()
    }

    #[test]
    fn a_root_that_does_not_reach_its_member_is_refused() {
        let claims = census("artifact 1 render crate::api::render");
        // What an optimizer that proved the decode fails leaves behind.
        let folded = format!(
            "{HEAD}define void @frustrate_check_block_example_1_render(ptr %r, i64 %l, i64 %h) {{\n  call void @_RNvNtCs1_7example19frustrate_generated13call_1_render()\n  ret void\n}}\n\
             define void @_RNvNtCs1_7example19frustrate_generated13call_1_render() {{\n  ret void\n}}\n"
        );
        let e = run_text(&folded, Some(&claims), &Sinks::default()).unwrap_err();
        assert!(e.contains("cannot verify: does not reach its own function `crate::api::render`"), "{e}");
        let kept = format!(
            "{HEAD}define void @frustrate_check_block_example_1_render(ptr %r, i64 %l, i64 %h) {{\n  call void @_RNvNtCs1_7example19frustrate_generated13call_1_render()\n  ret void\n}}\n\
             define void @_RNvNtCs1_7example19frustrate_generated13call_1_render() {{\n  call void @_RNvNtCs1_7example3api6render()\n  ret void\n}}\n\
             define void @_RNvNtCs1_7example3api6render() {{\n  ret void\n}}\n"
        );
        let ok = run_text(&kept, Some(&claims), &Sinks::default());
        assert!(ok.is_ok(), "{ok:?}");
    }

    #[test]
    fn a_member_is_matched_by_its_path() {
        let v = |s: &str| s.split("::").map(str::to_string).collect::<Vec<_>>();
        let want = v("crate::api::render");
        assert!(is_member(&v("example::api::render"), "example", &want, &["render"]));
        assert!(!is_member(&v("example::other::render"), "example", &want, &["render"]));
        assert!(!is_member(&v("example::api::render::{closure#0}"), "example", &want, &["render"]));
        // A method of a type defined outside the module holding its impl.
        let want = v("crate::api::Doc::render");
        assert!(is_member(&v("example::model::Doc::render"), "example", &want, &["Doc", "render"]));
        assert!(!is_member(&v("other::api::Doc::render"), "example", &want, &["Doc", "render"]));
        assert!(!is_member(&v("example::api::Page::render"), "example", &want, &["Doc", "render"]));
    }

    /// A `dyn` member's own function is wherever the handle's vtable points:
    /// every implementation of the trait method in the program is a callee.
    #[test]
    fn a_dyn_members_implementations_are_its_callees() {
        let claims = census("artifact 1 Shape::area crate::api::Shape::area");
        let text = format!(
            "{HEAD}define void @frustrate_check_block_example_1_area(ptr %r, i64 %l, i64 %h) {{\n  call void @_RNvNtCs1_7example19frustrate_generated11call_1_area()\n  ret void\n}}\n\
             define void @_RNvNtCs1_7example19frustrate_generated11call_1_area() {{\n  %f = load ptr, ptr %v\n  call void %f()\n  ret void\n}}\n\
             @vtable.0 = private constant <{{ ptr }}> <{{ ptr @_RNvXs_NtCs1_7example3apiNtB4_6CircleNtB4_5Shape4area }}>\n\
             define void @_RNvXs_NtCs1_7example3apiNtB4_6CircleNtB4_5Shape4area() {{\n  call i32 @nanosleep(ptr %0)\n  ret void\n}}\n\
             declare i32 @nanosleep(ptr)\n"
        );
        let e = run_text(&text, Some(&claims), &Sinks::default()).unwrap_err();
        assert!(e.contains("Shape::area\n      reaches nanosleep — waits"), "{e}");
    }


    /// The check target's silence names what the finding names, and the
    /// verdict lists it with its reason.
    #[test]
    fn a_target_silence_is_applied_and_listed() {
        let text = format!(
            "{HEAD}{ROOT}define void @_RNvCs1_7example6render() {{\n  call i32 @sqlite3_step(ptr null)\n  ret void\n}}\n\
             declare i32 @sqlite3_step(ptr)\n"
        );
        let e = run_text(&text, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("silence it as `sqlite3_step`"), "{e}");
        let ok = run_text(&text, None, &sinks_allowing(&["sqlite3_step"])).unwrap();
        assert!(ok.contains("`sqlite3_step` in every claim (the check target's `allow`)"), "{ok}");
    }

    fn sinks_allowing(t: &[&str]) -> Sinks {
        Sinks {
            allowed: t.iter().map(|a| a.to_string()).collect(),
            ..Sinks::default()
        }
    }

    /// A silence that silences nothing here is not applied; whether another
    /// analysis applies it is `--silences`' question.
    #[test]
    fn a_silence_that_silences_nothing_here_is_not_applied() {
        let text = format!("{HEAD}{ROOT}define void @_RNvCs1_7example6render() {{\n  ret void\n}}\n");
        let g = run_green(&text, None, &sinks_allowing(&["sqlite3_step"])).unwrap();
        assert!(!g.text.contains("Silenced"), "{}", g.text);
        assert!(g.applied.silences.is_empty());
        let g = run_green(
            &format!("{HEAD}{ROOT}define void @_RNvCs1_7example6render() {{\n  call i32 @sqlite3_step(ptr null)\n  ret void\n}}\ndeclare i32 @sqlite3_step(ptr)\n"),
            None,
            &sinks_allowing(&["sqlite3_step"]),
        )
        .unwrap();
        assert!(g.applied.silences.contains(&Silence::Target("sqlite3_step".into())));
    }

    /// A member's own silence covers that member and no other.
    #[test]
    fn a_member_silence_covers_its_member_only() {
        let claims = Claims::parse(
            &format!(
                "{}\ncrate example\nartifact 1 a crate::a\nallow 1 sqlite3_step\nartifact 2 b crate::b\n",
                super::super::CENSUS_HEADER
            ),
            "c",
        )
        .unwrap();
        let root = |id: u32, name: &str| {
            format!(
                "define void @frustrate_check_block_example_{id}_{name}(ptr %r, i64 %l, i64 %h) {{\n  call void @_RNvNtCs1_7example19frustrate_generated8call_{id}_{name}()\n  ret void\n}}\n\
                 define internal void @_RNvNtCs1_7example19frustrate_generated8call_{id}_{name}() {{\n  call void @_RNvCs1_7example1{name}()\n  ret void\n}}\n\
                 define internal void @_RNvCs1_7example1{name}() {{\n  call i32 @sqlite3_step(ptr null)\n  ret void\n}}\n"
            )
        };
        let text = format!("{HEAD}{}{}declare i32 @sqlite3_step(ptr)\n", root(1, "a"), root(2, "b"));
        let e = run_text(&text, Some(&claims), &Sinks::default()).unwrap_err();
        assert!(e.contains("0 `#[bridge(no_block)]` member(s) reach a wait and 1 cannot be verified"), "{e}");
        assert!(e.contains("  b\n      cannot verify: reaches sqlite3_step"), "{e}");
    }

    /// Two bridges linked into one program, each with a member `a` numbered 1
    /// that calls a C function. One bridge's silence on its member is not the
    /// other's, and a finding names the bridge it is in.
    #[test]
    fn linked_bridges_keep_their_own_silences_and_roots() {
        let bridge = |krate: &str, allow: &str| {
            Claims::parse(
                &format!(
                    "{}\ncrate {krate}\nartifact 1 a crate::a\n{allow}",
                    super::super::CENSUS_HEADER
                ),
                &format!("{krate}.claims"),
            )
            .unwrap()
        };
        let root = |krate: &str| {
            let n = krate.len();
            format!(
                "define void @frustrate_check_block_{krate}_1_a(ptr %r, i64 %l, i64 %h) {{\n  call void @_RNvNtCs1_{n}{krate}19frustrate_generated8call_1_a()\n  ret void\n}}\n\
                 define internal void @_RNvNtCs1_{n}{krate}19frustrate_generated8call_1_a() {{\n  call void @_RNvCs1_{n}{krate}1a()\n  ret void\n}}\n\
                 define internal void @_RNvCs1_{n}{krate}1a() {{\n  call i32 @sqlite3_step(ptr null)\n  ret void\n}}\n"
            )
        };
        let text = format!("{HEAD}{}{}declare i32 @sqlite3_step(ptr)\n", root("example"), root("app"));
        let merge = |example: Claims, app: Claims| {
            Claims::merge(vec![("example.claims".into(), example), ("app.claims".into(), app)]).unwrap()
        };

        // Only `example` silences it: `app`'s member, numbered the same, is
        // still the one finding, and is named with its bridge.
        let claims = merge(bridge("example", "allow 1 sqlite3_step\n"), bridge("app", ""));
        let e = run_text(&text, Some(&claims), &Sinks::default()).unwrap_err();
        assert!(e.contains("0 `#[bridge(no_block)]` member(s) reach a wait and 1 cannot be verified"), "{e}");
        assert!(e.contains("  app: a\n      cannot verify: reaches sqlite3_step"), "{e}");
        assert!(!e.contains("example: a"), "{e}");

        // Both silence their own, and each silence is applied under its own
        // bridge's id.
        let claims = merge(
            bridge("example", "allow 1 sqlite3_step\n"),
            bridge("app", "allow 1 sqlite3_step\n"),
        );
        let g = run_green(&text, Some(&claims), &Sinks::default()).unwrap();
        assert!(g.applied.silences.contains(&Silence::Member("example:1".into(), "sqlite3_step".into())));
        assert!(g.applied.silences.contains(&Silence::Member("app:1".into(), "sqlite3_step".into())));

        // A root of a linked bridge that no census names is not a pass.
        let only = bridge("example", "allow 1 sqlite3_step\n");
        let e = run_text(&text, Some(&only), &Sinks::default()).unwrap_err();
        assert!(e.contains("frustrate_check_block_app_1_a — carries the crate of none of `example`"), "{e}");
    }


    /// `#[frustrate::no_block(allow)]`: a function that calls the marker first.
    const MARK: &str = "define void @frustrate_no_block_allow() {\n  ret void\n}\n";

    fn marked(render_calls: &str, wrap_body: &str) -> String {
        format!(
            "{HEAD}{ROOT}define void @_RNvCs1_7example6render() {{\n{render_calls}  ret void\n}}\n\
             define void @_RNvCs1_7example4wrap() {{\n  call void @frustrate_no_block_allow()\n{wrap_body}  ret void\n}}\n\
             {MARK}declare i32 @pthread_mutex_lock(ptr)\n"
        )
    }

    #[test]
    fn a_marked_function_is_a_stop_point_and_the_silence_is_listed() {
        let wait = "  call i32 @pthread_mutex_lock(ptr %0)\n";
        let call = "  call void @_RNvCs1_7example4wrap()\n";
        let ok = run_text(&marked(call, wait), None, &Sinks::default()).unwrap();
        assert!(ok.contains("clean"), "{ok}");
        assert!(ok.contains("`example::wrap` in every claim (`#[frustrate::no_block(allow)]`)"), "{ok}");
        // The same wait reached around the marked function is still a finding,
        // and the marker is never one itself.
        let both = format!("{call}{wait}");
        let e = run_text(&marked(&both, wait), None, &Sinks::default()).unwrap_err();
        assert!(e.contains("reaches pthread_mutex_lock — waits"), "{e}");
        assert!(!e.contains("frustrate_no_block_allow"), "{e}");
    }

    #[test]
    fn a_marked_function_whose_subtree_holds_no_finding_is_idle() {
        let g = run_green(
            &marked("  call void @_RNvCs1_7example4wrap()\n", "  call ptr @malloc(i64 8)\n"),
            None,
            &Sinks::default(),
        )
        .unwrap();
        assert!(g.applied.silences.is_empty());
        assert!(g.applied.idle_marks.contains("example::wrap"));
    }

    #[test]
    fn a_marked_function_no_claim_reaches_is_ignored() {
        let ok = run_text(&marked("", "  call i32 @pthread_mutex_lock(ptr %0)\n"), None, &Sinks::default()).unwrap();
        assert!(ok.contains("clean"), "{ok}");
        assert!(!ok.contains("Silenced"), "{ok}");
    }

    /// Every instance of a generic function carries its own marker call; the
    /// silence is one entry by path, and holds if any instance does.
    #[test]
    fn a_generic_marked_function_is_one_silence_across_its_instances() {
        let text = format!(
            "{HEAD}{ROOT}define void @_RNvCs1_7example6render() {{\n  call void @_RINvCs1_7example4wrapNtNtCs2_3std2fs4FileE()\n  call void @_RINvCs1_7example4wrapNtNtCs2_3std2fs4PathE()\n  ret void\n}}\n\
             define void @_RINvCs1_7example4wrapNtNtCs2_3std2fs4FileE() {{\n  call void @frustrate_no_block_allow()\n  call i32 @pthread_mutex_lock(ptr %0)\n  ret void\n}}\n\
             define void @_RINvCs1_7example4wrapNtNtCs2_3std2fs4PathE() {{\n  call void @frustrate_no_block_allow()\n  ret void\n}}\n\
             {MARK}declare i32 @pthread_mutex_lock(ptr)\n"
        );
        let ok = run_text(&text, None, &Sinks::default()).unwrap();
        assert_eq!(ok.matches("`example::wrap` in every claim").count(), 1, "{ok}");
    }

    /// The bounded lock's section proof stops at a marked function, as a
    /// target silence applies to it.
    #[test]
    fn the_bounded_locks_section_proof_stops_at_a_marked_function() {
        let text = bounded(
            "  call void @_RNvCs1_7example4wrap()\n",
            &format!(
                "define void @_RNvCs1_7example4wrap() {{\n  call void @frustrate_no_block_allow()\n  call i32 @nanosleep(ptr %0)\n  ret void\n}}\n{MARK}"
            ),
        );
        let ok = run_text(&text, None, &Sinks::default()).unwrap();
        assert!(ok.contains("the bounded lock's sections are all proven"), "{ok}");
        assert!(ok.contains("`example::wrap` in every claim"), "{ok}");
    }

    /// A call through a value loaded from a static: someone else installed
    /// it, so the check cannot say what it is.
    #[test]
    fn a_call_through_a_value_loaded_from_a_static_cannot_be_verified() {
        let text = format!(
            "{HEAD}@SLOT = internal global ptr null\n\
             define void @frustrate_check_block_example_1_render() {{\n  call void @_RNvCs1_7example6render()\n  ret void\n}}\n\
             define internal void @_RNvCs1_7example6render() {{\n  %f = load ptr, ptr @SLOT\n  call void %f()\n  ret void\n}}\n"
        );
        let e = run_text(&text, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("cannot verify: reaches an indirect call in example::render"), "{e}");
        assert!(e.contains("loaded from `SLOT`"), "{e}");
        assert!(e.contains("silence it as `example::render`"), "{e}");
        let ok = run_text(&text, None, &sinks_allowing(&["example::render"])).unwrap();
        assert!(ok.contains("`example::render` in every claim"), "{ok}");
    }

    /// A thread-local kept in a pthread key (Android's) is a static's contents,
    /// as a native thread-local is: a call through what is loaded from it
    /// cannot be verified, and says where it came from.
    #[test]
    fn a_call_through_a_thread_local_kept_in_a_pthread_key_cannot_be_verified() {
        let text = "target triple = \"aarch64-unknown-linux-android\"\n\
             define void @frustrate_check_block_example_1_render() {\n  call void @_RNvCs1_7example6render()\n  ret void\n}\n\
             define internal void @_RNvCs1_7example6render() {\n  %b = call ptr @pthread_getspecific(i32 %k)\n  %f = load ptr, ptr %b\n  call void %f()\n  ret void\n}\n\
             declare ptr @pthread_getspecific(i32)\n";
        let e = run_text(text, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("cannot verify: reaches an indirect call in example::render"), "{e}");
        assert!(e.contains("loaded from `thread-locals kept in pthread keys`"), "{e}");
    }

    /// A pointer that may be any of more stack slots than the analysis names
    /// one by one is anywhere in the frame: what is stored through it is
    /// seen by a load from each slot, so the finding stands.
    #[test]
    fn a_store_through_a_pointer_to_many_slots_reaches_every_load() {
        let n = 40;
        let allocas: String = (0..n).map(|i| format!("  %a{i} = alloca [8 x i8], align 8\n")).collect();
        let phi: Vec<String> = (0..n).map(|i| format!("[ %a{i}, %start ]")).collect();
        let text = format!(
            "{HEAD}@SLOT = internal global ptr null\n\
             define void @frustrate_check_block_example_1_render() {{\n  call void @_RNvCs1_7example6render()\n  ret void\n}}\n\
             define internal void @_RNvCs1_7example6render() {{\nstart:\n{allocas}  br label %l\nl:\n  \
             %p = phi ptr {}\n  %v = load ptr, ptr @SLOT\n  store ptr %v, ptr %p, align 8\n  \
             %f = load ptr, ptr %a7, align 8\n  call void %f()\n  ret void\n}}\n",
            phi.join(", ")
        );
        let e = run_text(&text, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("an indirect call in example::render"), "{e}");
        assert!(e.contains("loaded from `SLOT`"), "{e}");
    }

    /// A value that may come from more statics than the analysis names one by
    /// one is still a finding, worded as such.
    #[test]
    fn a_value_from_many_statics_cannot_be_verified() {
        let n = 20;
        let statics: String = (0..n).map(|i| format!("@S{i} = internal global ptr null\n")).collect();
        let loads: String = (0..n).map(|i| format!("  %v{i} = load ptr, ptr @S{i}\n")).collect();
        let vs: Vec<String> = (0..n).map(|i| format!("ptr %v{i}")).collect();
        let text = format!(
            "{HEAD}{statics}\
             define void @frustrate_check_block_example_1_render() {{\n  call void @_RNvCs1_7example6render()\n  ret void\n}}\n\
             define internal void @_RNvCs1_7example6render() {{\n{loads}  %f = phi ptr {}\n  call void %f()\n  ret void\n}}\n",
            vs.iter().map(|v| format!("[ {}, %x ]", &v[4..])).collect::<Vec<_>>().join(", ")
        );
        let e = run_text(&text, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("an indirect call in example::render"), "{e}");
        assert!(e.contains("more than 16 statics and external functions"), "{e}");
    }

    /// A function pointer the member keeps in a local of its own is one it
    /// produced.
    #[test]
    fn a_call_through_a_pointer_the_member_stored_itself_is_followed() {
        let text = format!(
            "{HEAD}define void @frustrate_check_block_example_1_render() {{\n  %slot = alloca [8 x i8], align 8\n  store ptr @_RNvCs1_7example4work, ptr %slot, align 8\n  %f = load ptr, ptr %slot, align 8\n  call void %f()\n  ret void\n}}\n\
             define internal void @_RNvCs1_7example4work() {{\n  ret void\n}}\n"
        );
        let ok = run_text(&text, None, &Sinks::default()).unwrap();
        assert!(ok.contains("clean"), "{ok}");
    }

    /// An accessor that returns what a static holds (`log::logger()`'s shape)
    /// carries where it came from to the call in its caller.
    #[test]
    fn a_value_returned_from_a_static_by_an_accessor_keeps_where_it_came_from() {
        let text = format!(
            "{HEAD}@LOGGER = internal global ptr null\n\
             define internal ptr @_RNvCs1_3log6logger() {{\n  %l = load ptr, ptr @LOGGER\n  ret ptr %l\n}}\n\
             define void @frustrate_check_block_example_1_render() {{\n  %l = call ptr @_RNvCs1_3log6logger()\n  %f = load ptr, ptr %l\n  call void %f()\n  ret void\n}}\n"
        );
        let e = run_text(&text, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("loaded from `LOGGER`"), "{e}");
    }

    /// A helper that calls whatever it is handed is reported only where a
    /// caller hands it a value from a static, and the finding names the
    /// helper's call.
    #[test]
    fn a_helper_is_reported_in_the_context_that_hands_it_a_slots_value() {
        let helper = "define internal void @_RNvCs1_7example6invoke(ptr %f) {\n  call void %f()\n  ret void\n}\n";
        let own = format!(
            "{HEAD}{helper}define void @frustrate_check_block_example_1_render() {{\n  call void @_RNvCs1_7example6invoke(ptr @_RNvCs1_7example4work)\n  ret void\n}}\n\
             define internal void @_RNvCs1_7example4work() {{\n  ret void\n}}\n"
        );
        assert!(run_text(&own, None, &Sinks::default()).is_ok());
        let slot = format!(
            "{HEAD}@SLOT = internal global ptr null\n{helper}\
             define void @frustrate_check_block_example_1_render() {{\n  %f = load ptr, ptr @SLOT\n  call void @_RNvCs1_7example6invoke(ptr %f)\n  ret void\n}}\n"
        );
        let e = run_text(&slot, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("an indirect call in example::invoke"), "{e}");
        assert!(e.contains("loaded from `SLOT`"), "{e}");
    }

    /// A slot frustrate vouches for is not a finding.
    #[test]
    fn a_known_slot_is_not_a_finding() {
        let text = format!(
            "{HEAD}@_RNvNtNtNtCs1_4core2io5error12os_functions12OS_FUNCTIONS = internal global ptr null\n\
             define void @frustrate_check_block_example_1_render() {{\n  %f = load ptr, ptr @_RNvNtNtNtCs1_4core2io5error12os_functions12OS_FUNCTIONS\n  call void %f()\n  ret void\n}}\n"
        );
        assert!(run_text(&text, None, &Sinks::default()).is_ok());
    }

    /// A Linux module whose member makes `syscall`s: what each is read as.
    fn linux(triple: &str, calls: &str) -> String {
        format!(
            "target triple = \"{triple}\"\n{ROOT}define void @_RNvCs1_7example6render() {{\n{calls}  ret void\n}}\n\
             declare i64 @syscall(i64 noundef, ...)\n"
        )
    }

    #[test]
    fn a_futex_wake_passes_and_a_futex_wait_is_red_on_both_linux_libraries() {
        for (triple, futex) in [
            ("x86_64-unknown-linux-gnu", 202),
            ("aarch64-unknown-linux-gnu", 98),
            ("x86_64-unknown-linux-android", 202),
            ("aarch64-unknown-linux-android", 98),
        ] {
            let wake = format!("  %1 = call i64 (i64, ...) @syscall(i64 noundef {futex}, ptr %0, i32 noundef 129, i32 noundef 1)\n");
            assert!(run_text(&linux(triple, &wake), None, &Sinks::default()).is_ok(), "{triple}");
            let wait = format!(
                "  %2 = call i64 (i64, ...) @syscall(i64 noundef {futex}, ptr %0, i32 noundef 137, i32 noundef 0, ptr null, ptr null, i32 noundef -1)\n"
            );
            let e = run_text(&linux(triple, &format!("{wake}{wait}")), None, &Sinks::default()).unwrap_err();
            assert!(e.contains("reaches syscall::SYS_futex::FUTEX_WAIT_BITSET — waits"), "{triple}: {e}");
            let ok = run_text(
                &linux(triple, &wait),
                None,
                &sinks(&[], &[], &["syscall::SYS_futex::FUTEX_WAIT_BITSET"]),
            );
            assert!(ok.is_ok(), "{triple}: the finding is silenced by the name it gives");
        }
    }

    #[test]
    fn a_syscall_whose_number_is_not_a_constant_cannot_be_verified() {
        let e = run_text(
            &linux("x86_64-unknown-linux-gnu", "  %1 = call i64 (i64, ...) @syscall(i64 noundef %n)\n"),
            None,
            &Sinks::default(),
        )
        .unwrap_err();
        assert!(e.contains("cannot verify: reaches syscall::unknown"), "{e}");
    }

    /// A kernel call std makes through `syscall` is in its IO class by name.
    #[test]
    fn a_syscall_is_in_an_io_class() {
        let e = run_text(
            &linux("aarch64-unknown-linux-gnu", "  %1 = call i64 (i64, ...) @syscall(i64 noundef 291, i32 %d, ptr %p, i32 0, i32 4095, ptr %b)\n"),
            None,
            &sinks(&[], &["files"], &[]),
        )
        .unwrap_err();
        assert!(e.contains("reaches syscall::SYS_statx — IO `files`"), "{e}");
    }

    /// The bounded lock's acquire on Linux waits in std's futex, which is the
    /// one wait its exemption allows.
    #[test]
    fn the_bounded_locks_futex_wait_is_exempt_on_linux() {
        let text = "target triple = \"x86_64-unknown-linux-gnu\"\n\
             define void @frustrate_check_block_example_1_render() {\n  call void @_RNvCs1_7example6render()\n  ret void\n}\n\
             define void @_RNvCs1_7example6render() {\n  call void @_RNvCs1_9frustrate4with()\n  ret void\n}\n\
             define void @frustrate_bounded_lock_acquire() {\n  %1 = call i64 (i64, ...) @syscall(i64 noundef 202, ptr %0, i32 noundef 137, i32 noundef 0, ptr null, ptr null, i32 noundef -1)\n  ret void\n}\n\
             define void @_RNvCs1_9frustrate4with() {\n  call void @frustrate_bounded_lock_acquire()\n  %1 = call i64 (i64, ...) @syscall(i64 noundef 202, ptr %0, i32 noundef 129, i32 noundef 1)\n  ret void\n}\n\
             declare i64 @syscall(i64 noundef, ...)\n";
        let ok = run_text(text, None, &Sinks::default()).unwrap();
        assert!(ok.contains("the bounded lock's sections are all proven"), "{ok}");
        // Apple's acquire wait is no exemption on Linux.
        let apple = text.replace(
            "%1 = call i64 (i64, ...) @syscall(i64 noundef 202, ptr %0, i32 noundef 137, i32 noundef 0, ptr null, ptr null, i32 noundef -1)",
            "%1 = call i32 @pthread_mutex_lock(ptr %0)",
        ) + "declare i32 @pthread_mutex_lock(ptr)\n";
        let e = run_text(&apple, None, &Sinks::default()).unwrap_err();
        assert!(e.contains("the acquire itself reaches pthread_mutex_lock"), "{e}");
    }

    #[test]
    fn a_target_without_a_classified_c_library_is_refused() {
        let e = run_text(
            &format!("target triple = \"x86_64-unknown-linux-musl\"\n{ROOT}"),
            None,
            &Sinks::default(),
        )
        .unwrap_err();
        assert!(e.contains("only for Apple's libSystem, Android's bionic, glibc"), "{e}");
    }

    /// std's `HashMap` keys on Linux: what std's fallback reaches is vouched
    /// for, as the only way there, and only that.
    #[test]
    fn std_hashmap_keys_are_vouched_for_on_linux_and_only_their_externals() {
        let keys = "_ZN3std3sys6random5linux19hashmap_random_keys17h0123456789abcdefE";
        let module = |render: &str, keys_body: &str| {
            linux(
                "x86_64-unknown-linux-gnu",
                &format!("  call void @{keys}()\n{render}"),
            ) + &format!(
                "define internal void @{keys}() {{\n{keys_body}  ret void\n}}\n\
                 declare i32 @open64(ptr, i32, ...)\ndeclare i64 @read(i32, ptr, i64)\n\
                 declare i32 @poll(ptr, i64, i32)\ndeclare i32 @close(i32)\n\
                 declare i32 @nanosleep(ptr, ptr)\n"
            )
        };
        let fallback = "  %fd = call i32 (ptr, i32, ...) @open64(ptr null, i32 0)\n  \
                        %n = call i64 @read(i32 %fd, ptr null, i64 16)\n  \
                        %p = call i32 @poll(ptr null, i64 1, i32 -1)\n  \
                        %c = call i32 @close(i32 %fd)\n";
        let ok = run_text(&module("", fallback), None, &Sinks::default()).unwrap();
        assert!(ok.contains("is clean"), "{ok}");
        let ok = run_text(&module("", fallback), None, &sinks(&[], &["files"], &[])).unwrap();
        assert!(ok.contains("is clean"), "{ok}");
        // Reached another way, a vouched-for external is a finding as anywhere.
        let e = run_text(
            &module("  %r = call i64 @read(i32 0, ptr null, i64 16)\n", fallback),
            None,
            &sinks(&[], &["files"], &[]),
        )
        .unwrap_err();
        assert!(e.contains("reaches read — IO `files`"), "{e}");
        // An external the vouch does not name is a finding beneath it too.
        let e = run_text(
            &module("", &format!("{fallback}  %s = call i32 @nanosleep(ptr null, ptr null)\n")),
            None,
            &Sinks::default(),
        )
        .unwrap_err();
        assert!(e.contains("reaches nanosleep — waits"), "{e}");
    }

    /// A static holding one of a known set of functions, as memchr's runtime
    /// CPU dispatch keeps one: the calls through it are its functions'.
    fn dispatch(global: &str, extra: &str) -> String {
        format!(
            "{HEAD}{ROOT}{global}\n\
             define internal void @_RNvCs1_7example6render() {{\n  %f = load atomic ptr, ptr @FN monotonic, align 8\n  call void %f()\n  ret void\n}}\n\
             define internal void @detect() {{\n  %slot = alloca [8 x i8], align 8\n  \
               %c = call i1 @has_wide()\n  br i1 %c, label %a, label %b\n\
             a:\n  store ptr @wide, ptr %slot, align 8\n  br label %d\n\
             b:\n  store ptr @narrow, ptr %slot, align 8\n  br label %d\n\
             d:\n  %fun = load ptr, ptr %slot, align 8\n  call void @store_fn(ptr align 8 @FN, ptr %fun)\n  \
               call void %fun()\n  ret void\n}}\n\
             define internal void @store_fn(ptr %0, ptr %1) {{\n  store atomic ptr %1, ptr %0 monotonic, align 8\n  ret void\n}}\n\
             define internal i1 @has_wide() {{\n  ret i1 true\n}}\n\
             define internal void @wide() {{\n  ret void\n}}\n\
             define internal void @narrow() {{\n  call i32 @pthread_mutex_lock(ptr null)\n  ret void\n}}\n\
             declare i32 @pthread_mutex_lock(ptr)\n{extra}"
        )
    }

    #[test]
    fn a_call_through_a_dispatch_table_follows_each_function_stored_in_it() {
        let e = run_text(&dispatch("@FN = internal global ptr @detect, align 8", ""), None, &Sinks::default())
            .unwrap_err();
        // Not "cannot verify": the stored functions are followed, so the one
        // that locks is the finding.
        assert!(!e.contains("cannot verify:"), "{e}");
        assert!(e.contains("reaches pthread_mutex_lock — waits"), "{e}");
        assert!(e.contains("narrow"), "{e}");
    }

    #[test]
    fn a_dispatch_table_this_cannot_read_whole_stays_unverified() {
        for (why, global, extra) in [
            ("external linkage", "@FN = global ptr @detect, align 8", ""),
            (
                "a store of a value that is not a function",
                "@FN = internal global ptr @detect, align 8",
                "define void @set(ptr %h) {\n  %v = load ptr, ptr %h\n  store ptr %v, ptr @FN\n  ret void\n}\n",
            ),
            (
                "its address handed to an external",
                "@FN = internal global ptr @detect, align 8",
                "declare void @install(ptr)\ndefine void @set() {\n  call void @install(ptr @FN)\n  ret void\n}\n",
            ),
            (
                "its address in another initialiser",
                "@FN = internal global ptr @detect, align 8\n@WHERE = internal constant ptr @FN",
                "",
            ),
        ] {
            let text = dispatch(global, extra).replace("  call i32 @pthread_mutex_lock(ptr null)\n", "");
            let e = run_text(&text, None, &Sinks::default()).unwrap_err();
            assert!(e.contains("cannot verify: reaches an indirect call in example::render"), "{why}: {e}");
            assert!(e.contains("loaded from `FN`"), "{why}: {e}");
        }
    }
}
