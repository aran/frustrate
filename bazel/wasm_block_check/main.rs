//! Fail the build when a `#[bridge(no_block)]` body can reach
//! `memory.atomic.wait32/64`, a function under a `--slow` path, or an import
//! that does IO in an `--io` category ([`io`]); on wasip1 (`--wasi`), also the
//! import that waits there.
//!
//! `no_block` is the author's claim that main-thread Dart calling a member
//! cannot be stalled (docs/design/blocking.md). Codegen cannot see through a
//! call into a dependency, so the claim is settled at *artifact* time: a throwaway
//! module is built with `--cfg frustrate_block_check`, where the **only**
//! exports are one root per claimed member. lld's GC deletes everything those
//! roots cannot reach, and this tool asks whether a wait instruction survived.
//!
//! # How it decides
//!
//! A **site** is a function holding a wait instruction, matching a `--slow`
//! path ([`SlowPath`]), or an import that waits or does IO ([`io`]); the
//! procedure is the same for all of them. Per site `W`, in
//! order:
//!
//! 1. **Start-function exemption.** `W` is the module's start function (section
//!    8), nothing `call`s it, it is not exported and it is not table-resident.
//!    That is lld's `__wasm_init_memory`: it runs once at instantiation, before
//!    any export can be called, and its wait is the passive-data-init barrier.
//!    Needed only in the Bazel flow, where `--shared-memory` comes from the
//!    atomics toolchain (`bazel/custom_std.bzl`) and cannot be subtracted
//!    downstream. The cargo flow drops those link args, so it has no start
//!    section at all.
//! 2. **No direct-call path at all** to `W` — from a root, from the start
//!    function, or from a fatal entry — an error, not a pass. It means the
//!    direct-call graph cannot account for how `W` is alive (reached only
//!    through `call_indirect`, or kept by a relocation), so this tool cannot
//!    decide, and silence would be the failure mode it exists to remove.
//! 3. **Reachable from a root with the fatal entries cut out** — red. This is
//!    the finding: a live, ordinary path from a claimed body to a wait.
//! 4. Otherwise **exempt via the fatal-path rule**, subject to a side condition
//!    checked per site (see [`FATAL_ENTRIES`] and [`side_condition`]).
//!
//! Green additionally requires that the parse worked (a name section carrying
//! mangled Rust symbols), that every *function* export is a check root, and that
//! there is at least one root — a gate over zero roots gates nothing.
//!
//! **Silences**, as natively ([`ir`]): a red site is silenced by naming it (a
//! function by its path, an import by its field) in `--allow` for every claim
//! or in a census `allow` row for one member, and a function marked
//! `#[frustrate::no_block(allow)]` (one that calls [`MARKER_PATH`] first) is a
//! stop point no root's walk enters. What was applied is written for
//! [`silences`], which judges every silence across the check target's
//! analyses.
//!
//! # Not every claim wants an artifact
//!
//! `no_block` says a caller on the Dart main thread cannot be stalled. A body
//! the caller never executes a wait of satisfies that wherever it is placed,
//! and two kinds do. An **actor** member runs on the actor's own executor — a
//! dedicated thread natively, a dedicated Worker on web — so waiting there is
//! legal on every configuration. A **dispatched** member
//! (the pool, or the cooperative executor) is placed off the caller wherever
//! `memory.atomic.wait32` exists, and runs on the caller only in a module that
//! has no such instruction. Codegen emits no root for either.
//!
//! A dispatched member can still leave a slice of itself on the caller's
//! thread: its parameters' `BytesCodec::from_bytes`, which the glue runs before
//! handing anything to the runtime, and the `Drop` of any handle it returns,
//! which a main-thread export runs later. That slice gets a root of its own —
//! its **residue** — containing those edges and nothing else. Its green means
//! something narrower than a full root's, and the census keeps the two apart so
//! the report can say which.
//!
//! Which leaves three states a bare module cannot tell apart, and they must
//! report differently: a claim set with nothing to prove, a claim set whose
//! roots are *missing* (the cfg never reached rustc), and no claims at all. So
//! codegen also writes a **claim census** ([`Claims`]), and both drivers hand it
//! over. With one, this tool matches roots against rows in both directions —
//! strictly sharper than "at least one root" — and can be green over a claim set
//! that needs no module at all (`--claims-only`, which takes no `.wasm`). That
//! last mode is what makes a placement-settled claim set cost nothing at all:
//! no module, no +atomics std, no build.
//!
//! The cfg reaches frustrate's own `#[no_mangle]`s and no further, so a graph
//! containing wasm-bindgen would arrive here with its dependency rlibs' exports
//! still on. The check configuration links through `link_filter.rs`,
//! which drops those `--export` arguments before lld's GC. It can only remove
//! exports, so one it misses is still a stray this refuses by name.
//!
//! # What it does not claim
//!
//! Green is a statement about **this artifact**. It is sound for what the
//! artifact contains because lld's GC is conservative — an address-take is a
//! relocation, so anything reachable at all is kept — and one-sided in the safe
//! direction: a wait kept alive by nothing goes red rather than quiet.
//!
//! It is not a whole-program proof for a downstream consumer's own build. A
//! `--release` build with LTO can inline the wait intrinsic into its callers,
//! which is why the check artifact is built **debug**, where the intrinsic
//! stays one named function.
//!
//! **A call into an import ends the walk.** Only defined functions have bodies,
//! so an edge below `func_imports` has nothing for [`Module::scan_of`] to
//! return and [`reach`] skips it. Sound for what this looks for — JS cannot
//! execute a wasm instruction, and re-entry has to come back through an export,
//! which here is a root already scanned — but it is the boundary past which
//! "this body never waits" stops being what green means.
//!
//! The scan decodes the code section with `wasmparser` ([`parse`]) rather than
//! matching bytes: a `memory.atomic.wait` string in a data section is not a
//! wait instruction, and a raw `0xFE 0x01` byte scan fires on `i32.const 254`.
//!
//! # Dependencies
//!
//! `wasmparser` and `rustc-demangle`, from the `@frustrate_crates` hub every
//! consumer already resolves for the runtime's own crates. The scanner is built
//! once, in the exec configuration.

use std::collections::{BTreeMap, BTreeSet};
use std::process::ExitCode;

use wasmparser::{
    BinaryReader, ConstExpr, CustomSectionReader, ElementItems, ElementKind, ExternalKind,
    KnownCustom, Name, Operator, OperatorsReader, Parser, Payload, TypeRef,
};

mod budget;
mod io;
mod provenance;
mod ir;
mod libc;
mod silences;
mod symbol;

use symbol::{own_path, readable};

#[global_allocator]
static ALLOC: budget::Budgeted = budget::Budgeted;

/// Every function export of a check artifact must start with this.
///
/// Codegen emits one `#[cfg(frustrate_block_check)] #[no_mangle] pub extern "C"
/// fn frustrate_check_block_{fn_id}_{name}()` per claimed member, and gates
/// every other `#[no_mangle]` in the generated bindings and in the runtime on
/// `#[cfg(not(frustrate_block_check))]`. So an artifact with any other function
/// export is either the production module (scan the wrong file and everything
/// is reachable, so the result means nothing) or a `#[no_mangle]` that escaped
/// the gate (its own reachable set is folded into the answer). Both are named
/// errors rather than a quiet pass.
const ROOT_PREFIX: &str = "frustrate_check_block_";

/// Function exports wasm-ld emits itself, which the check build cannot suppress.
///
/// The atomics toolchain carries `-C link-arg=--export=__wasm_init_tls` in
/// `toolchain/custom_std/dist/atomics/manifest.json`, and a downstream target
/// cannot subtract a link arg — the same situation, from the same manifest
/// line, that `__wasm_init_memory` already gets a structural exemption for.
/// Only the Bazel flow sees these: the cargo driver drops the shared-memory
/// link args entirely and its artifact has neither.
///
/// Excluded from `roots` as well as from `strays`, deliberately. It is not a
/// claimed body, so nothing it reaches may count as reachable-from-a-root; a
/// wait it did reach would then be live with no path from a root, the start
/// function, or a fatal entry — which is rule 2, a hard error, not a pass.
const LINKER_EXPORTS: &[&str] = &["__wasm_init_tls"];

/// Proof that the name section is present *and* carries mangled Rust symbols —
/// the same control, and the same reasoning, as `wasm_std_check`'s.
///
/// Present under both mangling schemes: legacy writes
/// `_ZN4core9panicking9panic_fmt17h…E`, v0 writes
/// `_RNvNtCs…_4core9panicking9panic_fmt`. A `-Zbuild-std` artifact carries v0
/// std symbols, so a control that only knew legacy would fire on every real
/// check artifact.
const SENTINEL: &str = "9panicking";

/// An entry into machinery that only runs on a thread that is already dying.
///
/// **Why the exemption is required, not a contingency.** Every Rust function can
/// panic, and the panic path takes two locks before it can report anything: the
/// panic hook's `RwLock<Hook>` and `std::sys::backtrace`'s `Mutex<()>`: the
/// backward direct-call region of the wait site is mostly `panic_with_hook`,
/// `default_hook`, `sys::backtrace::lock` and the std lock internals beneath
/// them. Without this rule the gate is red on every artifact, including one
/// whose claimed bodies do nothing but arithmetic, which is a gate that says
/// nothing.
///
/// **Why the rule is path-based rather than site-based.** `futex_wait` itself
/// can never be exempted — real `Mutex`/`Once` uses share it, and exempting the
/// site would exempt them too. A wait site is exempt iff *every* backward
/// direct-call path from it to a check root passes through one of these entries.
/// For a body that never locks, the surviving paths are all fatal paths; for a
/// body that does lock, an ordinary path exists and it goes red. That is exactly
/// the wanted discrimination.
///
/// **What it costs.** The hazard `no_block` exists to kill is the silent stall.
/// A wait behind one of these entries runs only on a thread that is already
/// failing loudly, where the worst case is a degraded report —
/// `trap_explain.dart` still attributes the trap to the right bridged call.
///
/// `rust_begin_unwind` alone cuts
/// every panic path — it is the single `#[panic_handler]`, and `panic!`,
/// `assert!`, bounds checks, `unwrap` and `handle_alloc_error`'s panic variant
/// all funnel through `core::panicking::panic_fmt` into it. The alloc-error hook
/// needs its own entry because nothing calls it directly: `std::alloc::rust_oom`
/// reads `HOOK: AtomicPtr` and calls it *indirectly*, so cutting `rust_oom`
/// would leave the hook as a rootless island of the backward region — and,
/// being address-taken, it is table-resident and would fail
/// [`side_condition`]. Pinning the hook itself is what makes the region clean.
struct FatalEntry {
    /// What the author would recognise, for the message.
    what: &'static str,
    /// Why a wait behind it is not the hazard `no_block` is about.
    why: &'static str,
    /// Mangled-symbol segments, matched **conjunctively within one symbol** and
    /// each carrying its Rust-mangling length prefix.
    ///
    /// Conjunctive, and this is the load-bearing part. Every name here is
    /// something a user could also call a function, and a match makes a
    /// function a *cut* — so a rule that matched too much would silently exempt
    /// a wait behind a fixture named `rust_begin_unwind`, which is a green this
    /// tool has no right to give. Length prefixes plus the owning path are what
    /// separate std's symbol from a lookalike:
    /// `…7___rustc17rust_begin_unwind` matches, `…13block_fixture3api17rust_begin_unwind`
    /// does not. Legacy and v0 differ on the module's own length prefix
    /// (`8___rustc` vs `7___rustc`), so the segment spans the join instead.
    segments: &'static [&'static str],
    /// Unmangled spellings, matched by equality. `#[panic_handler]` produced a
    /// bare `rust_begin_unwind` symbol on older toolchains.
    exact: &'static [&'static str],
    /// A path whose items ([`own_path`]) are all entries: every method of a
    /// type, its trait impls' included.
    path: &'static [&'static str],
}

impl FatalEntry {
    fn matches(&self, sym: &str) -> bool {
        self.exact.contains(&sym)
            || (!self.segments.is_empty() && self.segments.iter().all(|s| sym.contains(*s)))
            || (!self.path.is_empty() && {
                let p = own_path(sym);
                p.len() >= self.path.len() && self.path.iter().zip(&p).all(|(a, b)| a == b)
            })
    }
}

const FATAL_ENTRIES: &[FatalEntry] = &[
    FatalEntry {
        what: "std's #[panic_handler] (rust_begin_unwind)",
        why: "the thread is already unwinding or aborting; the wait is the panic \
              hook's RwLock and the backtrace print lock",
        segments: &["rustc17rust_begin_unwind"],
        exact: &["rust_begin_unwind"],
        path: &[],
    },
    FatalEntry {
        what: "std::alloc::default_alloc_error_hook",
        why: "the allocator has already failed and handle_alloc_error diverges; \
              the wait is the backtrace print lock inside the report",
        segments: &["3std", "5alloc", "24default_alloc_error_hook"],
        exact: &[],
        path: &[],
    },
    FatalEntry {
        what: "std's foreign-exception abort (__rust_foreign_exception)",
        why: "an exception from another language reached `catch_unwind`, and std \
              aborts the process; the write is its last message",
        segments: &["rustc24___rust_foreign_exception"],
        exact: &[],
        path: &[],
    },
    FatalEntry {
        what: "std's unlocked stderr (`<std::sys::stdio::unix::Stderr as Write>`)",
        why: "std writes to it without `io::stderr`'s lock only for a fatal \
              runtime message before it aborts the process, and from the panic \
              hook; every other writer takes that lock first, which is counted",
        segments: &[],
        exact: &[],
        path: &["std", "sys", "stdio", "unix", "Stderr"],
    },
    FatalEntry {
        what: "std's resume_unwind (std::panicking::resume_unwind)",
        why: "it starts unwinding again with a panic's payload, which a caller \
              holds only once a panic was caught; it raises without the hook, \
              and the panic path is outside the check",
        segments: &[],
        exact: &[],
        path: &["std", "panicking", "resume_unwind"],
    },
    FatalEntry {
        what: "std::alloc::rust_oom",
        why: "the allocator has already failed; it calls the allocation-error \
              hook, then aborts",
        segments: &[],
        exact: &[],
        path: &["std", "alloc", "rust_oom"],
    },
    FatalEntry {
        what: "frustrate's panic report (frustrate::panic::observed)",
        why: "it runs only with a caught panic in hand, and the panic path is \
              outside the check",
        segments: &[],
        exact: &[],
        path: &["frustrate", "panic", "observed"],
    },
];

/// The wait instructions. `memory.atomic.notify` is deliberately absent: it
/// wakes, it never blocks, and it is legal on the main thread.
const WAIT32: &str = "memory.atomic.wait32";
const WAIT64: &str = "memory.atomic.wait64";

/// The path of the function `#[frustrate::no_block(allow)]` makes a function
/// call first, in the check build (`runtime/rust/src/lib.rs`). A wasm module
/// knows it by its name-section path; natively it is exported as
/// [`ir::MARKER`].
const MARKER_PATH: &[&str] = &["frustrate", "__no_block_allow"];

/// A green verdict: what was checked, and the silences that kept findings out
/// of it.
#[derive(Debug)]
struct Green {
    text: String,
    applied: silences::Applied,
}

/// A function the project names as slow work (`--slow <path>`): a claimed
/// body may no more reach it than a wait.
///
/// Written as a Rust path and matched, whole segments only, against the
/// leading segments of each function's demangled path ([`own_path`]). So
/// `std::fs` matches `std::fs::read` and `std::fs::File::open` but not
/// `std::fsx`, and `app::main_thread::slow` matches that function and its
/// closures. A method's path is its type's, trait impls included: a method of
/// `impl Read for File` is `std::fs::File::read` and matches `std::fs`; a
/// function generic over `std::fs::File` written elsewhere does not.
///
/// A marker function must survive as a function of its own, so
/// `#[inline(never)]`: even `#[inline]` vanishes from an opt-level 0 artifact.
/// For the same reason std's IO cannot be named on `wasm32-unknown-unknown`,
/// where `std::fs` and `std::net` are stubs that inline away.
///
/// A path that names nothing is refused by the native analysis
/// ([`ir`]), whose build keeps the whole program. A wasm scan cannot
/// refuse it: its module keeps only what the roots reach, so a clean one is
/// one in which the named functions are absent.
struct SlowPath(Vec<String>);

impl SlowPath {
    fn parse(text: &str) -> Result<SlowPath, String> {
        let segs: Vec<String> = text.split("::").map(str::to_string).collect();
        let ident = |s: &String| {
            !s.is_empty()
                && !s.starts_with(|c: char| c.is_ascii_digit())
                && s.chars().all(|c| c.is_alphanumeric() || c == '_')
        };
        if !segs.iter().all(ident) {
            return Err(format!(
                "wasm_block_check: `--slow {text}` is not a Rust path. Write the \
                 path to a module or function, segments joined by `::`, with no \
                 generics or leading `::` — `std::fs`, `my_crate::io::save`."
            ));
        }
        if matches!(segs[0].as_str(), "crate" | "self" | "super") {
            return Err(format!(
                "wasm_block_check: `--slow {text}` starts with `{}`, which no \
                 symbol carries. Start it with the crate's name as rustc sees it \
                 (hyphens become underscores).",
                segs[0]
            ));
        }
        Ok(SlowPath(segs))
    }

    fn matches(&self, path: &[String]) -> bool {
        path.len() >= self.0.len() && self.0.iter().zip(path).all(|(a, b)| a == b)
    }

    fn text(&self) -> String {
        self.0.join("::")
    }
}

/// What a claimed body may not reach besides a wait instruction.
#[derive(Default)]
struct Sinks {
    paths: Vec<SlowPath>,
    /// IO categories ([`io`]), which each scan maps to its own target's calls.
    io: Vec<&'static io::Category>,
    /// The module targets wasip1, whose waits are imports
    /// ([`io::WASI_WAITS`]) rather than instructions.
    wasi: bool,
    /// The check target's silences (`allow`): what each names — an external
    /// function, or the function holding an indirect call — and why. Each
    /// applies to every claim, and one that silences nothing is an error.
    allowed: Vec<String>,
}

