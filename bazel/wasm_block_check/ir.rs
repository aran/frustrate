//! The native check: the same `#[bridge(no_block)]` claims, settled over the
//! LLVM IR of a native build rather than over a wasm module.
//!
//! The input is one module: the bridge crate built for one platform as a
//! `staticlib` with fat LTO and `--emit=llvm-ir`, so std and every dependency
//! are merged in from their bitcode, under `--cfg frustrate_block_check` so
//! the only roots are the claimed members. On the native target a wait is a
//! call into the C library (`pthread_mutex_lock`, `dispatch_semaphore_wait`,
//! a futex wait through `syscall` on Linux, …), which the IR keeps as a call
//! to an external declaration, so a wait is exact here in the way a wait
//! instruction is on wasm. The target's C library ([`super::libc`], chosen by
//! the module's `target triple`) says which externals wait; a module whose
//! triple names no library the check knows is refused. On Linux a call to
//! `syscall` is an external named after the kernel call it makes
//! ([`libc::syscall_name`]).
//!
//! # How it decides
//!
//! The graph's edges are every symbol a function body or a global initialiser
//! mentions: direct calls, and the address-takes that put a function in a
//! vtable or a function pointer. Unlike a wasm table, every address-take is
//! attributed to the function or constant that makes it, so following them is
//! the conservative closure of what a root can run, with no "cannot verify"
//! case: a sink that a root can reach only through a `dyn` call is reached.
//!
//! Per root, a sink in that closure is red, with the shortest path to it, by
//! direct calls where one exists. Two kinds of edge are not followed:
//!
//! - **Into the fatal entries** ([`super::FATAL_ENTRIES`]): a wait behind the
//!   panic handler runs on a thread that is already failing.
//! - **Into frustrate's endings** ([`ENDINGS`]): cleanup that runs after an
//!   isolate has died and is documented never to block.
//! - **From frustrate's bounded runtime locks into `std::sync`**
//!   ([`BOUNDED`]): the runtime's own registries, whose critical sections are
//!   container operations. On threaded wasm they are spin locks, which the
//!   wasm check already accepts; a native `Mutex` holding the same section
//!   stalls a caller exactly as long. Everything else those functions call is
//!   followed. frustrate's own test bridge keeps this list honest: a new
//!   runtime lock reached from a member turns its check red.
//!
//! Green covers the one platform the module was built for. A body split by
//! `#[cfg]` is checked in that arm only.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::libc;
use super::{match_census, own_path, readable, Claims, Sinks, FATAL_ENTRIES, ROOT_PREFIX};

/// frustrate's runtime locks whose `std::sync` edges are not followed, and
/// why each section is bounded. A path is a module or a function.
const BOUNDED: &[(&[&str], &str)] = &[
    (
        &["frustrate", "spin"],
        "the runtime's registry lock, whose sections spin.rs bounds to a \
         container operation",
    ),
    (
        &["frustrate", "resident"],
        "the resident registry: one container operation per mint, use and \
         reclaim",
    ),
    (
        &["frustrate", "post", "native"],
        "the isolate routing table: a lookup per delivery, an insert or \
         removal per isolate",
    ),
];