impl Sinks {
    /// Whether a project named anything; the target's own waits are not
    /// counted.
    fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.io.is_empty()
    }

    fn count(&self) -> usize {
        self.paths.len() + self.io.len()
    }

    fn text(&self) -> String {
        self.paths
            .iter()
            .map(|p| format!("`{}`", p.text()))
            .chain(self.io.iter().map(|c| format!("IO `{}`", c.name)))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// --------------------------------------------------------------- decoding --

/// Everything one instruction sequence tells this tool.
#[derive(Default)]
struct Scan {
    /// `call` / `return_call` targets, in the module's function index space.
    calls: Vec<u32>,
    /// `ref.func` targets. An address-take is as good as a table entry for
    /// [`side_condition`]: with reference types, a `ref.func`'d function can be
    /// installed into a table at run time by `table.set`.
    refs: Vec<u32>,
    /// The wait instructions found, by mnemonic.
    waits: Vec<&'static str>,
}

/// Decode an instruction sequence to its `end`, and check it ends there with
/// its blocks balanced: an instruction read wrongly is an error, never a
/// shorter or longer scan.
fn scan_ops(mut r: OperatorsReader<'_>, out: &mut Scan) -> wasmparser::Result<()> {
    while !r.eof() {
        match r.read()? {
            // `return_call` is a real call edge too; rustc only emits it under
            // `+tail-call`, but a missed edge would be a silent
            // under-approximation of reachability.
            Operator::Call { function_index } | Operator::ReturnCall { function_index } => {
                out.calls.push(function_index)
            }
            Operator::RefFunc { function_index } => out.refs.push(function_index),
            Operator::MemoryAtomicWait32 { .. } => out.waits.push(WAIT32),
            Operator::MemoryAtomicWait64 { .. } => out.waits.push(WAIT64),
            _ => {}
        }
    }
    r.finish()
}

/// The `ref.func`s in a constant expression (an element or global initialiser).
fn const_refs(e: &ConstExpr<'_>, table: &mut BTreeSet<u32>) -> wasmparser::Result<()> {
    let mut s = Scan::default();
    scan_ops(e.get_operators_reader(), &mut s)?;
    table.extend(s.refs);
    Ok(())
}

// ------------------------------------------------------------ the module --

struct Module {
    /// Imported functions occupy `0..func_imports` of the index space; the code
    /// section's `n`th entry is function `func_imports + n`. Getting this wrong
    /// shifts every call edge, which is why `an_import_shifts_the_index_space`
    /// exists.
    func_imports: u32,
    /// `(module, field)` of each function import, by function index.
    imports: Vec<(String, String)>,
    /// The `target_features` custom section, when there is one: `Some(true)`
    /// if the module was compiled `+atomics`. Without atomics rustc emits no
    /// wait instruction, so a clean scan says nothing about waits.
    atomics: Option<bool>,
    names: BTreeMap<u32, String>,
    /// `(name, kind, index)`, kind as the binary format codes it: 0 is a
    /// function.
    exports: Vec<(String, u8, u32)>,
    start: Option<u32>,
    /// Functions an element segment, a global initialiser or a `ref.func`
    /// makes indirectly callable.
    table: BTreeSet<u32>,
    /// One per defined function, in code-section order.
    bodies: Vec<Scan>,
}

impl Module {
    fn scan_of(&self, f: u32) -> Option<&Scan> {
        f.checked_sub(self.func_imports)
            .and_then(|n| self.bodies.get(n as usize))
    }

    fn label(&self, f: u32) -> String {
        match self.names.get(&f) {
            Some(n) => readable(n),
            None => format!("function #{f} (no name-section entry)"),
        }
    }
}

/// Decode a core wasm module. Any decoding error fails the whole parse: a
/// module this cannot read completely is one it cannot speak for.
fn parse(d: &[u8]) -> Result<Module, String> {
    if !Parser::is_core_wasm(d) {
        return Err("not a wasm module (a component is not one either)".into());
    }
    parse_module(d).map_err(|e| format!("cannot decode this wasm module: {e}"))
}

fn parse_module(d: &[u8]) -> wasmparser::Result<Module> {
    let mut m = Module {
        func_imports: 0,
        imports: Vec::new(),
        atomics: None,
        names: BTreeMap::new(),
        exports: Vec::new(),
        start: None,
        table: BTreeSet::new(),
        bodies: Vec::new(),
    };
    for payload in Parser::new(0).parse_all(d) {
        match payload? {
            Payload::ImportSection(r) => {
                for i in r.into_imports() {
                    let i = i?;
                    if let TypeRef::Func(_) | TypeRef::FuncExact(_) = i.ty {
                        m.imports.push((i.module.to_string(), i.name.to_string()));
                    }
                }
                m.func_imports = m.imports.len() as u32;
            }
            Payload::GlobalSection(r) => {
                for g in r {
                    const_refs(&g?.init_expr, &mut m.table)?;
                }
            }
            Payload::ExportSection(r) => {
                for e in r {
                    let e = e?;
                    let kind = match e.kind {
                        ExternalKind::Func | ExternalKind::FuncExact => 0,
                        ExternalKind::Table => 1,
                        ExternalKind::Memory => 2,
                        ExternalKind::Global => 3,
                        ExternalKind::Tag => 4,
                    };
                    m.exports.push((e.name.to_string(), kind, e.index));
                }
            }
            Payload::StartSection { func, .. } => m.start = Some(func),
            Payload::ElementSection(r) => {
                for e in r {
                    let e = e?;
                    if let ElementKind::Active { offset_expr, .. } = &e.kind {
                        const_refs(offset_expr, &mut m.table)?;
                    }
                    match e.items {
                        ElementItems::Functions(fs) => {
                            for f in fs {
                                m.table.insert(f?);
                            }
                        }
                        ElementItems::Expressions(_, es) => {
                            for x in es {
                                const_refs(&x?, &mut m.table)?;
                            }
                        }
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                let mut s = Scan::default();
                scan_ops(body.get_operators_reader()?, &mut s)?;
                // An address-take in a body is as good as an element-segment
                // entry: `ref.func` puts a callable reference in a value, and
                // `table.set` can install it at run time.
                m.table.extend(s.refs.iter().copied());
                m.bodies.push(s);
            }
            Payload::CustomSection(c) => match c.as_known() {
                KnownCustom::Name(names) => {
                    for sub in names {
                        if let Name::Function(map) = sub? {
                            for n in map {
                                let n = n?;
                                m.names.insert(n.index, n.name.to_string());
                            }
                        }
                    }
                }
                _ if c.name() == "target_features" => {
                    m.atomics = Some(target_features_atomics(&c)?);
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(m)
}

/// The `target_features` custom section: a vector of `(prefix, name)`, where
/// prefix `+` means the feature was used. True if `atomics` was.
fn target_features_atomics(c: &CustomSectionReader<'_>) -> wasmparser::Result<bool> {
    let mut r = BinaryReader::new(c.data(), c.data_offset());
    let mut atomics = false;
    for _ in 0..r.read_var_u32()? {
        let prefix = r.read_u8()?;
        if r.read_string()? == "atomics" && prefix == b'+' {
            atomics = true;
        }
    }
    Ok(atomics)
}

// -------------------------------------------------------------- the graph --

/// Function indices whose name matches any [`FatalEntry`].
fn fatal_entries(m: &Module) -> BTreeSet<u32> {
    m.names
        .iter()
        .filter(|(_, n)| FATAL_ENTRIES.iter().any(|f| f.matches(n)))
        .map(|(i, _)| *i)
        .collect()
}

/// Forward reachability over direct call edges, never entering `cut`.
fn reach(m: &Module, roots: &[u32], cut: &BTreeSet<u32>) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    let mut stack = Vec::new();
    for r in roots {
        if !cut.contains(r) && seen.insert(*r) {
            stack.push(*r);
        }
    }
    while let Some(f) = stack.pop() {
        let Some(s) = m.scan_of(f) else { continue };
        for c in &s.calls {
            if !cut.contains(c) && seen.insert(*c) {
                stack.push(*c);
            }
        }
    }
    seen
}

/// Backward reachability: every function that can reach `target` over direct
/// call edges without passing through `cut`. `target` itself is included.
fn back_reach(rev: &BTreeMap<u32, Vec<u32>>, target: u32, cut: &BTreeSet<u32>) -> BTreeSet<u32> {
    let mut seen = BTreeSet::new();
    seen.insert(target);
    let mut stack = vec![target];
    while let Some(f) = stack.pop() {
        for p in rev.get(&f).map(|v| v.as_slice()).unwrap_or(&[]) {
            if !cut.contains(p) && seen.insert(*p) {
                stack.push(*p);
            }
        }
    }
    seen
}

fn reverse_edges(m: &Module) -> BTreeMap<u32, Vec<u32>> {
    let mut rev: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    for (n, s) in m.bodies.iter().enumerate() {
        let caller = m.func_imports + n as u32;
        for c in &s.calls {
            rev.entry(*c).or_default().push(caller);
        }
    }
    rev
}

/// The exemption's soundness side condition.
///
/// Cutting at the fatal entries proves "every path goes through one" only if
/// the direct call edges are *complete* for the region the cut isolates. They
/// are not complete for a function something can reach indirectly: a
/// `call_indirect` can enter it from anywhere, and this tool deliberately does
/// not model indirect edges (a typed closure over the table reaches nearly
/// every function, because Rust monomorphisation erases the type information
/// wasm would need to discriminate).
///
/// So: a function absent from every element segment, every global initialiser
/// and every `ref.func` cannot be reached indirectly, and for a region built
/// only of such functions the direct edges *are* complete. This returns the
/// members of the backward region that break that, and the caller refuses to
/// certify rather than assume.
///
/// `default_alloc_error_hook` is the region's one table-resident member, which
/// is why it is a [`FatalEntry`] in its own right rather than an interior node.
fn side_condition(
    m: &Module,
    rev: &BTreeMap<u32, Vec<u32>>,
    wait_fn: u32,
    cut: &BTreeSet<u32>,
) -> Vec<u32> {
    back_reach(rev, wait_fn, cut)
        .into_iter()
        .filter(|f| m.table.contains(f))
        .collect()
}

// -------------------------------------------------------------- the verdict --

/// A site — a function holding a wait instruction or named slow — and what it
/// is.
struct Finding {
    func: u32,
    kinds: Vec<String>,
    /// Holds a wait instruction, as opposed to only being named slow. Only a
    /// wait can be the start function's own barrier.
    waits: bool,
}

// ------------------------------------------------------------- the census --

/// The first line of a claim census, and the only version this tool reads.
///
/// Bumped whenever the settlement vocabulary changes, because an older scanner
/// reading a newer census would have to guess at a token it does not know — and
/// the one guess that is cheap to make ("treat it as needing no artifact") is
/// the one that turns a misconfigured build into a silent pass.
const CENSUS_HEADER: &str = "frustrate-claim-census 6";

/// The `#[bridge(no_block)]` claims of one bridge crate, and how each is
/// settled — written by codegen (`emit_rust::claim_census`), read here.
///
/// Line-oriented, and a **separate** file from `interface.frustrate.json`
/// because that JSON is the wire-schema fingerprint: `no_block` is
/// `#[serde(skip)]` there precisely so a claim never moves the hash, and two
/// halves that disagree about a claim still interoperate.
#[derive(Default, Debug)]
struct Claims {
    crate_name: String,
    /// Settled by placement on an **actor**: the body runs on the actor's own
    /// executor on every platform, so no configuration exists in which the
    /// caller's thread runs it. No root.
    placement: Vec<String>,
    /// Settled by placement on a **dispatched** member — a plain async member
    /// or an `async fn` handed to the pool or the cooperative executor — with
    /// nothing of it left on the caller's thread. No root.
    ///
    /// A weaker fact than [`Claims::placement`], and separate for that reason.
    /// The argument has two legs rather than one: where the wait instruction
    /// exists (threaded web) the body is off the caller, and where the body is
    /// inline on the caller (single-threaded web) the module has no wait
    /// instruction in it. Whichever leg a configuration takes, the caller
    /// executes no wait belonging to this member — but unlike an actor's, the
    /// reason changes with the configuration.
    placement_dispatch: Vec<String>,
    /// Wants a root reaching the member's whole body:
    /// `(expected export name, display name)`.
    artifact: Vec<(String, String)>,
    /// The path of each full root's member function, by root symbol, split
    /// into segments: the root must reach a function with that path.
    member_paths: BTreeMap<String, Vec<String>>,
    /// The silences each claim carries in source (`no_block(allow(...))` on the member,
    /// its impl block or its file), by fn id: what each names, and why.
    allow: BTreeMap<String, Vec<String>>,
    /// Each claim's fn id, by root symbol, and by display name for the
    /// claims with no root.
    ids: BTreeMap<String, String>,
    /// Wants a root reaching only the member's caller-side **residue** — the
    /// `BytesCodec::from_bytes` of its bridge-external parameters and the drop
    /// glue of the handles it hands out. Same shape as [`Claims::artifact`] and
    /// matched against the module's exports the same way; a separate list only
    /// so the report can say which green it is. "Everything this member reaches
    /// is clean" and "what it leaves on the caller is clean, and the rest is
    /// placed" are different statements.
    residue: Vec<(String, String)>,
    /// `requires_native`: no wasm glue exists, so no artifact can hold it.
    ///
    /// On web that is the claim being satisfied, not evaded — the member is
    /// absent from the web Dart surface and its glue is absent from the
    /// module, so no browser main thread can enter it, and web is where a wait
    /// instruction exists at all. The unproven half is native, and FR0048 /
    /// FR0049 are the whole of it. Counted, never green-blocking, and never
    /// green-*making* either: a target whose census is nothing but these has
    /// nothing to do (see [`Claims::refuse_empty`]).
    declared: Vec<String>,
}

impl Claims {
    /// Parse a census, or say exactly what is wrong with it.
    ///
    /// Every rejection is named. A census this tool cannot read must never
    /// degrade to "no claims": that is the one reading that would turn a
    /// misconfigured build into a silent pass.
    fn parse(text: &str, path: &str) -> Result<Claims, String> {
        let mut lines = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'));
        let head = lines.next().unwrap_or("");
        if head != CENSUS_HEADER {
            return Err(format!(
                "wasm_block_check: {path} is not a claim census this tool reads.\n\
                 \n\
                 Expected the first non-comment line to be `{CENSUS_HEADER}`; found \
                 `{head}`.\n\
                 The census is generated — `frustrate_bridge`'s `claims` output \
                 group under Bazel, `GenerateConfig::claims_out` under cargo — so \
                 either this is a different file, or the codegen that wrote it is \
                 older than this scanner."
            ));
        }
        let mut c = Claims::default();
        for line in lines {
            if let Some(name) = line.strip_prefix("crate ") {
                c.crate_name = name.to_string();
                continue;
            }
            if let Some(rest) = line.strip_prefix("allow ") {
                let mut f = rest.split(' ');
                let (Some(id), Some(what), None) = (f.next(), f.next(), f.next()) else {
                    return Err(format!(
                        "wasm_block_check: {path}: cannot read `{line}`.\n\
                         An allow row is `allow <fn_id> <name>`."
                    ));
                };
                c.allow.entry(id.to_string()).or_default().push(what.to_string());
                continue;
            }
            let mut f = line.split(' ');
            let (Some(kind), Some(id), Some(name), member, None) =
                (f.next(), f.next(), f.next(), f.next(), f.next())
            else {
                return Err(format!(
                    "wasm_block_check: {path}: cannot read `{line}`.\n\
                     A census row is `<settlement> <fn_id> <name>`, and an \
                     `artifact` row adds the member's own function path."
                ));
            };
            if id.parse::<u32>().is_err() {
                return Err(format!(
                    "wasm_block_check: {path}: `{line}` has no numeric fn id."
                ));
            }
            if member.is_some() != (kind == "artifact") {
                return Err(format!(
                    "wasm_block_check: {path}: `{line}`: an `artifact` row, and \
                     only an `artifact` row, carries the member's function path."
                ));
            }
            // The check root's symbol embeds the *bare* member name; the census
            // carries the qualified one so a message can name `Node::send`.
            let bare = name.rsplit("::").next().unwrap_or(name);
            let key = if kind == "artifact" || kind == "artifact-residue" {
                format!("{ROOT_PREFIX}{id}_{bare}")
            } else {
                name.to_string()
            };
            c.ids.insert(key, id.to_string());
            match kind {
                "placement" => c.placement.push(name.to_string()),
                "placement-dispatch" => c.placement_dispatch.push(name.to_string()),
                "declared" => c.declared.push(name.to_string()),
                "artifact" => {
                    let sym = format!("{ROOT_PREFIX}{id}_{bare}");
                    let segs = member.unwrap_or_default().split("::").map(str::to_string).collect();
                    c.member_paths.insert(sym.clone(), segs);
                    c.artifact.push((sym, name.to_string()));
                }
                "artifact-residue" => c
                    .residue
                    .push((format!("{ROOT_PREFIX}{id}_{bare}"), name.to_string())),
                _ => {
                    return Err(format!(
                        "wasm_block_check: {path}: `{kind}` is not a settlement this \
                         tool knows (`placement`, `placement-dispatch`, `artifact`, \
                         `artifact-residue`, `declared`).\n\
                         The census was written by a newer codegen than this scanner."
                    ))
                }
            }
        }
        Ok(c)
    }

    /// Every claim that wants a check root, whatever shape that root has. The
    /// module-side controls — "the census lists a root the artifact lacks" and
    /// "the artifact exports a root the census does not claim" — are about the
    /// export set, which the two kinds share.
    fn roots(&self) -> impl Iterator<Item = &(String, String)> {
        self.artifact.iter().chain(self.residue.iter())
    }

    fn root_count(&self) -> usize {
        self.artifact.len() + self.residue.len()
    }

    /// How a claim is named in a message, by its fn id.
    fn display_of<'a>(&'a self, id: &'a str) -> &'a str {
        let Some((key, _)) = self.ids.iter().find(|(_, i)| *i == id) else {
            return id;
        };
        self.roots()
            .find(|(s, _)| s == key)
            .map(|(_, d)| d.as_str())
            .unwrap_or(key)
    }

    /// How the crate is named in a message.
    fn who(&self) -> String {
        if self.crate_name.is_empty() {
            "this bridge".into()
        } else {
            format!("`{}`", self.crate_name)
        }
    }

    /// The trailing accounting on a green line: what was settled, and how.
    fn accounting(&self) -> String {
        let mut parts = Vec::new();
        if !self.placement.is_empty() {
            parts.push(format!(
                "{} settled by placement (an actor member's body runs on the \
                 actor's own executor, never the caller's thread)",
                self.placement.len()
            ));
        }
        if !self.placement_dispatch.is_empty() {
            parts.push(format!(
                "{} settled by dispatch placement (the body is handed to the \
                 pool or the executor, so where a wait instruction exists it is \
                 off the caller's thread, and where it runs on the caller's \
                 thread the module has no wait instruction)",
                self.placement_dispatch.len()
            ));
        }
        if !self.residue.is_empty() {
            parts.push(format!(
                "{} dispatched with a caller-side residue, scanned here (a \
                 parameter's `BytesCodec::from_bytes`, or the `Drop` of a handle \
                 it hands out); their bodies are settled by dispatch placement",
                self.residue.len()
            ));
        }
        if !self.declared.is_empty() {
            parts.push(format!(
                "{} native-only, so absent from the web surface — nothing for a \
                 browser main thread to run, and FR0048/FR0049 are the native \
                 half",
                self.declared.len()
            ));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(", {}", parts.join(", "))
        }
    }

    /// The states that are decided by the census alone, before any module is
    /// read. Kept ahead of the scan on purpose: "there is nothing to prove"
    /// must never arrive as an empty set matching an empty set.
    fn refuse_empty(&self, label: &str) -> Result<(), String> {
        if !self.placement.is_empty()
            || !self.placement_dispatch.is_empty()
            || self.root_count() > 0
        {
            return Ok(());
        }
        let tail = if self.declared.is_empty() {
            "Claim a member `#[bridge(no_block)]`, or drop this target."
        } else {
            "Every claim here is native-only. On web that settles them by \
             absence — no glue, so no main-thread body — and on native FR0048 \
             and FR0049 have already said everything that can be said, so \
             neither an artifact nor a placement adds anything and this target \
             has no work to do. Claim a portable member, or drop this target."
        };
        Err(format!(
            "wasm_block_check: {} claims nothing this check can settle, so \
             {label} covers nothing and its silence means nothing.\n\
             \n\
             {tail}",
            self.who()
        ))
    }
}

/// Green over a claim set that needs no artifact at all.
///
/// Reached only through `--claims-only`, which is a *declaration* by the target
/// that built no module. So the census is what has to hold it honest: a claim
/// that wants a root is refused here by name, which is how a project that adds
/// one to an ir-only bridge finds out rather than keeping a green that no longer
/// covers it.
fn claims_only(label: &str, c: &Claims) -> Result<String, String> {
    c.refuse_empty(label)?;
    let silences: Vec<String> = c
        .allow
        .iter()
        .flat_map(|(id, names)| names.iter().map(move |w| format!("  `{w}` on {}", c.display_of(id))))
        .collect();
    if !silences.is_empty() {
        return Err(format!(
            "wasm_block_check: {label}: {} silence(s) silence nothing:\n\n{}\n\n\
             Every claim here is settled by placement, so nothing it reaches is \
             scanned and a silence on it has nothing to silence. Remove it.",
            silences.len(),
            silences.join("\n")
        ));
    }
    if c.root_count() > 0 {
        let names: Vec<String> = c
            .roots()
            .map(|(_, display)| format!("  {display}"))
            .collect();
        return Err(format!(
            "wasm_block_check: {} has {} claim(s) that need artifact evidence, \
             and this target builds none:\n\n{}\n\n\
             A claims-only target settles claims by *placement* — on an actor's \
             own executor, or by dispatch onto the pool with nothing left on the \
             caller. The claims above leave something behind: a member's own body \
             (`#[bridge(sync)]`), a bridge-external parameter whose `from_bytes` \
             runs on the caller, or a handle whose `Drop` does. Give the target \
             its bridge crate (`crate = ...`) so they can be proven, or drop the \
             claims.",
            c.who(),
            c.root_count(),
            names.join("\n")
        ));
    }
    Ok(format!(
        "wasm_block_check: {label} is clean — {} `no_block` claim(s), none \
         needing an artifact{}.",
        c.placement.len() + c.placement_dispatch.len() + c.declared.len(),
        c.accounting()
    ))
}

/// The gate with no census and no slow paths: every control it can apply from
/// the module alone.
#[cfg(test)]
fn run(path: &str, label: &str) -> Result<String, String> {
    run_with(path, label, None, &Sinks::default()).map(|g| g.text)
}

fn run_with(
    path: &str,
    label: &str,
    claims: Option<&Claims>,
    sinks: &Sinks,
) -> Result<Green, String> {
    budget::set(path, label);
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let m = parse(&bytes)?;

    // Control 1: the parse worked and the names are there to read.
    if !m.names.values().any(|n| n.contains(SENTINEL)) {
        return Err(format!(
            "wasm_block_check: cannot verify {label}.\n\
             \n\
             This module's wasm name section is missing, empty, or carries no \
             mangled Rust symbols (built with `-C strip=symbols`, or run through \
             a post-pass that rewrote the names?). The panic-path exemption and \
             every message here are keyed on those symbols, so a scan without \
             them would report success for a module it never examined.\n\
             \n\
             Keep the name section on the check artifact. It is built to be \
             scanned and thrown away, so nothing about it needs to be small."
        ));
    }

    // Control 2: the right artifact.
    let mut roots = Vec::new();
    let mut root_names = Vec::new();
    let mut strays = Vec::new();
    for (n, kind, idx) in &m.exports {
        if *kind != 0 {
            continue; // memory, __data_end and __heap_base are not code
        }
        if LINKER_EXPORTS.contains(&n.as_str()) {
            continue; // wasm-ld's own, forced by the atomics toolchain
        }
        if n.starts_with(ROOT_PREFIX) {
            roots.push(*idx);
            root_names.push(n.clone());
        } else {
            strays.push(n.clone());
        }
    }
    if !strays.is_empty() {
        return Err(format!(
            "wasm_block_check: {label} exports {} function(s) that are not \
             `{ROOT_PREFIX}*`:\n\n{}\n\n\
             A check artifact's only function exports are the roots codegen \
             emits under `--cfg frustrate_block_check`; everything else is \
             `#[cfg(not(frustrate_block_check))]`. So this is either the \
             production module — in which case every finding below would be \
             about code no claimed body reaches — or a `#[no_mangle]` that \
             escaped the gate, whose own reachable set would be folded into the \
             answer. Both make the result meaningless, so neither is a pass.",
            strays.len(),
            strays
                .iter()
                .map(|s| format!("  {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    // Control 3: the roots are the ones claimed.
    match_census(&root_names, claims, label)?;

    // Control 4: there is something to find. A module built without atomics
    // holds no wait instruction, and unless it targets wasip1 (whose waits are
    // imports) it has no other way to wait, so with no slow names its green
    // would be vacuous.
    if m.atomics == Some(false) && !sinks.wasi && sinks.is_empty() {
        return Err(format!(
            "wasm_block_check: {label} was built without `+atomics` and not for \
             wasip1, so it can hold no wait, and no slow names were given: this \
             check covers nothing and its silence means nothing.\n\
             \n\
             Build the artifact on a `+atomics` platform to check waits, or name \
             what a claimed body may not reach (`--slow`, `--io`)."
        ));
    }

    let rev = reverse_edges(&m);
    let cut = fatal_entries(&m);
    // The engine calls the start function at instantiation, so it is a root for
    // everything it reaches. Its *own* body is handled by the structural
    // exemption below, which is why that test comes first.
    let mut traversal_roots = roots.clone();
    if let Some(s) = m.start {
        traversal_roots.push(s);
    }
    // `live` answers "can the direct-call graph account for this function being
    // here at all?", so it starts from everything known to run: the roots, the
    // start function, and the fatal entries — which are alive by construction
    // (the engine reaches the panic handler through `__rust_start_panic`, and
    // `rust_oom` reaches the alloc hook through an `AtomicPtr`, neither of them
    // an edge this graph carries). `live_pruned` is the one that decides red,
    // and it starts only from the roots with the entries cut out.
    let mut live_roots = traversal_roots.clone();
    live_roots.extend(cut.iter().copied());
    let live = reach(&m, &live_roots, &BTreeSet::new());
    let live_pruned = reach(&m, &traversal_roots, &cut);

    // Control 5: every full root still reaches its member. A root calls its
    // member's glue with an empty request; an optimizer that sees the decode
    // fail deletes the body after it, and the scan would pass every member
    // that takes an argument.
    if let Some(c) = claims {
        let folded: Vec<String> = c
            .artifact
            .iter()
            .filter_map(|(sym, display)| {
                let at = root_names.iter().position(|n| n == sym)?;
                let glue = format!("call_{}", &sym[ROOT_PREFIX.len()..]);
                let reaches = reach(&m, &[roots[at]], &BTreeSet::new()).iter().any(|f| {
                    m.names
                        .get(f)
                        .is_some_and(|n| own_path(n).last().is_some_and(|l| *l == glue))
                });
                (!reaches).then(|| format!("  {display}"))
            })
            .take(MEMBER_CAP)
            .collect();
        if !folded.is_empty() {
            return Err(format!(
                "wasm_block_check: cannot verify {label}: these roots no longer \
                 reach their member's glue:\n\n{}\n\n\
                 The check artifact was optimized: a flag outside the check's \
                 configuration (a crate's own `rustc_flags`, say) raised its \
                 opt-level, and the optimizer deleted the member bodies the \
                 scan exists to read. Build the check at opt-level 0.",
                folded.join("\n")
            ));
        }
    }

    let waits: Vec<Finding> = m
        .bodies
        .iter()
        .enumerate()
        .filter_map(|(n, s)| {
            let func = m.func_imports + n as u32;
            let mut kinds: Vec<String> = s.waits.iter().map(|k| k.to_string()).collect();
            kinds.sort_unstable();
            kinds.dedup();
            let waits = !kinds.is_empty();
            if !sinks.paths.is_empty() {
                if let Some(name) = m.names.get(&func) {
                    let path = own_path(name);
                    kinds.extend(
                        sinks.paths.iter()
                            .filter(|p| p.matches(&path))
                            .map(|p| format!("named slow by `{}`", p.text())),
                    );
                }
            }
            (!kinds.is_empty()).then_some(Finding { func, kinds, waits })
        })
        .collect();
    // Imported functions have no body to hold a wait, only a name to match.
    let mut waits = waits;
    for (f, (module, field)) in m.imports.iter().enumerate() {
        let mut kinds: Vec<String> = Vec::new();
        if sinks.wasi && io::wasi_wait(module, field) {
            kinds.push(format!("imports `{module}.{field}`, which waits"));
        }
        kinds.extend(
            sinks
                .io
                .iter()
                .filter(|c| c.wasi_call(module, field))
                .map(|c| format!("imports `{module}.{field}`, IO `{}`", c.name)),
        );
        if !kinds.is_empty() {
            // Not `waits`: that marks a wait instruction, which only the start
            // function's own barrier may hold.
            waits.push(Finding { func: f as u32, kinds, waits: false });
        }
    }

    let called: BTreeSet<u32> = m.bodies.iter().flat_map(|s| s.calls.iter().copied()).collect();
    let exported: BTreeSet<u32> = m
        .exports
        .iter()
        .filter(|(_, k, _)| *k == 0)
        .map(|(_, _, i)| *i)
        .collect();

    let mut red: Vec<&Finding> = Vec::new();
    let mut exempt_fatal = 0usize;
    let mut exempt_start = 0usize;

    for w in &waits {
        // 1. The start function's own barrier.
        if w.waits
            && m.start == Some(w.func)
            && !called.contains(&w.func)
            && !exported.contains(&w.func)
            && !m.table.contains(&w.func)
        {
            exempt_start += 1;
            continue;
        }
        // A pinned entry that itself waits is the exemption, not a finding.
        if cut.contains(&w.func) {
            exempt_fatal += 1;
            continue;
        }
        // 2. Present but unexplained.
        if !live.contains(&w.func) {
            // Where the indirect entry probably is: the table-resident functions
            // that call down to the site. A trait object or a function pointer
            // is the usual way in.
            let mut entries: Vec<String> = back_reach(&rev, w.func, &BTreeSet::new())
                .into_iter()
                .filter(|f| m.table.contains(f))
                .map(|f| format!("  {}", m.label(f)))
                .collect();
            let more = entries.len().saturating_sub(MEMBER_CAP);
            entries.truncate(MEMBER_CAP);
            if more > 0 {
                entries.push(format!("  … {more} more"));
            }
            let entries = if entries.is_empty() {
                String::new()
            } else {
                format!(
                    "\n\nIndirectly callable functions that call down to it, \
                     where a `dyn` call or a function pointer may enter:\n\n{}",
                    entries.join("\n")
                )
            };
            return Err(format!(
                "wasm_block_check: cannot verify {label}.\n\
                 \n\
                 `{}` ({}) is in the module but no `call` path reaches it — not \
                 from a check root, not from the start function, not from a fatal \
                 entry. Something keeps it alive that this tool does not model: a \
                 `call_indirect` through the table, or a relocation lld could not \
                 prove dead. The direct-call graph cannot account for it, so the \
                 fatal-path exemption cannot be argued for it either.{entries}\n\
                 \n\
                 Refusing rather than passing: a site whose liveness is \
                 unexplained is precisely the case a green result must not \
                 cover.",
                m.label(w.func),
                w.kinds.join(" and ")
            ));
        }
        // 3. A live, ordinary path.
        if live_pruned.contains(&w.func) {
            red.push(w);
            continue;
        }
        // 4. Exempt — if the direct edges are complete for the region.
        let leaks = side_condition(&m, &rev, w.func, &cut);
        if !leaks.is_empty() {
            return Err(format!(
                "wasm_block_check: cannot verify {label}.\n\
                 \n\
                 `{}` is only reachable through {}, which is why it would be \
                 exempt. But the exemption assumes the direct call edges are \
                 complete for the region between the entry and the wait, and \
                 {} function(s) in that region are indirectly callable — they \
                 appear in an element segment, a global initialiser or a \
                 `ref.func`:\n\n{}\n\n\
                 A `call_indirect` could enter the region without passing the \
                 entry, so the exemption would be a soundness claim this \
                 analysis does not have. Either pin the function above as a \
                 fatal entry in its own right, if a wait behind it really does \
                 only run on a dying thread, or stop exempting.",
                m.label(w.func),
                FATAL_ENTRIES
                    .iter()
                    .map(|f| f.what)
                    .collect::<Vec<_>>()
                    .join(" / "),
                leaks.len(),
                leaks
                    .iter()
                    .map(|f| format!("  {}", m.label(*f)))
                    .collect::<Vec<_>>()
                    .join("\n")
            ));
        }
        exempt_fatal += 1;
    }

    // Functions marked `#[frustrate::no_block(allow)]`: each calls the
    // marker first, and no claim's walk enters one, so nothing under it is a
    // finding.
    let marker = m
        .names
        .iter()
        .find(|(f, n)| m.scan_of(**f).is_some() && own_path(n) == MARKER_PATH)
        .map(|(f, _)| *f);
    let marked: BTreeSet<u32> = match marker {
        Some(k) => (0..m.bodies.len() as u32)
            .map(|n| n + m.func_imports)
            .filter(|f| *f != k && m.scan_of(*f).is_some_and(|s| s.calls.contains(&k)))
            .collect(),
        None => BTreeSet::new(),
    };
    let mut stops = cut.clone();
    stops.extend(&marked);

    // What a silence names: an import by its field, a function by its path.
    let key_of = |f: u32| -> String {
        match m.imports.get(f as usize) {
            Some((_, field)) => field.clone(),
            None => m
                .names
                .get(&f)
                .map(|n| own_path(n).join("::"))
                .unwrap_or_else(|| m.label(f)),
        }
    };
    // The check target's silences apply to every claim.
    let target_silenced: BTreeSet<u32> = red
        .iter()
        .map(|w| w.func)
        .filter(|f| sinks.allowed.contains(&key_of(*f)))
        .collect();
    let findable: BTreeSet<u32> = red
        .iter()
        .map(|w| w.func)
        .filter(|f| !target_silenced.contains(f))
        .collect();
    let display = |sym: &str| -> String {
        claims
            .and_then(|c| c.roots().find(|(s, _)| s == sym).map(|(_, d)| d.clone()))
            .unwrap_or_else(|| sym.to_string())
    };
    let empty: Vec<String> = Vec::new();
    let member_allow = |sym: &str| -> (&str, &Vec<String>) {
        claims
            .and_then(|c| c.ids.get(sym).and_then(|id| c.allow.get(id).map(|a| (id.as_str(), a))))
            .unwrap_or(("", &empty))
    };
    let mut applied = silences::Applied::default();
    let mut listed: Vec<String> = Vec::new();
    let mut used_target: BTreeSet<String> = BTreeSet::new();
    // Marked functions a claim reaches, by path: whether any instance holds a
    // finding the marker keeps out of the verdict.
    let mut marks: BTreeMap<String, bool> = BTreeMap::new();
    // Grouped by member, because that is the unit an author fixes: one slow
    // marker reached from forty members is forty findings, not one. Each
    // member gets the shortest path to the nearest site it reaches.
    let mut members: Vec<(String, Vec<u32>, usize)> = Vec::new();
    for (r, sym) in roots.iter().zip(&root_names) {
        let closure = reach(&m, &[*r], &stops);
        for k in marked.iter().filter(|k| {
            closure
                .iter()
                .any(|f| m.scan_of(*f).is_some_and(|s| s.calls.contains(k)))
        }) {
            let key = ir::marked_key(m.names.get(k).map(String::as_str).unwrap_or(""));
            let mut inside = stops.clone();
            inside.remove(k);
            let live = reach(&m, &[*k], &inside).iter().any(|f| findable.contains(f));
            *marks.entry(key).or_default() |= live;
        }
        used_target.extend(closure.iter().filter(|f| target_silenced.contains(f)).map(|f| key_of(*f)));
        let (id, mine) = member_allow(sym);
        let mut used_mine: BTreeSet<String> = BTreeSet::new();
        let reached: BTreeSet<u32> = closure
            .iter()
            .copied()
            .filter(|f| findable.contains(f))
            .filter(|f| {
                let k = key_of(*f);
                let silenced = mine.contains(&k);
                if silenced {
                    used_mine.insert(k);
                }
                !silenced
            })
            .collect();
        for w in mine.iter().filter(|w| used_mine.contains(*w)) {
            listed.push(format!("  `{w}` in {}", display(sym)));
            applied.silences.insert(silences::Silence::Member(id.to_string(), w.clone()));
        }
        if let Some(path) = witness(&m, &[*r], &reached, &stops) {
            members.push((display(sym), path, reached.len()));
        }
    }
    // Live only from the start function: no member to name.
    let from_start: BTreeSet<u32> = match m.start {
        Some(st) => reach(&m, &[st], &stops),
        None => BTreeSet::new(),
    };
    used_target.extend(from_start.iter().filter(|f| target_silenced.contains(f)).map(|f| key_of(*f)));
    let start_red: Vec<&&Finding> = red
        .iter()
        .filter(|w| findable.contains(&w.func) && from_start.contains(&w.func))
        .collect();
    for (path, live) in marks {
        if live {
            listed.push(format!("  `{path}` in every claim (`#[frustrate::no_block(allow)]`)"));
            applied.silences.insert(silences::Silence::Mark(path));
        } else {
            applied.idle_marks.insert(path);
        }
    }
    for w in sinks.allowed.iter().filter(|w| used_target.contains(*w)) {
        listed.push(format!("  `{w}` in every claim (the check target's `allow`)"));
        applied.silences.insert(silences::Silence::Target(w.clone()));
    }

    let slow_note = if sinks.is_empty() {
        String::new()
    } else {
        format!(
            ", checked against {} slow name(s) ({})",
            sinks.count(),
            sinks.text()
        )
    };
    let no_waits = match (m.atomics == Some(false), sinks.wasi) {
        (true, true) => {
            " The module targets wasip1 without `+atomics`, so its waits are \
             `poll_oneoff`, its only import that sleeps or polls."
        }
        (true, false) => {
            " The module was built without `+atomics`, so it can hold no wait \
             instruction: this checked only the slow names."
        }
        _ => "",
    };
    if members.is_empty() && start_red.is_empty() {
        let silence_note = if listed.is_empty() {
            String::new()
        } else {
            format!("\nSilenced:\n{}", listed.join("\n"))
        };
        let text = format!(
            "wasm_block_check: {label} is clean — {} `no_block` root(s){slow_note}, \
             {} site(s) exempt on the fatal-path rule, {} on the start function{}.{no_waits}{silence_note}",
            roots.len(),
            exempt_fatal,
            exempt_start,
            claims.map(Claims::accounting).unwrap_or_default()
        );
        return Ok(Green { text, applied });
    }

    let kinds_of = |f: u32| -> String {
        let kinds = red
            .iter()
            .find(|w| w.func == f)
            .map(|w| w.kinds.join(", "))
            .unwrap_or_default();
        format!("{kinds} (silence it as `{}`)", key_of(f))
    };
    let mut msg = format!(
        "wasm_block_check: {label}: {} `#[bridge(no_block)]` member(s) reach {}.\n\n",
        members.len(),
        match (m.atomics == Some(false), sinks.is_empty()) {
            (true, _) if sinks.wasi => "a wait or a function named slow",
            (true, _) => "a function named slow",
            (false, true) => "a wait instruction",
            (false, false) => "a wait instruction or a function named slow",
        }
    );
    for (n, (who, path, sites)) in members.iter().enumerate() {
        let last = *path.last().unwrap();
        if n >= WITNESSED {
            if n == WITNESSED {
                msg.push_str("  Without paths:\n");
            }
            msg.push_str(&format!("  {who} — reaches {}\n", m.label(last)));
            continue;
        }
        msg.push_str(&format!("  {who}\n      reaches {} — {}\n", m.label(last), kinds_of(last)));
        if *sites > 1 {
            msg.push_str(&format!("      and {} other site(s)\n", sites - 1));
        }
        for (d, f) in path.iter().enumerate() {
            msg.push_str(&format!("        {}{}\n", "  ".repeat(d.min(8)), m.label(*f)));
        }
        msg.push('\n');
    }
    for w in &start_red {
        msg.push_str(&format!(
            "  {} — {}, reached from the start function\n",
            m.label(w.func),
            kinds_of(w.func)
        ));
    }
    msg.push_str(
        "\nThese paths are not panic paths: they are what a claimed body does \
         when nothing has gone\nwrong. A claimed member's caller may be the \
         main thread, where a wait traps on threaded\nwasm and stalls \
         elsewhere, and where slow work stalls the UI.\n\
         \n\
         Three ways out, in the order they are usually right:\n\
         \n\
         1. Take it out of the body — a lock-free structure, or state behind \
         an Actor so the\n   ownership, not a mutex, does the serialising; \
         slow work moved behind a member that\n   is not `sync`.\n\
         2. Make the member `async`: the body is dispatched off the caller, \
         and an `.await`\n   yields where a lock would have parked the thread.\n\
         3. Drop the claim. `no_block` is a promise to a caller; withdrawing \
         it is honest, and\n   `on_contention` plus \
         `Capabilities::blocking_allowed` already describe a body that\n   \
         may block.\n\
         \n\
         If a finding cannot stall the caller in practice, silence it: \
         `no_block(allow(<name>))` on the\nmember, its impl or its file \
         (`bridge_file!`), or `allow` in the check target for every claim, or \
         put\n`#[frustrate::no_block(allow)]` on a function of your own that \
         the member reaches.\n\
         \n",
    );
    msg.push_str("Sites behind these entries are exempt, and were not counted above:\n");
    for f in FATAL_ENTRIES {
        msg.push_str(&format!("  {} — {}.\n", f.what, f.why));
    }
    Err(msg)
}

/// How many members a red verdict gives a full path for. The rest are named
/// with the site they reach, so the verdict is still the whole inventory.
const WITNESSED: usize = 32;

/// Control 3 of both scanners: the roots are the ones claimed. With a census
/// this is an equality in both directions, which is strictly sharper than "at
/// least one root": losing *some* roots is otherwise invisible, and the
/// artifact would be scanned — and pass — over whatever remained.
fn match_census(root_names: &[String], claims: Option<&Claims>, label: &str) -> Result<(), String> {
    match claims {
        Some(c) => {
            let missing: Vec<&(String, String)> = c
                .roots()
                .filter(|(sym, _)| !root_names.contains(sym))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "wasm_block_check: {label} is missing the check root of {} \
                     claim(s) the census lists:\n\n{}\n\n\
                     A root is emitted for every artifact-settled \
                     `#[bridge(no_block)]` member, so either `--cfg \
                     frustrate_block_check` never reached rustc — in which case \
                     the module's real exports were suppressed by something else \
                     and nothing below would mean anything — or the module and \
                     the census were generated from different sources. Rebuild \
                     both.",
                    missing.len(),
                    missing
                        .iter()
                        .map(|(sym, display)| format!("  {display} — expected `{sym}`"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            let extra: Vec<&String> = root_names
                .iter()
                .filter(|n| !c.roots().any(|(sym, _)| sym == *n))
                .collect();
            if !extra.is_empty() {
                return Err(format!(
                    "wasm_block_check: {label} exports check root(s) the census \
                     does not claim:\n\n{}\n\n\
                     The module and the census disagree about what is claimed, so \
                     a scan over these roots answers a question nobody asked. \
                     Rebuild both from the same sources.",
                    extra
                        .iter()
                        .map(|n| format!("  {n}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
        }
        None => {
            if root_names.is_empty() {
                return Err(format!(
                    "wasm_block_check: {label} exports no `{ROOT_PREFIX}*` function, so \
                     this check covers nothing and its silence means nothing.\n\
                     \n\
                     Either no member in this module is `#[bridge(no_block)]` — claim \
                     one, or drop the check target — or the artifact was built without \
                     `--cfg frustrate_block_check`, in which case the roots were never \
                     emitted and the module's real exports were suppressed by something \
                     else."
                ));
            }
        }
    }
    Ok(())
}

/// A shortest path from the roots to the nearest of `targets` over the pruned
/// graph, for the message.
fn witness(
    m: &Module,
    roots: &[u32],
    targets: &BTreeSet<u32>,
    cut: &BTreeSet<u32>,
) -> Option<Vec<u32>> {
    let mut prev: BTreeMap<u32, u32> = BTreeMap::new();
    let mut seen: BTreeSet<u32> = BTreeSet::new();
    let mut queue: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
    for r in roots {
        if !cut.contains(r) && seen.insert(*r) {
            queue.push_back(*r);
        }
    }
    while let Some(f) = queue.pop_front() {
        if targets.contains(&f) {
            let mut path = vec![f];
            let mut at = f;
            while let Some(p) = prev.get(&at) {
                path.push(*p);
                at = *p;
            }
            path.reverse();
            return Some(path);
        }
        let Some(s) = m.scan_of(f) else { continue };
        for c in &s.calls {
            if !cut.contains(c) && seen.insert(*c) {
                prev.insert(*c, f);
                queue.push_back(*c);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- report --

/// What an export is, for the ancestry lines.
///
/// The buckets exist because "which exports keep this wait alive" is only
/// useful if a reader can tell a *claimed body* from the plumbing wasm-bindgen
/// forces into every module. A `__wbindgen_describe_*` shim exists so the
/// bindgen post-pass can read a type; it is never called at run time and
/// nothing a user writes reaches it. A `__wbindgen_malloc` is called by the
/// generated JS. Neither is a body anyone claimed, and lumping them in with
/// `frustrate_<crate>_call_sync` would hide the only distinction the report is
/// for.
///
/// Ordering is significant: `__wbindgen_describe_intounderlyingsource_pull`
/// matches three of these prefixes, and the describe shim is what it is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ExportKind {
    /// `frustrate_check_block_*` — a claimed member's check root. Only a
    /// `--cfg frustrate_block_check` artifact has these.
    CheckRoot,
    /// `frustrate_<crate>_call_sync` / `_call_async` / `_schema_hash`
    /// — the production module's dispatch entries. Every bridged member is
    /// behind one, so reaching a wait *from* one is necessary but not
    /// sufficient for "a claimed body reaches it": the witness path is what
    /// says which member.
    Dispatch,
    /// `__wbindgen_describe*` — type-description shims read by the bindgen
    /// post-pass and never called at run time.
    Describe,
    /// `__wbindgen_malloc` / `realloc` / `free` / `exn_store` /
    /// `add_to_stack_pointer`, `__externref_table_*`: the allocator and
    /// reference-table plumbing the generated JS calls.
    Intrinsic,
    /// `__wbg_*`, `intounderlying*`, and anything else `#[wasm_bindgen]`
    /// exported — closure invokers and JS-callable methods.
    Bindgen,
    /// `__wasm_init_tls`, `__wasm_call_ctors` and friends: wasm-ld's own.
    Linker,
    /// Everything else. `ring_core_*` lands here, and so would a stray
    /// `#[no_mangle]`.
    Other,
}

impl ExportKind {
    fn label(self) -> &'static str {
        match self {
            ExportKind::CheckRoot => "check root",
            ExportKind::Dispatch => "frustrate dispatch",
            ExportKind::Describe => "bindgen describe",
            ExportKind::Intrinsic => "bindgen intrinsic",
            ExportKind::Bindgen => "wasm-bindgen export",
            ExportKind::Linker => "linker export",
            ExportKind::Other => "other",
        }
    }
}

/// Classified on the **raw** export name, never on [`readable`]'s output: these
/// are `#[no_mangle]` symbols, which `readable` returns unchanged, and keying
/// on the demangler would make the buckets depend on it.
fn classify_export(name: &str) -> ExportKind {
    const INTRINSICS: &[&str] = &[
        "__wbindgen_malloc",
        "__wbindgen_realloc",
        "__wbindgen_free",
        "__wbindgen_exn_store",
        "__wbindgen_add_to_stack_pointer",
        "__wbindgen_start",
    ];
    if name.starts_with(ROOT_PREFIX) {
        ExportKind::CheckRoot
    } else if name.starts_with("frustrate_") {
        ExportKind::Dispatch
    } else if name.starts_with("__wbindgen_describe") {
        ExportKind::Describe
    } else if INTRINSICS.contains(&name) || name.starts_with("__externref_") {
        ExportKind::Intrinsic
    } else if name.starts_with("__wbindgen") || name.starts_with("__wbg_") || name.starts_with("intounderlying") {
        ExportKind::Bindgen
    } else if name.starts_with("__wasm_") {
        ExportKind::Linker
    } else {
        ExportKind::Other
    }
}

/// `check root 1, bindgen describe 412` — a histogram in a fixed bucket order.
fn histogram(kinds: &[ExportKind]) -> String {
    let all = [
        ExportKind::CheckRoot,
        ExportKind::Dispatch,
        ExportKind::Describe,
        ExportKind::Intrinsic,
        ExportKind::Bindgen,
        ExportKind::Linker,
        ExportKind::Other,
    ];
    let parts: Vec<String> = all
        .iter()
        .filter_map(|k| {
            let n = kinds.iter().filter(|c| *c == k).count();
            (n > 0).then(|| format!("{} {n}", k.label()))
        })
        .collect();
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(", ")
    }
}

/// The module codegen writes its per-member glue into.
///
/// A production module has one dispatch export for the whole bridge, so
/// "reachable from `frustrate_<crate>_call_sync`" says nothing about *which*
/// member. The glue does: `iroh_rust::frustrate_generated::actor_2_connect` is one
/// member's entry and nothing else's. Listing the glue in a site's ancestry is
/// therefore the closest thing a production module has to the per-member cone a
/// check artifact would isolate by construction.
const GLUE_MODULE: &str = "frustrate_generated";

/// Is this the *entry* of one bridged member?
///
/// Structural rather than a substring test, and that is the whole point: a
/// closure inside the dispatcher, and every `catch_unwind` monomorphised over
/// one, carries `frustrate_generated` somewhere in its symbol too. A member entry
/// is exactly `<crate>::frustrate_generated::<name>` with nothing after it,
/// which [`readable`] renders as three components (a closure adds a fourth,
/// `{closure#0}`).
fn is_member_glue(label: &str) -> bool {
    let parts: Vec<&str> = label.split("::").collect();
    parts.len() == 3 && parts[1] == GLUE_MODULE
}

/// How many glue entries and witness paths one site prints before it stops.
const MEMBER_CAP: usize = 16;
const WITNESS_CAP: usize = 4;

/// How many callers deep the backward tree goes, and how wide per level.
///
/// Both are legibility limits, not analysis limits — the ancestry *sets* below
/// are exact and unbounded. A futex site in a real module has hundreds of
/// callers over a dozen levels, and printing them all produces something no
/// reader gets through.
const TREE_DEPTH: usize = 4;
const TREE_FANOUT: usize = 6;

/// A depth-limited backward walk, for reading.
///
/// A `cut` member is printed and **not** descended through, because that is
/// exactly where the fatal-path rule stops the ordinary graph; seeing the tree
/// terminate at `rust_begin_unwind` is how a reader recognises a panic-only
/// site.
#[allow(clippy::too_many_arguments)]
fn backward_tree(
    m: &Module,
    rev: &BTreeMap<u32, Vec<u32>>,
    node: u32,
    cut: &BTreeSet<u32>,
    depth: usize,
    indent: usize,
    seen: &mut BTreeSet<u32>,
    out: &mut String,
) {
    if depth == 0 {
        return;
    }
    let mut callers: Vec<u32> = rev.get(&node).cloned().unwrap_or_default();
    callers.sort_unstable();
    callers.dedup();
    let pad = "  ".repeat(indent + 3);
    for c in callers.iter().take(TREE_FANOUT) {
        if cut.contains(c) {
            out.push_str(&format!("{pad}{}  [fatal entry — the ordinary graph stops here]\n", m.label(*c)));
            continue;
        }
        if !seen.insert(*c) {
            out.push_str(&format!("{pad}{}  [shown above]\n", m.label(*c)));
            continue;
        }
        out.push_str(&format!("{pad}{}\n", m.label(*c)));
        backward_tree(m, rev, *c, cut, depth - 1, indent + 1, seen, out);
    }
    if callers.len() > TREE_FANOUT {
        out.push_str(&format!("{pad}… {} more caller(s)\n", callers.len() - TREE_FANOUT));
    }
}

/// Inventory every wait site in a module, with no verdict attached.
///
/// Deliberately **not** [`run`]: it applies none of that function's controls,
/// because its whole use is on modules those controls reject — a production
/// cdylib has thousands of exports and no check root, which is control 2's
/// error and rightly so. Nothing here decides anything; it prints what the
/// direct-call graph knows so a person can decide.
///
/// Two ancestries are printed per site, and both are needed. **Full** (nothing
/// cut) is the liveness answer: it explains why lld kept the function, and in a
/// real module it is close to the whole graph, because every Rust function can
/// panic and the panic path takes a lock. **Ordinary** ([`FATAL_ENTRIES`] cut)
/// is the answer that discriminates — it is the graph rule 3 decides red on, so
/// a site with no export in its ordinary ancestry is one the gate would exempt.
/// Printing only the first would make every site look reachable from
/// everything; printing only the second would not say what keeps it alive.
fn report(path: &str, label: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let m = parse(&bytes)?;
    let rev = reverse_edges(&m);
    let cut = fatal_entries(&m);

    let fexports: Vec<(&str, u32)> = m
        .exports
        .iter()
        .filter(|(_, k, _)| *k == 0)
        .map(|(n, _, i)| (n.as_str(), *i))
        .collect();

    let mut out = format!("wasm_block_check --report: {label}\n\n");
    out.push_str(&format!(
        "  {} function(s): {} imported, {} defined\n",
        m.func_imports as usize + m.bodies.len(),
        m.func_imports,
        m.bodies.len()
    ));
    out.push_str(&format!(
        "  {} function export(s): {}\n",
        fexports.len(),
        histogram(&fexports.iter().map(|(n, _)| classify_export(n)).collect::<Vec<_>>())
    ));
    out.push_str(&format!(
        "  {} function(s) table-resident (element segment, global initialiser or ref.func)\n",
        m.table.len()
    ));
    out.push_str(&match m.start {
        Some(s) => format!("  start function: {}\n", m.label(s)),
        None => "  start function: none\n".into(),
    });
    out.push_str(&format!(
        "  fatal entries present: {}\n",
        if cut.is_empty() {
            "none".to_string()
        } else {
            cut.iter().map(|f| m.label(*f)).collect::<Vec<_>>().join(", ")
        }
    ));
    if !m.names.values().any(|n| n.contains(SENTINEL)) {
        out.push_str(
            "  WARNING: no mangled Rust symbol in the name section, so every label below is a \
             bare index.\n",
        );
    }

    let waits: Vec<Finding> = m
        .bodies
        .iter()
        .enumerate()
        .filter(|(_, s)| !s.waits.is_empty())
        .map(|(n, s)| Finding {
            func: m.func_imports + n as u32,
            kinds: {
                let mut k: Vec<String> = s.waits.iter().map(|k| k.to_string()).collect();
                k.sort_unstable();
                k.dedup();
                k
            },
            waits: true,
        })
        .collect();

    out.push_str(&format!("\n{} wait site(s).\n", waits.len()));

    let mut ordinary_free = 0usize;
    let mut ordinary_reached = 0usize;

    for (n, w) in waits.iter().enumerate() {
        let full = back_reach(&rev, w.func, &BTreeSet::new());
        let ordinary = back_reach(&rev, w.func, &cut);
        let alive: Vec<(&str, u32)> =
            fexports.iter().filter(|(_, i)| full.contains(i)).copied().collect();
        let ordinary_exports: Vec<(&str, u32)> =
            fexports.iter().filter(|(_, i)| ordinary.contains(i)).copied().collect();
        let resident = side_condition(&m, &rev, w.func, &cut);

        out.push_str(&format!(
            "\n[{}] {} — {}\n",
            n + 1,
            m.label(w.func),
            w.kinds.join(" and ")
        ));
        out.push_str(&format!(
            "    table-resident: {}\n",
            if m.table.contains(&w.func) { "yes — an indirect call can enter it, so direct edges are not complete for it" } else { "no" }
        ));
        out.push_str(&format!(
            "    is the start function: {}\n",
            if m.start == Some(w.func) { "yes" } else { "no" }
        ));

        // Ordinary ancestry — the graph rule 3 decides on.
        out.push_str(&format!(
            "    ordinary ancestry (fatal entries cut): {} function(s), {} of them table-resident\n",
            ordinary.len(),
            resident.len()
        ));
        if ordinary_exports.is_empty() {
            ordinary_free += 1;
            out.push_str(
                "      kept alive by no export over ordinary edges — this is the shape the \
                 fatal-path rule exempts\n",
            );
        } else {
            ordinary_reached += 1;
            out.push_str(&format!(
                "      kept alive by {} export(s): {}\n",
                ordinary_exports.len(),
                histogram(&ordinary_exports.iter().map(|(e, _)| classify_export(e)).collect::<Vec<_>>())
            ));
            for (name, _) in ordinary_exports.iter().take(MEMBER_CAP) {
                out.push_str(&format!("        {name}\n"));
            }
            if ordinary_exports.len() > MEMBER_CAP {
                out.push_str(&format!("        … {} more\n", ordinary_exports.len() - MEMBER_CAP));
            }
            // Which *members* — the glue names one, the dispatch export does not.
            let glue: Vec<u32> = ordinary
                .iter()
                .copied()
                .filter(|f| is_member_glue(&m.label(*f)))
                .collect();
            out.push_str(&format!(
                "      bridged member entries in that ancestry ({}):{}\n",
                glue.len(),
                if glue.is_empty() {
                    " none — no member's own entry is on an ordinary path to this wait"
                } else {
                    ""
                }
            ));
            for g in glue.iter().take(MEMBER_CAP) {
                out.push_str(&format!("        {}\n", m.label(*g)));
            }
            if glue.len() > MEMBER_CAP {
                out.push_str(&format!("        … {} more\n", glue.len() - MEMBER_CAP));
            }
            // Witness from the member entry where there is one: the dispatch
            // export's own prefix is a dozen frames of `catch_unwind` shared by
            // every member, and the part that says *whose body* starts at the
            // glue. Where no member entry is on the path — the runtime's own
            // exports — witness from the export instead, one per class.
            let mut sources: Vec<(String, u32)> = if glue.is_empty() {
                let mut shown: BTreeSet<ExportKind> = BTreeSet::new();
                ordinary_exports
                    .iter()
                    .filter(|(n, _)| shown.insert(classify_export(n)))
                    .map(|(n, i)| (format!("{} `{n}`", classify_export(n).label()), *i))
                    .collect()
            } else {
                glue.iter().map(|g| (format!("member entry {}", m.label(*g)), *g)).collect()
            };
            sources.truncate(WITNESS_CAP);
            for (what, idx) in sources {
                out.push_str(&format!("      via {what}:\n"));
                match witness(&m, &[idx], &BTreeSet::from([w.func]), &cut) {
                    Some(p) => {
                        for (d, f) in p.iter().enumerate() {
                            out.push_str(&format!("        {}{}\n", "  ".repeat(d.min(8)), m.label(*f)));
                        }
                    }
                    None => out.push_str("        (no path — the source is the site itself)\n"),
                }
            }
        }

        // Full ancestry — the liveness answer.
        out.push_str(&format!(
            "    full ancestry (nothing cut): {} function(s)\n      kept alive by {} export(s): {}\n",
            full.len(),
            alive.len(),
            histogram(&alive.iter().map(|(e, _)| classify_export(e)).collect::<Vec<_>>())
        ));
        if alive.is_empty() {
            out.push_str(
                "      no export reaches it at all: it is kept by the table, by a relocation, or \
                 by the start function\n",
            );
        }
        out.push_str(&format!(
            "    reached by the start function: {}\n",
            match m.start {
                Some(s) if full.contains(&s) => "yes",
                Some(_) => "no",
                None => "no start function",
            }
        ));

        let mut direct: Vec<u32> = rev.get(&w.func).cloned().unwrap_or_default();
        direct.sort_unstable();
        direct.dedup();
        out.push_str(&format!("    direct callers ({}):\n", direct.len()));
        let mut seen: BTreeSet<u32> = BTreeSet::new();
        seen.insert(w.func);
        backward_tree(&m, &rev, w.func, &cut, TREE_DEPTH, 0, &mut seen, &mut out);
    }

    out.push_str(&format!(
        "\nSummary: {} site(s) with no export in their ordinary ancestry, {} with one.\n\
         An ordinary export ancestry is not by itself a verdict — `{}` reaches every bridged \
         member, so the witness path above is what says whose body it is.\n",
        ordinary_free, ordinary_reached, "frustrate_<crate>_call_sync"
    ));
    Ok(out)
}

// -------------------------------------------------------------- self-test --

/// Prove the predicate bites before letting it say "clean".
///
/// `tools/check_no_park.dart` records why this is not optional: four scanners
/// were written for the parking report and the first passed its own sabotage
/// test while matching nothing, because a pattern that matches nothing looks
/// exactly like a codebase with nothing to find. Here the risk is sharper —
/// `0xFE` is an ordinary byte inside an `i32.const` and inside a memarg offset,
/// so a scanner that pattern-matched would be red on innocent code and a
/// decoder that mis-skipped an immediate would be green on guilty code. Both
/// directions are checked.
fn self_test() -> Result<(), String> {
    let with_wait = decode_body(&[
        0x41, 0x00, // i32.const 0
        0x41, 0x00, // i32.const 0
        0x42, 0x7f, // i64.const -1
        0xfe, 0x01, 0x02, 0x00, // memory.atomic.wait32 align=2 offset=0
        0x1a, // drop
    ])?;
    if with_wait.waits != vec![WAIT32] {
        return Err("self-test: a body containing memory.atomic.wait32 was not \
                    detected, so the gate cannot fire and its \"clean\" is a lie"
            .into());
    }
    let wait64 = decode_body(&[0xfe, 0x02, 0x03, 0x00, 0x1a])?;
    if wait64.waits != vec![WAIT64] {
        return Err("self-test: memory.atomic.wait64 was not detected".into());
    }
    // The false-positive direction: 0xFE 0x01 appearing as data, not as an
    // opcode. `i32.const 254` is `0x41 0xFE 0x01`; a load with offset 254 is
    // `0x28 0x02 0xFE 0x01`. Both are exactly the wait32 opcode pair.
    let innocent = decode_body(&[
        0x41, 0xfe, 0x01, // i32.const 254
        0x28, 0x02, 0xfe, 0x01, // i32.load align=2 offset=254
        0x42, 0xfe, 0x01, // i64.const 254
        0xfe, 0x00, 0x02, 0x00, // memory.atomic.notify — wakes, never blocks
        0x1a,
    ])?;
    if !innocent.waits.is_empty() {
        return Err(format!(
            "self-test: a body with 0xFE-valued *immediates* and a notify was \
             read as {} wait(s), so this gate would fire on innocent code",
            innocent.waits.len()
        ));
    }
    Ok(())
}

/// Decode a bare instruction sequence, appending the `end` the frame needs.
fn decode_body(instrs: &[u8]) -> Result<Scan, String> {
    let mut d = instrs.to_vec();
    d.push(0x0b);
    let mut s = Scan::default();
    scan_ops(OperatorsReader::new(BinaryReader::new(&d, 0)), &mut s)
        .map_err(|e| format!("self-test: {e}"))?;
    Ok(s)
}

/// The environment variable that makes this binary a test's runner: it names
/// the verdict a check action wrote, which the runner prints. The verdict was
/// reached at build time, so a red check fails the test's build and a test
/// that runs at all is green; running it shows what was checked.
const VERDICT_ENV: &str = "FRUSTRATE_NO_BLOCK_VERDICT";

fn main() -> ExitCode {
    if let Some(path) = std::env::var_os(VERDICT_ENV) {
        return match std::fs::read_to_string(&path) {
            Ok(text) => {
                print!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("wasm_block_check: {}: {e}", path.to_string_lossy());
                ExitCode::FAILURE
            }
        };
    }
    // `--slow <path>` and `--io <category>`, repeatable and anywhere on the
    // line, add named sinks to the gate shapes; the positional parsing below
    // never sees them.
    let mut args: Vec<String> = Vec::new();
    let mut slow = Sinks::default();
    // `--ir`: the input is a native build's LLVM IR, not a wasm module.
    let mut ir_mode = false;
    // `--applied <file>`: where a green check writes the silences it applied.
    let mut applied_out: Option<String> = None;
    let mut raw = std::env::args();
    while let Some(a) = raw.next() {
        if a == "--ir" {
            ir_mode = true;
            continue;
        }
        // `--wasi`: the module targets wasip1, so `poll_oneoff` is a wait.
        if a == "--wasi" {
            slow.wasi = true;
            continue;
        }
        if a == "--applied" {
            match raw.next() {
                Some(v) => applied_out = Some(v),
                None => {
                    eprintln!("wasm_block_check: `--applied` wants a value");
                    return ExitCode::FAILURE;
                }
            }
            continue;
        }
        if a != "--slow" && a != "--io" && a != "--allow" {
            args.push(a);
            continue;
        }
        let parsed = raw
            .next()
            .ok_or_else(|| format!("wasm_block_check: `{a}` wants a value"))
            .and_then(|v| {
                match a.as_str() {
                    "--slow" => slow.paths.push(SlowPath::parse(&v)?),
                    "--allow" => slow.allowed.push(v),
                    _ => slow.io.push(io::parse(&v)?),
                }
                Ok(())
            });
        if let Err(e) = parsed {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    }
    if let Err(e) = self_test() {
        eprintln!("wasm_block_check: {e}");
        return ExitCode::FAILURE;
    }
    // `--report` is diagnostic and additive: it reaches no verdict, applies none
    // of `run`'s controls, and succeeds whenever the module decodes. It exists
    // for the modules those controls reject — a production cdylib above all.
    if args.get(1).map(String::as_str) == Some("--report") {
        let Some(path) = args.get(2) else {
            eprintln!("usage: wasm_block_check --report <module.wasm> [label]");
            return ExitCode::FAILURE;
        };
        let label = args.get(3).map(String::as_str).unwrap_or(path);
        return match report(path, label) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("wasm_block_check --report: {e}");
                ExitCode::FAILURE
            }
        };
    }
    // The three gate shapes. `--claims` is what both real drivers pass; the
    // bare positional form is kept because it is what a hand run reaches for,
    // and it is the only one whose verdict rests on the module alone.
    let usage = "usage: wasm_block_check [--wasi] [--slow <path> | --io <category> | --allow <name>]... \
         [--applied <file>] <module.wasm> <label> [marker-out]\n\
         \x20      wasm_block_check [--wasi] [--slow ... | --io ... | --allow ...] [--applied <file>] \
         --claims <census> <module.wasm> <label> [marker-out]\n\
         \x20      wasm_block_check --ir [--slow ... | --io ... | --allow ...] [--applied <file>] \
         [--claims <census>] <module.ll> <label> [marker-out]\n\
         \x20      wasm_block_check --claims-only <census> <label> [marker-out]\n\
         \x20      wasm_block_check [--allow <name>]... --silences <census> <applied>... <label> <marker-out>\n\
         \x20      wasm_block_check --report <module.wasm> [label]";
    let (verdict, marker) = match args.get(1).map(String::as_str) {
        Some("--claims") if args.len() >= 5 => {
            let claims = match std::fs::read_to_string(&args[2])
                .map_err(|e| format!("wasm_block_check: {}: {e}", args[2]))
                .and_then(|t| Claims::parse(&t, &args[2]))
            {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            };
            // Census-only states first, so "nothing to prove" can never arrive
            // as an empty root set matching an empty claim set.
            let v = claims.refuse_empty(&args[4]).and_then(|()| {
                if claims.root_count() == 0 {
                    Err(format!(
                        "wasm_block_check: every claim in {} is settled by \
                         placement, so {} can prove nothing.\n\
                         \n\
                         Build no artifact for it: drop `crate` from the check \
                         target and it checks the census alone, which needs no \
                         toolchain at all.",
                        claims.who(),
                        args[3]
                    ))
                } else {
                    if ir_mode {
                        ir::run_ir(&args[3], &args[4], Some(&claims), &slow)
                    } else {
                        run_with(&args[3], &args[4], Some(&claims), &slow)
                    }
                }
            });
            (v, args.get(5))
        }
        // Every silence the check target declares, against the files its
        // analyses wrote with `--applied` (silences.rs).
        Some("--silences") if args.len() >= 5 => {
            let n = args.len();
            let v = std::fs::read_to_string(&args[2])
                .map_err(|e| format!("wasm_block_check: {}: {e}", args[2]))
                .and_then(|t| Claims::parse(&t, &args[2]))
                .and_then(|c| {
                    let checks = args[3..n - 2]
                        .iter()
                        .map(|p| {
                            std::fs::read_to_string(p)
                                .map_err(|e| format!("wasm_block_check: {p}: {e}"))
                                .and_then(|t| silences::Applied::parse(&t, p))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    silences::judge(&args[n - 2], &c, &slow.allowed, &checks)
                });
            (v.map(|text| Green { text, applied: silences::Applied::default() }), args.get(n - 1))
        }
        Some("--claims-only") if args.len() >= 4 => {
            let v = std::fs::read_to_string(&args[2])
                .map_err(|e| format!("wasm_block_check: {}: {e}", args[2]))
                .and_then(|t| Claims::parse(&t, &args[2]))
                .and_then(|c| claims_only(&args[3], &c));
            (v.map(|text| Green { text, applied: silences::Applied::default() }), args.get(4))
        }
        Some(flag) if flag.starts_with("--") => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
        _ if args.len() >= 3 && ir_mode => (ir::run_ir(&args[1], &args[2], None, &slow), args.get(3)),
        _ if args.len() >= 3 => (run_with(&args[1], &args[2], None, &slow), args.get(3)),
        _ => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };
    let label = match args.get(1).map(String::as_str) {
        Some("--claims") => args.get(4),
        _ => args.get(2),
    };
    match verdict {
        Ok(Green { text: report, applied }) => {
            println!("{report}");
            if let (Some(out), Some(label)) = (&applied_out, label) {
                if let Err(e) = std::fs::write(out, applied.text(label)) {
                    eprintln!("wasm_block_check: writing {out}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            if let Some(marker) = marker {
                // Bazel's declared output for the check action; writing it is
                // what lets a pass be cached. The test runner prints it.
                if let Err(e) = std::fs::write(marker, format!("{report}\n")) {
                    eprintln!("wasm_block_check: writing {marker}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- a hand-built module ------------------------------------------

    fn u(v: usize, out: &mut Vec<u8>) {
        let mut v = v;
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return;
            }
            out.push(b | 0x80);
        }
    }

    fn s(text: &str, out: &mut Vec<u8>) {
        u(text.len(), out);
        out.extend_from_slice(text.as_bytes());
    }

    fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
        out.push(id);
        u(body.len(), out);
        out.extend_from_slice(body);
    }

    /// A defined function: its name-section name and its instruction bytes
    /// (locals and the trailing `end` are added here).
    #[derive(Clone)]
    struct Fun {
        name: String,
        code: Vec<u8>,
    }

    fn fun(name: &str, code: Vec<u8>) -> Fun {
        Fun {
            name: name.into(),
            code,
        }
    }

    /// `call` to a function index, in the module index space.
    fn call(idx: usize) -> Vec<u8> {
        let mut v = vec![0x10];
        u(idx, &mut v);
        v
    }

    /// A realistic `memory.atomic.wait32`, operands and all.
    fn wait() -> Vec<u8> {
        vec![
            0x41, 0x00, // i32.const 0
            0x41, 0x00, // i32.const 0
            0x42, 0x7f, // i64.const -1
            0xfe, 0x01, 0x02, 0x00, // memory.atomic.wait32
            0x1a, // drop
        ]
    }

    #[derive(Default, Clone)]
    struct Build {
        /// Function imports, which shift the whole index space.
        imports: Vec<(&'static str, &'static str)>,
        funcs: Vec<Fun>,
        /// `(export name, kind, index)`. Defaults to one root per `root_*` fn.
        exports: Vec<(String, u8, u32)>,
        elements: Vec<u32>,
        start: Option<u32>,
        /// Drop the name section entirely.
        no_names: bool,
        /// A `target_features` section with these `+` features, when set.
        features: Option<&'static [&'static str]>,
    }

    impl Build {
        fn wasm(&self) -> Vec<u8> {
            let mut m = b"\0asm\x01\0\0\0".to_vec();

            // One type, `() -> ()`, for everything.
            let mut ty = Vec::new();
            u(1, &mut ty);
            ty.extend_from_slice(&[0x60, 0x00, 0x00]);
            section(1, &ty, &mut m);

            if !self.imports.is_empty() {
                let mut imp = Vec::new();
                u(self.imports.len(), &mut imp);
                for (module, field) in &self.imports {
                    s(module, &mut imp);
                    s(field, &mut imp);
                    imp.push(0); // func
                    u(0, &mut imp); // typeidx
                }
                section(2, &imp, &mut m);
            }

            let mut fs = Vec::new();
            u(self.funcs.len(), &mut fs);
            for _ in &self.funcs {
                u(0, &mut fs);
            }
            section(3, &fs, &mut m);

            // One funcref table, so an element segment is legal.
            let mut tbl = Vec::new();
            u(1, &mut tbl);
            tbl.push(0x70);
            tbl.push(0x00);
            u(self.funcs.len() + self.imports.len() + 1, &mut tbl);
            section(4, &tbl, &mut m);

            let mut mem = Vec::new();
            u(1, &mut mem);
            mem.push(0x00);
            u(1, &mut mem);
            section(5, &mem, &mut m);

            let mut exp = Vec::new();
            u(self.exports.len(), &mut exp);
            for (n, kind, idx) in &self.exports {
                s(n, &mut exp);
                exp.push(*kind);
                u(*idx as usize, &mut exp);
            }
            section(7, &exp, &mut m);

            if let Some(st) = self.start {
                let mut b = Vec::new();
                u(st as usize, &mut b);
                section(8, &b, &mut m);
            }

            if !self.elements.is_empty() {
                let mut el = Vec::new();
                u(1, &mut el); // one segment
                u(0, &mut el); // flags 0: active, table 0, funcidx vector
                el.extend_from_slice(&[0x41, 0x00, 0x0b]); // (i32.const 0)
                u(self.elements.len(), &mut el);
                for f in &self.elements {
                    u(*f as usize, &mut el);
                }
                section(9, &el, &mut m);
            }

            let mut code = Vec::new();
            u(self.funcs.len(), &mut code);
            for f in &self.funcs {
                let mut body = Vec::new();
                u(0, &mut body); // no locals
                body.extend_from_slice(&f.code);
                body.push(0x0b); // end
                u(body.len(), &mut code);
                code.extend_from_slice(&body);
            }
            section(10, &code, &mut m);

            if !self.no_names {
                let mut sub = Vec::new();
                u(self.funcs.len(), &mut sub);
                for (n, f) in self.funcs.iter().enumerate() {
                    u(n + self.imports.len(), &mut sub);
                    s(&f.name, &mut sub);
                }
                let mut custom = Vec::new();
                s("name", &mut custom);
                custom.push(1);
                u(sub.len(), &mut custom);
                custom.extend_from_slice(&sub);
                section(0, &custom, &mut m);
            }
            if let Some(features) = self.features {
                let mut custom = Vec::new();
                s("target_features", &mut custom);
                u(features.len(), &mut custom);
                for f in features {
                    custom.push(b'+');
                    s(f, &mut custom);
                }
                section(0, &custom, &mut m);
            }
            m
        }

        fn check(&self) -> Result<String, String> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            // A fresh path per call, never a hash of the inputs: these tests run
            // in parallel in one process and two of them legitimately build the
            // same module, which content-addressing would make share a path
            // that `fs::write` truncates before it fills. `wasm_std_check`'s
            // suite learned this the intermittent way.
            let dir = std::env::temp_dir().join(format!("wasm_block_check_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("m{}.wasm", NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::write(&p, self.wasm()).unwrap();
            run(p.to_str().unwrap(), "//pkg:target.wasm")
        }

        /// This module with each check root calling a glue function named as
        /// codegen names it, `…::frustrate_generated::call_{id}_{name}`, the
        /// way a real root does: what the census-matched gate requires.
        fn with_glue(&self) -> Build {
            let mut b = self.clone();
            let base = b.imports.len() + b.funcs.len();
            let mut added = 0;
            for (name, kind, idx) in self.exports.iter() {
                let Some(rest) = name.strip_prefix(ROOT_PREFIX) else { continue };
                if *kind != 0 {
                    continue;
                }
                let glue = format!("call_{rest}");
                let sym = format!("_RNvNtCs1_7fixture19frustrate_generated{}{glue}", glue.len());
                b.funcs.push(fun(&sym, vec![0x01]));
                let at = *idx as usize - b.imports.len();
                let mut code = call(base + added);
                code.extend(b.funcs[at].code.clone());
                b.funcs[at].code = code;
                added += 1;
            }
            b
        }

        /// The gate with `--slow` paths and an optional census.
        fn check_slow(&self, slow: &[&str], rows: Option<&str>) -> Result<String, String> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!("wasm_block_slow_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("m{}.wasm", NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::write(&p, self.with_glue().wasm()).unwrap();
            // A category's name is an IO category; anything else a path.
            // `--wasi` rides along as the name `wasi`.
            let sinks = Sinks {
                paths: slow
                    .iter()
                    .filter(|t| io::parse(t).is_err() && **t != "wasi")
                    .map(|t| SlowPath::parse(t).unwrap())
                    .collect(),
                io: slow.iter().filter_map(|t| io::parse(t).ok()).collect(),
                wasi: slow.contains(&"wasi"),
                allowed: Vec::new(),
            };
            let c = rows.map(census).transpose()?;
            run_with(p.to_str().unwrap(), "//pkg:target.wasm", c.as_ref(), &sinks).map(|g| g.text)
        }

        /// The same module through `--report`. Separate from [`Self::check`]
        /// because the two are meant to disagree about what is acceptable
        /// input: `check` rejects a module without check roots, `report`
        /// inventories one.
        fn report(&self) -> Result<String, String> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!("wasm_block_report_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("m{}.wasm", NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::write(&p, self.wasm()).unwrap();
            report(p.to_str().unwrap(), "//pkg:target.wasm")
        }

        /// The same module, with a claim census beside it — what both real
        /// drivers pass.
        fn check_claims(&self, rows: &str) -> Result<String, String> {
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!("wasm_block_claims_{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let p = dir.join(format!("m{}.wasm", NEXT.fetch_add(1, Ordering::Relaxed)));
            std::fs::write(&p, self.with_glue().wasm()).unwrap();
            let c = census(rows)?;
            c.refuse_empty("//pkg:target.wasm")?;
            if c.root_count() == 0 {
                return Err(format!(
                    "wasm_block_check: every claim in {} is settled by placement, \
                     so this artifact can prove nothing.",
                    c.who()
                ));
            }
            run_with(p.to_str().unwrap(), "//pkg:target.wasm", Some(&c), &Sinks::default()).map(|g| g.text)
        }
    }

    /// A census from its rows, through the real parser — so a test that asserts
    /// on a verdict has also asserted the format it was written in.
    fn census(rows: &str) -> Result<Claims, String> {
        Claims::parse(
            &format!("# generated\n{CENSUS_HEADER}\ncrate fixture\n{rows}"),
            "no_block.claims",
        )
    }

    // The real symbol shapes, copied from a module this repo builds.
    const PANIC_FMT: &str = "_RNvNtCsfBCJUfclz7H_4core9panicking9panic_fmt";
    const BEGIN_UNWIND: &str = "_RNvCsd4XCIHxVim2_7___rustc17rust_begin_unwind";
    const HOOK_READ: &str =
        "_RNvMs9_NtNtNtCsbOwqmHKCkC2_3std4sync9nonpoison6rwlockINtB5_6RwLockNtNtBb_9panicking4HookE4readBb_";
    const FUTEX_WAIT: &str = "_RNvNtNtNtNtCsbOwqmHKCkC2_3std3sys3pal4wasm5futex10futex_wait";
    const ALLOC_HOOK: &str = "_RNvNtCsbOwqmHKCkC2_3std5alloc24default_alloc_error_hook";
    const ROOT: &str = "frustrate_check_block_0_tag_join";

    fn root_export(idx: u32) -> (String, u8, u32) {
        (ROOT.to_string(), 0, idx)
    }

    /// Roots, a panic funnel, and a wait behind it — the shape every real check
    /// artifact has. Function indices, with no imports:
    /// 0 root, 1 panic_fmt, 2 rust_begin_unwind, 3 RwLock<Hook>::read,
    /// 4 futex_wait.
    fn panic_shaped(root_code: Vec<u8>) -> Build {
        Build {
            funcs: vec![
                fun(ROOT, root_code),
                fun(PANIC_FMT, call(2)),
                fun(BEGIN_UNWIND, call(3)),
                fun(HOOK_READ, call(4)),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![root_export(0)],
            ..Default::default()
        }
    }

    // ---- the predicate itself -----------------------------------------

    #[test]
    fn the_self_test_passes() {
        self_test().unwrap();
    }

    #[test]
    fn a_module_with_no_wait_instruction_passes() {
        let b = Build {
            funcs: vec![fun(ROOT, call(1)), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0)],
            ..Default::default()
        };
        let ok = b.check().unwrap();
        assert!(ok.contains("clean"), "{ok}");
        assert!(ok.contains("1 `no_block` root"), "{ok}");
    }

    #[test]
    fn a_wait_a_claimed_body_reaches_is_named() {
        let b = Build {
            funcs: vec![
                fun(ROOT, call(1)),
                fun("_ZN8test_api3api7lock_it17h0b1b2c3d4e5f6071E", call(2)),
                fun(PANIC_FMT, vec![0x01]),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![root_export(0)],
            ..Default::default()
        };
        // Route the root's body through the waiter as well, so the wait is
        // genuinely reachable: 0 -> 1 -> 2 is the panic funnel, 1 -> 3 the lock.
        let mut b = b;
        b.funcs[1].code = call(3);
        let e = b.check().unwrap_err();
        assert!(e.contains("test_api::api::lock_it"), "names the caller: {e}");
        assert!(e.contains("futex_wait"), "names the wait site: {e}");
        assert!(e.contains("memory.atomic.wait32"), "{e}");
        assert!(e.contains("Three ways out"), "offers a way out: {e}");
    }

    /// The monomorphisation hash is dropped, as `check_no_park.dart:237` does.
    #[test]
    fn the_report_strips_the_mangling_hash() {
        assert_eq!(
            readable("_ZN3std6thread4park17h6d5c1c9d4f1a2b3cE"),
            "std::thread::park"
        );
        assert_eq!(readable(FUTEX_WAIT), "std::sys::pal::wasm::futex::futex_wait");
        // v0's trailing instantiating crate (`…CsbOwqmHKCkC2_3std`) is not
        // part of the name.
        assert_eq!(
            readable("_RNvNtNtNtCsfBCJUfclz7H_4core9core_arch6wasm326atomic20memory_atomic_wait32CsbOwqmHKCkC2_3std"),
            "core::core_arch::wasm32::atomic::memory_atomic_wait32"
        );
        // v0's `_` separator, and a name that is neither scheme.
        assert_eq!(readable(BEGIN_UNWIND), "__rustc::rust_begin_unwind");
        assert_eq!(readable(ROOT), ROOT);
    }

    /// A `0xFE` byte in an immediate is not an opcode: the false red a
    /// byte-pattern scan would give.
    #[test]
    fn an_fe_valued_immediate_does_not_fire() {
        let mut body = vec![0x41, 0xfe, 0x01]; // i32.const 254
        body.extend_from_slice(&[0x28, 0x02, 0xfe, 0x01, 0x1a]); // i32.load offset=254
        body.extend_from_slice(&[0x42, 0xfe, 0x01, 0x1a]); // i64.const 254
        let b = Build {
            funcs: vec![fun(ROOT, body), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0)],
            ..Default::default()
        };
        assert!(b.check().unwrap().contains("clean"));
    }

    /// `memory.atomic.notify` wakes a waiter; it never blocks, and it is legal
    /// on the browser's main thread. Matching it would make the gate fire on
    /// the *correct* half of a lock.
    #[test]
    fn notify_is_not_a_wait() {
        let b = Build {
            funcs: vec![
                fun(ROOT, vec![0x41, 0x00, 0x41, 0x01, 0xfe, 0x00, 0x02, 0x00, 0x1a]),
                fun(PANIC_FMT, vec![0x01]),
            ],
            exports: vec![root_export(0)],
            ..Default::default()
        };
        assert!(b.check().unwrap().contains("clean"));
    }

    // ---- the panic exemption, both ways --------------------------------

    #[test]
    fn a_wait_only_behind_the_panic_handler_is_exempt() {
        let b = panic_shaped(call(1));
        let ok = b.check().unwrap();
        assert!(ok.contains("clean"), "{ok}");
        assert!(ok.contains("1 site(s) exempt on the fatal-path rule"), "{ok}");
    }

    /// The discrimination that makes the rule worth having: the same wait site,
    /// the same panic path, plus one ordinary path — and it goes red.
    #[test]
    fn the_same_wait_goes_red_when_an_ordinary_path_also_reaches_it() {
        let mut b = panic_shaped(call(1));
        // The root now also locks directly: 0 -> 3 -> 4.
        b.funcs[0].code = {
            let mut c = call(1);
            c.extend_from_slice(&call(3));
            c
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("futex_wait"), "{e}");
        assert!(e.contains("not panic paths"), "{e}");
        assert!(
            e.contains("        frustrate_check_block_0_tag_join\n"),
            "shows the witness from the root: {e}"
        );
    }

    /// The alloc-error hook is a fatal entry in its own right. It has to be:
    /// `std::alloc::rust_oom` calls it through an `AtomicPtr`, so nothing calls
    /// it directly and it is table-resident.
    #[test]
    fn the_alloc_error_hook_is_a_fatal_entry_too() {
        let b = Build {
            funcs: vec![
                fun(ROOT, call(1)),
                fun(PANIC_FMT, vec![0x01]),
                fun(ALLOC_HOOK, call(3)),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![root_export(0)],
            // Address-taken, exactly as the real one is.
            elements: vec![2],
            ..Default::default()
        };
        // Reached only through the hook, which the table also makes indirectly
        // callable: exempt, and the side condition still holds because the hook
        // is the cut rather than an interior node.
        let mut b = b;
        b.funcs[0].code = {
            let mut c = call(1);
            c.extend_from_slice(&call(2));
            c
        };
        let ok = b.check().unwrap();
        assert!(ok.contains("clean"), "{ok}");
    }

    /// A cut is an exemption, so matching too much is a *false green* — the one
    /// direction this tool must not fail in. A user function named
    /// `rust_begin_unwind` or `default_alloc_error_hook` is not std's, and a
    /// wait behind it is an ordinary finding.
    ///
    /// Mirror image of the bug `tools/check_no_park.dart` records, where
    /// `6Thread4park` matched the type instead of the free function and the
    /// scanner reported 111 crossings while missing the only silent one. There
    /// the miss was a false negative; here it would be a false clean.
    #[test]
    fn a_user_function_that_shares_a_fatal_entrys_name_is_not_one() {
        for lookalike in [
            "_RNvNtCs8uzoL3LKNOJ_13block_fixture3api17rust_begin_unwind",
            "_RNvNtCs8uzoL3LKNOJ_13block_fixture3api24default_alloc_error_hook",
            "_ZN13block_fixture3api17rust_begin_unwind17h0b1b2c3d4e5f6071E",
        ] {
            assert!(
                !FATAL_ENTRIES.iter().any(|f| f.matches(lookalike)),
                "{lookalike} was taken for a fatal entry"
            );
            let mut b = panic_shaped(call(1));
            b.funcs[2] = fun(lookalike, call(3));
            let e = b.check().unwrap_err();
            assert!(e.contains("futex_wait"), "not red for {lookalike}: {e}");
        }
        // And the real ones still are, under both mangling schemes plus the
        // bare `#[no_mangle]` spelling older toolchains used.
        for real in [
            BEGIN_UNWIND,
            "_ZN7___rustc17rust_begin_unwind17h0b1b2c3d4e5f6071E",
            "rust_begin_unwind",
            ALLOC_HOOK,
            "_ZN3std5alloc24default_alloc_error_hook17h0b1b2c3d4e5f6071E",
        ] {
            assert!(
                FATAL_ENTRIES.iter().any(|f| f.matches(real)),
                "{real} was not recognised, so every artifact would go red"
            );
        }
    }

    /// The soundness side condition. An interior node of the exempted region
    /// that is table-resident means a `call_indirect` could enter the region
    /// without passing the entry, so the exemption is refused rather than
    /// assumed.
    #[test]
    fn a_table_resident_function_inside_the_exempt_region_is_refused() {
        let mut b = panic_shaped(call(1));
        b.elements = vec![3]; // RwLock<Hook>::read, an interior node
        let e = b.check().unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
        assert!(e.contains("indirectly callable"), "{e}");
        assert!(e.contains("RwLock"), "names the leak: {e}");
    }

    /// The same region, clean: nothing in it is table-resident, so the direct
    /// edges are complete and the exemption stands. Pinned beside the failure
    /// above so the check is known to be discriminating rather than always red.
    #[test]
    fn an_unrelated_table_resident_function_does_not_break_the_exemption() {
        let mut b = panic_shaped(call(1));
        b.funcs.push(fun("_ZN8test_api3api7unused17h0b1b2c3d4e5f6071E", vec![0x01]));
        b.elements = vec![5];
        assert!(b.check().unwrap().contains("clean"));
    }

    /// A wait no `call` reaches is unexplained, not clean. Under lld's GC it
    /// should not exist, so its presence means something keeps it alive that
    /// this tool does not model.
    #[test]
    fn a_wait_no_call_path_reaches_is_an_error_not_a_pass() {
        let b = Build {
            funcs: vec![
                fun(ROOT, vec![0x01]),
                fun(PANIC_FMT, vec![0x01]),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![root_export(0)],
            elements: vec![2], // alive only through the table
            ..Default::default()
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
        assert!(e.contains("no `call` path"), "{e}");
    }

    // ---- the start function -------------------------------------------

    /// lld synthesises `__wasm_init_memory` as the start function under
    /// `--shared-memory`, and its wait is the passive-data-init barrier. The
    /// Bazel flow cannot subtract those link args, so the exemption is needed
    /// there; the cargo flow has no start section at all.
    #[test]
    fn the_start_functions_own_barrier_is_exempt() {
        let b = Build {
            funcs: vec![
                fun(ROOT, call(1)),
                fun(PANIC_FMT, vec![0x01]),
                fun("__wasm_init_memory", wait()),
            ],
            exports: vec![root_export(0)],
            start: Some(2),
            ..Default::default()
        };
        let ok = b.check().unwrap();
        assert!(ok.contains("1 on the start function"), "{ok}");
    }

    /// Only while it really is unreachable from code. A start function
    /// something also calls is an ordinary function with an ordinary wait.
    #[test]
    fn a_start_function_that_code_also_calls_is_not_exempt() {
        let b = Build {
            funcs: vec![
                fun(ROOT, call(2)),
                fun(PANIC_FMT, vec![0x01]),
                fun("__wasm_init_memory", wait()),
            ],
            exports: vec![root_export(0)],
            start: Some(2),
            ..Default::default()
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("__wasm_init_memory"), "{e}");
    }

    // ---- the controls --------------------------------------------------

    #[test]
    fn a_stripped_name_section_is_an_error_not_a_pass() {
        let b = Build {
            funcs: vec![fun(ROOT, vec![0x01])],
            exports: vec![root_export(0)],
            no_names: true,
            ..Default::default()
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
        assert!(e.contains("strip=symbols"), "{e}");
    }

    /// A name section carrying demangled or rewritten names reads as "cannot
    /// verify" too: the exemption is keyed on those symbols.
    #[test]
    fn names_without_mangled_rust_symbols_are_an_error_too() {
        let b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun("core::panicking::panic_fmt", vec![0x01])],
            exports: vec![root_export(0)],
            ..Default::default()
        };
        assert!(b.check().unwrap_err().contains("cannot verify"));
    }

    #[test]
    fn a_function_export_that_is_not_a_check_root_fails() {
        let b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0), ("frustrate_call_sync".into(), 0, 1)],
            ..Default::default()
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("frustrate_call_sync"), "{e}");
        assert!(e.contains("production module"), "{e}");
    }

    /// The exports a check artifact really does have besides its roots, all of
    /// them non-function (`memory`, `__data_end`, `__heap_base`).
    #[test]
    fn non_function_exports_are_allowed() {
        let b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![
                ("memory".into(), 2, 0),
                root_export(0),
                ("__data_end".into(), 3, 0),
                ("__heap_base".into(), 3, 1),
            ],
            ..Default::default()
        };
        assert!(b.check().unwrap().contains("clean"));
    }

    /// `__wasm_init_tls` is a *function* export, so unlike the three above it
    /// would trip the stray check — and the Bazel flow cannot remove it: the
    /// atomics toolchain forces it via `--export=__wasm_init_tls` and a
    /// downstream target cannot subtract a link arg.
    #[test]
    fn the_linkers_own_tls_export_is_not_a_stray() {
        let b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0), ("__wasm_init_tls".into(), 0, 1)],
            ..Default::default()
        };
        assert!(b.check().unwrap().contains("clean"));
    }

    /// Exempting it must not make it a *root*. It is nobody's claimed body, so
    /// a wait it reached would be live with no path from a root, the start
    /// function or a fatal entry — rule 2, a hard error rather than a pass.
    /// Without the `roots` half of the exemption this would report clean.
    #[test]
    fn the_tls_export_does_not_launder_a_wait_into_the_answer() {
        let b = Build {
            // fn 1 is the TLS export and calls fn 2, which waits.
            funcs: vec![
                fun(ROOT, vec![0x01]),
                fun("__wasm_init_tls", vec![0x10, 0x02]), // call 2
                fun("waiter", wait()),
                fun(PANIC_FMT, vec![0x01]), // the name-section sentinel
            ],
            exports: vec![root_export(0), ("__wasm_init_tls".into(), 0, 1)],
            ..Default::default()
        };
        let e = b.check().unwrap_err();
        assert!(
            e.contains("no call path") || e.contains("unexplained"),
            "a wait reached only from the linker's TLS export must be refused, \
             not counted as clean: {e}"
        );
    }

    #[test]
    fn zero_roots_is_an_error_because_a_gate_over_nothing_gates_nothing() {
        let b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![("memory".into(), 2, 0)],
            ..Default::default()
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("covers nothing"), "{e}");
        assert!(e.contains("frustrate_block_check"), "{e}");
    }

    // ---- the decoder ---------------------------------------------------

    /// Imported functions come first in the index space, so a `call 2` means a
    /// different function depending on how many imports there are. A shift here
    /// would silently point every edge at the wrong node.
    #[test]
    fn an_import_shifts_the_index_space() {
        // Two imports, so the defined functions are indices 2..=5:
        // 2 root, 3 panic_fmt, 4 rust_begin_unwind, 5 futex_wait.
        let mut b = Build {
            imports: vec![("frustrate", "post"), ("frustrate", "panic")],
            funcs: vec![
                fun(ROOT, call(3)),
                fun(PANIC_FMT, call(4)),
                fun(BEGIN_UNWIND, call(5)),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![root_export(2)],
            ..Default::default()
        };
        // Through the panic funnel only: exempt.
        assert!(b.check().unwrap().contains("clean"));

        // Straight at the waiter: red. If the imports were not counted, `call 5`
        // would resolve to no defined function and the wait would read as
        // unexplained instead — a different message, and a wrong one.
        b.funcs[0].code = {
            let mut c = call(3);
            c.extend_from_slice(&call(5));
            c
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("futex_wait"), "{e}");
        assert!(e.contains("not panic paths"), "read as a finding, not as a parse failure: {e}");
    }

    #[test]
    fn an_unknown_opcode_is_an_error_never_a_pass() {
        for op in [
            vec![0x27],
            vec![0xfe, 0x60],
            vec![0xfc, 0x40],
            vec![0xfd, 0xff, 0x7f],
        ] {
            let b = Build {
                funcs: vec![fun(ROOT, op), fun(PANIC_FMT, vec![0x01])],
                exports: vec![root_export(0)],
                ..Default::default()
            };
            let e = b.check().unwrap_err();
            assert!(e.contains("cannot decode this wasm module"), "{e}");
        }
    }

    /// SIMD and exception-handling instructions are decoded like any other:
    /// a `v128.const` whose bytes spell a wait32 is not a wait, and a call
    /// inside a `try_table` is still an edge.
    #[test]
    fn simd_and_exception_handling_are_scanned() {
        let mut simd = vec![0xfd, 0x0c]; // v128.const
        simd.extend_from_slice(&[0xfe, 0x01, 0x02, 0x00].repeat(4));
        simd.push(0x1a); // drop
        assert!(panic_shaped(simd.clone())
            .check()
            .unwrap()
            .contains("clean"));

        simd.extend_from_slice(&call(4));
        let e = panic_shaped(simd).check().unwrap_err();
        assert!(
            e.contains("futex_wait") && e.contains("not panic paths"),
            "{e}"
        );

        // try_table (empty blocktype, no catch clauses) call 4 end
        let mut eh = vec![0x1f, 0x40, 0x00];
        eh.extend_from_slice(&call(4));
        eh.push(0x0b);
        let e = panic_shaped(eh).check().unwrap_err();
        assert!(
            e.contains("futex_wait") && e.contains("not panic paths"),
            "{e}"
        );
    }

    /// A body whose declared size disagrees with its instructions is reported
    /// rather than absorbed.
    #[test]
    fn a_body_that_does_not_end_where_it_says_is_an_error() {
        let mut b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0)],
            ..Default::default()
        }
        .wasm();
        // Grow the first code entry's declared size by one without giving it
        // another byte of body.
        let at = b
            .windows(2)
            .position(|w| w == [0x0a, 0x09])
            .expect("code section header");
        b[at + 2] += 1;
        let dir = std::env::temp_dir().join(format!("wasm_block_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("desync.wasm");
        std::fs::write(&p, &b).unwrap();
        let e = run(p.to_str().unwrap(), "//pkg:x").unwrap_err();
        assert!(e.contains("cannot decode this wasm module"), "{e}");
    }

    #[test]
    fn a_truncated_section_is_an_error() {
        let mut b = Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0)],
            ..Default::default()
        }
        .wasm();
        b.truncate(b.len() - 12);
        let dir = std::env::temp_dir().join(format!("wasm_block_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("short.wasm");
        std::fs::write(&p, &b).unwrap();
        let e = run(p.to_str().unwrap(), "//pkg:x").unwrap_err();
        assert!(e.contains("cannot decode this wasm module"), "{e}");
    }

    #[test]
    fn a_file_that_is_not_wasm_is_rejected() {
        let dir = std::env::temp_dir().join(format!("wasm_block_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("junk.wasm");
        std::fs::write(&p, b"not a wasm file at all").unwrap();
        assert!(run(p.to_str().unwrap(), "//pkg:x")
            .unwrap_err()
            .contains("not a wasm module"));
    }

    /// `return_call` is a call. rustc does not emit it without `+tail-call`,
    /// but a missed edge would be a silent under-approximation.
    #[test]
    fn a_return_call_is_a_call_edge() {
        let mut b = panic_shaped(call(1));
        b.funcs[0].code = {
            let mut c = call(1);
            c.push(0x12); // return_call
            u(3, &mut c);
            c
        };
        assert!(b.check().unwrap_err().contains("futex_wait"));
    }

    /// A `ref.func` is an address-take, so it makes a function indirectly
    /// callable even with no element segment naming it.
    #[test]
    fn a_ref_func_makes_a_function_table_resident() {
        let mut b = panic_shaped(call(1));
        b.funcs[0].code = {
            let mut c = call(1);
            c.push(0xd2); // ref.func RwLock<Hook>::read
            u(3, &mut c);
            c.push(0x1a); // drop
            c
        };
        let e = b.check().unwrap_err();
        assert!(e.contains("indirectly callable"), "{e}");
    }

    // ---- the report ---------------------------------------------------

    /// `iroh_rust::frustrate_generated::actor_2_connect`.
    const GLUE: &str = "_ZN9iroh_rust19frustrate_generated15actor_2_connect17h0123456789abcdefE";
    /// A closure *inside* the dispatcher. Carries `frustrate_generated` too,
    /// which is why [`is_member_glue`] is structural rather than a substring.
    const GLUE_CLOSURE: &str =
        "_ZN9iroh_rust19frustrate_generated19frustrate_call_sync7closure17h0123456789abcdefE";

    #[test]
    fn a_dispatch_closure_is_not_a_member_entry() {
        assert!(is_member_glue(&readable(GLUE)));
        assert!(!is_member_glue(&readable(GLUE_CLOSURE)));
        assert!(!is_member_glue("frustrate_call_sync"));
    }

    #[test]
    fn the_export_classes_are_read_off_the_raw_symbol() {
        assert!(matches!(classify_export(ROOT), ExportKind::CheckRoot));
        assert!(matches!(classify_export("frustrate_call_sync"), ExportKind::Dispatch));
        assert!(matches!(
            classify_export("__wbindgen_describe_intounderlyingsource_pull"),
            ExportKind::Describe
        ));
        assert!(matches!(classify_export("__wbindgen_malloc"), ExportKind::Intrinsic));
        assert!(matches!(classify_export("__externref_table_dealloc"), ExportKind::Intrinsic));
        assert!(matches!(classify_export("intounderlyingsource_pull"), ExportKind::Bindgen));
        assert!(matches!(classify_export("__wbg_intounderlyingsink_free"), ExportKind::Bindgen));
        assert!(matches!(classify_export("__wasm_init_tls"), ExportKind::Linker));
        assert!(matches!(
            classify_export("ring_core_0_17_14__bn_mul_mont"),
            ExportKind::Other
        ));
    }

    /// The report's core claim: *which* export keeps a wait alive, and which
    /// member's entry is on the path. The describe shim here reaches nothing,
    /// which is the whole point of separating the buckets.
    #[test]
    fn the_report_names_the_export_and_the_member_that_reach_a_wait() {
        let b = Build {
            funcs: vec![
                fun("frustrate_call_sync", call(2)),
                fun("__wbindgen_describe_foo", call(3)),
                fun(GLUE, call(4)),
                fun(PANIC_FMT, Vec::new()),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![
                ("frustrate_call_sync".into(), 0, 0),
                ("__wbindgen_describe_foo".into(), 0, 1),
            ],
            ..Default::default()
        };
        let r = b.report().unwrap();
        assert!(r.contains("frustrate dispatch 1, bindgen describe 1"), "{r}");
        assert!(r.contains("kept alive by 1 export(s): frustrate dispatch 1"), "{r}");
        assert!(r.contains("bridged member entries in that ancestry (1)"), "{r}");
        assert!(
            r.contains("via member entry iroh_rust::frustrate_generated::actor_2_connect"),
            "{r}"
        );
        // The describe shim is an export, and it reaches nothing.
        assert!(!r.contains("bindgen describe 1\n      bridged"), "{r}");
    }

    /// The two ancestries have to be printed separately or the report says
    /// nothing: every Rust function reaches the panic locks, so the *full*
    /// ancestry names the root while the *ordinary* one is empty. Conflating
    /// them would make a clean module look reachable from everything.
    #[test]
    fn a_panic_only_wait_is_ordinary_free_but_still_has_a_full_ancestry() {
        let r = panic_shaped(call(1)).report().unwrap();
        assert!(
            r.contains("kept alive by no export over ordinary edges"),
            "{r}"
        );
        assert!(r.contains("full ancestry"), "{r}");
        assert!(r.contains("kept alive by 1 export(s): check root 1"), "{r}");
        // And the tree shows where the ordinary graph stops.
        assert!(r.contains("[fatal entry — the ordinary graph stops here]"), "{r}");
        assert!(r.contains("1 site(s) with no export in their ordinary ancestry"), "{r}");
    }

    /// Table-residency is the side condition's input, so the report has to
    /// surface it per site rather than leave a reader to infer it.
    #[test]
    fn the_report_surfaces_table_residency() {
        let mut b = panic_shaped(call(1));
        b.elements = vec![4]; // futex_wait itself
        let r = b.report().unwrap();
        assert!(r.contains("table-resident: yes"), "{r}");
        assert!(r.contains("an indirect call can enter it"), "{r}");
    }

    /// `--report` is additive: it inventories exactly the modules the gate
    /// refuses to reason about, so it must not inherit the gate's controls.
    #[test]
    fn the_report_accepts_a_module_the_gate_rejects() {
        let b = Build {
            funcs: vec![fun(PANIC_FMT, call(1)), fun(FUTEX_WAIT, wait())],
            exports: vec![("ring_core_0_17_14__bn_mul_mont".into(), 0, 0)],
            ..Default::default()
        };
        let gate = b.check().unwrap_err();
        assert!(gate.contains("that are not `frustrate_check_block_*`"), "{gate}");
        let r = b.report().unwrap();
        assert!(r.contains("1 wait site(s)"), "{r}");
        assert!(r.contains("kept alive by 1 export(s): other 1"), "{r}");
    }

    /// A module with no roots at all is the gate's "gates nothing" error and
    /// the report's ordinary input.
    #[test]
    fn the_report_says_when_nothing_reaches_a_wait() {
        let b = Build {
            funcs: vec![fun(PANIC_FMT, Vec::new()), fun(FUTEX_WAIT, wait())],
            ..Default::default()
        };
        assert!(b.check().unwrap_err().contains("covers nothing"), "gate");
        let r = b.report().unwrap();
        assert!(r.contains("no export reaches it at all"), "{r}");
    }

    // ---- the claim census ---------------------------------------------

    /// A clean module whose only claim is the one root it exports. The shape
    /// every census test below varies from.
    fn clean() -> Build {
        Build {
            funcs: vec![fun(ROOT, vec![0x01]), fun(PANIC_FMT, vec![0x01])],
            exports: vec![root_export(0)],
            ..Default::default()
        }
    }

    #[test]
    fn a_census_that_cannot_be_read_is_never_read_as_no_claims() {
        let bad = Claims::parse("artifact 0 tag_join m::tag_join", "x.claims").unwrap_err();
        assert!(bad.contains("is not a claim census"), "{bad}");
        let older = Claims::parse("frustrate-claim-census 5", "x.claims").unwrap_err();
        assert!(older.contains("older than this scanner"), "{older}");
        let newer = Claims::parse("frustrate-claim-census 7", "x.claims").unwrap_err();
        assert!(newer.contains("older than this scanner"), "{newer}");
        let torn = census("artifact tag_join").unwrap_err();
        assert!(torn.contains("<settlement> <fn_id> <name>"), "{torn}");
        let unnumbered = census("artifact x tag_join").unwrap_err();
        assert!(unnumbered.contains("no numeric fn id"), "{unnumbered}");
        let pathless = census("artifact 0 tag_join").unwrap_err();
        assert!(pathless.contains("carries the member's function path"), "{pathless}");
        let future = census("proven 0 tag_join").unwrap_err();
        assert!(future.contains("is not a settlement this tool knows"), "{future}");
    }

    /// Comments, the crate line and blank lines are structure, not rows.
    #[test]
    fn a_census_carries_the_crate_name_and_ignores_comments() {
        let c = census("\n# a comment\nplacement 3 Node::send\n").unwrap();
        assert_eq!(c.crate_name, "fixture");
        assert_eq!(c.placement, vec!["Node::send".to_string()]);
        assert!(c.who().contains("fixture"), "{}", c.who());
    }

    /// The point of the census: a claim whose root is gone is named, where a
    /// bare module can only notice when *every* root is gone.
    #[test]
    fn a_claim_whose_root_is_missing_is_named() {
        let e = clean()
            .check_claims("artifact 0 tag_join m::tag_join\nartifact 7 withdraw m::withdraw")
            .unwrap_err();
        assert!(e.contains("withdraw — expected `frustrate_check_block_7_withdraw`"), "{e}");
        assert!(e.contains("never reached rustc"), "{e}");
        // The claim that IS rooted must not be reported as missing.
        assert!(!e.contains("tag_join —"), "{e}");
    }

    /// The other direction, which is what a stale artifact looks like: a root
    /// for a member the census no longer claims (an actor root emitted by
    /// codegen from before placement settled it, say).
    #[test]
    fn a_root_the_census_does_not_claim_is_named() {
        let b = Build {
            exports: vec![
                root_export(0),
                ("frustrate_check_block_4_elsewhere".into(), 0, 0),
            ],
            ..clean()
        };
        let e = b.check_claims("artifact 0 tag_join m::tag_join").unwrap_err();
        assert!(e.contains("does not claim"), "{e}");
        assert!(e.contains("frustrate_check_block_4_elsewhere"), "{e}");
    }

    /// A qualified member name resolves to the root symbol's bare tail —
    /// `Node::send` is exported as `frustrate_check_block_N_send`.
    #[test]
    fn a_qualified_name_matches_its_bare_root_symbol() {
        let c = census("artifact 0 Tag::tag_join m::Tag::tag_join").unwrap();
        assert_eq!(c.artifact[0].0, ROOT);
        let ok = clean().check_claims("artifact 0 Tag::tag_join m::Tag::tag_join").unwrap();
        assert!(ok.contains("is clean"), "{ok}");
    }

    /// The mixed report — both halves, in one line. This is what a claim set
    /// that is partly placed and partly proven must say.
    #[test]
    fn a_mixed_claim_set_reports_both_halves() {
        let r = clean()
            .check_claims("artifact 0 tag_join m::tag_join\nplacement 5 Node::send\nplacement 6 Node::ticket")
            .unwrap();
        assert!(r.contains("1 `no_block` root(s)"), "{r}");
        assert!(r.contains("2 settled by placement"), "{r}");
    }

    /// A native-only claim is settled on web by absence rather than by an
    /// artifact, so it is counted and named — "green because there is nothing
    /// there" is a different fact from "green because it was checked".
    #[test]
    fn a_declared_only_claim_is_counted_not_hidden() {
        let r = clean()
            .check_claims("artifact 0 tag_join m::tag_join\ndeclared 9 read_file")
            .unwrap();
        assert!(r.contains("1 native-only, so absent from the web surface"), "{r}");
    }

    /// Three silences that must not read alike. Nothing to prove, nothing
    /// claimed, and nothing but native-only claims.
    #[test]
    fn the_empty_states_report_differently() {
        // Nothing to prove: green without a module, red with one.
        let placed = census("placement 5 Node::send").unwrap();
        let ok = claims_only("//pkg:demo", &placed).unwrap();
        assert!(ok.contains("none needing an artifact"), "{ok}");
        assert!(ok.contains("1 settled by placement"), "{ok}");
        let pointless = clean().check_claims("placement 5 Node::send").unwrap_err();
        assert!(pointless.contains("can prove nothing"), "{pointless}");

        // No claims at all — in both modes, and distinct from the above.
        let none = census("").unwrap();
        let e = claims_only("//pkg:demo", &none).unwrap_err();
        assert!(e.contains("claims nothing this check can settle"), "{e}");
        assert!(e.contains("Claim a member"), "{e}");
        assert!(clean().check_claims("").unwrap_err().contains("claims nothing"), "gate");

        // Only native-only claims: there are claims, and none of them is
        // settleable here. Its own wording, not "claim a member".
        let d = census("declared 9 read_file").unwrap();
        let e = claims_only("//pkg:demo", &d).unwrap_err();
        assert!(e.contains("Every claim here is native-only"), "{e}");
    }

    /// The loud failure a claims-only project gets when it adds a claim that
    /// needs proving — the whole reason the mode is a declaration rather than
    /// an inference.
    #[test]
    fn a_claims_only_target_refuses_a_claim_that_needs_an_artifact() {
        let c = census("placement 5 Node::send\nartifact 0 tag_join m::tag_join").unwrap();
        let e = claims_only("//pkg:demo", &c).unwrap_err();
        assert!(e.contains("need artifact evidence"), "{e}");
        assert!(e.contains("  tag_join"), "{e}");
        assert!(e.contains("crate = "), "{e}");
    }

    /// Dispatch placement needs no artifact, exactly like actor placement —
    /// but it must not be *reported* like it. The two greens rest on different
    /// arguments (an actor's holds on every configuration; a dispatched
    /// member's has one leg for the configurations where the wait instruction
    /// exists and another for the ones where it does not), and a reader who
    /// cannot tell them apart cannot check either.
    #[test]
    fn dispatch_placement_settles_without_an_artifact_and_says_so_apart() {
        let c = census("placement 5 Node::send\nplacement-dispatch 7 find_snapshot").unwrap();
        let ok = claims_only("//pkg:demo", &c).unwrap();
        assert!(ok.contains("2 `no_block` claim(s), none needing an artifact"), "{ok}");
        assert!(ok.contains("1 settled by placement (an actor member"), "{ok}");
        assert!(ok.contains("1 settled by dispatch placement"), "{ok}");
        // On its own it is still a claim set worth having, not an empty one.
        let alone = census("placement-dispatch 7 find_snapshot").unwrap();
        assert!(claims_only("//pkg:demo", &alone).is_ok());
        // And it adds nothing an artifact could hold, so building one is the
        // same error a pure actor claim set gets.
        let pointless = clean().check_claims("placement-dispatch 7 f").unwrap_err();
        assert!(pointless.contains("can prove nothing"), "{pointless}");
    }

    /// A residue root is matched against the module's exports exactly like a
    /// full one — both directions — because the control is about the export
    /// set, which the two kinds share. What differs is the sentence it earns.
    #[test]
    fn a_residue_claim_is_rooted_like_an_artifact_claim_and_reported_apart() {
        let ok = clean().check_claims("artifact-residue 0 Tag::tag_join").unwrap();
        assert!(ok.contains("1 `no_block` root(s)"), "{ok}");
        assert!(ok.contains("dispatched with a caller-side residue"), "{ok}");
        // Missing in the artifact: named, like any other claimed root.
        let e = clean()
            .check_claims("artifact-residue 0 tag_join\nartifact-residue 7 take")
            .unwrap_err();
        assert!(e.contains("take — expected `frustrate_check_block_7_take`"), "{e}");
        // A claims-only target cannot carry one: there is something to scan.
        let c = census("artifact-residue 0 tag_join").unwrap();
        let refused = claims_only("//pkg:demo", &c).unwrap_err();
        assert!(refused.contains("need artifact evidence"), "{refused}");
        assert!(refused.contains("whose `from_bytes` \
             runs on the caller"), "{refused}");
    }

    /// A census does not weaken the predicate: a reachable wait is still red,
    /// with its witness.
    #[test]
    fn a_census_does_not_soften_a_finding() {
        let b = Build {
            funcs: vec![
                fun(ROOT, call(2)),
                fun(PANIC_FMT, vec![0x01]),
                fun(FUTEX_WAIT, wait()),
            ],
            exports: vec![root_export(0)],
            ..Default::default()
        };
        let e = b.check_claims("artifact 0 tag_join m::tag_join\nplacement 5 Node::send").unwrap_err();
        assert!(e.contains("1 `#[bridge(no_block)]` member(s) reach a wait"), "{e}");
        // The census names the member; the path still starts at its root.
        assert!(e.contains("  tag_join\n      reaches "), "{e}");
        assert!(e.contains("        frustrate_check_block_0_tag_join\n"), "{e}");
    }

    // ---- slow paths ---------------------------------------------------

    const SLOW_MARK: &str = "_RNvNtCsgIThcpvLFmw_7example11main_thread4slow";
    const JUDGE: &str = "_RNvNtCsgIThcpvLFmw_7example6access5judge";

    #[test]
    fn own_path_is_the_items_path_in_both_manglings() {
        let p = |s: &str| own_path(s).join("::");
        assert_eq!(p(SLOW_MARK), "example::main_thread::slow");
        // v0 inherent impl, instantiated in another crate: the type's path,
        // without the instantiating crate.
        assert_eq!(
            p("_RNvMs5_NtCs6jEIwG9Ob0J_5alloc7raw_vecNtB5_11RawVecInner14grow_amortizedCs6uIOFUfKMfT_9frustrate"),
            "alloc::raw_vec::RawVecInner::grow_amortized"
        );
        // v0 generic instance: the function, without its type arguments.
        assert_eq!(p("_RINvNtCs1_4mine3api7processNtNtCs2_3std2fs4FileE"), "mine::api::process");
        // Legacy trait impl and generic type, by the type's path.
        assert_eq!(
            p("_ZN47_$LT$std..fs..File$u20$as$u20$std..io..Read$GT$4read17h0123456789abcdefE"),
            "std::fs::File::read"
        );
        assert_eq!(p("_ZN5alloc3vec16Vec$LT$T$C$A$GT$4push17h0123456789abcdefE"), "alloc::vec::Vec::push");
    }

    #[test]
    fn a_slow_path_matches_whole_leading_segments() {
        let fs = SlowPath::parse("std::fs").unwrap();
        let seg = |s: &str| s.split("::").map(str::to_string).collect::<Vec<_>>();
        assert!(fs.matches(&seg("std::fs::read")));
        assert!(fs.matches(&seg("std::fs")));
        assert!(!fs.matches(&seg("std::fsx::read")));
        assert!(!fs.matches(&seg("mine::std::fs::read")));
        assert!(!fs.matches(&seg("std")));
    }

    #[test]
    fn a_slow_path_that_cannot_name_a_symbol_is_refused() {
        for bad in ["crate::io::save", "self::x", "::std::fs", "std::fs::read<T>", "std::", ""] {
            assert!(SlowPath::parse(bad).is_err(), "{bad}");
        }
    }

    /// Two members, one of which reaches the slow marker through ordinary
    /// code. Indices: 0 root a, 1 root b, 2 judge, 3 slow, 4 panic_fmt.
    fn two_members(a_code: Vec<u8>) -> Build {
        Build {
            funcs: vec![
                fun("frustrate_check_block_1_render", a_code),
                fun("frustrate_check_block_2_title", call(4)),
                fun(JUDGE, call(3)),
                fun(SLOW_MARK, vec![0x01]),
                fun(PANIC_FMT, vec![0x01]),
            ],
            exports: vec![
                ("frustrate_check_block_1_render".into(), 0, 0),
                ("frustrate_check_block_2_title".into(), 0, 1),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn a_member_that_reaches_a_slow_function_is_named_with_its_path() {
        let e = two_members(call(2))
            .check_slow(
                &["example::main_thread::slow"],
                Some("artifact 1 Doc::render m::Doc::render\nartifact 2 Doc::title m::Doc::title"),
            )
            .unwrap_err();
        assert!(e.contains("1 `#[bridge(no_block)]` member(s) reach a wait instruction or a function named slow"), "{e}");
        assert!(e.contains("  Doc::render\n      reaches example::main_thread::slow — named slow by `example::main_thread::slow`"), "{e}");
        assert!(e.contains("example::access::judge"), "the path runs through the caller: {e}");
        assert!(!e.contains("Doc::title"), "the clean member is not listed: {e}");
    }

    #[test]
    fn a_module_prefix_names_everything_under_it() {
        let e = two_members(call(2)).check_slow(&["example::access"], None).unwrap_err();
        assert!(e.contains("reaches example::access::judge — named slow by `example::access`"), "{e}");
    }

    #[test]
    fn a_slow_function_no_member_reaches_is_absent_and_clean() {
        // The marker exists only because the panic funnel calls it.
        let mut b = two_members(call(4));
        b.funcs[4].code = call(3);
        b.funcs.push(fun(BEGIN_UNWIND, call(3)));
        b.funcs[4].code = call(5);
        let ok = b.check_slow(&["example::main_thread::slow"], None).unwrap();
        assert!(ok.contains("clean"), "{ok}");
        assert!(ok.contains("checked against 1 slow name(s) (`example::main_thread::slow`)"), "{ok}");
        assert!(ok.contains("1 site(s) exempt on the fatal-path rule"), "{ok}");
    }

    #[test]
    fn without_slow_paths_a_slow_function_is_ordinary_code() {
        let ok = two_members(call(2)).check_slow(&[], None).unwrap();
        assert!(ok.contains("clean"), "{ok}");
    }

    #[test]
    fn a_slow_function_reached_only_indirectly_is_refused_with_the_entry() {
        // judge is table-resident and nothing calls it: a `dyn` method.
        let mut b = two_members(vec![0x01]);
        b.elements = vec![2];
        let e = b.check_slow(&["example::main_thread::slow"], None).unwrap_err();
        assert!(e.contains("cannot verify"), "{e}");
        assert!(e.contains("example::main_thread::slow` (named slow by"), "{e}");
        assert!(e.contains("Indirectly callable functions that call down to it"), "{e}");
        assert!(e.contains("  example::access::judge"), "{e}");
    }

    #[test]
    fn a_wait_and_a_slow_function_are_both_findings() {
        let mut b = two_members(call(2));
        b.funcs[1].code = call(5);
        b.funcs.push(fun(FUTEX_WAIT, wait()));
        let e = b.check_slow(&["example::main_thread::slow"], None).unwrap_err();
        assert!(e.contains("2 `#[bridge(no_block)]` member(s)"), "{e}");
        assert!(e.contains("reaches std::sys::pal::wasm::futex::futex_wait — memory.atomic.wait32"), "{e}");
    }

    // ---- slow imports and modules without atomics ----------------------

    /// A wasip1-shaped module: one import, no atomics. Indices: 0 the
    /// import, 1 root, 2 a std file open, 3 panic_fmt.
    fn wasi_shaped(root_code: Vec<u8>) -> Build {
        Build {
            imports: vec![("wasi_snapshot_preview1", "path_open")],
            funcs: vec![
                fun(ROOT, root_code),
                fun("_ZN3std3sys2fs4unix4File4open17h0123456789abcdefE", call(0)),
                fun(PANIC_FMT, vec![0x01]),
            ],
            exports: vec![(ROOT.to_string(), 0, 1)],
            features: Some(&["bulk-memory", "mutable-globals"]),
            ..Default::default()
        }
    }

    #[test]
    fn a_member_that_calls_a_slow_import_is_named_with_its_path() {
        let e = wasi_shaped(call(2))
            .check_slow(&["files"], None)
            .unwrap_err();
        assert!(e.contains("1 `#[bridge(no_block)]` member(s) reach a function named slow."), "{e}");
        assert!(e.contains("imports `wasi_snapshot_preview1.path_open`, IO `files`"), "{e}");
        assert!(e.contains("std::sys::fs::unix::File::open"), "the path runs through std: {e}");
    }

    #[test]
    fn a_module_without_atomics_says_it_checked_no_waits() {
        // The root reaches nothing; the import is kept by the file open alone,
        // which the panic funnel reaches.
        let mut b = wasi_shaped(call(3));
        b.funcs[2].code = call(2);
        b.funcs.push(fun(BEGIN_UNWIND, call(2)));
        b.funcs[2].code = call(4);
        let ok = b.check_slow(&["files"], None).unwrap();
        assert!(ok.contains("clean"), "{ok}");
        assert!(ok.contains("built without `+atomics`, so it can hold no wait instruction"), "{ok}");
    }

    #[test]
    fn a_module_without_atomics_and_without_slow_names_is_refused() {
        let e = wasi_shaped(vec![0x01]).check_slow(&[], None).unwrap_err();
        assert!(e.contains("covers nothing"), "{e}");
    }

    #[test]
    fn on_wasip1_poll_oneoff_is_a_wait_with_nothing_named() {
        let mut b = wasi_shaped(call(2));
        b.imports = vec![("wasi_snapshot_preview1", "poll_oneoff")];
        let e = b.check_slow(&["wasi"], None).unwrap_err();
        assert!(e.contains("reach a wait or a function named slow"), "{e}");
        assert!(e.contains("imports `wasi_snapshot_preview1.poll_oneoff`, which waits"), "{e}");
        // Without the import, the same module is clean, and says what it checked.
        let ok = wasi_shaped(vec![0x01]).check_slow(&["wasi"], None).unwrap();
        assert!(ok.contains("its waits are `poll_oneoff`"), "{ok}");
    }

    #[test]
    fn a_module_with_atomics_keeps_the_wait_check() {
        let mut b = clean();
        b.features = Some(&["atomics", "bulk-memory"]);
        let ok = b.check().unwrap();
        assert!(ok.contains("clean") && !ok.contains("without `+atomics`"), "{ok}");
    }

    #[test]
    fn a_root_that_no_longer_reaches_its_glue_is_refused() {
        // `clean()` without `with_glue`: the root calls nothing, as an
        // optimizer leaves it once it has proved the decode fails.
        let dir = std::env::temp_dir().join(format!("wasm_block_folded_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.wasm");
        std::fs::write(&p, clean().wasm()).unwrap();
        let c = census("artifact 0 tag_join m::tag_join").unwrap();
        let e = run_with(p.to_str().unwrap(), "//pkg:t", Some(&c), &Sinks::default()).unwrap_err();
        assert!(e.contains("no longer reach their member's glue"), "{e}");
        assert!(e.contains("  tag_join"), "{e}");
    }

    // ---- silences ------------------------------------------------------

    const MARKER: &str = "_RNvCs1_9frustrate16___no_block_allow";
    const WRAP: &str = "_RNvCs1_7fixture4wrap";

    /// A root that reaches the wait through `wrap`, which calls the marker
    /// first when `marked`, and directly too when `around`.
    fn silenced(marked: bool, around: bool) -> Build {
        let mut wrap = if marked { call(2) } else { Vec::new() };
        wrap.extend(call(3));
        let mut root = call(1);
        if around {
            root.extend(call(3));
        }
        Build {
            funcs: vec![
                fun(ROOT, root),
                fun(WRAP, wrap),
                fun(MARKER, vec![0x01]),
                fun(FUTEX_WAIT, wait()),
                fun(PANIC_FMT, vec![0x01]),
            ],
            exports: vec![root_export(0)],
            ..Default::default()
        }
    }

    /// The scan with a census and the check target's `allow`.
    fn check_allowing(b: &Build, rows: &str, allowed: &[&str]) -> Result<Green, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("wasm_block_silences_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("m{}.wasm", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, b.with_glue().wasm()).unwrap();
        let c = census(rows)?;
        let sinks = Sinks {
            allowed: allowed.iter().map(|a| a.to_string()).collect(),
            ..Sinks::default()
        };
        run_with(p.to_str().unwrap(), "//pkg:target.wasm", Some(&c), &sinks)
    }

    const ROW: &str = "artifact 0 tag_join m::tag_join\n";

    /// A finding says what silences it: the path of the function holding the
    /// wait.
    #[test]
    fn a_wait_names_what_silences_it() {
        let e = check_allowing(&silenced(false, false), ROW, &[]).unwrap_err();
        assert!(e.contains("(silence it as `std::sys::pal::wasm::futex::futex_wait`)"), "{e}");
    }

    #[test]
    fn a_member_silence_is_applied_and_recorded() {
        let rows = format!("{ROW}allow 0 std::sys::pal::wasm::futex::futex_wait\n");
        let g = check_allowing(&silenced(false, false), &rows, &[]).unwrap();
        assert!(g.text.contains("`std::sys::pal::wasm::futex::futex_wait` in tag_join"), "{}", g.text);
        assert!(g.applied.silences.contains(&silences::Silence::Member(
            "0".into(),
            "std::sys::pal::wasm::futex::futex_wait".into()
        )));
    }

    #[test]
    fn a_target_silence_is_applied_and_recorded() {
        let g = check_allowing(&silenced(false, false), ROW, &["std::sys::pal::wasm::futex::futex_wait"]).unwrap();
        assert!(g.text.contains("in every claim (the check target's `allow`)"), "{}", g.text);
        assert!(g
            .applied
            .silences
            .contains(&silences::Silence::Target("std::sys::pal::wasm::futex::futex_wait".into())));
    }

    /// A silence that matches nothing here is not an error here: another
    /// analysis may apply it, which `--silences` settles.
    #[test]
    fn a_silence_that_matches_nothing_here_is_not_applied() {
        let rows = format!("{ROW}allow 0 pthread_mutex_lock\n");
        let g = check_allowing(&clean(), &rows, &["sqlite3_step"]).unwrap();
        assert!(g.applied.silences.is_empty(), "{:?}", g.applied);
    }

    #[test]
    fn a_marked_function_is_a_stop_point() {
        let g = check_allowing(&silenced(true, false), ROW, &[]).unwrap();
        assert!(g.text.contains("`fixture::wrap` in every claim (`#[frustrate::no_block(allow)]`)"), "{}", g.text);
        assert!(g.applied.silences.contains(&silences::Silence::Mark("fixture::wrap".into())));
        // The same wait reached around the marked function is still a finding.
        let e = check_allowing(&silenced(true, true), ROW, &[]).unwrap_err();
        assert!(e.contains("futex_wait"), "{e}");
    }

    #[test]
    fn a_marked_function_with_nothing_under_it_is_idle() {
        let mut b = silenced(true, false);
        b.funcs[3].code = vec![0x01];
        let g = check_allowing(&b, ROW, &[]).unwrap();
        assert!(g.applied.silences.is_empty(), "{:?}", g.applied);
        assert!(g.applied.idle_marks.contains("fixture::wrap"), "{:?}", g.applied);
    }

    /// An import is silenced by its field, as a C function is natively.
    #[test]
    fn an_import_is_silenced_by_its_field() {
        let b = Build {
            imports: vec![("wasi_snapshot_preview1", "poll_oneoff")],
            funcs: vec![fun(ROOT, call(0)), fun(PANIC_FMT, vec![0x01])],
            exports: vec![(ROOT.to_string(), 0, 1)],
            ..Default::default()
        };
        let mut c = b.clone();
        c.funcs[0].code = call(0);
        let dir = std::env::temp_dir().join(format!("wasm_block_import_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("m.wasm");
        std::fs::write(&p, c.with_glue().wasm()).unwrap();
        let sinks = |allowed: &[&str]| Sinks {
            wasi: true,
            allowed: allowed.iter().map(|a| a.to_string()).collect(),
            ..Sinks::default()
        };
        let e = run_with(p.to_str().unwrap(), "//pkg:t", None, &sinks(&[])).unwrap_err();
        assert!(e.contains("(silence it as `poll_oneoff`)"), "{e}");
        let g = run_with(p.to_str().unwrap(), "//pkg:t", None, &sinks(&["poll_oneoff"])).unwrap();
        assert!(g.applied.silences.contains(&silences::Silence::Target("poll_oneoff".into())));
    }
}