/// frustrate's runtime functions that are not followed at all, like the
/// fatal entries: each runs only after something has already ended, and its
/// documented contract is that it does not block.
const ENDINGS: &[(&[&str], &str)] = &[(
    &["frustrate", "post", "native", "mark_gone"],
    "the cleanup after a post finds its isolate dead; it must not block (its \
     contract, and that of `actor::reap_isolate` beneath it): short registry \
     locks, then sends",
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
    fn arch(&self) -> &str {
        self.triple.split('-').next().unwrap_or("")
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
/// String constants (`c"…"`) and comments are skipped, so an `@` inside data
/// is never read as a reference.
fn symbols(line: &str) -> Vec<(String, bool)> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b';' => break,
            b'c' if b.get(i + 1) == Some(&b'"') => {
                i += 2;
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

fn path_starts(path: &[String], prefix: &[&str]) -> bool {
    path.len() >= prefix.len() && prefix.iter().zip(path).all(|(a, b)| a == b)
}

/// Settle the claims of one native IR module.
pub(crate) fn run_ir(
    path: &str,
    label: &str,
    claims: Option<&Claims>,
    sinks: &Sinks,
) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut ir = parse(&text).map_err(|e| format!("wasm_block_check: {label}: {e}"))?;

    let Some(libc) = libc::for_triple(&ir.triple) else {
        return Err(format!(
            "wasm_block_check: {label} targets `{}`, and this check knows which \
             C library calls wait only for {}. A wait set for another C library \
             has to be drawn from its implementation before it can be trusted.",
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
            "wasm_block_check: cannot verify {label}: Rust functions are declared \
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

    // The bounded runtime locks' edges into std::sync.
    for f in 0..ir.names.len() {
        if !BOUNDED.iter().any(|(p, _)| path_starts(&paths[f], p)) {
            continue;
        }
        let into_sync = |c: &u32| {
            let p = &paths[*c as usize];
            path_starts(p, &["std", "sync"]) || path_starts(p, &["std", "sys", "sync"])
        };
        ir.refs[f].retain(|c| !into_sync(c));
        ir.calls[f].retain(|c| !into_sync(c));
    }

    let cut: BTreeSet<u32> = (0..ir.names.len() as u32)
        .filter(|f| {
            FATAL_ENTRIES.iter().any(|e| e.matches(&ir.names[*f as usize]))
                || ENDINGS.iter().any(|(p, _)| path_starts(&paths[*f as usize], p))
        })
        .collect();

    // What each sink is.
    let mut kinds: BTreeMap<u32, String> = BTreeMap::new();
    for f in 0..ir.names.len() {
        let n = &ir.names[f];
        let mut k: Vec<String> = Vec::new();
        if !ir.defined[f] {
            if libc.waits(n) {
                k.push("waits".into());
            }
            k.extend(
                sinks
                    .io
                    .iter()
                    .filter(|c| c.native_call(libc.os, n))
                    .map(|c| format!("IO `{}`", c.name)),
            );
        } else {
            k.extend(
                sinks
                    .paths
                    .iter()
                    .filter(|p| p.matches(&paths[f]))
                    .map(|p| format!("named slow by `{}`", p.text())),
            );
        }
        if !k.is_empty() {
            kinds.insert(f as u32, k.join(", "));
        }
    }

    let reach = |start: u32, edges: &Vec<BTreeSet<u32>>| -> BTreeSet<u32> {
        let mut seen = BTreeSet::new();
        let mut stack = vec![start];
        seen.insert(start);
        while let Some(f) = stack.pop() {
            for c in &edges[f as usize] {
                if !cut.contains(c) && seen.insert(*c) {
                    stack.push(*c);
                }
            }
        }
        seen
    };

    // Control: every full root still reaches its member. A root calls its
    // member's glue with an empty request; an optimizer that sees the decode
    // fail deletes the body after it, and the scan would pass every member
    // that takes an argument.
    if let Some(c) = claims {
        let folded: Vec<String> = c
            .artifact
            .iter()
            .filter_map(|(sym, display)| {
                let r = *ir.index.get(sym)?;
                let glue = format!("call_{}", &sym[ROOT_PREFIX.len()..]);
                let reaches = reach(r, &ir.refs)
                    .iter()
                    .any(|f| paths[*f as usize].last().is_some_and(|l| *l == glue));
                (!reaches).then(|| format!("  {display}"))
            })
            .take(super::MEMBER_CAP)
            .collect();
        if !folded.is_empty() {
            return Err(format!(
                "wasm_block_check: cannot verify {label}: these roots no longer \
                 reach their member's glue:\n\n{}\n\n\
                 The check build was optimized: a flag outside the check's \
                 configuration (a crate's own `rustc_flags`, say) raised its \
                 opt-level, and the optimizer deleted the member bodies the \
                 scan exists to read. Build the check at opt-level 0.",
                folded.join("\n")
            ));
        }
    }

    let display = |sym: &str| -> String {
        claims
            .and_then(|c| c.roots().find(|(s, _)| s == sym).map(|(_, d)| d.clone()))
            .unwrap_or_else(|| sym.to_string())
    };

    let mut members: Vec<(String, Vec<(u32, bool)>, usize)> = Vec::new();
    for (r, sym) in roots.iter().zip(&root_names) {
        let reached: BTreeSet<u32> = reach(*r, &ir.refs)
            .into_iter()
            .filter(|f| kinds.contains_key(f))
            .collect();
        if reached.is_empty() {
            continue;
        }
        let path = shortest(&ir.calls, &ir.calls, *r, &reached, &cut)
            .or_else(|| shortest(&ir.refs, &ir.calls, *r, &reached, &cut))
            .expect("a reached sink has a path");
        members.push((display(sym), path, reached.len()));
    }

    let mut named: Vec<String> = libc.waits.iter().map(|w| format!("`{w}`")).collect();
    named.truncate(4);
    let sink_note = if sinks.is_empty() {
        String::new()
    } else {
        format!(", and {}", sinks.text())
    };

    if members.is_empty() {
        return Ok(format!(
            "wasm_block_check: {label} is clean — {} `no_block` root(s) on `{}`, \
             checked against {}'s waits ({}, …){sink_note}{}.",
            roots.len(),
            ir.triple,
            libc.name,
            named.join(", "),
            claims.map(Claims::accounting).unwrap_or_default()
        ));
    }

    let mut msg = format!(
        "wasm_block_check: {label}: {} `#[bridge(no_block)]` member(s) reach a \
         native wait{} on `{}`.\n\n",
        members.len(),
        if sinks.is_empty() { "" } else { " or a function named slow or IO" },
        ir.triple
    );
    for (n, (who, path, sites)) in members.iter().enumerate() {
        let last = path.last().unwrap().0;
        if n >= super::WITNESSED {
            if n == super::WITNESSED {
                msg.push_str("  Without paths:\n");
            }
            msg.push_str(&format!("  {who} — reaches {}\n", ir.label(last)));
            continue;
        }
        msg.push_str(&format!("  {who}\n      reaches {} — {}\n", ir.label(last), kinds[&last]));
        if *sites > 1 {
            msg.push_str(&format!("      and {} other site(s)\n", sites - 1));
        }
        for (d, (f, call)) in path.iter().enumerate() {
            msg.push_str(&format!(
                "        {}{}{}\n",
                "  ".repeat(d.min(8)),
                ir.label(*f),
                if *call || d == 0 { "" } else { "   (address taken: a `dyn` or fn-pointer call)" }
            ));
        }
        msg.push('\n');
    }
    msg.push_str(
        "A claimed member's caller may be the main isolate, where a wait \
         stalls the UI for as\nlong as another thread holds what it waits for. \
         Ways out: take the lock or the slow\ncall out of the body, make the \
         member `async` so it is dispatched off the caller, or\ndrop the claim.\n\
         \nNot followed, and not counted above:\n",
    );
    for f in FATAL_ENTRIES {
        msg.push_str(&format!("  {} — {}.\n", f.what, f.why));
    }
    for (p, why) in BOUNDED {
        msg.push_str(&format!("  {}'s locks — {why}.\n", p.join("::")));
    }
    for (p, why) in ENDINGS {
        msg.push_str(&format!("  {} — {why}.\n", p.join("::")));
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

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "target triple = \"arm64-apple-macosx11.0.0\"\n";

    fn check(body: &str, slow: &[&str], io: &[&str]) -> Result<String, String> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!("ir_block_check_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("m{}.ll", NEXT.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, format!("{HEAD}{body}")).unwrap();
        let sinks = Sinks {
            paths: slow.iter().map(|t| super::super::SlowPath::parse(t).unwrap()).collect(),
            io: io.iter().map(|t| super::super::io::parse(t).unwrap()).collect(),
            wasi: false,
        };
        run_ir(p.to_str().unwrap(), "//pkg:target", None, &sinks)
    }

    const ROOT: &str = "define void @frustrate_check_block_1_render() {\n  call void @_RNvCs1_7example6render()\n  ret void\n}\n";

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
        assert!(e.contains("1 `#[bridge(no_block)]` member(s) reach a native wait"), "{e}");
        assert!(e.contains("reaches pthread_mutex_lock — waits"), "{e}");
        assert!(e.contains("example::render"), "{e}");
    }

    #[test]
    fn a_wait_reached_through_a_vtable_is_attributed_to_the_member() {
        // render stores a vtable whose method locks: no direct call path.
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

    #[test]
    fn a_bounded_runtime_lock_is_not_followed_into_std_sync_only() {
        let lock = "define void @_RNvNtCs1_9frustrate4spin4with() {\n  call void @_RNvNtNtNtCs2_3std4sync6poison5mutex4lock()\n  call void @_RNvCs1_7example4work()\n  ret void\n}\n\
                    define void @_RNvNtNtNtCs2_3std4sync6poison5mutex4lock() {\n  call i32 @pthread_mutex_lock(ptr %0)\n  ret void\n}\n\
                    declare i32 @pthread_mutex_lock(ptr)\n";
        let root = "define void @frustrate_check_block_1_render() {\n  call void @_RNvNtCs1_9frustrate4spin4with()\n  ret void\n}\n";
        let ok = check(&format!("{root}{lock}define void @_RNvCs1_7example4work() {{\n  ret void\n}}\n"), &[], &[]).unwrap();
        assert!(ok.contains("clean"), "{ok}");
        // What the section calls besides std::sync is still followed.
        let e = check(
            &format!("{root}{lock}define void @_RNvCs1_7example4work() {{\n  call i32 @nanosleep(ptr %0)\n  ret void\n}}\ndeclare i32 @nanosleep(ptr)\n"),
            &[],
            &[],
        )
        .unwrap_err();
        assert!(e.contains("reaches nanosleep — waits"), "{e}");
    }

    #[test]
    fn io_categories_and_paths_are_sinks_too() {
        let body = format!(
            "{ROOT}define void @_RNvCs1_7example6render() {{\n  call i32 @open(ptr %0)\n  ret void\n}}\ndeclare i32 @open(ptr)\n"
        );
        assert!(check(&body, &[], &[]).is_ok());
        assert!(check(&body, &[], &["network"]).is_ok());
        let e = check(&body, &[], &["files"]).unwrap_err();
        assert!(e.contains("reaches open — IO `files`"), "{e}");
        let e = check(&body, &["example::render"], &[]).unwrap_err();
        assert!(e.contains("named slow by `example::render`"), "{e}");
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

    #[test]
    fn a_root_folded_away_from_its_glue_is_refused() {
        let claims = Claims::parse(
            &format!("{}\ncrate example\nartifact 1 render\n", super::super::CENSUS_HEADER),
            "c",
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("ir_block_folded_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let run = |body: &str| {
            let p = dir.join(format!("m{}.ll", body.len()));
            std::fs::write(&p, format!("{HEAD}{body}")).unwrap();
            run_ir(p.to_str().unwrap(), "//pkg:t", Some(&claims), &Sinks::default())
        };
        // What an optimizer leaves: the root panics on the empty request.
        let folded = "define void @frustrate_check_block_1_render() {\n  call void @_RNvNtCs8_4core6option13expect_failed()\n  ret void\n}\n\
                      define void @_RNvNtCs8_4core6option13expect_failed() {\n  ret void\n}\n";
        let e = run(folded).unwrap_err();
        assert!(e.contains("no longer reach their member's glue"), "{e}");
        let kept = "define void @frustrate_check_block_1_render() {\n  call void @_RNvNtCs1_7example19frustrate_generated13call_1_render()\n  ret void\n}\n\
                    define void @_RNvNtCs1_7example19frustrate_generated13call_1_render() {\n  ret void\n}\n";
        assert!(run(kept).is_ok());
    }

    /// A module for `triple`, written out and scanned.
    fn run_triple(triple: &str, body: &str, io: &[&str]) -> Result<String, String> {
        let dir = std::env::temp_dir().join(format!("ir_block_triple_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("{triple}-{}-{}.ll", body.len(), io.len()));
        std::fs::write(&p, format!("target triple = \"{triple}\"\n{body}")).unwrap();
        let sinks = Sinks {
            io: io.iter().map(|t| super::super::io::parse(t).unwrap()).collect(),
            ..Sinks::default()
        };
        run_ir(p.to_str().unwrap(), "//pkg:t", None, &sinks)
    }

    /// A Linux member that makes `syscall`s: what each is read as.
    fn linux(calls: &str) -> String {
        format!(
            "{ROOT}define void @_RNvCs1_7example6render() {{\n{calls}  ret void\n}}\n\
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
            assert!(run_triple(triple, &linux(&wake), &[]).is_ok(), "{triple}");
            let wait = format!(
                "  %2 = call i64 (i64, ...) @syscall(i64 noundef {futex}, ptr %0, i32 noundef 137, i32 noundef 0, ptr null, ptr null, i32 noundef -1)\n"
            );
            let e = run_triple(triple, &linux(&format!("{wake}{wait}")), &[]).unwrap_err();
            assert!(e.contains("reaches syscall::SYS_futex::FUTEX_WAIT_BITSET — waits"), "{triple}: {e}");
        }
    }

    /// A kernel call std makes through `syscall` is in its IO class by name.
    #[test]
    fn a_syscall_is_in_an_io_class() {
        let e = run_triple(
            "aarch64-unknown-linux-gnu",
            &linux("  %1 = call i64 (i64, ...) @syscall(i64 noundef 291, i32 %d, ptr %p, i32 0, i32 4095, ptr %b)\n"),
            &["files"],
        )
        .unwrap_err();
        assert!(e.contains("reaches syscall::SYS_statx — IO `files`"), "{e}");
    }

    #[test]
    fn a_target_without_a_classified_c_library_is_refused() {
        let e = run_triple("x86_64-unknown-linux-musl", ROOT, &[]).unwrap_err();
        assert!(e.contains("only for Apple's libSystem, Android's bionic, glibc"), "{e}");
    }
}
